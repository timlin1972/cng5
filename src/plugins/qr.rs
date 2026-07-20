use anyhow::{bail, Result};
use qrcode::render::unicode;
use qrcode::QrCode;

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};
use crate::sysinfo;
use crate::web::PORT;

/// 沒有指令、也不快取——面板開著就照目前狀態即時算，這樣 `system server <ip>`
/// 改掉之後 QR 面板下次重繪就自動反映新網址，不用另外下指令重新產生。
pub struct QrPlugin {
    ctx: SharedContext,
    /// PgUp/PgDn 目前切到第幾個目標（見 `targets`），純粹是一個遞增/遞減的
    /// 計數器，真正顯示哪一個是畫面重繪當下用 `% targets.len()` 換算——這樣
    /// `server` 設定中途被加上/拿掉、目標數量跟著變化時，這個值不會失去意義
    /// 或需要額外去夾範圍。
    showing: usize,
}

impl QrPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx, showing: 0 }
    }

    pub fn cycle_next(&mut self) {
        self.showing = self.showing.wrapping_add(1);
    }

    pub fn cycle_prev(&mut self) {
        self.showing = self.showing.wrapping_sub(1);
    }

    /// 目前可以看的目標：一定有 `local`（這台機器自己的 web UI 網址），如果
    /// `system` plugin 設定過 `server <ip>`（`ContextInner::server_addr`），
    /// 再加一個 `server` 的 web UI 網址；沒設定就不組，只有一個目標可以看。
    /// local 的 ip 直接讀 `system` plugin 回報到 `ctx.devices` 裡「自己」那
    /// 筆的 `ip`（tailscale 優先、否則本機區網 ip，見
    /// `SystemPlugin::build_report`），而不是自己另外重查一次——這樣才保證
    /// 跟 `system status`/panel 看到的是同一個 ip，不會兩邊各自查出不同結果。
    fn targets(&self) -> Vec<(&'static str, String)> {
        let inner = self.ctx.lock().unwrap();
        let local_ip = inner
            .devices
            .get(&sysinfo::hostname())
            .map(|entry| entry.report.ip.clone())
            .unwrap_or_else(sysinfo::local_ip);
        let mut targets = vec![("local", format!("http://{local_ip}:{PORT}"))];
        if let Some(addr) = &inner.server_addr {
            targets.push(("server", format!("http://{addr}:{PORT}")));
        }
        targets
    }

    /// panel 顯示的內容：目前這個目標是 local 還是 server、網址是什麼，接著
    /// 是 QR code 本身——用 `qrcode` crate 內建的 unicode 半格 renderer，一個
    /// 字元同時表示上下兩個模組，抵銷終端機字元格「高比寬約 2:1」的比例，
    /// 畫出來的 QR 才不會被拉長變形、掃不到。
    fn render(&self) -> String {
        let targets = self.targets();
        let idx = self.showing % targets.len();
        let (label, url) = &targets[idx];
        let hint =
            if targets.len() > 1 { "PgUp/PgDn 切換 local/server" } else { "(未設定 server，只有 local)" };
        let ascii = match QrCode::new(url.as_bytes()) {
            Ok(code) => code.render::<unicode::Dense1x2>().build(),
            Err(err) => format!("QR code 產生失敗: {err}"),
        };
        format!("[{label}] {url}\n\n{ascii}\n\n{hint}")
    }
}

impl Plugin for QrPlugin {
    // 沒有任何指令：切換 local/server 是 GUI panel 裡的 PgUp/PgDn（見 `gui.rs`
    // 的 `with_qr`），不透過 `execute_line` 送指令字串。
    fn commands(&self) -> &'static [&'static str] {
        &[]
    }

    fn dispatch(&mut self, cmd: &str, _args: &[String], _out: &OutputBuffer) -> Result<()> {
        bail!("qr 不認得指令: {cmd}")
    }

    fn panel_text(&self) -> Option<String> {
        Some(self.render())
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
