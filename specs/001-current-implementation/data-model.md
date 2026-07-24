# Phase 1 Data Model: 現有系統能力總覽

本文件記錄既有實作中實際存在的資料結構（`plugin.rs`/`crypto.rs`/`activity.rs`/
`shell.rs` 及各外掛），對應 spec.md 的 Key Entities 一節，並補上具體欄位與生命
週期，供之後新增/調整功能時比對現有形狀。

## 核心共享狀態

### ContextInner（所有外掛共用的行程內狀態）

行程啟動時建立一份，包在 `Arc<Mutex<>>` 中由所有外掛與 web handler 共用；不落地
到磁碟，行程重啟即清空。

| 欄位 | 型別 | 說明 |
|------|------|------|
| `devices` | `HashMap<String, DeviceEntry>` | 本群組（domain）內已知裝置，key 為裝置 id |
| `server_addr` | `Option<String>` | `system` plugin 設定的群組伺服器位址 |
| `is_server` | `bool` | 本機是否為群組伺服器角色 |
| `domain_name` | `Option<String>` | 本機（伺服器角色時）所屬的 domain 名稱 |
| `global` | `HashMap<String, GlobalRegistryEntry>` | 其他 domain 透過橋接得知的裝置，key 為 `"<domain>/<id>"` |
| `remote_target` | `Option<(String, String)>` | 同群組內 `remote connect <id>` 的目前連線目標 `(id, ip)` |
| `cross_domain_remote_target` | `Option<(String, String)>` | 跨網域 `remote connect <domain>/<id>` 的目前連線目標 |
| `mqtt_client` | `Arc<Mutex<Option<(String, Client)>>>` | 目前存活的 MQTT 連線與其使用的 bridge-id（同一時間至多一條） |
| `cross_domain_pending` | `Arc<Mutex<HashMap<String, Sender<RemoteReply>>>>` | 跨網域請求關聯表：`request_id -> 一次性回覆通道` |
| `activities` | `Arc<ActivityLog>` | 網路活動流水帳 |

## 裝置與群組

### DeviceReport
一台機器目前狀態的快照，本機自報與透過 `/api/device/register` 收到的其他機器
回報共用同一格式。

- `id: String`、`ip: String`、`os: String`（缺欄位時預設 `"N/A"`，向後相容舊版）、
  `version: String`（同上預設）、`tailscale: bool`、`mode: String`、
  `device_uptime_secs: u64`、`app_uptime_secs: u64`

### DeviceEntry
`ContextInner.devices` 裡的一筆：`{ report: DeviceReport, last_seen: Instant }`。
斷線的裝置 `report` 保留最後一次內容，只有 alive 判斷（依賴 `last_seen`）會變成
false，不會被清掉。

### DeviceListItem
`GET /api/device/list` 的可序列化回應形狀：`{ report: DeviceReport, age_secs: f64 }`
——用經過秒數而非 `Instant`（不可序列化、不同機器間不可比較）表示新鮮度，拉取端
自行重建本機 `Instant`。

### GlobalRegistryEntry / GlobalListItem
跨 domain 橋接得知的裝置，比 `DeviceEntry`/`DeviceListItem` 多一個 `domain:
String` 欄位；registry 內 key 為 `global_registry_key(domain, id) = "<domain>/<id>"`
（同一 bridge-id 下不同 domain 可能有同名 id）。

## 跨網域協定

### Envelope\<T\>（`crypto.rs`，wire 層外殼）
`{ ts: u64, body: T }`——`ts` 是防重放用的 Unix 秒數時間戳，`body` 是實際訊息
（`CrossDomainAsk`/`RemoteRequest`/`RemoteReply` 等）。wire bytes 格式為
`nonce(12 bytes) || ChaCha20Poly1305密文(Envelope的JSON序列化結果)`。

### CrossDomainAsk（呼叫端組裝、尚未加上關聯資訊的請求內容）
`Exec { target_id, line }` / `Panel { target_id, panel_name }` /
`FileList { target_id, folder, offset }`（offset 是清單索引，非位元組位移）/
`FilePull { target_id, folder, name, offset }`（offset 是位元組位移）/
`FilePush { target_id, folder, name, offset, data }`（`data` 為單一 chunk 的內容）

### RemoteRequest（實際發布到 MQTT 的請求，比 `CrossDomainAsk` 多關聯資訊）
每個變體都多帶 `request_id: String`（發起端產生的關聯 id，process 內不重複即可，
不需密碼學等級隨機性）與 `source_domain: String`；其餘欄位與對應的
`CrossDomainAsk` 變體相同。發布到 topic `<bridge-id>/<target-domain>/remote/request`。

### RemoteReply（回覆，經 `<bridge-id>/<source-domain>/remote/reply` 送回）
`Exec { request_id, prompt, error: Option<String> }` /
`Panel { request_id, text: Option<String> }` /
`Error { request_id, message }`（涵蓋所有處理失敗情況，如目標 id 查無此裝置） /
`FileList { request_id, files: Vec<FileMeta>, total: usize }` /
`FileChunk { request_id, data: String }` /
`FilePushAck { request_id }`

### FileMeta
`{ name: String, size: u64 }`——同時是 `FileList` 回覆裡每筆檔案的形狀，也是
`GET /api/files/{folder}` 的 JSON 回應形狀（同群組與跨網域共用同一型別）。

**常數約束**：`FILE_CHUNK_SIZE = 4096` bytes（`FilePull`/`FilePush` 單次資料量）、
`FILE_LIST_PAGE_BUDGET = 4096` bytes（`FileList` 單頁預算，依檔名長度估算）、
`REPLAY_WINDOW_SECS = 30`（`Envelope.ts` 容許誤差）。

## 活動稽核

### ActivityEntry / ActivityLog
`ActivityEntry { ts_secs: u64, kind: &'static str, detail: String }`；`kind` 固定
分類字串：`"mqtt-out"` / `"mqtt-in"` / `"http-out"` / `"http-in"` / `"external"`。
`ActivityLog` 是一個上限筆數（預設 1000）的 `VecDeque`，超過上限自動丟棄最舊項目，
純行程記憶體、不落地磁碟。

## 面板與模式

### PanelState
`{ x: i64, y: i64, width: i64, height: i64, visible: bool }`——單位是佔整個畫面的
百分比（0-100）；預設 `{0,0,100,100,false}`。移動限制 `PANEL_MAX_POSITION = 90`、
縮放限制 `PANEL_MIN_SIZE = 10`，確保面板不會完全移出或縮到看不見。

### Mode（每個外掛的互動模式狀態機）／ UiMode
`Mode`：Root → `plugin enter <name>` 之後的外掛層 →（僅 GUI）`panel` 子層；`~`
從任何一層跳回 root。`UiMode`：`Cli` / `Gui`，由 `mode cli`/`mode gui` 指令切換。

## 各外掛的資料實體（依 spec.md Key Entities 對應）

- **WOL Target**（`wol` plugin）：登記的 `{ name, mac 位址 }`。
- **Weather Location**（`weather` plugin）：登記的城市/地點，搭配 `CacheEntry`
  （查詢結果 + 快取時間，5 分鐘有效）與 `LocationReport`（依來源 IP 自動偵測的
  第一筆地點）。
- **Git Watch Entry**（`gitrepo` plugin）：登記的本機路徑（可以是單一 repo 或
  多個 repo 的父目錄），搭配 `DirtyRepo`（目前偵測為有未提交變更的 repo）與
  `ScanState`（掃描進度），持久化到磁碟。
- **Music Track / Download Job**（`music` plugin）：本機 `music/*.mp3` 檔案，搭配
  `DownloadStatus`（下載中/成功/失敗）追蹤背景下載工作。
- **Note**（`notepad` plugin）：`notepad/*.md` 檔案，GUI 端另有 `EditState`
  （目前編輯中的游標位置/內容）。
- **File Transfer Job**（`files` plugin）：`TransferStatus`（目前複製工作的進度）、
  `Direction`（to/from）、`CopyTarget`（同網域 vs 跨網域目標），一次只允許一個
  複製工作進行中。
