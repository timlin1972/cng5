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
    /// 這個 plugin 的 panel 要顯示的內容；預設 `None`（空殼，只有邊框標題）。
    /// `output` panel 的內容是即時捲動紀錄，由 GUI 直接處理，不走這個。
    fn panel_text(&self) -> Option<String> {
        None
    }

    /// 把 `Box<dyn Plugin>` 向下轉型回具體型別，讓外部（目前只有 GUI 的
    /// notepad 編輯功能，見 `gui.rs` 的 `with_notepad`）能直接操作某個 plugin
    /// 的內部狀態，而不是只能透過 `dispatch` 送指令字串——逐字元編輯這種高
    /// 頻率、內容含任意字元（含空白/引號）的操作，透過 `execute_line`/
    /// `shell_words` 指令解析既麻煩也沒必要。這是 Rust trait object 向下轉型
    /// 的標準寫法，沒辦法只在這裡寫一次預設實作套用到所有型別，每個 plugin
    /// 都要各自實作成 `{ self }`。
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}
