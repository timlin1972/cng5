# topology panel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增一個獨立於 `global`/`device` 表格之外的 web UI panel（跟 `player`/`shell` 一樣是選單裡獨立一項），把「哪些機器、哪個 domain、誰連誰（client→server、server→MQTT broker）」畫成一張力導向節點圖，滑鼠移到節點上顯示基本資料。

**Architecture:** 後端只補一個既有資料結構缺的欄位（`DeviceReport.server_addr`，讓別人也能看出某台 client 設定要連誰），資料來源沿用現成的 `/api/device/list`／`/api/global/list` 兩個唯讀端點，前端定期 poll＋合併＋手刻簡化版力導向模擬＋SVG 繪製，完全不新增後端端點。

**Tech Stack:** Rust（`serde` 既有的 `#[serde(default)]` 欄位相容模式），純 JS＋內嵌 SVG（`frontend.html`，無框架、無新依賴）。

## Global Constraints

- 不新增任何 Cargo 依賴，也不在前端加任何繪圖/力導向 JS 函式庫（例如 D3.js）——手刻簡化版力導向模擬。
- 不新增任何後端 HTTP/SSE 端點——資料完全來自既有的 `/api/device/list`／`/api/global/list`。
- 圖上的邊（連線）代表「設定上的關係」（client 設定要連哪個 server、server 設定要連 MQTT broker），不即時驗證真的連上、不代表實體網路路徑。
- 存活門檻跟後端 `ALIVE_TTL`（`REPORT_INTERVAL(10s) * 3` = 30 秒）保持一致，前端寫死 `30000`ms。
- 離線的節點維持顯示（灰底虛線邊框），不從圖上移除。

---

### Task 1: `DeviceReport` 補上 `server_addr` 欄位

**Files:**
- Modify: `src/plugin.rs:22-51`（`DeviceReport` struct 定義）
- Modify: `src/plugins/system.rs:157-200`（`spawn_reporter`／`build_report`）
- Modify: `src/plugins/device.rs:189-201`（測試用的 `make_report` fixture）

**Interfaces:**
- Produces: `DeviceReport.server_addr: Option<String>`（`#[serde(default)]`，序列化進 `/api/device/list`／`/api/global/list`／`/api/device/register` 的既有 JSON 裡，Task 2 的前端會讀這個欄位）。

- [ ] **Step 1: `DeviceReport` 新增欄位**

把（`src/plugin.rs`，`DeviceReport` struct 定義結尾）：

```rust
    /// 同上，總共容量（bytes）。
    #[serde(default)]
    pub disk_total_bytes: u64,
}
```

改成：

```rust
    /// 同上，總共容量（bytes）。
    #[serde(default)]
    pub disk_total_bytes: u64,
    /// mode 是 client 且設定過 `server <ip>` 時，是這個 client 目前要推播/
    /// 拉清單的目標 ip；`server`／`standalone` 角色，或 client 還沒設定過
    /// server，都是 `None`。給 web UI 的 topology panel 畫「client 連到哪個
    /// server」那條線用（見 `system.rs` 的 `build_report`）。舊版（還沒有
    /// 這個欄位的 build）傳過來的 JSON 缺這個 key 時解析成 `None`，跟其他
    /// 欄位同一套「缺欄位不能讓整筆資料解析失敗」的理由。
    #[serde(default)]
    pub server_addr: Option<String>,
}
```

- [ ] **Step 2: `build_report` 簽名加 `server_addr` 參數，只有 client 角色才真的填**

把（`src/plugins/system.rs`）：

```rust
    fn build_report(id: &str, tailscale: &TailscaleCache, mode: SystemMode) -> DeviceReport {
        let tailscale_ip = tailscale.get();
        let ip = tailscale_ip.clone().unwrap_or_else(sysinfo::local_ip);
        let (disk_free_bytes, disk_total_bytes) = sysinfo::disk_usage(Path::new(".")).unwrap_or((0, 0));
        DeviceReport {
            id: id.to_string(),
            ip,
            os: sysinfo::os().to_string(),
            version: APP_VERSION.to_string(),
            tailscale: tailscale_ip.is_some(),
            mode: mode.as_str().to_string(),
            device_uptime_secs: sysinfo::device_uptime_secs(),
            app_uptime_secs: sysinfo::app_uptime_secs(),
            disk_free_bytes,
            disk_total_bytes,
        }
    }
```

改成：

```rust
    /// `server_addr` 只在 `mode == Client` 時才會真的寫進回報內容——`server`／
    /// `standalone` 角色即使 `ContextInner.server_addr` 底下還留著舊值（例如
    /// 使用者之前設過、後來切換角色但沒清掉），也一律回報成 `None`，避免
    /// topology panel 誤判成「這台機器現在正在推播給誰」。
    fn build_report(
        id: &str,
        tailscale: &TailscaleCache,
        mode: SystemMode,
        server_addr: Option<String>,
    ) -> DeviceReport {
        let tailscale_ip = tailscale.get();
        let ip = tailscale_ip.clone().unwrap_or_else(sysinfo::local_ip);
        let (disk_free_bytes, disk_total_bytes) = sysinfo::disk_usage(Path::new(".")).unwrap_or((0, 0));
        DeviceReport {
            id: id.to_string(),
            ip,
            os: sysinfo::os().to_string(),
            version: APP_VERSION.to_string(),
            tailscale: tailscale_ip.is_some(),
            mode: mode.as_str().to_string(),
            device_uptime_secs: sysinfo::device_uptime_secs(),
            app_uptime_secs: sysinfo::app_uptime_secs(),
            disk_free_bytes,
            disk_total_bytes,
            server_addr: if mode == SystemMode::Client { server_addr } else { None },
        }
    }
```

- [ ] **Step 3: `spawn_reporter` 先讀 `server_addr` 再組 report**

把（`src/plugins/system.rs`）：

```rust
        thread::spawn(move || loop {
            let current_mode = *mode.lock().unwrap();
            let report = Self::build_report(&id, &tailscale, current_mode);
            let server_addr = {
                let mut inner = ctx.lock().unwrap();
                inner.devices.insert(id.clone(), DeviceEntry { report: report.clone(), last_seen: Instant::now() });
                inner.server_addr.clone()
            };
            if current_mode == SystemMode::Client
                && let Some(addr) = server_addr
            {
```

改成：

```rust
        thread::spawn(move || loop {
            let current_mode = *mode.lock().unwrap();
            let server_addr = ctx.lock().unwrap().server_addr.clone();
            let report = Self::build_report(&id, &tailscale, current_mode, server_addr.clone());
            {
                let mut inner = ctx.lock().unwrap();
                inner.devices.insert(id.clone(), DeviceEntry { report: report.clone(), last_seen: Instant::now() });
            }
            if current_mode == SystemMode::Client
                && let Some(addr) = server_addr
            {
```

（後面 `ctx.lock().unwrap().log_activity(...)`／`Self::push_report(...)`／`Self::pull_peers(...)`／`thread::sleep(REPORT_INTERVAL);` 這幾行不用改。）

- [ ] **Step 4: 修測試 fixture，讓它繼續編譯**

把（`src/plugins/device.rs`，`make_report` 函式）：

```rust
    fn make_report(id: &str) -> DeviceReport {
        DeviceReport {
            id: id.to_string(),
            ip: "127.0.0.1".to_string(),
            os: "linux".to_string(),
            version: "1.3.0".to_string(),
            tailscale: false,
            mode: "standalone".to_string(),
            device_uptime_secs: 0,
            app_uptime_secs: 0,
            disk_free_bytes: 0,
            disk_total_bytes: 0,
        }
    }
```

改成：

```rust
    fn make_report(id: &str) -> DeviceReport {
        DeviceReport {
            id: id.to_string(),
            ip: "127.0.0.1".to_string(),
            os: "linux".to_string(),
            version: "1.3.0".to_string(),
            tailscale: false,
            mode: "standalone".to_string(),
            device_uptime_secs: 0,
            app_uptime_secs: 0,
            disk_free_bytes: 0,
            disk_total_bytes: 0,
            server_addr: None,
        }
    }
```

- [ ] **Step 5: 編譯＋測試確認**

Run: `cargo build`
Expected: 成功，無錯誤。

Run: `cargo test`
Expected: 134 passed（跟改動前同一個數字，這個 task 沒有新增測試，只是讓既有測試繼續編譯過）。

- [ ] **Step 6: 手動確認欄位有進到 JSON 裡**

啟動程式（`cargo run`），在另一個視窗：
- `curl -s http://127.0.0.1:9759/api/device/list | head -c 400`，確認回應裡每一筆都多了 `"server_addr":null`（還沒設定 `server <ip>` 或 mode 不是 client 的情況）。
- 進互動模式，`plugin enter system` → `mode client` → `server 127.0.0.1`（隨便填一個能通的 ip 測試用），等一輪 `REPORT_INTERVAL`（10 秒）後再 `curl` 一次 `/api/device/list`，確認這台自己的那一筆變成 `"server_addr":"127.0.0.1"`。

- [ ] **Step 7: Commit**

```bash
git add src/plugin.rs src/plugins/system.rs src/plugins/device.rs
git commit -m "$(cat <<'EOF'
DeviceReport 新增 server_addr，讓其他機器也能看出 client 連到哪個 server

給之後的 topology panel 畫 client→server 連線用；server/standalone 角色
一律回報 None，避免誤判成「現在正在推播給誰」。
EOF
)"
```

---

### Task 2: web UI 新增 `topology` panel

**Files:**
- Modify: `src/web/frontend.html`（CSS、新增 JS 函式、`openPanel` 相關的好幾處、選單）

**Interfaces:**
- Consumes: Task 1 的 `DeviceReport.server_addr`（透過 `/api/device/list`／`/api/global/list` 現有的 JSON）。
- Produces: 無下游任務依賴這個 task 的產物（這是這個功能的最後一個 task，除了 Task 3 的手動驗證）。

- [ ] **Step 1: 新增 CSS**

在 `.cell-flash { animation: cellFlash 1s ease-out; }`（第 646 行）之後、
`</style>`（第 647 行）之前，加入：

```css
  .topology-body { display: flex; flex-direction: column; height: 100%; overflow: hidden; position: relative; }
  .topology-svg { flex: 1; width: 100%; height: 100%; }
  .topology-node { cursor: default; }
  .topology-node-label { font-size: 10px; fill: #d8dee9; text-anchor: middle; pointer-events: none; }
  .topology-domain-box { fill: none; stroke: #3a4150; stroke-width: 1; stroke-dasharray: 5,3; }
  .topology-domain-label { font-size: 10px; fill: #6b7280; }
  .topology-edge { stroke: #5a6272; stroke-width: 1.5; }
  .topology-broker { fill: #1b1e26; stroke: #d8dee9; stroke-width: 2; }
  .topology-tooltip {
    position: absolute;
    display: none;
    pointer-events: none;
    background: #1b1e26;
    border: 1px solid #3a4150;
    border-radius: 4px;
    padding: 6px 10px;
    font-size: 12px;
    color: #d8dee9;
    white-space: pre;
    z-index: 20;
  }
```

- [ ] **Step 2: 新增資料合併／格式化／力導向模擬的 JS 函式**

在 `function openPanel(name) {`（第 1663 行）之前加入：

```js
  const SVGNS = "http://www.w3.org/2000/svg";
  // 跟 device.rs/global.rs 的 ALIVE_TTL（REPORT_INTERVAL 10 秒 * 3）保持
  // 一致，這裡沒有對應的 API 可以查，只能寫死並用註解註明依賴關係。
  const TOPOLOGY_ALIVE_TTL_MS = 30000;
  const TOPOLOGY_POLL_MS = 2500;

  // 跟 `sysinfo::format_uptime` 算法一致（秒數 -> "Xd HH:MM:SS" 或
  // "HH:MM:SS"），topology 的 hover tooltip 用——這是目前唯一需要在前端把
  // 原始秒數格式化的地方，其餘表格類 panel 都是後端已經排版好的文字。
  function formatUptimeJs(secs) {
    const days = Math.floor(secs / 86400);
    const hours = Math.floor((secs % 86400) / 3600);
    const minutes = Math.floor((secs % 3600) / 60);
    const seconds = Math.floor(secs % 60);
    const pad = (n) => String(n).padStart(2, "0");
    return days > 0 ? `${days}d ${pad(hours)}:${pad(minutes)}:${pad(seconds)}` : `${pad(hours)}:${pad(minutes)}:${pad(seconds)}`;
  }

  // 跟 `sysinfo::format_disk_usage`/`format_bytes_short` 算法一致。
  function formatDiskJs(free, total) {
    if (total === 0) return "N/A";
    const short = (bytes) => {
      const units = ["B", "K", "M", "G", "T"];
      let value = bytes;
      let unit = 0;
      while (value >= 1024 && unit < units.length - 1) {
        value /= 1024;
        unit++;
      }
      return unit === 0 ? `${bytes}${units[0]}` : `${value.toFixed(0)}${units[unit]}`;
    };
    return `${short(free)}/${short(total)}`;
  }

  // 合併 `/api/device/list`（本地 domain）跟 `/api/global/list`（`domain`
  // 設定過的話包含本地 domain + 其他 domain 透過 MQTT 收到的）成一份節點
  // 清單，key 優先用 `global/list` 給的 `${domain}/${id}`；`device/list`
  // 裡任何 `id` 已經在 `global/list` 出現過的就跳過（同一台機器不畫兩次），
  // 沒出現過的用 `local/${id}` 當 key（代表「不知道 domain 名稱，但本機
  // 看得到」，不畫進任何 domain 外框，見 `render` 對 `domain` 為 null 的
  // 處理）。
  async function fetchTopologyNodes() {
    let deviceList = [];
    let globalList = [];
    try {
      [deviceList, globalList] = await Promise.all([
        fetch("/api/device/list").then((r) => r.json()),
        fetch("/api/global/list").then((r) => r.json()),
      ]);
    } catch (_err) {
      return new Map();
    }
    const nodes = new Map();
    for (const item of globalList) {
      const key = `${item.domain}/${item.report.id}`;
      nodes.set(key, { key, domain: item.domain, report: item.report, ageSecs: item.age_secs });
    }
    const idsFromGlobal = new Set([...nodes.values()].map((n) => n.report.id));
    for (const item of deviceList) {
      if (idsFromGlobal.has(item.report.id)) continue;
      const key = `local/${item.report.id}`;
      nodes.set(key, { key, domain: null, report: item.report, ageSecs: item.age_secs });
    }
    return nodes;
  }

  // 邊代表「設定上的關係」，不即時驗證真的連上：每個 `mode === "server"`
  // 的節點都連去一個共用的 `"broker"` 節點（沒有任何 server 就完全不畫
  // broker，由呼叫端看 `hasServer` 決定）；每個 `report.server_addr` 有值
  // 的節點，連去 ip 等於那個值的節點（找不到就不畫這條線，可能是那台
  // server 還沒被任何清單看到）。
  function computeTopologyEdges(nodes) {
    const byIp = new Map();
    for (const n of nodes.values()) byIp.set(n.report.ip, n.key);
    const edges = [];
    let hasServer = false;
    for (const n of nodes.values()) {
      if (n.report.mode === "server") {
        hasServer = true;
        edges.push({ from: n.key, to: "broker" });
      }
      if (n.report.server_addr) {
        const targetKey = byIp.get(n.report.server_addr);
        if (targetKey && targetKey !== n.key) edges.push({ from: n.key, to: targetKey });
      }
    }
    return { edges, hasServer };
  }

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
      },
    };
  }

```

- [ ] **Step 3: 宣告 `topologyUi`**

把（`openPanel` 內）：

```js
    let body;
    let musicUi = null;
    let shellUi = null;
    let notepadUi = null;
    let storageUi = null;
    let dataTableUi = null;
```

改成：

```js
    let body;
    let musicUi = null;
    let shellUi = null;
    let notepadUi = null;
    let storageUi = null;
    let dataTableUi = null;
    let topologyUi = null;
```

- [ ] **Step 4: 新增 `topology` 分支**

把（第 2256-2257 行）：

```js
      shellUi = { container, term, ws, resizeObserver };
    } else if (name === "notepad") {
```

改成（在兩行之間插入一整個新的 `else if` 分支，`shellUi = ...` 那一行跟
最後的 `} else if (name === "notepad") {` 都原樣保留，只是中間多了新分支）：

```js
      shellUi = { container, term, ws, resizeObserver };
    } else if (name === "topology") {
      panel.style.width = "640px";
      panel.style.height = "420px";

      const container = document.createElement("div");
      container.className = "topology-body";
      const svg = document.createElementNS(SVGNS, "svg");
      svg.setAttribute("class", "topology-svg");
      const domainLayer = document.createElementNS(SVGNS, "g");
      const edgeLayer = document.createElementNS(SVGNS, "g");
      const nodeLayer = document.createElementNS(SVGNS, "g");
      svg.appendChild(domainLayer);
      svg.appendChild(edgeLayer);
      svg.appendChild(nodeLayer);
      const tooltip = document.createElement("div");
      tooltip.className = "topology-tooltip";
      container.appendChild(svg);
      container.appendChild(tooltip);

      topologyUi = startTopology(container, svg, domainLayer, edgeLayer, nodeLayer, tooltip);
    } else if (name === "notepad") {
```

- [ ] **Step 5: SSE 排除清單加上 `topology`**

把（第 2749 行）：

```js
    if (name !== "player" && name !== "shell" && name !== "notepad" && name !== "storage") {
```

改成：

```js
    if (name !== "player" && name !== "shell" && name !== "notepad" && name !== "storage" && name !== "topology") {
```

- [ ] **Step 6: `panel.appendChild` 選擇邏輯加上 `topologyUi`**

把：

```js
    panel.appendChild(
      musicUi
        ? musicUi.container
        : shellUi
        ? shellUi.container
        : notepadUi
        ? notepadUi.container
        : storageUi
        ? storageUi.container
        : dataTableUi
        ? dataTableUi.container
        : body
    );
```

改成：

```js
    panel.appendChild(
      musicUi
        ? musicUi.container
        : shellUi
        ? shellUi.container
        : notepadUi
        ? notepadUi.container
        : storageUi
        ? storageUi.container
        : dataTableUi
        ? dataTableUi.container
        : topologyUi
        ? topologyUi.container
        : body
    );
```

- [ ] **Step 7: `open.set(...)` 帶上 `topologyUi`，`closePanel` 收尾時停掉它**

把（第 2785 行）：

```js
    open.set(name, { el: panel, es, musicUi, shellUi, notepadUi, storageUi });
```

改成：

```js
    open.set(name, { el: panel, es, musicUi, shellUi, notepadUi, storageUi, topologyUi });
```

把 `closePanel` 函式（第 985-1002 行）裡：

```js
    if (entry.shellUi) {
      entry.shellUi.resizeObserver.disconnect();
      entry.shellUi.ws.close();
      entry.shellUi.term.dispose();
    }
    entry.el.remove();
```

改成：

```js
    if (entry.shellUi) {
      entry.shellUi.resizeObserver.disconnect();
      entry.shellUi.ws.close();
      entry.shellUi.term.dispose();
    }
    if (entry.topologyUi) {
      entry.topologyUi.stop();
    }
    entry.el.remove();
```

- [ ] **Step 8: 加進選單**

把（第 2815-2816 行）：

```js
  addMenuItem("player", "🎵");
  addMenuItem("shell", "🖥️");
```

改成：

```js
  addMenuItem("player", "🎵");
  addMenuItem("shell", "🖥️");
  addMenuItem("topology", "🌐");
```

- [ ] **Step 9: 語法檢查＋編譯確認**

Run: `cargo build`（`frontend.html` 是 `include_str!` 進 binary，主要是確認沒有破壞其他部分，不會檢查 JS 語法本身）
Expected: 成功。

用 `node --check` 對主程式的 `<script>` 內容做語法檢查。目前檔案裡有 3 個
`<script>...</script>` 區塊：index 0 是內嵌的 xterm.js library bundle（約
28 萬字元）、index 1 是一個小 loader（約 1500 字元）、**index 2 是主程式**
（含 `openPanel`／`startTopology` 的那個，這次改完後會比原本的 75120 字元
更大一些）。改動只會讓 index 2 變大，不會改變這三個區塊的順序/數量，執行
前可以先確認一下（`grep -c '<script>' src/web/frontend.html` 應該還是
`3`）：

```bash
python3 -c "
import re
html = open('src/web/frontend.html', encoding='utf-8').read()
scripts = re.findall(r'<script>(.*?)</script>', html, re.S)
open('/tmp/main_script.js', 'w', encoding='utf-8').write(scripts[2])
"
node --check /tmp/main_script.js && echo "syntax OK"
```

Expected: `syntax OK`，沒有 SyntaxError。

- [ ] **Step 10: 手動驗證**

1. `cargo run`（或 headless 模式），瀏覽器開 `http://127.0.0.1:9759`。
2. 點選單「🌐 topology」，確認開出一個新面板，至少看到一個節點（本機自己）。
3. 如果環境裡有多台機器互相回報（其中至少一台是 server、一台是設定了
   `server <ip>` 的 client），確認：
   - server 角色是藍色圓，client 是綠色圓，standalone 是灰色圓。
   - 有 server 存在時，畫面上有一個六角形「MQTT broker」節點，且每個
     server 都有一條線連過去。
   - client 節點有一條線連到它 `server_addr` 對應的節點。
   - 同 domain 的節點被一個虛線框框住，框旁邊有 domain 名稱文字。
   - 節點的位置會自己動態調整（不會全部疊在一起或飄出面板），過幾秒後
     大致穩定下來。
4. 滑鼠移到任一台機器的節點上，確認出現 tooltip，內容包含
   id/ip/os/version/mode/device uptime/app uptime/disk/alive，數字/文字
   合理（跟同一台機器在 `device`/`global` 表格 panel 看到的資訊一致）。
5. 讓某台機器離線（關掉它的程式，或等它超過 30 秒沒回報），確認它在
   topology 圖上變成灰底虛線邊框，而不是直接消失。
6. 關掉 topology panel（按面板的 × 或選單裡再點一次—如果有 toggle 行為），
   用瀏覽器 DevTools 的效能/記憶體面板或簡單觀察 CPU，確認關閉後背景動畫
   跟 poll 真的停了（沒有殘留的 `setInterval`/`requestAnimationFrame` 一直
   在跑）。

- [ ] **Step 11: Commit**

```bash
git add src/web/frontend.html
git commit -m "$(cat <<'EOF'
web：新增 topology panel，力導向圖呈現跨機器連線關係

跟 player/shell 一樣是選單裡獨立一項，不掛在 global/device 表格底下。
資料來自既有的 /api/device/list + /api/global/list 定期 poll 合併，手刻
簡化版力導向模擬（不加繪圖套件），顏色分角色、虛線框分 domain、離線灰底
虛線、hover 顯示基本資料。
EOF
)"
```

---

### Task 3: 端對端驗證

**Files:** 無新增/修改檔案，純驗證。

- [ ] **Step 1: 全專案編譯與既有測試**

Run: `cargo build && cargo test`
Expected: 全部成功，134 passed，沒有新的編譯警告。

- [ ] **Step 2: 多機環境驗證（如果環境允許）**

在真實的多機環境下（至少一台 server、一台 client，理想上再加一台跨
domain 的機器）重跑一次 Task 2 Step 10 的手動驗證，確認：
- 圖上正確反映實際的 server/client/standalone 角色分佈。
- client→server、server→broker 的線都畫對了（跟各台機器的 `system
  status` 顯示的設定一致）。
- 跨 domain 的機器（如果有）也正確出現在圖上、被框進正確的 domain。

（這個 task 純粹是驗證，沒有新的程式碼異動，不需要 commit。）
