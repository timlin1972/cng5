use std::time::Duration;

use anyhow::{bail, Result};
use unicode_width::UnicodeWidthStr;

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};
use crate::plugins::REPORT_INTERVAL;
use crate::sysinfo;

/// 裝置多久沒回報就視為離線（不是刪掉資料，只是 alive 顯示 false，`report`
/// 保留最後一次收到的內容）。設成回報間隔（`REPORT_INTERVAL`）的 3 倍，容許
/// 偶爾漏個一兩次（網路一時不通、server 忙）還不會被判定離線。
const ALIVE_TTL: Duration = Duration::from_secs(REPORT_INTERVAL.as_secs() * 3);

/// `manual` 指令的說明。
const MANUAL_TEXT: &str = "\
device：顯示這台機器跟其他回報過的機器（見 system plugin 的 mode client/server）
目前的狀態——ip、os、有沒有 tailscale、mode、開機/程式執行多久、還在不在線上。

範例：
  list                查表格：每台裝置的 id/ip/os/version/tailscale/mode/uptime/alive
  status              簡短摘要：裝置總數、目前上線幾台

alive 是「多久沒回報就視為離線」（回報間隔的 3 倍），不是真的把資料刪掉，離線
機器最後一次回報的內容還留著，只是 alive 顯示會變成空白。
";

pub struct DevicePlugin {
    ctx: SharedContext,
}

impl DevicePlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx }
    }

    /// 把目前 registry 裡的每一筆組成一個文字表格：id/ip/os/tailscale/mode/
    /// device uptime/app uptime/alive。是不是自己不再獨立成一欄，而是拿這一
    /// 列的 id 跟本機的 `sysinfo::hostname()` 比對，是自己就在 id 前面加上
    /// `* `，不是就補兩個空白，讓每一列的 id 都對齊——不管這筆資料是本機自己寫進 registry
    /// 的、還是從 server 那邊拉回來的清單，判斷方式都一樣，對 server 端跟
    /// client 端都成立（client 顯示「哪一列是自己」用的也是它自己的
    /// hostname，不是相信伺服器回傳的任何欄位）。alive 欄位是活著就打
    /// `*`，沒回報就留空白。
    fn table_text(&self) -> String {
        let my_id = sysinfo::hostname();
        let inner = self.ctx.lock().unwrap();
        if inner.devices.is_empty() {
            return "(還沒有任何裝置資料)".to_string();
        }
        let mut ids: Vec<&String> = inner.devices.keys().collect();
        ids.sort();

        let headers =
            ["  id", "ip", "os", "version", "tailscale", "mode", "device uptime", "app uptime", "alive"];
        let rows: Vec<[String; 9]> = ids
            .into_iter()
            .map(|id| {
                let entry = &inner.devices[id];
                let alive = entry.last_seen.elapsed() < ALIVE_TTL;
                let id_cell = if entry.report.id == my_id {
                    format!("* {}", entry.report.id)
                } else {
                    format!("  {}", entry.report.id)
                };
                [
                    id_cell,
                    entry.report.ip.clone(),
                    entry.report.os.clone(),
                    entry.report.version.clone(),
                    yes_no(entry.report.tailscale),
                    entry.report.mode.clone(),
                    sysinfo::format_uptime(entry.report.device_uptime_secs),
                    sysinfo::format_uptime(entry.report.app_uptime_secs),
                    if alive { "*".to_string() } else { String::new() },
                ]
            })
            .collect();
        render_table(&headers, &rows)
    }

    fn list(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push(&format!("{}\n", self.table_text()));
        Ok(())
    }

    fn status(&mut self, out: &OutputBuffer) -> Result<()> {
        let inner = self.ctx.lock().unwrap();
        let total = inner.devices.len();
        let alive = inner.devices.values().filter(|e| e.last_seen.elapsed() < ALIVE_TTL).count();
        drop(inner);
        out.push(&format!("裝置總數: {total}\n上線中: {alive}\n"));
        Ok(())
    }

}

fn yes_no(b: bool) -> String {
    if b { "yes".to_string() } else { "no".to_string() }
}

/// 組一個純文字表格（表頭 + 分隔線 + 每一列），欄寬依這一欄裡最寬的內容決
/// 定，用 `UnicodeWidthStr` 對齊。跟 `WeatherPlugin` 的 `render_table`/`pad`
/// 是同一個理由，但這裡的每個儲存格都只有單行內容，不需要它處理多行儲存格
/// 那一層複雜度，所以另外寫一份精簡版而不是共用。
fn render_table(headers: &[&str], rows: &[[String; 9]]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| UnicodeWidthStr::width(*h)).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(UnicodeWidthStr::width(cell.as_str()));
        }
    }
    let pad = |s: &str, w: usize| format!("{s}{}", " ".repeat(w.saturating_sub(UnicodeWidthStr::width(s))));
    let header_line = headers.iter().zip(&widths).map(|(h, w)| pad(h, *w)).collect::<Vec<_>>().join(" | ");
    let separator = widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("-+-");
    let mut lines = vec![header_line, separator];
    for row in rows {
        lines.push(row.iter().zip(&widths).map(|(c, w)| pad(c, *w)).collect::<Vec<_>>().join(" | "));
    }
    lines.join("\n")
}

impl Plugin for DevicePlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["list", "status"]
    }

    fn dispatch(&mut self, cmd: &str, _args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "list" => self.list(out),
            "status" => self.status(out),
            other => bail!("device 不認得指令: {other}"),
        }
    }

    fn panel_text(&self) -> Option<String> {
        Some(self.table_text())
    }

    fn manual_text(&self) -> &'static str {
        MANUAL_TEXT
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
