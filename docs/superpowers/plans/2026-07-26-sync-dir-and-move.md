# sync 空資料夾建立/刪除傳遞 + 單側改名/搬移偵測 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend the `sync` plugin so it (1) propagates empty-directory creation/deletion between sync partners, and (2) detects single-sided file renames/moves (by content hash) and reuses the existing content instead of re-transferring it, both for same-domain and cross-domain sync.

**Architecture:** `Baseline` (currently a `HashMap` type alias) becomes a struct with `files` (unchanged per-path hash tracking) and a new `known_dirs` set (all directory paths both sides agreed existed as of the last successful sync, regardless of whether they held files at the time). `classify` gains a post-processing pass that pairs same-side delete+add candidates by content hash into `MoveLocal`/`MoveRemote` actions; a new sibling pure function `classify_directories` handles directory create/delete using this round's already-fetched manifests (no extra network round-trip). `run_sync_pass` executes all file-level actions (including moves) first, then directory actions (create shallow-to-deep, delete deep-to-shallow, non-recursive delete as a safety net). Cross-domain rename reuses the existing `CrossDomainAsk`/`RemoteRequest`/`RemoteReply` relay machinery (same-domain rename already has a working HTTP endpoint).

**Tech Stack:** Rust, existing `sync`/`sync_baseline`/`storage`/`global`/`shell`/`plugin` modules, `anyhow`, `serde`/`serde_json` (all already dependencies) — no new dependencies.

## Global Constraints

- Full design context: `docs/superpowers/specs/2026-07-26-sync-dir-and-move-design.md`.
- Directory create/delete only fires when this round's already-fetched manifests show **zero files (recursively) under that path on both sides** — never re-query after executing file actions (accept up to one extra polling round of delay instead of a second network round-trip).
- Directory deletes call `remove(path, recursive=false)` / `transport.delete(path, false)` as a safety net — if the guard was wrong and the directory still has content at execution time, the delete fails safely (logged, skipped) instead of destroying data.
- Move/rename detection only pairs delete-candidates and add-candidates that are **both on the same side** in the same round (a device's own rename). It never pairs across sides, never touches paths classified as `Conflict`, and never touches "single-side modified an existing baseline-tracked path" (only genuinely new paths with no baseline entry are add-candidates).
- Duplicate-hash pairing is deterministic: group by hash, sort each group's paths, zip in order; unmatched leftovers fall back to ordinary delete/push.
- Directories never produce `Conflict` — they hold no content.
- No new HTTP endpoint for same-domain rename (`/api/storage/rename` + `storage::rename_path` already exist). Only the cross-domain path (`CrossDomainAsk::StorageRename`) is new.
- `Baseline`'s on-disk JSON format changes (flat map → `{files, known_dirs}`). Old-format files must fail to parse and degrade to an empty `Baseline` via the existing `load_baseline` error-handling path — never a custom migration, never a panic.
- `SyncAction` is matched exhaustively (no wildcard `_`) inside `run_sync_pass`'s main action loop today. Every task that adds a new `SyncAction` variant must, in that same task, keep that match exhaustive — either by adding the variant's real execution arm (if `classify()` can already produce it by the end of that task), or by adding an explicit `unreachable!()` arm naming exactly that variant (only when nothing yet calls the function that would produce it). Never leave a task boundary where a variant `classify()` can already produce is marked `unreachable!()` — that would panic the live background sync thread.

---

### Task 1: `Baseline` struct migration (`files` + `known_dirs`)

**Files:**
- Modify: `src/plugins/sync_baseline.rs`
- Modify: `src/plugins/sync.rs` (only the parts that touch `Baseline` as a bare `HashMap`)

**Interfaces:**
- Produces: `Baseline` struct with `pub(crate) files: HashMap<String, BaselineEntry>` and `pub(crate) known_dirs: HashSet<String>`, both `#[serde(default)]`, deriving `Serialize, Deserialize, Clone, PartialEq, Debug, Default`. `Baseline::default()` replaces the old `Baseline::new()` everywhere.
- Consumed by: Task 2 (`known_dirs` param), Task 4 (`.files` lookups), Task 4/5 (`run_sync_pass` mutates `.files`/`.known_dirs`).

- [ ] **Step 1: Change the `Baseline` type and update `sync_baseline.rs`**

In `src/plugins/sync_baseline.rs`, change the top of the file from:

```rust
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
```

to:

```rust
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
```

Change:

```rust
/// 一個同步對象（同 domain 的某個 client id、或跨 domain 的某個 domain 名稱）
/// 的完整 baseline：相對路徑 -> 上次同步完成時的狀態。
pub(crate) type Baseline = HashMap<String, BaselineEntry>;
```

to:

```rust
/// 一個同步對象（同 domain 的某個 client id、或跨 domain 的某個 domain 名稱）
/// 的完整 baseline：檔案層級的狀態（相對路徑 -> 上次同步完成時的狀態），加上
/// 目錄層級的 `known_dirs`。
///
/// `known_dirs` 記錄「上一輪同步完成時，雙邊都存在的所有目錄路徑」——不限於
/// 當時是空的：一個原本有檔案、後來檔案被清空的目錄，只要它在上一輪同步完成
/// 時雙邊都存在過，就會在這裡；只追蹤「一直是空的」目錄沒辦法正確修好「client
/// 砍掉一個有內容的目錄，server 端檔案跟著刪了但目錄本身變成孤兒」這個情況。
///
/// 兩個欄位都加 `#[serde(default)]`，讓改版前那種扁平 `{路徑: BaselineEntry}`
/// （沒有這層 `files`/`known_dirs` 外殼）的舊格式檔案，用新結構去解析時觸發的
/// 失敗，能被 `load_baseline` 既有的「格式壞掉當作沒有 baseline」錯誤處理路徑
/// 接住，安全降級成空 `Baseline`，不會 panic。
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub(crate) struct Baseline {
    #[serde(default)]
    pub(crate) files: HashMap<String, BaselineEntry>,
    #[serde(default)]
    pub(crate) known_dirs: HashSet<String>,
}
```

Change `load_baseline` from:

```rust
pub(crate) fn load_baseline(state_dir: &Path, partner_key: &str) -> Baseline {
    let path = baseline_path(state_dir, partner_key);
    match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Baseline::new(),
    }
}
```

to:

```rust
pub(crate) fn load_baseline(state_dir: &Path, partner_key: &str) -> Baseline {
    let path = baseline_path(state_dir, partner_key);
    match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Baseline::default(),
    }
}
```

- [ ] **Step 2: Update `sync_baseline.rs`'s existing tests to go through `.files`**

Replace the whole `#[cfg(test)] mod tests { ... }` block in `src/plugins/sync_baseline.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 每個測試各自用一個獨立命名的暫存資料夾，避免 cargo test 平行跑測試時
    /// 互相踩到彼此的檔案（跟 `storage.rs` 的 `test_root` 同一個理由）。
    fn test_state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cng5-sync-baseline-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn save_then_load_round_trips() {
        let state_dir = test_state_dir("round-trip");
        let mut baseline = Baseline::default();
        baseline.files.insert(
            "photos/beach.jpg".to_string(),
            BaselineEntry { local_hash: "abc123".to_string(), remote_hash: "abc123".to_string() },
        );
        save_baseline(&state_dir, "client-office-pc", &baseline).unwrap();
        let loaded = load_baseline(&state_dir, "client-office-pc");
        assert_eq!(loaded, baseline);
    }

    #[test]
    fn load_missing_file_returns_empty_baseline() {
        let state_dir = test_state_dir("missing-file");
        let loaded = load_baseline(&state_dir, "nonexistent-partner");
        assert!(loaded.files.is_empty());
        assert!(loaded.known_dirs.is_empty());
    }

    #[test]
    fn load_corrupt_file_returns_empty_baseline_not_panic() {
        let state_dir = test_state_dir("corrupt-file");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(baseline_path(&state_dir, "broken"), b"{ this is not valid json").unwrap();
        let loaded = load_baseline(&state_dir, "broken");
        assert!(loaded.files.is_empty());
    }

    #[test]
    fn load_legacy_flat_format_returns_empty_baseline_not_panic() {
        // 改版前的舊格式：扁平的 {路徑: BaselineEntry}，沒有 files/known_dirs
        // 這層外殼——新結構解析這種格式一定會失敗，必須安全降級，不能 panic。
        let state_dir = test_state_dir("legacy-flat-format");
        fs::create_dir_all(&state_dir).unwrap();
        let legacy_json = r#"{"photos/beach.jpg": {"local_hash": "abc123", "remote_hash": "abc123"}}"#;
        fs::write(baseline_path(&state_dir, "legacy-partner"), legacy_json).unwrap();
        let loaded = load_baseline(&state_dir, "legacy-partner");
        assert!(loaded.files.is_empty());
        assert!(loaded.known_dirs.is_empty());
    }

    #[test]
    fn save_creates_state_dir_if_missing() {
        let state_dir = test_state_dir("creates-dir");
        assert!(!state_dir.exists());
        save_baseline(&state_dir, "domain-branch-b", &Baseline::default()).unwrap();
        assert!(state_dir.is_dir());
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let state_dir = test_state_dir("no-temp-leftover");
        save_baseline(&state_dir, "client-a", &Baseline::default()).unwrap();
        let tmp_path = baseline_path(&state_dir, "client-a").with_extension("json.tmp");
        assert!(!tmp_path.exists());
        assert!(baseline_path(&state_dir, "client-a").exists());
    }

    #[test]
    fn different_partners_get_different_files() {
        let state_dir = test_state_dir("different-partners");
        let mut baseline_a = Baseline::default();
        baseline_a
            .files
            .insert("x.txt".to_string(), BaselineEntry { local_hash: "h1".to_string(), remote_hash: "h1".to_string() });
        save_baseline(&state_dir, "client-a", &baseline_a).unwrap();
        save_baseline(&state_dir, "client-b", &Baseline::default()).unwrap();
        assert_eq!(load_baseline(&state_dir, "client-a"), baseline_a);
        assert!(load_baseline(&state_dir, "client-b").files.is_empty());
    }
}
```

- [ ] **Step 3: Run the sync_baseline tests to verify they compile and pass**

Run: `cargo test sync_baseline:: 2>&1 | tail -30`
Expected: all 7 tests pass (6 existing + 1 new `load_legacy_flat_format_returns_empty_baseline_not_panic`).

- [ ] **Step 4: Update `sync.rs`'s uses of `Baseline` as a bare `HashMap`**

In `src/plugins/sync.rs`, inside `classify` (currently lines 42-58), change:

```rust
    let mut all_paths: HashSet<&String> = HashSet::new();
    all_paths.extend(local_states.keys());
    all_paths.extend(remote_states.keys());
    all_paths.extend(baseline.keys());

    let mut paths: Vec<&String> = all_paths.into_iter().collect();
    paths.sort();

    let mut actions = Vec::new();
    for path in paths {
        let local_state = local_states.get(path);
        let remote_state = remote_states.get(path);
        let base = baseline.get(path);
```

to:

```rust
    let mut all_paths: HashSet<&String> = HashSet::new();
    all_paths.extend(local_states.keys());
    all_paths.extend(remote_states.keys());
    all_paths.extend(baseline.files.keys());

    let mut paths: Vec<&String> = all_paths.into_iter().collect();
    paths.sort();

    let mut actions = Vec::new();
    for path in paths {
        let local_state = local_states.get(path);
        let remote_state = remote_states.get(path);
        let base = baseline.files.get(path);
```

In `run_sync_pass`, change every `baseline.insert(` to `baseline.files.insert(` and every `baseline.remove(` to `baseline.files.remove(`. There are 5 call sites (3 inserts, 2 removes) inside the `match action` block:

```rust
                        if let Some(hash) = find_hash(&local, &path) {
                            baseline.insert(
                                path,
                                BaselineEntry { local_hash: hash.to_string(), remote_hash: hash.to_string() },
                            );
                        }
```
→
```rust
                        if let Some(hash) = find_hash(&local, &path) {
                            baseline.files.insert(
                                path,
                                BaselineEntry { local_hash: hash.to_string(), remote_hash: hash.to_string() },
                            );
                        }
```

(this exact `if let` shape appears for the `PushToRemote` success branch)

```rust
                        if let Some(hash) = find_hash(&remote, &path) {
                            baseline.insert(
                                path,
                                BaselineEntry { local_hash: hash.to_string(), remote_hash: hash.to_string() },
                            );
                        }
```
→
```rust
                        if let Some(hash) = find_hash(&remote, &path) {
                            baseline.files.insert(
                                path,
                                BaselineEntry { local_hash: hash.to_string(), remote_hash: hash.to_string() },
                            );
                        }
```

(this shape appears for the `PullFromRemote` success branch)

```rust
            SyncAction::DeleteLocal { path } => match crate::plugins::remove(&local_root.join(&path), true) {
                Ok(()) => {
                    baseline.remove(&path);
                    outcome.deleted_local += 1;
                }
                Err(err) => outcome.error = Some(format!("刪除本機 {path} 失敗: {err:#}")),
            },
            SyncAction::DeleteRemote { path } => match transport.delete(&path, true) {
                Ok(()) => {
                    baseline.remove(&path);
                    outcome.deleted_remote += 1;
                }
                Err(err) => outcome.error = Some(format!("刪除對方 {path} 失敗: {err:#}")),
            },
```
→
```rust
            SyncAction::DeleteLocal { path } => match crate::plugins::remove(&local_root.join(&path), true) {
                Ok(()) => {
                    baseline.files.remove(&path);
                    outcome.deleted_local += 1;
                }
                Err(err) => outcome.error = Some(format!("刪除本機 {path} 失敗: {err:#}")),
            },
            SyncAction::DeleteRemote { path } => match transport.delete(&path, true) {
                Ok(()) => {
                    baseline.files.remove(&path);
                    outcome.deleted_remote += 1;
                }
                Err(err) => outcome.error = Some(format!("刪除對方 {path} 失敗: {err:#}")),
            },
```

And in the `Conflict` success branch:

```rust
                        if let (Some(local_hash), Some(remote_hash)) =
                            (find_hash(&local, &path), find_hash(&remote, &path))
                        {
                            baseline.insert(
                                path,
                                BaselineEntry {
                                    local_hash: local_hash.to_string(),
                                    remote_hash: remote_hash.to_string(),
                                },
                            );
                        }
```
→
```rust
                        if let (Some(local_hash), Some(remote_hash)) =
                            (find_hash(&local, &path), find_hash(&remote, &path))
                        {
                            baseline.files.insert(
                                path,
                                BaselineEntry {
                                    local_hash: local_hash.to_string(),
                                    remote_hash: remote_hash.to_string(),
                                },
                            );
                        }
```

- [ ] **Step 5: Update `sync.rs`'s existing test helper and all `Baseline::new()` calls**

In the `#[cfg(test)] mod tests` block, change `baseline_of` from:

```rust
    /// 每個項目是 `(路徑, local_hash, remote_hash)`——正常同步完成後兩者相等，
    /// 但已經確認過的衝突會刻意留下不同的值，測試裡分開指定才能覆蓋到兩種
    /// 情況。
    fn baseline_of(entries: &[(&str, &str, &str)]) -> Baseline {
        entries
            .iter()
            .map(|(path, local_hash, remote_hash)| {
                (
                    path.to_string(),
                    BaselineEntry { local_hash: local_hash.to_string(), remote_hash: remote_hash.to_string() },
                )
            })
            .collect()
    }
```

to:

```rust
    /// 每個項目是 `(路徑, local_hash, remote_hash)`——正常同步完成後兩者相等，
    /// 但已經確認過的衝突會刻意留下不同的值，測試裡分開指定才能覆蓋到兩種
    /// 情況。
    fn baseline_of(entries: &[(&str, &str, &str)]) -> Baseline {
        let files = entries
            .iter()
            .map(|(path, local_hash, remote_hash)| {
                (
                    path.to_string(),
                    BaselineEntry { local_hash: local_hash.to_string(), remote_hash: remote_hash.to_string() },
                )
            })
            .collect();
        Baseline { files, known_dirs: HashSet::new() }
    }
```

Then replace every remaining `Baseline::new()` in this test module (in `new_local_file_pushes_to_remote`, `new_remote_file_pulls_from_remote`, `both_sides_independently_created_same_name_different_content_is_conflict`, `both_sides_independently_created_same_name_same_content_is_no_action`, `no_baseline_and_content_differs_is_conservatively_a_conflict`, `multiple_independent_paths_each_classified_separately`) with `Baseline::default()`. These are simple find-and-replace occurrences — the test bodies and assertions themselves do not change.

- [ ] **Step 6: Run the full test suite to verify no regressions**

Run: `cargo build 2>&1 | tail -30` — expect clean build.
Run: `cargo test 2>&1 | tail -15` — expect all existing tests passing (97: 96 from the previous storage-overview work + 1 new `load_legacy_flat_format_returns_empty_baseline_not_panic`).

- [ ] **Step 7: Commit**

```bash
git add src/plugins/sync_baseline.rs src/plugins/sync.rs
git commit -m "$(cat <<'EOF'
Baseline 改成 struct，新增 known_dirs 欄位

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: `classify_directories` — directory create/delete classification

**Files:**
- Modify: `src/plugins/sync.rs`

**Interfaces:**
- Consumes: `Baseline.known_dirs: HashSet<String>` (Task 1).
- Produces:
  - `SyncAction` gains 4 new variants: `CreateLocalDir { path: String }`, `CreateRemoteDir { path: String }`, `DeleteLocalDir { path: String }`, `DeleteRemoteDir { path: String }`.
  - `pub(crate) struct DirClassification { pub(crate) actions: Vec<SyncAction>, pub(crate) confirmed_dirs: Vec<String>, pub(crate) stale_dirs: Vec<String> }`.
  - `pub(crate) fn classify_directories(local: &[SyncEntry], remote: &[SyncEntry], known_dirs: &HashSet<String>) -> DirClassification`.
  - Consumed by Task 5 (`run_sync_pass` calls this after the file-level `classify` and executes the directory actions).

**Important — a real subtlety, not optional:** `run_sync_pass`'s main `for action in actions { match action { ... } }` loop (`actions` comes from `classify`, never from `classify_directories`) is matched exhaustively today with no wildcard arm. The moment this task adds 4 new variants to `SyncAction`, that match becomes non-exhaustive and **`cargo build` will fail** unless this task also adds an arm for them. Since `classify()` never produces these 4 variants (only the new, not-yet-called `classify_directories` does), it is correct and safe to make that arm `unreachable!()` — it genuinely cannot be hit yet. Step 3 below includes this.

- [ ] **Step 1: Write the failing tests**

Add this to the existing `#[cfg(test)] mod tests { ... }` block in `src/plugins/sync.rs` (add alongside the other tests, before the closing `}` of the module):

```rust
    fn dir(path: &str) -> SyncEntry {
        SyncEntry { path: path.to_string(), is_dir: true, size: 0, modified: 0, hash: None }
    }

    #[test]
    fn new_empty_dir_only_local_creates_remote_dir() {
        let local = vec![dir("newdir")];
        let remote = vec![];
        let result = classify_directories(&local, &remote, &HashSet::new());
        assert_eq!(result.actions, vec![SyncAction::CreateRemoteDir { path: "newdir".to_string() }]);
        assert!(result.confirmed_dirs.is_empty());
        assert!(result.stale_dirs.is_empty());
    }

    #[test]
    fn new_empty_dir_only_remote_creates_local_dir() {
        let local = vec![];
        let remote = vec![dir("newdir")];
        let result = classify_directories(&local, &remote, &HashSet::new());
        assert_eq!(result.actions, vec![SyncAction::CreateLocalDir { path: "newdir".to_string() }]);
    }

    #[test]
    fn known_dir_missing_from_remote_deletes_local_dir() {
        // known_dirs 有紀錄（上一輪雙邊都有）、這輪 remote 找不到這個目錄了、
        // 雙邊底下都沒有檔案 → 傳遞刪除到 local，讓 local 跟上 remote 已經
        // 刪除的狀態。
        let local = vec![dir("olddir")];
        let remote = vec![];
        let known_dirs: HashSet<String> = ["olddir".to_string()].into_iter().collect();
        let result = classify_directories(&local, &remote, &known_dirs);
        assert_eq!(result.actions, vec![SyncAction::DeleteLocalDir { path: "olddir".to_string() }]);
    }

    #[test]
    fn known_dir_missing_from_local_deletes_remote_dir() {
        let local = vec![];
        let remote = vec![dir("olddir")];
        let known_dirs: HashSet<String> = ["olddir".to_string()].into_iter().collect();
        let result = classify_directories(&local, &remote, &known_dirs);
        assert_eq!(result.actions, vec![SyncAction::DeleteRemoteDir { path: "olddir".to_string() }]);
    }

    #[test]
    fn dir_guard_skips_when_remote_still_has_files_under_it() {
        // photos 在 local 這輪已經完全消失（含檔案），但 remote 的 manifest
        // 還沒反映出檔案層級的刪除（下一輪才會）——保護規則：remote 底下還有
        // 檔案，這輪完全跳過這個目錄的建立/刪除判斷。
        let local = vec![];
        let remote = vec![dir("photos"), file("photos/img.jpg", "h1")];
        let known_dirs: HashSet<String> = ["photos".to_string()].into_iter().collect();
        let result = classify_directories(&local, &remote, &known_dirs);
        assert!(result.actions.is_empty());
        assert!(result.confirmed_dirs.is_empty());
        assert!(result.stale_dirs.is_empty());
    }

    #[test]
    fn dir_guard_skips_when_local_still_has_files_under_it() {
        let local = vec![dir("photos"), file("photos/img.jpg", "h1")];
        let remote = vec![];
        let known_dirs: HashSet<String> = ["photos".to_string()].into_iter().collect();
        let result = classify_directories(&local, &remote, &known_dirs);
        assert!(result.actions.is_empty());
    }

    #[test]
    fn both_sides_have_new_dir_confirms_without_conflict() {
        // 雙邊各自獨立新建同名空目錄，known_dirs 沒有記錄——不是衝突，直接
        // 確認一致，之後應該被記進 known_dirs，不輸出任何動作。
        let local = vec![dir("shared")];
        let remote = vec![dir("shared")];
        let result = classify_directories(&local, &remote, &HashSet::new());
        assert!(result.actions.is_empty());
        assert_eq!(result.confirmed_dirs, vec!["shared".to_string()]);
    }

    #[test]
    fn both_sides_missing_previously_known_dir_marks_stale() {
        // 雙邊都已經不再有這個目錄了（先前各自都刪過），known_dirs 裡的舊
        // 紀錄應該被清掉，不然之後任何一邊重新建立同名空目錄時，會被誤判成
        // 「known_dirs 有記錄、只剩一邊有」而錯誤地觸發刪除，而不是建立。
        let local = vec![];
        let remote = vec![];
        let known_dirs: HashSet<String> = ["gone".to_string()].into_iter().collect();
        let result = classify_directories(&local, &remote, &known_dirs);
        assert!(result.actions.is_empty());
        assert_eq!(result.stale_dirs, vec!["gone".to_string()]);
    }

    #[test]
    fn dir_present_on_both_sides_and_known_produces_no_action() {
        let local = vec![dir("stable")];
        let remote = vec![dir("stable")];
        let known_dirs: HashSet<String> = ["stable".to_string()].into_iter().collect();
        let result = classify_directories(&local, &remote, &known_dirs);
        assert!(result.actions.is_empty());
        assert_eq!(result.confirmed_dirs, vec!["stable".to_string()]);
        assert!(result.stale_dirs.is_empty());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test sync:: 2>&1 | tail -40`
Expected: compile errors — `cannot find function 'classify_directories'`, `no variant named 'CreateRemoteDir'`, etc.

- [ ] **Step 3: Implement `SyncAction`'s new variants, `DirClassification`, `classify_directories`, and keep `run_sync_pass`'s match exhaustive**

In `src/plugins/sync.rs`, change the `SyncAction` enum from:

```rust
/// 這一輪同步對某個路徑該做的事。只處理檔案，不處理空資料夾——資料夾是搬檔
/// 案時由執行層視需要自動建立，不會有獨立的「建立資料夾」/「刪除資料夾」
/// 動作。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SyncAction {
    PushToRemote { path: String },
    PullFromRemote { path: String },
    DeleteLocal { path: String },
    DeleteRemote { path: String },
    Conflict { path: String },
}
```

to:

```rust
/// 這一輪同步對某個路徑（檔案）或某個目錄該做的事。檔案動作（`classify`）
/// 跟目錄動作（`classify_directories`）是分開算的兩批，執行時檔案動作全部
/// 先跑完才處理目錄動作——見 `run_sync_pass`。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SyncAction {
    PushToRemote { path: String },
    PullFromRemote { path: String },
    DeleteLocal { path: String },
    DeleteRemote { path: String },
    Conflict { path: String },
    CreateLocalDir { path: String },
    CreateRemoteDir { path: String },
    DeleteLocalDir { path: String },
    DeleteRemoteDir { path: String },
}
```

Add this right after the `classify` function (after its closing `}`, before the `SyncTransport` trait):

```rust
/// `classify_directories` 的輸出：目錄動作，加上這一輪雙邊都確認一致、應該
/// （重新）記進 `known_dirs` 的路徑，以及雙邊都已經不存在、應該從
/// `known_dirs` 移除的路徑（不移除的話，之後任何一邊重新建立同名空目錄會被
/// 誤判成「known_dirs 有記錄、只剩一邊有」而錯誤觸發刪除，而不是建立）。
#[derive(Debug, PartialEq)]
pub(crate) struct DirClassification {
    pub(crate) actions: Vec<SyncAction>,
    pub(crate) confirmed_dirs: Vec<String>,
    pub(crate) stale_dirs: Vec<String>,
}

fn dir_paths(entries: &[SyncEntry]) -> HashSet<String> {
    entries.iter().filter(|e| e.is_dir).map(|e| e.path.clone()).collect()
}

/// `entries` 裡有沒有任何檔案的路徑是 `dir_path` 底下（遞迴）——目錄建立/
/// 刪除的保護規則用這個判斷「這輪還不能動這個目錄」，見 `classify_directories`。
fn has_files_under(entries: &[SyncEntry], dir_path: &str) -> bool {
    let prefix = format!("{dir_path}/");
    entries.iter().any(|e| !e.is_dir && e.path.starts_with(&prefix))
}

/// 目錄層級的分類：只看目錄本身的建立/刪除，不牽涉任何檔案內容，純函式。
/// 用這一輪已經抓到的雙邊 manifest（`local`/`remote`）判斷，不額外重新查詢。
pub(crate) fn classify_directories(
    local: &[SyncEntry],
    remote: &[SyncEntry],
    known_dirs: &HashSet<String>,
) -> DirClassification {
    let local_dirs = dir_paths(local);
    let remote_dirs = dir_paths(remote);

    let mut all_dirs: HashSet<&String> = HashSet::new();
    all_dirs.extend(local_dirs.iter());
    all_dirs.extend(remote_dirs.iter());
    all_dirs.extend(known_dirs.iter());
    let mut dirs: Vec<&String> = all_dirs.into_iter().collect();
    dirs.sort();

    let mut actions = Vec::new();
    let mut confirmed_dirs = Vec::new();
    let mut stale_dirs = Vec::new();

    for path in dirs {
        let has_local = local_dirs.contains(path);
        let has_remote = remote_dirs.contains(path);
        let known = known_dirs.contains(path);

        // 保護規則：這輪的雙邊 manifest，只要有一邊底下還有檔案，就完全跳過
        // 這個目錄的建立/刪除判斷，讓檔案層級的動作先處理（見設計文件「執行
        // 順序」一節）。
        if has_files_under(local, path) || has_files_under(remote, path) {
            continue;
        }

        match (has_local, has_remote, known) {
            (true, true, _) => confirmed_dirs.push(path.clone()),
            (true, false, false) => actions.push(SyncAction::CreateRemoteDir { path: path.clone() }),
            (false, true, false) => actions.push(SyncAction::CreateLocalDir { path: path.clone() }),
            (true, false, true) => actions.push(SyncAction::DeleteLocalDir { path: path.clone() }),
            (false, true, true) => actions.push(SyncAction::DeleteRemoteDir { path: path.clone() }),
            (false, false, true) => stale_dirs.push(path.clone()),
            (false, false, false) => {} // 從沒被雙邊同時擁有過，無事可做
        }
    }

    DirClassification { actions, confirmed_dirs, stale_dirs }
}
```

Now keep `run_sync_pass`'s main loop exhaustive. Change the end of its `match action { ... }` (right after the `SyncAction::Conflict` arm) from:

```rust
            SyncAction::Conflict { path } => {
                let remote_size = find_size(&remote, &path);
                match resolve_conflict(transport, &path, my_label, partner_label, remote_size) {
                    Ok(()) => {
                        // 只更新原本這個路徑的 baseline（承認兩邊從此故意分歧），
                        // 不替新產生的兩份衝突副本建立 baseline 條目——見本
                        // task 開頭的「已知取捨」說明。
                        if let (Some(local_hash), Some(remote_hash)) =
                            (find_hash(&local, &path), find_hash(&remote, &path))
                        {
                            baseline.files.insert(
                                path,
                                BaselineEntry {
                                    local_hash: local_hash.to_string(),
                                    remote_hash: remote_hash.to_string(),
                                },
                            );
                        }
                        outcome.conflicts += 1;
                    }
                    Err(err) => outcome.error = Some(format!("處理衝突 {path} 失敗: {err:#}")),
                }
            }
        }
    }
```

to:

```rust
            SyncAction::Conflict { path } => {
                let remote_size = find_size(&remote, &path);
                match resolve_conflict(transport, &path, my_label, partner_label, remote_size) {
                    Ok(()) => {
                        // 只更新原本這個路徑的 baseline（承認兩邊從此故意分歧），
                        // 不替新產生的兩份衝突副本建立 baseline 條目——見本
                        // task 開頭的「已知取捨」說明。
                        if let (Some(local_hash), Some(remote_hash)) =
                            (find_hash(&local, &path), find_hash(&remote, &path))
                        {
                            baseline.files.insert(
                                path,
                                BaselineEntry {
                                    local_hash: local_hash.to_string(),
                                    remote_hash: remote_hash.to_string(),
                                },
                            );
                        }
                        outcome.conflicts += 1;
                    }
                    Err(err) => outcome.error = Some(format!("處理衝突 {path} 失敗: {err:#}")),
                }
            }
            SyncAction::CreateLocalDir { .. }
            | SyncAction::CreateRemoteDir { .. }
            | SyncAction::DeleteLocalDir { .. }
            | SyncAction::DeleteRemoteDir { .. } => {
                unreachable!(
                    "classify() never produces directory actions — classify_directories() does, and its \
                     output is executed in a separate block after this loop (see a later task in this plan)"
                )
            }
        }
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test sync:: 2>&1 | tail -60`
Expected: all new `classify_directories` tests pass, alongside every existing `classify` test (unaffected).

- [ ] **Step 5: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -30` — expect clean build.
Run: `cargo test 2>&1 | tail -15` — expect all tests passing (97 + 9 new = 106).

- [ ] **Step 6: Commit**

```bash
git add src/plugins/sync.rs
git commit -m "$(cat <<'EOF'
新增 classify_directories：目錄建立/刪除分類

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Cross-domain `StorageRename` + `SyncTransport::rename`

**Files:**
- Modify: `src/plugin.rs`
- Modify: `src/shell.rs`
- Modify: `src/plugins/global.rs`
- Modify: `src/plugins/sync.rs`

**Interfaces:**
- Consumes: existing `storage::rename_path` (already `pub(crate)`, re-exported via `crate::plugins::rename_path`), existing `safe_storage_path`, existing `CrossDomainAsk`/`RemoteRequest`/`RemoteReply`/`send_cross_domain_request` machinery.
- Produces: `CrossDomainAsk::StorageRename { from: String, to: String }`; `SyncTransport` trait gains `fn rename(&self, from: &str, to: &str) -> Result<()>`, implemented by both `HttpTransport` (hits the existing `/api/storage/rename` HTTP endpoint) and `CrossDomainTransport` (sends the new `CrossDomainAsk::StorageRename`).
- Consumed by Task 4 (`run_sync_pass` calls `transport.rename(...)` to execute `MoveRemote` actions).

This task does not touch `SyncAction` or any `match` over it — it only adds a method to the `SyncTransport` trait and its two implementations, plus the cross-domain plumbing for the new request kind. No risk of breaking exhaustiveness elsewhere.

**No new tests in this task** — matches the existing convention that `HttpTransport`/`CrossDomainTransport` methods, and `global.rs`'s `build_remote_reply` match arms, are not unit tested (verified via `cargo build`/`cargo test` for no regressions, plus Task 4/5's manual smoke test will exercise this end-to-end).

- [ ] **Step 1: Add `CrossDomainAsk::StorageRename` and `RemoteRequest::StorageRename`**

In `src/plugin.rs`, change:

```rust
#[derive(Clone, Serialize, Deserialize)]
pub enum CrossDomainAsk {
    Exec { target_id: String, line: String },
    Panel { target_id: String, panel_name: String },
    FileList { target_id: String, folder: String, offset: usize },
    FilePull { target_id: String, folder: String, name: String, offset: u64 },
    FilePush { target_id: String, folder: String, name: String, offset: u64, data: String },
    StorageManifest { offset: usize },
    StorageFilePull { path: String, offset: u64 },
    StorageFilePush { path: String, offset: u64, data: String },
    StorageMkdir { path: String },
    StorageDelete { path: String, recursive: bool },
}
```

to:

```rust
#[derive(Clone, Serialize, Deserialize)]
pub enum CrossDomainAsk {
    Exec { target_id: String, line: String },
    Panel { target_id: String, panel_name: String },
    FileList { target_id: String, folder: String, offset: usize },
    FilePull { target_id: String, folder: String, name: String, offset: u64 },
    FilePush { target_id: String, folder: String, name: String, offset: u64, data: String },
    StorageManifest { offset: usize },
    StorageFilePull { path: String, offset: u64 },
    StorageFilePush { path: String, offset: u64, data: String },
    StorageMkdir { path: String },
    StorageDelete { path: String, recursive: bool },
    StorageRename { from: String, to: String },
}
```

Change:

```rust
#[derive(Clone, Serialize, Deserialize)]
pub enum RemoteRequest {
    Exec { request_id: String, source_domain: String, target_id: String, line: String },
    Panel { request_id: String, source_domain: String, target_id: String, panel_name: String },
    FileList { request_id: String, source_domain: String, target_id: String, folder: String, offset: usize },
    FilePull { request_id: String, source_domain: String, target_id: String, folder: String, name: String, offset: u64 },
    FilePush { request_id: String, source_domain: String, target_id: String, folder: String, name: String, offset: u64, data: String },
    StorageManifest { request_id: String, source_domain: String, offset: usize },
    StorageFilePull { request_id: String, source_domain: String, path: String, offset: u64 },
    StorageFilePush { request_id: String, source_domain: String, path: String, offset: u64, data: String },
    StorageMkdir { request_id: String, source_domain: String, path: String },
    StorageDelete { request_id: String, source_domain: String, path: String, recursive: bool },
}

impl RemoteRequest {
    pub fn source_domain(&self) -> &str {
        match self {
            RemoteRequest::Exec { source_domain, .. }
            | RemoteRequest::Panel { source_domain, .. }
            | RemoteRequest::FileList { source_domain, .. }
            | RemoteRequest::FilePull { source_domain, .. }
            | RemoteRequest::FilePush { source_domain, .. }
            | RemoteRequest::StorageManifest { source_domain, .. }
            | RemoteRequest::StorageFilePull { source_domain, .. }
            | RemoteRequest::StorageFilePush { source_domain, .. }
            | RemoteRequest::StorageMkdir { source_domain, .. }
            | RemoteRequest::StorageDelete { source_domain, .. } => source_domain,
        }
    }
}
```

to:

```rust
#[derive(Clone, Serialize, Deserialize)]
pub enum RemoteRequest {
    Exec { request_id: String, source_domain: String, target_id: String, line: String },
    Panel { request_id: String, source_domain: String, target_id: String, panel_name: String },
    FileList { request_id: String, source_domain: String, target_id: String, folder: String, offset: usize },
    FilePull { request_id: String, source_domain: String, target_id: String, folder: String, name: String, offset: u64 },
    FilePush { request_id: String, source_domain: String, target_id: String, folder: String, name: String, offset: u64, data: String },
    StorageManifest { request_id: String, source_domain: String, offset: usize },
    StorageFilePull { request_id: String, source_domain: String, path: String, offset: u64 },
    StorageFilePush { request_id: String, source_domain: String, path: String, offset: u64, data: String },
    StorageMkdir { request_id: String, source_domain: String, path: String },
    StorageDelete { request_id: String, source_domain: String, path: String, recursive: bool },
    StorageRename { request_id: String, source_domain: String, from: String, to: String },
}

impl RemoteRequest {
    pub fn source_domain(&self) -> &str {
        match self {
            RemoteRequest::Exec { source_domain, .. }
            | RemoteRequest::Panel { source_domain, .. }
            | RemoteRequest::FileList { source_domain, .. }
            | RemoteRequest::FilePull { source_domain, .. }
            | RemoteRequest::FilePush { source_domain, .. }
            | RemoteRequest::StorageManifest { source_domain, .. }
            | RemoteRequest::StorageFilePull { source_domain, .. }
            | RemoteRequest::StorageFilePush { source_domain, .. }
            | RemoteRequest::StorageMkdir { source_domain, .. }
            | RemoteRequest::StorageDelete { source_domain, .. }
            | RemoteRequest::StorageRename { source_domain, .. } => source_domain,
        }
    }
}
```

- [ ] **Step 2: Wire the new variant through `shell.rs`**

In `src/shell.rs`, change:

```rust
fn cross_domain_timeout(ask: &CrossDomainAsk) -> Duration {
    match ask {
        CrossDomainAsk::Exec { .. } | CrossDomainAsk::Panel { .. } => Duration::from_secs(5),
        CrossDomainAsk::FileList { .. }
        | CrossDomainAsk::FilePull { .. }
        | CrossDomainAsk::FilePush { .. }
        | CrossDomainAsk::StorageManifest { .. }
        | CrossDomainAsk::StorageFilePull { .. }
        | CrossDomainAsk::StorageFilePush { .. }
        | CrossDomainAsk::StorageMkdir { .. }
        | CrossDomainAsk::StorageDelete { .. } => Duration::from_secs(20),
    }
}
```

to:

```rust
fn cross_domain_timeout(ask: &CrossDomainAsk) -> Duration {
    match ask {
        CrossDomainAsk::Exec { .. } | CrossDomainAsk::Panel { .. } => Duration::from_secs(5),
        CrossDomainAsk::FileList { .. }
        | CrossDomainAsk::FilePull { .. }
        | CrossDomainAsk::FilePush { .. }
        | CrossDomainAsk::StorageManifest { .. }
        | CrossDomainAsk::StorageFilePull { .. }
        | CrossDomainAsk::StorageFilePush { .. }
        | CrossDomainAsk::StorageMkdir { .. }
        | CrossDomainAsk::StorageDelete { .. }
        | CrossDomainAsk::StorageRename { .. } => Duration::from_secs(20),
    }
}
```

Change:

```rust
        CrossDomainAsk::StorageMkdir { path } => {
            RemoteRequest::StorageMkdir { request_id: request_id.clone(), source_domain, path }
        }
        CrossDomainAsk::StorageDelete { path, recursive } => {
            RemoteRequest::StorageDelete { request_id: request_id.clone(), source_domain, path, recursive }
        }
    };
```

to:

```rust
        CrossDomainAsk::StorageMkdir { path } => {
            RemoteRequest::StorageMkdir { request_id: request_id.clone(), source_domain, path }
        }
        CrossDomainAsk::StorageDelete { path, recursive } => {
            RemoteRequest::StorageDelete { request_id: request_id.clone(), source_domain, path, recursive }
        }
        CrossDomainAsk::StorageRename { from, to } => {
            RemoteRequest::StorageRename { request_id: request_id.clone(), source_domain, from, to }
        }
    };
```

- [ ] **Step 3: Handle `RemoteRequest::StorageRename` in `global.rs`**

In `src/plugins/global.rs`, add `rename_path` to the existing import from `crate::plugins`. Change:

```rust
use crate::plugins::{
    make_dir, paginate_sync_entries, read_chunk, remove, safe_file_path, safe_storage_path, url_encode_filename,
    walk_with_hashes, write_chunk, ALLOWED_FOLDERS, REPORT_INTERVAL, STORAGE_DIR,
};
```

to:

```rust
use crate::plugins::{
    make_dir, paginate_sync_entries, read_chunk, remove, rename_path, safe_file_path, safe_storage_path,
    url_encode_filename, walk_with_hashes, write_chunk, ALLOWED_FOLDERS, REPORT_INTERVAL, STORAGE_DIR,
};
```

In `build_remote_reply`, change:

```rust
        RemoteRequest::StorageDelete { request_id, path, recursive, .. } => {
            let Some(target) = safe_storage_path(Path::new(STORAGE_DIR), path) else {
                return RemoteReply::Error {
                    request_id: request_id.clone(),
                    message: format!("不合法的路徑: {path}"),
                };
            };
            match remove(&target, *recursive) {
                Ok(()) => RemoteReply::Ack { request_id: request_id.clone() },
                Err(err) => RemoteReply::Error { request_id: request_id.clone(), message: format!("{err:#}") },
            }
        }
    }
}
```

to:

```rust
        RemoteRequest::StorageDelete { request_id, path, recursive, .. } => {
            let Some(target) = safe_storage_path(Path::new(STORAGE_DIR), path) else {
                return RemoteReply::Error {
                    request_id: request_id.clone(),
                    message: format!("不合法的路徑: {path}"),
                };
            };
            match remove(&target, *recursive) {
                Ok(()) => RemoteReply::Ack { request_id: request_id.clone() },
                Err(err) => RemoteReply::Error { request_id: request_id.clone(), message: format!("{err:#}") },
            }
        }
        RemoteRequest::StorageRename { request_id, from, to, .. } => {
            let (Some(from_path), Some(to_path)) = (
                safe_storage_path(Path::new(STORAGE_DIR), from),
                safe_storage_path(Path::new(STORAGE_DIR), to),
            ) else {
                return RemoteReply::Error {
                    request_id: request_id.clone(),
                    message: format!("不合法的路徑: {from} -> {to}"),
                };
            };
            match rename_path(&from_path, &to_path) {
                Ok(()) => RemoteReply::Ack { request_id: request_id.clone() },
                Err(err) => RemoteReply::Error { request_id: request_id.clone(), message: format!("{err:#}") },
            }
        }
    }
}
```

Change `request_kind` from:

```rust
fn request_kind(request: &RemoteRequest) -> &'static str {
    match request {
        RemoteRequest::Exec { .. } => "Exec",
        RemoteRequest::Panel { .. } => "Panel",
        RemoteRequest::FileList { .. } => "FileList",
        RemoteRequest::FilePull { .. } => "FilePull",
        RemoteRequest::FilePush { .. } => "FilePush",
        RemoteRequest::StorageManifest { .. } => "StorageManifest",
        RemoteRequest::StorageFilePull { .. } => "StorageFilePull",
        RemoteRequest::StorageFilePush { .. } => "StorageFilePush",
        RemoteRequest::StorageMkdir { .. } => "StorageMkdir",
        RemoteRequest::StorageDelete { .. } => "StorageDelete",
    }
}
```

to:

```rust
fn request_kind(request: &RemoteRequest) -> &'static str {
    match request {
        RemoteRequest::Exec { .. } => "Exec",
        RemoteRequest::Panel { .. } => "Panel",
        RemoteRequest::FileList { .. } => "FileList",
        RemoteRequest::FilePull { .. } => "FilePull",
        RemoteRequest::FilePush { .. } => "FilePush",
        RemoteRequest::StorageManifest { .. } => "StorageManifest",
        RemoteRequest::StorageFilePull { .. } => "StorageFilePull",
        RemoteRequest::StorageFilePush { .. } => "StorageFilePush",
        RemoteRequest::StorageMkdir { .. } => "StorageMkdir",
        RemoteRequest::StorageDelete { .. } => "StorageDelete",
        RemoteRequest::StorageRename { .. } => "StorageRename",
    }
}
```

- [ ] **Step 4: Add `rename` to `SyncTransport` and both implementations**

In `src/plugins/sync.rs`, change the `SyncTransport` trait from:

```rust
pub(crate) trait SyncTransport {
    /// 取得對方目前整棵 `storage/` 樹的清單（含每個檔案的 hash）。
    fn manifest(&self) -> Result<Vec<SyncEntry>>;
    /// 把對方 `path` 這個檔案下載到本機的 `dest` 路徑；`expected_size` 是這個
    /// 檔案在對方那邊的大小（來自觸發這次下載的 manifest 條目），`HttpTransport`
    /// 用不到（HTTP 下載讀到 EOF 就結束），`CrossDomainTransport`（Task 6）需要
    /// 靠它判斷分段下載何時結束。
    fn download_to(&self, path: &str, expected_size: u64, dest: &Path) -> Result<()>;
    /// 把本機 `src` 這個檔案上傳成對方的 `path`。
    fn upload_from(&self, path: &str, src: &Path) -> Result<()>;
    /// 在對方建立 `path` 這個資料夾。
    fn mkdir(&self, path: &str) -> Result<()>;
    /// 刪除對方的 `path`（檔案或資料夾，`recursive` 語意跟 `storage` plugin
    /// 的 `remove` 一致）。
    fn delete(&self, path: &str, recursive: bool) -> Result<()>;
}
```

to:

```rust
pub(crate) trait SyncTransport {
    /// 取得對方目前整棵 `storage/` 樹的清單（含每個檔案的 hash）。
    fn manifest(&self) -> Result<Vec<SyncEntry>>;
    /// 把對方 `path` 這個檔案下載到本機的 `dest` 路徑；`expected_size` 是這個
    /// 檔案在對方那邊的大小（來自觸發這次下載的 manifest 條目），`HttpTransport`
    /// 用不到（HTTP 下載讀到 EOF 就結束），`CrossDomainTransport`（Task 6）需要
    /// 靠它判斷分段下載何時結束。
    fn download_to(&self, path: &str, expected_size: u64, dest: &Path) -> Result<()>;
    /// 把本機 `src` 這個檔案上傳成對方的 `path`。
    fn upload_from(&self, path: &str, src: &Path) -> Result<()>;
    /// 在對方建立 `path` 這個資料夾。
    fn mkdir(&self, path: &str) -> Result<()>;
    /// 刪除對方的 `path`（檔案或資料夾，`recursive` 語意跟 `storage` plugin
    /// 的 `remove` 一致）。
    fn delete(&self, path: &str, recursive: bool) -> Result<()>;
    /// 在對方那邊直接把 `from` 重新命名/搬移成 `to`，不重傳內容——呼叫端要
    /// 先確保 `to` 的上層目錄在對方那邊存在（見 `ensure_remote_parent_dirs`），
    /// 這個方法本身不會自動建立父目錄。
    fn rename(&self, from: &str, to: &str) -> Result<()>;
}
```

In `HttpTransport`'s `impl SyncTransport for HttpTransport`, add this method (after `delete`, before the closing `}` of the impl block):

```rust
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let url = format!(
            "http://{}:{PORT}/api/storage/rename?from={}&to={}",
            self.ip,
            url_encode_filename(from),
            url_encode_filename(to)
        );
        let output = Command::new("curl")
            .args(["--silent", "--fail", "--max-time", "10", "-X", "POST", &url])
            .output()
            .context("執行 curl 失敗")?;
        if !output.status.success() {
            bail!("搬移/重新命名失敗: {from} -> {to}");
        }
        Ok(())
    }
```

In `CrossDomainTransport`'s `impl SyncTransport for CrossDomainTransport`, add this method (after `delete`, before the closing `}` of the impl block):

```rust
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let ask = CrossDomainAsk::StorageRename { from: from.to_string(), to: to.to_string() };
        match send_cross_domain_request(&self.ctx, &self.domain, ask)? {
            RemoteReply::Ack { .. } => Ok(()),
            RemoteReply::Error { message, .. } => bail!(message),
            _ => bail!("收到不符預期的回覆型別"),
        }
    }
```

In the test module's `MkdirTrackingTransport` (used only to test `ensure_remote_parent_dirs`), add the new required method so the `impl SyncTransport` block still compiles:

```rust
        fn delete(&self, _path: &str, _recursive: bool) -> Result<()> {
            unimplemented!("not exercised by this test")
        }
```

to:

```rust
        fn delete(&self, _path: &str, _recursive: bool) -> Result<()> {
            unimplemented!("not exercised by this test")
        }
        fn rename(&self, _from: &str, _to: &str) -> Result<()> {
            unimplemented!("not exercised by this test")
        }
```

- [ ] **Step 5: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -30` — expect clean build (this is a mechanical, no-test-impact task; if `cargo build` fails, the most likely cause is a missed match arm — re-check every `match` over `CrossDomainAsk`/`RemoteRequest` for exhaustiveness).
Run: `cargo test 2>&1 | tail -15` — expect the same 106 tests passing, no new ones.

- [ ] **Step 6: Commit**

```bash
git add src/plugin.rs src/shell.rs src/plugins/global.rs src/plugins/sync.rs
git commit -m "$(cat <<'EOF'
新增跨 domain StorageRename 與 SyncTransport::rename

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Single-sided move/rename pairing in `classify`, and its execution in `run_sync_pass`

**Files:**
- Modify: `src/plugins/sync.rs`

**Interfaces:**
- Consumes: `Baseline.files` (Task 1), `SyncTransport::rename` (Task 3), existing `ensure_remote_parent_dirs`, existing `crate::plugins::rename_path`.
- Produces: `SyncAction` gains `MoveLocal { from: String, to: String }` and `MoveRemote { from: String, to: String }`. `classify`'s existing signature is unchanged — the new pairing step runs internally before it returns. `SyncOutcome` gains `moved_local: usize`, `moved_remote: usize`.

**Why classify's pairing and run_sync_pass's execution are one task:** `run_sync_pass`'s main loop matches `SyncAction` exhaustively. The moment `classify()` can produce `MoveLocal`/`MoveRemote` (which happens as soon as the pairing logic is added), those variants are no longer merely theoretical — a live background sync round could hit them. Splitting "make classify produce Move actions" and "make run_sync_pass execute them" into two separate tasks/commits would mean the in-between commit either fails to build (non-exhaustive match) or, if patched with an `unreachable!()` placeholder, panics the live background sync thread the first time a real rename is detected. Both changes land together.

- [ ] **Step 1: Write the failing tests for the classify-level pairing**

Add these to the existing `#[cfg(test)] mod tests { ... }` block in `src/plugins/sync.rs`:

```rust
    #[test]
    fn local_rename_pairs_into_move_remote() {
        // 本機把 old.jpg 改名成 new.jpg（同一份內容 h1）：baseline 有
        // old.jpg，這一輪 local 沒有 old.jpg、有 new.jpg（baseline 沒有
        // new.jpg 的記錄）。不應該各自變成 DeleteRemote/PushToRemote，應該
        // 合併成一個 MoveRemote。
        let local = vec![file("new.jpg", "h1")];
        let remote = vec![file("old.jpg", "h1")];
        let baseline = baseline_of(&[("old.jpg", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::MoveRemote { from: "old.jpg".to_string(), to: "new.jpg".to_string() }]);
    }

    #[test]
    fn remote_rename_pairs_into_move_local() {
        let local = vec![file("old.jpg", "h1")];
        let remote = vec![file("new.jpg", "h1")];
        let baseline = baseline_of(&[("old.jpg", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::MoveLocal { from: "old.jpg".to_string(), to: "new.jpg".to_string() }]);
    }

    #[test]
    fn duplicate_hash_rename_pairs_by_sorted_path() {
        // 兩張內容完全相同的照片（都是 h1）同時被改名：a-old/b-old 都刪了，
        // a-new/b-new 都是新的。依路徑字串排序後依序配對：a-new 配 a-old，
        // b-new 配 b-old（不是隨機順序）。
        let local = vec![file("a-new.jpg", "h1"), file("b-new.jpg", "h1")];
        let remote = vec![file("a-old.jpg", "h1"), file("b-old.jpg", "h1")];
        let baseline = baseline_of(&[("a-old.jpg", "h1", "h1"), ("b-old.jpg", "h1", "h1")]);
        let mut actions = classify(&local, &remote, &baseline);
        actions.sort_by_key(|a| match a {
            SyncAction::MoveRemote { from, .. } => from.clone(),
            _ => String::new(),
        });
        assert_eq!(
            actions,
            vec![
                SyncAction::MoveRemote { from: "a-old.jpg".to_string(), to: "a-new.jpg".to_string() },
                SyncAction::MoveRemote { from: "b-old.jpg".to_string(), to: "b-new.jpg".to_string() },
            ]
        );
    }

    #[test]
    fn duplicate_hash_uneven_counts_falls_back_for_leftover() {
        // 兩個刪除候選（同 hash h1）只有一個新增候選——只配對得出一對 Move，
        // 剩下那個刪除候選維持原本的 DeleteRemote。
        let local = vec![file("a-new.jpg", "h1")];
        let remote = vec![file("a-old.jpg", "h1"), file("b-old.jpg", "h1")];
        let baseline = baseline_of(&[("a-old.jpg", "h1", "h1"), ("b-old.jpg", "h1", "h1")]);
        let mut actions = classify(&local, &remote, &baseline);
        actions.sort_by_key(|a| match a {
            SyncAction::MoveRemote { from, .. } => format!("0{from}"),
            SyncAction::DeleteRemote { path } => format!("1{path}"),
            _ => String::new(),
        });
        assert_eq!(
            actions,
            vec![
                SyncAction::MoveRemote { from: "a-old.jpg".to_string(), to: "a-new.jpg".to_string() },
                SyncAction::DeleteRemote { path: "b-old.jpg".to_string() },
            ]
        );
    }

    #[test]
    fn conflict_paths_are_not_paired_into_moves() {
        // f.txt 在 baseline 有記錄、雙邊都改過而且改成不一樣（真衝突），同一
        // 輪 local 又冒出一個內容剛好等於 baseline 舊值的全新路徑——不應該把
        // 這個衝突誤配成「改名」。
        let local = vec![file("f.txt", "h2"), file("new.txt", "h1")];
        let remote = vec![file("f.txt", "h3")];
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let mut actions = classify(&local, &remote, &baseline);
        actions.sort_by_key(|a| format!("{a:?}"));
        assert_eq!(
            actions,
            vec![
                SyncAction::Conflict { path: "f.txt".to_string() },
                SyncAction::PushToRemote { path: "new.txt".to_string() },
            ]
        );
    }

    #[test]
    fn single_side_modification_is_not_treated_as_new_for_pairing() {
        // f.txt 是 baseline 已經記錄的路徑、本機單邊修改過（PushToRemote），
        // 不是「全新路徑」，不該被拿去跟任何刪除候選配對成 Move。
        let local = vec![file("f.txt", "h2"), file("moved-away.txt", "hX")];
        let remote = vec![file("gone.txt", "hX")];
        let baseline = baseline_of(&[("f.txt", "h1", "h1"), ("gone.txt", "hX", "hX")]);
        let actions = classify(&local, &remote, &baseline);
        // moved-away.txt (新路徑, hash hX) 跟 gone.txt (刪除候選, hash hX)
        // 內容相同、都在「local 這一側」——這才是預期會配對成的 Move；f.txt
        // 的單邊修改必須維持是 PushToRemote，不能被誤吃進配對邏輯。
        let mut sorted = actions.clone();
        sorted.sort_by_key(|a| format!("{a:?}"));
        assert_eq!(
            sorted,
            vec![
                SyncAction::MoveRemote { from: "gone.txt".to_string(), to: "moved-away.txt".to_string() },
                SyncAction::PushToRemote { path: "f.txt".to_string() },
            ]
        );
    }

    #[test]
    fn cross_side_delete_and_add_do_not_pair() {
        // 刪除候選在 remote 那一側（DeleteLocal：local 還留著 gone.txt、內容
        // 沒變，remote 已經沒有它了），新增候選在 local 那一側（PushToRemote：
        // new.txt 是全新路徑）——就算 hash 剛好一樣，這兩個屬於不同的配對
        // bucket（DeleteLocal 只會去配 PullFromRemote，不會去配
        // PushToRemote），不該被誤配成 Move。
        let local = vec![file("gone.txt", "h1"), file("new.txt", "h1")];
        let remote = vec![];
        let baseline = baseline_of(&[("gone.txt", "h1", "h1")]);
        let mut actions = classify(&local, &remote, &baseline);
        actions.sort_by_key(|a| format!("{a:?}"));
        assert_eq!(
            actions,
            vec![
                SyncAction::DeleteLocal { path: "gone.txt".to_string() },
                SyncAction::PushToRemote { path: "new.txt".to_string() },
            ]
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test sync:: 2>&1 | tail -60`
Expected: compile errors (`no variant named 'MoveRemote'`/`'MoveLocal'`) or assertion failures showing the old `DeleteRemote`/`PushToRemote` pairs instead of merged `Move*` actions.

- [ ] **Step 3: Add the new `SyncAction` variants and the pairing post-process in `classify`**

Change the `SyncAction` enum (from Task 2's version) to add two more variants:

```rust
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SyncAction {
    PushToRemote { path: String },
    PullFromRemote { path: String },
    DeleteLocal { path: String },
    DeleteRemote { path: String },
    Conflict { path: String },
    MoveLocal { from: String, to: String },
    MoveRemote { from: String, to: String },
    CreateLocalDir { path: String },
    CreateRemoteDir { path: String },
    DeleteLocalDir { path: String },
    DeleteRemoteDir { path: String },
}
```

Change the end of `classify` from:

```rust
            (Some(local), Some(remote), base) => {
                if local.hash == remote.hash {
                    continue; // 內容一樣，不用管有沒有 baseline
                }
                match base {
                    None => actions.push(SyncAction::Conflict { path: path.clone() }),
                    Some(base) => {
                        // 分開比對本機、對方各自跟「自己那一邊上次記錄的值」
                        // 有沒有變——`base.local_hash` 跟 `base.remote_hash`
                        // 本來就可能不一樣（已經確認過的衝突就是這樣故意留
                        // 著），不能假設兩者相等。
                        let local_changed = local.hash != base.local_hash;
                        let remote_changed = remote.hash != base.remote_hash;
                        match (local_changed, remote_changed) {
                            (true, false) => actions.push(SyncAction::PushToRemote { path: path.clone() }),
                            (false, true) => actions.push(SyncAction::PullFromRemote { path: path.clone() }),
                            (false, false) => {
                                // 兩邊都跟上次記錄的一樣——這是先前已經確認、
                                // 故意保留分歧的衝突，不用再產生一次衝突副本。
                                continue;
                            }
                            (true, true) => actions.push(SyncAction::Conflict { path: path.clone() }),
                        }
                    }
                }
            }
        }
    }
    actions
}
```

to:

```rust
            (Some(local), Some(remote), base) => {
                if local.hash == remote.hash {
                    continue; // 內容一樣，不用管有沒有 baseline
                }
                match base {
                    None => actions.push(SyncAction::Conflict { path: path.clone() }),
                    Some(base) => {
                        // 分開比對本機、對方各自跟「自己那一邊上次記錄的值」
                        // 有沒有變——`base.local_hash` 跟 `base.remote_hash`
                        // 本來就可能不一樣（已經確認過的衝突就是這樣故意留
                        // 著），不能假設兩者相等。
                        let local_changed = local.hash != base.local_hash;
                        let remote_changed = remote.hash != base.remote_hash;
                        match (local_changed, remote_changed) {
                            (true, false) => actions.push(SyncAction::PushToRemote { path: path.clone() }),
                            (false, true) => actions.push(SyncAction::PullFromRemote { path: path.clone() }),
                            (false, false) => {
                                // 兩邊都跟上次記錄的一樣——這是先前已經確認、
                                // 故意保留分歧的衝突，不用再產生一次衝突副本。
                                continue;
                            }
                            (true, true) => actions.push(SyncAction::Conflict { path: path.clone() }),
                        }
                    }
                }
            }
        }
    }
    pair_moves(actions, &local_states, &remote_states, baseline)
}

/// 單側改名/搬移配對：把「這輪本來要各自輸出的 `DeleteRemote`」（本機刪了一
/// 個 baseline 有記錄的路徑）跟「這輪本來要各自輸出的 `PushToRemote`、且該
/// 路徑在 baseline 裡完全沒有記錄」（本機的全新檔案，不是單邊修改既有路徑）
/// 依內容 hash 配對，配成功的合併成一個 `MoveRemote`，不再各自輸出。對稱地，
/// `DeleteLocal` 配「baseline 沒有記錄的 `PullFromRemote`」，合併成
/// `MoveLocal`。只在同一側配對（不會拿 `DeleteRemote` 去配
/// `PullFromRemote`），`Conflict`、或「baseline 有記錄的單邊修改」都不參與
/// 配對——只有乾淨的「刪除」跟「全新新增」才可能是同一次改名/搬移的兩端。
fn pair_moves(
    actions: Vec<SyncAction>,
    local_states: &HashMap<String, FileState>,
    remote_states: &HashMap<String, FileState>,
    baseline: &Baseline,
) -> Vec<SyncAction> {
    let local_side = pair_side(
        &actions,
        |a| match a {
            SyncAction::DeleteRemote { path } => baseline.files.get(path).map(|b| (path.clone(), b.local_hash.clone())),
            _ => None,
        },
        |a| match a {
            SyncAction::PushToRemote { path } if !baseline.files.contains_key(path) => {
                local_states.get(path).map(|s| (path.clone(), s.hash.clone()))
            }
            _ => None,
        },
    );
    let remote_side = pair_side(
        &actions,
        |a| match a {
            SyncAction::DeleteLocal { path } => baseline.files.get(path).map(|b| (path.clone(), b.remote_hash.clone())),
            _ => None,
        },
        |a| match a {
            SyncAction::PullFromRemote { path } if !baseline.files.contains_key(path) => {
                remote_states.get(path).map(|s| (path.clone(), s.hash.clone()))
            }
            _ => None,
        },
    );

    let mut moved_away: HashSet<String> = HashSet::new();
    let mut moved_new: HashSet<String> = HashSet::new();
    let mut moves = Vec::new();
    for (from, to) in local_side {
        moved_away.insert(from.clone());
        moved_new.insert(to.clone());
        moves.push(SyncAction::MoveRemote { from, to });
    }
    for (from, to) in remote_side {
        moved_away.insert(from.clone());
        moved_new.insert(to.clone());
        moves.push(SyncAction::MoveLocal { from, to });
    }

    let mut result: Vec<SyncAction> = actions
        .into_iter()
        .filter(|a| match a {
            SyncAction::DeleteRemote { path } | SyncAction::DeleteLocal { path } => !moved_away.contains(path),
            SyncAction::PushToRemote { path } | SyncAction::PullFromRemote { path } => !moved_new.contains(path),
            _ => true,
        })
        .collect();
    result.extend(moves);
    result
}

/// `pair_moves` 的配對核心：把 `actions` 篩成「刪除候選」跟「新增候選」（各
/// 自用 `del_of`/`add_of` 從單一個 `SyncAction` 抽出 `(路徑, hash)`，不符合
/// 就回 `None`），依 hash 分組、組內依路徑字串排序後依序配對（結果穩定、可
/// 重現），回傳 `(舊路徑, 新路徑)` 配對清單；配不完的候選不會出現在回傳值
/// 裡，維持原本各自的動作。
fn pair_side(
    actions: &[SyncAction],
    del_of: impl Fn(&SyncAction) -> Option<(String, String)>,
    add_of: impl Fn(&SyncAction) -> Option<(String, String)>,
) -> Vec<(String, String)> {
    let mut deletes: HashMap<String, Vec<String>> = HashMap::new();
    let mut adds: HashMap<String, Vec<String>> = HashMap::new();
    for action in actions {
        if let Some((path, hash)) = del_of(action) {
            deletes.entry(hash).or_default().push(path);
        }
        if let Some((path, hash)) = add_of(action) {
            adds.entry(hash).or_default().push(path);
        }
    }
    let mut pairs = Vec::new();
    for (hash, mut del_paths) in deletes {
        let Some(mut add_paths) = adds.remove(&hash) else { continue };
        del_paths.sort();
        add_paths.sort();
        for (from, to) in del_paths.into_iter().zip(add_paths) {
            pairs.push((from, to));
        }
    }
    pairs
}
```

- [ ] **Step 4: Run the classify-level tests to verify they pass**

Run: `cargo test sync:: 2>&1 | tail -80`
Expected: all new pairing tests pass, alongside every existing `classify` test (none of them contain a same-side delete+add pair with matching hash, so `pair_moves` is a no-op for all of them).

Note: at this exact point (before Step 5 below), `cargo build` for the whole crate **will fail** — `run_sync_pass`'s main loop match is no longer exhaustive (it doesn't yet have arms for `MoveLocal`/`MoveRemote`). This is expected and temporary within this task; Step 5 fixes it. Do not commit between Step 4 and Step 5.

- [ ] **Step 5: Add `SyncOutcome` fields and execute `MoveLocal`/`MoveRemote` in `run_sync_pass`**

Change `SyncOutcome` from:

```rust
/// 一輪同步對某個對象的結果摘要，`sync status` 指令跟 panel 都讀這個。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SyncOutcome {
    pub(crate) pushed: usize,
    pub(crate) pulled: usize,
    pub(crate) deleted_local: usize,
    pub(crate) deleted_remote: usize,
    pub(crate) conflicts: usize,
    pub(crate) error: Option<String>,
}
```

to:

```rust
/// 一輪同步對某個對象的結果摘要，`sync status` 指令跟 panel 都讀這個。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SyncOutcome {
    pub(crate) pushed: usize,
    pub(crate) pulled: usize,
    pub(crate) deleted_local: usize,
    pub(crate) deleted_remote: usize,
    pub(crate) conflicts: usize,
    pub(crate) moved_local: usize,
    pub(crate) moved_remote: usize,
    pub(crate) error: Option<String>,
}
```

In `run_sync_pass`, change the end of the main `match action { ... }` block from:

```rust
            SyncAction::Conflict { path } => {
                let remote_size = find_size(&remote, &path);
                match resolve_conflict(transport, &path, my_label, partner_label, remote_size) {
                    Ok(()) => {
                        // 只更新原本這個路徑的 baseline（承認兩邊從此故意分歧），
                        // 不替新產生的兩份衝突副本建立 baseline 條目——見本
                        // task 開頭的「已知取捨」說明。
                        if let (Some(local_hash), Some(remote_hash)) =
                            (find_hash(&local, &path), find_hash(&remote, &path))
                        {
                            baseline.files.insert(
                                path,
                                BaselineEntry {
                                    local_hash: local_hash.to_string(),
                                    remote_hash: remote_hash.to_string(),
                                },
                            );
                        }
                        outcome.conflicts += 1;
                    }
                    Err(err) => outcome.error = Some(format!("處理衝突 {path} 失敗: {err:#}")),
                }
            }
            SyncAction::CreateLocalDir { .. }
            | SyncAction::CreateRemoteDir { .. }
            | SyncAction::DeleteLocalDir { .. }
            | SyncAction::DeleteRemoteDir { .. } => {
                unreachable!(
                    "classify() never produces directory actions — classify_directories() does, and its \
                     output is executed in a separate block after this loop (see a later task in this plan)"
                )
            }
        }
    }
```

to:

```rust
            SyncAction::Conflict { path } => {
                let remote_size = find_size(&remote, &path);
                match resolve_conflict(transport, &path, my_label, partner_label, remote_size) {
                    Ok(()) => {
                        // 只更新原本這個路徑的 baseline（承認兩邊從此故意分歧），
                        // 不替新產生的兩份衝突副本建立 baseline 條目——見本
                        // task 開頭的「已知取捨」說明。
                        if let (Some(local_hash), Some(remote_hash)) =
                            (find_hash(&local, &path), find_hash(&remote, &path))
                        {
                            baseline.files.insert(
                                path,
                                BaselineEntry {
                                    local_hash: local_hash.to_string(),
                                    remote_hash: remote_hash.to_string(),
                                },
                            );
                        }
                        outcome.conflicts += 1;
                    }
                    Err(err) => outcome.error = Some(format!("處理衝突 {path} 失敗: {err:#}")),
                }
            }
            SyncAction::MoveRemote { from, to } => {
                let result = ensure_remote_parent_dirs(transport, &to, &mut known_remote_dirs)
                    .and_then(|_| transport.rename(&from, &to));
                match result {
                    Ok(()) => {
                        if let Some(hash) = find_hash(&local, &to) {
                            baseline.files.remove(&from);
                            baseline.files.insert(
                                to,
                                BaselineEntry { local_hash: hash.to_string(), remote_hash: hash.to_string() },
                            );
                        }
                        outcome.moved_remote += 1;
                    }
                    Err(err) => outcome.error = Some(format!("搬移對方 {from} -> {to} 失敗: {err:#}")),
                }
            }
            SyncAction::MoveLocal { from, to } => {
                let from_path = local_root.join(&from);
                let to_path = local_root.join(&to);
                let result = (|| -> Result<()> {
                    if let Some(parent) = to_path.parent() {
                        fs::create_dir_all(parent)
                            .with_context(|| format!("建立資料夾失敗: {}", parent.display()))?;
                    }
                    crate::plugins::rename_path(&from_path, &to_path)
                })();
                match result {
                    Ok(()) => {
                        if let Some(hash) = find_hash(&remote, &to) {
                            baseline.files.remove(&from);
                            baseline.files.insert(
                                to,
                                BaselineEntry { local_hash: hash.to_string(), remote_hash: hash.to_string() },
                            );
                        }
                        outcome.moved_local += 1;
                    }
                    Err(err) => outcome.error = Some(format!("搬移本機 {from} -> {to} 失敗: {err:#}")),
                }
            }
            SyncAction::CreateLocalDir { .. }
            | SyncAction::CreateRemoteDir { .. }
            | SyncAction::DeleteLocalDir { .. }
            | SyncAction::DeleteRemoteDir { .. } => {
                unreachable!(
                    "classify() never produces directory actions — classify_directories() does, and its \
                     output is executed in a separate block after this loop (see a later task in this plan)"
                )
            }
        }
    }
```

- [ ] **Step 6: Update `status_text`/`log_outcome`/`MANUAL_TEXT` to surface move counts**

Change `status_text`'s format string from:

```rust
            text.push_str(&format!(
                "{key}: {result}（{elapsed} 秒前）推 {} 拉 {} 刪本機 {} 刪對方 {} 衝突 {}\n",
                status.outcome.pushed,
                status.outcome.pulled,
                status.outcome.deleted_local,
                status.outcome.deleted_remote,
                status.outcome.conflicts,
            ));
```

to:

```rust
            text.push_str(&format!(
                "{key}: {result}（{elapsed} 秒前）推 {} 拉 {} 刪本機 {} 刪對方 {} 衝突 {} 搬本機 {} 搬對方 {}\n",
                status.outcome.pushed,
                status.outcome.pulled,
                status.outcome.deleted_local,
                status.outcome.deleted_remote,
                status.outcome.conflicts,
                status.outcome.moved_local,
                status.outcome.moved_remote,
            ));
```

Change `log_outcome` from:

```rust
fn log_outcome(ctx: &SharedContext, partner_key: &str, outcome: &SyncOutcome) {
    let had_activity = outcome.pushed > 0
        || outcome.pulled > 0
        || outcome.deleted_local > 0
        || outcome.deleted_remote > 0
        || outcome.conflicts > 0
        || outcome.error.is_some();
    if !had_activity {
        return;
    }
    let detail = match &outcome.error {
        Some(err) => format!("{partner_key} 同步失敗: {err}"),
        None => format!(
            "{partner_key} 同步完成：推 {} 拉 {} 刪本機 {} 刪對方 {} 衝突 {}",
            outcome.pushed, outcome.pulled, outcome.deleted_local, outcome.deleted_remote, outcome.conflicts
        ),
    };
    ctx.lock().unwrap().log_activity("sync", detail);
}
```

to:

```rust
fn log_outcome(ctx: &SharedContext, partner_key: &str, outcome: &SyncOutcome) {
    let had_activity = outcome.pushed > 0
        || outcome.pulled > 0
        || outcome.deleted_local > 0
        || outcome.deleted_remote > 0
        || outcome.conflicts > 0
        || outcome.moved_local > 0
        || outcome.moved_remote > 0
        || outcome.error.is_some();
    if !had_activity {
        return;
    }
    let detail = match &outcome.error {
        Some(err) => format!("{partner_key} 同步失敗: {err}"),
        None => format!(
            "{partner_key} 同步完成：推 {} 拉 {} 刪本機 {} 刪對方 {} 衝突 {} 搬本機 {} 搬對方 {}",
            outcome.pushed,
            outcome.pulled,
            outcome.deleted_local,
            outcome.deleted_remote,
            outcome.conflicts,
            outcome.moved_local,
            outcome.moved_remote,
        ),
    };
    ctx.lock().unwrap().log_activity("sync", detail);
}
```

Change `MANUAL_TEXT`'s closing paragraph from:

```
指令：
  status              列出每個同步對象上次同步的時間、結果、搬了幾個檔案、
                       刪了幾個、產生幾個衝突副本

真的衝突（雙方自上次同步後都改過同一個檔案）不會覆蓋任何一邊，兩邊都會保留
自己原本的檔案，並且各自多一份帶「(衝突自 <對方>，日期)」標記的對方版本副
本，需要使用者自己手動整理。
";
```

to:

```
指令：
  status              列出每個同步對象上次同步的時間、結果、搬了幾個檔案、
                       刪了幾個、產生幾個衝突副本、偵測到幾個改名/搬移

真的衝突（雙方自上次同步後都改過同一個檔案）不會覆蓋任何一邊，兩邊都會保留
自己原本的檔案，並且各自多一份帶「(衝突自 <對方>，日期)」標記的對方版本副
本，需要使用者自己手動整理。

單側改名/搬移一個檔案時，會被偵測出來直接在對方那邊重新命名，不會重傳整份
內容——資料夾整個改名，是靠底下每個檔案各自被偵測成改名疊加達成效果。
";
```

- [ ] **Step 7: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -30` — expect clean build (this is the point where the temporary non-exhaustive-match state from Step 4 gets fixed).
Run: `cargo test 2>&1 | tail -15` — expect all tests passing (106 + 7 new = 113).

- [ ] **Step 8: Manual verification**

1. `cargo run`, enter `storage`, `mkdir photos`, create a file under it (e.g. via the OS shell in another terminal: `echo hello > storage/photos/a.txt`), then in `storage`'s prompt run `mv photos/a.txt photos/b.txt` to rename it, confirm via `ls photos` that only `b.txt` exists. This confirms `storage::rename_path` (reused directly by `MoveLocal` and by the HTTP/cross-domain rename path) still works correctly for the interactive CLI path — this task doesn't change `rename_path` itself, only adds new callers, so this is a regression check.
2. Full cross-device Move verification (two real devices actually syncing a renamed file without re-transfer) is deferred to whenever this work is tested against the user's real multi-device setup — noted as a known verification gap, consistent with how the original sync plugin's cross-device behavior was verified.

- [ ] **Step 9: Commit**

```bash
git add src/plugins/sync.rs
git commit -m "$(cat <<'EOF'
classify 新增單側改名/搬移偵測，run_sync_pass 執行 Move 動作

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Wire directory create/delete execution into `run_sync_pass`

**Files:**
- Modify: `src/plugins/sync.rs`

**Interfaces:**
- Consumes: `classify_directories`/`DirClassification` (Task 2), `Baseline.known_dirs` (Task 1).
- Produces: `SyncOutcome` gains `dirs_created: usize`, `dirs_deleted: usize`. A new pure helper `fn order_dir_actions(actions: Vec<SyncAction>) -> Vec<SyncAction>` (creates ascending path depth, deletes descending path depth) is added and unit tested. `run_sync_pass` calls `classify_directories` after the main file-action loop (which already handles `CreateLocalDir`/`CreateRemoteDir`/`DeleteLocalDir`/`DeleteRemoteDir` with an `unreachable!()` arm, since `classify()` never produces them) and executes the ordered result in a new block after that loop.

This task does not touch the main loop's `match action` at all — `classify_directories`'s output never flows through it. The `unreachable!()` arm added in Task 2 stays correct and untouched.

- [ ] **Step 1: Write the failing test for `order_dir_actions`**

Add this to the `#[cfg(test)] mod tests { ... }` block in `src/plugins/sync.rs`:

```rust
    #[test]
    fn order_dir_actions_creates_shallow_before_deep_deletes_deep_before_shallow() {
        let actions = vec![
            SyncAction::DeleteRemoteDir { path: "x/y".to_string() },
            SyncAction::CreateLocalDir { path: "a/b/c".to_string() },
            SyncAction::DeleteRemoteDir { path: "x".to_string() },
            SyncAction::CreateLocalDir { path: "a".to_string() },
            SyncAction::CreateLocalDir { path: "a/b".to_string() },
        ];
        let ordered = order_dir_actions(actions);
        assert_eq!(
            ordered,
            vec![
                SyncAction::CreateLocalDir { path: "a".to_string() },
                SyncAction::CreateLocalDir { path: "a/b".to_string() },
                SyncAction::CreateLocalDir { path: "a/b/c".to_string() },
                SyncAction::DeleteRemoteDir { path: "x/y".to_string() },
                SyncAction::DeleteRemoteDir { path: "x".to_string() },
            ]
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test sync:: 2>&1 | tail -30`
Expected: compile error — `cannot find function 'order_dir_actions'`.

- [ ] **Step 3: Implement `order_dir_actions` and the outcome fields**

In `src/plugins/sync.rs`, change `SyncOutcome` (from Task 4's version) from:

```rust
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SyncOutcome {
    pub(crate) pushed: usize,
    pub(crate) pulled: usize,
    pub(crate) deleted_local: usize,
    pub(crate) deleted_remote: usize,
    pub(crate) conflicts: usize,
    pub(crate) moved_local: usize,
    pub(crate) moved_remote: usize,
    pub(crate) error: Option<String>,
}
```

to:

```rust
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SyncOutcome {
    pub(crate) pushed: usize,
    pub(crate) pulled: usize,
    pub(crate) deleted_local: usize,
    pub(crate) deleted_remote: usize,
    pub(crate) conflicts: usize,
    pub(crate) moved_local: usize,
    pub(crate) moved_remote: usize,
    pub(crate) dirs_created: usize,
    pub(crate) dirs_deleted: usize,
    pub(crate) error: Option<String>,
}
```

Add this function right after `pair_side` (still above `SyncTransport`):

```rust
fn dir_action_path(action: &SyncAction) -> &str {
    match action {
        SyncAction::CreateLocalDir { path }
        | SyncAction::CreateRemoteDir { path }
        | SyncAction::DeleteLocalDir { path }
        | SyncAction::DeleteRemoteDir { path } => path,
        other => unreachable!("order_dir_actions only takes directory actions, got {other:?}"),
    }
}

/// 決定目錄動作的執行順序：建立動作依路徑深度由淺到深（父目錄要先存在，
/// `mkdir` 才不會失敗），刪除動作依路徑深度由深到淺（巢狀空目錄要先刪子層，
/// 才能刪父層），建立整批排在刪除整批之前。純函式，只重新排序，不改變
/// `actions` 的內容。只接受 `classify_directories` 產生的 4 種目錄動作。
fn order_dir_actions(actions: Vec<SyncAction>) -> Vec<SyncAction> {
    let mut creates: Vec<SyncAction> = Vec::new();
    let mut deletes: Vec<SyncAction> = Vec::new();
    for action in actions {
        match &action {
            SyncAction::CreateLocalDir { .. } | SyncAction::CreateRemoteDir { .. } => creates.push(action),
            SyncAction::DeleteLocalDir { .. } | SyncAction::DeleteRemoteDir { .. } => deletes.push(action),
            other => unreachable!("order_dir_actions only takes directory actions, got {other:?}"),
        }
    }
    creates.sort_by_key(|a| dir_action_path(a).matches('/').count());
    deletes.sort_by_key(|a| std::cmp::Reverse(dir_action_path(a).matches('/').count()));
    creates.into_iter().chain(deletes).collect()
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test sync:: 2>&1 | tail -30`
Expected: `order_dir_actions_creates_shallow_before_deep_deletes_deep_before_shallow` passes.

- [ ] **Step 5: Wire directory classification and execution into `run_sync_pass`**

Change the end of `run_sync_pass` (right after the closing `}` of the main `for action in actions { ... }` loop, before `if let Err(err) = save_baseline(...)`) from:

```rust
        }
    }

    if let Err(err) = save_baseline(state_dir, partner_key, &baseline) {
        outcome.error = Some(format!("寫入 baseline 失敗: {err:#}"));
    }
    outcome
}
```

to:

```rust
        }
    }

    let dir_classification = classify_directories(&local, &remote, &baseline.known_dirs);
    for path in &dir_classification.confirmed_dirs {
        baseline.known_dirs.insert(path.clone());
    }
    for path in &dir_classification.stale_dirs {
        baseline.known_dirs.remove(path);
    }
    for action in order_dir_actions(dir_classification.actions) {
        match action {
            SyncAction::CreateRemoteDir { path } => match transport.mkdir(&path) {
                Ok(()) => {
                    baseline.known_dirs.insert(path);
                    outcome.dirs_created += 1;
                }
                Err(err) => outcome.error = Some(format!("在對方建立資料夾 {path} 失敗: {err:#}")),
            },
            SyncAction::CreateLocalDir { path } => match fs::create_dir_all(local_root.join(&path)) {
                Ok(()) => {
                    baseline.known_dirs.insert(path);
                    outcome.dirs_created += 1;
                }
                Err(err) => outcome.error = Some(format!("在本機建立資料夾 {path} 失敗: {err:#}")),
            },
            SyncAction::DeleteRemoteDir { path } => match transport.delete(&path, false) {
                Ok(()) => {
                    baseline.known_dirs.remove(&path);
                    outcome.dirs_deleted += 1;
                }
                Err(err) => outcome.error = Some(format!("刪除對方資料夾 {path} 失敗: {err:#}")),
            },
            SyncAction::DeleteLocalDir { path } => match crate::plugins::remove(&local_root.join(&path), false) {
                Ok(()) => {
                    baseline.known_dirs.remove(&path);
                    outcome.dirs_deleted += 1;
                }
                Err(err) => outcome.error = Some(format!("刪除本機資料夾 {path} 失敗: {err:#}")),
            },
            other => unreachable!("classify_directories only produces directory actions, got {other:?}"),
        }
    }

    if let Err(err) = save_baseline(state_dir, partner_key, &baseline) {
        outcome.error = Some(format!("寫入 baseline 失敗: {err:#}"));
    }
    outcome
}
```

- [ ] **Step 6: Update `status_text`/`log_outcome`/`MANUAL_TEXT` to surface directory counts**

Change `status_text`'s format string (from Task 4's version) from:

```rust
            text.push_str(&format!(
                "{key}: {result}（{elapsed} 秒前）推 {} 拉 {} 刪本機 {} 刪對方 {} 衝突 {} 搬本機 {} 搬對方 {}\n",
                status.outcome.pushed,
                status.outcome.pulled,
                status.outcome.deleted_local,
                status.outcome.deleted_remote,
                status.outcome.conflicts,
                status.outcome.moved_local,
                status.outcome.moved_remote,
            ));
```

to:

```rust
            text.push_str(&format!(
                "{key}: {result}（{elapsed} 秒前）推 {} 拉 {} 刪本機 {} 刪對方 {} 衝突 {} 搬本機 {} 搬對方 {} 建目錄 {} 刪目錄 {}\n",
                status.outcome.pushed,
                status.outcome.pulled,
                status.outcome.deleted_local,
                status.outcome.deleted_remote,
                status.outcome.conflicts,
                status.outcome.moved_local,
                status.outcome.moved_remote,
                status.outcome.dirs_created,
                status.outcome.dirs_deleted,
            ));
```

Change `log_outcome` from:

```rust
fn log_outcome(ctx: &SharedContext, partner_key: &str, outcome: &SyncOutcome) {
    let had_activity = outcome.pushed > 0
        || outcome.pulled > 0
        || outcome.deleted_local > 0
        || outcome.deleted_remote > 0
        || outcome.conflicts > 0
        || outcome.moved_local > 0
        || outcome.moved_remote > 0
        || outcome.error.is_some();
    if !had_activity {
        return;
    }
    let detail = match &outcome.error {
        Some(err) => format!("{partner_key} 同步失敗: {err}"),
        None => format!(
            "{partner_key} 同步完成：推 {} 拉 {} 刪本機 {} 刪對方 {} 衝突 {} 搬本機 {} 搬對方 {}",
            outcome.pushed,
            outcome.pulled,
            outcome.deleted_local,
            outcome.deleted_remote,
            outcome.conflicts,
            outcome.moved_local,
            outcome.moved_remote,
        ),
    };
    ctx.lock().unwrap().log_activity("sync", detail);
}
```

to:

```rust
fn log_outcome(ctx: &SharedContext, partner_key: &str, outcome: &SyncOutcome) {
    let had_activity = outcome.pushed > 0
        || outcome.pulled > 0
        || outcome.deleted_local > 0
        || outcome.deleted_remote > 0
        || outcome.conflicts > 0
        || outcome.moved_local > 0
        || outcome.moved_remote > 0
        || outcome.dirs_created > 0
        || outcome.dirs_deleted > 0
        || outcome.error.is_some();
    if !had_activity {
        return;
    }
    let detail = match &outcome.error {
        Some(err) => format!("{partner_key} 同步失敗: {err}"),
        None => format!(
            "{partner_key} 同步完成：推 {} 拉 {} 刪本機 {} 刪對方 {} 衝突 {} 搬本機 {} 搬對方 {} 建目錄 {} 刪目錄 {}",
            outcome.pushed,
            outcome.pulled,
            outcome.deleted_local,
            outcome.deleted_remote,
            outcome.conflicts,
            outcome.moved_local,
            outcome.moved_remote,
            outcome.dirs_created,
            outcome.dirs_deleted,
        ),
    };
    ctx.lock().unwrap().log_activity("sync", detail);
}
```

Change `MANUAL_TEXT`'s command description line (from Task 4's version) from:

```
  status              列出每個同步對象上次同步的時間、結果、搬了幾個檔案、
                       刪了幾個、產生幾個衝突副本、偵測到幾個改名/搬移
```

to:

```
  status              列出每個同步對象上次同步的時間、結果、搬了幾個檔案、
                       刪了幾個、產生幾個衝突副本、偵測到幾個改名/搬移、
                       建立/刪除了幾個空資料夾
```

- [ ] **Step 7: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -30` — expect clean build.
Run: `cargo test 2>&1 | tail -15` — expect all tests passing (113 + 1 new `order_dir_actions` test = 114).

- [ ] **Step 8: Manual verification**

Run: `cargo run`, then:
```
storage
mkdir emptyfolder
exit
```
Confirm `storage/emptyfolder` exists on disk (`ls storage/`). This confirms the directory-creation path this task's `fs::create_dir_all`/`transport.mkdir` calls depend on is reachable via the existing `storage` plugin — full cross-device propagation (two real devices, one creates/deletes an empty folder, the other picks it up next poll) is deferred to real multi-device testing, same gap noted in Task 4.

- [ ] **Step 9: Commit**

```bash
git add src/plugins/sync.rs
git commit -m "$(cat <<'EOF'
run_sync_pass 執行空資料夾建立/刪除動作

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```
