use anyhow::{bail, Context, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

/// `manual` 指令的說明。`wakeon`/`status` 目前都還是 TODO 佔位，先如實寫出
/// 「打算做什麼」，不假裝已經實作完成。
const MANUAL_TEXT: &str = "\
wol：喚醒區網內支援 Wake-on-LAN 的裝置（送 magic packet）。

範例：
  wakeon AA:BB:CC:DD:EE:FF   送 magic packet 喚醒指定 MAC 位址的機器
  status                     查詢目前的喚醒狀態

目前 wakeon/status 都還只是 TODO 佔位，還沒接上真正送封包/查狀態的邏輯。
";

pub struct WolPlugin {
    #[allow(dead_code)]
    ctx: SharedContext,
}

impl WolPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx }
    }

    fn wakeon(&mut self, target: &str, out: &OutputBuffer) -> Result<()> {
        out.push(&format!("TODO: wakeon {target}\n"));
        Ok(())
    }

    fn status(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push("TODO: wol status\n");
        Ok(())
    }
}

impl Plugin for WolPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["wakeon <ip/mac>", "status"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "wakeon" => self.wakeon(args.first().context("wakeon 需要一個 ip/mac 參數")?, out),
            "status" => self.status(out),
            other => bail!("wol 不認得指令: {other}"),
        }
    }

    fn manual_text(&self) -> &'static str {
        MANUAL_TEXT
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
