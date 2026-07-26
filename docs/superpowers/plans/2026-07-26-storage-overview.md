# Storage Overview Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show each device's disk usage (free/total space on the filesystem containing the program's working directory) in the existing `device list` (same-domain) and `global list` (cross-domain) tables, by extending the existing device-report/aggregation mechanism — no new plugin, no new transport.

**Architecture:** A new `sysinfo::disk_usage(path: &Path) -> Option<(u64, u64)>` (free, total bytes) queries the OS (shell out to `df -k` on Unix, `GetDiskFreeSpaceExW` FFI on Windows, matching this file's existing platform-split style), plus a shared `sysinfo::format_disk_usage(free: u64, total: u64) -> String` for the compact `<free>/<total>` display string (mirroring how `sysinfo::format_uptime` is already shared by both `device.rs` and `global.rs`). `DeviceReport` gets two new `u64` fields carried automatically by the existing HTTP/MQTT report-and-aggregate machinery; `device.rs`/`global.rs`'s fixed-width tables grow from 9 to 10 columns.

**Tech Stack:** Rust, existing `sysinfo.rs` platform-FFI/shell-out conventions, `serde` (already a dependency) — no new dependencies.

## Global Constraints

- Disk usage is measured on the filesystem containing the program's working directory (where `storage/`/`music/`/etc. actually live), not a hardcoded `/` or system drive.
- Query failure (command fails, FFI call fails) must degrade to `None` → callers store `(0, 0)` in `DeviceReport` → display shows `N/A` — never let this new data cause an existing device report to fail to parse or display (matches the existing `os`/`version` `#[serde(default)]` precedent).
- Display format is compact: `<free>/<total>` (e.g. `302G/512G`), no percentage, no decimals — keeps the already-wide 9-column tables from growing too wide on narrow terminals.
- Out of scope: `storage/`-folder-specific usage (recursive file-size summing), a dedicated new plugin/panel, historical trend charts, capacity alerts. Do not build any of these.
- No new dependencies, no changes to the HTTP report endpoint (`/api/device/register`) or the MQTT cross-domain relay — the two new `DeviceReport` fields ride the existing mechanisms unchanged.

---

### Task 1: `sysinfo::disk_usage` and `sysinfo::format_disk_usage`

**Files:**
- Modify: `src/sysinfo.rs`

**Interfaces:**
- Produces:
  - `pub fn disk_usage(path: &Path) -> Option<(u64, u64)>` — returns `(free_bytes, total_bytes)` for the filesystem containing `path`; `None` on any failure.
  - `pub fn format_disk_usage(free: u64, total: u64) -> String` — `"<free>/<total>"` compact string (e.g. `"302G/512G"`); `"N/A"` when `total == 0`.
- Consumed by: Task 2's `system.rs` (`disk_usage`) and `device.rs`/`global.rs` (`format_disk_usage`).

- [ ] **Step 1: Write the failing test**

Add this to the existing `#[cfg(test)] mod tests { ... }` block in `src/sysinfo.rs` (add these `#[test]` functions inside the existing braces, alongside the other tests):

```rust
    #[test]
    fn disk_usage_reports_sane_values_for_current_dir() {
        let (free, total) = disk_usage(Path::new(".")).expect("查詢目前目錄所在檔案系統的容量應該要成功");
        assert!(total > 0, "總容量應該大於 0");
        assert!(free <= total, "可用容量不應該超過總容量");
    }

    #[test]
    fn format_disk_usage_formats_as_free_slash_total() {
        // 512G = 512 * 1024^3 bytes，302G = 302 * 1024^3 bytes——挑不會因為
        // 四捨五入邊界而變得不穩定的整數 GiB 值。
        let total = 512u64 * 1024 * 1024 * 1024;
        let free = 302u64 * 1024 * 1024 * 1024;
        assert_eq!(format_disk_usage(free, total), "302G/512G");
    }

    #[test]
    fn format_disk_usage_reports_na_when_total_is_zero() {
        assert_eq!(format_disk_usage(0, 0), "N/A");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test sysinfo:: 2>&1 | tail -40`
Expected: compile error — `cannot find function 'disk_usage'` / `'format_disk_usage'` in this scope (neither exists yet).

- [ ] **Step 3: Implement `disk_usage` and `format_disk_usage`**

Add `use std::path::Path;` to the top of `src/sysinfo.rs`, alongside the existing `use std::net::UdpSocket;` / `use std::process::Command;` lines.

Add this Windows FFI declaration to the **existing** `#[cfg(windows)] unsafe extern "system" { ... }` block (the one already containing `GetTickCount64`/`GetComputerNameW`/etc. — add this as one more function inside those same braces, don't create a second `extern` block):

```rust
    /// `disk_usage` 用：查 `lpDirectoryName` 所在磁碟區的可用/總共容量
    /// （`kernel32.dll`）。三個容量參數在 Win32 API 裡的型別是
    /// `PULARGE_INTEGER`（`ULARGE_INTEGER` 的指標），但 `ULARGE_INTEGER`
    /// 的記憶體佈局就是單純一個 8 bytes 的無號整數（透過 `.QuadPart` 存取），
    /// 直接宣告成 `*mut u64` 可以拿到一樣的資料，不需要另外宣告那個 union
    /// 型別。失敗（例如路徑不存在、沒有權限）回傳 0。
    fn GetDiskFreeSpaceExW(
        lpDirectoryName: *const u16,
        lpFreeBytesAvailableToCaller: *mut u64,
        lpTotalNumberOfBytes: *mut u64,
        lpTotalNumberOfFreeBytes: *mut u64,
    ) -> i32;
```

Add these two functions anywhere in `src/sysinfo.rs` after the `format_uptime` function (a sensible place — both are "format a system-resource number for display" style helpers):

```rust
/// 量測 `path` 所在檔案系統的可用/總共容量（bytes）。查不到（指令失敗、FFI
/// 呼叫失敗）就回傳 `None`，呼叫端（`system` plugin 的 `build_report`）落地
/// 成 0，顯示端看到 0 當作查不到處理——跟 `os`/`version` 現有「查不到就填
/// 預設值，不能讓整包解析失敗」同一套精神。
#[cfg(not(windows))]
pub fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    let output = Command::new("df").arg("-k").arg(path).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    // `df -k` 至少輸出兩行：表頭 + 一筆資料。掛載點路徑太長時 `df` 會把
    // Filesystem 那欄自己獨立一行、資料擠到下一行，所以用「最後一行」而不是
    // 寫死拿第二行，兩種輸出排版都能正確解析。
    let last_line = text.lines().last()?;
    let fields: Vec<&str> = last_line.split_whitespace().collect();
    // 標準欄位順序：Filesystem, 1K-blocks, Used, Available, Use%, Mounted on
    if fields.len() < 4 {
        return None;
    }
    let total_kb: u64 = fields[1].parse().ok()?;
    let free_kb: u64 = fields[3].parse().ok()?;
    Some((free_kb * 1024, total_kb * 1024))
}

#[cfg(windows)]
pub fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut free_available: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut total_free: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(wide.as_ptr(), &mut free_available, &mut total_bytes, &mut total_free) != 0
    };
    if !ok {
        return None;
    }
    Some((free_available, total_bytes))
}

/// 把可用/總共 bytes 換算成 `device`/`global` 表格用的精簡格式，例如
/// `302G/512G`；`total == 0`（代表查不到）就回傳 `N/A`。兩個表格共用同一份
/// （跟 `format_uptime` 已經是這兩個表格共用的既有慣例一致），不是各自
/// 維護一份。
pub fn format_disk_usage(free: u64, total: u64) -> String {
    if total == 0 {
        return "N/A".to_string();
    }
    format!("{}/{}", format_bytes_short(free), format_bytes_short(total))
}

fn format_bytes_short(bytes: u64) -> String {
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
        format!("{value:.0}{}", UNITS[unit])
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test sysinfo:: 2>&1 | tail -40`
Expected: all 3 new tests pass, alongside the existing `sysinfo::tests` (e.g. `local_hms_matches_system_clock`, `pid_alive_reflects_process_lifetime`).

- [ ] **Step 5: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -20` — expect clean, no errors, no warnings.
Run: `cargo test 2>&1 | tail -15` — expect all tests passing (93 existing + 3 new = 96).

- [ ] **Step 6: Commit**

```bash
git add src/sysinfo.rs
git commit -m "$(cat <<'EOF'
新增 sysinfo::disk_usage 與 format_disk_usage

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Wire disk usage into `DeviceReport` and the `device`/`global` tables

**Files:**
- Modify: `src/plugin.rs` (add two fields to `DeviceReport`)
- Modify: `src/plugins/system.rs` (`build_report` populates the new fields)
- Modify: `src/plugins/device.rs` (table grows to 10 columns)
- Modify: `src/plugins/global.rs` (table grows to 10 columns)

**Interfaces:**
- Consumes: `sysinfo::disk_usage`/`sysinfo::format_disk_usage` from Task 1.
- Produces: `DeviceReport` gains `pub disk_free_bytes: u64` and `pub disk_total_bytes: u64` (both `#[serde(default)]`), automatically carried through the existing `/api/device/register` HTTP endpoint and the `global` plugin's MQTT cross-domain relay (both already serialize/deserialize `DeviceReport` as a whole — no endpoint changes needed).

- [ ] **Step 1: Add the two fields to `DeviceReport`**

In `src/plugin.rs`, change the `DeviceReport` struct (currently ending with `app_uptime_secs: u64,`) from:

```rust
#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceReport {
    pub id: String,
    pub ip: String,
    #[serde(default = "default_os")]
    pub os: String,
    #[serde(default = "default_version")]
    pub version: String,
    pub tailscale: bool,
    pub mode: String,
    pub device_uptime_secs: u64,
    pub app_uptime_secs: u64,
}
```

to:

```rust
#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceReport {
    pub id: String,
    pub ip: String,
    #[serde(default = "default_os")]
    pub os: String,
    #[serde(default = "default_version")]
    pub version: String,
    pub tailscale: bool,
    pub mode: String,
    pub device_uptime_secs: u64,
    pub app_uptime_secs: u64,
    /// 這台裝置回報當下，程式執行目錄所在檔案系統的可用容量（bytes）。舊版
    /// （還沒有這個欄位的 build）傳過來的 JSON 缺這個 key 時，`#[serde(default)]`
    /// 讓它解析成 0——跟 `os`/`version` 同一套「缺欄位不能讓整筆資料解析失敗」
    /// 的理由，0 在顯示端（`sysinfo::format_disk_usage`）會被當成「查不到」
    /// 印成 `N/A`。
    #[serde(default)]
    pub disk_free_bytes: u64,
    /// 同上，總共容量（bytes）。
    #[serde(default)]
    pub disk_total_bytes: u64,
}
```

- [ ] **Step 2: Populate the fields in `build_report`**

In `src/plugins/system.rs`, add `use std::path::Path;` to the top of the file, alongside the existing `use std::process::Command;` line.

Change `build_report` (currently `src/plugins/system.rs:183-196`) from:

```rust
    fn build_report(id: &str, tailscale: &TailscaleCache, mode: SystemMode) -> DeviceReport {
        let tailscale_ip = tailscale.get();
        let ip = tailscale_ip.clone().unwrap_or_else(sysinfo::local_ip);
        DeviceReport {
            id: id.to_string(),
            ip,
            os: sysinfo::os().to_string(),
            version: APP_VERSION.to_string(),
            tailscale: tailscale_ip.is_some(),
            mode: mode.as_str().to_string(),
            device_uptime_secs: sysinfo::device_uptime_secs(),
            app_uptime_secs: sysinfo::app_uptime_secs(),
        }
    }
```

to:

```rust
    fn build_report(id: &str, tailscale: &TailscaleCache, mode: SystemMode) -> DeviceReport {
        let tailscale_ip = tailscale.get();
        let ip = tailscale_ip.clone().unwrap_or_else(sysinfo::local_ip);
        let (disk_free_bytes, disk_total_bytes) = sysinfo::disk_usage(Path::new(".")).unwrap_or((0, 0));
        DeviceReport {
            id: id.to_string(),
            ip,
            os: sysinfo::os().to_string(),
            version: APP_VERSION.to_string(),
            tailscale: tailscale_ip.is_some(),
            mode: mode.as_str().to_string(),
            device_uptime_secs: sysinfo::device_uptime_secs(),
            app_uptime_secs: sysinfo::app_uptime_secs(),
            disk_free_bytes,
            disk_total_bytes,
        }
    }
```

- [ ] **Step 3: Add the column to `device.rs`'s table**

In `src/plugins/device.rs`, change `table_text` (currently lines 46-75) from:

```rust
        let headers =
            ["  id", "ip", "os", "version", "tailscale", "mode", "device uptime", "app uptime", "alive"];
        let rows: Vec<[String; 9]> = ids
            .into_iter()
            .map(|id| {
                let entry = &inner.devices[id];
                let alive = entry.last_seen.elapsed() < ALIVE_TTL;
                let id_cell = if entry.report.id == my_id {
                    format!("* {}", entry.report.id)
                } else {
                    format!("  {}", entry.report.id)
                };
                [
                    id_cell,
                    entry.report.ip.clone(),
                    entry.report.os.clone(),
                    entry.report.version.clone(),
                    yes_no(entry.report.tailscale),
                    entry.report.mode.clone(),
                    sysinfo::format_uptime(entry.report.device_uptime_secs),
                    sysinfo::format_uptime(entry.report.app_uptime_secs),
                    if alive { "*".to_string() } else { String::new() },
                ]
            })
            .collect();
        render_table(&headers, &rows)
```

to:

```rust
        let headers = [
            "  id", "ip", "os", "version", "tailscale", "mode", "device uptime", "app uptime", "disk",
            "alive",
        ];
        let rows: Vec<[String; 10]> = ids
            .into_iter()
            .map(|id| {
                let entry = &inner.devices[id];
                let alive = entry.last_seen.elapsed() < ALIVE_TTL;
                let id_cell = if entry.report.id == my_id {
                    format!("* {}", entry.report.id)
                } else {
                    format!("  {}", entry.report.id)
                };
                [
                    id_cell,
                    entry.report.ip.clone(),
                    entry.report.os.clone(),
                    entry.report.version.clone(),
                    yes_no(entry.report.tailscale),
                    entry.report.mode.clone(),
                    sysinfo::format_uptime(entry.report.device_uptime_secs),
                    sysinfo::format_uptime(entry.report.app_uptime_secs),
                    sysinfo::format_disk_usage(entry.report.disk_free_bytes, entry.report.disk_total_bytes),
                    if alive { "*".to_string() } else { String::new() },
                ]
            })
            .collect();
        render_table(&headers, &rows)
```

Change the `render_table` function signature (currently `src/plugins/device.rs`, right after `yes_no`) from:

```rust
fn render_table(headers: &[&str], rows: &[[String; 9]]) -> String {
```

to:

```rust
fn render_table(headers: &[&str], rows: &[[String; 10]]) -> String {
```

(The function body doesn't reference the array length anywhere else — it iterates via `.iter()`/`.zip()` — so no other change is needed inside `render_table` itself.)

Update the `MANUAL_TEXT` constant's example line — change:

```
  list                查表格：每台裝置的 id/ip/os/version/tailscale/mode/uptime/alive
```

to:

```
  list                查表格：每台裝置的 id/ip/os/version/tailscale/mode/uptime/disk/alive
```

- [ ] **Step 4: Add the column to `global.rs`'s table**

In `src/plugins/global.rs`, change `table_text` (currently lines 257-284) from:

```rust
        let headers = ["domain", "id", "ip", "os", "version", "mode", "device uptime", "app uptime", "alive"];
        let rows: Vec<[String; 9]> = items
            .into_iter()
            .map(|item| {
                let alive = item.age_secs < ALIVE_TTL.as_secs_f64();
                [
                    item.domain,
                    item.report.id,
                    item.report.ip,
                    item.report.os,
                    item.report.version,
                    item.report.mode,
                    sysinfo::format_uptime(item.report.device_uptime_secs),
                    sysinfo::format_uptime(item.report.app_uptime_secs),
                    if alive { "*".to_string() } else { String::new() },
                ]
            })
            .collect();
        render_table(&headers, &rows)
```

to:

```rust
        let headers = [
            "domain", "id", "ip", "os", "version", "mode", "device uptime", "app uptime", "disk", "alive",
        ];
        let rows: Vec<[String; 10]> = items
            .into_iter()
            .map(|item| {
                let alive = item.age_secs < ALIVE_TTL.as_secs_f64();
                [
                    item.domain,
                    item.report.id,
                    item.report.ip,
                    item.report.os,
                    item.report.version,
                    item.report.mode,
                    sysinfo::format_uptime(item.report.device_uptime_secs),
                    sysinfo::format_uptime(item.report.app_uptime_secs),
                    sysinfo::format_disk_usage(item.report.disk_free_bytes, item.report.disk_total_bytes),
                    if alive { "*".to_string() } else { String::new() },
                ]
            })
            .collect();
        render_table(&headers, &rows)
```

(`disk_free_bytes`/`disk_total_bytes` are `u64`, a `Copy` type, so reading them via `item.report.disk_free_bytes` works regardless of where in the array literal it appears relative to the `String` fields being moved out of `item.report` — no ordering constraint here, unlike moving two different `String` fields out in the wrong order would be.)

Change the `render_table` function signature in `src/plugins/global.rs` (right after `run_mqtt_session`'s closing, or wherever it currently sits — search for `fn render_table`) from:

```rust
fn render_table(headers: &[&str], rows: &[[String; 9]]) -> String {
```

to:

```rust
fn render_table(headers: &[&str], rows: &[[String; 10]]) -> String {
```

- [ ] **Step 5: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -30`
Expected: clean build, no errors, no warnings.

Run: `cargo test 2>&1 | tail -15`
Expected: all 96 tests pass (no regressions; this task adds no new tests — `device.rs`/`global.rs` have no existing test module, matching the plan's Global Constraints).

- [ ] **Step 6: Manual smoke test**

Run: `cargo run`, then at the root prompt:

```
device
list
exit
global
list
exit
```

Confirm:
1. `device list` now shows a `disk` column between `app uptime` and `alive`, formatted like `302G/512G` (actual numbers will differ) for this machine's own row.
2. `global list` shows the same new column (will likely say "還沒有任何跨 domain 裝置資料..." if no `domain`/`bridge` is configured in this environment — that's expected and fine, it just confirms the code compiles and the empty-state message still works; the column only becomes visible once there's at least one cross-domain device to show).

- [ ] **Step 7: Commit**

```bash
git add src/plugin.rs src/plugins/system.rs src/plugins/device.rs src/plugins/global.rs
git commit -m "$(cat <<'EOF'
在 device/global 表格顯示磁機用量

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```
