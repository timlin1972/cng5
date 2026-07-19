use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

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
}

impl SystemPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx, mode: SystemMode::Standalone }
    }

    fn status(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push(&format!(
            "tailscale ip: {}\nmode: {}\n",
            Self::tailscale_ip(),
            self.mode.as_str()
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

    /// 執行 `tailscale ip -4` 取得目前的 tailscale IPv4 位址；沒安裝 tailscale、
    /// 沒登入或沒有位址（指令失敗或沒輸出）都算沒有，回傳 "N/A"。
    fn tailscale_ip() -> String {
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
            Self::tailscale_ip(),
            self.mode.as_str(),
            env!("CNG5_BUILD_TIMESTAMP")
        ))
    }
}
