# clock plugin 設計

日期：2026-07-25

## 目的

新增一個 `clock` plugin，panel 用 ASCII art 顯示現在時間（`hh:mm:ss`，不含日期）。每個
`hh:mm:ss` 不管內容是什麼，畫出來的寬度都要一樣。

## 字型

每個數字（`0`-`9`）用固定 **3 欄 × 5 列**的點陣字型畫出，`1`→`█`、`0`→空白：

```
███   █   ███  ███  █ █  ███  ███  ███  ███  ███
█ █   █     █    █  █ █  █    █      █  █ █  █ █
█ █   █   ███  ███  ███  ███  ███    █  ███  ███
█ █   █   █      █    █    █  █ █    █  █ █    █
███   █   ███  ███    █  ███  ███    █  ███  ███
0    1    2    3    4    5    6    7    8    9
```

冒號 `:` 是獨立的 **1 欄 × 5 列**圖案，只有第 2、4 列（0-indexed 為第 1、3 列）各畫一個
`█`，其餘留白：

```
[ ]   （亮）第 1 列: [█]，第 3 列: [█]
[ ]   （暗）全部留白
```

冒號每秒閃爍：`ts_secs % 2 == 0` 顯示、`== 1` 隱藏（用空白取代，欄位寬度不變）。

範例：`12:34:56`，冒號亮/暗兩種狀態：

```
冒號亮：
 █   ███     ███  █ █     ███  ███
 █     █  █    █  █ █  █  █    █
 █   ███     ███  ███     ███  ███
 █   █    █    █    █  █    █  █ █
 █   ███     ███    █     ███  ███

冒號暗（一秒後）：
 █   ███     ███  █ █     ███  ███
 █     █       █  █ █     █    █
 █   ███     ███  ███     ███  ███
 █   █         █    █       █  █ █
 █   ███     ███    █     ███  ███
```

兩者只有冒號那兩欄不同，其餘數字欄位完全一致（已用 script 驗證過）。

## 拼接規則

- 每個 glyph（數字 3 寬、冒號 1 寬）之間固定加 2 欄空白分隔（跟本文件「範例」一節
  實際 render 出來、已核准的寬度一致）。
- 因為每個數字 glyph 寬度固定是 3、冒號固定是 1，`hh:mm:ss` 不管實際數字是什麼，
  拼出來的總寬度永遠一樣：6 個數字（18 欄）+ 2 個冒號（2 欄）+ 7 個分隔欄
  （7 × 2 = 14 欄）= 34 欄 × 5 列。
- panel 太窄畫不下時不特別處理，直接讓現有的 `Paragraph` 裁切（`plugin_panel_text`
  的呼叫端沒有把 panel 寬度傳進來，plugin 本身也無從判斷）。

## 資料來源

不新增時間函式庫依賴。直接重用 `src/sysinfo.rs` 既有的
`pub fn local_hms(ts_secs: u64) -> String`（本地時區、24 小時制、`"HH:MM:SS"`，已有
測試對照系統 `date` 指令驗證過）：

1. `SystemTime::now().duration_since(UNIX_EPOCH)` 取得 epoch 秒 `ts`
2. `sysinfo::local_hms(ts)` → `"HH:MM:SS"` 字串
3. 逐字元查字型表，冒號依 `ts % 2` 決定亮/暗，組成 5 行 ASCII art

## Plugin 結構

新檔案 `src/plugins/clock.rs`：

- `const FONT`：`'0'..'9'` 對照表，每個數字 5 行、每行 3 個 `1`/`0` 的 bitmap 字串。
- `ClockPlugin`：無狀態的空結構，不存任何欄位（沒有計時器、沒有快取——
  `panel_text()` 每次呼叫都直接即時算，跟 `QrPlugin` 的「不快取、即時算」是同一套路）。
- `impl Plugin for ClockPlugin`：
  - `commands(&self) -> &'static [&'static str]`：回傳 `&[]`（沒有指令，純顯示，
    跟 `qr` plugin 一樣）。
  - `dispatch(&mut self, cmd: &str, ...) -> Result<()>`：`bail!("clock 不認得指令: {cmd}")`。
  - `panel_text(&self) -> Option<String>`：回傳上面「資料來源」步驟組出來的 ASCII art。
  - `manual_text(&self) -> &'static str`：說明沒有指令、冒號每秒閃爍、panel 太窄會被裁切。
  - `as_any_mut(&mut self) -> &mut dyn std::any::Any`：回傳 `self`（trait 要求，這裡
    不會真的被下轉型使用）。
- `ClockPlugin::new(_ctx: SharedContext) -> Self`：接受 `ctx` 參數只是為了跟其他
  plugin 建構子簽名一致（plugin registry 的 closure 型別需要），實際不使用、不儲存。

## 註冊

- `src/plugins/mod.rs`：加 `mod clock;` + `pub use clock::ClockPlugin;`
- `src/main.rs`：plugin 清單加一行
  `("clock", Box::new(|ctx| Box::new(ClockPlugin::new(ctx)) as Box<dyn Plugin>))`

## 測試

- **字型表正確性**：針對 `0`-`9` 每個數字比對預期的 5×3 bitmap。
- **寬度不變性**：對不同時間字串（如 `"00:00:00"` vs `"23:59:59"`）產生的 ASCII art，
  每一行字元數應相同。
- **冒號閃爍**：同一組數字、`ts` 為偶數 vs 奇數時，冒號欄位內容不同（一個是 `█`、
  一個是空白），其餘欄位不變。

## 不做的事

- 不顯示日期、不支援 12 小時制/AM-PM。
- 不處理 panel 太窄的降級顯示（直接裁切）。
- 不新增時間函式庫依賴（重用 `sysinfo::local_hms`）。
- 沒有任何互動指令（沒有 `commands()`，也沒有 GUI 快捷鍵）。
