# Contract: 內嵌 Web API（`web.rs`，永遠在 port 9759 背景執行）

三種前端（CLI/GUI/瀏覽器）共用同一份 `Shell`/`ContextInner` 狀態；瀏覽器前端
（`web/frontend.html`）透過以下端點驅動。所有端點沒有身分驗證——設計前提是這台
機器本身的網路可達性即為信任邊界（見 spec.md Assumptions）。

| Method | Path | 用途 |
|--------|------|------|
| GET | `/` | 內嵌單頁前端（含 xterm.js 遠端 shell 終端機） |
| GET | `/api/plugins` | 列出目前已註冊的外掛 |
| GET | `/api/version` | 回傳 App 版本號與建置時間戳 |
| GET | `/api/panel/{name}/stream` | 指定外掛面板內容的 SSE 即時串流 |
| GET | `/api/prompt` | 取得目前 Shell 提示字元（依目前 Mode/連線狀態而變） |
| POST | `/api/exec` | 執行一行指令（與 CLI/GUI 走同一個 `execute_line`） |
| GET | `/api/shell/ws` | WebSocket，橋接到一個真實 host shell PTY（每連線各自獨立） |
| GET | `/api/music/files` | 列出音樂庫檔案 |
| GET | `/api/music/file/{name}/audio` | 串流播放指定音樂檔（支援 Range） |
| GET | `/api/music/file/{name}/cover` | 取得音樂檔內嵌封面（ID3） |
| GET | `/api/music/file/{name}/lyrics` | 取得解析後的歌詞（`.srt`） |
| DELETE | `/api/music/file/{name}` | 刪除音樂檔 |
| GET | `/api/notepad/content?name=` | 取得筆記內容 |
| POST | `/api/notepad/content` | 儲存筆記內容（覆蓋寫入，最後儲存者為準） |
| POST | `/api/device/register` | 群組客戶端回報自身 `DeviceReport` 給伺服器 |
| GET | `/api/device/list` | 取得群組裝置清單（`Vec<DeviceListItem>`） |
| GET | `/api/global/list` | 取得跨 domain 橋接得知的裝置清單（`Vec<GlobalListItem>`） |
| POST | `/api/remote/cross-relay` | 客戶端請本機伺服器代為發起一次跨網域請求（單跳，不可鏈式轉發） |
| GET | `/api/files/{folder}` | 列出白名單資料夾內檔案（`Vec<FileMeta>`） |
| GET | `/api/files/{folder}/{name}` | 下載檔案（支援 Range，供分段跨網域傳輸複用） |
| POST | `/api/files/{folder}/{name}` | 上傳檔案 |

**限制**：`/api/files/*` 與 `files` plugin 的 `copy` 指令僅允許 `music`/`notepad`
兩個資料夾（見 `plugins::files::ALLOWED_FOLDERS`），其餘路徑一律拒絕。
