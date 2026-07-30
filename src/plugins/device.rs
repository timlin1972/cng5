use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Result};
use unicode_width::UnicodeWidthStr;

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};
use crate::plugins::{RowDiffTracker, TableSnapshot, REPORT_INTERVAL};
use crate::sysinfo;

/// 裝置多久沒回報就視為離線（不是刪掉資料，只是 alive 顯示 false，`report`
/// 保留最後一次收到的內容）。設成回報間隔（`REPORT_INTERVAL`）的 3 倍，容許
/// 偶爾漏個一兩次（網路一時不通、server 忙）還不會被判定離線。
const ALIVE_TTL: Duration = Duration::from_secs(REPORT_INTERVAL.as_secs() * 3);

const HEADERS: [&str; 10] =
    ["  id", "ip", "os", "version", "tailscale", "mode", "device uptime", "app uptime", "disk", "alive"];

/// `manual` 指令的說明。
const MANUAL_TEXT: &str = "\
device：顯示這台機器跟其他回報過的機器（見 system plugin 的 mode client/server）
目前的狀態——ip、os、有沒有 tailscale、mode、開機/程式執行多久、還在不在線上。

範例：
  list                查表格：每台裝置的 id/ip/os/version/tailscale/mode/uptime/disk/alive
  status              簡短摘要：裝置總數、目前上線幾台

alive 是「多久沒回報就視為離線」（回報間隔的 3 倍），不是真的把資料刪掉，離線
機器最後一次回報的內容還留著，只是 alive 顯示會變成空白。
";

pub struct DevicePlugin {
    ctx: SharedContext,
    /// TUI 用的逐格變化比對狀態，跟 `web_diff` 分開存的理由見
    /// `table_diff::RowDiffTracker` 的說明——TUI／web 兩邊重繪頻率不同，共用
    /// 一份會有其中一邊讀到「已經被另一邊消費掉的變化」而漏閃的 race。
    tui_diff: Mutex<RowDiffTracker>,
    web_diff: Mutex<RowDiffTracker>,
}

impl DevicePlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx, tui_diff: Mutex::new(RowDiffTracker::new()), web_diff: Mutex::new(RowDiffTracker::new()) }
    }

    /// 組出目前每一列的 (row key＝裝置 id, 每一欄的文字)，`table_text()`／
    /// `tui_snapshot()`／`web_snapshot()` 共用同一份。
    fn rows(&self) -> Vec<(String, Vec<String>)> {
        let my_id = sysinfo::hostname();
        let inner = self.ctx.lock().unwrap();
        let mut ids: Vec<&String> = inner.devices.keys().collect();
        ids.sort();
        ids.into_iter()
            .map(|id| {
                let entry = &inner.devices[id];
                let alive = entry.last_seen.elapsed() < ALIVE_TTL;
                let id_cell = if entry.report.id == my_id {
                    format!("* {}", entry.report.id)
                } else {
                    format!("  {}", entry.report.id)
                };
                // mode 縮寫成單一字元，省表格寬度：s=server、c=client、
                // -=standalone（還沒設過 mode 的預設值）。查不到對應值（未來
                // 版本間協定不一致）就照原字串顯示，不讓表格憑空消失一欄。
                let mode = match entry.report.mode.as_str() {
                    "server" => "s".to_string(),
                    "client" => "c".to_string(),
                    "standalone" => "-".to_string(),
                    other => other.to_string(),
                };
                let cells = vec![
                    id_cell,
                    entry.report.ip.clone(),
                    entry.report.os.clone(),
                    entry.report.version.clone(),
                    yes_no(entry.report.tailscale),
                    mode,
                    sysinfo::format_uptime(entry.report.device_uptime_secs),
                    sysinfo::format_uptime(entry.report.app_uptime_secs),
                    sysinfo::format_disk_usage(entry.report.disk_free_bytes, entry.report.disk_total_bytes),
                    if alive { "*".to_string() } else { String::new() },
                ];
                (id.clone(), cells)
            })
            .collect()
    }

    fn table_text(&self) -> String {
        let rows = self.rows();
        if rows.is_empty() {
            return "(還沒有任何裝置資料)".to_string();
        }
        let row_values: Vec<Vec<String>> = rows.into_iter().map(|(_, cells)| cells).collect();
        render_table(&HEADERS, &row_values)
    }

    /// 給 TUI 用的逐格 snapshot，`gui.rs` 的 `with_device` 會呼叫這個。
    pub(crate) fn tui_snapshot(&self) -> TableSnapshot {
        self.tui_diff.lock().unwrap().snapshot(&HEADERS, self.rows())
    }

    /// 給 web SSE 用的逐格 snapshot，`web.rs` 呼叫後轉成 JSON 推播。
    pub(crate) fn web_snapshot(&self) -> TableSnapshot {
        self.web_diff.lock().unwrap().snapshot(&HEADERS, self.rows())
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
fn render_table(headers: &[&str], rows: &[Vec<String>]) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use crate::plugin::{ContextInner, DeviceEntry, DeviceReport};

    fn make_report(id: &str) -> DeviceReport {
        DeviceReport {
            id: id.to_string(),
            ip: "127.0.0.1".to_string(),
            os: "linux".to_string(),
            version: "1.3.0".to_string(),
            tailscale: false,
            mode: "standalone".to_string(),
            device_uptime_secs: 0,
            app_uptime_secs: 0,
            disk_free_bytes: 0,
            disk_total_bytes: 0,
        }
    }

    fn ctx_with_entry(last_seen: Instant) -> SharedContext {
        let ctx: SharedContext = Arc::new(Mutex::new(ContextInner::default()));
        ctx.lock().unwrap().devices.insert("a".to_string(), DeviceEntry { report: make_report("a"), last_seen });
        ctx
    }

    /// 剛回報過（`last_seen` 就是現在）的裝置，還在 `ALIVE_TTL` 窗口內，`status`
    /// 應該把它算進「上線中」。
    #[test]
    fn recent_report_counts_as_alive() {
        let ctx = ctx_with_entry(Instant::now());
        let mut plugin = DevicePlugin::new(ctx);
        let out = OutputBuffer::new();
        plugin.status(&out).unwrap();
        assert!(out.all().join("\n").contains("上線中: 1"));
    }

    /// 上次回報時間已經超過 `ALIVE_TTL`（回報間隔的 3 倍）的裝置，`status`
    /// 應該把它算進「離線」（不計入上線中），即使它的 `report` 資料還留著。
    #[test]
    fn stale_report_counts_as_offline() {
        let stale = Instant::now().checked_sub(ALIVE_TTL + Duration::from_secs(1)).expect("測試環境時鐘異常");
        let ctx = ctx_with_entry(stale);
        let mut plugin = DevicePlugin::new(ctx);
        let out = OutputBuffer::new();
        plugin.status(&out).unwrap();
        assert!(out.all().join("\n").contains("上線中: 0"));
    }

    /// 剛好在 `ALIVE_TTL` 窗口內（差 1 秒未過期）仍應算上線——確認邊界是
    /// 「嚴格大於才算離線」而不是提早一點就誤判。
    #[test]
    fn report_just_inside_ttl_still_alive() {
        let almost_stale =
            Instant::now().checked_sub(ALIVE_TTL - Duration::from_secs(1)).expect("測試環境時鐘異常");
        let ctx = ctx_with_entry(almost_stale);
        let mut plugin = DevicePlugin::new(ctx);
        let out = OutputBuffer::new();
        plugin.status(&out).unwrap();
        assert!(out.all().join("\n").contains("上線中: 1"));
    }
}
