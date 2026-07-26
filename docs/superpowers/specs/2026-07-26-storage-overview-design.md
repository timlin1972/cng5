# 儲存空間總覽設計

日期：2026-07-26

## 目的

「NAS 功能」拆解出的最後一個子項目：像 TrueNAS 的儲存儀表板一樣，看得到所有
裝置的硬碟用量，可以獨立做，跟「通用檔案總管」（`storage` plugin）關聯不大。
完全複用現有的 `device`/`global` 裝置回報＋彙整顯示機制，不新增 plugin、不
新增傳輸機制。

## 範圍

- 「儲存空間」指整台裝置的磁機空間（像 `df` 那樣量），不是只算 `storage/`
  資料夾自己用了多少——量測的路徑是程式執行目錄（也就是 `storage/`、
  `music/` 等資料夾實際所在）所在的那個檔案系統，不是寫死量測 `/` 或系統碟。
- 顯示在既有的 `device list`（同 domain）跟 `global list`（跨 domain）表格
  裡多加一欄，不做獨立的專門 plugin/面板。

## 資料蒐集

- `src/sysinfo.rs` 新增 `disk_usage(path: &Path) -> Option<(u64, u64)>`，
  回傳 `(可用 bytes, 總共 bytes)`；查不到（指令失敗、FFI 呼叫失敗）就回傳
  `None`，呼叫端填 `(0, 0)` 落地成 `DeviceReport` 的欄位，顯示端看到 0 就印
  `N/A`——跟 `os`/`version` 現有「查不到就填預設值，不能讓整包解析失敗」
  同一套精神。
- 實作沿用「不額外加依賴」原則，但跟這個檔案其他函式一樣依平台混用：
  - Unix：shell 出去跑 `df -k <path>`，解析輸出——跟現有 `arp_table`/
    `fetch_tailscale_ip` 那種「簡單外部指令 + 文字解析」的做法一致。
  - Windows：直接呼叫 `GetDiskFreeSpaceExW`（跟 `local_hms`/
    `device_uptime_secs` 現有的 raw FFI 風格一致，這個 API 只回傳幾個
    `u64` 型別的 out 參數，比 `FILETIME` 轉換簡單，不需要額外的複雜
    struct）。

## 資料結構與回報

- `DeviceReport`（`src/plugin.rs`）新增兩個欄位：
  `disk_free_bytes: u64`、`disk_total_bytes: u64`，都加
  `#[serde(default)]`（值是 0），理由跟 `os`/`version` 欄位現有的
  serde-default 處理一致：舊版裝置回報的 JSON 沒有這兩個 key 時，解析成 0
  而不是讓整筆（甚至整份清單）資料解析失敗。
- `system` plugin 的 `build_report()`（`src/plugins/system.rs`）呼叫
  `sysinfo::disk_usage(...)`，把結果放進 `DeviceReport` 的這兩個新欄位。
  這兩個欄位會自動跟著既有的回報機制走：同網域走
  `POST /api/device/register`，跨 domain 走 `global` plugin 既有的 MQTT
  發布/彙整，不需要另外改傳輸層。

## 顯示

- `src/plugins/device.rs`、`src/plugins/global.rs` 的表格目前都是寫死的
  `[String; 9]` 陣列（兩邊各自獨立一份 `render_table`，沒有共用），各自
  改成 `[String; 10]`，新增一欄放在 `app uptime` 之後、`alive` 之前。
- 新欄位內容格式是精簡的 `<可用>/<總共>`（例如 `302G/512G`），不含百分比，
  避免表格在窄終端機下太寬；`disk_total_bytes == 0`（查不到）時顯示
  `N/A`。單位換算（bytes 轉 K/M/G/T）沿用 `files.rs`/`storage.rs` 現有的
  `format_bytes`/`format_size` 同一套邏輯風格（各自檔案目前都有自己一份
  類似的小函式，不特別抽出共用，跟這個專案既有的「小型重複優先於過早抽象」
  慣例一致）。

## 不做的事

- 不做只針對 `storage/` 資料夾用量的統計（那需要遞迴加總每個檔案大小，
  跟系統層級的磁機空間是不同的東西，這次不做）。
- 不新增專門的 plugin/面板——直接擴充 `device`/`global` 既有表格。
- 不做歷史趨勢圖表/容量預警通知。
- 不改動任何傳輸機制（HTTP 回報、MQTT 跨 domain 轉發）——新欄位純粹搭
  `DeviceReport` 既有機制的便車。

## 測試

- `sysinfo::disk_usage` 針對目前執行環境（例如專案根目錄）測試回傳值合理
  （`Some((free, total))` 且 `free <= total`、`total > 0`），比照
  `sysinfo.rs` 現有測試（如 `local_hms_matches_system_clock`）用真實系統
  資源驗證、不 mock 的風格。
- `device.rs`/`global.rs` 的表格渲染變動（多一欄）人工驗證即可，這兩個檔案
  本身沒有既有的表格輸出測試。
