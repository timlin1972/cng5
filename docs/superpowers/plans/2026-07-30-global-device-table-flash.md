# global／device panel 欄位變化閃爍效果 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `global`／`device` panel（TUI 跟 web 兩邊）在任何一格資料跟上一次看到的不一樣時（含這個 row 第一次出現），短暫閃爍提示（白底閃一下，1 秒內淡出）。

**Architecture:** 新增一個共用模組 `src/plugins/table_diff.rs`，提供 `RowDiffTracker`（記錄「這個 viewer 上次看到的表格」，每次呼叫算出哪些格子剛好變了）跟 `TableSnapshot`（結構化的表頭＋逐格資料）。`GlobalPlugin`／`DevicePlugin` 各自持有兩份獨立的 tracker（一份給 TUI、一份給 web，避免兩邊重繪頻率不同互搶而漏閃），新增 `tui_snapshot()`／`web_snapshot()` 方法。`gui.rs` 用既有的向下轉型套路（比照 `with_notepad`）拿到 snapshot，逐格畫 `Span`＋`Style` 模擬淡出；`web.rs` 把 snapshot 轉成 JSON 走既有的 SSE 管道，`frontend.html` 逐格建 `<table>`，變化的格子套 CSS 動畫。

**Tech Stack:** Rust（ratatui/crossterm 做 TUI，actix-web+SSE 做 web 推播，serde/serde_json 做序列化，全部已經是既有依賴，不新增任何 crate），純 JS＋CSS（`frontend.html` 內嵌，無框架）。

## Global Constraints

- 不改變 `table_text()`／`render_table()`／CLI `list` 指令目前的輸出格式（純文字表格的內容、對齊方式必須跟改動前逐字元相同）。
- 不新增任何 Cargo 依賴——`serde`／`serde_json`／`unicode_width`／`ratatui` 都已經是既有依賴。
- TUI 淡出效果只能用 ratatui 內建具名顏色（`Color::White`/`Gray`/`DarkGray`），不依賴 truecolor。
- Web 前端逐格填值一律用 DOM `textContent`（不得拼接 HTML 字串或使用 `innerHTML` 塞入後端傳來的文字），因為 `global` 的資料有一部分來自其他 domain 透過公開 MQTT broker 回報的字串，不能假設乾淨。

---

### Task 1: 新增共用模組 `table_diff.rs`（逐格 diff 追蹤器）

**Files:**
- Create: `src/plugins/table_diff.rs`
- Modify: `src/plugins/mod.rs`

**Interfaces:**
- Produces（後面所有 task 都靠這幾個名字）：
  - `pub(crate) struct TableCell { pub text: String, pub changed_at: std::time::Instant }`
  - `pub(crate) struct TableRow { pub cells: Vec<TableCell> }`
  - `pub(crate) struct TableSnapshot { pub headers: Vec<String>, pub rows: Vec<TableRow>, /* private */ computed_at: Instant }`
    - `impl TableSnapshot { pub(crate) fn column_widths(&self) -> Vec<usize>; pub(crate) fn to_json(&self) -> JsonTableSnapshot; }`
  - `pub(crate) struct RowDiffTracker` with `pub(crate) fn new() -> Self` and
    `pub(crate) fn snapshot(&mut self, headers: &[&str], rows: Vec<(String, Vec<String>)>) -> TableSnapshot`
  - `plugins::mod.rs` 重新匯出 `pub(crate) use table_diff::{RowDiffTracker, TableSnapshot};`（`JsonTableSnapshot` 等 JSON 型別不用匯出，只有 `TableSnapshot::to_json()` 的回傳型別用得到，呼叫端不需要指名）。

- [ ] **Step 1: 寫 `table_diff.rs`（含實作，先讓下面的測試有東西可以編譯）**

```rust
use std::collections::HashMap;
use std::time::Instant;

use unicode_width::UnicodeWidthStr;

/// 一個表格欄位目前的文字，以及「最近一次偵測到跟上一輪不一樣」的時間點。
/// 因為 `RowDiffTracker` 把「這個 row 是第一次出現」也算進「有變化」，一個
/// 出現在 `TableSnapshot` 裡的格子必定至少變化過一次（就是它被建立的那一
/// 刻），所以這裡不用 `Option<Instant>`。
pub(crate) struct TableCell {
    pub text: String,
    pub changed_at: Instant,
}

pub(crate) struct TableRow {
    pub cells: Vec<TableCell>,
}

/// 一次「目前這個 viewer 看到的表格長什麼樣子」。`computed_at` 是這次呼叫
/// `RowDiffTracker::snapshot` 當下的時間，用來判斷哪些格子是「這一次剛好
/// 變化」（給 web 的 `changed` 布林值用，見 `to_json`）；TUI 端要的是持續
/// 淡出的效果，直接用每個 `TableCell::changed_at.elapsed()`，不需要
/// `computed_at`。
pub(crate) struct TableSnapshot {
    pub headers: Vec<String>,
    pub rows: Vec<TableRow>,
    computed_at: Instant,
}

impl TableSnapshot {
    /// 依目前內容算出每欄要多寬（跟表頭/所有格子裡最寬的那個看齊），給 TUI
    /// 逐格畫的時候組固定寬度用，跟 `global.rs`/`device.rs` 原本
    /// `render_table` 的排版邏輯一致。
    pub(crate) fn column_widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = self.headers.iter().map(|h| UnicodeWidthStr::width(h.as_str())).collect();
        for row in &self.rows {
            for (width, cell) in widths.iter_mut().zip(&row.cells) {
                *width = (*width).max(UnicodeWidthStr::width(cell.text.as_str()));
            }
        }
        widths
    }

    /// 轉成給 web 用的 JSON 可序列化版本：`Instant` 不能序列化，改成單純的
    /// `changed` 布林值——`true` 代表這一格在這次 snapshot 裡剛好被偵測到
    /// 變化（`changed_at` 剛好等於這次呼叫的 `computed_at`）。
    pub(crate) fn to_json(&self) -> JsonTableSnapshot {
        JsonTableSnapshot {
            headers: self.headers.clone(),
            rows: self
                .rows
                .iter()
                .map(|row| JsonTableRow {
                    cells: row
                        .cells
                        .iter()
                        .map(|cell| JsonTableCell {
                            text: cell.text.clone(),
                            changed: cell.changed_at == self.computed_at,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

#[derive(serde::Serialize)]
pub(crate) struct JsonTableSnapshot {
    pub headers: Vec<String>,
    pub rows: Vec<JsonTableRow>,
}

#[derive(serde::Serialize)]
pub(crate) struct JsonTableRow {
    pub cells: Vec<JsonTableCell>,
}

#[derive(serde::Serialize)]
pub(crate) struct JsonTableCell {
    pub text: String,
    pub changed: bool,
}

/// 記錄「這個 viewer 上一次看到的表格長什麼樣子」，每次呼叫 `snapshot()`
/// 拿目前資料跟這份記錄比對，算出每一格是不是「剛好變了」，再把記錄整個
/// 換成這一次的內容。
///
/// `global`／`device` 這兩個 plugin 各自需要兩份獨立的 `RowDiffTracker`
/// （一份給 TUI、一份給 web）：TUI 最快 200ms 重繪一次、web 固定 300ms 一個
/// tick，如果共用同一份記錄，先讀到變化的那一邊會把記錄更新掉，另一邊接著
/// 讀就會誤判成「沒變」而漏掉閃爍效果。
#[derive(Default)]
pub(crate) struct RowDiffTracker {
    rows: HashMap<String, Vec<(String, Instant)>>,
}

impl RowDiffTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// `headers` 是欄位名稱、`rows` 是「這次」的 (row key, 每一欄的文字)。
    /// 只要某個 key 這次沒出現在輸入裡，它在下一次呼叫用的記錄裡就會消失
    /// ——之後同一個 key 重新出現時，等同「第一次出現」，會整列重新算成
    /// 「有變化」，不需要另外處理「row 消失後又出現」這種情況（也涵蓋
    /// `global clear`／裝置離線很久之後資料重新回來的情況）。
    pub(crate) fn snapshot(&mut self, headers: &[&str], rows: Vec<(String, Vec<String>)>) -> TableSnapshot {
        let now = Instant::now();
        let mut next = HashMap::with_capacity(rows.len());
        let mut out_rows = Vec::with_capacity(rows.len());
        for (key, values) in rows {
            let previous = self.rows.get(&key);
            let mut cells = Vec::with_capacity(values.len());
            let mut recorded = Vec::with_capacity(values.len());
            for (i, value) in values.into_iter().enumerate() {
                let changed_at = match previous.and_then(|p| p.get(i)) {
                    Some((prev_value, prev_changed_at)) if *prev_value == value => *prev_changed_at,
                    _ => now,
                };
                recorded.push((value.clone(), changed_at));
                cells.push(TableCell { text: value, changed_at });
            }
            next.insert(key, recorded);
            out_rows.push(TableRow { cells });
        }
        self.rows = next;
        TableSnapshot { headers: headers.iter().map(|h| h.to_string()).collect(), rows: out_rows, computed_at: now }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_appearance_counts_as_changed() {
        let mut tracker = RowDiffTracker::new();
        let snapshot =
            tracker.snapshot(&["a", "b"], vec![("k1".to_string(), vec!["1".to_string(), "2".to_string()])]);
        let json = snapshot.to_json();
        assert_eq!(json.headers, vec!["a", "b"]);
        assert_eq!(json.rows.len(), 1);
        assert!(json.rows[0].cells.iter().all(|c| c.changed));
    }

    #[test]
    fn unchanged_value_keeps_old_changed_at() {
        let mut tracker = RowDiffTracker::new();
        let first = tracker.snapshot(&["a"], vec![("k1".to_string(), vec!["1".to_string()])]);
        let first_changed_at = first.rows[0].cells[0].changed_at;
        let second = tracker.snapshot(&["a"], vec![("k1".to_string(), vec!["1".to_string()])]);
        assert_eq!(second.rows[0].cells[0].changed_at, first_changed_at);
        assert!(!second.to_json().rows[0].cells[0].changed);
    }

    #[test]
    fn only_the_differing_column_updates() {
        let mut tracker = RowDiffTracker::new();
        tracker.snapshot(&["a", "b"], vec![("k1".to_string(), vec!["1".to_string(), "x".to_string()])]);
        let second = tracker.snapshot(&["a", "b"], vec![("k1".to_string(), vec!["1".to_string(), "y".to_string()])]);
        let json = second.to_json();
        assert!(!json.rows[0].cells[0].changed);
        assert!(json.rows[0].cells[1].changed);
    }

    #[test]
    fn row_missing_then_reappearing_counts_as_new() {
        let mut tracker = RowDiffTracker::new();
        tracker.snapshot(&["a"], vec![("k1".to_string(), vec!["1".to_string()])]);
        tracker.snapshot(&["a"], vec![]); // k1 這一輪沒出現，等同消失
        let third = tracker.snapshot(&["a"], vec![("k1".to_string(), vec!["1".to_string()])]);
        assert!(third.to_json().rows[0].cells[0].changed);
    }
}
```

- [ ] **Step 2: 在 `src/plugins/mod.rs` 註冊模組並匯出**

在檔案開頭的 `mod ...;` 清單裡（跟 `mod system;` 同一段，字母順序插入）加一行：

```rust
mod table_diff;
```

在下面 `pub(crate) use` 那幾行（跟 `pub(crate) use system::REPORT_INTERVAL;` 相鄰即可）加一行：

```rust
pub(crate) use table_diff::{RowDiffTracker, TableSnapshot};
```

- [ ] **Step 3: 執行測試，確認全部通過**

Run: `cargo test table_diff`
Expected: 4 個測試（`first_appearance_counts_as_changed`／`unchanged_value_keeps_old_changed_at`／`only_the_differing_column_updates`／`row_missing_then_reappearing_counts_as_new`）全部 PASS。

- [ ] **Step 4: 確認整個專案仍能編譯**

Run: `cargo build`
Expected: 成功，無 warning（新模組目前沒有任何呼叫端，`RowDiffTracker`/`TableSnapshot` 會被視為「已使用」是因為 `mod.rs` 的 `pub(crate) use`——如果編譯器仍警告 dead_code，先確認 Step 2 有確實加上匯出）。

- [ ] **Step 5: Commit**

```bash
git add src/plugins/table_diff.rs src/plugins/mod.rs
git commit -m "$(cat <<'EOF'
新增共用的表格逐格 diff 追蹤器 table_diff

給 global/device panel 的欄位變化閃爍效果用，「第一次出現」也算變化，
一個 plugin 需要 TUI/web 各自獨立一份追蹤狀態（見模組內註解說明原因）。
EOF
)"
```

---

### Task 2: `GlobalPlugin` 接上 `table_diff`

**Files:**
- Modify: `src/plugins/global.rs:82-99`（struct 定義）、`:101-107`（`new()`）、`:257-294`（`table_text()`）、`:786-801`（`render_table()`）

**Interfaces:**
- Consumes: Task 1 的 `RowDiffTracker`、`TableSnapshot`（透過 `use crate::plugins::{RowDiffTracker, TableSnapshot};`，跟現有 `use crate::plugins::{...}` 那行合併）。
- Produces：
  - `pub(crate) fn GlobalPlugin::tui_snapshot(&self) -> TableSnapshot`
  - `pub(crate) fn GlobalPlugin::web_snapshot(&self) -> TableSnapshot`
  （這兩個是 Task 4／Task 5 要呼叫的介面）

- [ ] **Step 1: 修改 import**

`src/plugins/global.rs` 開頭的：

```rust
use crate::plugins::{
    make_dir, paginate_sync_entries, read_chunk, remove, rename_path, safe_music_copy_path, safe_storage_path,
    url_encode_filename, walk_with_hashes, write_chunk, REPORT_INTERVAL, STORAGE_DIR,
};
```

改成：

```rust
use crate::plugins::{
    make_dir, paginate_sync_entries, read_chunk, remove, rename_path, safe_music_copy_path, safe_storage_path,
    url_encode_filename, walk_with_hashes, write_chunk, RowDiffTracker, TableSnapshot, REPORT_INTERVAL, STORAGE_DIR,
};
```

- [ ] **Step 2: struct 加兩個欄位，`new()` 一併初始化**

把：

```rust
pub struct GlobalPlugin {
    ctx: SharedContext,
    bridge: Arc<Mutex<Option<String>>>,
    connected: Arc<Mutex<bool>>,
    clear_status: Arc<Mutex<Option<String>>>,
}
```

改成：

```rust
pub struct GlobalPlugin {
    ctx: SharedContext,
    bridge: Arc<Mutex<Option<String>>>,
    connected: Arc<Mutex<bool>>,
    clear_status: Arc<Mutex<Option<String>>>,
    /// TUI 用的逐格變化比對狀態，跟 `web_diff` 分開存的理由見
    /// `table_diff::RowDiffTracker` 的說明——TUI／web 兩邊重繪頻率不同，共用
    /// 一份會有其中一邊讀到「已經被另一邊消費掉的變化」而漏閃的 race。
    tui_diff: Mutex<RowDiffTracker>,
    web_diff: Mutex<RowDiffTracker>,
}
```

把：

```rust
    pub fn new(ctx: SharedContext) -> Self {
        let bridge = Arc::new(Mutex::new(None));
        let connected = Arc::new(Mutex::new(false));
        Self::spawn_supervisor(ctx.clone(), bridge.clone(), connected.clone());
        Self { ctx, bridge, connected, clear_status: Arc::new(Mutex::new(None)) }
    }
```

改成：

```rust
    pub fn new(ctx: SharedContext) -> Self {
        let bridge = Arc::new(Mutex::new(None));
        let connected = Arc::new(Mutex::new(false));
        Self::spawn_supervisor(ctx.clone(), bridge.clone(), connected.clone());
        Self {
            ctx,
            bridge,
            connected,
            clear_status: Arc::new(Mutex::new(None)),
            tui_diff: Mutex::new(RowDiffTracker::new()),
            web_diff: Mutex::new(RowDiffTracker::new()),
        }
    }
```

- [ ] **Step 3: 把 `table_text()` 拆成共用的 `rows()` + 不變的輸出，新增兩個 snapshot 方法**

把整個 `table_text()`（原本第 257-294 行）：

```rust
    fn table_text(&self) -> String {
        let mut items = merged_global_view(&self.ctx.lock().unwrap());
        if items.is_empty() {
            return "(還沒有任何跨 domain 裝置資料——確認 domain/bridge 有沒有設定，\n\
                     或這台機器目前是不是 client 角色、它的 server 有沒有設定過)"
                .to_string();
        }
        items.sort_by(|a, b| (&a.domain, &a.report.id).cmp(&(&b.domain, &b.report.id)));

        let headers = ["id", "ip", "os", "version", "mode", "device uptime", "app uptime", "disk", "alive"];
        let rows: Vec<[String; 9]> = items
            .into_iter()
            .map(|item| {
                let alive = item.age_secs < ALIVE_TTL.as_secs_f64();
                // mode 縮寫成單一字元，省表格寬度：s=server、c=client、
                // -=standalone（還沒設過 mode 的預設值）。查不到對應值（未來
                // 版本間協定不一致）就照原字串顯示，不讓表格憑空消失一欄。
                let mode = match item.report.mode.as_str() {
                    "server" => "s".to_string(),
                    "client" => "c".to_string(),
                    "standalone" => "-".to_string(),
                    other => other.to_string(),
                };
                [
                    format!("{}/{}", item.domain, item.report.id),
                    item.report.ip,
                    item.report.os,
                    item.report.version,
                    mode,
                    sysinfo::format_uptime(item.report.device_uptime_secs),
                    sysinfo::format_uptime(item.report.app_uptime_secs),
                    sysinfo::format_disk_usage(item.report.disk_free_bytes, item.report.disk_total_bytes),
                    if alive { "*".to_string() } else { String::new() },
                ]
            })
            .collect();
        render_table(&headers, &rows)
    }
```

換成：

```rust
    /// 組出目前每一列的 (row key, 每一欄的文字)，`table_text()`／
    /// `tui_snapshot()`／`web_snapshot()` 共用同一份，只是後續分別拿去排版
    /// 成純文字表格，或是餵給 diff tracker 算出「哪一格剛好跟上次不一樣」。
    fn rows(&self) -> Vec<(String, Vec<String>)> {
        let mut items = merged_global_view(&self.ctx.lock().unwrap());
        items.sort_by(|a, b| (&a.domain, &a.report.id).cmp(&(&b.domain, &b.report.id)));
        items
            .into_iter()
            .map(|item| {
                let alive = item.age_secs < ALIVE_TTL.as_secs_f64();
                // mode 縮寫成單一字元，省表格寬度：s=server、c=client、
                // -=standalone（還沒設過 mode 的預設值）。查不到對應值（未來
                // 版本間協定不一致）就照原字串顯示，不讓表格憑空消失一欄。
                let mode = match item.report.mode.as_str() {
                    "server" => "s".to_string(),
                    "client" => "c".to_string(),
                    "standalone" => "-".to_string(),
                    other => other.to_string(),
                };
                let key = format!("{}/{}", item.domain, item.report.id);
                let cells = vec![
                    key.clone(),
                    item.report.ip,
                    item.report.os,
                    item.report.version,
                    mode,
                    sysinfo::format_uptime(item.report.device_uptime_secs),
                    sysinfo::format_uptime(item.report.app_uptime_secs),
                    sysinfo::format_disk_usage(item.report.disk_free_bytes, item.report.disk_total_bytes),
                    if alive { "*".to_string() } else { String::new() },
                ];
                (key, cells)
            })
            .collect()
    }

    fn table_text(&self) -> String {
        let rows = self.rows();
        if rows.is_empty() {
            return "(還沒有任何跨 domain 裝置資料——確認 domain/bridge 有沒有設定，\n\
                     或這台機器目前是不是 client 角色、它的 server 有沒有設定過)"
                .to_string();
        }
        let row_values: Vec<Vec<String>> = rows.into_iter().map(|(_, cells)| cells).collect();
        render_table(&HEADERS, &row_values)
    }

    /// 給 TUI 用的逐格 snapshot，`gui.rs` 的 `with_global` 會呼叫這個。
    pub(crate) fn tui_snapshot(&self) -> TableSnapshot {
        self.tui_diff.lock().unwrap().snapshot(&HEADERS, self.rows())
    }

    /// 給 web SSE 用的逐格 snapshot，`web.rs` 呼叫後轉成 JSON 推播。
    pub(crate) fn web_snapshot(&self) -> TableSnapshot {
        self.web_diff.lock().unwrap().snapshot(&HEADERS, self.rows())
    }
```

在 `impl GlobalPlugin` 區塊之前（例如緊接在 `ALIVE_TTL` 常數定義之後）新增模組層級常數：

```rust
const HEADERS: [&str; 9] = ["id", "ip", "os", "version", "mode", "device uptime", "app uptime", "disk", "alive"];
```

- [ ] **Step 4: `render_table()` 簽名從固定陣列改成 `Vec<String>`（排版邏輯不變）**

把（原本第 786-801 行）：

```rust
fn render_table(headers: &[&str], rows: &[[String; 9]]) -> String {
```

改成：

```rust
fn render_table(headers: &[&str], rows: &[Vec<String>]) -> String {
```

函式內容（迴圈/`zip`/`pad`/`join` 那些）完全不用改，`Vec<String>` 一樣可以 `.iter().zip(&widths)`。

- [ ] **Step 5: 確認 CLI 輸出沒有變**

Run: `cargo build`
Expected: 成功。

Run: `cargo run -- ` 進互動模式後輸入 `global list`（或先 `global status`），跟修改前的輸出比對（可以先在修改前用 `git stash` 記一份輸出，或單純確認格式仍是「表頭 + 分隔線 + 對齊好的每一列」，沒有跑版）。

- [ ] **Step 6: Commit**

```bash
git add src/plugins/global.rs
git commit -m "$(cat <<'EOF'
global plugin 接上 table_diff，新增 tui_snapshot/web_snapshot

table_text()/render_table() 的輸出格式不變，只是把組 row 資料的邏輯抽成
共用的 rows()，讓新的 snapshot 方法可以重用同一份邏輯。
EOF
)"
```

---

### Task 3: `DevicePlugin` 接上 `table_diff`

**Files:**
- Modify: `src/plugins/device.rs`（整份檔案的 import、struct、`new()`、`table_text()`、`render_table()`）

**Interfaces:**
- Consumes: Task 1 的 `RowDiffTracker`、`TableSnapshot`。
- Produces：
  - `pub(crate) fn DevicePlugin::tui_snapshot(&self) -> TableSnapshot`
  - `pub(crate) fn DevicePlugin::web_snapshot(&self) -> TableSnapshot`

- [ ] **Step 1: 修改 import**

把檔案開頭的：

```rust
use std::time::Duration;

use anyhow::{bail, Result};
use unicode_width::UnicodeWidthStr;

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};
use crate::plugins::REPORT_INTERVAL;
use crate::sysinfo;
```

改成：

```rust
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Result};
use unicode_width::UnicodeWidthStr;

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};
use crate::plugins::{RowDiffTracker, TableSnapshot, REPORT_INTERVAL};
use crate::sysinfo;
```

- [ ] **Step 2: struct 加兩個欄位，`new()` 一併初始化**

把：

```rust
pub struct DevicePlugin {
    ctx: SharedContext,
}

impl DevicePlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx }
    }
```

改成：

```rust
pub struct DevicePlugin {
    ctx: SharedContext,
    /// TUI 用的逐格變化比對狀態，跟 `web_diff` 分開存的理由見
    /// `table_diff::RowDiffTracker` 的說明——TUI／web 兩邊重繪頻率不同，共用
    /// 一份會有其中一邊讀到「已經被另一邊消費掉的變化」而漏閃的 race。
    tui_diff: Mutex<RowDiffTracker>,
    web_diff: Mutex<RowDiffTracker>,
}

impl DevicePlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx, tui_diff: Mutex::new(RowDiffTracker::new()), web_diff: Mutex::new(RowDiffTracker::new()) }
    }
```

- [ ] **Step 3: 把 `table_text()` 拆成共用的 `rows()`，新增兩個 snapshot 方法**

把（原本第 46-93 行）：

```rust
    fn table_text(&self) -> String {
        let my_id = sysinfo::hostname();
        let inner = self.ctx.lock().unwrap();
        if inner.devices.is_empty() {
            return "(還沒有任何裝置資料)".to_string();
        }
        let mut ids: Vec<&String> = inner.devices.keys().collect();
        ids.sort();

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
                // mode 縮寫成單一字元，省表格寬度：s=server、c=client、
                // -=standalone（還沒設過 mode 的預設值）。查不到對應值（未來
                // 版本間協定不一致）就照原字串顯示，不讓表格憑空消失一欄。
                let mode = match entry.report.mode.as_str() {
                    "server" => "s".to_string(),
                    "client" => "c".to_string(),
                    "standalone" => "-".to_string(),
                    other => other.to_string(),
                };
                [
                    id_cell,
                    entry.report.ip.clone(),
                    entry.report.os.clone(),
                    entry.report.version.clone(),
                    yes_no(entry.report.tailscale),
                    mode,
                    sysinfo::format_uptime(entry.report.device_uptime_secs),
                    sysinfo::format_uptime(entry.report.app_uptime_secs),
                    sysinfo::format_disk_usage(entry.report.disk_free_bytes, entry.report.disk_total_bytes),
                    if alive { "*".to_string() } else { String::new() },
                ]
            })
            .collect();
        render_table(&headers, &rows)
    }
```

換成：

```rust
    /// 組出目前每一列的 (row key＝裝置 id, 每一欄的文字)，`table_text()`／
    /// `tui_snapshot()`／`web_snapshot()` 共用同一份。
    fn rows(&self) -> Vec<(String, Vec<String>)> {
        let my_id = sysinfo::hostname();
        let inner = self.ctx.lock().unwrap();
        let mut ids: Vec<&String> = inner.devices.keys().collect();
        ids.sort();
        ids.into_iter()
            .map(|id| {
                let entry = &inner.devices[id];
                let alive = entry.last_seen.elapsed() < ALIVE_TTL;
                let id_cell = if entry.report.id == my_id {
                    format!("* {}", entry.report.id)
                } else {
                    format!("  {}", entry.report.id)
                };
                // mode 縮寫成單一字元，省表格寬度：s=server、c=client、
                // -=standalone（還沒設過 mode 的預設值）。查不到對應值（未來
                // 版本間協定不一致）就照原字串顯示，不讓表格憑空消失一欄。
                let mode = match entry.report.mode.as_str() {
                    "server" => "s".to_string(),
                    "client" => "c".to_string(),
                    "standalone" => "-".to_string(),
                    other => other.to_string(),
                };
                let cells = vec![
                    id_cell,
                    entry.report.ip.clone(),
                    entry.report.os.clone(),
                    entry.report.version.clone(),
                    yes_no(entry.report.tailscale),
                    mode,
                    sysinfo::format_uptime(entry.report.device_uptime_secs),
                    sysinfo::format_uptime(entry.report.app_uptime_secs),
                    sysinfo::format_disk_usage(entry.report.disk_free_bytes, entry.report.disk_total_bytes),
                    if alive { "*".to_string() } else { String::new() },
                ];
                (id.clone(), cells)
            })
            .collect()
    }

    fn table_text(&self) -> String {
        let rows = self.rows();
        if rows.is_empty() {
            return "(還沒有任何裝置資料)".to_string();
        }
        let row_values: Vec<Vec<String>> = rows.into_iter().map(|(_, cells)| cells).collect();
        render_table(&HEADERS, &row_values)
    }

    /// 給 TUI 用的逐格 snapshot，`gui.rs` 的 `with_device` 會呼叫這個。
    pub(crate) fn tui_snapshot(&self) -> TableSnapshot {
        self.tui_diff.lock().unwrap().snapshot(&HEADERS, self.rows())
    }

    /// 給 web SSE 用的逐格 snapshot，`web.rs` 呼叫後轉成 JSON 推播。
    pub(crate) fn web_snapshot(&self) -> TableSnapshot {
        self.web_diff.lock().unwrap().snapshot(&HEADERS, self.rows())
    }
```

在 `ALIVE_TTL` 常數定義之後新增模組層級常數：

```rust
const HEADERS: [&str; 10] =
    ["  id", "ip", "os", "version", "tailscale", "mode", "device uptime", "app uptime", "disk", "alive"];
```

- [ ] **Step 4: `render_table()` 簽名從固定陣列改成 `Vec<String>`**

把：

```rust
fn render_table(headers: &[&str], rows: &[[String; 10]]) -> String {
```

改成：

```rust
fn render_table(headers: &[&str], rows: &[Vec<String>]) -> String {
```

函式內容不用改。

- [ ] **Step 5: 執行既有測試，確認沒有回歸**

Run: `cargo test --lib device`
Expected: `recent_report_counts_as_alive`／`stale_report_counts_as_offline`／`report_just_inside_ttl_still_alive` 三個既有測試全部 PASS（這幾個測試呼叫的是 `status()`，跟這次改動的 `table_text()`/`rows()` 無關，但用來確認整個 struct 改動沒有破壞既有行為）。

- [ ] **Step 6: 手動確認 CLI 輸出沒有變**

Run: `cargo build`，然後互動模式下跑 `device list`，確認表格格式（欄寬、`* `/`  ` 前綴、`alive` 欄）跟修改前一致。

- [ ] **Step 7: Commit**

```bash
git add src/plugins/device.rs
git commit -m "$(cat <<'EOF'
device plugin 接上 table_diff，新增 tui_snapshot/web_snapshot

跟 global plugin 同一次改動的做法一致：table_text()/render_table() 輸出
格式不變，組 row 資料的邏輯抽成共用的 rows()。
EOF
)"
```

---

### Task 4: TUI（`gui.rs`）逐格閃爍呈現

**Files:**
- Modify: `src/gui.rs`（import、新增兩個 `with_*` helper、新增 `flash_style`/`render_table_snapshot`、面板繪製迴圈新增分支）

**Interfaces:**
- Consumes: `GlobalPlugin::tui_snapshot`/`DevicePlugin::tui_snapshot`（Task 2/3）、`TableSnapshot`（Task 1）。
- Produces: 無下游任務依賴這個 task 的產物。

- [ ] **Step 1: import 新增型別**

把檔案開頭的：

```rust
use crate::plugins::{NotepadPlugin, QrPlugin};
```

改成：

```rust
use crate::plugins::{DevicePlugin, GlobalPlugin, NotepadPlugin, QrPlugin, TableSnapshot};
```

- [ ] **Step 2: 新增 `with_global`/`with_device`（緊接在 `with_qr` 之後）**

在 `with_qr` 函式（第 37-42 行）後面加：

```rust
/// 跟 `with_notepad`同一個套路，借出 `global` plugin 的具體型別可變參考，
/// 讓 GUI 能呼叫 `tui_snapshot()` 拿逐格資料。
fn with_global<R>(shell: &Arc<Mutex<Shell>>, f: impl FnOnce(&mut GlobalPlugin) -> R) -> Option<R> {
    let mut sh = lock_shell(shell);
    let plugin = sh.plugin_mut("global")?;
    let global = plugin.as_any_mut().downcast_mut::<GlobalPlugin>()?;
    Some(f(global))
}

/// 跟 `with_global` 同一個套路，借出 `device` plugin。
fn with_device<R>(shell: &Arc<Mutex<Shell>>, f: impl FnOnce(&mut DevicePlugin) -> R) -> Option<R> {
    let mut sh = lock_shell(shell);
    let plugin = sh.plugin_mut("device")?;
    let device = plugin.as_any_mut().downcast_mut::<DevicePlugin>()?;
    Some(f(device))
}
```

- [ ] **Step 3: 新增 `flash_style`/`render_table_snapshot`（緊接在 `with_device` 之後）**

```rust
/// 逐格閃爍：剛好偵測到變化的那一刻先用白底，接著兩段更暗的灰階模擬淡出，
/// 1 秒後完全恢復預設樣式。只用 ratatui 內建的具名顏色（不用 truecolor），
/// 不是每個終端機都保證支援任意 RGB 顏色。
fn flash_style(elapsed: Duration) -> Style {
    if elapsed < Duration::from_millis(333) {
        Style::default().bg(Color::White).fg(Color::Black)
    } else if elapsed < Duration::from_millis(666) {
        Style::default().bg(Color::Gray).fg(Color::Black)
    } else if elapsed < Duration::from_secs(1) {
        Style::default().bg(Color::DarkGray).fg(Color::White)
    } else {
        Style::default()
    }
}

/// 把一份 `TableSnapshot` 畫成對齊好的逐格 `Line`：表頭／分隔線排版邏輯跟
/// `global.rs`/`device.rs` 的 `render_table` 一致，每一格再依 `flash_style`
/// 套上樣式。
fn render_table_snapshot(snapshot: &TableSnapshot) -> Vec<Line<'static>> {
    let widths = snapshot.column_widths();
    let pad = |s: &str, w: usize| format!("{s}{}", " ".repeat(w.saturating_sub(UnicodeWidthStr::width(s))));

    let mut header_spans = Vec::new();
    for (i, (h, w)) in snapshot.headers.iter().zip(&widths).enumerate() {
        if i > 0 {
            header_spans.push(Span::raw(" | "));
        }
        header_spans.push(Span::raw(pad(h, *w)));
    }
    let separator = widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("-+-");

    let mut lines = vec![Line::from(header_spans), Line::raw(separator)];
    for row in &snapshot.rows {
        let mut spans = Vec::new();
        for (i, (cell, w)) in row.cells.iter().zip(&widths).enumerate() {
            if i > 0 {
                spans.push(Span::raw(" | "));
            }
            spans.push(Span::styled(pad(&cell.text, *w), flash_style(cell.changed_at.elapsed())));
        }
        lines.push(Line::from(spans));
    }
    lines
}
```

- [ ] **Step 4: 面板繪製迴圈新增 `"global"`/`"device"` 分支**

在 `} else if name == "notepad" {` 分支結束的 `}`（原本第 1095 行）跟
`} else if let Some(text) = lock_shell(shell).plugin_panel_text(name) {`
（原本第 1096 行）之間，插入：

```rust
                } else if name == "global" || name == "device" {
                    let inner = block.inner(rect);
                    frame.render_widget(block, rect);
                    if inner.height > 0 {
                        let body_height = inner.height.saturating_sub(1);
                        let hint_area = Rect { x: inner.x, y: inner.y + body_height, width: inner.width, height: 1 };
                        let body_area = Rect { x: inner.x, y: inner.y, width: inner.width, height: body_height };
                        let snapshot = if name == "global" {
                            with_global(shell, |g| g.tui_snapshot())
                        } else {
                            with_device(shell, |d| d.tui_snapshot())
                        };
                        let lines: Vec<Line> = match snapshot.filter(|s| !s.rows.is_empty()) {
                            Some(snapshot) => render_table_snapshot(&snapshot),
                            None => {
                                let text = lock_shell(shell).plugin_panel_text(name).unwrap_or_default();
                                text.lines().map(|l| Line::raw(l.to_string())).collect()
                            }
                        };
                        let start = lines.len().saturating_sub(body_area.height as usize);
                        let visible: Vec<Line> = lines[start..].to_vec();
                        frame.render_widget(Paragraph::new(visible), body_area);
                        let hint_style = Style::default().fg(Color::DarkGray);
                        frame.render_widget(Paragraph::new(PANEL_HINT).style(hint_style), hint_area);
                    }
```

（保留原本 `} else if let Some(text) = ... {` 那個通用分支不動，給其他 panel 繼續用。）

- [ ] **Step 5: 編譯確認**

Run: `cargo build`
Expected: 成功，無錯誤。`cargo clippy` 如果專案有在用，順便跑一次確認沒有新增的 lint 警告。

- [ ] **Step 6: 手動驗證（TUI）**

1. `cargo run`，進 GUI 模式（`mode gui` 或啟動時已經是 GUI）。
2. 打開 `device` panel（`panel show device` 或既有開啟方式）。
3. 讓另一個裝置的回報資料有變化（或本機重開一次程式讓 uptime 重新回報），觀察對應那一格是否先變白底、再經過灰階、大約 1 秒後恢復正常，其餘沒變的格子維持原樣不受影響。
4. 同樣方式驗證 `global` panel（需要有至少一個跨 domain 的裝置在回報資料；如果目前環境沒有設定 domain/bridge，確認至少「沒有資料時顯示原本的提示文字」這個 fallback 正常運作）。

- [ ] **Step 7: Commit**

```bash
git add src/gui.rs
git commit -m "$(cat <<'EOF'
TUI：global/device panel 逐格資料變化時閃爍提示

用既有的 with_notepad/with_qr 向下轉型套路拿 tui_snapshot()，白底閃一下
後分兩段灰階淡出，約 1 秒後恢復正常樣式。
EOF
)"
```

---

### Task 5: Web（`web.rs`）推送結構化 JSON snapshot

**Files:**
- Modify: `src/web.rs`（import、`panel_text_for`、新增 helper）

**Interfaces:**
- Consumes: `GlobalPlugin::web_snapshot`/`DevicePlugin::web_snapshot`（Task 2/3）、`TableSnapshot::to_json`（Task 1）。
- Produces: `panel_text_for` 對 `name == "global" || name == "device"` 回傳的字串現在是
  `JsonTableSnapshot` 的 JSON（`{"headers": [...], "rows": [{"cells": [{"text": "...", "changed": true|false}, ...]}, ...]}`），
  是 Task 6（`frontend.html`）要解析的格式。

- [ ] **Step 1: import 新增型別**

把：

```rust
use crate::plugins::{
    list_dir, make_dir, remove, rename_path, safe_music_copy_path, safe_storage_path, walk_with_hashes,
    DEFAULT_NOTEPAD_FILE, MUSIC_DIR, NOTEPAD_DIR, STORAGE_DIR, SUBTITLE_LANG_PRIORITY,
};
```

改成：

```rust
use crate::plugins::{
    list_dir, make_dir, remove, rename_path, safe_music_copy_path, safe_storage_path, walk_with_hashes,
    DevicePlugin, GlobalPlugin, DEFAULT_NOTEPAD_FILE, MUSIC_DIR, NOTEPAD_DIR, STORAGE_DIR, SUBTITLE_LANG_PRIORITY,
};
```

- [ ] **Step 2: `panel_text_for` 新增分支＋新增 helper**

把（原本第 419-427 行）：

```rust
fn panel_text_for(shell: &Mutex<Shell>, output: &OutputBuffer, name: &str) -> String {
    if name == "output" {
        let lines = output.all();
        let start = lines.len().saturating_sub(OUTPUT_TAIL_LINES);
        lines[start..].join("\n")
    } else {
        lock_shell(shell).plugin_panel_text(name).unwrap_or_default()
    }
}
```

改成：

```rust
fn panel_text_for(shell: &Mutex<Shell>, output: &OutputBuffer, name: &str) -> String {
    if name == "output" {
        let lines = output.all();
        let start = lines.len().saturating_sub(OUTPUT_TAIL_LINES);
        lines[start..].join("\n")
    } else if name == "global" || name == "device" {
        table_snapshot_json(shell, name)
    } else {
        lock_shell(shell).plugin_panel_text(name).unwrap_or_default()
    }
}

/// `global`／`device` 這兩個 panel 在 web 這邊不是推純文字，是推結構化的
/// JSON（表頭＋每一格的文字＋「這一格剛剛變了沒」），讓前端能逐格套用閃爍
/// 效果，見 `frontend.html` 的 `renderTableSnapshot`。跟 `gui.rs` 的
/// `with_global`／`with_device` 是同一個「向下轉型拿具體型別」的做法，只是
/// 這裡要的是 web 專用的那份 `web_snapshot()`（TUI／web 各自獨立的比對狀態，
/// 見 `table_diff::RowDiffTracker` 的說明）。
fn table_snapshot_json(shell: &Mutex<Shell>, name: &str) -> String {
    let mut sh = lock_shell(shell);
    let snapshot = if name == "global" {
        sh.plugin_mut("global").and_then(|p| p.as_any_mut().downcast_mut::<GlobalPlugin>()).map(|g| g.web_snapshot())
    } else {
        sh.plugin_mut("device").and_then(|p| p.as_any_mut().downcast_mut::<DevicePlugin>()).map(|d| d.web_snapshot())
    };
    drop(sh);
    match snapshot {
        Some(snapshot) => serde_json::to_string(&snapshot.to_json()).unwrap_or_default(),
        None => String::new(),
    }
}
```

（`broadcast_ticker`／`sse_frame`／`panel_stream` 完全不用改：它們只是把
`panel_text_for` 回傳的字串當成不透明內容比對/快取/推播，內容是純文字表格
還是 JSON 物件字串對它們來說沒有差別。）

- [ ] **Step 3: 編譯確認**

Run: `cargo build`
Expected: 成功。

- [ ] **Step 4: 手動驗證（先確認資料格式，不用等前端做完）**

1. `cargo run` 啟動（確認 web server 有一起啟動，看 log 或直接連下面的網址）。
2. 瀏覽器（或 `curl -N`）打開 `http://127.0.0.1:9759/api/panel/device/stream`，確認收到的 `data:` 內容是「JSON 字串包著另一個 JSON 字串」——外層是 SSE 既有的 `sse_frame` 編碼，內層解開後應該長得像
   `{"headers":["  id","ip",...],"rows":[{"cells":[{"text":"...","changed":true}, ...]}]}`。
3. 確認 `global` 也一樣，且沒有設定 domain/bridge 時（`rows` 應為空陣列）不會噴錯。

- [ ] **Step 5: Commit**

```bash
git add src/web.rs
git commit -m "$(cat <<'EOF'
web：global/device panel 推送結構化 JSON snapshot

其餘 panel（含 sse_frame/broadcast_ticker 的既有快取比對邏輯）完全不變，
只有這兩個 panel 的 panel_text_for 換成序列化 TableSnapshot。
EOF
)"
```

---

### Task 6: `frontend.html` 逐格建表＋閃爍動畫

**Files:**
- Modify: `src/web/frontend.html`（CSS、新增 JS 函式、`openPanel` 內的幾處）

**Interfaces:**
- Consumes: Task 5 的 JSON 格式（`{"headers": [...], "rows": [{"cells": [{"text","changed"}]}]}`）。
- Produces: 無下游任務依賴。

- [ ] **Step 1: 新增 CSS**

在 `.storage-table button:hover { color: #d8dee9; }`（第 625 行）之後、
`</style>`（第 626 行）之前，加入：

```css
  .data-table-body { display: flex; flex-direction: column; height: 100%; overflow: hidden; }
  .data-table-empty { padding: 8px; color: #9aa4b8; }
  .data-table { flex: 1; overflow: auto; border-collapse: collapse; width: 100%; font-size: 12px; }
  .data-table th, .data-table td { padding: 4px 10px; border-top: 1px solid #262a33; white-space: nowrap; text-align: left; }
  .data-table th { color: #9aa4b8; font-weight: normal; border-top: none; border-bottom: 1px solid #3a4150; }
  @keyframes cellFlash {
    from { background-color: rgba(255, 255, 255, 0.35); }
    to { background-color: transparent; }
  }
  .cell-flash { animation: cellFlash 1s ease-out; }
```

- [ ] **Step 2: 新增 `renderTableSnapshot` 函式**

在 `function openPanel(name) {`（第 1600 行）之前加入：

```js
  // `global`／`device` panel 專用：把後端送來的逐格 snapshot（見 `web.rs` 的
  // `table_snapshot_json`）畫成一個真正的 `<table>`。用 `textContent` 逐格
  // 填值（不是拼 HTML 字串），這樣任何欄位內容（`global` 的資料有一部分是
  // 其他 domain 透過公開 MQTT broker 回報的字串，不能假設乾淨）都會被瀏覽器
  // 自動跳脫，不會被當成 HTML 解析。`thead`/`tbody` 每次都整個重新建立，
  // 所以剛變化的格子掛的 `cell-flash` class 每次都是全新 DOM node，CSS
  // 動畫自然會重新播放一次，不用額外處理「同一個 class 不會重播」的問題。
  function renderTableSnapshot(ui, snapshot) {
    const hasRows = snapshot.rows.length > 0;
    ui.empty.style.display = hasRows ? "none" : "";
    ui.table.style.display = hasRows ? "" : "none";

    ui.thead.innerHTML = "";
    const headRow = document.createElement("tr");
    for (const header of snapshot.headers) {
      const th = document.createElement("th");
      th.textContent = header;
      headRow.appendChild(th);
    }
    ui.thead.appendChild(headRow);

    ui.tbody.innerHTML = "";
    for (const row of snapshot.rows) {
      const tr = document.createElement("tr");
      for (const cell of row.cells) {
        const td = document.createElement("td");
        td.textContent = cell.text;
        if (cell.changed) td.className = "cell-flash";
        tr.appendChild(td);
      }
      ui.tbody.appendChild(tr);
    }
  }

```

- [ ] **Step 3: `openPanel` 內宣告 `dataTableUi`**

把：

```js
    let body;
    let musicUi = null;
    let shellUi = null;
    let notepadUi = null;
    let storageUi = null;
```

改成：

```js
    let body;
    let musicUi = null;
    let shellUi = null;
    let notepadUi = null;
    let storageUi = null;
    let dataTableUi = null;
```

- [ ] **Step 4: 新增 `global`/`device` 的面板內容分支**

把（第 2559-2566 行附近）：

```js
      refresh();

      storageUi = { container };
    } else {
      body = document.createElement("pre");
      body.className = "panel-body";
      body.textContent = "(等待資料...)";
    }
```

改成：

```js
      refresh();

      storageUi = { container };
    } else if (name === "global" || name === "device") {
      panel.style.width = "640px";
      panel.style.height = "320px";

      const container = document.createElement("div");
      container.className = "data-table-body";
      const table = document.createElement("table");
      table.className = "data-table";
      table.style.display = "none";
      const thead = document.createElement("thead");
      const tbody = document.createElement("tbody");
      table.appendChild(thead);
      table.appendChild(tbody);
      const empty = document.createElement("div");
      empty.className = "data-table-empty";
      empty.textContent = "(等待資料...)";
      container.appendChild(table);
      container.appendChild(empty);

      dataTableUi = { container, table, thead, tbody, empty };
    } else {
      body = document.createElement("pre");
      body.className = "panel-body";
      body.textContent = "(等待資料...)";
    }
```

- [ ] **Step 5: `panel.appendChild` 選擇邏輯加上 `dataTableUi`**

把：

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

改成：

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
        : dataTableUi
        ? dataTableUi.container
        : body
    );
```

- [ ] **Step 6: SSE `onmessage` 分流到 `renderTableSnapshot`**

把（第 2659-2675 行附近）：

```js
    let es = null;
    if (name !== "player" && name !== "shell" && name !== "notepad" && name !== "storage") {
      es = new EventSource(`/api/panel/${encodeURIComponent(name)}/stream`);
      es.onmessage = (e) => {
        // 使用者如果往上捲去看舊內容，不要硬把它拉回底部；只有原本就在（或接近）
        // 最底下時，新內容進來才跟著捲到最新，跟終端機 tail 的習慣一致。
        const wasNearBottom = body.scrollHeight - body.scrollTop - body.clientHeight < 24;
        try {
          body.textContent = JSON.parse(e.data);
        } catch (_err) {
          body.textContent = e.data;
        }
        if (wasNearBottom) {
          body.scrollTop = body.scrollHeight;
        }
      };
    }
```

改成：

```js
    let es = null;
    if (name !== "player" && name !== "shell" && name !== "notepad" && name !== "storage") {
      es = new EventSource(`/api/panel/${encodeURIComponent(name)}/stream`);
      es.onmessage = (e) => {
        if (dataTableUi) {
          // `web.rs` 的 `table_snapshot_json` 送的是「JSON 物件字串」，經過
          // `sse_frame` 又整個包了一層 JSON 字串編碼，所以要解兩層。
          let snapshot;
          try {
            snapshot = JSON.parse(JSON.parse(e.data));
          } catch (_err) {
            return;
          }
          renderTableSnapshot(dataTableUi, snapshot);
          return;
        }
        // 使用者如果往上捲去看舊內容，不要硬把它拉回底部；只有原本就在（或接近）
        // 最底下時，新內容進來才跟著捲到最新，跟終端機 tail 的習慣一致。
        const wasNearBottom = body.scrollHeight - body.scrollTop - body.clientHeight < 24;
        try {
          body.textContent = JSON.parse(e.data);
        } catch (_err) {
          body.textContent = e.data;
        }
        if (wasNearBottom) {
          body.scrollTop = body.scrollHeight;
        }
      };
    }
```

- [ ] **Step 7: 手動驗證（web，瀏覽器實測）**

1. `cargo build && cargo run`，瀏覽器開 `http://127.0.0.1:9759`。
2. 打開 `device` panel，確認畫面是一個有表頭的 `<table>`，不是純文字 `<pre>`。
3. 讓某個欄位的資料改變（例如另一台機器回報新的 uptime），確認對應的 `<td>` 出現白色→透明的淡出動畫（開瀏覽器 DevTools 觀察該 `<td>` 短暫多了 `cell-flash` class）。
4. 開兩個瀏覽器分頁看同一個 `device` panel，確認兩邊都會在資料變化時同時閃爍（驗證兩個分頁共用同一份 `web_diff` 狀態，行為一致）。
5. 打開 `global` panel，確認沒有資料時顯示「(等待資料...)」或後端的提示文字，不會顯示空表格或報錯（打開瀏覽器 Console 確認無 JS 例外）。
6. 確認 `output`/`system`/`weather` 等其他 panel 完全沒有受影響（還是原本的 `<pre>` 純文字捲動內容）。

- [ ] **Step 8: Commit**

```bash
git add src/web/frontend.html
git commit -m "$(cat <<'EOF'
web 前端：global/device panel 改成逐格建表＋變化閃爍動畫

沿用既有 storage 面板逐格建 DOM 的做法（td.textContent，不用 innerHTML
拼字串，避免其他 domain 回報的字串被當 HTML 解析）；剛變化的格子套
cell-flash class，白色半透明背景 1 秒內用 CSS 動畫淡出。
EOF
)"
```

---

### Task 7: 端對端驗證

**Files:** 無新增/修改檔案，純驗證。

- [ ] **Step 1: 全專案編譯與既有測試**

Run: `cargo build && cargo test`
Expected: 全部成功，沒有新的編譯警告或測試失敗。

- [ ] **Step 2: TUI 端對端**

依照 Task 4 Step 6 的手動驗證步驟，在真實的多機環境（至少兩台機器互相回報，或至少一台加另一台模擬的 domain 資料）下重跑一次，確認 `device`／`global` 兩個 panel 在 TUI 模式下都能正確閃爍，且切換 panel／`panel show`/`panel hide` 不會導致 crash 或殘留舊的閃爍狀態。

- [ ] **Step 3: Web 端對端**

依照 Task 6 Step 7 的手動驗證步驟重跑一次，額外確認：
- 重新整理瀏覽器分頁（重新建立 SSE 連線）不會誤把「初次拿到的完整資料」當成變化而整批閃爍一次（因為這是新分頁第一次看到這些 row，設計上就是「第一次出現算變化」，所以整批閃一次是**預期行為**，不是 bug——這點在驗證時不要誤判成問題）。
- `global clear` 之後，之前顯示過的裝置資料消失，之後重新收到同一台裝置的回報時，那一列會整列重新閃一次（符合「消失後重新出現視同第一次出現」的設計）。

- [ ] **Step 4: CLI 輸出回歸確認**

Run: 互動模式下依序執行 `global list`／`global status`／`device list`／`device status`，人工比對輸出格式（欄位、對齊、提示文字）跟這次改動之前完全一致。

（這個 task 純粹是驗證，沒有新的程式碼異動，不需要 commit。）
