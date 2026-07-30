# global／device panel：欄位資料跟上次不一樣時閃爍提示

日期：2026-07-30

## 目的

`global`／`device` panel 目前不管是 TUI 還是 web，都是整段純文字表格，每次
重繪／推播都是整段蓋掉，使用者很難注意到「剛剛哪一格資料變了」（例如某台裝置
的 IP 換了、alive 狀態翻轉、新裝置突然出現）。這次要加上：**任何一格資料跟
上一次看到的不一樣，就短暫閃爍提示（白底閃一下、約 1 秒內淡出）**，包含這一
row 是第一次出現的情況（整列都算「跟上次不一樣」，一起閃）。

## 變化判定

- 「這一格有變化」＝「這次的值 ≠ 這一欄上次記錄的值」；如果上次根本沒有值
  （這個 row 第一次出現），一律視為「≠」，不特別處理——新出現的 row 因此會
  整列一起閃，不需要額外邏輯。
- row 的識別鍵：`global` 用 `domain/id`（已經是唯一值）、`device` 用 `id`
  （`inner.devices` 的 key，本來就唯一）。
- TUI 跟 web 是兩個獨立、重繪頻率不同的 renderer（TUI 最快 200ms 一次、web
  固定 300ms 一個 tick）。如果共用同一份「上次看到的值」，先讀到變化的那一邊
  會把狀態更新掉，另一邊接著讀就會誤判成「沒變」而漏閃。因此每個 plugin
  各自維護兩份獨立的比對狀態：一份給 TUI 用、一份給 web 用，兩邊互不影響。
- row 消失（裝置離線很久、或 `global clear`）不特別處理，讓對應的比對狀態
  自然被下一次重建時濾掉；之後同一個 key 重新出現，等同「第一次出現」，會
  整列閃一次。

## 共用機制：`src/plugins/table_diff.rs`

`global.rs` 跟 `device.rs` 的表格結構幾乎同型（都是 `Vec<String>` 組成的
rows，最後排版成對齊的文字表格），這次兩邊都要做，值得抽出一個小的共用
模組，避免同一套 diff 邏輯寫兩份：

- `TableCell { text: String, changed_at: Instant }`：`changed_at` 是這一格
  「最近一次偵測到變化」的時間點。因為「第一次出現也算變化」，`Table
  Snapshot` 裡出現的每一格必定至少變化過一次（就是它被建立的那一刻），所以
  不需要 `Option`——一個 key 在 tracker 內部完全沒有記錄，等同它還沒出現在
  任何一次 snapshot 裡，也就不會有對應的 `TableCell` 需要處理。
- `TableRow { key: String, cells: Vec<TableCell> }`、
  `TableSnapshot { headers: Vec<String>, rows: Vec<TableRow> }`。
- `RowDiffTracker`：內部是 `HashMap<key, Vec<(String value, Instant
  changed_at)>>`，代表「這個 tracker 上次看到的樣子」。提供一個方法，輸入
  目前的 `headers` 跟 `rows: Vec<(key, Vec<String>)>`，輸出 `TableSnapshot`
  ——每次呼叫都用「目前存在的 key」整個重建一份新的內部 map（沿用還存在的
  key 的舊值/時間，其餘捨棄），這樣消失的 row 自然被清掉，不需要另外寫
  清除邏輯。

`GlobalPlugin`／`DevicePlugin` 各自新增兩個欄位：`tui_diff:
Mutex<RowDiffTracker>`、`web_diff: Mutex<RowDiffTracker>`，以及兩個新方法
（例如 `tui_snapshot(&self) -> TableSnapshot`／`web_snapshot(&self) ->
TableSnapshot`），內部沿用原本 `table_text()` 組 row 資料的邏輯（`merged_
global_view`／`inner.devices` 那段），只是最後不呼叫 `render_table()` 排成
字串，改丟進對應的 tracker。

**原本的 `table_text()`／`render_table()` 完全不動**，繼續給 CLI 的 `list`
指令、以及 `Plugin::panel_text()`（純文字版本）使用——`plugin_panel_text()`
還有其他呼叫端（`gui.rs` notepad 內嵌顯示其他 plugin 內容、`clock`／
`storage` 的既有測試），這次不去牽動這些。

## TUI 呈現（`gui.rs`）

- 比照現有 `with_notepad`／`with_qr` 的向下轉型寫法（`as_any_mut::<T>()`），
  新增 `with_global`／`with_device` 兩個 helper。
- panel 繪製迴圈原本 `else if let Some(text) = ... plugin_panel_text(name)`
  那個通用分支之前，新增兩個分支：`name == "global"` 跟 `name == "device"`
  ，呼叫對應的 `tui_snapshot()` 拿到 `TableSnapshot`，用一個共用函式把它畫成
  逐格 `Span`＋`Style`（畫法上比照 `highlight_code_line` 那樣組 `Vec<Span>`
  再包成 `Line`）。
- 淡出效果用 3 段離散階梯模擬（避免依賴不保證所有終端機都支援的
  truecolor，只用 ratatui 內建的具名顏色）：
  - 0–333ms：白底黑字
  - 333–666ms：灰底黑字
  - 666–1000ms：深灰底白字
  - 1 秒後：恢復預設樣式（無背景色）
- 其餘版面（捲動、`PANEL_HINT` 提示列位置）跟現有通用分支一致，只是內容從
  `Line::raw` 換成逐格組出來的 `Line`。

## Web 呈現

- `web.rs` 的 `panel_text_for`／`broadcast_ticker`：針對 `global`／`device`
  這兩個名字，改成呼叫對應的 `web_snapshot()`，序列化成 JSON 字串（結構對應
  `TableSnapshot`，但 `changed_at` 換成一個布林 `changed`——`true` 代表「這
  一格在這次呼叫裡被偵測到剛變化」，也就是 `RowDiffTracker` 這次更新時真的
  把 `changed_at` 設成 `Instant::now()` 的那些格子）當作 SSE payload；其餘
  panel 維持原本整段純文字的行為不變。
- `frontend.html` 新增一個共用函式（例如 `renderTableSnapshot(container,
  snapshot)`），`global`／`device` 的 SSE `onmessage` 都改呼叫它，比照
  `storage` 面板既有的做法（`frontend.html:2462` 附近）用 DOM API 逐格建
  `<table>`：每個 `<td>` 用 `td.textContent = cell.text`（自動跳脫，不用
  自己處理 HTML escape——`global` 的資料有一部分來自其他 domain 透過公開
  MQTT broker 回報的字串，不能假設內容乾淨，用 `textContent` 而不是拼
  HTML 字串／`innerHTML` 是必要的，不只是圖方便）。
- 有變化的格子（`cell.changed === true`）多加一個 `class="cell-flash"`。因為
  每次都是整個 `<tbody>` 重新建立（沿用 `storage` 面板 `tbody.innerHTML =
  ""` 的做法），新插入的 DOM node 上掛的 CSS 動畫每次都會重新從頭播放，不用
  額外處理「同一個 class 不會重播動畫」的問題。
- CSS：新增 `@keyframes cellFlash`，從白色半透明背景（例如
  `rgba(255,255,255,0.35)`）在 1 秒內用 `ease-out` 淡出到透明，`.cell-flash`
  套用這個動畫。跟目前固定深色主題（面板背景 `#1b1e26`）對比清楚。

## 範圍

- 新增 `src/plugins/table_diff.rs`（`TableCell`/`TableRow`/`TableSnapshot`/
  `RowDiffTracker`），並在 `src/plugins/mod.rs` 用 `pub(crate) use` 匯出給
  `global.rs`／`device.rs` 使用。
- `src/plugins/global.rs`：新增 `tui_diff`/`web_diff` 欄位與
  `tui_snapshot`/`web_snapshot` 方法，沿用既有 row 組裝邏輯。
- `src/plugins/device.rs`：同上。
- `src/gui.rs`：新增 `with_global`/`with_device`，新增 `name == "global"`／
  `name == "device"` 繪製分支與共用的 snapshot→`Span` 畫法函式。
- `src/web.rs`：`panel_text_for`／`broadcast_ticker` 針對這兩個名字改送
  JSON snapshot。
- `src/web/frontend.html`：新增 `renderTableSnapshot`、`global`/`device`
  SSE handler 改走這個路徑、新增 `.cell-flash`／`@keyframes cellFlash`
  CSS。

## 不做的事

- 不動 `weather.rs`／`music.rs` 等其他表格型 panel——這兩個目前沒人要求，
  等真的需要再套用同一套 `table_diff` 機制。
- 不改變 `table_text()`／`render_table()`／CLI `list` 指令的輸出格式。
- 不做「跨瀏覽器分頁同步閃爍時間點」之類的額外保證——`web_diff` 是
  process 內單一份狀態，多個 SSE 連線收到的是同一份 snapshot，行為自然
  一致，不需要額外設計。
- 不特別處理「同一格短時間內連續變化好幾次」的疊加效果，每次變化都是重新
  設一次 `changed_at`／`changed`，视觉上就是重新閃一次，不刻意去疊加或
  加速。

## 測試

- `table_diff.rs` 的 `RowDiffTracker` 是純邏輯、不碰任何 I/O，加
  `#[cfg(test)]` 單元測試：全新 key 第一次出現 → 整列 `changed_at` 都設成
  這次呼叫的時間點；同一個 key 下一次呼叫值不變 → 對應格子 `changed_at`
  維持原本的時間點不動（不會被誤判成「又變了」）；同一個 key 只有其中一欄
  變了 → 只有那一格更新 `changed_at`；原本存在的 key 這次沒出現在輸入裡 →
  之後這個 key 重新出現時視同全新（跟第一次出現行為一致）。
- 其餘（`gui.rs`／`web.rs`／`frontend.html` 的呈現效果）沒有既有的自動化
  測試可以掛，跟這幾個檔案一直以來的做法一致：靠 `cargo build`/`cargo
  test` 確認不回歸，加上手動操作驗證（TUI 模式看 global/device panel 換
  一台裝置的資料再觀察閃爍效果；web 模式開瀏覽器開兩個分頁看同一個 panel
  ，確認兩邊都會閃、且切斷重連 SSE 不會誤閃舊資料）。
