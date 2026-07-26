# storage plugin（本機檔案總管）設計

日期：2026-07-26

## 目的

在 cng5 裡新增一個通用的本機檔案總管功能，做為「NAS 功能」這個大題目下第一個子項目
（其餘子項目：共享資料夾/權限、媒體庫、多裝置同步備份、儲存空間總覽，之後再各自獨立
brainstorm）。參考 Synology DSM File Station / Nextcloud / TrueNAS 這類產品的檔案瀏覽
體驗，但範圍收斂到「管理本機這台裝置的儲存」。

## 範圍

- 只管**本機**這台裝置的儲存，不做跨裝置瀏覽/存取（跨裝置操作已經有 `files` plugin
  負責整份資料夾同步，是不同的功能，這次不動它）。
- 單一根目錄，不做多個具名共用資料夾（那是「共享資料夾/權限」子項目的範圍）。
- 不做儲存空間/硬碟用量總覽（那是「儲存空間總覽」子項目的範圍）。
- 新增一個獨立的 plugin（暫定命名 `storage`），不修改現有 `files` plugin 的行為。

## 儲存架構

- 新常數 `STORAGE_DIR = "storage"`（跟 `MUSIC_DIR = "music"`、`NOTEPAD_DIR = "notepad"`
  同樣的命名慣例：相對於程式執行時工作目錄的資料夾）。
- 根目錄下可以有任意深度的巢狀子資料夾——這是跟 `files` plugin 最大的不同（`files`
  的 `list_local_files` 只處理平面、單層資料夾，跳過子目錄）。
- 第一次啟動時如果 `storage/` 不存在就自動建立（`fs::create_dir_all`），跟 `music`/
  `notepad` 現有做法一致。

## 路徑安全

新寫 `safe_storage_path(root: &Path, relative: &str) -> Option<PathBuf>`，比照現有
`files.rs` 的 `safe_file_path` 防禦邏輯，但支援巢狀相對路徑：

- 拒絕絕對路徑
- 拒絕任何 `..` 路徑片段（逐段檢查，不能靠字串比對整個路徑打馬虎眼）
- 拒絕空字串
- 組出完整路徑後，用 canonicalize 確認結果實際上還在 `storage/` 根目錄底下（雙重防護，
  防止 symlink 之類的方式繞過純字串檢查逃出根目錄）

CLI 指令跟 Web API 的每一個端點都要先過這個函式驗證，才能碰檔案系統——不管是本機
plugin 內部呼叫，還是 HTTP 請求帶進來的路徑，都是同一關卡，不能因為請求來源不同就
放寬檢查。

## Plugin（CLI）設計

- `StorageState`：plugin 記住「目前瀏覽到哪個子路徑」（相對 `storage/` 根目錄的
  `PathBuf`），進入 plugin 時預設在根目錄。
- 指令：
  - `ls [path]`——列出目前路徑（或指定 `path`，不改變目前位置）底下的檔案/資料夾，
    含大小、修改時間
  - `cd <path>`——切換目前位置；`cd ..` 回上一層、`cd /` 回根目錄
  - `mkdir <name>`——在目前位置建立子資料夾
  - `rm <name> [--recursive]`——刪除檔案；刪資料夾時若非空，沒加 `--recursive` 就報錯
    拒絕
  - `mv <old> <new>`——重新命名，或搬到目前位置下的另一個子路徑；目的地已經是個
    「檔案」就直接覆蓋（跟上傳的覆蓋規則一致），但目的地已經是個「資料夾」一律拒絕
    （見下方「錯誤處理」，避免「搬移覆蓋資料夾」語意不清楚導致誤刪內容）
- 沒有 `upload`/`download` CLI 指令：CLI 執行的機器本身就是儲存所在地，本來就能直接
  用作業系統的檔案總管或 `cp` 操作 `storage/` 底下的內容；上傳/下載是給「別的裝置的
  瀏覽器」用的，只在 Web UI 提供。
- `panel_text()` 顯示目前路徑：breadcrumb 一行，接著是名稱/大小/時間的表格，呼應
  Web UI 的表格清單版面。

## Web API 設計

新的端點（獨立於現有 `/api/files/...`，不共用限制邏輯，因為這裡要支援巢狀路徑）：

- `GET /api/storage/list?path=<相對路徑>`——回傳 JSON：該路徑底下的項目清單（名稱、
  是否為資料夾、大小、修改時間）
- `GET /api/storage/download?path=<相對檔案路徑>`——下載單一檔案，沿用現有
  `actix_files::NamedFile`（自動處理 `Range`，大檔案/影片也能拖拉進度，跟現有 music
  下載一致）
- `POST /api/storage/upload?path=<目的地相對路徑>`——上傳檔案到指定位置，同名直接
  覆蓋
- `POST /api/storage/mkdir?path=<相對路徑>`——建立資料夾
- `POST /api/storage/delete?path=<相對路徑>&recursive=<bool>`——刪除檔案/資料夾，
  非空資料夾沒帶 `recursive=true` 就回錯誤
- `POST /api/storage/rename?from=<相對路徑>&to=<相對路徑>`——重新命名/搬移；規則
  同 CLI 的 `mv`：目的地是檔案就覆蓋，是資料夾就拒絕

所有端點的 `path`/`from`/`to` 都先過 `safe_storage_path` 驗證。

## Web UI 設計

跟 `player`/`shell`/`notepad` 一樣是前端客製化的 panel（不是純文字），採表格清單版面：

- 上方 breadcrumb（例如 `/ photos / 2026-summer`），每一段路徑可以點擊回到那一層
- 工具列兩個按鈕：「⬆ 上傳」（開瀏覽器原生檔案選擇器 → `POST /api/storage/upload`）、
  「＋ 新資料夾」（跳出輸入名稱 → `POST /api/storage/mkdir`）
- 表格欄位：名稱（資料夾點擊進入、檔案點擊或旁邊圖示下載）、大小、修改時間；每一列
  尾端有刪除／重新命名的小按鈕
- 刪除非空資料夾時彈確認視窗，顯示「內含 N 個檔案」，確認後才送出
  `recursive=true` 的刪除請求
- 前端「目前路徑」是瀏覽器端 JS 自己的狀態（每次操作用 `path` 查詢參數打 API），
  跟 CLI 那邊各自獨立的「目前路徑」不互相影響，這跟其他 panel 的既有模式一致
- panel 太窄時不做特別的降級，直接讓內容被裁切/捲動（跟 `clock` plugin 的決定一致）

## 錯誤處理

- 路徑穿越/絕對路徑/canonicalize 逃逸出根目錄——一律拒絕
- `mkdir` 在已存在同名「檔案」的地方——報錯，不會建立
- `rm`/`mv` 目標不存在——報錯
- 上傳目的地已有同名檔案——直接覆蓋
- `mv`/rename 目的地已存在：是檔案就覆蓋，是資料夾就拒絕並報錯（不做資料夾合併，
  避免語意不清楚導致誤刪內容）

## 測試

- `safe_storage_path` 的單元測試：合法巢狀路徑、`..`、絕對路徑、canonicalize 逃逸、
  空字串都要拒絕——這是最關鍵的安全部分
- CLI 指令行為測試：`cd`/`ls`/`mkdir`/`rm`（含非空資料夾沒加 `--recursive` 要報錯）/
  `mv`
- Web API 的 handler 本身不另外寫測試：這個專案目前 `web.rs` 沒有既有的 HTTP handler
  測試慣例（`files_list`/`files_download`/`files_upload` 也沒有），handler 只是薄薄
  一層轉呼叫，邏輯都在會被測試的共用函式裡，跟現有模式一致

## 不做的事

- 不做跨裝置瀏覽/存取（維持只管本機；跨裝置同步是 `files` plugin 的範圍）。
- 不做多個具名共用資料夾（先只有一個根目錄）。
- 不做儲存空間/硬碟用量總覽。
- 不做檔案內容預覽（縮圖、線上看圖/影片播放器）——只有下載連結。
- 不做搜尋功能。
- 不做使用者帳號/權限（cng5 目前沒有多使用者概念）。
- CLI 不提供 `upload`/`download` 指令（見「Plugin（CLI）設計」）。
