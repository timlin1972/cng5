use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::output::OutputBuffer;
use crate::plugin::{DeviceEntry, DeviceListItem, DeviceReport, Plugin, SharedContext};
use crate::sysinfo;

/// tailscale ip 多久重新查一次。過期後不是在呼叫端（`status`/`panel_text()`，
/// 可能是 CLI 的 `status` 指令、GUI 畫面重繪、web 的 `broadcast_ticker`，也
/// 可能是背景回報執行緒 `spawn_reporter`）當場去跑 `tailscale`，而是丟給背景
/// 執行緒查（見 `TailscaleCache::spawn_refresh`），呼叫端先拿現有的快取值，
/// 不會被這個子行程呼叫卡住。這條指令通常很快（查本機的 tailscaled daemon，
/// 不是真的網路請求），但只要沒有絕對保證一定快（tailscale 沒裝好、daemon
/// 沒起來、系統一時忙碌都可能拖到上百毫秒甚至更久），而 `panel_text()`/
/// `status()` 都是在持有共用的 `Shell` 鎖的情況下呼叫——一旦卡住，CLI 打
/// 指令、GUI 畫面、web 都會跟著卡住等鎖，這正是這個快取機制要避免的狀況。
const TAILSCALE_TTL: Duration = Duration::from_secs(10);

/// 背景回報執行緒（`spawn_reporter`）多久跑一次：把自己的資訊寫進本機的
/// device registry，mode 是 client 時額外推播給設定的 server、並拉一次完整
/// 清單回來合併。`pub(crate)` 給 `DevicePlugin` 算 `ALIVE_TTL` 用，兩邊的
/// 「多久算離線」需要跟這個回報間隔保持倍數關係，避免各自寫死不同步。
pub(crate) const REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// `manual` 指令的說明。
const MANUAL_TEXT: &str = "\
system：這台機器自己的資訊（ip/tailscale/開機時間/版本），以及 standalone/
client/server 三種模式怎麼串起多台機器互相回報狀態（device plugin 顯示的清單）。

範例：
  status                查這台機器目前的 id/ip/tailscale/mode/server/uptime
  version               編譯時間版本號
  mode standalone       只回報自己（寫進本機的 device registry），不推播、
                        不拉別人的清單
  mode server           開放讓 client 推播進來，device list 看得到所有推播過
                        的機器
  mode client           定期把自己的資訊推播給 server（用 server <ip> 設定的
                        目標），同時拉一份完整清單回來合併
  server <ip>           設定 client 模式要推播/拉清單的目標；不管目前是哪個
                        mode 都可以先設好，等切成 client 才會真的用到

不管哪個 mode，這台機器自己的資訊都會固定每 5 秒（REPORT_INTERVAL）寫進本機的
device registry，所以 standalone 模式下 device list 也看得到自己這一列。
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum SystemMode {
    Standalone,
    Server,
    Client,
}

impl SystemMode {
    fn as_str(self) -> &'static str {
        match self {
            SystemMode::Standalone => "standalone",
            SystemMode::Server => "server",
            SystemMode::Client => "client",
        }
    }
}

/// tailscale ip 的 TTL 快取，包成一個可以 `Clone`（內部全是 `Arc`）的小型別，
/// 這樣同一份快取狀態除了 `SystemPlugin` 自己的方法能用，也能整個搬進背景
/// 回報執行緒（`spawn_reporter`）的 closure 裡用，不用重複實作一次「讀快取、
/// 過期就丟一個背景執行緒去重查」的邏輯。
#[derive(Clone)]
struct TailscaleCache {
    cache: Arc<Mutex<Option<(Instant, String)>>>,
    pending: Arc<Mutex<bool>>,
}

impl TailscaleCache {
    fn new() -> Self {
        Self { cache: Arc::new(Mutex::new(None)), pending: Arc::new(Mutex::new(false)) }
    }

    /// 只讀快取，不做任何子行程呼叫：有資料就回傳（同時判斷是否過期該重
    /// 查），沒資料（還沒查過）或查到的是空字串（沒裝/沒登入/沒位址）都回傳
    /// `None`。真正的查詢一律丟給 `spawn_refresh`，這裡不等它。
    fn get(&self) -> Option<String> {
        let cached = self.cache.lock().unwrap().clone();
        let stale = match &cached {
            None => true,
            Some((fetched_at, _)) => fetched_at.elapsed() >= TAILSCALE_TTL,
        };
        if stale {
            self.spawn_refresh();
        }
        cached.map(|(_, ip)| ip).filter(|ip| !ip.is_empty())
    }

    /// 開一個背景執行緒去查 tailscale ip，查完寫回 `cache`；如果已經有一個
    /// 背景執行緒在查了就不重複開，查詢本身（`sysinfo::fetch_tailscale_ip`）
    /// 完全不會碰到 `Shell` 的鎖，呼叫端也不用等它做完。
    fn spawn_refresh(&self) {
        let mut pending = self.pending.lock().unwrap();
        if *pending {
            return; // 已經有背景執行緒在查了。
        }
        *pending = true;
        drop(pending);

        let cache = self.cache.clone();
        let pending = self.pending.clone();
        thread::spawn(move || {
            let ip = sysinfo::fetch_tailscale_ip().unwrap_or_default();
            *cache.lock().unwrap() = Some((Instant::now(), ip));
            *pending.lock().unwrap() = false;
        });
    }
}

pub struct SystemPlugin {
    /// `server_addr`（`ContextInner` 共用欄位，`qr` plugin 也會讀）讀寫要用得到。
    ctx: SharedContext,
    /// 這台機器在 device registry 裡的識別碼（電腦名稱）。`DevicePlugin` 判斷
    /// 「哪一列是自己」用的也是同一個值（各自呼叫 `sysinfo::hostname()`），
    /// 不需要透過 `ctx` 傳遞。
    id: String,
    /// 用 `Arc<Mutex<_>>` 而不是普通欄位，因為背景回報執行緒（`spawn_reporter`）
    /// 需要讀目前的 mode，而 `set_mode` 是在另一個執行緒（持有 `Shell` 鎖的
    /// 那個）寫入的。
    mode: Arc<Mutex<SystemMode>>,
    tailscale: TailscaleCache,
}

impl SystemPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        let id = sysinfo::hostname();
        let mode = Arc::new(Mutex::new(SystemMode::Standalone));
        let tailscale = TailscaleCache::new();
        Self::spawn_reporter(ctx.clone(), id.clone(), mode.clone(), tailscale.clone());
        Self { ctx, id, mode, tailscale }
    }

    /// 背景回報執行緒，整個程式活著期間持續跑，每 `REPORT_INTERVAL` 一次：
    /// 1. 不管目前是哪個 mode，都把自己的資訊寫進本機的 device registry
    ///    （`ContextInner::devices`），這樣 `DevicePlugin` 才能顯示「自己」
    ///    這一列，standalone/server 模式下 `device list` 也一樣看得到本機
    ///    資料，不用額外判斷。
    /// 2. 只有 mode 是 client 且設定過 `server_addr` 時，才推播自己的資訊給
    ///    對方（`push_report`），並拉一次完整的裝置清單回來合併進本機
    ///    registry（`pull_peers`）——自己那一列用本機剛查好的資料為準，不
    ///    會被伺服器回傳、可能有延遲的版本蓋掉。
    fn spawn_reporter(ctx: SharedContext, id: String, mode: Arc<Mutex<SystemMode>>, tailscale: TailscaleCache) {
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
                Self::push_report(&addr, &report);
                Self::pull_peers(&addr, &ctx, &id);
            }
            thread::sleep(REPORT_INTERVAL);
        });
    }

    fn build_report(id: &str, tailscale: &TailscaleCache, mode: SystemMode) -> DeviceReport {
        let tailscale_ip = tailscale.get();
        let ip = tailscale_ip.clone().unwrap_or_else(sysinfo::local_ip);
        DeviceReport {
            id: id.to_string(),
            ip,
            tailscale: tailscale_ip.is_some(),
            mode: mode.as_str().to_string(),
            device_uptime_secs: sysinfo::device_uptime_secs(),
            app_uptime_secs: sysinfo::app_uptime_secs(),
        }
    }

    /// 把 `report` POST 給 `addr` 這台 server 的 `/api/device/register`。跟
    /// `WeatherPlugin::fetch`/`sysinfo::fetch_tailscale_ip` 一樣透過 `curl`
    /// 子行程發網路請求，不額外引入 HTTP client crate。失敗（沒開 server、
    /// 網路不通、逾時）就放棄這一輪，下一次 `REPORT_INTERVAL` 再試，不重試、
    /// 不報錯給使用者——背景回報本來就是盡力而為。
    fn push_report(addr: &str, report: &DeviceReport) {
        let Ok(body) = serde_json::to_string(report) else { return };
        let url = format!("http://{addr}:9759/api/device/register");
        let _ = Command::new("curl")
            .args([
                "--silent",
                "--max-time",
                "5",
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "-d",
                &body,
                &url,
            ])
            .output();
    }

    /// 跟 `addr` 這台 server 的 `/api/device/list` 拿目前完整的裝置清單，合併
    /// 進本機 registry——除了自己那一列（本機剛查好的資料已經在這一輪寫過，
    /// 見 `spawn_reporter`，跳過避免被伺服器回傳、可能有延遲的版本蓋掉），其
    /// 餘每一列都直接覆蓋。`last_seen` 依伺服器回傳的「幾秒前收到」往回推算
    /// 出一個本機的 `Instant`，這樣 `DevicePlugin` 判斷 alive 用的是同一套
    /// 邏輯，不需要另外從網路上搬一個「alive」旗標過來。
    fn pull_peers(addr: &str, ctx: &SharedContext, my_id: &str) {
        let url = format!("http://{addr}:9759/api/device/list");
        let Ok(output) = Command::new("curl").args(["--silent", "--max-time", "5", &url]).output() else {
            return;
        };
        if !output.status.success() {
            return;
        }
        let Ok(body) = String::from_utf8(output.stdout) else { return };
        let Ok(items) = serde_json::from_str::<Vec<DeviceListItem>>(&body) else { return };
        let mut inner = ctx.lock().unwrap();
        for item in items {
            if item.report.id == my_id {
                continue;
            }
            let last_seen = Instant::now()
                .checked_sub(Duration::from_secs_f64(item.age_secs.max(0.0)))
                .unwrap_or_else(Instant::now);
            inner.devices.insert(item.report.id.clone(), DeviceEntry { report: item.report, last_seen });
        }
    }

    fn ip(&self) -> String {
        self.tailscale.get().unwrap_or_else(sysinfo::local_ip)
    }

    fn tailscale_flag(&self) -> &'static str {
        if self.tailscale.get().is_some() {
            "yes"
        } else {
            "no"
        }
    }

    fn mode_str(&self) -> &'static str {
        self.mode.lock().unwrap().as_str()
    }

    fn server_text(&self) -> String {
        self.ctx.lock().unwrap().server_addr.clone().unwrap_or_else(|| "未設定".to_string())
    }

    fn status(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push(&format!(
            "id: {}\nip: {}\ntailscale: {}\nmode: {}\nserver: {}\ndevice uptime: {}\napp uptime: {}\n",
            self.id,
            self.ip(),
            self.tailscale_flag(),
            self.mode_str(),
            self.server_text(),
            sysinfo::format_uptime(sysinfo::device_uptime_secs()),
            sysinfo::format_uptime(sysinfo::app_uptime_secs()),
        ));
        Ok(())
    }

    /// `CNG5_BUILD_TIMESTAMP` 是 `build.rs` 在編譯當下算好塞進來的環境變數
    /// （build date/time），不是執行當下的時間。
    fn version(&self, out: &OutputBuffer) -> Result<()> {
        out.push(&format!("build: {}\n", env!("CNG5_BUILD_TIMESTAMP")));
        Ok(())
    }

    fn set_mode(&mut self, args: &[String], out: &OutputBuffer) -> Result<()> {
        let target = args.first().context("mode 需要接 server/client/standalone")?;
        let resolved = Self::resolve_mode(target)?;
        *self.mode.lock().unwrap() = resolved;
        // `global` plugin 讀這個決定要不要連 MQTT（見 `ContextInner::is_server`
        // 的說明），跟 `mode` 指令同步寫，不需要另外輪詢。
        self.ctx.lock().unwrap().is_server = resolved == SystemMode::Server;
        out.push(&format!("system mode 設定為 {}\n", resolved.as_str()));
        Ok(())
    }

    /// 設定 client 要回報/拉清單的目標 server ip；mode 不是 client 時這個值
    /// 一樣可以先設好，等之後 `mode client` 時背景回報執行緒（`spawn_reporter`）
    /// 就會開始使用它，不需要照特定順序下指令。
    fn set_server(&mut self, args: &[String], out: &OutputBuffer) -> Result<()> {
        let addr = args.first().context("server 需要接伺服器的 ip")?;
        self.ctx.lock().unwrap().server_addr = Some(addr.clone());
        out.push(&format!(
            "server ip 設定為 {addr}（mode 是 client 時，每 {} 秒回報一次）\n",
            REPORT_INTERVAL.as_secs()
        ));
        Ok(())
    }

    /// 用前綴比對 `token` 對應到哪個 mode：完全相符優先；不然找前綴唯一對到誰
    /// （跟 root 的 `mode cli`/`mode gui` 縮寫比對邏輯一致），讓 `mode cli` 這種
    /// 縮寫也能展開成 `client`。找不到或有歧異都清楚報錯，不亂猜。
    fn resolve_mode(token: &str) -> Result<SystemMode> {
        const CANDIDATES: [(&str, SystemMode); 3] = [
            ("server", SystemMode::Server),
            ("client", SystemMode::Client),
            ("standalone", SystemMode::Standalone),
        ];
        if let Some((_, mode)) = CANDIDATES.iter().find(|(name, _)| *name == token) {
            return Ok(*mode);
        }
        let matches: Vec<&(&str, SystemMode)> =
            CANDIDATES.iter().filter(|(name, _)| name.starts_with(token)).collect();
        match matches.as_slice() {
            [] => bail!("mode 不認得: {token}（可用: server/client/standalone）"),
            [(_, mode)] => Ok(*mode),
            many => bail!(
                "mode 不明確: {token}（可能是: {}）",
                many.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

impl Plugin for SystemPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["status", "mode server", "mode client", "mode standalone", "server <ip>", "version"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "status" => self.status(out),
            "version" => self.version(out),
            "mode" => self.set_mode(args, out),
            "server" => self.set_server(args, out),
            other => bail!("system 不認得指令: {other}"),
        }
    }

    fn panel_text(&self) -> Option<String> {
        Some(format!(
            "id: {}\nip: {}\ntailscale: {}\nmode: {}\nserver: {}\ndevice uptime: {}\napp uptime: {}\nbuild: {}",
            self.id,
            self.ip(),
            self.tailscale_flag(),
            self.mode_str(),
            self.server_text(),
            sysinfo::format_uptime(sysinfo::device_uptime_secs()),
            sysinfo::format_uptime(sysinfo::app_uptime_secs()),
            env!("CNG5_BUILD_TIMESTAMP")
        ))
    }

    fn manual_text(&self) -> &'static str {
        MANUAL_TEXT
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
