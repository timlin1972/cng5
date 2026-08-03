# topology 面板改成固定式階層佈局

## 目的

現有的 topology 面板（桌面版 `frontend.html`、`/tablet` 版 `tablet.html` 各自
一份）用手刻簡化版力導向模擬決定節點座標，節點會自己動來動去找平衡位置。這次
改成固定式階層佈局：節點座標完全依「角色（broker/server/client）＋ domain」
直接算出來，不再跑物理模擬，畫面更穩定、可預期。

視覺呈現（顏色分角色、外框分 domain、broker 六角形、alive/離線樣式、hover
tooltip 內容）完全不變——這次只換「節點座標怎麼算」，`render()` 裡畫
domain 外框／邊／節點的邏輯繼續沿用（它們本來就是讀節點目前的 `x`/`y`
畫圖，跟座標來源無關）。`computeTopologyEdges`（決定畫哪些連線）也完全不動。

## 範圍

- 修改 `src/web/frontend.html` 的 topology 面板（桌面版）。
- 修改 `src/web/tablet.html` 的 topology 分頁（`/tablet`）。
- 兩邊各自獨立一份程式碼（既有慣例，不共用 JS 模組），這次分開改兩次。
- 不改：後端任何邏輯、`/api/device/list`／`/api/global/list` 這兩個既有
  端點、`computeTopologyEdges`（邊的計算規則）、節點/邊/domain 外框的視覺
  樣式（顏色、形狀、CSS class）、tooltip 內容、alive 判斷邏輯。

## 拿掉的東西

- **力導向模擬**：`tick()` 函式、`REPULSION`/`SPRING_LENGTH`/
  `SPRING_STRENGTH`/`CENTER_STRENGTH`/`DAMPING` 這些常數、`makeNode()` 隨機
  初始座標、`requestAnimationFrame` 逐幀迴圈——固定佈局下座標是算出來的，
  不需要每幀更新，每次 poll（沿用既有的 2.5 秒間隔）重算一次座標、呼叫一次
  `render()` 就好。
- **桌面版節點拖曳**：`onNodeMouseDown`／`onDocumentMouseMove`／
  `onDocumentMouseUp`、`draggingKey`／`draggingEl`、`mousedown` 監聽、
  `document` 上的 `mousemove`/`mouseup` 監聽。固定佈局下手動拖曳沒有意義
  （下一次 poll 座標就會被算出來的固定值蓋掉），拿掉後桌面版跟 `/tablet`
  版行為一致（`/tablet` 版本來就沒有拖曳）。
- 節點位置變動不做平滑過渡動畫——domain 加入/離開、節點集合改變時直接跳到
  新座標，不插值（保持簡單，這是這次要的效果）。

## 佈局演算法

新增一個純函式 `computeLayout(nodesData, hasServer, width, height)`，輸入
跟現有 `applyPoll`/`fetchTopologyNodes` 回傳的資料結構一樣（`Map<key, {
key, domain, report, ageSecs }>` 加上 `hasServer` 布林值），輸出每個節點
（含 `"broker"` 這個特殊 key，如果 `hasServer` 為真）的 `{x, y}`。

**分組規則：**
1. 依 `domain` 欄位分組：`domain` 有值的節點依 domain 分組；`domain` 為
   `null` 的節點（`local/${id}` 這種不知道 domain 名稱的）自成一組，視為
   「local」欄，永遠排在所有具名 domain 的最右邊。
2. 具名 domain 依名稱字串排序（`Array.prototype.sort()` 預設字典序），由
   左到右排欄；「local」欄固定排最後一欄（如果這組是空的就不佔欄）。
3. 每組內再依 `report.mode` 分兩排：`mode === "server"` 的節點在「server
   排」；其他（`client`、`standalone`）在「client 排」。

**座標計算：**
- 欄數 = 具名 domain 數 ＋（local 組非空則 +1）。欄數為 0（沒有任何節點）
  時面板留白，不畫任何東西（跟現在「沒有資料」的行為一致，`render()` 遇到
  空的 `sim.nodes`／`domainEls` 自然不會畫出東西，不用特別處理）。
- 每欄寬度 = `width / 欄數`，欄的水平中心 = `欄寬 * (欄索引 + 0.5)`。
- 三個固定 Y 座標（由上到下）：`brokerY`／`serverRowY`／`clientRowY`，用
  面板高度等分決定（例如 `height * 0.2`／`height * 0.5`／`height * 0.8`，
  實作時可依實際視覺效果微調，不需要跟這裡的比例完全一致）。
- broker（如果 `hasServer`）固定畫在 `x = width / 2, y = brokerY`（面板正
  上方置中，不管欄怎麼分）。
- 同一欄、同一排如果有多個節點（例如同 domain 有兩台 server），該排節點
  在欄寬範圍內水平等距展開（置中對齊該欄中心），不特別對齊到它連線的另一
  端節點——保持演算法簡單，實際連線用 `computeTopologyEdges` 算出來的線
  自然會畫出誰連誰，不需要靠 x 座標對齊來表達關係。
- 一欄裡某一排沒有節點（例如這個 domain 只有 client 沒有 server）就是
  該排空著，不影響其他欄／其他排的 Y 座標（維持所有欄的 server 排/client
  排上下對齊，視覺一致）。

**與現有程式碼的介接：**
- `applyPoll`（決定 `sim.nodes` 裡有哪些 key）簡化成：每次 poll 呼叫
  `fetchTopologyNodes()` 拿到 `nodesData`，呼叫 `computeTopologyEdges` 拿到
  `{ edges, hasServer }`，呼叫新的 `computeLayout(nodesData, hasServer,
  width, height)` 拿到每個 key 的 `{x, y}`，直接把這組 `x`/`y` 寫進
  `sim.nodes`（不再需要 `vx`/`vy`/`fx`/`fy` 這些物理量，`sim.nodes` 的節點
  物件只保留 `x`/`y`/`isBroker`/`data`）。
- `render()` 本身不用改（它就是讀 `sim.nodes` 目前的 `x`/`y` 畫 domain
  外框/邊/節點，不管座標怎麼來的）。
- 呼叫時機：poll 一次（`setInterval`，沿用桌面版/`. tablet` 版原本各自的
  poll 間隔）就重算一次 `computeLayout` 並呼叫一次 `render()`，不再需要
  `requestAnimationFrame` 迴圈。

## 測試

這是純前端 SVG 渲染邏輯，跟後端無關。驗證方式：
- `node --check` 對兩個檔案的 `<script>` 內容做語法檢查（沿用專案既有的
  驗證慣例，`tablet.html` 只有一個 script block；`frontend.html` 有多個
  script block，需要抓對包含 topology 程式碼的那一塊，或抓全部逐一檢查）。
- 手動驗證（沒有自動化 UI 測試，這個專案目前也沒有為 web UI 寫過前端測試）：
  瀏覽器開 `/` 跟 `/tablet` 的 topology 面板，確認：
  - 單一 domain、多 domain、沒有 domain（`domain` 全是 `null`）、沒有任何
    server（不畫 broker）這幾種情境佈局都合理、不重疊。
  - 資料每次 poll 更新時（例如新裝置上線/離線）位置正確重算，沒有殘留
    的舊節點/舊 domain 外框。
  - 桌面版滑鼠不能再拖動節點（改成固定佈局後的預期行為）。
  - hover tooltip、alive/離線樣式、broker 六角形、domain 虛線外框視覺跟
    改版前一致（這次不改視覺樣式）。
