use anyhow::{bail, Context, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

pub struct MusicPlugin {
    #[allow(dead_code)]
    ctx: SharedContext,
}

impl MusicPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx }
    }

    fn download(&mut self, target: &str, out: &OutputBuffer) -> Result<()> {
        out.push(&format!("TODO: download {target}\n"));
        Ok(())
    }

    fn list(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push("TODO: music list\n");
        Ok(())
    }
}

impl Plugin for MusicPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["download <target>", "list"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "download" => self.download(args.first().context("download 需要一個目標參數")?, out),
            "list" => self.list(out),
            other => bail!("music 不認得指令: {other}"),
        }
    }
}
