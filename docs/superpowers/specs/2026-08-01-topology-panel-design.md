# topology：web UI 的跨機器拓樸圖

日期：2026-08-01

## 目的

現有的 `global`/`device` panel 只用表格列出機器，看不出機器之間「誰連誰」的關係
（哪些機器是同一個 domain、client 設定要推播/拉清單給哪個 server、哪些 server
之間透過 MQTT broker 互相看得到對方）。這次新增一個獨立的視覺化 panel，把這層
關係畫成圖（節點＋連線），跟 `player`/`shell` 一樣是選單裡獨立一項，不掛在
`global`/`device` panel 底下。

## 資料來源與更新方式

前端定期（每 2.5 秒）分別 `fetch` 現成的兩個唯讀端點：

- `/api/device/list`：本機這個 domain 看得到的裝置（`DeviceListItem { report,
  age_secs }`）。
- `/api/global/list`：`merged_global_view` 算出來的清單，`domain` 設定過的話
  會包含本機 domain 的裝置（跟 `device/list` 重疊）＋其他 domain 透過 MQTT
  收到的裝置；`domain` 沒設定過就是空陣列。

後端不新增任何 SSE 或聚合端點——純粹前端合併這兩份既有清單，理由：這兩個端點
本來就是唯讀查詢，資料量小，2.5 秒 poll 一次的成本可以忽略，不值得為了這個
視覺化 panel 在後端另開一條推播管道。

**合併規則**（前端做，每次 poll 都重算，不做增量 diff）：
- `global/list` 每一筆的識別鍵是 `${domain}/${id}`（跟 `global` plugin 現有的
  key 規則一致）。
- `device/list` 每一筆，如果 `id` 已經出現在 `global/list` 的某一筆裡就跳過
  （避免同一台機器畫兩次）；沒出現就補進來，識別鍵用 `local/${id}`（代表「不
  知道 domain 名稱，但是本機看得到的機器」），不畫進任何 domain 外框。
- 最終得到一份「這一輪看到的所有節點」，跟上一輪的節點集合比較：消失的節點
  從模擬狀態裡移除，新出現的節點插進去給一個隨機初始座標（讓力導向自然把它
  推到平衡位置），沒消失也沒新增的節點維持原本的座標／速度，不重置。

## 需要後端補的一個欄位：`DeviceReport.server_addr`

現有的 `DeviceReport`（`src/plugin.rs`）沒有「這台 client 設定要推播/拉清單給
哪個 server」的資訊——`system` plugin 自己知道（`ContextInner.server_addr`），
但沒有放進回報內容，所以只有看自己的清單才知道自己連到誰，看不出「其他機器」
連到誰。要畫出正確的 client→server 連線，需要：

- `DeviceReport` 新增 `#[serde(default)] pub server_addr: Option<String>`（跟
  `os`/`version`/`disk_free_bytes` 同一套「舊版機器沒有這個欄位時解析成預設值，
  不讓整筆資料解析失敗」的理由）。
- `system.rs` 的 `build_report` 簽名加一個 `server_addr: Option<String>` 參數，
  填進新欄位；呼叫端（`spawn_reporter`）要先讀出 `ctx.server_addr`，再傳給
  `build_report`（目前是反過來：先組 report，之後才讀 `server_addr` 決定要不
  要推播/拉清單——只需要調整讀取順序，不影響現有的推播/拉清單邏輯）。

這個欄位只在 `mode == "client"` 且真的設定過 `server <ip>` 時才會是
`Some(..)`，`server`／`standalone` 角色一律是 `None`。

## 節點／連線的視覺呈現

跟使用者在視覺化 companion 選定的方案一致：

- **角色用顏色**：`server` 角色＝藍色圓、`client` 角色＝綠色圓、`standalone`
  角色＝灰色淡色圓（沒有任何連線，畫成孤立節點）。
- **domain 用外框分組**：同一個 `domain`（`global/list` 給的那個）的節點，
  外面畫一個虛線圓角框；`local/${id}` 這種不知道 domain 名稱的節點不畫框。
- **MQTT broker**：畫一個固定的六角形圖示（不是某台機器，是抽象的「MQTT
  broker」概念節點）；只要目前看到的節點裡「有任何一個 `mode == "server"`」
  就畫出來，一個都沒有就完全不畫（沒有伺服器就沒有 broker 可畫）。
- **邊（連線）**：
  - 每個 `mode == "server"` 的節點都連一條線到 broker 節點。
  - 每個 `server_addr` 不是 `None` 的節點，連一條線到「ip 等於這個
    `server_addr` 的那個節點」（找不到對應節點就不畫這條線，可能是那台
    server 還沒被任何清單看到）。
  - 這些線代表「設定上的關係」（誰配置要連誰），不是即時驗證真的連上、也
    不代表實體網路路徑——跟一開始跟使用者確認過的前提一致。
  - `server_addr` 是 `DeviceReport` 的欄位，`global/list` 本來就會把整個
    `DeviceReport` 帶出來，所以跨 domain 機器的 client→server 邊「順便」
    就畫得出來，不需要另外新增查詢機制。
- **存活狀態**：`age_secs` 超過門檻（跟 `device.rs`/`global.rs` 的
  `ALIVE_TTL` 一致，即 `REPORT_INTERVAL * 3` = 30 秒；前端直接寫死
  `30000`ms，用註解註明要跟後端常數保持一致，這次不因為一個視覺化 panel
  就新增一個「把常數也序列化給前端」的端點）的節點，改成灰底虛線外框，
  不移除、不隱藏——跟表格版 `alive` 欄位「離線但資料還留著」的既有慣例
  一致。
- **hover 提示**：滑鼠移到節點上顯示一個 tooltip，內容跟 `device`/`global`
  表格欄位一致：id、ip、os、version、mode、device uptime、app uptime、
  disk、alive（存活/離線文字，不是表格那種 `*`/空白符號）。

## 佈局：手刻簡化版力導向模擬

不加 D3.js 或任何繪圈套件（跟這個專案一貫「不為了一個功能就加外部依賴」的
原則一致），純 JS 寫一個簡化版力導向：

- 每個節點一個 `(x, y, vx, vy)`。
- 節點兩兩之間有排斥力（避免疊在一起）。
- 每一條邊（server→broker、client→server）有彈簧吸引力（避免連線拉得太遠）。
- 一個往面板中心的微弱向心力（避免整組節點一路飄出可視範圍）。
- 每一幀（`requestAnimationFrame`）套用力、更新速度/位置、乘上一個衰減係數
  （避免永遠抖動、逐漸穩定下來）。
- broker 節點跟其他節點一樣參與模擬（不特別固定位置），簡單、行為一致。

用 SVG 畫（節點＝`<circle>`、domain 外框＝`<rect rx=...>`、broker＝
`<polygon>`、邊＝`<line>`），每一幀重算座標後更新這些元素的屬性，不是每一幀
整個重建 DOM——跟 `renderTableSnapshot` 每次整個重建 tbody 的做法不同，因為
這裡節點數量、身分在兩次 poll 之間通常不變，只是座標在動畫，用屬性更新比
整個重建更合理。

## 面板規劃

- 選單新增一項（`addMenuItem("topology", "🌐")`，緊接在 `player`/`shell`
  後面），跟這兩個一樣立刻加進選單，不用等 `/api/plugins`。
- `openPanel` 的 SSE 排除清單（`name !== "player" && name !== "shell" && ...`）
  加上 `topology`——這個 panel 完全不走 `/api/panel/<name>/stream` 那條路，
  自己用 `setInterval` poll 上面兩個既有端點。
- panel 內容是一個填滿面板的 `<svg>`（跟 storage/data-table 面板一樣，預設
  面板大小給寬一點，例如 640×420，讓一開始就有足夠空間看出佈局），面板本身
  可以拖拉/縮放/最大化，跟其他 panel 行為一致。
- 關閉這個 panel 時要停掉 `setInterval`／動畫迴圈（跟 `shellUi` 關閉時清理
  WebSocket/PTY 是同一個理由，不留背景在跑）。

## 範圍

- `src/plugin.rs`：`DeviceReport` 新增 `server_addr` 欄位。
- `src/plugins/system.rs`：`build_report` 簽名調整、`spawn_reporter` 讀取
  順序調整。
- `src/web/frontend.html`：新增 CSS（節點/邊/domain 外框/tooltip 樣式）、
  新增 `topology` 選單項、`openPanel` 新分支（建 SVG 容器＋啟動 poll/力導向
  迴圈）、力導向模擬與資料合併的 JS 函式、tooltip 顯示邏輯、面板關閉時的
  清理。
- 不改動 `src/web.rs`（`/api/device/list`/`/api/global/list` 已經存在，
  不需要新端點）。

## 不做的事

- 不做「即時驗證連線是否真的存活」（例如真的去 ping MQTT broker 或量測
  RTT）——邊只代表設定上的關係，這次一開始就跟使用者確認過。
- 不新增任何「查詢別的 domain 內部拓樸」的機制（`RemoteRequest`/
  `CrossDomainAsk` 這次不動）——不需要，因為 `server_addr` 隨著
  `DeviceReport` 一起流進 `global/list`，見上面的說明。
- 不加任何繪圖/力導向函式庫，手刻的簡化版力導向不追求跟 D3-force 一樣精確
  的物理模擬，只要「大致分得開、看得出群組跟連線」就夠。
- 不做「拖曳節點手動調整佈局並記住位置」這種進階互動，這次只有 hover
  tooltip。
- 不新增後端聚合端點，資料合併全部在前端做。

## 測試

- `system.rs` 的 `build_report`/`spawn_reporter` 目前沒有既有測試覆蓋（純
  組資料+背景執行緒），這次也不新增——跟這兩個函式一直以來的做法一致，靠
  `cargo build`/`cargo test` 確認無回歸＋手動確認 `device list`/`global
  list` 的 JSON 多了 `server_addr` 欄位、值符合預期（`client` 且設定過
  `server` 的機器是 `Some(ip)`，其餘是 `null`）。
- `frontend.html` 的力導向/繪圖/合併邏輯沒有自動化測試可以掛（跟這個檔案
  一直以來的做法一致），靠手動操作驗證：開 `topology` panel，確認：
  - 至少一台機器（本機自己）能正確顯示成一個節點。
  - 如果環境裡有多台機器互相回報，能看到 server/client 顏色跟連線正確。
  - 讓某台機器離線一段時間（或直接關掉它），確認對應節點變成灰底虛線邊框
    而不是消失。
  - 關閉 panel 後，瀏覽器分頁的背景 poll/動畫確實停止（用瀏覽器
    DevTools 觀察，不會一直卡著背景計算）。
