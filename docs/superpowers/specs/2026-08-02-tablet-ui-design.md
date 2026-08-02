# tablet UI：獨立的分頁式觸控介面

日期：2026-08-02

## 目的

現有的 web UI（`src/web/frontend.html`）是滑鼠導向的桌面風格：可拖拉/縮放/
最小化的浮動 panel，適合用滑鼠精準操作，但在觸控裝置（8-10 吋平板）上拖曳
邊角、點小按鈕都不順手。這次新增一個完全獨立的「平板版」頁面，用固定式的
分頁切換取代浮動 panel，觸控字級/間距加大，一次只顯示一個分頁的內容。

跟現有桌面版**共存**，不取代、不互相影響——兩邊都打同一組既有的後端 API，
後端這次完全不用改。

## 範圍（MVP）

只做 5 個分頁：**device／global／topology／weather／system**。其他既有
plugin（storage／notepad／music／shell／output／wol／gitrepo／sync／
remote）這次不做，之後有需要再擴充。

## 新路由與檔案

- 新增 `src/web/tablet.html`（跟 `frontend.html` 完全獨立的檔案，各自
  `include_str!` 進 binary，不共用 JS 模組——這次的重複取捨見下面「不做的
  事」）。
- `src/web.rs` 新增：
  - `const TABLET_HTML: &str = include_str!("web/tablet.html");`
  - `async fn tablet_index() -> impl Responder { HttpResponse::Ok().content_type("text/html; charset=utf-8").body(TABLET_HTML) }`
  - 路由 `.route("/tablet", web::get().to(tablet_index))`。
- 不新增任何其他後端端點——5 個分頁完全靠既有的 `/api/device/list`、
  `/api/global/list`、`/api/panel/weather/stream`、`/api/panel/system/stream`。

## 版面

- 畫面分兩塊：上面是內容區（填滿剩餘空間），下面固定一條分頁列（視覺化
  companion 確認過的方案：分頁列在下面，觸控裝置手持時拇指好按）。
- 分頁列 5 個項目（device/global/topology/weather/system），點一個切一個，
  同一時間只顯示一個分頁的內容——不像桌面版可以同時開好幾個浮動 panel。
- 沒有拖拉/縮放/最小化這些桌面版才有的操作；字級、間距、可點擊區域都比
  桌面版大一些，符合觸控裝置的操作習慣。
- 不做「自動偵測螢幕尺寸切換模式」——使用者直接用平板瀏覽器開
  `http://<機器>:9759/tablet` 這個固定網址（可以之後加到平板主畫面當
  捷徑），跟桌面版的 `/` 完全分開，互不影響。

## 分頁內容與資料來源

- **device**／**global**：定期 poll 既有的 `/api/device/list`／
  `/api/global/list`（純 REST，不用桌面版 `global`/`device` panel 那套
  結構化 JSON snapshot／逐格閃爍機制——平板版是「看多、操作少」的場景，
  不需要那個效果，用最簡單的表格顯示，欄位跟桌面版表格一致
  （id/ip/os/version/mode/uptime/disk/alive）。
- **topology**：跟桌面版 topology panel 同樣的做法（poll 上面兩個端點合併
  節點、手刻簡化版力導向模擬、SVG 畫圖、顏色分角色/虛線框分 domain/broker
  節點/hover 顯示基本資料），但**這次不支援手指拖動節點**——純瀏覽，力導向
  佈局自動運作即可。
- **weather**／**system**：沿用桌面版現有的「純文字 panel」機制，訂閱
  `/api/panel/weather/stream`／`/api/panel/system/stream`，內容原樣顯示
  （跟桌面版這兩個 panel 顯示的文字一致）。

## 分頁切換的資源生命週期

同一時間只有**目前顯示中**的分頁在跑背景工作（poll interval／SSE
連線／topology 的 `requestAnimationFrame` 力導向迴圈）；切到別的分頁時，
舊分頁要停掉自己的背景工作（跟桌面版 topology panel 關閉時呼叫 `stop()`
是同一個理由——平板上背景耗電/流量比桌面瀏覽器更值得省，沒有理由讓 5 個
分頁同時都在背景輪詢）。每個分頁模組對外提供 `start()`/`stop()` 兩個函式，
切分頁時依序呼叫「舊分頁 `stop()` → 新分頁 `start()`」。

## 安全性

跟桌面版同一個原則：任何來自其他機器（`global` 的資料有一部分是透過公開
MQTT broker 收到的）的文字內容，一律用 `textContent` 填值，不拼接 HTML
字串、不用 `innerHTML`。

## 不做的事

- 不做 storage／notepad／music／shell／output／wol／gitrepo／sync／
  remote 這些分頁——MVP 範圍只有前面列的 5 個。
- 不支援 topology 分頁的手指拖動節點——純瀏覽，之後有需要再加 touch
  事件處理。
- 不做「自動偵測螢幕尺寸/裝置類型」在同一個網址切換桌面版/平板版
  ——兩個是固定分開的網址（`/` 跟 `/tablet`）。
- 不新增任何後端端點或修改既有端點的回應格式——5 個分頁都是既有 API 的
  純消費端。
- **刻意接受的重複**：`tablet.html` 的 topology 力導向模擬／SVG 繪製這段
  程式碼會跟 `frontend.html` 各自一份，不透過額外的共用靜態資源機制
  （例如抽成 `/static/topology.js` 讓兩邊 `<script src>`）共用——這次為了
  維持「桌面版跟平板版兩個檔案完全獨立、互不影響」的簡單性，接受這個
  重複。如果之後平板版收錄的功能變多、跟桌面版重複的邏輯持續增加，才
  值得重新評估要不要抽共用模組。

## 測試

- `src/web.rs` 新增的 `tablet_index`／路由沒有既有測試模式可以掛（跟
  `index`／`FRONTEND_HTML` 現有的做法一致，純靜態內容），靠 `cargo build`
  確認無回歸＋手動 `curl http://127.0.0.1:9759/tablet` 確認回應是
  `200`、`Content-Type: text/html`、內容是新檔案。
- `tablet.html` 的 JS（分頁切換、poll、topology 力導向）跟 `frontend.html`
  一樣沒有自動化測試，靠：
  - `node --check` 對主要 `<script>` 內容做語法檢查。
  - 手動操作：在瀏覽器（模擬平板尺寸的視窗，或實機）開
    `http://<機器>:9759/tablet`，依序點 5 個分頁，確認每個分頁都顯示
    正確內容、切分頁時舊分頁的背景工作確實停止（DevTools 觀察沒有殘留
    的 `setInterval`/`EventSource`/`requestAnimationFrame`）。
  - 確認 `/`（桌面版）跟 `/tablet`（平板版）互不影響——桌面版原有功能
    照舊正常運作。
