use anyhow::{bail, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

/// `manual` 指令的說明。
const MANUAL_TEXT: &str = "\
output：所有指令的輸出（help、其他 plugin 的 dispatch 結果、script 執行時的
echo……）都會即時捲動顯示在這個 panel 裡，沒有自己的指令。

開著這個 panel 就像開了一個 console log，適合開在旁邊隨時看整體有沒有錯誤訊息，
不用切換到某個特定 plugin 才看得到它剛剛印了什麼。
";

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

    fn manual_text(&self) -> &'static str {
        MANUAL_TEXT
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
