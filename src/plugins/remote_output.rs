use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::output::OutputBuffer;
use crate::plugin::{CrossDomainAsk, Plugin, RemoteReply, SharedContext};
use crate::shell::send_cross_domain_request;

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

跨 domain 連線（`connect <domain>/<id>`）鏡射的方式不一樣：沒有直接可達的 ip
可以開 SSE 長連線，改成每 POLL_INTERVAL 送一次加密的一次性查詢（透過跟
remote 指令轉發共用的 `send_cross_domain_request`），實際更新頻率會隨這個
請求本身要多久有落差（正常應該是一瞬間，MQTT/中繼逾時的話最多會慢個幾秒）。
";

/// 目前正在鏡射的來源。同網域（`Http`）用持續訂閱 SSE 的 curl 子行程；跨
/// domain（`Cross`）沒有這種長連線可用，靠背景迴圈每輪重新送一次性查詢，見
/// `spawn_supervisor`。
#[derive(Clone, PartialEq)]
enum Wanted {
    None,
    Http { id: String, ip: String, panel: String },
    Cross { domain: String, target_id: String, panel: String },
}

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
    /// 該鏡射誰（`ctx.remote_target`/`ctx.cross_domain_remote_target` +
    /// `panel_name`），跟上一輪不一樣就把舊的 curl 子行程殺掉（同網域）、清空
    /// 畫面、視情況訂閱新的來源；目標是跨 domain 的話，每一輪都額外送一次性的
    /// `Panel` 查詢更新畫面（沒有 SSE 長連線可以持續訂閱，見 `Wanted::Cross`
    /// 的說明）。
    fn spawn_supervisor(ctx: SharedContext, panel_name: Arc<Mutex<String>>, buffer: Arc<Mutex<Option<String>>>) {
        thread::spawn(move || {
            let mut current = Wanted::None;
            let mut child: Option<Child> = None;
            loop {
                let panel = panel_name.lock().unwrap().clone();
                let (remote_target, cross_target) = {
                    let inner = ctx.lock().unwrap();
                    (inner.remote_target.clone(), inner.cross_domain_remote_target.clone())
                };
                let wanted = match (remote_target, cross_target) {
                    (Some((id, ip)), _) => Wanted::Http { id, ip, panel },
                    (None, Some((domain, target_id))) => Wanted::Cross { domain, target_id, panel },
                    (None, None) => Wanted::None,
                };

                if wanted != current {
                    if let Some(mut old) = child.take() {
                        let _ = old.kill();
                        let _ = old.wait();
                    }
                    *buffer.lock().unwrap() = None;
                    child = match &wanted {
                        Wanted::Http { ip, panel, .. } => spawn_stream(ip, panel, buffer.clone()),
                        Wanted::Cross { .. } | Wanted::None => None,
                    };
                    current = wanted;
                }

                // 跨 domain 沒有持續訂閱的長連線，每一輪都要重新查一次；请求
                // 本身可能要等到 `send_cross_domain_request` 的逾時上限
                // （MQTT 直發 5 秒／經自己 server 中繼最多 10 秒），失敗/逾時
                // 就維持畫面上一輪的內容，不要讓它整個清空閃爍，等下一輪再試。
                if let Wanted::Cross { domain, target_id, panel } = &current {
                    let ask = CrossDomainAsk::Panel { target_id: target_id.clone(), panel_name: panel.clone() };
                    if let Ok(RemoteReply::Panel { text, .. }) = send_cross_domain_request(&ctx, domain, ask) {
                        *buffer.lock().unwrap() = Some(text.unwrap_or_default());
                    }
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

    /// `render`/`panel_text` 顯示用的「目前連線目標」簡短描述，同網域顯示 id，
    /// 跨 domain 顯示 `<domain>/<id>`。
    fn target_label(&self) -> Option<String> {
        let inner = self.ctx.lock().unwrap();
        if let Some((id, _ip)) = &inner.remote_target {
            return Some(id.clone());
        }
        if let Some((domain, target_id)) = &inner.cross_domain_remote_target {
            return Some(format!("{domain}/{target_id}"));
        }
        None
    }

    fn render(&self) -> String {
        let Some(id) = self.target_label() else {
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
