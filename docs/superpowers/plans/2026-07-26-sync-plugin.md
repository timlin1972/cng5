# Sync Plugin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `sync` plugin that automatically, bidirectionally syncs the `storage/` tree between devices — same-domain client↔server (hub model) and cross-domain server↔server — with conflict detection/resolution and deletion propagation, driven entirely by a server-role background poll loop (no manual trigger command).

**Architecture:** A pure classification function (`classify`) compares local/remote file listings (path→{size, modified, hash}) against a per-partner baseline (last-known-synced state, persisted to `sync-state/<partner>.json`) to decide push/pull/delete/conflict per file. A `SyncTransport` trait abstracts "how to talk to a partner" so the classification+execution engine is transport-agnostic; `HttpTransport` implements it for same-domain partners (reusing `storage` plugin's existing `/api/storage/*` endpoints plus one new `/api/storage/sync-manifest` endpoint), `CrossDomainTransport` implements it for cross-domain partners (five new `CrossDomainAsk`/`RemoteRequest`/`RemoteReply` variants mirroring the existing `FileList`/`FilePull`/`FilePush` chunked pattern). Only `mode server` devices run the engine, on a background thread polling every 60 seconds, one partner at a time; `mode client` devices are fully passive.

**Tech Stack:** Rust, existing `Plugin` trait, `anyhow`, `serde`/`serde_json`, `actix-web`, existing `CrossDomainAsk`/`RemoteRequest`/`RemoteReply` cross-domain relay (`src/shell.rs`, `src/plugins/global.rs`), existing `system` plugin's background-thread pattern. New dependency: `sha2` (content hashing for change/conflict detection — confirmed absent from `Cargo.toml` today).

## Global Constraints

- Sync operates on the whole `storage/` tree only (no per-subfolder configuration).
- No new pairing/config UI: sync partners are derived entirely from existing `system`/`global` plugin state — `ContextInner.devices` (same-domain clients, when `is_server` is true), `ContextInner.global` (cross-domain visible servers, via `merged_global_view`), `ContextInner.domain_name` (this device's own domain).
- Hub topology: a server syncs with every known same-domain client and every visible cross-domain server; clients never sync directly with each other; a server never proactively syncs with another server whose domain name sorts lexicographically before its own (tie-break rule — see Task 7) to avoid both sides of a visible domain pair initiating simultaneously.
- Only `mode server` devices (`ContextInner.is_server == true`) run the background poll loop; `mode client` devices do nothing active. There is **no manual `sync` command** — the engine is fully automatic, poll interval 60 seconds, independent of the existing 10-second device-report `REPORT_INTERVAL`.
- Content comparison uses SHA-256 (`sha2` crate) computed over full file bytes, not size/mtime heuristics — every file gets hashed on every sync pass (no lazy/on-demand hashing optimization).
- Deletions propagate: if a path is missing on one side but present in the baseline and unchanged on the other side, it's deleted on the other side too.
- Conflict rule: both sides keep their own version under the original name, and each additionally receives a copy of the OTHER side's version renamed to `<name> (衝突自 <partner-id>，<date>).<ext>`. Both new conflict-copy paths are recorded into the baseline immediately so they aren't re-flagged next round.
- Baseline persistence: `sync-state/<partner-key>.json`, one file per partner, atomic write (temp file + rename), missing/corrupt file degrades to an empty baseline (never panics). `sync-state/` is `.gitignore`d.
- No content-integrity re-verification after transfer (trust the transport, matching `files` plugin's existing convention).
- No version history, no per-subfolder sync config, no manual trigger command, no cross-domain target filtering (syncs with every domain visible in the `global` registry).
- The classification algorithm only tracks **files** by path — empty directories are not synced as standalone entities; any directories needed to hold a synced file are created implicitly by the execution layer before writing that file.

---

### Task 1: `SyncEntry` and `walk_with_hashes` in `storage.rs`

**Files:**
- Modify: `Cargo.toml` (add `sha2` dependency)
- Modify: `src/plugins/storage.rs` (add `SyncEntry` struct and `walk_with_hashes`/`hash_file` functions + tests)
- Modify: `src/plugins/mod.rs` (re-export `SyncEntry`/`walk_with_hashes`/`paginate_sync_entries` so code outside the `plugins` module, e.g. `src/web.rs`, can reach them)

**Interfaces:**
- Produces:
  - `pub(crate) struct SyncEntry { pub(crate) path: String, pub(crate) is_dir: bool, pub(crate) size: u64, pub(crate) modified: u64, pub(crate) hash: Option<String> }` — `path` is relative to the walked root, forward-slash-separated regardless of platform, `hash` is `None` for directories and `Some(<64-char lowercase hex SHA-256>)` for files.
  - `pub(crate) fn walk_with_hashes(root: &Path) -> Result<Vec<SyncEntry>>` — recursively walks `root` (reusing Task 1's existing `list_dir` per directory), returns a flat list covering every file and directory at every depth.
  - `pub(crate) fn paginate_sync_entries(all: &[SyncEntry], offset: usize) -> Vec<SyncEntry>` — splits a full listing into one page bounded by a fixed byte budget, mirroring `global.rs`'s existing `paginate_file_list` for `FileMeta`. Only needed cross-domain (MQTT messages have a hard size limit); the same-domain HTTP endpoint in Task 5 returns the full unpaginated list in one response, matching this file's existing `storage_list`/`files_list`/`music_files` precedent (plain HTTP has no such constraint).
- Consumed by: Task 3's classification algorithm (via `SyncEntry`), Task 4's cross-domain reply handler (via `paginate_sync_entries`), and Task 5/6's transports (which call `walk_with_hashes` locally and expose the same shape remotely).

- [ ] **Step 1: Add the `sha2` dependency**

In `Cargo.toml`, add this line to the `[dependencies]` section (alphabetically between `serde_json` and `shell-words`):

```toml
serde_json = "1"
sha2 = "0.10"
shell-words = "1.1.1"
```

- [ ] **Step 2: Write the failing tests**

Append this to the existing `#[cfg(test)] mod tests { use super::*; ... }` block in `src/plugins/storage.rs` (add these `#[test]` functions inside the existing braces, after the last Task-2 test):

```rust
    #[test]
    fn walk_with_hashes_covers_nested_files_and_dirs() {
        let root = test_root("walk-nested");
        fs::create_dir_all(root.join("photos/2026")).unwrap();
        fs::write(root.join("a.txt"), b"top level").unwrap();
        fs::write(root.join("photos/2026/beach.jpg"), b"jpg bytes").unwrap();
        let entries = walk_with_hashes(&root).unwrap();
        let mut paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, vec!["a.txt", "photos", "photos/2026", "photos/2026/beach.jpg"]);

        let dir_entry = entries.iter().find(|e| e.path == "photos").unwrap();
        assert!(dir_entry.is_dir);
        assert_eq!(dir_entry.hash, None);

        let file_entry = entries.iter().find(|e| e.path == "a.txt").unwrap();
        assert!(!file_entry.is_dir);
        assert!(file_entry.hash.is_some());
        assert_eq!(file_entry.size, "top level".len() as u64);
    }

    #[test]
    fn walk_with_hashes_uses_forward_slash_paths() {
        let root = test_root("walk-forward-slash");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/c.txt"), b"x").unwrap();
        let entries = walk_with_hashes(&root).unwrap();
        let file_entry = entries.iter().find(|e| !e.is_dir).unwrap();
        assert_eq!(file_entry.path, "a/b/c.txt");
        assert!(!file_entry.path.contains('\\'));
    }

    #[test]
    fn identical_content_produces_identical_hash() {
        let root = test_root("walk-hash-identical");
        fs::write(root.join("one.txt"), b"same content").unwrap();
        fs::write(root.join("two.txt"), b"same content").unwrap();
        let entries = walk_with_hashes(&root).unwrap();
        let hash_one = entries.iter().find(|e| e.path == "one.txt").unwrap().hash.clone();
        let hash_two = entries.iter().find(|e| e.path == "two.txt").unwrap().hash.clone();
        assert_eq!(hash_one, hash_two);
        assert!(hash_one.unwrap().len() == 64);
    }

    #[test]
    fn different_content_produces_different_hash() {
        let root = test_root("walk-hash-different");
        fs::write(root.join("one.txt"), b"content A").unwrap();
        fs::write(root.join("two.txt"), b"content B").unwrap();
        let entries = walk_with_hashes(&root).unwrap();
        let hash_one = entries.iter().find(|e| e.path == "one.txt").unwrap().hash.clone();
        let hash_two = entries.iter().find(|e| e.path == "two.txt").unwrap().hash.clone();
        assert_ne!(hash_one, hash_two);
    }

    #[test]
    fn walk_with_hashes_on_empty_root_returns_empty() {
        let root = test_root("walk-empty");
        let entries = walk_with_hashes(&root).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn paginate_sync_entries_splits_by_budget_and_offset() {
        let entries: Vec<SyncEntry> = (0..2000)
            .map(|i| SyncEntry {
                path: format!("file-{i}.txt"),
                is_dir: false,
                size: 1,
                modified: 0,
                hash: Some("a".repeat(64)),
            })
            .collect();
        let page1 = paginate_sync_entries(&entries, 0);
        assert!(!page1.is_empty());
        assert!(page1.len() < entries.len(), "應該分頁，不會一次全部塞進一頁");
        let page2 = paginate_sync_entries(&entries, page1.len());
        assert_eq!(page2.first().unwrap().path, entries[page1.len()].path);
    }

    #[test]
    fn paginate_sync_entries_past_end_returns_empty() {
        let entries = vec![SyncEntry {
            path: "only.txt".to_string(),
            is_dir: false,
            size: 1,
            modified: 0,
            hash: Some("h".to_string()),
        }];
        assert!(paginate_sync_entries(&entries, entries.len()).is_empty());
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test storage:: 2>&1 | tail -40`
Expected: compile error — `cannot find function 'walk_with_hashes'`/`'paginate_sync_entries'` in this scope (neither exists yet).

- [ ] **Step 4: Implement `SyncEntry` and `walk_with_hashes`**

Add `use sha2::{Digest, Sha256};` to the top of `src/plugins/storage.rs`, alongside the existing `use` lines (after `use serde::{Deserialize, Serialize};`).

Add this after the existing `StorageEntry` struct definition (before `safe_storage_path`):

```rust
/// 一個檔案或資料夾在同步演算法裡的狀態：`path` 是相對於被走訪的根目錄、用
/// `/` 分隔的相對路徑（不管實際作業系統的分隔符是什麼，統一轉成 `/`，這樣
/// 兩台不同作業系統的機器比對路徑字串時才不會因為分隔符不同而誤判成不同路
/// 徑）。`hash` 資料夾一律是 `None`，檔案才有 `Some(<64 字元小寫十六進位
/// SHA-256>)`。
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub(crate) struct SyncEntry {
    pub(crate) path: String,
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
    pub(crate) modified: u64,
    pub(crate) hash: Option<String>,
}

/// 遞迴走訪 `root` 底下所有檔案/資料夾，攤平成一份清單，每個檔案都算好
/// SHA-256（同步演算法拿這份清單去跟對方、跟 baseline 比對）。每次同步都
/// 重新算一次，不做「只在懷疑衝突時才算 hash」這種提前優化。
pub(crate) fn walk_with_hashes(root: &Path) -> Result<Vec<SyncEntry>> {
    let mut out = Vec::new();
    walk_with_hashes_inner(root, root, &mut out)?;
    Ok(out)
}

fn walk_with_hashes_inner(root: &Path, dir: &Path, out: &mut Vec<SyncEntry>) -> Result<()> {
    for entry in list_dir(dir)? {
        let full_path = dir.join(&entry.name);
        let relative = full_path.strip_prefix(root).unwrap_or(&full_path);
        let path_str = relative.to_string_lossy().replace('\\', "/");
        if entry.is_dir {
            out.push(SyncEntry { path: path_str, is_dir: true, size: 0, modified: entry.modified, hash: None });
            walk_with_hashes_inner(root, &full_path, out)?;
        } else {
            let hash = hash_file(&full_path)?;
            out.push(SyncEntry {
                path: path_str,
                is_dir: false,
                size: entry.size,
                modified: entry.modified,
                hash: Some(hash),
            });
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    let data = fs::read(path).with_context(|| format!("讀取檔案失敗: {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(format!("{:x}", hasher.finalize()))
}

/// 分頁工具，給同網域的 `/api/storage/sync-manifest`（Task 5）跟跨 domain 的
/// `RemoteRequest::StorageManifest` 處理（Task 4）共用，兩邊都需要把
/// `walk_with_hashes` 算出來的完整清單，依照跟現有 `files`/`global` 的
/// `FileList` 分頁一樣的位元組預算切成一頁一頁回傳，公開 MQTT broker 對單則
/// 訊息大小有限制，不能整份塞進一則回覆。
pub(crate) fn paginate_sync_entries(all: &[SyncEntry], offset: usize) -> Vec<SyncEntry> {
    const PAGE_BUDGET: usize = 4 * 1024;
    let mut page = Vec::new();
    let mut size = 0usize;
    for item in all.iter().skip(offset) {
        let item_size = item.path.len() + item.hash.as_ref().map(|h| h.len()).unwrap_or(0) + 32;
        if !page.is_empty() && size + item_size > PAGE_BUDGET {
            break;
        }
        size += item_size;
        page.push(item.clone());
    }
    page
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test storage:: 2>&1 | tail -50`
Expected: all tests in `plugins::storage::tests` pass, including the 7 new ones (22 total: 15 from before + 7 new).

- [ ] **Step 6: Re-export the new items from `src/plugins/mod.rs`**

`src/plugins/mod.rs` currently has this line (from earlier work on the `storage` plugin):

```rust
pub(crate) use storage::{list_dir, make_dir, remove, rename_path, safe_storage_path, STORAGE_DIR};
```

`web.rs` (Task 5) and `global.rs` (Task 4) need to reach `walk_with_hashes`, `paginate_sync_entries`, and the `SyncEntry` type from outside/across the `plugins` module tree, so add them to this same re-export line:

```rust
pub(crate) use storage::{
    list_dir, make_dir, paginate_sync_entries, remove, rename_path, safe_storage_path, walk_with_hashes,
    STORAGE_DIR,
};
pub(crate) use storage::SyncEntry;
```

- [ ] **Step 7: Build to confirm the re-export compiles**

Run: `cargo build 2>&1 | tail -20`
Expected: clean build, no errors (this step adds no test coverage of its own — it's a visibility/re-export change, verified by the fact that later tasks' code, which references these via `crate::plugins::...`, compiles).

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/plugins/storage.rs src/plugins/mod.rs
git commit -m "$(cat <<'EOF'
新增 sync 功能所需的 SyncEntry 與遞迴 hash 走訪

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Baseline persistence

**Files:**
- Create: `src/plugins/sync_baseline.rs`
- Modify: `src/plugins/mod.rs` (add `mod sync_baseline;` declaration only — no `pub use` yet, nothing outside this module needs it until Task 7)
- Modify: `.gitignore` (add `/sync-state`)

**Interfaces:**
- Produces:
  - `pub(crate) const SYNC_STATE_DIR: &str = "sync-state";`
  - `pub(crate) struct BaselineEntry { pub(crate) local_hash: String, pub(crate) remote_hash: String }` — tracks the **last-known hash of each side independently**, not one shared value. This matters for acknowledged conflicts: after a conflict is resolved (Task 7), the two sides' originals are deliberately left different forever, and recording `local_hash`/`remote_hash` as those two (different) values lets `classify` (Task 3) recognize "yes these still differ, but that's already been resolved" on every later round, instead of re-flagging the same conflict and spamming a fresh conflict-copy file every poll cycle.
  - `pub(crate) type Baseline = std::collections::HashMap<String, BaselineEntry>;` — keyed by the same relative-path strings `SyncEntry::path` uses.
  - `pub(crate) fn baseline_path(state_dir: &Path, partner_key: &str) -> PathBuf`
  - `pub(crate) fn load_baseline(state_dir: &Path, partner_key: &str) -> Baseline` — never fails; missing/corrupt file yields an empty `Baseline`.
  - `pub(crate) fn save_baseline(state_dir: &Path, partner_key: &str, baseline: &Baseline) -> Result<()>` — atomic (temp file + rename), creates `state_dir` if missing.
- Consumed by: Task 7's `SyncPlugin`, always called with `Path::new(SYNC_STATE_DIR)` as `state_dir` in production; tests pass an isolated temp directory instead (same testability pattern as `storage.rs`'s `safe_storage_path(root: &Path, ...)`).

- [ ] **Step 1: Write the failing tests**

Create `src/plugins/sync_baseline.rs` with only this content (fails to compile — expected "red" state):

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
        let mut baseline = Baseline::new();
        baseline.insert(
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
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_corrupt_file_returns_empty_baseline_not_panic() {
        let state_dir = test_state_dir("corrupt-file");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(baseline_path(&state_dir, "broken"), b"{ this is not valid json").unwrap();
        let loaded = load_baseline(&state_dir, "broken");
        assert!(loaded.is_empty());
    }

    #[test]
    fn save_creates_state_dir_if_missing() {
        let state_dir = test_state_dir("creates-dir");
        assert!(!state_dir.exists());
        save_baseline(&state_dir, "domain-branch-b", &Baseline::new()).unwrap();
        assert!(state_dir.is_dir());
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let state_dir = test_state_dir("no-temp-leftover");
        save_baseline(&state_dir, "client-a", &Baseline::new()).unwrap();
        let tmp_path = baseline_path(&state_dir, "client-a").with_extension("json.tmp");
        assert!(!tmp_path.exists());
        assert!(baseline_path(&state_dir, "client-a").exists());
    }

    #[test]
    fn different_partners_get_different_files() {
        let state_dir = test_state_dir("different-partners");
        let mut baseline_a = Baseline::new();
        baseline_a.insert("x.txt".to_string(), BaselineEntry { local_hash: "h1".to_string(), remote_hash: "h1".to_string() });
        save_baseline(&state_dir, "client-a", &baseline_a).unwrap();
        save_baseline(&state_dir, "client-b", &Baseline::new()).unwrap();
        assert_eq!(load_baseline(&state_dir, "client-a"), baseline_a);
        assert!(load_baseline(&state_dir, "client-b").is_empty());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Add `mod sync_baseline;` to `src/plugins/mod.rs` first (alphabetically between `mod system;` and `mod weather;`... actually `sync_baseline` sorts before `system`, so insert between `mod storage;` and `mod system;`):

```rust
mod storage;
mod sync_baseline;
mod system;
```

Run: `cargo test sync_baseline:: 2>&1 | tail -40`
Expected: compile error — `cannot find type 'Baseline'` / `cannot find function 'save_baseline'` etc. in this scope (none of them exist yet).

- [ ] **Step 3: Implement baseline persistence**

Add this above the `#[cfg(test)]` block in `src/plugins/sync_baseline.rs`:

```rust
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 頂層資料夾，跟 `storage/`、`music/`、`notepad/` 同一層——不放在 `storage/`
/// 裡面，放裡面會被當成使用者可見的同步內容，甚至被誤判成也要同步的檔案。
pub(crate) const SYNC_STATE_DIR: &str = "sync-state";

/// 某個路徑上次同步完成時，本機、對方各自的內容 hash——特意分開記錄兩邊，
/// 不是共用一個值。正常同步完成後兩者會相等（雙方內容一致），但「已經確認
/// 過的衝突」是特例：兩邊故意保留不同的內容，這時候 `local_hash`/
/// `remote_hash` 就會不一樣，讓 `classify`（Task 3）能認得出「這個分歧是已知
/// 且已經處理過的」，不會每一輪都重新判定成新衝突、瘋狂產生新的衝突副本。
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub(crate) struct BaselineEntry {
    pub(crate) local_hash: String,
    pub(crate) remote_hash: String,
}

/// 一個同步對象（同 domain 的某個 client id、或跨 domain 的某個 domain 名稱）
/// 的完整 baseline：相對路徑 -> 上次同步完成時的狀態。
pub(crate) type Baseline = HashMap<String, BaselineEntry>;

pub(crate) fn baseline_path(state_dir: &Path, partner_key: &str) -> PathBuf {
    state_dir.join(format!("{partner_key}.json"))
}

/// 讀取失敗（檔案不存在、格式壞掉）一律當作「這個對象還沒有 baseline」，回傳
/// 空的 `Baseline`，不會讓呼叫端因此整個失敗——保守處理，往後遇到的差異會被
/// 當成潛在衝突，見同步演算法的說明。
pub(crate) fn load_baseline(state_dir: &Path, partner_key: &str) -> Baseline {
    let path = baseline_path(state_dir, partner_key);
    match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Baseline::new(),
    }
}

/// 先寫暫存檔、再 rename 成正式檔名，避免中途當機留下寫壞一半的檔案。
pub(crate) fn save_baseline(state_dir: &Path, partner_key: &str, baseline: &Baseline) -> Result<()> {
    fs::create_dir_all(state_dir).with_context(|| format!("建立 {} 失敗", state_dir.display()))?;
    let path = baseline_path(state_dir, partner_key);
    let tmp_path = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(baseline).context("序列化 baseline 失敗")?;
    fs::write(&tmp_path, content).with_context(|| format!("寫入暫存檔失敗: {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &path).with_context(|| format!("重新命名暫存檔失敗: {}", path.display()))?;
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test sync_baseline:: 2>&1 | tail -40`
Expected: all 6 tests in `plugins::sync_baseline::tests` pass.

- [ ] **Step 5: Add `/sync-state` to `.gitignore`**

In `.gitignore`, add a `/sync-state` entry following the existing blank-line-separated style, next to `/storage`:

```
/storage

/sync-state

/wol
```

- [ ] **Step 6: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -20` — expect clean, no errors/warnings.
Run: `cargo test 2>&1 | tail -15` — expect all existing tests plus the new ones passing.

- [ ] **Step 7: Commit**

```bash
git add src/plugins/sync_baseline.rs src/plugins/mod.rs .gitignore
git commit -m "$(cat <<'EOF'
新增 sync baseline 的磁碟持久化

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Core classification algorithm

**Files:**
- Create: `src/plugins/sync.rs`
- Modify: `src/plugins/mod.rs` (add `mod sync;` declaration only — no `pub use` yet)

**Interfaces:**
- Consumes: `SyncEntry` from Task 1 (`crate::plugins::storage::SyncEntry`), `Baseline`/`BaselineEntry` from Task 2 (`crate::plugins::sync_baseline::{Baseline, BaselineEntry}`).
- Produces:
  - `pub(crate) enum SyncAction { PushToRemote { path: String }, PullFromRemote { path: String }, DeleteLocal { path: String }, DeleteRemote { path: String }, Conflict { path: String } }` (derives `Clone, Debug, PartialEq`)
  - `pub(crate) fn classify(local: &[SyncEntry], remote: &[SyncEntry], baseline: &Baseline) -> Vec<SyncAction>` — the pure diff/decision function. Task 7's engine calls this once per sync pass per partner, then executes the returned actions via a `SyncTransport`.

- [ ] **Step 1: Write the failing tests**

Create `src/plugins/sync.rs` with only this content (references types/functions that don't exist yet — fails to compile, expected "red" state):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::storage::SyncEntry;
    use crate::plugins::sync_baseline::{Baseline, BaselineEntry};

    fn file(path: &str, hash: &str) -> SyncEntry {
        SyncEntry { path: path.to_string(), is_dir: false, size: 10, modified: 0, hash: Some(hash.to_string()) }
    }

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

    #[test]
    fn new_local_file_pushes_to_remote() {
        let local = vec![file("new.txt", "h1")];
        let remote = vec![];
        let actions = classify(&local, &remote, &Baseline::new());
        assert_eq!(actions, vec![SyncAction::PushToRemote { path: "new.txt".to_string() }]);
    }

    #[test]
    fn new_remote_file_pulls_from_remote() {
        let local = vec![];
        let remote = vec![file("new.txt", "h1")];
        let actions = classify(&local, &remote, &Baseline::new());
        assert_eq!(actions, vec![SyncAction::PullFromRemote { path: "new.txt".to_string() }]);
    }

    #[test]
    fn unchanged_on_both_sides_produces_no_action() {
        let local = vec![file("same.txt", "h1")];
        let remote = vec![file("same.txt", "h1")];
        let baseline = baseline_of(&[("same.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert!(actions.is_empty());
    }

    #[test]
    fn local_only_change_pushes_to_remote() {
        let local = vec![file("f.txt", "h2")]; // 本機改成 h2
        let remote = vec![file("f.txt", "h1")]; // 對方還是舊的 h1
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]); // 上次同步時雙方都是 h1
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::PushToRemote { path: "f.txt".to_string() }]);
    }

    #[test]
    fn remote_only_change_pulls_from_remote() {
        let local = vec![file("f.txt", "h1")];
        let remote = vec![file("f.txt", "h2")];
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::PullFromRemote { path: "f.txt".to_string() }]);
    }

    #[test]
    fn both_sides_changed_to_same_content_is_no_action() {
        let local = vec![file("f.txt", "h2")];
        let remote = vec![file("f.txt", "h2")]; // 剛好改成一樣
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert!(actions.is_empty());
    }

    #[test]
    fn both_sides_changed_differently_is_conflict() {
        let local = vec![file("f.txt", "h2")];
        let remote = vec![file("f.txt", "h3")];
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::Conflict { path: "f.txt".to_string() }]);
    }

    #[test]
    fn both_sides_independently_created_same_name_different_content_is_conflict() {
        let local = vec![file("new.txt", "hA")];
        let remote = vec![file("new.txt", "hB")];
        let actions = classify(&local, &remote, &Baseline::new()); // 沒有 baseline
        assert_eq!(actions, vec![SyncAction::Conflict { path: "new.txt".to_string() }]);
    }

    #[test]
    fn both_sides_independently_created_same_name_same_content_is_no_action() {
        let local = vec![file("new.txt", "hA")];
        let remote = vec![file("new.txt", "hA")];
        let actions = classify(&local, &remote, &Baseline::new());
        assert!(actions.is_empty());
    }

    #[test]
    fn remote_deleted_unchanged_local_file_deletes_local() {
        let local = vec![file("gone.txt", "h1")]; // 本機沒動過
        let remote = vec![]; // 對方刪掉了
        let baseline = baseline_of(&[("gone.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::DeleteLocal { path: "gone.txt".to_string() }]);
    }

    #[test]
    fn local_deleted_unchanged_remote_file_deletes_remote() {
        let local = vec![]; // 本機刪掉了
        let remote = vec![file("gone.txt", "h1")]; // 對方沒動過
        let baseline = baseline_of(&[("gone.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::DeleteRemote { path: "gone.txt".to_string() }]);
    }

    #[test]
    fn remote_deleted_but_local_edited_keeps_local_edit_by_pushing() {
        // 對方把這個路徑刪了，但本機這邊在同一時間反而編輯過它——保留本機的
        // 修改（推過去），不要因為對方刪除就跟著刪掉本機也改過的內容。
        let local = vec![file("f.txt", "h2")]; // 本機改過
        let remote = vec![]; // 對方刪了
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::PushToRemote { path: "f.txt".to_string() }]);
    }

    #[test]
    fn local_deleted_but_remote_edited_keeps_remote_edit_by_pulling() {
        let local = vec![]; // 本機刪了
        let remote = vec![file("f.txt", "h2")]; // 對方改過
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::PullFromRemote { path: "f.txt".to_string() }]);
    }

    #[test]
    fn both_sides_deleted_produces_no_action() {
        let local = vec![];
        let remote = vec![];
        let baseline = baseline_of(&[("gone.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert!(actions.is_empty());
    }

    #[test]
    fn no_baseline_and_content_differs_is_conservatively_a_conflict() {
        // 對應「重啟後完全沒有 baseline」的情境：兩邊都有、內容不一樣、又沒有
        // baseline 可以判斷是誰改的，保守當成衝突處理。
        let local = vec![file("f.txt", "hA")];
        let remote = vec![file("f.txt", "hB")];
        let actions = classify(&local, &remote, &Baseline::new());
        assert_eq!(actions, vec![SyncAction::Conflict { path: "f.txt".to_string() }]);
    }

    #[test]
    fn already_acknowledged_conflict_produces_no_action() {
        // baseline 記錄了「local 是 hA、remote 是 hB」——代表上一輪已經處理過
        // 這個衝突（兩邊故意留下不同內容），這一輪雙方都還是各自原本的值，
        // 不應該再被判定成新衝突、重複產生衝突副本。
        let local = vec![file("f.txt", "hA")];
        let remote = vec![file("f.txt", "hB")];
        let baseline = baseline_of(&[("f.txt", "hA", "hB")]);
        let actions = classify(&local, &remote, &baseline);
        assert!(actions.is_empty());
    }

    #[test]
    fn acknowledged_conflict_then_local_changes_again_pushes_to_remote() {
        // 已經確認過的衝突（baseline: local=hA, remote=hB）之後，本機這邊又
        // 改了一次（hC）——這是相對 baseline 的單向變更，應該推過去，不是
        // 重新判定成衝突。
        let local = vec![file("f.txt", "hC")];
        let remote = vec![file("f.txt", "hB")];
        let baseline = baseline_of(&[("f.txt", "hA", "hB")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::PushToRemote { path: "f.txt".to_string() }]);
    }

    #[test]
    fn acknowledged_conflict_then_both_change_again_is_a_new_conflict() {
        let local = vec![file("f.txt", "hC")];
        let remote = vec![file("f.txt", "hD")];
        let baseline = baseline_of(&[("f.txt", "hA", "hB")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::Conflict { path: "f.txt".to_string() }]);
    }

    #[test]
    fn multiple_independent_paths_each_classified_separately() {
        let local = vec![file("a.txt", "h1"), file("b.txt", "h1")];
        let remote = vec![file("b.txt", "h1")]; // a.txt 只有本機有，b.txt 兩邊一樣
        let actions = classify(&local, &remote, &Baseline::new());
        assert_eq!(actions, vec![SyncAction::PushToRemote { path: "a.txt".to_string() }]);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Add `mod sync;` to `src/plugins/mod.rs` (alphabetically: `"sync"` sorts before `"sync_baseline"` since it's a prefix of it — insert between `mod storage;` and `mod sync_baseline;`):

```rust
mod storage;
mod sync;
mod sync_baseline;
mod system;
```

Run: `cargo test plugins::sync:: 2>&1 | tail -60`
Expected: compile error — `cannot find function 'classify'` / `cannot find enum 'SyncAction'` in this scope (neither exists yet).

- [ ] **Step 3: Implement `SyncAction` and `classify`**

Add this above the `#[cfg(test)]` block in `src/plugins/sync.rs`:

```rust
use std::collections::{HashMap, HashSet};

use crate::plugins::storage::SyncEntry;
use crate::plugins::sync_baseline::Baseline;

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

struct FileState {
    hash: String,
}

/// 把 `walk_with_hashes` 的結果過濾成「只有檔案、路徑對到雜湊」的對照表，
/// 資料夾條目直接跳過（同步演算法不處理空資料夾，見上方 `SyncAction` 的說
/// 明）。
fn file_states(entries: &[SyncEntry]) -> HashMap<String, FileState> {
    entries
        .iter()
        .filter(|e| !e.is_dir)
        .filter_map(|e| e.hash.clone().map(|hash| (e.path.clone(), FileState { hash })))
        .collect()
}

/// 核心分類邏輯：比對本機清單、對方清單、這個同步對象的 baseline，決定每個
/// 路徑該做的事。純函式，不牽涉任何網路/檔案系統 I/O。
pub(crate) fn classify(local: &[SyncEntry], remote: &[SyncEntry], baseline: &Baseline) -> Vec<SyncAction> {
    let local_states = file_states(local);
    let remote_states = file_states(remote);

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

        match (local_state, remote_state, base) {
            (Some(_), None, None) => actions.push(SyncAction::PushToRemote { path: path.clone() }),
            (None, Some(_), None) => actions.push(SyncAction::PullFromRemote { path: path.clone() }),
            (None, None, _) => {} // 兩邊都沒有：不管有沒有舊 baseline 紀錄，都無事可做
            (Some(local), None, Some(base)) => {
                if local.hash == base.local_hash {
                    actions.push(SyncAction::DeleteLocal { path: path.clone() });
                } else {
                    // 對方刪了，但本機這邊同時改過——保留本機的修改，當作
                    // 本機端的變更重新推過去。
                    actions.push(SyncAction::PushToRemote { path: path.clone() });
                }
            }
            (None, Some(remote), Some(base)) => {
                if remote.hash == base.remote_hash {
                    actions.push(SyncAction::DeleteRemote { path: path.clone() });
                } else {
                    actions.push(SyncAction::PullFromRemote { path: path.clone() });
                }
            }
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

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test plugins::sync:: 2>&1 | tail -60`
Expected: all 19 tests in `plugins::sync::tests` pass.

- [ ] **Step 5: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -20` — expect clean.
Run: `cargo test 2>&1 | tail -15` — expect all tests passing.

- [ ] **Step 6: Commit**

```bash
git add src/plugins/sync.rs src/plugins/mod.rs
git commit -m "$(cat <<'EOF'
新增 sync 的核心分類演算法（純函式）

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Cross-domain protocol additions

**Files:**
- Modify: `src/plugin.rs` (new `CrossDomainAsk`/`RemoteRequest`/`RemoteReply` variants)
- Modify: `src/shell.rs` (new match arms in the ask→request conversion and the timeout function)
- Modify: `src/plugins/global.rs` (new match arms in `build_remote_reply`/`request_kind`)
- Modify: `src/plugins/storage.rs` (two new small chunk I/O helpers used by `build_remote_reply`)
- Modify: `src/plugins/mod.rs` (re-export the two new chunk I/O helpers)

**Interfaces:**
- Consumes: `SyncEntry`/`paginate_sync_entries`/`STORAGE_DIR`/`safe_storage_path`/`make_dir`/`remove` from Task 1/earlier `storage.rs` work.
- Produces:
  - Five new `CrossDomainAsk` variants: `StorageManifest { offset: usize }`, `StorageFilePull { path: String, offset: u64 }`, `StorageFilePush { path: String, offset: u64, data: String }`, `StorageMkdir { path: String }`, `StorageDelete { path: String, recursive: bool }`.
  - Matching `RemoteRequest` variants (same fields plus `request_id: String, source_domain: String`).
  - Two new `RemoteReply` variants: `StorageManifest { request_id: String, entries: Vec<SyncEntry>, total: usize }` and `Ack { request_id: String }` (generic success-with-no-data reply, used by `StorageMkdir`/`StorageDelete`; the existing `FileChunk`/`FilePushAck`/`Error` variants are reused as-is for the pull/push/error cases).
  - `pub(crate) fn read_chunk(path: &Path, offset: u64) -> Result<String>` and `pub(crate) fn write_chunk(path: &Path, offset: u64, data_b64: &str) -> Result<()>` in `storage.rs`.
- Consumed by: Task 6's `CrossDomainTransport`.

**Important design note — no `target_id`:** the existing `FileList`/`FilePull`/`FilePush` asks carry a `target_id` because they relay a request from one domain to a *specific device* within another domain (see `global.rs`'s `target_ip` lookup). The new `Storage*` asks are different: cross-domain sync is server-to-server, and each server answers with **its own** `storage/` tree — there is no further per-device relay. So none of the five new variants have a `target_id` field; the receiving domain's server (already the only side that runs `build_remote_reply` for requests addressed to its own domain — see the `my_domain.as_deref() != Some(domain)` guard in `handle_remote_request`) just operates directly on its local `STORAGE_DIR`.

- [ ] **Step 1: Add the new `CrossDomainAsk`/`RemoteRequest`/`RemoteReply` variants**

In `src/plugin.rs`, change the `CrossDomainAsk` enum (currently at `src/plugin.rs:151-158`) from:

```rust
pub enum CrossDomainAsk {
    Exec { target_id: String, line: String },
    Panel { target_id: String, panel_name: String },
    FileList { target_id: String, folder: String, offset: usize },
    FilePull { target_id: String, folder: String, name: String, offset: u64 },
    FilePush { target_id: String, folder: String, name: String, offset: u64, data: String },
}
```

to:

```rust
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

Change `RemoteRequest` (currently `src/plugin.rs:173-192`) from:

```rust
pub enum RemoteRequest {
    Exec { request_id: String, source_domain: String, target_id: String, line: String },
    Panel { request_id: String, source_domain: String, target_id: String, panel_name: String },
    FileList { request_id: String, source_domain: String, target_id: String, folder: String, offset: usize },
    FilePull { request_id: String, source_domain: String, target_id: String, folder: String, name: String, offset: u64 },
    FilePush { request_id: String, source_domain: String, target_id: String, folder: String, name: String, offset: u64, data: String },
}

impl RemoteRequest {
    pub fn source_domain(&self) -> &str {
        match self {
            RemoteRequest::Exec { source_domain, .. }
            | RemoteRequest::Panel { source_domain, .. }
            | RemoteRequest::FileList { source_domain, .. }
            | RemoteRequest::FilePull { source_domain, .. }
            | RemoteRequest::FilePush { source_domain, .. } => source_domain,
        }
    }
}
```

to:

```rust
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

Change `RemoteReply` (currently `src/plugin.rs:197-218`) from:

```rust
pub enum RemoteReply {
    Exec { request_id: String, prompt: String, error: Option<String> },
    Panel { request_id: String, text: Option<String> },
    Error { request_id: String, message: String },
    FileList { request_id: String, files: Vec<FileMeta>, total: usize },
    FileChunk { request_id: String, data: String },
    FilePushAck { request_id: String },
}

impl RemoteReply {
    pub fn request_id(&self) -> &str {
        match self {
            RemoteReply::Exec { request_id, .. }
            | RemoteReply::Panel { request_id, .. }
            | RemoteReply::Error { request_id, .. }
            | RemoteReply::FileList { request_id, .. }
            | RemoteReply::FileChunk { request_id, .. }
            | RemoteReply::FilePushAck { request_id, .. } => request_id,
        }
    }
}
```

to:

```rust
pub enum RemoteReply {
    Exec { request_id: String, prompt: String, error: Option<String> },
    Panel { request_id: String, text: Option<String> },
    Error { request_id: String, message: String },
    FileList { request_id: String, files: Vec<FileMeta>, total: usize },
    FileChunk { request_id: String, data: String },
    FilePushAck { request_id: String },
    StorageManifest { request_id: String, entries: Vec<crate::plugins::SyncEntry>, total: usize },
    Ack { request_id: String },
}

impl RemoteReply {
    pub fn request_id(&self) -> &str {
        match self {
            RemoteReply::Exec { request_id, .. }
            | RemoteReply::Panel { request_id, .. }
            | RemoteReply::Error { request_id, .. }
            | RemoteReply::FileList { request_id, .. }
            | RemoteReply::FileChunk { request_id, .. }
            | RemoteReply::FilePushAck { request_id, .. }
            | RemoteReply::StorageManifest { request_id, .. }
            | RemoteReply::Ack { request_id, .. } => request_id,
        }
    }
}
```

- [ ] **Step 2: Add `read_chunk`/`write_chunk` to `storage.rs`**

Add `use data_encoding::BASE64;` and `use std::fs::OpenOptions;` and `use std::io::{Seek, SeekFrom, Write};` to the top of `src/plugins/storage.rs` (alongside the existing `use` lines).

Add this after `hash_file` (still before the `#[cfg(test)]` block):

```rust
/// 從 `path` 這個檔案的 `offset` 開始讀最多 `FILE_CHUNK_SIZE`（見
/// `crate::plugin::FILE_CHUNK_SIZE`）個位元組，回傳 base64 編碼——給跨 domain
/// 的 `RemoteRequest::StorageFilePull` 處理用（`global.rs` 的
/// `build_remote_reply`），單則 MQTT 訊息塞不下整個檔案，一次只回一個 chunk。
pub(crate) fn read_chunk(path: &Path, offset: u64) -> Result<String> {
    let data = fs::read(path).with_context(|| format!("讀取檔案失敗: {}", path.display()))?;
    let start = (offset as usize).min(data.len());
    let end = (start + crate::plugin::FILE_CHUNK_SIZE).min(data.len());
    Ok(BASE64.encode(&data[start..end]))
}

/// 把 base64 編碼的 `data_b64` 解碼後寫進 `path` 的 `offset` 位置；`offset == 0`
/// 那一次順便建立（或清空）檔案，並確保上層資料夾存在——給跨 domain 的
/// `RemoteRequest::StorageFilePush` 處理用，做法跟 `web.rs` 既有的
/// `files_upload`（同網域版本）一樣是「每個 chunk 各自帶自己的 offset，seek
/// 到對的位置寫入」。
pub(crate) fn write_chunk(path: &Path, offset: u64, data_b64: &str) -> Result<()> {
    let bytes = BASE64.decode(data_b64.as_bytes()).context("chunk 不是合法的 base64")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("建立資料夾失敗: {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(offset == 0)
        .open(path)
        .with_context(|| format!("開啟檔案失敗: {}", path.display()))?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&bytes)?;
    Ok(())
}
```

- [ ] **Step 3: Add match arms in `shell.rs`**

In `src/shell.rs`, change `cross_domain_timeout` (currently `src/shell.rs:395-402`) from:

```rust
fn cross_domain_timeout(ask: &CrossDomainAsk) -> Duration {
    match ask {
        CrossDomainAsk::Exec { .. } | CrossDomainAsk::Panel { .. } => Duration::from_secs(5),
        CrossDomainAsk::FileList { .. } | CrossDomainAsk::FilePull { .. } | CrossDomainAsk::FilePush { .. } => {
            Duration::from_secs(20)
        }
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
        | CrossDomainAsk::StorageDelete { .. } => Duration::from_secs(20),
    }
}
```

In `src/shell.rs`'s `send_via_mqtt` (currently `src/shell.rs:412-458`), change the `let request = match ask { ... };` block from:

```rust
    let request = match ask {
        CrossDomainAsk::Exec { target_id, line } => {
            RemoteRequest::Exec { request_id: request_id.clone(), source_domain, target_id, line }
        }
        CrossDomainAsk::Panel { target_id, panel_name } => {
            RemoteRequest::Panel { request_id: request_id.clone(), source_domain, target_id, panel_name }
        }
        CrossDomainAsk::FileList { target_id, folder, offset } => {
            RemoteRequest::FileList { request_id: request_id.clone(), source_domain, target_id, folder, offset }
        }
        CrossDomainAsk::FilePull { target_id, folder, name, offset } => {
            RemoteRequest::FilePull { request_id: request_id.clone(), source_domain, target_id, folder, name, offset }
        }
        CrossDomainAsk::FilePush { target_id, folder, name, offset, data } => {
            RemoteRequest::FilePush { request_id: request_id.clone(), source_domain, target_id, folder, name, offset, data }
        }
    };
```

to:

```rust
    let request = match ask {
        CrossDomainAsk::Exec { target_id, line } => {
            RemoteRequest::Exec { request_id: request_id.clone(), source_domain, target_id, line }
        }
        CrossDomainAsk::Panel { target_id, panel_name } => {
            RemoteRequest::Panel { request_id: request_id.clone(), source_domain, target_id, panel_name }
        }
        CrossDomainAsk::FileList { target_id, folder, offset } => {
            RemoteRequest::FileList { request_id: request_id.clone(), source_domain, target_id, folder, offset }
        }
        CrossDomainAsk::FilePull { target_id, folder, name, offset } => {
            RemoteRequest::FilePull { request_id: request_id.clone(), source_domain, target_id, folder, name, offset }
        }
        CrossDomainAsk::FilePush { target_id, folder, name, offset, data } => {
            RemoteRequest::FilePush { request_id: request_id.clone(), source_domain, target_id, folder, name, offset, data }
        }
        CrossDomainAsk::StorageManifest { offset } => {
            RemoteRequest::StorageManifest { request_id: request_id.clone(), source_domain, offset }
        }
        CrossDomainAsk::StorageFilePull { path, offset } => {
            RemoteRequest::StorageFilePull { request_id: request_id.clone(), source_domain, path, offset }
        }
        CrossDomainAsk::StorageFilePush { path, offset, data } => {
            RemoteRequest::StorageFilePush { request_id: request_id.clone(), source_domain, path, offset, data }
        }
        CrossDomainAsk::StorageMkdir { path } => {
            RemoteRequest::StorageMkdir { request_id: request_id.clone(), source_domain, path }
        }
        CrossDomainAsk::StorageDelete { path, recursive } => {
            RemoteRequest::StorageDelete { request_id: request_id.clone(), source_domain, path, recursive }
        }
    };
```

- [ ] **Step 4: Add match arms in `global.rs`**

First, in `src/plugins/mod.rs`, add `read_chunk, write_chunk` (new in this task's Step 2) to the same re-export line Task 1 already extended:

```rust
pub(crate) use storage::{
    list_dir, make_dir, paginate_sync_entries, read_chunk, remove, rename_path, safe_storage_path,
    walk_with_hashes, write_chunk, STORAGE_DIR,
};
pub(crate) use storage::SyncEntry;
```

Then, in `src/plugins/global.rs`, add `walk_with_hashes, paginate_sync_entries, read_chunk, write_chunk, make_dir, remove, safe_storage_path, STORAGE_DIR` to whatever `use crate::plugins::{...}` import already exists at the top of the file (add these names to that existing import list — check the file's current imports first, since the exact existing line wasn't fully quoted during research; if there's no such import line yet, add a new one: `use crate::plugins::{make_dir, paginate_sync_entries, read_chunk, remove, safe_storage_path, walk_with_hashes, write_chunk, STORAGE_DIR};`). Also add `use crate::plugins::SyncEntry;` if `RemoteReply::StorageManifest`'s `entries: Vec<crate::plugins::SyncEntry>` field type (Step 1) isn't already fully qualifying the path inline — the fully-qualified form from Step 1 works either way, this import just makes the rest of the file's references less verbose if you choose to use bare `SyncEntry` elsewhere in this file.

Change `request_kind` (currently `src/plugins/global.rs:511-519`) from:

```rust
fn request_kind(request: &RemoteRequest) -> &'static str {
    match request {
        RemoteRequest::Exec { .. } => "Exec",
        RemoteRequest::Panel { .. } => "Panel",
        RemoteRequest::FileList { .. } => "FileList",
        RemoteRequest::FilePull { .. } => "FilePull",
        RemoteRequest::FilePush { .. } => "FilePush",
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
    }
}
```

In `build_remote_reply` (currently `src/plugins/global.rs:426-509`), add five new arms to the `match request { ... }` block, right after the existing `RemoteRequest::FilePush { .. } => { ... }` arm and before the closing `}` of the match:

```rust
        RemoteRequest::StorageManifest { request_id, offset, .. } => match walk_with_hashes(Path::new(STORAGE_DIR)) {
            Ok(all_entries) => {
                let total = all_entries.len();
                let entries = paginate_sync_entries(&all_entries, *offset);
                RemoteReply::StorageManifest { request_id: request_id.clone(), entries, total }
            }
            Err(err) => RemoteReply::Error { request_id: request_id.clone(), message: format!("{err:#}") },
        },
        RemoteRequest::StorageFilePull { request_id, path, offset, .. } => {
            let Some(file_path) = safe_storage_path(Path::new(STORAGE_DIR), path) else {
                return RemoteReply::Error {
                    request_id: request_id.clone(),
                    message: format!("不合法的路徑: {path}"),
                };
            };
            match read_chunk(&file_path, *offset) {
                Ok(data) => RemoteReply::FileChunk { request_id: request_id.clone(), data },
                Err(err) => RemoteReply::Error { request_id: request_id.clone(), message: format!("{err:#}") },
            }
        }
        RemoteRequest::StorageFilePush { request_id, path, offset, data, .. } => {
            let Some(file_path) = safe_storage_path(Path::new(STORAGE_DIR), path) else {
                return RemoteReply::Error {
                    request_id: request_id.clone(),
                    message: format!("不合法的路徑: {path}"),
                };
            };
            match write_chunk(&file_path, *offset, data) {
                Ok(()) => RemoteReply::FilePushAck { request_id: request_id.clone() },
                Err(err) => RemoteReply::Error { request_id: request_id.clone(), message: format!("{err:#}") },
            }
        }
        RemoteRequest::StorageMkdir { request_id, path, .. } => {
            let Some(dir_path) = safe_storage_path(Path::new(STORAGE_DIR), path) else {
                return RemoteReply::Error {
                    request_id: request_id.clone(),
                    message: format!("不合法的路徑: {path}"),
                };
            };
            match make_dir(&dir_path) {
                Ok(()) => RemoteReply::Ack { request_id: request_id.clone() },
                Err(err) => RemoteReply::Error { request_id: request_id.clone(), message: format!("{err:#}") },
            }
        }
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
```

Note these five new arms do **not** call `target_ip`/look up a `target_id` (there isn't one on these variants) — they operate directly on this device's own `STORAGE_DIR`, per the design note above.

- [ ] **Step 5: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -40`
Expected: builds with no errors. You will likely need to fix a few things iteratively here since this task changes three `pub enum`s with exhaustive matches elsewhere in the codebase — the compiler will point at every match that needs a new arm (that's the whole point of Step 1-4 above; if you missed a spot, `cargo build`'s error output names the exact file:line).

Run: `cargo test 2>&1 | tail -15`
Expected: all existing tests still pass (this task adds no new `#[test]` functions of its own — `read_chunk`/`write_chunk` are exercised indirectly by Task 6's tests, not here).

- [ ] **Step 6: Commit**

```bash
git add src/plugin.rs src/shell.rs src/plugins/global.rs src/plugins/storage.rs src/plugins/mod.rs
git commit -m "$(cat <<'EOF'
新增跨 domain 的 storage 同步請求種類

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: `SyncTransport` trait and same-domain `HttpTransport`

**Files:**
- Modify: `src/plugins/sync.rs` (add the `SyncTransport` trait and `HttpTransport` struct/impl)
- Modify: `src/web.rs` (add the new `GET /api/storage/sync-manifest` endpoint + route)

**Interfaces:**
- Consumes: `walk_with_hashes`/`SyncEntry` (Task 1), `url_encode_filename` (already `pub(crate)` in `src/plugins/files.rs`, re-exported via `src/plugins/mod.rs`), `crate::web::PORT`.
- Produces:
  - `pub(crate) trait SyncTransport { fn manifest(&self) -> Result<Vec<SyncEntry>>; fn download_to(&self, path: &str, expected_size: u64, dest: &Path) -> Result<()>; fn upload_from(&self, path: &str, src: &Path) -> Result<()>; fn mkdir(&self, path: &str) -> Result<()>; fn delete(&self, path: &str, recursive: bool) -> Result<()>; }` — object-safe (no generics, no `Self: Sized` methods), so `Box<dyn SyncTransport>` works. `expected_size` comes from the manifest entry that triggered the pull; `HttpTransport` ignores it (plain HTTP download reads to EOF regardless), `CrossDomainTransport` (Task 6) needs it as the chunk-loop's stop condition, matching `files.rs`'s existing `pull_file_mqtt(..., meta: &FileMeta, ...)` which loops `while offset < meta.size`.
  - `pub(crate) struct HttpTransport { pub(crate) ip: String }` implementing it, for same-domain partners.
  - `GET /api/storage/sync-manifest` — returns the full `Vec<SyncEntry>` unpaginated (same-domain HTTP has no MQTT-style message-size limit, matching this file's existing `storage_list`/`files_list`/`music_files` endpoints, which are also unpaginated).
- Consumed by: Task 7's engine (constructs an `HttpTransport { ip }` for each same-domain client) and Task 6 (which implements the same trait for cross-domain).

- [ ] **Step 1: Add the new endpoint**

In `src/web.rs`, add this handler function right after the existing `storage_rename` function:

```rust
/// `GET /api/storage/sync-manifest`：回傳整棵 `storage/` 樹（含子資料夾、每個
/// 檔案的 hash）攤平後的清單，同網域的 `sync` plugin 用這個端點取得對方的完
/// 整清單去跟本機清單、baseline 比對。同網域走 HTTP，沒有 MQTT 那種單則訊息
/// 大小限制，所以不分頁，直接回傳全部，跟這個檔案裡其他既有的
/// `storage_list`/`files_list`/`music_files` 端點一樣不分頁。
async fn storage_sync_manifest() -> HttpResponse {
    match walk_with_hashes(Path::new(STORAGE_DIR)) {
        Ok(entries) => HttpResponse::Ok().json(entries),
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}
```

Add `walk_with_hashes` to the existing `use crate::plugins::{...}` import block in `src/web.rs` (the one that already has `list_dir, make_dir, remove, rename_path, safe_storage_path, STORAGE_DIR` from Task 3 of the storage plugin's earlier plan — add `walk_with_hashes` to that same list).

Add the route registration right after `.route("/api/storage/rename", web::post().to(storage_rename))`:

```rust
            .route("/api/storage/rename", web::post().to(storage_rename))
            .route("/api/storage/sync-manifest", web::get().to(storage_sync_manifest))
```

- [ ] **Step 2: Run the build to confirm the endpoint compiles**

Run: `cargo build 2>&1 | tail -20`
Expected: clean build, no errors.

- [ ] **Step 3: Add the `SyncTransport` trait and `HttpTransport`**

Add this to `src/plugins/sync.rs`, above the `#[cfg(test)]` block (after the `classify` function from Task 3):

```rust
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::plugins::url_encode_filename;
use crate::web::PORT;

/// 跟一個同步對象「怎麼溝通」的抽象介面——分類演算法（`classify`）算完要做
/// 什麼事之後，實際執行搬檔/建資料夾/刪除都透過這個介面，不管對方是同網域
/// （`HttpTransport`）還是跨 domain（Task 6 的 `CrossDomainTransport`），呼叫端
/// 的程式碼完全一樣。物件安全（沒有泛型方法、沒有 `Self: Sized` 限制），可以
/// 用 `Box<dyn SyncTransport>` 依角色動態選擇實作。
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

/// 同網域的同步對象，透過既有的 `/api/storage/...` 端點溝通——不重做傳輸層，
/// 直接呼叫 `storage` plugin 已經做好的 HTTP API。
pub(crate) struct HttpTransport {
    pub(crate) ip: String,
}

impl SyncTransport for HttpTransport {
    fn manifest(&self) -> Result<Vec<SyncEntry>> {
        let url = format!("http://{}:{PORT}/api/storage/sync-manifest", self.ip);
        let output = Command::new("curl")
            .args(["--silent", "--fail", "--max-time", "30", &url])
            .output()
            .context("執行 curl 失敗")?;
        if !output.status.success() {
            bail!("查詢 sync-manifest 失敗");
        }
        let body = String::from_utf8(output.stdout).context("回應不是合法的 UTF-8")?;
        serde_json::from_str(&body).context("回應格式不對")
    }

    fn download_to(&self, path: &str, _expected_size: u64, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("建立資料夾失敗: {}", parent.display()))?;
        }
        let url = format!("http://{}:{PORT}/api/storage/download?path={}", self.ip, url_encode_filename(path));
        let output = Command::new("curl")
            .args(["--silent", "--fail", "--max-time", "120", "-o", &dest.display().to_string(), &url])
            .output()
            .context("執行 curl 失敗")?;
        if !output.status.success() {
            bail!("下載失敗: {path}");
        }
        Ok(())
    }

    fn upload_from(&self, path: &str, src: &Path) -> Result<()> {
        let url = format!("http://{}:{PORT}/api/storage/upload?path={}", self.ip, url_encode_filename(path));
        let output = Command::new("curl")
            .args([
                "--silent",
                "--fail",
                "--max-time",
                "120",
                "-X",
                "POST",
                "--data-binary",
                &format!("@{}", src.display()),
                &url,
            ])
            .output()
            .context("執行 curl 失敗")?;
        if !output.status.success() {
            bail!("上傳失敗: {path}");
        }
        Ok(())
    }

    fn mkdir(&self, path: &str) -> Result<()> {
        let url = format!("http://{}:{PORT}/api/storage/mkdir?path={}", self.ip, url_encode_filename(path));
        let output = Command::new("curl")
            .args(["--silent", "--fail", "--max-time", "10", "-X", "POST", &url])
            .output()
            .context("執行 curl 失敗")?;
        if !output.status.success() {
            bail!("建立資料夾失敗: {path}");
        }
        Ok(())
    }

    fn delete(&self, path: &str, recursive: bool) -> Result<()> {
        let url =
            format!("http://{}:{PORT}/api/storage/delete?path={}&recursive={recursive}", self.ip, url_encode_filename(path));
        let output = Command::new("curl")
            .args(["--silent", "--fail", "--max-time", "30", "-X", "POST", &url])
            .output()
            .context("執行 curl 失敗")?;
        if !output.status.success() {
            bail!("刪除失敗: {path}");
        }
        Ok(())
    }
}
```

Note: this task adds no new `#[test]` functions — `HttpTransport` is thin `curl`-shelling network code, matching this codebase's existing convention of not unit-testing network transport code directly (`push_file_http`/`pull_file_http` in `files.rs` aren't tested either). `classify` (Task 3) already carries the algorithm's test coverage; `HttpTransport` is exercised end-to-end in Task 7's manual verification step.

- [ ] **Step 4: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -30`
Expected: clean build, no errors, no warnings.

Run: `cargo test 2>&1 | tail -15`
Expected: all existing tests still pass.

- [ ] **Step 5: Manual smoke test of the new endpoint**

Run: `cargo run` (starts the web server on port 9759).

From another terminal: `curl -s "http://localhost:9759/api/storage/sync-manifest" | head -c 500`
Expected: a JSON array (possibly empty `[]` if `storage/` has no content yet) matching `SyncEntry`'s shape (`{"path":...,"is_dir":...,"size":...,"modified":...,"hash":...}` per entry).

- [ ] **Step 6: Commit**

```bash
git add src/plugins/sync.rs src/web.rs
git commit -m "$(cat <<'EOF'
新增 SyncTransport 抽象與同網域的 HttpTransport

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Cross-domain `CrossDomainTransport`

**Files:**
- Modify: `src/plugins/sync.rs` (add `CrossDomainTransport`)

**Interfaces:**
- Consumes: Task 4's five new `CrossDomainAsk`/`RemoteReply` variants, `crate::shell::send_cross_domain_request`, `crate::plugin::SharedContext`.
- Produces: `pub(crate) struct CrossDomainTransport { pub(crate) ctx: SharedContext, pub(crate) domain: String }` implementing Task 5's `SyncTransport` trait.
- Consumed by: Task 7's engine (constructs a `CrossDomainTransport { ctx, domain }` for each visible cross-domain partner).

- [ ] **Step 1: Implement `CrossDomainTransport`**

Add this to `src/plugins/sync.rs`, right after `HttpTransport`'s `impl SyncTransport` block (still before `#[cfg(test)]`):

```rust
use std::io::Write;

use crate::plugin::{CrossDomainAsk, RemoteReply, SharedContext};
use crate::shell::send_cross_domain_request;

/// 跨 domain 的同步對象，透過 Task 4 新增的 `CrossDomainAsk::Storage*` 系列，走
/// 既有的 MQTT 中繼機制（`send_cross_domain_request`）溝通，做法比照
/// `files.rs` 的 `list_remote_files_mqtt`/`push_file_mqtt`/`pull_file_mqtt`
/// （4KB chunk、逐段往返），只是路徑換成 storage 底下的巢狀相對路徑，而且不需
/// 要 `target_id`（見 Task 4 的設計說明：跨 domain 同步是 server 對 server，
/// 對方直接回答自己的 `storage/`，不用再往下 relay 給某個特定裝置）。
pub(crate) struct CrossDomainTransport {
    pub(crate) ctx: SharedContext,
    pub(crate) domain: String,
}

impl SyncTransport for CrossDomainTransport {
    fn manifest(&self) -> Result<Vec<SyncEntry>> {
        let mut entries = Vec::new();
        loop {
            let ask = CrossDomainAsk::StorageManifest { offset: entries.len() };
            match send_cross_domain_request(&self.ctx, &self.domain, ask)? {
                RemoteReply::StorageManifest { entries: page, total, .. } => {
                    if page.is_empty() {
                        break;
                    }
                    entries.extend(page);
                    if entries.len() >= total {
                        break;
                    }
                }
                RemoteReply::Error { message, .. } => bail!(message),
                _ => bail!("收到不符預期的回覆型別"),
            }
        }
        Ok(entries)
    }

    fn download_to(&self, path: &str, expected_size: u64, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("建立資料夾失敗: {}", parent.display()))?;
        }
        let mut file = fs::File::create(dest).with_context(|| format!("建立檔案失敗: {}", dest.display()))?;
        if expected_size == 0 {
            return Ok(()); // 空檔案：建立完就結束，不需要真的要任何 chunk。
        }
        let mut offset: u64 = 0;
        while offset < expected_size {
            let ask = CrossDomainAsk::StorageFilePull { path: path.to_string(), offset };
            let data = match send_cross_domain_request(&self.ctx, &self.domain, ask)? {
                RemoteReply::FileChunk { data, .. } => data,
                RemoteReply::Error { message, .. } => bail!(message),
                _ => bail!("收到不符預期的回覆型別"),
            };
            let bytes = data_encoding::BASE64.decode(data.as_bytes()).context("chunk 不是合法的 base64")?;
            if bytes.is_empty() {
                bail!("遠端回傳空的 chunk（檔案可能在傳輸過程中被改動），已知大小: {expected_size}");
            }
            file.write_all(&bytes)?;
            offset += bytes.len() as u64;
        }
        Ok(())
    }

    fn upload_from(&self, path: &str, src: &Path) -> Result<()> {
        let data = fs::read(src).with_context(|| format!("讀取檔案失敗: {}", src.display()))?;
        let mut offset: usize = 0;
        loop {
            let end = (offset + crate::plugin::FILE_CHUNK_SIZE).min(data.len());
            let chunk = &data[offset..end];
            let ask = CrossDomainAsk::StorageFilePush {
                path: path.to_string(),
                offset: offset as u64,
                data: data_encoding::BASE64.encode(chunk),
            };
            match send_cross_domain_request(&self.ctx, &self.domain, ask)? {
                RemoteReply::FilePushAck { .. } => {}
                RemoteReply::Error { message, .. } => bail!(message),
                _ => bail!("收到不符預期的回覆型別"),
            }
            offset = end;
            // 空檔案也要送這一次「第 0 個 chunk、內容是空的」請求，讓對面至少
            // 建立一個空檔案出來，見 `files.rs` 的 `push_file_mqtt` 同樣的理由。
            if offset >= data.len() {
                break;
            }
        }
        Ok(())
    }

    fn mkdir(&self, path: &str) -> Result<()> {
        let ask = CrossDomainAsk::StorageMkdir { path: path.to_string() };
        match send_cross_domain_request(&self.ctx, &self.domain, ask)? {
            RemoteReply::Ack { .. } => Ok(()),
            RemoteReply::Error { message, .. } => bail!(message),
            _ => bail!("收到不符預期的回覆型別"),
        }
    }

    fn delete(&self, path: &str, recursive: bool) -> Result<()> {
        let ask = CrossDomainAsk::StorageDelete { path: path.to_string(), recursive };
        match send_cross_domain_request(&self.ctx, &self.domain, ask)? {
            RemoteReply::Ack { .. } => Ok(()),
            RemoteReply::Error { message, .. } => bail!(message),
            _ => bail!("收到不符預期的回覆型別"),
        }
    }
}
```

No new `use` line is needed for `data_encoding::BASE64` — the code above references it via its fully-qualified path (`data_encoding::BASE64.decode(...)`), which resolves without an import as long as the crate is a dependency, and it already is (`data_encoding` is in `Cargo.toml`, already used the same way by `files.rs`) — no new dependency here.

This task adds no new `#[test]` functions — `CrossDomainTransport`, like `HttpTransport`, is network transport code exercised end-to-end in Task 7's manual verification, not unit-tested directly (matching this codebase's convention, and matching how `files.rs`'s `push_file_mqtt`/`pull_file_mqtt`/`list_remote_files_mqtt` aren't unit-tested either).

- [ ] **Step 2: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -30`
Expected: clean build, no errors, no warnings.

Run: `cargo test 2>&1 | tail -15`
Expected: all existing tests still pass.

- [ ] **Step 3: Commit**

```bash
git add src/plugins/sync.rs
git commit -m "$(cat <<'EOF'
新增跨 domain 的 CrossDomainTransport

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: Conflict resolution and the sync engine

**Files:**
- Modify: `src/plugins/sync.rs` (add conflict-copy naming, a dependency-free epoch→date helper, `run_sync_pass`, and the per-partner status tracking)

**Interfaces:**
- Consumes: `SyncTransport`/`HttpTransport`/`CrossDomainTransport` (Tasks 5-6), `classify`/`SyncAction` (Task 3), `load_baseline`/`save_baseline`/`Baseline`/`BaselineEntry` (Task 2), `walk_with_hashes`/`STORAGE_DIR` (Task 1), `SYNC_STATE_DIR` (Task 2).
- Produces:
  - `pub(crate) fn conflict_copy_name(path: &str, label: &str, date: &str) -> String`
  - `pub(crate) fn epoch_to_utc_date(epoch_secs: u64) -> String`
  - `pub(crate) struct SyncOutcome { pub(crate) pushed: usize, pub(crate) pulled: usize, pub(crate) deleted_local: usize, pub(crate) deleted_remote: usize, pub(crate) conflicts: usize, pub(crate) error: Option<String> }` (derives `Clone, Debug, Default, PartialEq`)
  - `pub(crate) fn run_sync_pass(ctx: &SharedContext, transport: &dyn SyncTransport, partner_key: &str, my_label: &str, partner_label: &str) -> SyncOutcome` — runs one full classify+execute+persist-baseline pass against one partner.
- Consumed by: Task 8's `SyncPlugin` background thread (calls `run_sync_pass` once per known partner per poll cycle).

**Accepted quirk, documented rather than engineered around:** after a conflict is resolved, the two new conflict-copy files (e.g. `f (衝突自 B，2026-07-26).txt` created locally, `f (衝突自 A，2026-07-26).txt` created on the partner) are deliberately **not** given a baseline entry. On the *next* sync pass, each of those brand-new files will look like an ordinary new local-only (or remote-only) file and get pushed/pulled like anything else — meaning each side eventually also receives a copy of the *other* side's conflict-copy file too (e.g. B ends up with a redundant `f (衝突自 B，date).txt` — a copy of its own old content, mirrored back to itself). This is harmless (no data loss, no incorrect deletion) and simpler than tracking a "never sync this specific file again" exemption list; it is called out here so it doesn't look like a bug during review.

- [ ] **Step 1: Write the failing tests for the pure helpers**

Add this inside the existing `#[cfg(test)] mod tests { ... }` block in `src/plugins/sync.rs` (append after the last Task 3 test, before the closing `}`):

```rust
    #[test]
    fn epoch_to_utc_date_known_values() {
        assert_eq!(epoch_to_utc_date(0), "1970-01-01");
        assert_eq!(epoch_to_utc_date(1785031586), "2026-07-24");
    }

    #[test]
    fn conflict_copy_name_inserts_marker_before_extension() {
        assert_eq!(
            conflict_copy_name("photos/beach.jpg", "office-pc", "2026-07-26"),
            "photos/beach (衝突自 office-pc，2026-07-26).jpg"
        );
    }

    #[test]
    fn conflict_copy_name_handles_no_extension() {
        assert_eq!(conflict_copy_name("README", "office-pc", "2026-07-26"), "README (衝突自 office-pc，2026-07-26)");
    }

    #[test]
    fn conflict_copy_name_handles_root_level_file() {
        assert_eq!(
            conflict_copy_name("notes.txt", "branch-b", "2026-07-26"),
            "notes (衝突自 branch-b，2026-07-26).txt"
        );
    }

    #[test]
    fn conflict_copy_name_preserves_directory_prefix() {
        let name = conflict_copy_name("a/b/c/deep.txt", "x", "2026-01-01");
        assert_eq!(name, "a/b/c/deep (衝突自 x，2026-01-01).txt");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test plugins::sync:: 2>&1 | tail -40`
Expected: compile error — `cannot find function 'epoch_to_utc_date'` / `'conflict_copy_name'` in this scope.

- [ ] **Step 3: Implement the pure helpers**

Add this to `src/plugins/sync.rs`, after the `classify` function and the transport `impl` blocks from Tasks 5-6 (still before `#[cfg(test)]`):

```rust
/// 從 unix epoch 秒數算出 UTC 曆法日期字串 `YYYY-MM-DD`，純算術、不依賴任何
/// 時間函式庫——沿用這個專案 `build.rs` 算編譯時間戳同樣的原則：不為了這裡
/// 需要一個日期字串就額外引入日期時間 crate。演算法出處：Howard Hinnant 的
/// `civil_from_days`（把「距離 1970-01-01 過了幾天」換算成西曆年月日，是這個
/// 換算方向被廣泛驗證過的標準寫法）。
pub(crate) fn epoch_to_utc_date(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86400) as i64;
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// 把一個路徑改成帶衝突標記的新檔名，標記插在副檔名前面（沒有副檔名就接在
/// 檔名後面），資料夾前綴維持不變。
pub(crate) fn conflict_copy_name(path: &str, label: &str, date: &str) -> String {
    let (dir, filename) = match path.rsplit_once('/') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, path),
    };
    let (stem, ext) = match filename.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (filename, None),
    };
    let new_name = match ext {
        Some(ext) => format!("{stem} (衝突自 {label}，{date}).{ext}"),
        None => format!("{stem} (衝突自 {label}，{date})"),
    };
    match dir {
        Some(dir) => format!("{dir}/{new_name}"),
        None => new_name,
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test plugins::sync:: 2>&1 | tail -40`
Expected: all 4 new tests pass (23 total in `plugins::sync::tests`: 19 from Task 3 + 4 new).

- [ ] **Step 5: Implement `SyncOutcome` and `run_sync_pass`**

Add this to `src/plugins/sync.rs`, after the pure helpers from Step 3 (still before `#[cfg(test)]`):

```rust
use std::time::{SystemTime, UNIX_EPOCH};

use crate::plugins::sync_baseline::{load_baseline, save_baseline, SYNC_STATE_DIR};
use crate::plugins::{walk_with_hashes, STORAGE_DIR};

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

fn find_hash<'a>(entries: &'a [SyncEntry], path: &str) -> Option<&'a str> {
    entries.iter().find(|e| e.path == path && !e.is_dir).and_then(|e| e.hash.as_deref())
}

fn find_size(entries: &[SyncEntry], path: &str) -> u64 {
    entries.iter().find(|e| e.path == path && !e.is_dir).map(|e| e.size).unwrap_or(0)
}

/// 處理一個真衝突：把對方的版本下載成本機一份帶衝突標記的新檔案，同時把本機
/// 原本的版本上傳成對方一份帶衝突標記的新檔案，兩邊各自原本的檔案都不動。
fn resolve_conflict(
    transport: &dyn SyncTransport,
    path: &str,
    my_label: &str,
    partner_label: &str,
    remote_size: u64,
) -> Result<()> {
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let date = epoch_to_utc_date(epoch);
    let local_root = Path::new(STORAGE_DIR);

    let local_copy_path = conflict_copy_name(path, partner_label, &date);
    let local_copy_dest = local_root.join(&local_copy_path);
    transport.download_to(path, remote_size, &local_copy_dest)?;

    let remote_copy_path = conflict_copy_name(path, my_label, &date);
    let local_original = local_root.join(path);
    transport.upload_from(&remote_copy_path, &local_original)?;

    Ok(())
}

/// 對 `path` 這個目的地確保上層資料夾在對方那邊存在——`upload_from`/
/// `HttpTransport` 底層的 `/api/storage/upload` 不會自動建立中間目錄（見
/// `storage_upload` 的文件），推新檔案進一個新的子資料夾之前要先在對方那邊
/// `mkdir` 過。只在資料夾還沒出現在 `remote` 清單裡時才呼叫，避免每次都白跑
/// 一次 mkdir 請求。
fn ensure_remote_parent_dirs(transport: &dyn SyncTransport, path: &str, remote: &[SyncEntry]) -> Result<()> {
    let Some((dir, _name)) = path.rsplit_once('/') else { return Ok(()) };
    let known_dirs: std::collections::HashSet<&str> =
        remote.iter().filter(|e| e.is_dir).map(|e| e.path.as_str()).collect();
    let mut acc = String::new();
    for segment in dir.split('/') {
        acc = if acc.is_empty() { segment.to_string() } else { format!("{acc}/{segment}") };
        if !known_dirs.contains(acc.as_str()) {
            transport.mkdir(&acc)?;
        }
    }
    Ok(())
}

/// 對一個同步對象跑完整的一輪：算本機清單、拿對方清單、讀 baseline、分類、
/// 執行、把成功的部分寫回 baseline。任何一步失敗都記錄在回傳的 `error`
/// 裡，不會 `panic`，讓背景執行緒可以繼續處理下一個對象。
pub(crate) fn run_sync_pass(
    _ctx: &SharedContext,
    transport: &dyn SyncTransport,
    partner_key: &str,
    my_label: &str,
    partner_label: &str,
) -> SyncOutcome {
    let mut outcome = SyncOutcome::default();
    let local_root = Path::new(STORAGE_DIR);

    let local = match walk_with_hashes(local_root) {
        Ok(l) => l,
        Err(err) => {
            outcome.error = Some(format!("讀取本機清單失敗: {err:#}"));
            return outcome;
        }
    };
    let remote = match transport.manifest() {
        Ok(r) => r,
        Err(err) => {
            outcome.error = Some(format!("取得對方清單失敗: {err:#}"));
            return outcome;
        }
    };

    let state_dir = Path::new(SYNC_STATE_DIR);
    let mut baseline = load_baseline(state_dir, partner_key);
    let actions = classify(&local, &remote, &baseline);

    for action in actions {
        match action {
            SyncAction::PushToRemote { path } => {
                let result = ensure_remote_parent_dirs(transport, &path, &remote)
                    .and_then(|_| transport.upload_from(&path, &local_root.join(&path)));
                match result {
                    Ok(()) => {
                        if let Some(hash) = find_hash(&local, &path) {
                            baseline.insert(
                                path,
                                BaselineEntry { local_hash: hash.to_string(), remote_hash: hash.to_string() },
                            );
                        }
                        outcome.pushed += 1;
                    }
                    Err(err) => outcome.error = Some(format!("推送 {path} 失敗: {err:#}")),
                }
            }
            SyncAction::PullFromRemote { path } => {
                let dest = local_root.join(&path);
                let size = find_size(&remote, &path);
                match transport.download_to(&path, size, &dest) {
                    Ok(()) => {
                        if let Some(hash) = find_hash(&remote, &path) {
                            baseline.insert(
                                path,
                                BaselineEntry { local_hash: hash.to_string(), remote_hash: hash.to_string() },
                            );
                        }
                        outcome.pulled += 1;
                    }
                    Err(err) => outcome.error = Some(format!("拉取 {path} 失敗: {err:#}")),
                }
            }
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
                            baseline.insert(
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

    if let Err(err) = save_baseline(state_dir, partner_key, &baseline) {
        outcome.error = Some(format!("寫入 baseline 失敗: {err:#}"));
    }
    outcome
}
```

This task adds no tests for `run_sync_pass`/`resolve_conflict`/`ensure_remote_parent_dirs` themselves (they're network+filesystem orchestration, matching this codebase's existing convention of not unit-testing that layer); `classify` (Task 3) already carries the decision-logic coverage, and Task 8's manual end-to-end verification exercises the full path.

- [ ] **Step 6: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -30`
Expected: clean build, no errors, no warnings.

Run: `cargo test 2>&1 | tail -20`
Expected: all tests pass, including the 4 new ones in `plugins::sync::tests`.

- [ ] **Step 7: Commit**

```bash
git add src/plugins/sync.rs
git commit -m "$(cat <<'EOF'
新增 sync 的衝突處理與整輪同步執行邏輯

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: `SyncPlugin`, background loop, registration, and manual verification

**Files:**
- Modify: `src/plugins/sync.rs` (add `SyncPlugin`, the background poll loop, partner enumeration with the domain tie-break rule)
- Modify: `src/plugins/mod.rs` (add `pub use sync::SyncPlugin;`)
- Modify: `src/main.rs` (register `SyncPlugin`)

**Interfaces:**
- Consumes: `run_sync_pass`/`SyncOutcome` (Task 7), `HttpTransport`/`CrossDomainTransport` (Tasks 5-6), `crate::plugin::{SharedContext, merged_global_view}`, `sysinfo::hostname`. `HashMap`/`HashSet` are already imported at the top of `src/plugins/sync.rs` from Task 3's `use std::collections::{HashMap, HashSet};` — do not re-import them in this task's new `use` block, Rust rejects a name imported twice into the same module scope even when both imports resolve to the identical item.
- Produces: `pub struct SyncPlugin` with `pub fn new(ctx: SharedContext) -> Self`, registered in `main.rs`'s plugin factory list exactly like every other plugin.

- [ ] **Step 1: Implement `SyncPlugin`**

Add this to `src/plugins/sync.rs`, after `run_sync_pass` (still before `#[cfg(test)]`):

```rust
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::output::OutputBuffer;
use crate::plugin::{merged_global_view, Plugin};
use crate::sysinfo;

/// 背景輪詢間隔——獨立於 `system` plugin 的 10 秒裝置回報間隔（`REPORT_INTERVAL`），
/// 同步整棵樹（含每個檔案重新算 hash）比單純回報裝置狀態貴得多，不用共用同一個
/// 頻率。
const SYNC_POLL_INTERVAL: Duration = Duration::from_secs(60);

const MANUAL_TEXT: &str = "\
sync：把本機 storage plugin 管理的整棵 storage/ 樹，跟其他裝置雙向同步（含刪除
傳遞、衝突處理），完全複用 system plugin 既有的 client/server/domain 角色，不用
另外設定要跟誰同步。

沒有手動觸發指令：只有這台裝置是 mode server 時，才會啟動背景輪詢（預設每 60
秒），自動對同 domain 底下每一個已知的 client、以及 global registry 看得到的
每一個別的 domain 的 server，各自跑一次同步。client 角色完全被動，不會主動做
任何事，最終還是會透過 server 端的輪詢自動同步到，只是有接力延遲。

指令：
  status              列出每個同步對象上次同步的時間、結果、搬了幾個檔案、
                       刪了幾個、產生幾個衝突副本

真的衝突（雙方自上次同步後都改過同一個檔案）不會覆蓋任何一邊，兩邊都會保留
自己原本的檔案，並且各自多一份帶「(衝突自 <對方>，日期)」標記的對方版本副
本，需要使用者自己手動整理。
";

#[derive(Clone, Debug, Default)]
struct PartnerStatus {
    last_run: Option<Instant>,
    outcome: SyncOutcome,
}

/// 沒有任何持久化狀態以外的欄位——`statuses` 只是給 `status` 指令/panel 顯示
/// 用的執行期摘要，重啟就沒了（真正需要跨重啟保留的是 baseline，見 Task 2，
/// 那個已經寫到磁碟）。
pub struct SyncPlugin {
    ctx: SharedContext,
    statuses: Arc<Mutex<HashMap<String, PartnerStatus>>>,
}

impl SyncPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        let statuses = Arc::new(Mutex::new(HashMap::new()));
        Self::spawn_engine(ctx.clone(), statuses.clone());
        Self { ctx, statuses }
    }

    fn spawn_engine(ctx: SharedContext, statuses: Arc<Mutex<HashMap<String, PartnerStatus>>>) {
        thread::spawn(move || loop {
            let is_server = ctx.lock().unwrap().is_server;
            if is_server {
                run_all_partners(&ctx, &statuses);
            }
            thread::sleep(SYNC_POLL_INTERVAL);
        });
    }

    fn status_text(&self) -> String {
        let statuses = self.statuses.lock().unwrap();
        if statuses.is_empty() {
            return "目前還沒有任何同步紀錄\n".to_string();
        }
        let mut keys: Vec<&String> = statuses.keys().collect();
        keys.sort();
        let mut text = String::new();
        for key in keys {
            let status = &statuses[key];
            let elapsed = status.last_run.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            let result = match &status.outcome.error {
                Some(err) => format!("失敗: {err}"),
                None => "成功".to_string(),
            };
            text.push_str(&format!(
                "{key}: {result}（{elapsed} 秒前）推 {} 拉 {} 刪本機 {} 刪對方 {} 衝突 {}\n",
                status.outcome.pushed,
                status.outcome.pulled,
                status.outcome.deleted_local,
                status.outcome.deleted_remote,
                status.outcome.conflicts,
            ));
        }
        text
    }
}

impl Plugin for SyncPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["status"]
    }

    fn dispatch(&mut self, cmd: &str, _args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "status" => {
                out.push(&self.status_text());
                Ok(())
            }
            other => bail!("sync 不認得指令: {other}"),
        }
    }

    fn panel_text(&self) -> Option<String> {
        Some(self.status_text())
    }

    fn manual_text(&self) -> &'static str {
        MANUAL_TEXT
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

fn record_status(
    statuses: &Arc<Mutex<HashMap<String, PartnerStatus>>>,
    key: &str,
    outcome: SyncOutcome,
) {
    statuses
        .lock()
        .unwrap()
        .insert(key.to_string(), PartnerStatus { last_run: Some(Instant::now()), outcome });
}

/// 一輪背景輪詢：對同 domain 底下每一個已知的 client、以及看得到的每一個別
/// 的 domain（套用字典序 tie-break），各自跑一次 `run_sync_pass`，並把結果
/// 記進 `statuses`、寫進 activities log。任何一個對象失敗都不影響其他對象
/// 繼續跑。
fn run_all_partners(ctx: &SharedContext, statuses: &Arc<Mutex<HashMap<String, PartnerStatus>>>) {
    let my_hostname = sysinfo::hostname();

    let (clients, my_domain, peer_domains): (Vec<(String, String)>, Option<String>, Vec<String>) = {
        let inner = ctx.lock().unwrap();
        let clients: Vec<(String, String)> = inner
            .devices
            .iter()
            .filter(|(id, _)| id.as_str() != my_hostname.as_str())
            .map(|(id, entry)| (id.clone(), entry.report.ip.clone()))
            .collect();
        let my_domain = inner.domain_name.clone();
        let peer_domains: HashSet<String> = merged_global_view(&inner).into_iter().map(|item| item.domain).collect();
        (clients, my_domain, peer_domains.into_iter().collect())
    };

    for (client_id, ip) in clients {
        let transport = HttpTransport { ip };
        let partner_key = format!("client-{client_id}");
        let outcome = run_sync_pass(ctx, &transport, &partner_key, &my_hostname, &client_id);
        log_outcome(ctx, &partner_key, &outcome);
        record_status(statuses, &partner_key, outcome);
    }

    if let Some(my_domain) = &my_domain {
        for peer_domain in peer_domains {
            if my_domain.as_str() >= peer_domain.as_str() {
                // Tie-break：兩個 domain 互相看得到彼此時，只有名稱字典序比較
                // 小的那一方才主動發起，避免兩邊同時互相發起造成競態（見設計
                // 文件「拓撲與觸發時機」一節）。`>=`（不是單純 `>`）這個等號
                // 也順便處理了另一件事：`merged_global_view` 回傳的清單裡本來
                // 就包含「自己 domain 底下的裝置」這一份（用自己的
                // `domain_name` 當 `domain` 欄位），`peer_domain == my_domain`
                // 這個情況一定會被這行擋掉，不會誤把自己的 domain 當成一個要
                // 主動發起同步的跨 domain 對象。
                continue;
            }
            let transport = CrossDomainTransport { ctx: ctx.clone(), domain: peer_domain.clone() };
            let partner_key = format!("domain-{peer_domain}");
            let outcome = run_sync_pass(ctx, &transport, &partner_key, my_domain, &peer_domain);
            log_outcome(ctx, &partner_key, &outcome);
            record_status(statuses, &partner_key, outcome);
        }
    }
}

fn log_outcome(ctx: &SharedContext, partner_key: &str, outcome: &SyncOutcome) {
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

- [ ] **Step 2: Register the plugin**

In `src/plugins/mod.rs`, add the export next to the other `pub use` lines, alphabetically (`sync` sorts after `storage` and before `sync_baseline`, but `sync_baseline` has no `pub use` — just add this line anywhere in the `pub use` group, e.g. right after `pub use storage::StoragePlugin;`):

```rust
pub use storage::StoragePlugin;
pub use sync::SyncPlugin;
```

In `src/main.rs`, add `SyncPlugin` to the `use plugins::{...}` import list:

```rust
use plugins::{
    ActivitiesPlugin, ClockPlugin, DevicePlugin, FilesPlugin, GitRepoPlugin, GlobalPlugin, MusicPlugin,
    NotepadPlugin, OutputPlugin, QrPlugin, RemoteOutputPlugin, RemotePlugin, StoragePlugin, SyncPlugin,
    SystemPlugin, WeatherPlugin, WolPlugin,
};
```

Add a factory entry to the `factories` vec in `src/main.rs`, right after the `"storage"` entry:

```rust
        (
            "storage",
            Box::new(|ctx| Box::new(StoragePlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "sync",
            Box::new(|ctx| Box::new(SyncPlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "system",
```

- [ ] **Step 3: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -40`
Expected: clean build, no errors, no warnings.

Run: `cargo test 2>&1 | tail -20`
Expected: all tests pass (23 in `plugins::sync::tests` + everything else).

- [ ] **Step 4: Manual end-to-end verification**

This feature needs two running instances to verify meaningfully. If you only have one machine available, do as much of this as you can and clearly report what wasn't verified and why — don't skip silently.

Setup (two terminals/machines, call them A and B, both able to reach each other's port 9759):

On A: `cargo run`, then at the root prompt:
```
system
mode server
exit
```

On B: `cargo run`, then at the root prompt:
```
system
mode client
server <A's IP>
exit
```

Wait at least 70 seconds (one poll cycle plus margin). Confirm:
1. On A, create a file under `storage/` (e.g. `echo hello > storage/test.txt` from a shell, or use the `storage` plugin's `mkdir`/CLI, or the web UI's upload button). Wait 70 seconds. Confirm the file appears in B's `storage/test.txt` too (server pushed/pulled it to the passive client — this is the "client did nothing, still got synced" behavior confirmed earlier in the design conversation).
2. On B, create a *different* new file directly in its `storage/` folder. Wait 70 seconds. Confirm it appears on A (server actively pulled a change that originated on the passive client).
3. Delete the file from step 1 on A. Wait 70 seconds. Confirm it's deleted on B too (deletion propagation).
4. Create a genuine conflict: on A and B simultaneously (within the same ~60s window), edit the *same* existing synced file with different content on each side. Wait 70 seconds. Confirm: the file's original content survives unchanged on **both** A and B, and each side additionally has a new file named `<name> (衝突自 <partner>，<date>).<ext>` containing the *other* side's version.
5. Run `sync status` on A (root prompt → `sync` → `status`). Confirm it shows the partner (`client-<B's hostname>`), a recent timestamp, and non-zero counts matching what actually happened.
6. Check A's activities log (the `activities` plugin) for `sync` entries describing what happened.

If cross-domain testing is feasible in your environment (two separate domains, both `mode server`, sharing the same `remote-key` file, both with `global bridge <id>` and `global domain <name>` configured — see the `global` plugin's existing manual for the exact setup commands), repeat steps 1-3 across domains instead of client/server, and additionally confirm the tie-break rule: only the domain with the lexicographically smaller name should show `sync` activity log entries initiating the connection; the other domain should only show it *responding* (no outbound `sync` activity for that pair from its side).

- [ ] **Step 5: Commit**

```bash
git add src/plugins/sync.rs src/plugins/mod.rs src/main.rs
git commit -m "$(cat <<'EOF'
新增 SyncPlugin：背景輪詢引擎、狀態指令與註冊

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```
