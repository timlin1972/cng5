use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::output::OutputBuffer;

/// 各 plugin 之後要共用的資源放這裡（目前還沒有明確項目）。
#[derive(Default)]
pub struct ContextInner {}

pub type SharedContext = Arc<Mutex<ContextInner>>;

/// 要求 Send 是因為互動模式下 `?` 按鍵的 callback（rustyline 的
/// `ConditionalEventHandler`）需要 Send + Sync 才能綁定。
pub trait Plugin: Send {
    /// 給 `help` 指令顯示用，每一項是一行「指令 <參數說明>」。
    fn commands(&self) -> &'static [&'static str];
    /// `out` 是輸出的地方，不要直接 `println!`——CLI 跟 GUI 模式顯示的方式不一樣。
    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()>;
}
