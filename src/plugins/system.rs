use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

/// tailscale ip 多久重新查一次。過期後不是在呼叫端（`status`/`panel_text()`，
/// 可能是 CLI 的 `status` 指令、GUI 畫面重繪，也可能是 web 的
/// `broadcast_ticker`，見 `web.rs`）當場去跑 `tailscale`，而是丟給背景執行緒
/// 查（見 `spawn_tailscale_refresh`），呼叫端先拿現有的快取值，不會被這個
/// 子行程呼叫卡住。這條指令通常很快（查本機的 tailscaled daemon，不是真的
/// 網路請求），但只要沒有絕對保證一定快（tailscale 沒裝好、daemon 沒起來、
/// 系統一時忙碌都可能拖到上百毫秒甚至更久），而 `panel_text()`/`status()`
/// 都是在持有共用的 `Shell` 鎖的情況下呼叫——一旦卡住，CLI 打指令、GUI
/// 畫面、web 都會跟著卡住等鎖，這正是這個快取機制要避免的狀況。
const TAILSCALE_TTL: Duration = Duration::from_secs(10);

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

pub struct SystemPlugin {
    #[allow(dead_code)]
    ctx: SharedContext,
    mode: SystemMode,
    /// 最後一次查到的 tailscale ip，背景執行緒（`spawn_tailscale_refresh`）
    /// 抓完寫回這裡；`tailscale_ip()` 只讀，不含任何耗時操作，不會卡住持有
    /// `Shell` 鎖的那個執行緒。
    tailscale_cache: Arc<Mutex<Option<(Instant, String)>>>,
    /// 目前是不是已經有一個背景執行緒在查 tailscale ip，避免快取一過期、
    /// 短時間內被連續呼叫（例如 web 每個 tick）就開出一堆重複的 tailscale
    /// 子行程。
    tailscale_pending: Arc<Mutex<bool>>,
}

impl SystemPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self {
            ctx,
            mode: SystemMode::Standalone,
            tailscale_cache: Arc::new(Mutex::new(None)),
            tailscale_pending: Arc::new(Mutex::new(false)),
        }
    }

    fn status(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push(&format!("tailscale ip: {}\nmode: {}\n", self.tailscale_ip(), self.mode.as_str()));
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
        self.mode = Self::resolve_mode(target)?;
        out.push(&format!("system mode 設定為 {}\n", self.mode.as_str()));
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

    /// 只讀快取，不做任何子行程呼叫：有資料就回傳（同時判斷是否過期該重
    /// 查），沒資料就先回傳「查詢中」的狀態文字。真正的查詢一律丟給
    /// `spawn_tailscale_refresh`。
    fn tailscale_ip(&self) -> String {
        let cached = self.tailscale_cache.lock().unwrap().clone();
        let stale = match &cached {
            None => true,
            Some((fetched_at, _)) => fetched_at.elapsed() >= TAILSCALE_TTL,
        };
        if stale {
            self.spawn_tailscale_refresh();
        }
        match cached {
            Some((_, ip)) => ip,
            None => "查詢中...".to_string(),
        }
    }

    /// 開一個背景執行緒去查 tailscale ip，查完寫回 `tailscale_cache`；如果
    /// 已經有一個背景執行緒在查了就不重複開，查詢本身（`Self::fetch_tailscale_ip`）
    /// 完全不會碰到 `Shell` 的鎖，呼叫端也不用等它做完。
    fn spawn_tailscale_refresh(&self) {
        let mut pending = self.tailscale_pending.lock().unwrap();
        if *pending {
            return; // 已經有背景執行緒在查了。
        }
        *pending = true;
        drop(pending);

        let cache = self.tailscale_cache.clone();
        let pending = self.tailscale_pending.clone();
        thread::spawn(move || {
            let ip = Self::fetch_tailscale_ip();
            *cache.lock().unwrap() = Some((Instant::now(), ip));
            *pending.lock().unwrap() = false;
        });
    }

    /// 執行 `tailscale ip -4` 取得目前的 tailscale IPv4 位址；沒安裝 tailscale、
    /// 沒登入或沒有位址（指令失敗或沒輸出）都算沒有，回傳 "N/A"。只會在
    /// `spawn_tailscale_refresh` 開的背景執行緒裡呼叫，不會卡住任何持有鎖的
    /// 執行緒。
    fn fetch_tailscale_ip() -> String {
        Command::new("tailscale")
            .args(["ip", "-4"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "N/A".to_string())
    }
}

impl Plugin for SystemPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["status", "mode server", "mode client", "mode standalone", "version"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "status" => self.status(out),
            "version" => self.version(out),
            "mode" => self.set_mode(args, out),
            other => bail!("system 不認得指令: {other}"),
        }
    }

    fn panel_text(&self) -> Option<String> {
        Some(format!(
            "tailscale ip: {}\nmode: {}\nbuild: {}",
            self.tailscale_ip(),
            self.mode.as_str(),
            env!("CNG5_BUILD_TIMESTAMP")
        ))
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
