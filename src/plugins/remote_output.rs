use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

/// 還沒下過 `show` 之前，預設鏡射遠端的 `output` panel——那是「所有指令執行
/// 結果」的彙總捲動紀錄，多數情況下鏡射這個就夠用；真的想看遠端某個 plugin
/// 當下的即時狀態畫面（例如遠端的 `weather` 表格），才需要 `show weather` 切
/// 過去。
const DEFAULT_PANEL: &str = "output";

/// 背景執行緒多久檢查一次「目標/要鏡射的 panel 有沒有變」（斷線、換連線目標、
/// `show` 換了 panel 名稱），跟 `system`/`global` 的 `REPORT_INTERVAL` 無關，
/// 這裡純粹是鏡射，不涉及任何回報邏輯，用一個獨立、比較短的常數就好。
const POLL_INTERVAL: Duration = Duration::from_millis(500);

const MANUAL_TEXT: &str = "\
remote-output：把 remote plugin 目前連線目標的某一個 panel 即時鏡射過來顯示。

之所以需要這個獨立的 plugin：remote plugin 的 connect/轉發指令用的是既有的
/api/exec，這個端點的回應只有「新的 prompt」跟「錯誤訊息」，不包含指令實際印出
來的內容（那些是寫進遠端自己的 output panel）——所以要看遠端指令印了什麼，得
另外訂閱遠端的 panel SSE 串流，這就是這個 plugin 在做的事。

範例：
  show <panel-name>   設定現在要鏡射遠端的哪一個 panel，預設是 output（所有
                      指令執行結果的彙總捲動紀錄）；要看遠端某個 plugin 當下
                      的即時狀態畫面，就 show 成那個 plugin 的名字（例如
                      show weather）

沒有透過 remote plugin 連線中的時候，這裡不會有任何內容；連線目標/panel 換掉
的時候，畫面會先清空、重新訂閱新的來源。
";

/// 目前正在鏡射的來源：(遠端 id, 遠端 ip, panel 名稱)，這三者有任何一個變了
/// 就代表要停掉舊的 curl 訂閱、換一個新的。
type MirrorKey = (String, String, String);

pub struct RemoteOutputPlugin {
    ctx: SharedContext,
    /// `show <panel-name>` 設定的目前要鏡射哪個 panel，背景執行緒讀這個決定
    /// 訂閱哪個 SSE 端點。
    panel_name: Arc<Mutex<String>>,
    /// 鏡射到的內容，`panel_text()` 直接讀這個顯示。`None` 是「還沒收到過任何
    /// 一次 SSE 訊息」（連線中，或訂閱還沒建立），`Some(content)` 是「已經收到
    /// 過至少一次」——`content` 本身可能是空字串（遠端那個 panel 目前真的沒有
    /// 內容，例如 output panel 還沒累積過任何一行），這兩種情況不能混在一起用
    /// 「是不是空字串」判斷，不然遠端 panel 剛好是空的時候，畫面會卡在「正在
    /// 連線」的提示，看起來像鏡射壞掉了，其實只是如實顯示遠端本來就是空的。
    buffer: Arc<Mutex<Option<String>>>,
}

impl RemoteOutputPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        let panel_name = Arc::new(Mutex::new(DEFAULT_PANEL.to_string()));
        let buffer = Arc::new(Mutex::new(None));
        Self::spawn_supervisor(ctx.clone(), panel_name.clone(), buffer.clone());
        Self { ctx, panel_name, buffer }
    }

    /// 背景監督執行緒，整個程式活著期間持續跑：每 `POLL_INTERVAL` 檢查一次目前
    /// 該鏡射誰（`ctx.remote_target` + `panel_name`），跟上一輪不一樣就把舊的
    /// curl 子行程殺掉、清空畫面、視情況訂閱新的來源。
    fn spawn_supervisor(ctx: SharedContext, panel_name: Arc<Mutex<String>>, buffer: Arc<Mutex<Option<String>>>) {
        thread::spawn(move || {
            let mut current: Option<MirrorKey> = None;
            let mut child: Option<Child> = None;
            loop {
                let target = ctx.lock().unwrap().remote_target.clone();
                let wanted: Option<MirrorKey> =
                    target.map(|(id, ip)| (id, ip, panel_name.lock().unwrap().clone()));
                if wanted != current {
                    if let Some(mut old) = child.take() {
                        let _ = old.kill();
                        let _ = old.wait();
                    }
                    *buffer.lock().unwrap() = None;
                    child = wanted.as_ref().and_then(|(_, ip, name)| spawn_stream(ip, name, buffer.clone()));
                    current = wanted;
                }
                thread::sleep(POLL_INTERVAL);
            }
        });
    }

    fn show(&mut self, name: &str, out: &OutputBuffer) -> Result<()> {
        *self.panel_name.lock().unwrap() = name.to_string();
        out.push(&format!("現在鏡射遠端的 {name} panel\n"));
        Ok(())
    }

    fn render(&self) -> String {
        let target = self.ctx.lock().unwrap().remote_target.clone();
        let Some((id, _ip)) = target else {
            return "(目前沒有連線，先用 remote plugin 的 connect <id> 連線)".to_string();
        };
        let panel_name = self.panel_name.lock().unwrap().clone();
        match &*self.buffer.lock().unwrap() {
            None => format!("(正在連線鏡射 {id} 的 {panel_name} panel...)"),
            Some(content) if content.is_empty() => {
                format!("[鏡射 {id} / {panel_name}]\n\n(目前是空的)")
            }
            Some(content) => format!("[鏡射 {id} / {panel_name}]\n\n{content}"),
        }
    }
}

/// 對 `ip` 開一個 `curl -N`（不緩衝）子行程訂閱 `/api/panel/{panel_name}/stream`
/// 這個既有的 SSE 端點，另開一個執行緒持續讀它的 stdout、解析 SSE 的
/// `data: "..."` 行（跟 `web.rs` 的 `sse_frame` 是同一套編碼：JSON 字串），寫進
/// `buffer`。回傳這個 curl 子行程的控制代碼，讓呼叫端在目標換掉時能把它殺掉。
fn spawn_stream(ip: &str, panel_name: &str, buffer: Arc<Mutex<Option<String>>>) -> Option<Child> {
    let url = format!("http://{ip}:9759/api/panel/{panel_name}/stream");
    let mut child = Command::new("curl").args(["--silent", "-N", &url]).stdout(Stdio::piped()).spawn().ok()?;
    let stdout = child.stdout.take()?;
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let Some(payload) = line.strip_prefix("data: ") else { continue };
            let Ok(text) = serde_json::from_str::<String>(payload) else { continue };
            *buffer.lock().unwrap() = Some(text);
        }
    });
    Some(child)
}

impl Plugin for RemoteOutputPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["show <panel-name>"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "show" => self.show(args.first().context("show 需要接 panel 名稱")?, out),
            other => bail!("remote-output 不認得指令: {other}"),
        }
    }

    fn panel_text(&self) -> Option<String> {
        Some(self.render())
    }

    fn manual_text(&self) -> &'static str {
        MANUAL_TEXT
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
