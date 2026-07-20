use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::output::OutputBuffer;

/// 一台裝置目前回報的資訊——不管是這台機器自己（見 `plugins::system` 背景
/// 回報執行緒直接寫入本機 registry），還是透過 `/api/device/register` 收到
/// 其他機器回報的，格式都一樣，這樣 `DevicePlugin` 顯示時不用區分來源。
#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceReport {
    pub id: String,
    pub ip: String,
    pub tailscale: bool,
    pub mode: String,
    pub device_uptime_secs: u64,
    pub app_uptime_secs: u64,
}

/// `GET /api/device/list` 的一筆回應：`age_secs` 是伺服器收到這筆回報後過了
/// 幾秒，而不是直接帶 `last_seen`（`Instant` 沒辦法序列化，而且不同機器的
/// `Instant` 本來就不能互相比較）。拉這份清單的一方（client 端的
/// `plugins::system::pull_peers`）憑這個秒數重建一個本機的 `Instant`，之後
/// 判斷 alive 的邏輯（`DevicePlugin`）就能跟本機自己的資料一致。
#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceListItem {
    pub report: DeviceReport,
    pub age_secs: f64,
}

/// registry 裡的一筆：上一次收到/寫入的時間，搭配 `DevicePlugin` 顯示時判斷
/// alive。曾經回報過、後來斷線的裝置，`report` 保留最後一次收到的內容不清
/// 掉，只有 alive 會變成 false（見 `DevicePlugin` 的 `ALIVE_TTL`）。
pub struct DeviceEntry {
    pub report: DeviceReport,
    pub last_seen: Instant,
}

/// 各 plugin 之後要共用的資源放這裡。
#[derive(Default)]
pub struct ContextInner {
    /// `system` plugin（不管是本機自己、還是透過 web 收到其他機器的回報，見
    /// `web::device_register`）寫入，`device` plugin 讀出來顯示。
    pub devices: HashMap<String, DeviceEntry>,
    /// `system` plugin 的 `server <ip>` 設定的目標。放在這裡（而不是
    /// `SystemPlugin` 自己的私有欄位）是因為 `qr` plugin 也需要讀它組「server
    /// 的 web UI 網址」QR code，兩個 plugin 都要碰得到，符合 `ContextInner`
    /// 本來就是「各 plugin 共用資源」的定位。
    pub server_addr: Option<String>,
}

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
