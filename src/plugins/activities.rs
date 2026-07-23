use anyhow::{bail, Context, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

/// panel 顯示最近幾筆，跟 `list` 不接參數時顯示全部（受 `ActivityLog` 上限節制）
/// 不一樣——panel 空間有限，只給一個即時的概略印象。
const PANEL_RECENT: usize = 20;

/// `manual` 指令的說明。
const MANUAL_TEXT: &str = "\
activities：記錄這台機器的網路活動流水帳——mqtt 發布/收到、對外/對內的 http 請求、
外部服務呼叫（weather 的 wttr.in、music 的 yt-dlp 下載）——方便事後查「這台機器
到底跟誰講過話」。

分類（每筆紀錄開頭的第二欄）：
  mqtt-out    透過 global 的 MQTT bridge 發布出去
  mqtt-in     透過 MQTT bridge 收到
  http-out    本機主動發起的 HTTP 請求（remote/system/files/global 的轉發、回報...）
  http-in     別人打進來的 HTTP 請求（web UI、remote、cross-relay...），用
              actix-web 的 middleware 統一攔截記錄
  external    plugin 自己發起的外部服務呼叫（weather 查天氣、music 下載影片）

範例：
  list          印出目前記錄的所有紀錄（由舊到新）
  list <n>      只印最近 n 筆
  status        目前筆數 / 上限
  limit <n>     調整上限，縮小時立刻裁掉最舊的紀錄
  clear         清空目前所有紀錄

筆數有上限（預設 1000），超過就從最舊的開始丟掉——這是流水帳，不是完整的稽核紀錄。
";

pub struct ActivitiesPlugin {
    ctx: SharedContext,
}

impl ActivitiesPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx }
    }

    fn list(&mut self, args: &[String], out: &OutputBuffer) -> Result<()> {
        let log = self.ctx.lock().unwrap().activities.clone();
        let n = match args.first() {
            Some(raw) => raw.parse::<usize>().context("list 的參數要是一個數字")?,
            None => log.limit(),
        };
        let lines = log.recent(n);
        if lines.is_empty() {
            out.push("(還沒有任何活動紀錄)\n");
        } else {
            out.push(&format!("{}\n", lines.join("\n")));
        }
        Ok(())
    }

    fn status(&mut self, out: &OutputBuffer) -> Result<()> {
        let log = self.ctx.lock().unwrap().activities.clone();
        out.push(&format!("目前筆數: {}\n上限: {}\n", log.len(), log.limit()));
        Ok(())
    }

    fn limit(&mut self, args: &[String], out: &OutputBuffer) -> Result<()> {
        let n = args.first().context("limit 需要接一個數字")?.parse::<usize>().context("limit 的參數要是一個數字")?;
        let log = self.ctx.lock().unwrap().activities.clone();
        log.set_limit(n);
        out.push(&format!("上限調整為 {n}\n"));
        Ok(())
    }

    fn clear(&mut self, out: &OutputBuffer) -> Result<()> {
        self.ctx.lock().unwrap().activities.clear();
        out.push("已清空所有活動紀錄\n");
        Ok(())
    }

    fn panel_text_impl(&self) -> String {
        let log = self.ctx.lock().unwrap().activities.clone();
        let lines = log.recent(PANEL_RECENT);
        if lines.is_empty() {
            "(還沒有任何活動紀錄)".to_string()
        } else {
            lines.join("\n")
        }
    }
}

impl Plugin for ActivitiesPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["list <n>", "status", "limit <n>", "clear"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "list" => self.list(args, out),
            "status" => self.status(out),
            "limit" => self.limit(args, out),
            "clear" => self.clear(out),
            other => bail!("activities 不認得指令: {other}"),
        }
    }

    fn panel_text(&self) -> Option<String> {
        Some(self.panel_text_impl())
    }

    fn manual_text(&self) -> &'static str {
        MANUAL_TEXT
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
