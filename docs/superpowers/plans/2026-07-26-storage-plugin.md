# Storage Plugin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `storage` plugin — a local, single-root, nested-subfolder file manager for this machine's own storage — with CLI commands (`ls`/`cd`/`mkdir`/`rm`/`mv`), a JSON Web API (list/download/upload/mkdir/delete/rename), and a custom browser panel (breadcrumb + table, matching the approved mockup) for upload/download/manage from any browser pointed at this device's web UI.

**Architecture:** A new self-contained module `src/plugins/storage.rs` holds the security-critical path-safety function (`safe_storage_path`) and pure filesystem operations (list/mkdir/remove/rename), fully unit-tested against temp directories, plus the `StoragePlugin` CLI on top of them. `web.rs` gets six new thin HTTP handlers that call the same `storage.rs` functions directly (bypassing the `Shell` lock, matching the existing `music`/`files` web-handler convention). `frontend.html` gets a new special-cased panel (like `player`/`shell`/`notepad`) driving those six endpoints with on-demand fetch (no polling), client-side "current path" state.

**Tech Stack:** Rust (existing `Plugin` trait, `anyhow`, `serde`, `actix-web`, `actix_files::NamedFile`), vanilla JS in the existing single-file frontend. No new dependencies.

## Global Constraints

- Local-only: no cross-device browsing/access (that remains `files` plugin's territory; this plugin doesn't touch it).
- Single root directory: `STORAGE_DIR = "storage"` (relative to program working directory, same convention as `MUSIC_DIR`/`NOTEPAD_DIR`), with arbitrary nested subfolders allowed — unlike `files` plugin, which is flat/single-level only.
- New, independent plugin (`storage`) — do not modify `src/plugins/files.rs` or its behavior.
- Path safety (`safe_storage_path`): reject absolute paths, reject empty strings, reject any path component that isn't `Component::Normal` (this rejects `..`, `.`, root prefixes, etc. in one check, per-platform-correct via `std::path::Component`) — AND additionally canonicalize the deepest existing ancestor of the candidate path and verify it is still under the canonicalized root (defense against symlink-based escapes). Every CLI command and every Web API endpoint must validate through this one function before touching the filesystem.
- Upload conflict rule: same-name file at the destination is overwritten (no prompt, no auto-rename).
- `mkdir` where a file (not a folder) already exists at that name: reject with an error.
- Deleting a non-empty folder without an explicit recursive flag: reject with an error. With the flag: recursive delete allowed.
- `mv`/rename conflict rule: if the destination already exists as a **file**, overwrite it. If the destination already exists as a **folder**, reject with an error (no folder-merge semantics).
- No CLI `upload`/`download` commands — CLI runs on the same machine that owns the storage, so those are Web-UI-only concepts.
- Out of scope (do not build): cross-device storage browsing, multiple named shared roots, storage-space/disk-usage overview, thumbnails/media preview, search, user accounts/permissions.
- Web UI is a custom panel (breadcrumb + table), matching the approved mockup: toolbar with "⬆ 上傳" and "＋ 新資料夾" buttons, table columns name/size/modified-time, per-row download (files only) and delete/rename actions, confirm dialog before deleting a non-empty folder that states the item count.
- No narrow-panel fallback: if the panel is too narrow, let the browser truncate/scroll — no special-cased responsive layout.
- Web API handlers get no dedicated tests, matching this codebase's existing convention (`files_list`/`files_download`/`files_upload`/`music_file_delete` in `web.rs` have none either) — the handlers are thin wrappers; all testable logic lives in `storage.rs`'s pure functions.

---

### Task 1: Path safety and pure filesystem operations

**Files:**
- Create: `src/plugins/storage.rs`
- Modify: `src/plugins/mod.rs` (add `mod storage;` declaration only — no `pub use` yet, `StoragePlugin` doesn't exist until Task 2)

**Interfaces:**
- Produces:
  - `pub(crate) const STORAGE_DIR: &str = "storage";`
  - `pub(crate) struct StorageEntry { pub(crate) name: String, pub(crate) is_dir: bool, pub(crate) size: u64, pub(crate) modified: u64 }` (derives `Serialize, Deserialize, Clone, PartialEq, Debug`)
  - `pub(crate) fn safe_storage_path(root: &Path, relative: &str) -> Option<PathBuf>`
  - `pub(crate) fn list_dir(dir: &Path) -> Result<Vec<StorageEntry>>`
  - `pub(crate) fn make_dir(path: &Path) -> Result<()>`
  - `pub(crate) fn remove(path: &Path, recursive: bool) -> Result<()>`
  - `pub(crate) fn rename_path(from: &Path, to: &Path) -> Result<()>`
  - Task 2's `StoragePlugin` (same file) and Task 3's web handlers (`web.rs`) both call these directly.

- [ ] **Step 1: Write the failing tests**

Create `src/plugins/storage.rs` with only this content (it references functions/types that don't exist yet, so it fails to compile — the expected "red" state):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 每個測試各自用一個獨立命名的暫存資料夾當 root，避免 cargo test 預設多執行緒
    /// 平行跑測試時互相踩到彼此的檔案——不能直接用真正的 `STORAGE_DIR`（那是
    /// 相對於工作目錄的相對路徑，多個測試同時寫入會互相干擾）。先清掉舊的殘留
    /// （上一次測試留下的），再建立乾淨的資料夾。
    fn test_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cng5-storage-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建立測試用暫存根目錄失敗");
        dir
    }

    #[test]
    fn nested_path_under_root_is_accepted() {
        let root = test_root("nested-ok");
        fs::create_dir_all(root.join("photos")).unwrap();
        let result = safe_storage_path(&root, "photos/beach.jpg");
        assert_eq!(result, Some(root.join("photos").join("beach.jpg")));
    }

    #[test]
    fn nonexistent_nested_path_still_accepted_for_new_files() {
        // mkdir/upload 的目的地此刻還不存在很正常（例如要新建的資料夾本身），
        // 只要路徑本身沒有 `..`、父層鏈一路往上追到 root 都合法，就該放行，
        // 不能因為「東西還不存在」就一律拒絕。
        let root = test_root("nonexistent-nested-ok");
        let result = safe_storage_path(&root, "new-folder/new-file.txt");
        assert_eq!(result, Some(root.join("new-folder").join("new-file.txt")));
    }

    #[test]
    fn parent_dir_component_rejected() {
        let root = test_root("parent-dir-rejected");
        assert_eq!(safe_storage_path(&root, "../secrets.txt"), None);
        assert_eq!(safe_storage_path(&root, "photos/../../secrets.txt"), None);
        assert_eq!(safe_storage_path(&root, ".."), None);
    }

    #[test]
    fn absolute_path_rejected() {
        let root = test_root("absolute-rejected");
        assert_eq!(safe_storage_path(&root, "/etc/passwd"), None);
    }

    #[test]
    fn empty_path_rejected() {
        let root = test_root("empty-rejected");
        assert_eq!(safe_storage_path(&root, ""), None);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_root_is_rejected() {
        use std::os::unix::fs::symlink;
        let root = test_root("symlink-escape-rejected");
        let outside = std::env::temp_dir().join("cng5-storage-test-symlink-escape-target");
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"top secret").unwrap();
        symlink(&outside, root.join("escape")).expect("建立測試用 symlink 失敗");
        // 純字串層面「escape/secret.txt」完全合法（沒有 `..`），但 `escape`
        // 這個 symlink 實際指向 root 外面，canonicalize 檢查要抓到這個。
        assert_eq!(safe_storage_path(&root, "escape/secret.txt"), None);
    }

    #[test]
    fn list_dir_reports_files_and_folders_with_dirs_first() {
        let root = test_root("list-dir");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("b.txt"), b"hello").unwrap();
        fs::write(root.join("a.txt"), b"hi").unwrap();
        let entries = list_dir(&root).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["sub", "a.txt", "b.txt"]);
        assert!(entries[0].is_dir);
        assert!(!entries[1].is_dir);
        assert_eq!(entries[2].size, 5); // "hello" 的位元組數
    }

    #[test]
    fn make_dir_creates_new_folder_and_rejects_existing_name() {
        let root = test_root("mkdir");
        make_dir(&root.join("new")).unwrap();
        assert!(root.join("new").is_dir());
        assert!(make_dir(&root.join("new")).is_err());
    }

    #[test]
    fn remove_requires_recursive_flag_for_nonempty_dir() {
        let root = test_root("remove");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/file.txt"), b"x").unwrap();
        assert!(remove(&root.join("sub"), false).is_err());
        assert!(root.join("sub").exists());
        remove(&root.join("sub"), true).unwrap();
        assert!(!root.join("sub").exists());
    }

    #[test]
    fn remove_file_ignores_recursive_flag() {
        let root = test_root("remove-file");
        fs::write(root.join("f.txt"), b"x").unwrap();
        remove(&root.join("f.txt"), false).unwrap();
        assert!(!root.join("f.txt").exists());
    }

    #[test]
    fn rename_overwrites_existing_file_but_rejects_existing_dir_target() {
        let root = test_root("rename");
        fs::write(root.join("old.txt"), b"new content").unwrap();
        fs::write(root.join("existing.txt"), b"will be replaced").unwrap();
        rename_path(&root.join("old.txt"), &root.join("existing.txt")).unwrap();
        assert_eq!(fs::read_to_string(root.join("existing.txt")).unwrap(), "new content");

        fs::write(root.join("another.txt"), b"x").unwrap();
        fs::create_dir_all(root.join("target-dir")).unwrap();
        assert!(rename_path(&root.join("another.txt"), &root.join("target-dir")).is_err());
    }

    #[test]
    fn rename_missing_source_errors() {
        let root = test_root("rename-missing-source");
        assert!(rename_path(&root.join("nope.txt"), &root.join("dest.txt")).is_err());
    }
}
```

Add the module declaration so `cargo test` can find it — in `src/plugins/mod.rs`, add `mod storage;` as a new line, alphabetically between `mod remote_output;` and `mod system;`:

```rust
mod remote_output;
mod storage;
mod system;
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test storage:: 2>&1 | tail -40`
Expected: compile error — `cannot find function 'safe_storage_path'` (and `list_dir`, `make_dir`, `remove`, `rename_path`) in this scope, since none of them exist yet.

- [ ] **Step 3: Implement the path-safety and filesystem functions**

Add this above the `#[cfg(test)]` block in `src/plugins/storage.rs`:

```rust
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// `storage` plugin 管理的檔案都放在這個資料夾底下（相對於程式執行時的工作
/// 目錄），跟 `MUSIC_DIR`/`NOTEPAD_DIR` 同樣的命名慣例。跟那兩個資料夾不同的
/// 是，這個資料夾底下允許任意深度的巢狀子資料夾。
pub(crate) const STORAGE_DIR: &str = "storage";

/// 一個檔案或資料夾的中繼資料，`storage list` 指令跟 `/api/storage/list` 都用
/// 這個格式回傳。
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub(crate) struct StorageEntry {
    pub(crate) name: String,
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
    pub(crate) modified: u64,
}

/// 驗證 `relative`（使用者輸入、或 HTTP 請求帶進來的相對路徑字串）是不是真的
/// 只會落在 `root` 底下，是的話回傳組好的實際路徑，不是就回傳 `None`——不管
/// 呼叫端是 CLI 指令、還是 Web API，都要過這一關才能碰檔案系統，這是唯一的
/// 安全把關點。
///
/// 兩層防護：
/// 1. 逐段檢查 `relative` 的每個路徑片段，只接受 `Component::Normal`（單純的
///    目錄/檔案名稱）——這樣就同時擋掉 `..`、`.`、絕對路徑起始的
///    `RootDir`/`Prefix`，而且用 `std::path::Component` 逐段解析，Windows 的
///    `\` 分隔符也能正確處理，不用自己手動判斷字串裡有沒有斜線。
/// 2. 組出完整路徑後，往上找「目前已經真的存在」的最深一層祖先目錄（candidate
///    本身可能還不存在，例如 mkdir 的目標、上傳/搬移的目的地），對這層祖先做
///    canonicalize，確認它仍然在 `root` 的 canonical 路徑底下——這一步是為了
///    擋掉「路徑字串看起來在 root 裡面、但其中某一段其實是指到 root 外面的
///    symlink」這種繞過純字串檢查的手法。
pub(crate) fn safe_storage_path(root: &Path, relative: &str) -> Option<PathBuf> {
    if relative.is_empty() {
        return None;
    }
    let rel_path = Path::new(relative);
    if rel_path.is_absolute() {
        return None;
    }
    for component in rel_path.components() {
        match component {
            Component::Normal(_) => {}
            _ => return None,
        }
    }
    let candidate = root.join(rel_path);

    let mut existing_ancestor = candidate.as_path();
    while !existing_ancestor.exists() {
        existing_ancestor = existing_ancestor.parent()?;
    }
    let root_canon = fs::canonicalize(root).ok()?;
    let ancestor_canon = fs::canonicalize(existing_ancestor).ok()?;
    if !ancestor_canon.starts_with(&root_canon) {
        return None;
    }
    Some(candidate)
}

/// `dir` 底下的所有項目（檔案跟資料夾都列出來，只列這一層，不遞迴）。資料夾
/// 排在前面、各自再依名稱排序——這是這個功能自己的顯示慣例，不是規格硬性要求，
/// 純粹讓瀏覽時資料夾跟檔案不會混在一起、比較好找。
pub(crate) fn list_dir(dir: &Path) -> Result<Vec<StorageEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("讀取資料夾失敗: {}", dir.display()))? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        entries.push(StorageEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: meta.is_dir(),
            size: meta.len(),
            modified,
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(entries)
}

/// 建立資料夾；已經有同名的檔案或資料夾就報錯，不會覆蓋或合併。
pub(crate) fn make_dir(path: &Path) -> Result<()> {
    if path.exists() {
        bail!("已經有同名的檔案或資料夾: {}", path.display());
    }
    fs::create_dir(path).with_context(|| format!("建立資料夾失敗: {}", path.display()))
}

/// 刪除 `path`：是檔案就直接刪；是資料夾的話，非空且沒有 `recursive` 就報錯
/// 拒絕，避免不小心整個資料夾被刪掉。
pub(crate) fn remove(path: &Path, recursive: bool) -> Result<()> {
    let meta = fs::metadata(path).with_context(|| format!("找不到: {}", path.display()))?;
    if meta.is_dir() {
        let mut children = fs::read_dir(path)?;
        if children.next().is_some() && !recursive {
            bail!("資料夾非空，需要加上 --recursive 才能刪除: {}", path.display());
        }
        fs::remove_dir_all(path).with_context(|| format!("刪除資料夾失敗: {}", path.display()))
    } else {
        fs::remove_file(path).with_context(|| format!("刪除檔案失敗: {}", path.display()))
    }
}

/// 重新命名/搬移 `from` 到 `to`：`to` 已經是檔案就直接覆蓋（`std::fs::rename`
/// 在 Unix 上跟 Windows 上都是「取代目的地」的語意），但 `to` 已經是資料夾就
/// 拒絕——不做資料夾合併，語意含糊，寧可讓使用者自己先處理。
pub(crate) fn rename_path(from: &Path, to: &Path) -> Result<()> {
    if !from.exists() {
        bail!("來源不存在: {}", from.display());
    }
    if to.exists() && to.is_dir() {
        bail!("目的地已經是資料夾，不支援覆蓋資料夾: {}", to.display());
    }
    fs::rename(from, to).with_context(|| format!("搬移/重新命名失敗: {} -> {}", from.display(), to.display()))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test storage:: 2>&1 | tail -40`
Expected: all 11 tests in `plugins::storage::tests` pass (`test result: ok. 11 passed`).

- [ ] **Step 5: Commit**

```bash
git add src/plugins/storage.rs src/plugins/mod.rs
git commit -m "$(cat <<'EOF'
新增 storage plugin 的路徑安全檢查與純檔案系統操作

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: `StoragePlugin` CLI and registration

**Files:**
- Modify: `src/plugins/storage.rs` (add `StoragePlugin` struct, `Plugin` impl, `render_listing`/`format_size` helpers, and tests — above or below the Task 1 content, inside the same file)
- Modify: `src/plugins/mod.rs` (add `pub use storage::StoragePlugin;`)
- Modify: `src/main.rs` (add `StoragePlugin` to the `use plugins::{...}` import list, and a `"storage"` entry to the `factories` vec)

**Interfaces:**
- Consumes: `STORAGE_DIR`, `StorageEntry`, `safe_storage_path`, `list_dir`, `make_dir`, `remove`, `rename_path` from Task 1 (same file, `src/plugins/storage.rs`); `crate::plugin::{Plugin, SharedContext}`; `crate::output::OutputBuffer`.
- Produces: `pub struct StoragePlugin` with `pub fn new(_ctx: SharedContext) -> Self`, registered in `main.rs`'s plugin factory list exactly like every other plugin (see `QrPlugin::new` at `src/main.rs:102` for the pattern).

- [ ] **Step 1: Write the failing tests**

Add this inside a **new** `#[cfg(test)] mod tests { use super::*; ... }` block appended after the Task 1 test module in `src/plugins/storage.rs` — actually, append these test functions **inside the existing** `mod tests { use super::*; ... }` block from Task 1 (same braces, just add more `#[test]` functions before the closing `}`):

```rust
    #[test]
    fn ls_cd_mkdir_rm_mv_round_trip() {
        // 這個測試操作真正的 `StoragePlugin`，但要讓它在一個獨立的暫存目錄
        // 裡運作，不能真的去動相對於工作目錄的 `storage/`（那會跟其他平行
        // 跑的測試/正式使用互相干擾）——用 `std::env::set_current_dir` 切
        // 到一個乾淨的暫存目錄，plugin 內部用的相對路徑 `STORAGE_DIR` 就會
        // 落在這個暫存目錄底下。`cargo test` 預設多執行緒平行跑測試，改變
        // 工作目錄是 process 全域的狀態，所以這個測試不能跟其他也會
        // `set_current_dir` 的測試同時執行；這個專案目前沒有其他測試會動
        // 工作目錄，所以先接受這個限制，不用額外做執行緒鎖。
        let workdir = std::env::temp_dir().join("cng5-storage-plugin-test-workdir");
        let _ = fs::remove_dir_all(&workdir);
        fs::create_dir_all(&workdir).unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(&workdir).unwrap();

        let ctx: SharedContext = std::sync::Arc::new(std::sync::Mutex::new(crate::plugin::ContextInner::default()));
        let mut plugin = StoragePlugin::new(ctx);
        let out = OutputBuffer::new();

        plugin.dispatch("mkdir", &["photos".to_string()], &out).unwrap();
        plugin.dispatch("cd", &["photos".to_string()], &out).unwrap();
        plugin.dispatch("mkdir", &["2026".to_string()], &out).unwrap();
        plugin.dispatch("cd", &["2026".to_string()], &out).unwrap();

        fs::write(Path::new(STORAGE_DIR).join("photos/2026/beach.jpg"), b"fake jpg bytes").unwrap();
        let listing = plugin.panel_text().unwrap();
        assert!(listing.contains("/photos/2026"));
        assert!(listing.contains("beach.jpg"));

        plugin.dispatch("cd", &["..".to_string()], &out).unwrap();
        let listing = plugin.panel_text().unwrap();
        assert!(listing.contains("/photos"));
        assert!(listing.contains("2026"));

        plugin.dispatch("mv", &["2026".to_string(), "2026-renamed".to_string()], &out).unwrap();
        assert!(Path::new(STORAGE_DIR).join("photos/2026-renamed/beach.jpg").exists());

        let err = plugin.dispatch("rm", &["2026-renamed".to_string()], &out).unwrap_err();
        assert!(err.to_string().contains("--recursive"));
        plugin.dispatch("rm", &["2026-renamed".to_string(), "--recursive".to_string()], &out).unwrap();
        assert!(!Path::new(STORAGE_DIR).join("photos/2026-renamed").exists());

        std::env::set_current_dir(original).unwrap();
        let _ = fs::remove_dir_all(&workdir);
    }

    #[test]
    fn dispatch_unknown_command_errors() {
        let ctx: SharedContext = std::sync::Arc::new(std::sync::Mutex::new(crate::plugin::ContextInner::default()));
        let workdir = std::env::temp_dir().join("cng5-storage-plugin-test-unknown-cmd");
        let _ = fs::remove_dir_all(&workdir);
        fs::create_dir_all(&workdir).unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(&workdir).unwrap();

        let mut plugin = StoragePlugin::new(ctx);
        let out = OutputBuffer::new();
        let err = plugin.dispatch("frobnicate", &[], &out).unwrap_err();
        assert!(err.to_string().contains("storage 不認得指令"));

        std::env::set_current_dir(original).unwrap();
        let _ = fs::remove_dir_all(&workdir);
    }

    #[test]
    fn commands_list_matches_documented_syntax() {
        let ctx: SharedContext = std::sync::Arc::new(std::sync::Mutex::new(crate::plugin::ContextInner::default()));
        let plugin = StoragePlugin::new(ctx);
        assert_eq!(
            plugin.commands(),
            &["ls [path]", "cd <path>", "mkdir <name>", "rm <name> [--recursive]", "mv <old> <new>"]
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test storage:: 2>&1 | tail -40`
Expected: compile error — `cannot find type 'StoragePlugin'` / `cannot find type 'SharedContext'` in this scope (the struct and its `use` statements are added together in Step 3).

- [ ] **Step 3: Implement `StoragePlugin`**

Add these `use` statements at the very top of `src/plugins/storage.rs` (before the `STORAGE_DIR` const from Task 1):

```rust
use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};
```

Add this after the Task 1 functions (still before the `#[cfg(test)]` block):

```rust
const MANUAL_TEXT: &str = "\
storage：管理本機這台裝置的儲存（不是跨裝置操作，那是 files plugin 的範圍）。
根目錄底下可以有任意深度的子資料夾。

指令：
  ls [path]              列出目前位置（或指定 path，不會改變目前位置）底下的內容
  cd <path>              切換目前位置；cd .. 回上一層、cd / 回根目錄
  mkdir <name>           在目前位置建立子資料夾
  rm <name> [--recursive] 刪除檔案；刪非空資料夾要加 --recursive，不然會報錯拒絕
  mv <old> <new>         重新命名，或搬到目前位置下的另一個名稱；目的地是檔案就覆蓋，
                         是資料夾就拒絕

沒有 upload/download 指令：這個 plugin 執行的機器本身就是儲存所在地，直接用作業系統
的檔案總管操作 storage/ 資料夾即可；上傳/下載是給瀏覽器（Web UI）用的。

cd .. 只支援單獨這樣打，不支援像 sub/.. 這種複合寫法。
";

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[0])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

fn render_listing(dir: &Path) -> Result<String> {
    let entries = list_dir(dir)?;
    let relative = dir.strip_prefix(STORAGE_DIR).unwrap_or(Path::new(""));
    let mut text = format!("/{}\n", relative.display());
    if entries.is_empty() {
        text.push_str("(空)\n");
        return Ok(text);
    }
    for entry in &entries {
        let kind = if entry.is_dir { "📁" } else { "📄" };
        let size = if entry.is_dir { "—".to_string() } else { format_size(entry.size) };
        text.push_str(&format!("{kind} {:<30} {:>8}\n", entry.name, size));
    }
    Ok(text)
}

/// 記住「目前瀏覽到哪個子路徑」，相對 `STORAGE_DIR` 根目錄；空路徑代表在
/// 根目錄。沒有其他狀態——每個指令都直接讀寫真正的檔案系統，不快取任何內容。
pub struct StoragePlugin {
    current: PathBuf,
}

impl StoragePlugin {
    pub fn new(_ctx: SharedContext) -> Self {
        let _ = fs::create_dir_all(STORAGE_DIR);
        Self { current: PathBuf::new() }
    }

    /// 目前位置對應的真正檔案系統路徑（`STORAGE_DIR` 加上 `self.current`）。
    fn current_real_path(&self) -> PathBuf {
        if self.current.as_os_str().is_empty() {
            Path::new(STORAGE_DIR).to_path_buf()
        } else {
            Path::new(STORAGE_DIR).join(&self.current)
        }
    }

    /// 把一個 CLI 參數（可能是普通名稱、也可能是 `..` 或 `/`）解讀成相對目前
    /// 位置的真正檔案系統路徑，並且驗證過 `safe_storage_path`。`..`/`/` 是
    /// 這個函式自己特殊處理的兩個字面值，不會被當成一般路徑片段送進
    /// `safe_storage_path`（那邊會拒絕任何 `..` 片段）——這樣「回上一層」的
    /// 導覽功能，跟「拒絕路徑裡帶 `..`」的安全檢查不會互相衝突。
    fn resolve_relative(&self, arg: &str) -> Result<PathBuf> {
        let combined_relative: String = if arg == "/" {
            String::new()
        } else if arg == ".." {
            self.current.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
        } else if self.current.as_os_str().is_empty() {
            arg.to_string()
        } else {
            format!("{}/{}", self.current.display(), arg)
        };
        if combined_relative.is_empty() {
            return Ok(Path::new(STORAGE_DIR).to_path_buf());
        }
        safe_storage_path(Path::new(STORAGE_DIR), &combined_relative)
            .with_context(|| format!("不合法的路徑: {arg}"))
    }
}

impl Plugin for StoragePlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["ls [path]", "cd <path>", "mkdir <name>", "rm <name> [--recursive]", "mv <old> <new>"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "ls" => {
                let target = match args.first() {
                    Some(a) => self.resolve_relative(a)?,
                    None => self.current_real_path(),
                };
                out.push(&render_listing(&target)?);
                Ok(())
            }
            "cd" => {
                let arg = args.first().context("cd 需要接路徑")?;
                let target = self.resolve_relative(arg)?;
                if !target.is_dir() {
                    bail!("不是資料夾: {arg}");
                }
                self.current = target.strip_prefix(STORAGE_DIR).unwrap_or(Path::new("")).to_path_buf();
                out.push(&format!("目前位置: /{}\n", self.current.display()));
                Ok(())
            }
            "mkdir" => {
                let name = args.first().context("mkdir 需要接資料夾名稱")?;
                let target = self.resolve_relative(name)?;
                make_dir(&target)?;
                out.push(&format!("已建立: {name}\n"));
                Ok(())
            }
            "rm" => {
                let name = args.first().context("rm 需要接檔案或資料夾名稱")?;
                let recursive = args.iter().any(|a| a == "--recursive");
                let target = self.resolve_relative(name)?;
                remove(&target, recursive)?;
                out.push(&format!("已刪除: {name}\n"));
                Ok(())
            }
            "mv" => {
                let old = args.first().context("mv 需要接來源名稱")?;
                let new = args.get(1).context("mv 需要接目的地名稱")?;
                let from = self.resolve_relative(old)?;
                let to = self.resolve_relative(new)?;
                rename_path(&from, &to)?;
                out.push(&format!("已搬移: {old} -> {new}\n"));
                Ok(())
            }
            other => bail!("storage 不認得指令: {other}"),
        }
    }

    fn panel_text(&self) -> Option<String> {
        render_listing(&self.current_real_path()).ok()
    }

    fn manual_text(&self) -> &'static str {
        MANUAL_TEXT
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
```

Also add `use anyhow::{bail, Context, Result};` — check the top of the file already has `use anyhow::{bail, Context, Result};` from Task 1's implementation; if so, do not duplicate it, just confirm it's already imported (Task 1's Step 3 code already added this exact line).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test storage:: 2>&1 | tail -50`
Expected: all 14 tests in `plugins::storage::tests` pass (11 from Task 1 + 3 new from Task 2 — `test result: ok. 14 passed`).

- [ ] **Step 5: Register the plugin**

In `src/plugins/mod.rs`, add the export next to the other `pub use` lines (alphabetical order — `storage` goes right before `system`):

```rust
pub use remote_output::RemoteOutputPlugin;
pub use storage::StoragePlugin;
pub(crate) use system::REPORT_INTERVAL;
```

In `src/main.rs`, add `StoragePlugin` to the `use plugins::{...}` import list:

```rust
use plugins::{
    ActivitiesPlugin, ClockPlugin, DevicePlugin, FilesPlugin, GitRepoPlugin, GlobalPlugin, MusicPlugin,
    NotepadPlugin, OutputPlugin, QrPlugin, RemoteOutputPlugin, RemotePlugin, StoragePlugin, SystemPlugin,
    WeatherPlugin, WolPlugin,
};
```

Add a factory entry to the `factories` vec in `src/main.rs` (right after the `"remote-output"` entry, alphabetically close to where it'll actually sit isn't required — putting it anywhere in the vec works functionally; placing it after `"remote-output"` and before `"system"` keeps it near alphabetically-similar neighbors):

```rust
        (
            "remote-output",
            Box::new(|ctx| Box::new(RemoteOutputPlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "storage",
            Box::new(|ctx| Box::new(StoragePlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "system",
```

- [ ] **Step 6: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -30`
Expected: builds with no errors and no new warnings.

Run: `cargo test 2>&1 | tail -20`
Expected: all tests pass, including the 14 new `plugins::storage::tests::*`.

- [ ] **Step 7: Commit**

```bash
git add src/plugins/storage.rs src/plugins/mod.rs src/main.rs
git commit -m "$(cat <<'EOF'
新增 storage plugin 的 CLI 指令與註冊

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Web API endpoints

**Files:**
- Modify: `src/plugins/mod.rs` (re-export `list_dir`, `make_dir`, `remove`, `rename_path`, `safe_storage_path`, `STORAGE_DIR` as `pub(crate)` so `web.rs` can use them)
- Modify: `src/web.rs` (add six handler functions + query-param structs, and six route registrations)

**Interfaces:**
- Consumes: `STORAGE_DIR`, `safe_storage_path`, `list_dir`, `make_dir`, `remove`, `rename_path` from Task 1 (`src/plugins/storage.rs`, re-exported via `src/plugins/mod.rs`).
- Produces: six HTTP endpoints under `/api/storage/...` (see Global Constraints for the exact list) that Task 4's frontend calls.

- [ ] **Step 1: Add the re-exports**

In `src/plugins/mod.rs`, add this line next to the other `pub(crate) use` lines (anywhere in that group is fine, e.g. right after the `files` re-export line):

```rust
pub(crate) use storage::{list_dir, make_dir, remove, rename_path, safe_storage_path, STORAGE_DIR};
```

- [ ] **Step 2: Add the query-param structs and handler functions**

In `src/web.rs`, add `list_dir, make_dir, remove, rename_path, safe_storage_path, STORAGE_DIR` to the existing `use crate::plugins::{...}` line (currently `use crate::plugins::{safe_file_path, ALLOWED_FOLDERS, DEFAULT_NOTEPAD_FILE, MUSIC_DIR, NOTEPAD_DIR, SUBTITLE_LANG_PRIORITY};`), making it:

```rust
use crate::plugins::{
    list_dir, make_dir, remove, rename_path, safe_file_path, safe_storage_path, ALLOWED_FOLDERS,
    DEFAULT_NOTEPAD_FILE, MUSIC_DIR, NOTEPAD_DIR, STORAGE_DIR, SUBTITLE_LANG_PRIORITY,
};
```

Add these handler functions anywhere in `src/web.rs` after the existing `files_upload` function (they're independent of it, placement elsewhere in the file works too, but keeping new file-ish endpoints near the existing `/api/files/...` handlers keeps related code together):

```rust
/// `GET /api/storage/list?path=<相對路徑>`：列出該路徑底下的項目。`path` 是
/// 空字串時代表根目錄本身（`safe_storage_path` 一律拒絕空字串，因為那對「檔案
/// 名稱」來說沒有意義，但對「要不要列根目錄」這個情境需要特別放行，所以這裡
/// 直接特判，不透過 `safe_storage_path`）。
#[derive(Deserialize)]
struct StoragePathQuery {
    path: String,
}

async fn storage_list(query: web::Query<StoragePathQuery>) -> HttpResponse {
    let target = if query.path.is_empty() {
        Some(PathBuf::from(STORAGE_DIR))
    } else {
        safe_storage_path(Path::new(STORAGE_DIR), &query.path)
    };
    let Some(target) = target else {
        return HttpResponse::BadRequest().finish();
    };
    match list_dir(&target) {
        Ok(entries) => HttpResponse::Ok().json(entries),
        Err(_) => HttpResponse::NotFound().finish(),
    }
}

/// `GET /api/storage/download?path=<相對檔案路徑>`：下載單一檔案。用
/// `actix_files::NamedFile` 是因為它會自動處理 `Range` 請求，跟現有
/// `files_download`/`music_file_audio` 同樣的理由——大檔案/影片也能拖拉進度。
async fn storage_download(query: web::Query<StoragePathQuery>, req: HttpRequest) -> HttpResponse {
    let Some(file_path) = safe_storage_path(Path::new(STORAGE_DIR), &query.path) else {
        return HttpResponse::BadRequest().finish();
    };
    match actix_files::NamedFile::open(&file_path) {
        Ok(file) => file.into_response(&req),
        Err(_) => HttpResponse::NotFound().finish(),
    }
}

/// `POST /api/storage/upload?path=<目的地相對路徑，含檔名>`：把 request body
/// 的原始位元組整個寫成一個檔案，同名直接覆蓋。目的地的上層資料夾必須已經
/// 存在（不會自動建立中間目錄——要先用 `storage/mkdir` 建立），這跟真實 NAS
/// 產品「先建資料夾、才能上傳進去」的使用順序一致。
async fn storage_upload(query: web::Query<StoragePathQuery>, body: web::Bytes) -> HttpResponse {
    let Some(file_path) = safe_storage_path(Path::new(STORAGE_DIR), &query.path) else {
        return HttpResponse::BadRequest().finish();
    };
    let Some(parent) = file_path.parent() else {
        return HttpResponse::BadRequest().finish();
    };
    if !parent.is_dir() {
        return HttpResponse::BadRequest().finish();
    }
    match std::fs::write(&file_path, &body) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}

/// `POST /api/storage/mkdir?path=<相對路徑>`：建立資料夾。
async fn storage_mkdir(query: web::Query<StoragePathQuery>) -> HttpResponse {
    let Some(dir_path) = safe_storage_path(Path::new(STORAGE_DIR), &query.path) else {
        return HttpResponse::BadRequest().finish();
    };
    match make_dir(&dir_path) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(_) => HttpResponse::BadRequest().finish(),
    }
}

/// `POST /api/storage/delete?path=<相對路徑>&recursive=<bool>`：刪除檔案/資料夾。
#[derive(Deserialize)]
struct StorageDeleteQuery {
    path: String,
    #[serde(default)]
    recursive: bool,
}

async fn storage_delete(query: web::Query<StorageDeleteQuery>) -> HttpResponse {
    let Some(target) = safe_storage_path(Path::new(STORAGE_DIR), &query.path) else {
        return HttpResponse::BadRequest().finish();
    };
    match remove(&target, query.recursive) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(_) => HttpResponse::BadRequest().finish(),
    }
}

/// `POST /api/storage/rename?from=<相對路徑>&to=<相對路徑>`：重新命名/搬移。
#[derive(Deserialize)]
struct StorageRenameQuery {
    from: String,
    to: String,
}

async fn storage_rename(query: web::Query<StorageRenameQuery>) -> HttpResponse {
    let (Some(from), Some(to)) = (
        safe_storage_path(Path::new(STORAGE_DIR), &query.from),
        safe_storage_path(Path::new(STORAGE_DIR), &query.to),
    ) else {
        return HttpResponse::BadRequest().finish();
    };
    match rename_path(&from, &to) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(_) => HttpResponse::BadRequest().finish(),
    }
}
```

- [ ] **Step 3: Register the routes**

In `src/web.rs`, add six routes to the `App::new()` chain right after the existing `.route("/api/files/{folder}/{name}", web::post().to(files_upload))` line:

```rust
            .route("/api/files/{folder}/{name}", web::post().to(files_upload))
            .route("/api/storage/list", web::get().to(storage_list))
            .route("/api/storage/download", web::get().to(storage_download))
            .route("/api/storage/upload", web::post().to(storage_upload))
            .route("/api/storage/mkdir", web::post().to(storage_mkdir))
            .route("/api/storage/delete", web::post().to(storage_delete))
            .route("/api/storage/rename", web::post().to(storage_rename))
```

- [ ] **Step 4: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -30`
Expected: builds with no errors and no new warnings (there are no new automated tests in this task — see Global Constraints on why web handlers aren't unit-tested in this codebase).

Run: `cargo test 2>&1 | tail -20`
Expected: all existing tests still pass (this task adds no new `#[test]` functions).

- [ ] **Step 5: Manual smoke test**

Run: `cargo run` (starts the web server on port 9759 alongside the CLI/GUI).

From another terminal on the same machine, exercise the new endpoints with `curl` (adjust if `curl` isn't available in your environment — any HTTP client works):

```bash
curl -s "http://localhost:9759/api/storage/list?path=" ; echo
curl -s -X POST "http://localhost:9759/api/storage/mkdir?path=testdir"
curl -s "http://localhost:9759/api/storage/list?path=" ; echo
curl -s -X POST --data-binary "hello world" "http://localhost:9759/api/storage/upload?path=testdir/hello.txt"
curl -s "http://localhost:9759/api/storage/list?path=testdir" ; echo
curl -s "http://localhost:9759/api/storage/download?path=testdir/hello.txt" ; echo
curl -s -X POST "http://localhost:9759/api/storage/rename?from=testdir/hello.txt&to=testdir/renamed.txt"
curl -s -X POST "http://localhost:9759/api/storage/delete?path=testdir&recursive=true"
curl -s "http://localhost:9759/api/storage/list?path=" ; echo
```

Confirm: `mkdir` makes `testdir` appear in the root listing; `upload` then listing `testdir` shows `hello.txt` with size 11; `download` returns `hello world`; after `rename` the file is gone and `renamed.txt` exists (you can re-list to confirm if you want extra certainty); after the recursive `delete`, the root listing no longer shows `testdir`.

- [ ] **Step 6: Commit**

```bash
git add src/plugins/mod.rs src/web.rs
git commit -m "$(cat <<'EOF'
新增 storage plugin 的 Web API 端點

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Web UI panel

**Files:**
- Modify: `src/web/frontend.html`

**Interfaces:**
- Consumes: the six `/api/storage/...` endpoints from Task 3. `GET /api/storage/list?path=<p>` returns a JSON array of `{name: string, is_dir: bool, size: number, modified: number}` objects (matching `StorageEntry`'s serde output).

- [ ] **Step 1: Add the CSS**

In `src/web/frontend.html`, insert this new block of CSS right before the `</style>` at line 579 (i.e., immediately after the existing `.notepad-view tbody tr:not(:first-child) td, .notepad-preview tbody tr:not(:first-child) td { border-top: 1px solid #262a33; }` rule that currently ends the block):

```css
  .storage-body {
    display: flex;
    flex-direction: column;
    height: 100%;
    overflow: hidden;
  }
  .storage-toolbar {
    flex: none;
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 10px;
    border-bottom: 1px solid #3a4150;
  }
  .storage-breadcrumb { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .storage-breadcrumb a { color: #6f9dff; text-decoration: none; margin-right: 2px; }
  .storage-breadcrumb a:hover { text-decoration: underline; }
  .storage-toolbar button {
    background: #232833;
    color: #d8dee9;
    border: 1px solid #3a4150;
    border-radius: 4px;
    padding: 3px 10px;
    cursor: pointer;
    font: inherit;
    font-size: 12px;
  }
  .storage-toolbar button:hover { background: #2b3140; }
  .storage-table {
    flex: 1;
    overflow: auto;
    border-collapse: collapse;
    width: 100%;
    font-size: 12px;
  }
  .storage-table td { padding: 4px 10px; border-top: 1px solid #262a33; white-space: nowrap; }
  .storage-table a { color: #6f9dff; text-decoration: none; }
  .storage-table a:hover { text-decoration: underline; }
  .storage-table button {
    background: none;
    border: none;
    color: #9aa4b8;
    cursor: pointer;
    font-size: 12px;
    margin-left: 6px;
  }
  .storage-table button:hover { color: #d8dee9; }
```

- [ ] **Step 2: Declare `storageUi` alongside the other special-panel state variables**

In `src/web/frontend.html`, find this block (inside `openPanel`, currently reads):

```js
    let body;
    let musicUi = null;
    let shellUi = null;
    let notepadUi = null;
```

Change it to:

```js
    let body;
    let musicUi = null;
    let shellUi = null;
    let notepadUi = null;
    let storageUi = null;
```

- [ ] **Step 3: Add the `storage` branch to `openPanel`**

Find this exact code (the end of the `notepad` branch, immediately followed by the generic default branch):

```js
      notepadUi = { container };
    } else {
      body = document.createElement("pre");
      body.className = "panel-body";
      body.textContent = "(等待資料...)";
    }
```

Change it to (inserting a new `else if` branch between the two):

```js
      notepadUi = { container };
    } else if (name === "storage") {
      panel.style.width = "560px";
      panel.style.height = "420px";

      const container = document.createElement("div");
      container.className = "storage-body";

      const toolbar = document.createElement("div");
      toolbar.className = "storage-toolbar";
      const breadcrumb = document.createElement("span");
      breadcrumb.className = "storage-breadcrumb";
      const uploadBtn = document.createElement("button");
      uploadBtn.textContent = "⬆ 上傳";
      const uploadInput = document.createElement("input");
      uploadInput.type = "file";
      uploadInput.style.display = "none";
      const mkdirBtn = document.createElement("button");
      mkdirBtn.textContent = "＋ 新資料夾";
      toolbar.appendChild(breadcrumb);
      toolbar.appendChild(uploadBtn);
      toolbar.appendChild(uploadInput);
      toolbar.appendChild(mkdirBtn);

      const table = document.createElement("table");
      table.className = "storage-table";
      const tbody = document.createElement("tbody");
      table.appendChild(tbody);

      container.appendChild(toolbar);
      container.appendChild(table);

      const state = { currentPath: "" };

      const joinPath = (base, name2) => (base ? `${base}/${name2}` : name2);

      function formatSize(bytes) {
        if (bytes === 0) return "—";
        const units = ["B", "K", "M", "G", "T"];
        let value = bytes;
        let unit = 0;
        while (value >= 1024 && unit < units.length - 1) {
          value /= 1024;
          unit++;
        }
        return unit === 0 ? `${bytes}${units[0]}` : `${value.toFixed(1)}${units[unit]}`;
      }

      function renderBreadcrumb() {
        breadcrumb.innerHTML = "";
        const rootSeg = document.createElement("a");
        rootSeg.href = "#";
        rootSeg.textContent = "/";
        rootSeg.addEventListener("click", (e) => {
          e.preventDefault();
          navigate("");
        });
        breadcrumb.appendChild(rootSeg);
        if (!state.currentPath) return;
        const parts = state.currentPath.split("/");
        let acc = "";
        for (const part of parts) {
          acc = acc ? `${acc}/${part}` : part;
          const seg = document.createElement("a");
          seg.href = "#";
          seg.textContent = ` ${part}`;
          const target = acc;
          seg.addEventListener("click", (e) => {
            e.preventDefault();
            navigate(target);
          });
          breadcrumb.appendChild(seg);
        }
      }

      async function refresh() {
        const path = state.currentPath;
        let entries;
        try {
          const res = await fetch(`/api/storage/list?path=${encodeURIComponent(path)}`);
          entries = await res.json();
        } catch (_err) {
          return; // 這次沒抓到就算了，使用者可以再手動切換一次路徑重試。
        }
        if (path !== state.currentPath) return; // 等待期間使用者又切了路徑，這次回應過期了，不要蓋掉新畫面。
        renderBreadcrumb();
        tbody.innerHTML = "";
        for (const entry of entries) {
          const row = document.createElement("tr");
          const nameCell = document.createElement("td");
          if (entry.is_dir) {
            const link = document.createElement("a");
            link.href = "#";
            link.textContent = `📁 ${entry.name}`;
            link.addEventListener("click", (e) => {
              e.preventDefault();
              navigate(joinPath(state.currentPath, entry.name));
            });
            nameCell.appendChild(link);
          } else {
            nameCell.textContent = `📄 ${entry.name}`;
          }
          const sizeCell = document.createElement("td");
          sizeCell.textContent = entry.is_dir ? "—" : formatSize(entry.size);
          const actionsCell = document.createElement("td");
          if (!entry.is_dir) {
            const dlBtn = document.createElement("a");
            dlBtn.href = `/api/storage/download?path=${encodeURIComponent(joinPath(state.currentPath, entry.name))}`;
            dlBtn.textContent = "⬇";
            actionsCell.appendChild(dlBtn);
          }
          const delBtn = document.createElement("button");
          delBtn.textContent = "🗑";
          delBtn.addEventListener("click", async () => {
            const full = joinPath(state.currentPath, entry.name);
            if (entry.is_dir) {
              if (!window.confirm(`確定要刪除「${entry.name}」（資料夾）嗎？裡面的內容會一併刪除`)) return;
              await fetch(`/api/storage/delete?path=${encodeURIComponent(full)}&recursive=true`, { method: "POST" });
            } else {
              if (!window.confirm(`確定要刪除「${entry.name}」嗎？`)) return;
              await fetch(`/api/storage/delete?path=${encodeURIComponent(full)}`, { method: "POST" });
            }
            refresh();
          });
          const renameBtn = document.createElement("button");
          renameBtn.textContent = "✎";
          renameBtn.addEventListener("click", async () => {
            const newName = window.prompt("新名稱", entry.name);
            if (!newName || newName === entry.name) return;
            const from = joinPath(state.currentPath, entry.name);
            const to = joinPath(state.currentPath, newName);
            await fetch(`/api/storage/rename?from=${encodeURIComponent(from)}&to=${encodeURIComponent(to)}`, {
              method: "POST",
            });
            refresh();
          });
          actionsCell.appendChild(delBtn);
          actionsCell.appendChild(renameBtn);
          row.appendChild(nameCell);
          row.appendChild(sizeCell);
          row.appendChild(actionsCell);
          tbody.appendChild(row);
        }
      }

      function navigate(path) {
        state.currentPath = path;
        refresh();
      }

      uploadBtn.addEventListener("click", () => uploadInput.click());
      uploadInput.addEventListener("change", async () => {
        const file = uploadInput.files[0];
        if (!file) return;
        const dest = joinPath(state.currentPath, file.name);
        await fetch(`/api/storage/upload?path=${encodeURIComponent(dest)}`, { method: "POST", body: file });
        uploadInput.value = "";
        refresh();
      });

      mkdirBtn.addEventListener("click", async () => {
        const dirName = window.prompt("新資料夾名稱");
        if (!dirName) return;
        const dest = joinPath(state.currentPath, dirName);
        await fetch(`/api/storage/mkdir?path=${encodeURIComponent(dest)}`, { method: "POST" });
        refresh();
      });

      refresh();

      storageUi = { container };
    } else {
      body = document.createElement("pre");
      body.className = "panel-body";
      body.textContent = "(等待資料...)";
    }
```

- [ ] **Step 4: Wire `storageUi` into the panel's DOM attachment, SSE guard, and open-panel registry**

Find this line:

```js
    panel.appendChild(musicUi ? musicUi.container : shellUi ? shellUi.container : notepadUi ? notepadUi.container : body);
```

Change it to:

```js
    panel.appendChild(
      musicUi
        ? musicUi.container
        : shellUi
        ? shellUi.container
        : notepadUi
        ? notepadUi.container
        : storageUi
        ? storageUi.container
        : body
    );
```

Find this line (the SSE-subscription exclusion guard):

```js
    if (name !== "player" && name !== "shell" && name !== "notepad") {
```

Change it to:

```js
    if (name !== "player" && name !== "shell" && name !== "notepad" && name !== "storage") {
```

Find this line (the final registration into the `open` Map):

```js
    open.set(name, { el: panel, es, musicUi, shellUi, notepadUi });
```

Change it to:

```js
    open.set(name, { el: panel, es, musicUi, shellUi, notepadUi, storageUi });
```

- [ ] **Step 5: Build and manually verify in the browser**

Run: `cargo build 2>&1 | tail -20`
Expected: builds with no errors (this task only touches the HTML/JS string constant `FRONTEND_HTML`, so a successful build just confirms the Rust file still parses as a valid string literal — it does not validate the JS itself).

Run: `cargo run`, then open `http://localhost:9759/` in a browser. Open the menu and click "storage" (it will have the generic 🪟 icon, listed alphabetically among the other plugins — see Task 2's registration).

Confirm:
1. The panel opens showing an empty (or "(空)"-equivalent, i.e. just no rows) table with breadcrumb `/`.
2. Click "＋ 新資料夾", type a name, confirm it appears as a row and you can click into it (breadcrumb updates, table now shows that folder's — empty — contents).
3. Click "⬆ 上傳", pick a small file from your computer — confirm it appears in the table with the right size, and clicking the "⬇" downloads it back correctly (byte-identical, spot-check by opening it).
4. Click "✎" on a file, rename it — confirm the row updates.
5. Click "🗑" on the folder created in step 2 (non-empty by now, since a file's in it) — confirm the browser's confirm dialog appears, and confirming removes the folder and its contents.
6. Click the `/` breadcrumb segment to return to root — confirm it shows root contents correctly.

- [ ] **Step 6: Commit**

```bash
git add src/web/frontend.html
git commit -m "$(cat <<'EOF'
新增 storage plugin 的 Web UI panel

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```
