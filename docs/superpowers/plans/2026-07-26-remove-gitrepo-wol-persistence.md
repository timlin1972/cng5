# Remove gitrepo/wol Directory Persistence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the on-disk directory persistence (`gitrepo/watched.txt`, `wol/devices.txt`) from the `gitrepo` and `wol` plugins — both lists now live purely in memory for the process's lifetime, since the user re-establishes them on every startup via `add` commands in `script-local.cli`.

**Architecture:** Delete the `load_*`/`save_*` file-I/O functions and their backing constants from both plugin files; each plugin's `new()` starts with an empty in-memory collection instead of reading from disk, and `add`/`remove`/`clear` no longer write to disk. No other behavior changes (scanning, wake-on-LAN, panel display, commands all stay identical).

**Tech Stack:** Rust, existing `Plugin` trait — no new dependencies, no new files.

## Global Constraints

- Do not touch `music`/`notepad` plugins' directories — those hold real user content (music files, notes), not a settings list, and are out of scope.
- Do not modify `.gitignore` — the `/gitrepo`/`/wol` entries can stay even though the directories may no longer be created by these plugins.
- No new tests added — neither file has an existing `#[cfg(test)]` module, and this is a pure removal of I/O with no new logic to unit test; verify via `cargo build`/`cargo test` (no regressions) plus a manual `add`/`remove`/`list` smoke check.
- Delete the two existing on-disk directories (`gitrepo/`, `wol/`, including `watched.txt`/`devices.txt`) as part of this change — the user has explicitly confirmed this data doesn't need to be kept.

---

### Task 1: Remove persistence from `gitrepo` and `wol` plugins

**Files:**
- Modify: `src/plugins/gitrepo.rs`
- Modify: `src/plugins/wol.rs`
- Delete: `gitrepo/` (directory, including `gitrepo/watched.txt`)
- Delete: `wol/` (directory, including `wol/devices.txt`)

**Interfaces:**
- No public interfaces change. `GitRepoPlugin::new(SharedContext) -> Self` and `WolPlugin::new(SharedContext) -> Self` keep the exact same signatures; only their internal initial state changes (empty instead of loaded-from-disk).
- Nothing outside these two files references the removed constants/functions (`GITREPO_DIR`, `WATCHED_FILE`, `watched_path`, `load_watched`, `save_watched`, `WOL_DIR`, `DEVICES_FILE`, `devices_path`, `load_devices`, `save_devices` are all private to their respective files).

- [ ] **Step 1: Remove persistence from `src/plugins/gitrepo.rs`**

Change this (currently near the top of the file, right after the `use` statements):

```rust
/// 監控目錄清單存放位置，跟 `NotepadPlugin`/`NOTEPAD_DIR` 一樣的作法：存在程式
/// 執行目錄底下，重啟後不用重新 `add` 一次。
const GITREPO_DIR: &str = "gitrepo";
const WATCHED_FILE: &str = "watched.txt";
```

to: delete these two lines entirely (no replacement).

Change the `MANUAL_TEXT` constant's "注意事項" block from:

```rust
注意事項：
  - scan 進行中再下一次 scan 會被擋掉，等上一輪跑完再重來。
  - add/remove 之後、下一次 scan 完成之前，list 只會顯示「目錄有更動，等待
    scan...」，不顯示舊資料，因為那已經不代表目前這份監控目錄清單了。
  - scan 進行中如果又 add/remove，這一輪 scan 的結果會作廢，等於中途停止。
  - 監控目錄清單會存檔，重開程式不用重新 add。
";
```

to:

```rust
注意事項：
  - scan 進行中再下一次 scan 會被擋掉，等上一輪跑完再重來。
  - add/remove 之後、下一次 scan 完成之前，list 只會顯示「目錄有更動，等待
    scan...」，不顯示舊資料，因為那已經不代表目前這份監控目錄清單了。
  - scan 進行中如果又 add/remove，這一輪 scan 的結果會作廢，等於中途停止。
";
```

Change `GitRepoPlugin::new` from:

```rust
impl GitRepoPlugin {
    pub fn new(_ctx: SharedContext) -> Self {
        // 剛啟動、還沒 scan 過，跟「目錄有更動」是同一種「資料不可信」的狀態，
        // 用同一個 `Stale` 表示，不用另外分兩種訊息。
        Self { watched: Self::load_watched(), scan: Arc::new(Mutex::new(ScanState::Stale)), generation: Arc::new(AtomicU64::new(0)) }
    }
```

to:

```rust
impl GitRepoPlugin {
    pub fn new(_ctx: SharedContext) -> Self {
        // 剛啟動、還沒 scan 過，跟「目錄有更動」是同一種「資料不可信」的狀態，
        // 用同一個 `Stale` 表示，不用另外分兩種訊息。監控目錄清單不做磁碟
        // 持久化——使用者改成把 `add` 指令寫進 `script-local.cli`，每次啟動
        // 都會重新執行，這裡直接從空清單開始就好。
        Self { watched: Vec::new(), scan: Arc::new(Mutex::new(ScanState::Stale)), generation: Arc::new(AtomicU64::new(0)) }
    }
```

Delete these three methods entirely (they currently sit between `mark_stale` and `add`):

```rust
    fn watched_path() -> PathBuf {
        Path::new(GITREPO_DIR).join(WATCHED_FILE)
    }

    fn load_watched() -> Vec<PathBuf> {
        fs::read_to_string(Self::watched_path())
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(PathBuf::from)
            .collect()
    }

    fn save_watched(&self) -> Result<()> {
        fs::create_dir_all(GITREPO_DIR).context("建立 gitrepo 目錄失敗")?;
        let content: String = self.watched.iter().map(|dir| format!("{}\n", dir.display())).collect();
        fs::write(Self::watched_path(), content).context("儲存監控目錄清單失敗")?;
        Ok(())
    }
```

In `add`, remove the `self.save_watched()?;` line — change:

```rust
        self.watched.push(canonical.clone());
        self.save_watched()?;
        self.mark_stale();
```

to:

```rust
        self.watched.push(canonical.clone());
        self.mark_stale();
```

In `remove`, remove the `self.save_watched()?;` line — change:

```rust
        self.watched.retain(|watched_dir| watched_dir != &canonical);
        if self.watched.len() == before {
            bail!("沒有監控這個目錄: {}", display_path(&canonical));
        }
        self.save_watched()?;
        self.mark_stale();
```

to:

```rust
        self.watched.retain(|watched_dir| watched_dir != &canonical);
        if self.watched.len() == before {
            bail!("沒有監控這個目錄: {}", display_path(&canonical));
        }
        self.mark_stale();
```

In `clear`, remove the `self.save_watched()?;` line — change:

```rust
        self.watched.clear();
        self.save_watched()?;
        self.mark_stale();
```

to:

```rust
        self.watched.clear();
        self.mark_stale();
```

`use std::fs;` and `use std::path::{Path, PathBuf};` at the top of the file stay — both are still used elsewhere (`home_dir`/`add`/`remove` use `fs::canonicalize`, `repos_under` uses `fs::read_dir`, `Path`/`PathBuf` are used throughout `expand_tilde`/`display_path`/`is_git_repo`/`repos_under`/the `GitRepoPlugin.watched` field type). Do not remove these imports.

- [ ] **Step 2: Remove persistence from `src/plugins/wol.rs`**

Change this (currently near the top of the file, right after `WOL_PORT`):

```rust
/// 已命名裝置清單存放位置，跟 `GitRepoPlugin`/`GITREPO_DIR` 一樣的作法：存在
/// 程式執行目錄底下，重啟後不用重新 `add` 一次。
const WOL_DIR: &str = "wol";
const DEVICES_FILE: &str = "devices.txt";
```

to: delete these two lines entirely (no replacement).

Change `WolPlugin::new` from:

```rust
impl WolPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx, devices: Self::load_devices() }
    }
```

to:

```rust
impl WolPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        // 命名裝置清單不做磁碟持久化——使用者改成把 `add` 指令寫進
        // `script-local.cli`，每次啟動都會重新執行，這裡直接從空清單開始。
        Self { ctx, devices: HashMap::new() }
    }
```

Delete these three methods entirely (they currently sit between `new` and `add`):

```rust
    fn devices_path() -> PathBuf {
        Path::new(WOL_DIR).join(DEVICES_FILE)
    }

    fn load_devices() -> HashMap<String, String> {
        fs::read_to_string(Self::devices_path())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(name, mac)| (name.to_string(), mac.to_string()))
            .collect()
    }

    fn save_devices(&self) -> Result<()> {
        fs::create_dir_all(WOL_DIR).context("建立 wol 目錄失敗")?;
        let content: String = self.devices.iter().map(|(name, mac)| format!("{name} {mac}\n")).collect();
        fs::write(Self::devices_path(), content).context("儲存裝置清單失敗")?;
        Ok(())
    }
```

In `add`, remove the `self.save_devices()?;` line — change:

```rust
        let existed = self.devices.insert(name.to_string(), mac.to_string()).is_some();
        self.save_devices()?;
        if existed {
```

to:

```rust
        let existed = self.devices.insert(name.to_string(), mac.to_string()).is_some();
        if existed {
```

In `remove`, remove the `self.save_devices()?;` line — change:

```rust
        if self.devices.remove(name).is_none() {
            bail!("沒有這個名字: {name}");
        }
        self.save_devices()?;
        out.push(&format!("已移除: {name}\n"));
```

to:

```rust
        if self.devices.remove(name).is_none() {
            bail!("沒有這個名字: {name}");
        }
        out.push(&format!("已移除: {name}\n"));
```

Remove the now-unused imports at the top of the file — change:

```rust
use std::collections::HashMap;
use std::fs;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
```

to:

```rust
use std::collections::HashMap;
use std::net::UdpSocket;

use anyhow::{bail, Context, Result};
```

(`fs`/`Path`/`PathBuf` were only used by the three deleted functions — nothing else in this file references them. `Context` stays: `parse_mac` still uses `.with_context(...)`.)

- [ ] **Step 3: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -30`
Expected: clean build, no errors, no warnings (specifically: no "unused import" warnings for `wol.rs` — if you see one, you missed removing a reference to `fs`/`Path`/`PathBuf` somewhere, re-check the file).

Run: `cargo test 2>&1 | tail -15`
Expected: all existing tests pass (this task adds no new tests, per the plan's Global Constraints — neither file had any before).

- [ ] **Step 4: Manual smoke test**

Run: `cargo run`, then at the root prompt:

```
gitrepo
add ~
list
exit
wol
add testdevice AA:BB:CC:DD:EE:FF
status
remove testdevice
status
exit
```

Confirm:
1. `gitrepo add ~` succeeds and `list` shows it (with a repo count) — same behavior as before, just not written to any file.
2. `wol add testdevice ...` succeeds, `status` shows it once, and after `remove testdevice`, `status` shows `(還沒有用 add 存過任何裝置)` (or no longer lists it).
3. Confirm no `gitrepo/watched.txt` or `wol/devices.txt` file gets created in the working directory during this session: `ls gitrepo wol 2>&1` should report both paths as "No such file or directory" (they were deleted in Step 5 below and nothing should recreate them).

- [ ] **Step 5: Delete the existing on-disk data**

The user explicitly confirmed this data (a `gitrepo/watched.txt` with two directories, `wol/devices.txt` with one device named `linds`) doesn't need to be kept, since these plugins no longer read or write it.

Run:
```bash
rm -rf gitrepo wol
```

Confirm: `ls gitrepo wol 2>&1` reports both as not existing.

- [ ] **Step 6: Commit**

```bash
git add src/plugins/gitrepo.rs src/plugins/wol.rs
git commit -m "$(cat <<'EOF'
移除 gitrepo/wol plugin 的目錄式設定持久化

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

Note: `gitrepo/` and `wol/` are already in `.gitignore`, so `git status` should show a clean tree after this commit with no trace of the deleted directories (they were never tracked).
