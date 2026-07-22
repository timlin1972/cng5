use anyhow::{bail, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

/// `manual` 指令的說明。
const MANUAL_TEXT: &str = "\
remote：像 ssh 一樣連到另一台機器，直接對它下指令，同網域跟跨 domain 都支援。

範例：
  connect <id>          連到同網域 device list 看得到的某一台機器（用它的
                        id/hostname），連上之後 prompt 會變成
                        <id>::<遠端目前的 prompt>，接下來打的每一行都會原封
                        不動轉發給那台機器執行，不用再包一層 cmd <cmd>
  connect <domain>/<id> 連到跨 domain 的機器（用 global list 看得到的
                        <domain>/<id>）。這條路徑透過 global plugin 的 MQTT
                        session 加密中繼（見 crypto 模組），需要：
                          - 所有機器的 remote-key 檔案內容要一樣（跨 domain
                            加解密用的共用金鑰，手動放檔案，不進版控）
                          - 本機是 system server 且已連上 MQTT，或本機的
                            server <ip> 有設定（會透過那台伺服器中繼）
                        跨 domain 連線不支援 shell（沒有直接可達的 ip 可以開
                        WebSocket）
  status                查目前有沒有連線中的目標

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
  - upgrade 會轉發給遠端執行（觸發那台機器的 upgrade 流程），但本機這邊會
    主動斷線回到 remote plugin，不會留在 <id>::... 這個 prompt——遠端接下來
    會重啟，繼續連著容易誤以為還連得上，之後打的指令送過去大概率失敗。

同網域用的是既有的 /api/exec、/api/prompt 端點。
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
        let inner = self.ctx.lock().unwrap();
        if let Some((id, ip)) = &inner.remote_target {
            return format!("連線中: {id} ({ip})\n");
        }
        if let Some((domain, id)) = &inner.cross_domain_remote_target {
            return format!("連線中（跨 domain）: {domain}/{id}\n");
        }
        "目前沒有連線\n".to_string()
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
        &["connect <id>", "connect <domain>/<id>", "status"]
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
