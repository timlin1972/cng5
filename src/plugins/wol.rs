use anyhow::{bail, Context, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

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
}
