use anyhow::{bail, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

pub struct OutputPlugin {
    #[allow(dead_code)]
    ctx: SharedContext,
}

impl OutputPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx }
    }
}

impl Plugin for OutputPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &[]
    }

    fn dispatch(&mut self, cmd: &str, _args: &[String], _out: &OutputBuffer) -> Result<()> {
        bail!("output 不認得指令: {cmd}")
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
