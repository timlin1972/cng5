# topology 固定式階層佈局 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把桌面版 `frontend.html` 跟 `/tablet` 版 `tablet.html` 的 topology 面板，從手刻力導向模擬改成依角色（broker/server/client）+ domain 直接算出的固定式階層佈局。

**Architecture:** 兩個檔案各自獨立一份 topology 程式碼（既有慣例，不共用模組），各自新增一個純函式 `computeLayout(nodesData, hasServer, width, height)` 取代原本的物理模擬（`tick()`），回傳每個節點的固定座標；`render()`（畫 domain 外框/邊/節點/tooltip）完全不動，因為它本來就是讀節點目前的 `x`/`y` 畫圖，跟座標怎麼算出來無關。桌面版額外要拿掉節點拖曳功能。

**Tech Stack:** 純 JS + SVG（沿用既有，不新增依賴）。

## Global Constraints

- 兩個檔案（`src/web/frontend.html`、`src/web/tablet.html`）都要改，各自獨立一份程式碼，不共用模組。
- 不改：後端任何邏輯、`/api/device/list`／`/api/global/list` 端點、`fetchTopologyNodes`／`computeTopologyEdges`（合併節點/算邊的規則）、節點/邊/domain 外框/broker 的視覺樣式（顏色、形狀、CSS class）、tooltip 內容、`isAlive`／`roleColor` 判斷邏輯、`render()` 函式本身。
- 拿掉力導向模擬（`tick()`、`REPULSION`/`SPRING_LENGTH`/`SPRING_STRENGTH`/`CENTER_STRENGTH`/`DAMPING`、`makeNode()` 隨機初始座標、`requestAnimationFrame` 逐幀迴圈）——每次 poll（沿用既有的 `TOPOLOGY_POLL_MS` 間隔常數，兩個檔案都叫這個名字）重算一次固定座標、呼叫一次 `render()` 就好，不需要逐幀更新。
- 桌面版額外拿掉節點拖曳（`onNodeMouseDown`/`onDocumentMouseMove`/`onDocumentMouseUp`、`draggingKey`/`draggingEl`、對應的事件監聽），CSS `.topology-node` 的 `cursor` 從 `grab` 改成 `default`（跟 `/tablet` 版一致）。`/tablet` 版本來就沒有拖曳，不用改這塊。
- 節點位置變動不做平滑過渡動畫，直接跳到新座標算出的位置。
- 佈局規則（兩邊都一樣）：
  - broker（六角形，只有 `hasServer` 為真才畫）固定在面板最上排、水平置中。
  - 具名 domain（`domain` 欄位有值）依名稱字串排序（`Array.prototype.sort()` 預設字典序）由左到右各佔一欄；`domain` 為 `null` 的節點（`local/${id}`）自成一欄，固定排在所有具名 domain 欄位的最後面。
  - 每欄內，`report.mode === "server"` 的節點排在「server 排」（面板高度 50%），其他（`client`/`standalone`）排在「client 排」（面板高度 80%）；同排多個節點在欄寬範圍內水平等距展開（每個節點間距 70px，置中對齊該欄中心）。broker 排固定在面板高度 20%。
  - 欄寬 = 面板寬度 / 欄數；沒有任何節點時欄數為 0，不畫任何東西。

---

### Task 1: `frontend.html`（桌面版）改成固定式階層佈局

**Files:**
- Modify: `src/web/frontend.html:649`（CSS `cursor`）
- Modify: `src/web/frontend.html:1776-2114`（`startTopology` 函式整個換掉）

**Interfaces:**
- Consumes: 既有的 `SVGNS`、`TOPOLOGY_ALIVE_TTL_MS`、`TOPOLOGY_POLL_MS`、`fetchTopologyNodes()`、`computeTopologyEdges(nodes)`、`formatUptimeJs`、`formatDiskJs`（都不變，這個 task 不修改它們）。
- Produces: 無下游任務依賴這個 task 的產物（Task 2 是 `tablet.html` 的獨立改動）。

- [ ] **Step 1: CSS 拿掉拖曳游標樣式**

把（第 649 行）：

```css
  .topology-node { cursor: grab; }
```

改成：

```css
  .topology-node { cursor: default; }
```

- [ ] **Step 2: 整個換掉 `startTopology` 函式**

把（第 1776-2114 行，從函式前的註解到函式結尾的 `}`）：

```js
  // 建立一個獨立運作的 topology 面板：定期 poll 上面兩個端點合併節點、跑一個
  // 手刻的簡化版力導向模擬（節點互推、邊當彈簧、往中心的微弱向心力、每幀
  // 衰減速度），用 SVG 逐幀更新座標（不是整個重建 DOM，因為節點身分在兩次
  // poll 之間通常不變，只是位置在動）。回傳 `{ container, stop }`，
  // `stop()` 給 `closePanel` 呼叫，清掉 interval／animation frame，不留
  // 背景在跑。
  function startTopology(container, svg, domainLayer, edgeLayer, nodeLayer, tooltip) {
    const sim = { nodes: new Map(), edges: [] };
    const domainEls = new Map();
    const edgeEls = new Map();
    const nodeEls = new Map();
    // 拖曳中的節點 key／DOM 元素——拖曳期間 `tick()` 會跳過這個節點的力學
    // 積分（見下面），完全交給滑鼠位置決定座標，放開滑鼠後才恢復正常物理。
    let draggingKey = null;
    let draggingEl = null;

    function panelSize() {
      const rect = svg.getBoundingClientRect();
      return { width: rect.width || 400, height: rect.height || 300 };
    }

    // 把滑鼠事件的螢幕座標換成跟節點 `x`/`y` 同一套座標系（SVG 左上角為原點）。
    function svgPoint(e) {
      const rect = svg.getBoundingClientRect();
      return { x: e.clientX - rect.left, y: e.clientY - rect.top };
    }

    function onNodeMouseDown(key, group, e) {
      e.preventDefault();
      draggingKey = key;
      draggingEl = group;
      group.style.cursor = "grabbing";
      const n = sim.nodes.get(key);
      if (n) {
        n.vx = 0;
        n.vy = 0;
      }
    }

    function onDocumentMouseMove(e) {
      if (!draggingKey) return;
      const n = sim.nodes.get(draggingKey);
      if (!n) return;
      const p = svgPoint(e);
      n.x = p.x;
      n.y = p.y;
      n.vx = 0;
      n.vy = 0;
    }

    function onDocumentMouseUp() {
      if (draggingEl) draggingEl.style.cursor = "grab";
      draggingKey = null;
      draggingEl = null;
    }

    document.addEventListener("mousemove", onDocumentMouseMove);
    document.addEventListener("mouseup", onDocumentMouseUp);

    function makeNode(isBroker) {
      const { width, height } = panelSize();
      return {
        x: width / 2 + (Math.random() - 0.5) * 60,
        y: height / 2 + (Math.random() - 0.5) * 60,
        vx: 0,
        vy: 0,
        fx: 0,
        fy: 0,
        isBroker,
        data: null,
      };
    }

    function applyPoll(nodesData, hasServer) {
      const wanted = new Set(nodesData.keys());
      if (hasServer) wanted.add("broker");
      for (const key of [...sim.nodes.keys()]) {
        if (!wanted.has(key)) sim.nodes.delete(key);
      }
      for (const key of wanted) {
        let n = sim.nodes.get(key);
        if (!n) {
          n = makeNode(key === "broker");
          sim.nodes.set(key, n);
        }
        if (key !== "broker") n.data = nodesData.get(key);
      }
    }

    function tick() {
      const { width, height } = panelSize();
      const entries = [...sim.nodes.values()];
      const draggedNode = draggingKey ? sim.nodes.get(draggingKey) : null;
      for (const n of entries) {
        n.fx = 0;
        n.fy = 0;
      }

      const REPULSION = 2200;
      for (let i = 0; i < entries.length; i++) {
        for (let j = i + 1; j < entries.length; j++) {
          const a = entries[i];
          const b = entries[j];
          const dx = a.x - b.x;
          const dy = a.y - b.y;
          let distSq = dx * dx + dy * dy;
          if (distSq < 1) distSq = 1;
          const dist = Math.sqrt(distSq);
          const force = REPULSION / distSq;
          const fx = (dx / dist) * force;
          const fy = (dy / dist) * force;
          a.fx += fx;
          a.fy += fy;
          b.fx -= fx;
          b.fy -= fy;
        }
      }

      const SPRING_LENGTH = 90;
      const SPRING_STRENGTH = 0.02;
      for (const edge of sim.edges) {
        const a = sim.nodes.get(edge.from);
        const b = sim.nodes.get(edge.to);
        if (!a || !b) continue;
        const dx = b.x - a.x;
        const dy = b.y - a.y;
        const dist = Math.sqrt(dx * dx + dy * dy) || 1;
        const stretch = dist - SPRING_LENGTH;
        const fx = (dx / dist) * stretch * SPRING_STRENGTH;
        const fy = (dy / dist) * stretch * SPRING_STRENGTH;
        a.fx += fx;
        a.fy += fy;
        b.fx -= fx;
        b.fy -= fy;
      }

      const CENTER_STRENGTH = 0.01;
      const DAMPING = 0.85;
      for (const n of entries) {
        if (n === draggedNode) continue;
        n.fx += (width / 2 - n.x) * CENTER_STRENGTH;
        n.fy += (height / 2 - n.y) * CENTER_STRENGTH;
        n.vx = (n.vx + n.fx) * DAMPING;
        n.vy = (n.vy + n.fy) * DAMPING;
        n.x += n.vx;
        n.y += n.vy;
      }
    }

    function isAlive(n) {
      return !!n.data && n.data.ageSecs * 1000 < TOPOLOGY_ALIVE_TTL_MS;
    }

    function roleColor(mode) {
      if (mode === "server") return "#6f9dff";
      if (mode === "client") return "#4caf7d";
      return "#5a6272";
    }

    function showTooltip(key) {
      const n = sim.nodes.get(key);
      if (!n || n.isBroker || !n.data) {
        tooltip.style.display = "none";
        return;
      }
      const r = n.data.report;
      const alive = isAlive(n) ? "是" : "否";
      tooltip.textContent =
        `id: ${r.id}\nip: ${r.ip}\nos: ${r.os}\nversion: ${r.version}\nmode: ${r.mode}\n` +
        `device uptime: ${formatUptimeJs(r.device_uptime_secs)}\napp uptime: ${formatUptimeJs(r.app_uptime_secs)}\n` +
        `disk: ${formatDiskJs(r.disk_free_bytes, r.disk_total_bytes)}\nalive: ${alive}`;
      tooltip.style.display = "block";
    }

    function positionTooltip(e) {
      if (tooltip.style.display !== "block") return;
      const rect = container.getBoundingClientRect();
      tooltip.style.left = `${e.clientX - rect.left + 12}px`;
      tooltip.style.top = `${e.clientY - rect.top + 12}px`;
    }

    function hideTooltip() {
      tooltip.style.display = "none";
    }

    function buildNodeEl(key, n) {
      const group = document.createElementNS(SVGNS, "g");
      group.setAttribute("class", "topology-node");
      let shape;
      if (n.isBroker) {
        shape = document.createElementNS(SVGNS, "polygon");
        shape.setAttribute("class", "topology-broker");
        shape.setAttribute("points", "0,-20 17,-10 17,10 0,20 -17,10 -17,-10");
      } else {
        shape = document.createElementNS(SVGNS, "circle");
        shape.setAttribute("r", "18");
      }
      group.appendChild(shape);
      const label = document.createElementNS(SVGNS, "text");
      label.setAttribute("class", "topology-node-label");
      label.setAttribute("y", "32");
      group.appendChild(label);
      nodeLayer.appendChild(group);
      group.addEventListener("mouseenter", () => showTooltip(key));
      group.addEventListener("mousemove", positionTooltip);
      group.addEventListener("mouseleave", hideTooltip);
      group.addEventListener("mousedown", (e) => onNodeMouseDown(key, group, e));
      return { group, shape, label };
    }

    function render() {
      // domain 外框：只框 `data.domain` 有值的節點（`local/${id}` 那種
      // domain 名稱不明的節點不畫框）。
      const groups = new Map();
      for (const [, n] of sim.nodes) {
        if (n.isBroker || !n.data || !n.data.domain) continue;
        if (!groups.has(n.data.domain)) groups.set(n.data.domain, []);
        groups.get(n.data.domain).push(n);
      }
      const seenDomains = new Set();
      for (const [domain, members] of groups) {
        seenDomains.add(domain);
        const pad = 30;
        const xs = members.map((m) => m.x);
        const ys = members.map((m) => m.y);
        const x = Math.min(...xs) - pad;
        const y = Math.min(...ys) - pad;
        const w = Math.max(...xs) - Math.min(...xs) + pad * 2;
        const h = Math.max(...ys) - Math.min(...ys) + pad * 2;
        let box = domainEls.get(domain);
        if (!box) {
          box = { rect: document.createElementNS(SVGNS, "rect"), label: document.createElementNS(SVGNS, "text") };
          box.rect.setAttribute("class", "topology-domain-box");
          box.label.setAttribute("class", "topology-domain-label");
          domainLayer.appendChild(box.rect);
          domainLayer.appendChild(box.label);
          domainEls.set(domain, box);
        }
        box.rect.setAttribute("x", x);
        box.rect.setAttribute("y", y);
        box.rect.setAttribute("width", w);
        box.rect.setAttribute("height", h);
        box.rect.setAttribute("rx", 12);
        box.label.setAttribute("x", x + 8);
        box.label.setAttribute("y", y + 16);
        box.label.textContent = domain;
      }
      for (const [domain, box] of [...domainEls]) {
        if (!seenDomains.has(domain)) {
          box.rect.remove();
          box.label.remove();
          domainEls.delete(domain);
        }
      }

      // 邊
      const seenEdges = new Set();
      for (const edge of sim.edges) {
        const a = sim.nodes.get(edge.from);
        const b = sim.nodes.get(edge.to);
        if (!a || !b) continue;
        const ekey = `${edge.from}|${edge.to}`;
        seenEdges.add(ekey);
        let line = edgeEls.get(ekey);
        if (!line) {
          line = document.createElementNS(SVGNS, "line");
          line.setAttribute("class", "topology-edge");
          edgeLayer.appendChild(line);
          edgeEls.set(ekey, line);
        }
        line.setAttribute("x1", a.x);
        line.setAttribute("y1", a.y);
        line.setAttribute("x2", b.x);
        line.setAttribute("y2", b.y);
      }
      for (const [ekey, line] of [...edgeEls]) {
        if (!seenEdges.has(ekey)) {
          line.remove();
          edgeEls.delete(ekey);
        }
      }

      // 節點
      const seenNodes = new Set();
      for (const [key, n] of sim.nodes) {
        seenNodes.add(key);
        let el = nodeEls.get(key);
        if (!el) {
          el = buildNodeEl(key, n);
          nodeEls.set(key, el);
        }
        el.group.setAttribute("transform", `translate(${n.x},${n.y})`);
        if (n.isBroker) {
          el.label.textContent = "MQTT broker";
        } else if (n.data) {
          const alive = isAlive(n);
          el.shape.setAttribute("fill", alive ? roleColor(n.data.report.mode) : "#3a4150");
          el.shape.setAttribute("stroke", alive ? "none" : "#6b7280");
          el.shape.setAttribute("stroke-width", alive ? "0" : "2");
          el.shape.setAttribute("stroke-dasharray", alive ? "0" : "3,3");
          el.label.textContent = n.data.report.id;
        }
      }
      for (const [key, el] of [...nodeEls]) {
        if (!seenNodes.has(key)) {
          el.group.remove();
          nodeEls.delete(key);
        }
      }
    }

    async function pollOnce() {
      const nodesData = await fetchTopologyNodes();
      const { edges, hasServer } = computeTopologyEdges(nodesData);
      applyPoll(nodesData, hasServer);
      sim.edges = edges;
    }

    let rafId = null;
    function loop() {
      tick();
      render();
      rafId = requestAnimationFrame(loop);
    }

    pollOnce();
    loop();
    const intervalId = setInterval(pollOnce, TOPOLOGY_POLL_MS);

    return {
      container,
      stop() {
        clearInterval(intervalId);
        if (rafId) cancelAnimationFrame(rafId);
        document.removeEventListener("mousemove", onDocumentMouseMove);
        document.removeEventListener("mouseup", onDocumentMouseUp);
      },
    };
  }
```

改成：

```js
  // 建立一個獨立運作的 topology 面板：定期 poll 上面兩個端點合併節點、依
  // 角色（broker/server/client）＋ domain 直接算出固定座標（見
  // `computeLayout`），不再跑力導向模擬，也不支援拖曳節點——固定佈局下手動
  // 拖曳沒有意義，下一次 poll 座標就會被算出來的固定值蓋掉。用 SVG 更新
  // 座標（不是整個重建 DOM，因為節點身分在兩次 poll 之間通常不變）。回傳
  // `{ container, stop }`，`stop()` 給 `closePanel` 呼叫，清掉 interval，
  // 不留背景在跑。
  function startTopology(container, svg, domainLayer, edgeLayer, nodeLayer, tooltip) {
    const sim = { nodes: new Map(), edges: [] };
    const domainEls = new Map();
    const edgeEls = new Map();
    const nodeEls = new Map();

    function panelSize() {
      const rect = svg.getBoundingClientRect();
      return { width: rect.width || 400, height: rect.height || 300 };
    }

    // 依角色＋domain 算出每個節點的固定座標：broker 置中最上排；具名 domain
    // 依名稱排序由左到右各佔一欄，不知道 domain 名稱的節點（`local/${id}`）
    // 另成一欄排在最後；欄內 server 排在上、client（含 standalone）排在下，
    // 同排多個節點在欄寬內水平等距展開。回傳 `Map<key, {x, y}>`（`key` 是
    // 節點 key 或 `"broker"`）。
    function computeLayout(nodesData, hasServer, width, height) {
      const domainOrder = [];
      const domainMembers = new Map();
      const localMembers = [];
      for (const n of nodesData.values()) {
        if (n.domain) {
          if (!domainMembers.has(n.domain)) {
            domainMembers.set(n.domain, []);
            domainOrder.push(n.domain);
          }
          domainMembers.get(n.domain).push(n);
        } else {
          localMembers.push(n);
        }
      }
      domainOrder.sort();
      const columns = domainOrder.map((d) => domainMembers.get(d));
      if (localMembers.length > 0) columns.push(localMembers);

      const positions = new Map();
      const brokerY = height * 0.2;
      const serverRowY = height * 0.5;
      const clientRowY = height * 0.8;

      if (hasServer) positions.set("broker", { x: width / 2, y: brokerY });

      const colWidth = columns.length > 0 ? width / columns.length : width;
      columns.forEach((members, colIndex) => {
        const colCenterX = colWidth * (colIndex + 0.5);
        const servers = members.filter((n) => n.report.mode === "server");
        const clients = members.filter((n) => n.report.mode !== "server");
        const spread = (list, y) => {
          list.forEach((n, i) => {
            const offset = (i - (list.length - 1) / 2) * 70;
            positions.set(n.key, { x: colCenterX + offset, y });
          });
        };
        spread(servers, serverRowY);
        spread(clients, clientRowY);
      });

      return positions;
    }

    // 把 poll 拿到的節點集合套進 `sim.nodes`：消失的節點移除，新出現的節點
    // 用 `computeLayout` 算出的座標建立，本來就有的節點座標也用最新算出的
    // 值覆蓋（固定佈局每次 poll 都是同一套規則重算，不保留舊座標）。
    function applyPoll(nodesData, hasServer, width, height) {
      const layout = computeLayout(nodesData, hasServer, width, height);
      const wanted = new Set(nodesData.keys());
      if (hasServer) wanted.add("broker");
      for (const key of [...sim.nodes.keys()]) {
        if (!wanted.has(key)) sim.nodes.delete(key);
      }
      for (const key of wanted) {
        const pos = layout.get(key) || { x: width / 2, y: height / 2 };
        let n = sim.nodes.get(key);
        if (!n) {
          n = { x: pos.x, y: pos.y, isBroker: key === "broker", data: null };
          sim.nodes.set(key, n);
        } else {
          n.x = pos.x;
          n.y = pos.y;
        }
        if (key !== "broker") n.data = nodesData.get(key);
      }
    }

    function isAlive(n) {
      return !!n.data && n.data.ageSecs * 1000 < TOPOLOGY_ALIVE_TTL_MS;
    }

    function roleColor(mode) {
      if (mode === "server") return "#6f9dff";
      if (mode === "client") return "#4caf7d";
      return "#5a6272";
    }

    function showTooltip(key) {
      const n = sim.nodes.get(key);
      if (!n || n.isBroker || !n.data) {
        tooltip.style.display = "none";
        return;
      }
      const r = n.data.report;
      const alive = isAlive(n) ? "是" : "否";
      tooltip.textContent =
        `id: ${r.id}\nip: ${r.ip}\nos: ${r.os}\nversion: ${r.version}\nmode: ${r.mode}\n` +
        `device uptime: ${formatUptimeJs(r.device_uptime_secs)}\napp uptime: ${formatUptimeJs(r.app_uptime_secs)}\n` +
        `disk: ${formatDiskJs(r.disk_free_bytes, r.disk_total_bytes)}\nalive: ${alive}`;
      tooltip.style.display = "block";
    }

    function positionTooltip(e) {
      if (tooltip.style.display !== "block") return;
      const rect = container.getBoundingClientRect();
      tooltip.style.left = `${e.clientX - rect.left + 12}px`;
      tooltip.style.top = `${e.clientY - rect.top + 12}px`;
    }

    function hideTooltip() {
      tooltip.style.display = "none";
    }

    function buildNodeEl(key, n) {
      const group = document.createElementNS(SVGNS, "g");
      group.setAttribute("class", "topology-node");
      let shape;
      if (n.isBroker) {
        shape = document.createElementNS(SVGNS, "polygon");
        shape.setAttribute("class", "topology-broker");
        shape.setAttribute("points", "0,-20 17,-10 17,10 0,20 -17,10 -17,-10");
      } else {
        shape = document.createElementNS(SVGNS, "circle");
        shape.setAttribute("r", "18");
      }
      group.appendChild(shape);
      const label = document.createElementNS(SVGNS, "text");
      label.setAttribute("class", "topology-node-label");
      label.setAttribute("y", "32");
      group.appendChild(label);
      nodeLayer.appendChild(group);
      group.addEventListener("mouseenter", () => showTooltip(key));
      group.addEventListener("mousemove", positionTooltip);
      group.addEventListener("mouseleave", hideTooltip);
      return { group, shape, label };
    }

    function render() {
      // domain 外框：只框 `data.domain` 有值的節點（`local/${id}` 那種
      // domain 名稱不明的節點不畫框）。
      const groups = new Map();
      for (const [, n] of sim.nodes) {
        if (n.isBroker || !n.data || !n.data.domain) continue;
        if (!groups.has(n.data.domain)) groups.set(n.data.domain, []);
        groups.get(n.data.domain).push(n);
      }
      const seenDomains = new Set();
      for (const [domain, members] of groups) {
        seenDomains.add(domain);
        const pad = 30;
        const xs = members.map((m) => m.x);
        const ys = members.map((m) => m.y);
        const x = Math.min(...xs) - pad;
        const y = Math.min(...ys) - pad;
        const w = Math.max(...xs) - Math.min(...xs) + pad * 2;
        const h = Math.max(...ys) - Math.min(...ys) + pad * 2;
        let box = domainEls.get(domain);
        if (!box) {
          box = { rect: document.createElementNS(SVGNS, "rect"), label: document.createElementNS(SVGNS, "text") };
          box.rect.setAttribute("class", "topology-domain-box");
          box.label.setAttribute("class", "topology-domain-label");
          domainLayer.appendChild(box.rect);
          domainLayer.appendChild(box.label);
          domainEls.set(domain, box);
        }
        box.rect.setAttribute("x", x);
        box.rect.setAttribute("y", y);
        box.rect.setAttribute("width", w);
        box.rect.setAttribute("height", h);
        box.rect.setAttribute("rx", 12);
        box.label.setAttribute("x", x + 8);
        box.label.setAttribute("y", y + 16);
        box.label.textContent = domain;
      }
      for (const [domain, box] of [...domainEls]) {
        if (!seenDomains.has(domain)) {
          box.rect.remove();
          box.label.remove();
          domainEls.delete(domain);
        }
      }

      // 邊
      const seenEdges = new Set();
      for (const edge of sim.edges) {
        const a = sim.nodes.get(edge.from);
        const b = sim.nodes.get(edge.to);
        if (!a || !b) continue;
        const ekey = `${edge.from}|${edge.to}`;
        seenEdges.add(ekey);
        let line = edgeEls.get(ekey);
        if (!line) {
          line = document.createElementNS(SVGNS, "line");
          line.setAttribute("class", "topology-edge");
          edgeLayer.appendChild(line);
          edgeEls.set(ekey, line);
        }
        line.setAttribute("x1", a.x);
        line.setAttribute("y1", a.y);
        line.setAttribute("x2", b.x);
        line.setAttribute("y2", b.y);
      }
      for (const [ekey, line] of [...edgeEls]) {
        if (!seenEdges.has(ekey)) {
          line.remove();
          edgeEls.delete(ekey);
        }
      }

      // 節點
      const seenNodes = new Set();
      for (const [key, n] of sim.nodes) {
        seenNodes.add(key);
        let el = nodeEls.get(key);
        if (!el) {
          el = buildNodeEl(key, n);
          nodeEls.set(key, el);
        }
        el.group.setAttribute("transform", `translate(${n.x},${n.y})`);
        if (n.isBroker) {
          el.label.textContent = "MQTT broker";
        } else if (n.data) {
          const alive = isAlive(n);
          el.shape.setAttribute("fill", alive ? roleColor(n.data.report.mode) : "#3a4150");
          el.shape.setAttribute("stroke", alive ? "none" : "#6b7280");
          el.shape.setAttribute("stroke-width", alive ? "0" : "2");
          el.shape.setAttribute("stroke-dasharray", alive ? "0" : "3,3");
          el.label.textContent = n.data.report.id;
        }
      }
      for (const [key, el] of [...nodeEls]) {
        if (!seenNodes.has(key)) {
          el.group.remove();
          nodeEls.delete(key);
        }
      }
    }

    async function pollOnce() {
      const nodesData = await fetchTopologyNodes();
      const { edges, hasServer } = computeTopologyEdges(nodesData);
      const { width, height } = panelSize();
      applyPoll(nodesData, hasServer, width, height);
      sim.edges = edges;
      render();
    }

    pollOnce();
    const intervalId = setInterval(pollOnce, TOPOLOGY_POLL_MS);

    return {
      container,
      stop() {
        clearInterval(intervalId);
      },
    };
  }
```

- [ ] **Step 3: 語法檢查**

```bash
python3 -c "
import re
html = open('src/web/frontend.html', encoding='utf-8').read()
scripts = re.findall(r'<script>(.*?)</script>', html, re.S)
print(len(scripts), 'script block(s)')
open('/tmp/main_script.js', 'w', encoding='utf-8').write(scripts[2])
"
node --check /tmp/main_script.js && echo "syntax OK"
```

Expected: `3 script block(s)`（index 2 是含 `startTopology` 的主程式區塊，
跟改動前一致），`syntax OK`。

- [ ] **Step 4: 編譯確認**

Run: `cargo build`
Expected: 成功（這個檔案是 `include_str!` 進 Rust 的靜態資源，語法錯誤會在
編譯期就被字串內嵌吃掉不會報錯，但至少確認整個專案還編譯得過，沒有動到
`src/web.rs` 之類的其他檔案）。

- [ ] **Step 5: 手動驗證**

`cargo run`（或 headless），瀏覽器開 `http://127.0.0.1:9759`，點選單
「🌐 topology」，確認：
- 至少一個節點時（本機自己），版面看起來合理，節點不會疊在一起。
- 如果環境裡有多台機器（跨 domain 更好），確認：broker 固定畫在最上方
  置中；每個具名 domain 各佔一欄，欄內 server 在上排、client 在下排；
  不知道 domain 的節點（如果有）排在最右邊一欄，且沒有虛線外框。
- 滑鼠**不能**再拖動節點（這次刻意拿掉，確認按住節點不會移動它）。
- hover 節點會顯示 tooltip，內容欄位跟改版前一致。
- 存活/離線的視覺樣式（灰底虛線）、broker 六角形、domain 虛線外框顏色都
  跟改版前一致（這次沒有動視覺樣式）。
- 隔一段時間（等一次 poll，2.5 秒）畫面重新整理座標，沒有殘留舊節點/舊
  domain 外框。
- 開 DevTools Console，確認沒有 JS 錯誤。

- [ ] **Step 6: Commit**

```bash
git add src/web/frontend.html
git commit -m "$(cat <<'EOF'
web：topology panel 改成固定式階層佈局，拿掉力導向模擬跟節點拖曳

broker 固定最上排置中，domain 依名稱排序左右各佔一欄，欄內 server 排在
上、client 排在下；不知道 domain 的節點另成一欄排最後。視覺樣式（顏色/
形狀/tooltip）不變，只換座標怎麼算。桌面版原本支援的節點拖曳這次拿掉，
跟 /tablet 版行為一致。
EOF
)"
```

---

### Task 2: `tablet.html`（/tablet）改成固定式階層佈局

**Files:**
- Modify: `src/web/tablet.html:286-585`（`startTopologyTab` 函式內的模擬部分整個換掉）

**Interfaces:**
- Consumes: 既有的 `SVGNS`、`ALIVE_TTL_MS`、`TOPOLOGY_POLL_MS`、`fetchTopologyNodes()`、`computeTopologyEdges(nodes)`、`formatUptimeJs`、`formatDiskJs`（都不變）。
- Produces: 無下游任務依賴這個 task 的產物（跟 Task 1 是各自獨立的改動）。

- [ ] **Step 1: 換掉函式前的說明註解**

把（第 286-290 行）：

```js
  // 建立 topology 分頁：定期 poll 上面兩個端點合併節點、跑一個手刻的簡化版
  // 力導向模擬（節點互推、邊當彈簧、往中心的微弱向心力、每幀衰減速度），
  // 用 SVG 逐幀更新座標。跟桌面版 topology panel 同一套演算法，唯一差異是
  // **這次不支援手指拖動節點**（純瀏覽）。回傳 `stop` 函式，切分頁時呼叫，
  // 清掉 interval／animation frame，不留背景在跑。
```

改成：

```js
  // 建立 topology 分頁：定期 poll 上面兩個端點合併節點、依角色（broker/
  // server/client）＋ domain 直接算出固定座標（見 `computeLayout`），跟
  // 桌面版 topology panel 同一套演算法（各自一份程式碼，不共用模組）。
  // 回傳 `stop` 函式，切分頁時呼叫，清掉 interval，不留背景在跑。
```

- [ ] **Step 2: 換掉 `sim`／座標計算／`tick`／`loop` 這幾個部分**

把（第 308-585 行，從 `const sim = ...` 到 `startTopologyTab` 結尾的 `}`）：

```js
    const sim = { nodes: new Map(), edges: [] };
    const domainEls = new Map();
    const edgeEls = new Map();
    const nodeEls = new Map();

    function panelSize() {
      const rect = svg.getBoundingClientRect();
      return { width: rect.width || 400, height: rect.height || 300 };
    }

    function makeNode(isBroker) {
      const { width, height } = panelSize();
      return {
        x: width / 2 + (Math.random() - 0.5) * 60,
        y: height / 2 + (Math.random() - 0.5) * 60,
        vx: 0,
        vy: 0,
        fx: 0,
        fy: 0,
        isBroker,
        data: null,
      };
    }

    function applyPoll(nodesData, hasServer) {
      const wanted = new Set(nodesData.keys());
      if (hasServer) wanted.add("broker");
      for (const key of [...sim.nodes.keys()]) {
        if (!wanted.has(key)) sim.nodes.delete(key);
      }
      for (const key of wanted) {
        let n = sim.nodes.get(key);
        if (!n) {
          n = makeNode(key === "broker");
          sim.nodes.set(key, n);
        }
        if (key !== "broker") n.data = nodesData.get(key);
      }
    }

    function tick() {
      const { width, height } = panelSize();
      const entries = [...sim.nodes.values()];
      for (const n of entries) {
        n.fx = 0;
        n.fy = 0;
      }

      const REPULSION = 2200;
      for (let i = 0; i < entries.length; i++) {
        for (let j = i + 1; j < entries.length; j++) {
          const a = entries[i];
          const b = entries[j];
          const dx = a.x - b.x;
          const dy = a.y - b.y;
          let distSq = dx * dx + dy * dy;
          if (distSq < 1) distSq = 1;
          const dist = Math.sqrt(distSq);
          const force = REPULSION / distSq;
          const fx = (dx / dist) * force;
          const fy = (dy / dist) * force;
          a.fx += fx;
          a.fy += fy;
          b.fx -= fx;
          b.fy -= fy;
        }
      }

      const SPRING_LENGTH = 90;
      const SPRING_STRENGTH = 0.02;
      for (const edge of sim.edges) {
        const a = sim.nodes.get(edge.from);
        const b = sim.nodes.get(edge.to);
        if (!a || !b) continue;
        const dx = b.x - a.x;
        const dy = b.y - a.y;
        const dist = Math.sqrt(dx * dx + dy * dy) || 1;
        const stretch = dist - SPRING_LENGTH;
        const fx = (dx / dist) * stretch * SPRING_STRENGTH;
        const fy = (dy / dist) * stretch * SPRING_STRENGTH;
        a.fx += fx;
        a.fy += fy;
        b.fx -= fx;
        b.fy -= fy;
      }

      const CENTER_STRENGTH = 0.01;
      const DAMPING = 0.85;
      for (const n of entries) {
        n.fx += (width / 2 - n.x) * CENTER_STRENGTH;
        n.fy += (height / 2 - n.y) * CENTER_STRENGTH;
        n.vx = (n.vx + n.fx) * DAMPING;
        n.vy = (n.vy + n.fy) * DAMPING;
        n.x += n.vx;
        n.y += n.vy;
      }
    }

    function isAlive(n) {
      return !!n.data && n.data.ageSecs * 1000 < ALIVE_TTL_MS;
    }

    function roleColor(mode) {
      if (mode === "server") return "#6f9dff";
      if (mode === "client") return "#4caf7d";
      return "#5a6272";
    }

    function showTooltip(key) {
      const n = sim.nodes.get(key);
      if (!n || n.isBroker || !n.data) {
        tooltip.style.display = "none";
        return;
      }
      const r = n.data.report;
      const alive = isAlive(n) ? "是" : "否";
      tooltip.textContent =
        `id: ${r.id}\nip: ${r.ip}\nos: ${r.os}\nversion: ${r.version}\nmode: ${r.mode}\n` +
        `device uptime: ${formatUptimeJs(r.device_uptime_secs)}\napp uptime: ${formatUptimeJs(r.app_uptime_secs)}\n` +
        `disk: ${formatDiskJs(r.disk_free_bytes, r.disk_total_bytes)}\nalive: ${alive}`;
      tooltip.style.display = "block";
    }

    function positionTooltip(e) {
      if (tooltip.style.display !== "block") return;
      const rect = wrap.getBoundingClientRect();
      tooltip.style.left = `${e.clientX - rect.left + 12}px`;
      tooltip.style.top = `${e.clientY - rect.top + 12}px`;
    }

    function hideTooltip() {
      tooltip.style.display = "none";
    }

    function buildNodeEl(key, n) {
      const group = document.createElementNS(SVGNS, "g");
      group.setAttribute("class", "topology-node");
      let shape;
      if (n.isBroker) {
        shape = document.createElementNS(SVGNS, "polygon");
        shape.setAttribute("class", "topology-broker");
        shape.setAttribute("points", "0,-20 17,-10 17,10 0,20 -17,10 -17,-10");
      } else {
        shape = document.createElementNS(SVGNS, "circle");
        shape.setAttribute("r", "18");
      }
      group.appendChild(shape);
      const label = document.createElementNS(SVGNS, "text");
      label.setAttribute("class", "topology-node-label");
      label.setAttribute("y", "32");
      group.appendChild(label);
      nodeLayer.appendChild(group);
      group.addEventListener("mouseenter", () => showTooltip(key));
      group.addEventListener("mousemove", positionTooltip);
      group.addEventListener("mouseleave", hideTooltip);
      return { group, shape, label };
    }

    function render() {
      const groups = new Map();
      for (const [, n] of sim.nodes) {
        if (n.isBroker || !n.data || !n.data.domain) continue;
        if (!groups.has(n.data.domain)) groups.set(n.data.domain, []);
        groups.get(n.data.domain).push(n);
      }
      const seenDomains = new Set();
      for (const [domain, members] of groups) {
        seenDomains.add(domain);
        const pad = 30;
        const xs = members.map((m) => m.x);
        const ys = members.map((m) => m.y);
        const x = Math.min(...xs) - pad;
        const y = Math.min(...ys) - pad;
        const w = Math.max(...xs) - Math.min(...xs) + pad * 2;
        const h = Math.max(...ys) - Math.min(...ys) + pad * 2;
        let box = domainEls.get(domain);
        if (!box) {
          box = { rect: document.createElementNS(SVGNS, "rect"), label: document.createElementNS(SVGNS, "text") };
          box.rect.setAttribute("class", "topology-domain-box");
          box.label.setAttribute("class", "topology-domain-label");
          domainLayer.appendChild(box.rect);
          domainLayer.appendChild(box.label);
          domainEls.set(domain, box);
        }
        box.rect.setAttribute("x", x);
        box.rect.setAttribute("y", y);
        box.rect.setAttribute("width", w);
        box.rect.setAttribute("height", h);
        box.rect.setAttribute("rx", 12);
        box.label.setAttribute("x", x + 8);
        box.label.setAttribute("y", y + 16);
        box.label.textContent = domain;
      }
      for (const [domain, box] of [...domainEls]) {
        if (!seenDomains.has(domain)) {
          box.rect.remove();
          box.label.remove();
          domainEls.delete(domain);
        }
      }

      const seenEdges = new Set();
      for (const edge of sim.edges) {
        const a = sim.nodes.get(edge.from);
        const b = sim.nodes.get(edge.to);
        if (!a || !b) continue;
        const ekey = `${edge.from}|${edge.to}`;
        seenEdges.add(ekey);
        let line = edgeEls.get(ekey);
        if (!line) {
          line = document.createElementNS(SVGNS, "line");
          line.setAttribute("class", "topology-edge");
          edgeLayer.appendChild(line);
          edgeEls.set(ekey, line);
        }
        line.setAttribute("x1", a.x);
        line.setAttribute("y1", a.y);
        line.setAttribute("x2", b.x);
        line.setAttribute("y2", b.y);
      }
      for (const [ekey, line] of [...edgeEls]) {
        if (!seenEdges.has(ekey)) {
          line.remove();
          edgeEls.delete(ekey);
        }
      }

      const seenNodes = new Set();
      for (const [key, n] of sim.nodes) {
        seenNodes.add(key);
        let el = nodeEls.get(key);
        if (!el) {
          el = buildNodeEl(key, n);
          nodeEls.set(key, el);
        }
        el.group.setAttribute("transform", `translate(${n.x},${n.y})`);
        if (n.isBroker) {
          el.label.textContent = "MQTT broker";
        } else if (n.data) {
          const alive = isAlive(n);
          el.shape.setAttribute("fill", alive ? roleColor(n.data.report.mode) : "#3a4150");
          el.shape.setAttribute("stroke", alive ? "none" : "#6b7280");
          el.shape.setAttribute("stroke-width", alive ? "0" : "2");
          el.shape.setAttribute("stroke-dasharray", alive ? "0" : "3,3");
          el.label.textContent = n.data.report.id;
        }
      }
      for (const [key, el] of [...nodeEls]) {
        if (!seenNodes.has(key)) {
          el.group.remove();
          nodeEls.delete(key);
        }
      }
    }

    async function pollOnce() {
      const nodesData = await fetchTopologyNodes();
      const { edges, hasServer } = computeTopologyEdges(nodesData);
      applyPoll(nodesData, hasServer);
      sim.edges = edges;
    }

    let rafId = null;
    function loop() {
      tick();
      render();
      rafId = requestAnimationFrame(loop);
    }

    pollOnce();
    loop();
    const intervalId = setInterval(pollOnce, TOPOLOGY_POLL_MS);

    return function stop() {
      clearInterval(intervalId);
      if (rafId) cancelAnimationFrame(rafId);
    };
  }
```

改成：

```js
    const sim = { nodes: new Map(), edges: [] };
    const domainEls = new Map();
    const edgeEls = new Map();
    const nodeEls = new Map();

    function panelSize() {
      const rect = svg.getBoundingClientRect();
      return { width: rect.width || 400, height: rect.height || 300 };
    }

    // 依角色＋domain 算出每個節點的固定座標：broker 置中最上排；具名 domain
    // 依名稱排序由左到右各佔一欄，不知道 domain 名稱的節點（`local/${id}`）
    // 另成一欄排在最後；欄內 server 排在上、client（含 standalone）排在下，
    // 同排多個節點在欄寬內水平等距展開。回傳 `Map<key, {x, y}>`（`key` 是
    // 節點 key 或 `"broker"`）。
    function computeLayout(nodesData, hasServer, width, height) {
      const domainOrder = [];
      const domainMembers = new Map();
      const localMembers = [];
      for (const n of nodesData.values()) {
        if (n.domain) {
          if (!domainMembers.has(n.domain)) {
            domainMembers.set(n.domain, []);
            domainOrder.push(n.domain);
          }
          domainMembers.get(n.domain).push(n);
        } else {
          localMembers.push(n);
        }
      }
      domainOrder.sort();
      const columns = domainOrder.map((d) => domainMembers.get(d));
      if (localMembers.length > 0) columns.push(localMembers);

      const positions = new Map();
      const brokerY = height * 0.2;
      const serverRowY = height * 0.5;
      const clientRowY = height * 0.8;

      if (hasServer) positions.set("broker", { x: width / 2, y: brokerY });

      const colWidth = columns.length > 0 ? width / columns.length : width;
      columns.forEach((members, colIndex) => {
        const colCenterX = colWidth * (colIndex + 0.5);
        const servers = members.filter((n) => n.report.mode === "server");
        const clients = members.filter((n) => n.report.mode !== "server");
        const spread = (list, y) => {
          list.forEach((n, i) => {
            const offset = (i - (list.length - 1) / 2) * 70;
            positions.set(n.key, { x: colCenterX + offset, y });
          });
        };
        spread(servers, serverRowY);
        spread(clients, clientRowY);
      });

      return positions;
    }

    // 把 poll 拿到的節點集合套進 `sim.nodes`：消失的節點移除，新出現的節點
    // 用 `computeLayout` 算出的座標建立，本來就有的節點座標也用最新算出的
    // 值覆蓋（固定佈局每次 poll 都是同一套規則重算，不保留舊座標）。
    function applyPoll(nodesData, hasServer, width, height) {
      const layout = computeLayout(nodesData, hasServer, width, height);
      const wanted = new Set(nodesData.keys());
      if (hasServer) wanted.add("broker");
      for (const key of [...sim.nodes.keys()]) {
        if (!wanted.has(key)) sim.nodes.delete(key);
      }
      for (const key of wanted) {
        const pos = layout.get(key) || { x: width / 2, y: height / 2 };
        let n = sim.nodes.get(key);
        if (!n) {
          n = { x: pos.x, y: pos.y, isBroker: key === "broker", data: null };
          sim.nodes.set(key, n);
        } else {
          n.x = pos.x;
          n.y = pos.y;
        }
        if (key !== "broker") n.data = nodesData.get(key);
      }
    }

    function isAlive(n) {
      return !!n.data && n.data.ageSecs * 1000 < ALIVE_TTL_MS;
    }

    function roleColor(mode) {
      if (mode === "server") return "#6f9dff";
      if (mode === "client") return "#4caf7d";
      return "#5a6272";
    }

    function showTooltip(key) {
      const n = sim.nodes.get(key);
      if (!n || n.isBroker || !n.data) {
        tooltip.style.display = "none";
        return;
      }
      const r = n.data.report;
      const alive = isAlive(n) ? "是" : "否";
      tooltip.textContent =
        `id: ${r.id}\nip: ${r.ip}\nos: ${r.os}\nversion: ${r.version}\nmode: ${r.mode}\n` +
        `device uptime: ${formatUptimeJs(r.device_uptime_secs)}\napp uptime: ${formatUptimeJs(r.app_uptime_secs)}\n` +
        `disk: ${formatDiskJs(r.disk_free_bytes, r.disk_total_bytes)}\nalive: ${alive}`;
      tooltip.style.display = "block";
    }

    function positionTooltip(e) {
      if (tooltip.style.display !== "block") return;
      const rect = wrap.getBoundingClientRect();
      tooltip.style.left = `${e.clientX - rect.left + 12}px`;
      tooltip.style.top = `${e.clientY - rect.top + 12}px`;
    }

    function hideTooltip() {
      tooltip.style.display = "none";
    }

    function buildNodeEl(key, n) {
      const group = document.createElementNS(SVGNS, "g");
      group.setAttribute("class", "topology-node");
      let shape;
      if (n.isBroker) {
        shape = document.createElementNS(SVGNS, "polygon");
        shape.setAttribute("class", "topology-broker");
        shape.setAttribute("points", "0,-20 17,-10 17,10 0,20 -17,10 -17,-10");
      } else {
        shape = document.createElementNS(SVGNS, "circle");
        shape.setAttribute("r", "18");
      }
      group.appendChild(shape);
      const label = document.createElementNS(SVGNS, "text");
      label.setAttribute("class", "topology-node-label");
      label.setAttribute("y", "32");
      group.appendChild(label);
      nodeLayer.appendChild(group);
      group.addEventListener("mouseenter", () => showTooltip(key));
      group.addEventListener("mousemove", positionTooltip);
      group.addEventListener("mouseleave", hideTooltip);
      return { group, shape, label };
    }

    function render() {
      const groups = new Map();
      for (const [, n] of sim.nodes) {
        if (n.isBroker || !n.data || !n.data.domain) continue;
        if (!groups.has(n.data.domain)) groups.set(n.data.domain, []);
        groups.get(n.data.domain).push(n);
      }
      const seenDomains = new Set();
      for (const [domain, members] of groups) {
        seenDomains.add(domain);
        const pad = 30;
        const xs = members.map((m) => m.x);
        const ys = members.map((m) => m.y);
        const x = Math.min(...xs) - pad;
        const y = Math.min(...ys) - pad;
        const w = Math.max(...xs) - Math.min(...xs) + pad * 2;
        const h = Math.max(...ys) - Math.min(...ys) + pad * 2;
        let box = domainEls.get(domain);
        if (!box) {
          box = { rect: document.createElementNS(SVGNS, "rect"), label: document.createElementNS(SVGNS, "text") };
          box.rect.setAttribute("class", "topology-domain-box");
          box.label.setAttribute("class", "topology-domain-label");
          domainLayer.appendChild(box.rect);
          domainLayer.appendChild(box.label);
          domainEls.set(domain, box);
        }
        box.rect.setAttribute("x", x);
        box.rect.setAttribute("y", y);
        box.rect.setAttribute("width", w);
        box.rect.setAttribute("height", h);
        box.rect.setAttribute("rx", 12);
        box.label.setAttribute("x", x + 8);
        box.label.setAttribute("y", y + 16);
        box.label.textContent = domain;
      }
      for (const [domain, box] of [...domainEls]) {
        if (!seenDomains.has(domain)) {
          box.rect.remove();
          box.label.remove();
          domainEls.delete(domain);
        }
      }

      const seenEdges = new Set();
      for (const edge of sim.edges) {
        const a = sim.nodes.get(edge.from);
        const b = sim.nodes.get(edge.to);
        if (!a || !b) continue;
        const ekey = `${edge.from}|${edge.to}`;
        seenEdges.add(ekey);
        let line = edgeEls.get(ekey);
        if (!line) {
          line = document.createElementNS(SVGNS, "line");
          line.setAttribute("class", "topology-edge");
          edgeLayer.appendChild(line);
          edgeEls.set(ekey, line);
        }
        line.setAttribute("x1", a.x);
        line.setAttribute("y1", a.y);
        line.setAttribute("x2", b.x);
        line.setAttribute("y2", b.y);
      }
      for (const [ekey, line] of [...edgeEls]) {
        if (!seenEdges.has(ekey)) {
          line.remove();
          edgeEls.delete(ekey);
        }
      }

      const seenNodes = new Set();
      for (const [key, n] of sim.nodes) {
        seenNodes.add(key);
        let el = nodeEls.get(key);
        if (!el) {
          el = buildNodeEl(key, n);
          nodeEls.set(key, el);
        }
        el.group.setAttribute("transform", `translate(${n.x},${n.y})`);
        if (n.isBroker) {
          el.label.textContent = "MQTT broker";
        } else if (n.data) {
          const alive = isAlive(n);
          el.shape.setAttribute("fill", alive ? roleColor(n.data.report.mode) : "#3a4150");
          el.shape.setAttribute("stroke", alive ? "none" : "#6b7280");
          el.shape.setAttribute("stroke-width", alive ? "0" : "2");
          el.shape.setAttribute("stroke-dasharray", alive ? "0" : "3,3");
          el.label.textContent = n.data.report.id;
        }
      }
      for (const [key, el] of [...nodeEls]) {
        if (!seenNodes.has(key)) {
          el.group.remove();
          nodeEls.delete(key);
        }
      }
    }

    async function pollOnce() {
      const nodesData = await fetchTopologyNodes();
      const { edges, hasServer } = computeTopologyEdges(nodesData);
      const { width, height } = panelSize();
      applyPoll(nodesData, hasServer, width, height);
      sim.edges = edges;
      render();
    }

    pollOnce();
    const intervalId = setInterval(pollOnce, TOPOLOGY_POLL_MS);

    return function stop() {
      clearInterval(intervalId);
    };
  }
```

- [ ] **Step 3: 語法檢查**

```bash
python3 -c "
import re
html = open('src/web/tablet.html', encoding='utf-8').read()
scripts = re.findall(r'<script>(.*?)</script>', html, re.S)
print(len(scripts), 'script block(s)')
open('/tmp/tablet_script.js', 'w', encoding='utf-8').write(scripts[0])
"
node --check /tmp/tablet_script.js && echo "syntax OK"
```

Expected: `1 script block(s)`、`syntax OK`。

- [ ] **Step 4: 編譯確認**

Run: `cargo build`
Expected: 成功。

- [ ] **Step 5: 手動驗證**

`cargo run`（或 headless），瀏覽器開 `http://127.0.0.1:9759/tablet`，點
topology 分頁，確認：
- 至少一個節點時（本機自己），版面看起來合理。
- 如果環境裡有多台機器，broker/domain 欄/server 排/client 排的排列方式
  跟桌面版（Task 1）驗證時一致。
- 切到別的分頁再切回來，圖能重新正常顯示（`stop()`/重新 `start()` 的
  生命週期正確，這點沒有改動，但改動後要重新確認一次）。
- 切離開 topology 分頁時，用 DevTools 確認沒有殘留的 `setInterval`（原本
  也要確認沒有 `requestAnimationFrame`，這次拿掉了 rAF 迴圈，所以不用再
  檢查這項）。
- hover tooltip、alive/離線樣式跟改版前一致。
- 開 DevTools Console，確認沒有 JS 錯誤。

- [ ] **Step 6: Commit**

```bash
git add src/web/tablet.html
git commit -m "$(cat <<'EOF'
/tablet：topology 分頁改成固定式階層佈局，拿掉力導向模擬

跟桌面版 topology panel 同一套固定佈局規則（各自一份程式碼，不共用
模組）：broker 最上排置中，domain 各佔一欄，欄內 server 在上、client
在下。視覺樣式不變。
EOF
)"
```

---

### Task 3: 端對端確認

**Files:** 無新增/修改檔案，純驗證。

- [ ] **Step 1: 全專案編譯與既有測試**

Run: `cargo build && cargo test`
Expected: 全部成功，134 passed，沒有新的編譯警告。

- [ ] **Step 2: 兩邊 topology 一起比對**

依照 Task 1 Step 5、Task 2 Step 5 的手動驗證步驟，這次額外確認桌面版
`http://127.0.0.1:9759` 跟 `/tablet` 版 `http://127.0.0.1:9759/tablet`
的 topology 用同一組測試資料（同一台/同幾台機器）看起來排列方式一致
（同樣的 domain 順序、同樣的 server/client 排列規則），沒有因為各自
一份程式碼而出現行為不一致的地方。

（這個 task 純粹是驗證，沒有新的程式碼異動，不需要 commit。）
