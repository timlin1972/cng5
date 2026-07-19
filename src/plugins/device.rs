use anyhow::{bail, Context, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

pub struct DevicePlugin {
    #[allow(dead_code)]
    ctx: SharedContext,
}

impl DevicePlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx }
    }

    fn list(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push("TODO: device list\n");
        Ok(())
    }

    fn status(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push("TODO: device status\n");
        Ok(())
    }

    fn poweron(&mut self, target: &str, out: &OutputBuffer) -> Result<()> {
        out.push(&format!("TODO: poweron {target}\n"));
        Ok(())
    }

    fn poweroff(&mut self, target: &str, out: &OutputBuffer) -> Result<()> {
        out.push(&format!("TODO: poweroff {target}\n"));
        Ok(())
    }
}

impl Plugin for DevicePlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["list", "status", "poweron <target>", "poweroff <target>"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "list" => self.list(out),
            "status" => self.status(out),
            "poweron" => self.poweron(args.first().context("poweron 需要一個目標參數")?, out),
            "poweroff" => self.poweroff(args.first().context("poweroff 需要一個目標參數")?, out),
            other => bail!("device 不認得指令: {other}"),
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
