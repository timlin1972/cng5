use anyhow::{bail, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

/// `manual` 指令的說明。
const MANUAL_TEXT: &str = "\
remote：像 ssh 一樣連到同網域內的另一台機器，直接對它下指令。

範例：
  connect <id>   連到 device list 看得到的某一台機器（用它的 id/hostname），
                 連上之後 prompt 會變成 <id>::<遠端目前的 prompt>，接下來打的
                 每一行都會原封不動轉發給那台機器執行，不用再包一層 cmd <cmd>
  status         查目前有沒有連線中的目標

連線期間（prompt 顯示 <id>::...）：
  - 在遠端的 root 下 exit/quit 會離開這個連線，回到本機的 remote plugin。
  - 其餘任何位置（包含遠端某個 plugin/panel 裡）打 exit/quit/~/../...，都是
    在遠端那邊生效（讓遠端自己跳回它的上一層），不會離開這個連線——這樣才不會
    不小心把對方那台機器的 cng5 整個關掉。
  - help 顯示的是這個連線的簡短說明，不是遠端的指令清單（本機不知道遠端裝了
    哪些 plugin）；想知道遠端有什麼指令，就直接在連線裡打 help，會轉發過去問
    遠端自己。
  - shell 會把目前這個終端機整個接上遠端的 host shell（跟 ssh 過去一樣真的
    互動，vim/top 這類全螢幕程式都能用），不是轉發成一行指令給遠端的 cng5
    執行。用遠端 shell 裡的 exit 離開，回到這個連線。

只能連同網域內、本機 device list 已經看得到的機器（同網域機器互相本來就連得
到，用的是既有的 /api/exec、/api/prompt 端點）。跨網域的版本之後再另外規劃。
搭配 remote-output plugin 使用：/api/exec 的回應只有 prompt/錯誤訊息，不含
指令實際印出來的內容，要看遠端指令印出了什麼，開 remote-output 的 panel（預設
鏡射遠端的 output panel）。
";

pub struct RemotePlugin {
    ctx: SharedContext,
}

impl RemotePlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx }
    }

    fn status_text(&self) -> String {
        match &self.ctx.lock().unwrap().remote_target {
            Some((id, ip)) => format!("連線中: {id} ({ip})\n"),
            None => "目前沒有連線\n".to_string(),
        }
    }

    fn status(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push(&self.status_text());
        Ok(())
    }
}

impl Plugin for RemotePlugin {
    // `connect <id>` 不在這裡處理——它需要「切換 Shell 的 mode」，`dispatch`
    // 的簽名做不到這件事，是 `Shell::execute_line` 自己攔截處理的（見
    // `shell.rs` 裡 `(Mode::InPlugin(name), "connect") if name == "remote"`
    // 那個分支），這裡列出來只是為了讓它出現在 `help`/tab 補全的候選清單裡。
    fn commands(&self) -> &'static [&'static str] {
        &["connect <id>", "status"]
    }

    fn dispatch(&mut self, cmd: &str, _args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "status" => self.status(out),
            other => bail!("remote 不認得指令: {other}"),
        }
    }

    fn panel_text(&self) -> Option<String> {
        Some(self.status_text())
    }

    fn manual_text(&self) -> &'static str {
        MANUAL_TEXT
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
