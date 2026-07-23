use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rumqttc::{Client, QoS};
use tungstenite::{Message, WebSocket};

use crate::crypto;
use crate::output::OutputBuffer;
use crate::plugin::{CrossDomainAsk, Plugin, RemoteReply, RemoteRequest, SharedContext};
use crate::sysinfo;

/// 現在終端機（CLI/GUI）跟背景的 web server（見 `web::spawn`）共用同一個
/// `Arc<Mutex<Shell>>`，如果任何一個執行緒在持有這把鎖的時候 panic，鎖會被標記
/// 為「poisoned」，之後別的執行緒對它呼叫 `.lock().unwrap()` 就會跟著 panic——
/// 對 GUI 來說這會跳過畫面收尾（`disable_raw_mode`/離開 alternate screen），
/// 讓終端機卡在 raw mode 出不來。`Shell` 本身沒有會因為操作中斷而壞掉的不變量，
/// 所以直接把內容拿出來繼續用即可，不需要讓一個執行緒的 panic 拖累其他執行緒。
pub fn lock_shell(shell: &Mutex<Shell>) -> MutexGuard<'_, Shell> {
    shell.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 目前平台預設要開的 host shell 是哪個程式：Unix-like 平台看 `SHELL`（使用者的
/// 登入 shell），沒設定就退回 `/bin/bash`；Windows 沒有 `SHELL` 這個慣例，改看
/// `COMSPEC`（一般由系統設好指向 `cmd.exe`），沒設定就退回 `cmd.exe`。`shell.rs`
/// 的 `run_host_shell` 跟 `web.rs` 的 `shell_ws` 都是「借一個真正的 host shell」
/// 這個概念，因此共用同一份平台判斷，不要各自維護一套。
pub fn default_shell_program() -> String {
    if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
    }
}

/// `shell` 指令實際借出終端機的地方：CLI（`main.rs`）跟 GUI（`gui.rs`）都要在
/// `Shell` 的鎖已經放開之後才呼叫這個，讓子行程（一個完整的、可能跑很久的互動
/// shell session）不會卡住其他共用同一個 `Shell` 的執行緒（例如背景的 web
/// server）。子行程預設繼承目前行程的 stdin/stdout/stderr，也就是使用者現在打
/// 字用的這個終端機，用起來就像真的執行了 `$SHELL`（或 Windows 上的 `%COMSPEC%`）
/// 一樣；執行失敗（例如設定的程式不存在）就當作使用者按了一下就離開，不特別報錯。
pub fn run_host_shell() {
    let _ = std::process::Command::new(default_shell_program()).status();
}

/// `upgrade` 指令：依序 `git fetch --all`、`git reset --hard origin/main`、
/// `cargo build`，全部在背景執行緒做——服務不中斷（舊的還在跑），只有編譯
/// 成功才會觸發重啟；任何一步失敗就把錯誤訊息 push 進 `output`、直接中止，
/// 程式維持原樣繼續運作，不會變成「已經把自己關掉、但新版本編譯失敗」這種
/// 兩邊都沒有東西在跑的狀態。
///
/// 真的要重啟的最後一步（編譯成功之後）沒辦法讓現在這個行程自己做——它要先
/// 完全結束、放開 port 9759，新編出來的執行檔才能重新 bind 上去。做法是拿
/// 現在這個行程的 pid，重新呼叫自己一份（同一個執行檔）帶上
/// `--respawn-after=<pid>`（見 `main.rs`）：那個小助手會先等這個 pid 真的
/// 消失，才啟動新編好的執行檔；本機這邊接著呼叫 `request_exit()` 走既有的
/// 正常關閉流程（GUI 收尾 raw mode/alternate screen、CLI 靠
/// `spawn_exit_watcher`），不是 `std::process::exit` 硬中斷。
///
/// 跟 `exit`/`shell` 一樣是透過 `execute_line` 處理，不管是本機終端機、GUI，
/// 還是透過 `remote` plugin 轉發過去的 `/api/exec`，都會走到同一個地方——
/// 這也是為什麼透過 remote 連線到別台機器之後打 `upgrade`，更新的是遠端那台
/// 機器，不是本機。
///
/// 設定這個環境變數（隨便什麼值，只看有沒有設）就會跳過自己重啟那一步
/// （`--respawn-after`），編譯成功後只呼叫 `request_exit()`——用在外層已經有
/// 別的機制會在這個行程結束後重新啟動的情境（例如 `utils/daemon.sh` 那種
/// `while true; do cargo run; done`）：如果兩邊都會在行程結束後各自啟動一個
/// 新的，會搶著 bind port 9759，其中一個會失敗、變成一個沒有 web server 的
/// 多餘 process。設了這個之後兩邊就不會打架，交給外層負責重啟即可。
pub const EXTERNAL_RESTART_ENV: &str = "CNG5_EXTERNAL_RESTART";

pub fn run_upgrade(shell: Arc<Mutex<Shell>>, output: Arc<OutputBuffer>) {
    thread::spawn(move || {
        // 一定要在 `cargo build` 之前先拿到目前執行檔的路徑：Linux 上
        // `env::current_exe()` 是即時查 `/proc/self/exe`，`cargo build` 完成
        // 後會把 `target/debug/cng5` 這個路徑換成新編出來的檔案（unlink 掉
        // 這個還在跑的行程原本用的那個 inode），這時候才查會拿到一個帶著
        // 「(deleted)」字尾、實際上不存在的路徑（因為這個行程本身還在用
        //「已經被換掉」的那個舊檔案），拿去 spawn 一定是 `No such file or
        // directory`。這裡先存成一般的路徑字串，跟哪個 inode 無關，之後
        // （新檔案已經編譯完成、真的存在那個路徑上）拿去 spawn 就沒問題。
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(err) => {
                output.push(&format!("upgrade 失敗（找不到目前的執行檔路徑）: {err:#}\n"));
                return;
            }
        };
        output.push("upgrade: git fetch --all\n");
        if let Err(err) = run_logged_command("git", &["fetch", "--all"]) {
            output.push(&format!("upgrade 失敗（git fetch）: {err:#}\n"));
            return;
        }
        output.push("upgrade: git reset --hard origin/main\n");
        if let Err(err) = run_logged_command("git", &["reset", "--hard", "origin/main"]) {
            output.push(&format!("upgrade 失敗（git reset）: {err:#}\n"));
            return;
        }
        output.push("upgrade: cargo build（背景編譯，服務不會中斷，可能要一兩分鐘）\n");
        if let Err(err) = run_logged_command("cargo", &["build"]) {
            output.push(&format!("upgrade 失敗（編譯錯誤，程式維持原樣繼續運作）: {err:#}\n"));
            return;
        }
        output.push("upgrade: 編譯成功，準備重啟...\n");
        if std::env::var_os(EXTERNAL_RESTART_ENV).is_some() {
            output.push(&format!(
                "偵測到環境變數 {EXTERNAL_RESTART_ENV}，交給外層的重啟機制處理（例如 daemon.sh），這裡不自己啟動新的執行檔\n"
            ));
            lock_shell(&shell).request_exit();
            return;
        }
        let pid = std::process::id();
        if let Err(err) = Command::new(&exe).arg(format!("--respawn-after={pid}")).spawn() {
            output.push(&format!("upgrade 失敗（無法啟動重啟程序，本次更新中止）: {err:#}\n"));
            return;
        }
        lock_shell(&shell).request_exit();
    });
}

/// 跑一個子行程，失敗（指令本身跑不起來，或跑起來但結束碼非 0）都回傳
/// `Err`，錯誤訊息帶 stderr 的內容方便診斷（`git`/`cargo` 的錯誤通常都印在
/// stderr）。
fn run_logged_command(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program).args(args).output().with_context(|| format!("執行 {program} 失敗"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{program} {} 失敗: {}", args.join(" "), stderr.trim());
    }
    Ok(())
}

/// 把一行指令裡 `#` 之後的內容當成註解砍掉，讓 `script.cli`（或 CLI/GUI 手動輸入）
/// 可以在指令後面加註解，例如 `panel show # 打開音樂面板`。只有沒被單引號/雙引號
/// 包住的 `#` 才算註解開頭——被引號包住的 `#`（例如某個參數本身就含 `#`）不受影響，
/// 維持原樣傳給 `shell_words::split` 解析。
fn strip_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    for (i, c) in line.char_indices() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => return &line[..i],
            _ => {}
        }
    }
    line
}

pub enum Mode {
    Root,
    InPlugin(String),
    InPanel(String),
    Remote(RemoteSession),
}

/// `connect` 連線的目標：同網域可以直接用 ip 打既有的 HTTP `/api/exec`
/// （`Http`）；跨 domain 沒有直接可達的 ip，要透過 `global` plugin 的 MQTT
/// session 加密中繼（`CrossDomain`，見 `send_cross_domain_request`）。`shell`
/// 指令（借一整個終端機給遠端的 host shell，見 `run_remote_shell`）只有
/// `Http` 這邊有意義——跨 domain 那端沒有 WebSocket 可以直接連，`execute_remote_line`
/// 攔截 `shell` 時會依這個回報錯誤。
#[derive(Clone)]
enum RemoteTarget {
    Http { ip: String },
    CrossDomain { domain: String, target_id: String },
}

impl RemoteTarget {
    /// `remote_help_text` 顯示用的簡短描述。
    fn display(&self) -> String {
        match self {
            RemoteTarget::Http { ip } => ip.clone(),
            RemoteTarget::CrossDomain { domain, .. } => format!("跨 domain: {domain}"),
        }
    }
}

/// `connect` 最多願意為了拿到正確的初始 prompt 等背景查詢多久（見
/// `spawn_remote_worker` 的說明）——同網域在區網內通常幾十毫秒就回來，這個
/// 上限只是避免遠端很慢/連不上時把 `connect`（因此共用的 `Shell` 鎖）卡住
/// 太久，超過就放棄等待，退回「先假設 root」的舊行為。
const INITIAL_PROMPT_WAIT: Duration = Duration::from_millis(300);

/// `Mode::Remote` 底下的連線狀態。`remote_prompt` 是目前已知的遠端 prompt 字串
/// （例如 `"cng5> "`、`"cng5(wol)> "`），每次轉發一行、拿到遠端回應後更新，本機
/// 的 `Shell::prompt` 拿它組成 `"<id>::<remote_prompt>"`，感覺像真的在遠端那台
/// 機器前面打字。用 `Arc<Mutex<_>>` 而不是普通欄位，是因為實際轉發（`remote_exec`
/// 或跨 domain 的 `send_cross_domain_request`）可能要等到好幾秒的逾時上限，
/// 不能卡在 `execute_line` 裡——那是在持有共用的 `Shell` 鎖的情況下呼叫的，
/// 卡住的話 GUI 重繪、web 的其他請求全部都會跟著卡住等鎖。轉發改成丟進
/// `sender` 這個佇列，背景的 `spawn_remote_worker` 執行緒依序（一次一個，
/// 不會並行送出多個請求打亂順序）真正處理，完成後直接更新這個 `Arc`，不需要
/// 重新拿 `Shell` 自己的鎖。
pub struct RemoteSession {
    pub id: String,
    target: RemoteTarget,
    remote_prompt: Arc<Mutex<String>>,
    sender: mpsc::Sender<String>,
}

impl RemoteSession {
    /// 遠端目前是不是在它自己的 root——本機打 `exit`/`quit` 要不要被攔截當成
    /// 「斷線回本機」就是靠這個判斷（見 `Shell::execute_remote_line`）：現有
    /// `execute_line` 對 `(Mode::Root, "exit"|"quit")` 是真的把那個行程關掉，
    /// 如果不攔截、原封不動轉發，會在遠端不是自己想斷線的情況下把對方那台機器
    /// 的 cng5 關掉。
    fn at_root(&self) -> bool {
        *self.remote_prompt.lock().unwrap() == "cng5> "
    }
}

/// `connect` 呼叫這個開一個背景執行緒，專門負責這個連線的所有網路呼叫：
/// 1. 一開始先查一次遠端目前的 prompt（可能不是 root，例如上次連線沒有乾淨地
///    離開），更新 `remote_prompt`——這一步以前是在 `connect` 當下同步做的，
///    會卡住 `Shell` 的鎖，現在搬進背景執行緒，`connect` 本身立刻回傳。跨
///    domain 沒有這種查詢方式（沒有直接可達的 ip 打 `/api/prompt`），先假設
///    在 root，之後跟同網域一樣靠每次轉發的回應更新。查完（不管成不成功）都會
///    往 `initial_prompt_ready` 送一個訊號，`connect` 短暫等一下這個訊號（見
///    呼叫端），這樣同網域在區網內通常幾十毫秒就查得到的情況下，`connect`
///    剛回傳、使用者還沒下第一個指令時看到的 prompt 就已經是對的，不用等到
///    下了第一個指令、轉發回應回來才修正。
/// 2. 依序處理 `receiver` 收到的每一行（`execute_remote_line` 只是 `send`
///    進來，不等結果），依 `target` 是 `Http` 還是 `CrossDomain` 轉發、更新
///    `remote_prompt`／把錯誤訊息 push 進 `output`。
/// 3. `sender`（`RemoteSession` 那一份）被丟掉（斷線、`Mode` 換掉）時，
///    `receiver` 的疊代會自然結束，這個執行緒跟著結束，不需要額外的收尾訊號。
fn spawn_remote_worker(
    target: RemoteTarget,
    receiver: mpsc::Receiver<String>,
    remote_prompt: Arc<Mutex<String>>,
    output: Arc<OutputBuffer>,
    ctx: SharedContext,
    initial_prompt_ready: mpsc::Sender<()>,
) {
    thread::spawn(move || {
        if let RemoteTarget::Http { ip } = &target {
            ctx.lock().unwrap().log_activity("http-out", format!("GET http://{ip}:9759/api/prompt"));
            if let Some(prompt) = fetch_remote_prompt(ip) {
                *remote_prompt.lock().unwrap() = prompt;
            }
        }
        let _ = initial_prompt_ready.send(());
        for line in receiver {
            let result = match &target {
                RemoteTarget::Http { ip } => {
                    ctx.lock().unwrap().log_activity("http-out", format!("POST http://{ip}:9759/api/exec"));
                    remote_exec(ip, &line)
                }
                RemoteTarget::CrossDomain { domain, target_id } => {
                    let ask = CrossDomainAsk::Exec { target_id: target_id.clone(), line: line.clone() };
                    match send_cross_domain_request(&ctx, domain, ask) {
                        Ok(RemoteReply::Exec { prompt, error, .. }) => Ok((prompt, error)),
                        Ok(RemoteReply::Error { message, .. }) => Err(anyhow::anyhow!(message)),
                        Ok(_) => Err(anyhow::anyhow!("收到不符預期的回覆型別")),
                        Err(err) => Err(err),
                    }
                }
            };
            match result {
                Ok((prompt, error)) => {
                    *remote_prompt.lock().unwrap() = prompt;
                    if let Some(msg) = error {
                        output.push(&format!("錯誤: {msg}\n"));
                    }
                }
                Err(err) => {
                    output.push(&format!("連線失敗: {err:#}\n"));
                }
            }
        }
    });
}

/// `connect`/每一行轉發呼叫的既有端點（`web.rs` 的 `/api/prompt`/`/api/exec`，
/// 本來是給 web 前端用的），解析出來的回應形狀。
#[derive(serde::Deserialize)]
struct RemotePromptResponse {
    prompt: String,
}

#[derive(serde::Deserialize)]
struct RemoteExecResponse {
    prompt: String,
    error: Option<String>,
}

/// `connect` 剛連上時查一次遠端目前的 prompt，這樣本機的顯示（`Shell::prompt`）
/// 才能從一開始就正確反映遠端當下的狀態（可能不是 root，例如上次連線沒有乾淨地
/// 離開）。查不到（連不上）就回傳 `None`，呼叫端會先假設在 root，之後每次轉發
/// 指令都會再更新。
fn fetch_remote_prompt(ip: &str) -> Option<String> {
    let url = format!("http://{ip}:9759/api/prompt");
    let output = Command::new("curl").args(["--silent", "--max-time", "5", &url]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let body = String::from_utf8(output.stdout).ok()?;
    serde_json::from_str::<RemotePromptResponse>(&body).ok().map(|r| r.prompt)
}

/// 把 `line` POST 給 `ip` 這台機器既有的 `/api/exec`（web UI 輸入框用的那個
/// 端點），回傳 `(遠端執行完的新 prompt, 錯誤訊息)`。跟 `global.rs` 的
/// `pull_global_from_server` 一樣透過 `curl` 子行程打 HTTP，不額外引入 HTTP
/// client crate。`pub(crate)` 是因為 `global.rs` 的跨 domain remote 請求處理
/// （收到 `RemoteRequest::Exec` 之後）也要呼叫這個轉發到同網域內實際的目標
/// 裝置，不想為了同一件事重寫一份。
pub(crate) fn remote_exec(ip: &str, line: &str) -> Result<(String, Option<String>)> {
    let body = serde_json::json!({ "line": line }).to_string();
    let url = format!("http://{ip}:9759/api/exec");
    let output = Command::new("curl")
        .args([
            "--silent",
            "--max-time",
            "5",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
            &url,
        ])
        .output()
        .context("執行 curl 失敗")?;
    if !output.status.success() {
        bail!("連不上 {ip}:9759");
    }
    let body = String::from_utf8(output.stdout).context("回應不是合法的 UTF-8")?;
    let resp: RemoteExecResponse = serde_json::from_str(&body).context("回應格式不對")?;
    Ok((resp.prompt, resp.error))
}

/// 產生跨 domain 請求的關聯 id：不需要密碼學等級的隨機性，只要在「這個
/// process 目前還沒處理完的請求」裡不重複即可（見 `plugin::RemoteRequest`
/// 的說明），用 hostname + pid + 行程內遞增計數器組出來就夠。
fn new_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}-{n}", sysinfo::hostname(), std::process::id())
}

/// 跨 domain remote 的核心送出邏輯，`remote` 指令轉發（`spawn_remote_worker`
/// 的 `CrossDomain` 分支）跟 `remote-output` 的跨 domain 輪詢（見
/// `plugins::remote_output`）共用同一份：
/// - 本機本身是 server 且 MQTT 已連上（`ctx.mqtt_client` 有值）：直接呼叫
///   `send_via_mqtt` 把請求加密發布出去。
/// - 其餘情況（不是 server，或是 server 但 MQTT 還沒連上）：改成呼叫自己
///   `system` 的 `server <ip>`（`ctx.server_addr`）新增的
///   `/api/remote/cross-relay` 端點，把同一份 `ask` 交給它代為送出——那台
///   機器收到之後會再呼叫這同一個函式（這次換它符合「是 server 且 MQTT 已
///   連上」），實際做事的邏輯只有一份，不因為是「直接發」還是「中繼」而各自
///   重寫一次。只有離開這個 domain、上公開 broker 那一段才需要 AEAD 加密，
///   這裡到自己 server 之間走既有同網域信任範圍的明文 HTTP，跟 `remote_exec`
///   是一樣的做法。
pub(crate) fn send_cross_domain_request(ctx: &SharedContext, target_domain: &str, ask: CrossDomainAsk) -> Result<RemoteReply> {
    let (is_server, mqtt_client, server_addr) = {
        let inner = ctx.lock().unwrap();
        (inner.is_server, inner.mqtt_client.clone(), inner.server_addr.clone())
    };
    if is_server {
        let (bridge_id, client) = mqtt_client
            .lock()
            .unwrap()
            .clone()
            .context("目前沒有連上 MQTT（跨 domain remote 需要 global 的 bridge 有設定且連線成功）")?;
        ctx.lock().unwrap().log_activity("mqtt-out", format!("publish {bridge_id}/{target_domain}/remote/request"));
        send_via_mqtt(ctx, &bridge_id, &client, target_domain, ask)
    } else {
        let addr = server_addr.context("跨 domain remote 需要先用 system 的 server <ip> 設定要中繼的伺服器")?;
        ctx.lock().unwrap().log_activity("http-out", format!("POST http://{addr}:9759/api/remote/cross-relay"));
        send_via_relay(&addr, target_domain, ask)
    }
}

/// 等 `RemoteReply` 送回來要等多久，依 `ask` 種類不同——`Exec`/`Panel` 收到
/// 請求那一端只需要呼叫既有的本機邏輯（`remote_exec`/`fetch_panel_text_once`），
/// 這兩個本身的 `--max-time` 都只有 5 秒左右，給發起端 5 秒等待綽綽有餘。但
/// `FileList`/`FilePull`/`FilePush` 收到請求那一端（`global.rs` 的
/// `fetch_remote_file_list`/`fetch_file_chunk`/`push_file_chunk`）還要再多轉
/// 一手 HTTP 請求給實際的目標裝置（`--max-time 10`），如果還是只給發起端
/// 5 秒，這一手中繼隨便比 MQTT 本身的傳輸延遲慢個幾秒就會被誤判成逾時——實際
/// 上請求可能還在正常處理，只是還沒回來而已。所以檔案相關的請求給更寬裕的
/// 20 秒，蓋過中繼那一手的 10 秒逾時上限，再留幾秒給 MQTT 本身的來回延遲。
fn cross_domain_timeout(ask: &CrossDomainAsk) -> Duration {
    match ask {
        CrossDomainAsk::Exec { .. } | CrossDomainAsk::Panel { .. } => Duration::from_secs(5),
        CrossDomainAsk::FileList { .. } | CrossDomainAsk::FilePull { .. } | CrossDomainAsk::FilePush { .. } => {
            Duration::from_secs(20)
        }
    }
}

/// 直接把 `ask` 包成 `RemoteRequest`（`request_id`/`source_domain` 在這裡才
/// 填上——`source_domain` 一定要是「真正發布出去那一刻」這台機器自己的
/// `domain_name`，不能讓呼叫端自己填，中繼過來的請求才會用中繼那台機器的
/// domain，而不是原始發起端的），加密發布到 `<bridge-id>/<target-domain>/remote/request`，
/// 在 `ctx.cross_domain_pending` 登記一個 channel，等 `global.rs` 的 MQTT
/// session 收到對應的 `RemoteReply` 送過來（逾時多久見 `cross_domain_timeout`）。
/// 不管成功/失敗/逾時，最後都要把登記的 channel 從表裡清掉，不然逾時放棄的
/// 請求會一直留著佔位置。
fn send_via_mqtt(
    ctx: &SharedContext,
    bridge_id: &str,
    client: &Client,
    target_domain: &str,
    ask: CrossDomainAsk,
) -> Result<RemoteReply> {
    let timeout = cross_domain_timeout(&ask);
    let source_domain = ctx
        .lock()
        .unwrap()
        .domain_name
        .clone()
        .context("跨 domain remote 需要先用 global 的 domain <name> 設定自己的 domain 名稱")?;
    let request_id = new_request_id();
    let request = match ask {
        CrossDomainAsk::Exec { target_id, line } => {
            RemoteRequest::Exec { request_id: request_id.clone(), source_domain, target_id, line }
        }
        CrossDomainAsk::Panel { target_id, panel_name } => {
            RemoteRequest::Panel { request_id: request_id.clone(), source_domain, target_id, panel_name }
        }
        CrossDomainAsk::FileList { target_id, folder, offset } => {
            RemoteRequest::FileList { request_id: request_id.clone(), source_domain, target_id, folder, offset }
        }
        CrossDomainAsk::FilePull { target_id, folder, name, offset } => {
            RemoteRequest::FilePull { request_id: request_id.clone(), source_domain, target_id, folder, name, offset }
        }
        CrossDomainAsk::FilePush { target_id, folder, name, offset, data } => {
            RemoteRequest::FilePush { request_id: request_id.clone(), source_domain, target_id, folder, name, offset, data }
        }
    };

    let pending = ctx.lock().unwrap().cross_domain_pending.clone();
    let (tx, rx) = mpsc::channel();
    pending.lock().unwrap().insert(request_id.clone(), tx);

    let result = (|| -> Result<RemoteReply> {
        let sealed = crypto::seal(&request)?;
        let topic = format!("{bridge_id}/{target_domain}/remote/request");
        client.publish(topic, QoS::AtMostOnce, false, sealed).map_err(|err| anyhow::anyhow!("MQTT publish 失敗: {err}"))?;
        rx.recv_timeout(timeout).map_err(|_| anyhow::anyhow!("等待跨 domain 回覆逾時（{} 秒）", timeout.as_secs()))
    })();

    pending.lock().unwrap().remove(&request_id);
    result
}

/// 中繼一次跨 domain 請求給 `addr`（自己的 system server）新增的
/// `/api/remote/cross-relay` 端點。`--max-time` 要比對方那一端
/// `send_via_mqtt` 實際會等的時間（見 `cross_domain_timeout`）再多一點餘裕，
/// 蓋掉這段 HTTP 呼叫本身的往返時間——不然這一段自己先逾時放棄，對方那邊的
/// `send_via_mqtt` 可能根本都還沒等到期。
fn send_via_relay(addr: &str, target_domain: &str, ask: CrossDomainAsk) -> Result<RemoteReply> {
    let curl_timeout = (cross_domain_timeout(&ask) + Duration::from_secs(5)).as_secs().to_string();
    let body = serde_json::json!({ "domain": target_domain, "ask": ask }).to_string();
    let url = format!("http://{addr}:9759/api/remote/cross-relay");
    let output = Command::new("curl")
        .args([
            "--silent",
            "--max-time",
            &curl_timeout,
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
            &url,
        ])
        .output()
        .context("執行 curl 失敗")?;
    if !output.status.success() {
        bail!("連不上 {addr}:9759");
    }
    let body = String::from_utf8(output.stdout).context("回應不是合法的 UTF-8")?;
    serde_json::from_str::<RemoteReply>(&body).context("回應格式不對")
}

/// `remote` 連線裡打 `shell`：不像其他指令轉發成一行文字給遠端執行，而是直接借用
/// 遠端既有的 `/api/shell/ws`（web 前端 xterm.js 面板用的同一個端點，見 `web.rs`
/// 的 `shell_ws`）在遠端開一個真正的 host shell，把目前這個終端機整個接上那個
/// PTY，效果跟 ssh 過去一樣：本地打的每個位元組原封不動送過去，遠端印出來的每個
/// 位元組原封不動印在本地——遠端 PTY 自己會處理 echo/行編輯，本地不能重複做，
/// 也不能用一行一行轉發（vim/top 這類全螢幕程式需要即時的位元組級輸出入）。
/// 呼叫端（CLI/GUI）要先自己把終端機切到 raw mode 才呼叫這個，結束後恢復；GUI
/// 另外要離開 alternate screen（讓遠端的畫面用一般的、可以捲動回看的螢幕，跟
/// 本地 `shell` passthrough 一樣），這兩件事都跟終端機物件（`Terminal`）綁在一起，
/// 這裡不知道那個型別，交給呼叫端做。連線失敗（連不上、握手失敗）就把錯誤訊息
/// push 進 `output`，跟本地 `run_host_shell` 失敗時「當作使用者按一下就離開」
/// 不同——遠端連線失敗比較可能發生也比較需要讓使用者知道原因。
pub fn run_remote_shell(ip: &str, output: &OutputBuffer) {
    if let Err(err) = remote_shell_session(ip) {
        output.push(&format!("連線失敗: {err:#}\n"));
    }
}

/// 把 `resize` 訊息編碼成 `web.rs` `shell_ws` 認得的格式（跟 `frontend.html` 那個
/// xterm.js 面板送出去的一樣），送不出去（例如連線已經斷了）就放棄，下一輪迴圈
/// 自然會因為讀取失敗結束整個 session，不需要在這裡額外處理。
fn send_resize(socket: &mut WebSocket<TcpStream>, cols: u16, rows: u16) {
    let msg = serde_json::json!({ "resize": { "cols": cols, "rows": rows } }).to_string();
    let _ = socket.send(Message::from(msg));
}

/// `run_remote_shell` 實際做事的地方，拆出來單純是為了能用 `?` 提早回傳。
fn remote_shell_session(ip: &str) -> Result<()> {
    let stream = TcpStream::connect((ip, 9759)).with_context(|| format!("連不上 {ip}:9759"))?;
    let _ = stream.set_nodelay(true);
    // 握手（讀 HTTP 回應）要能一次讀到完整回應，不能在這裡就設定短逾時——
    // `tungstenite::client` 不會自己重試被逾時中斷的讀取。逾時留到握手成功、
    // 進入下面互動迴圈才設定（見下方 `set_read_timeout`）。
    let url = format!("ws://{ip}:9759/api/shell/ws");
    let (mut socket, _response) =
        tungstenite::client(url, stream).map_err(|err| anyhow::anyhow!("WebSocket 握手失敗: {err}"))?;
    // 進入互動迴圈之後，同一個執行緒要輪流檢查「遠端有沒有新輸出」跟「本地鍵盤
    // 有沒有新輸入」，讀取不能一直卡住，所以設一個短逾時；逾時傳回的
    // `ErrorKind` 依平台不同（Unix 是 `WouldBlock`，Windows 是 `TimedOut`），
    // 下面兩種都當作「這次沒有新資料」處理。
    socket.get_ref().set_read_timeout(Some(Duration::from_millis(20)))?;

    // 鍵盤輸入沒辦法非阻塞地讀，改用獨立執行緒阻塞讀 stdin，讀到的位元組透過
    // channel 轉給下面的主迴圈。這個執行緒在使用者離開遠端 shell 之後可能還卡在
    // 讀 stdin 上，理論上要下一次按鍵、送進已經沒人收的 channel 失敗了才會結束
    // ——如果使用者在那之前又立刻進了另一個 remote shell，這個還沒死透的舊執行緒
    // 有機會搶走第一個按鍵。這是刻意接受的邊角案例，不為了它另外實作可以中斷的
    // stdin 讀取。
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) if tx.send(buf[..n].to_vec()).is_err() => break,
                Ok(_) => {}
            }
        }
    });

    let mut stdout = std::io::stdout();
    let mut last_size = crossterm::terminal::size().ok();
    if let Some((cols, rows)) = last_size {
        send_resize(&mut socket, cols, rows);
    }
    loop {
        for bytes in rx.try_iter() {
            socket.send(Message::from(bytes)).context("送出鍵盤輸入失敗")?;
        }
        // 順便看一下終端機大小有沒有變，變了就通知遠端調整 PTY 大小，不然遠端
        // 那邊全螢幕的程式（例如 vim/top）畫面會跟本地視窗大小對不起來。
        if let Ok(size) = crossterm::terminal::size()
            && Some(size) != last_size
        {
            last_size = Some(size);
            send_resize(&mut socket, size.0, size.1);
        }
        match socket.read() {
            Ok(Message::Binary(data)) => {
                stdout.write_all(&data)?;
                stdout.flush()?;
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(err))
                if err.kind() == std::io::ErrorKind::WouldBlock || err.kind() == std::io::ErrorKind::TimedOut => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

/// 每個 plugin 的 panel 位置/大小（`rect` 設定，單位是佔整個畫面的百分比 0-100）
/// 跟顯示與否（`show`/`hidden`），GUI 畫面依這個狀態把 panel 畫出來。
#[derive(Clone, Copy)]
pub struct PanelState {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    pub visible: bool,
}

/// Alt-方向鍵移動 panel 時，x/y 最多只能到這個百分比，讓 panel 至少留
/// `100 - PANEL_MAX_POSITION`% 還在畫面範圍內，不會整個往右/往下移出去看不見。
const PANEL_MAX_POSITION: i64 = 90;

/// Alt-WASD 縮放 panel 時，width/height 最小只能到這個百分比，不管加大還是
/// 減小都不會讓 panel 整個消失或縮到看不見。
const PANEL_MIN_SIZE: i64 = 10;

/// root mode 的 `manual` 指令：整體介面怎麼運作（plugin/mode/panel/history 這些
/// 概念怎麼串起來），比 `help` 那種一行式指令清單詳細。各 plugin 進去之後自己的
/// `manual` 是 `Plugin::manual_text`，這裡只講 root 這一層。
const ROOT_MANUAL_TEXT: &str = "\
cng5 把各種小工具包成一個個 plugin，root 負責在它們之間切換、管理 CLI/GUI 畫面。

範例：
  plugin show           列出目前有哪些 plugin
  plugin enter wol      進入 wol plugin，之後打的指令都是它的（help/manual 看
                        它有哪些指令）
  mode gui              切換成上下可以開好幾個 panel 的 GUI 畫面
  mode cli              切換回一般的命令列畫面
  shell                 借用目前終端機開一個真正的 host shell，exit 就會回來
  upgrade               更新這台機器的 cng5（見下面的說明）
  history               列出之前執行過的指令
  !3                    重新執行 history 裡第 3 筆指令

進到某個 plugin 之後：
  help                  這個 plugin 底下的指令清單（一行式簽名）
  manual                這個 plugin 更完整的說明與範例
  panel                 （只有 GUI 畫面才有）進去設定這個 plugin 的 panel
                        位置/大小/顯示與否
  ~                     不管在哪一層都直接跳回 root

upgrade：
  依序做 git fetch --all、git reset --hard origin/main、cargo build——編譯
  期間服務不中斷（舊的還在跑），只有編譯成功才會重啟成新版本；編譯失敗會把
  錯誤訊息印出來，程式維持原樣繼續運作，不會變成什麼都沒在跑。整個過程都在
  背景執行緒做，這裡打完 upgrade 立刻就能繼續下別的指令。
  因為跟 exit/shell 一樣是透過 execute_line 處理，透過 remote plugin 連線到
  別台機器之後打 upgrade，就是在遠端那台機器上做更新，不是本機。

  重啟的最後一步：編譯成功後會自己重新呼叫執行檔，等舊行程真的結束、放開
  port 9759 才啟動新的（見 --respawn-after）。如果外層已經有別的機制會在
  這個行程結束後自動重啟（例如 utils/daemon.sh 那種 while true 迴圈），設定
  環境變數 CNG5_EXTERNAL_RESTART（值隨便，只看有沒有設）讓 upgrade 編譯成功
  後只結束自己、不要自己重啟，避免兩邊同時搶著啟動下一個、搶 bind port。
";

impl Default for PanelState {
    fn default() -> Self {
        Self { x: 0, y: 0, width: 100, height: 100, visible: false }
    }
}

/// root mode 的 `mode cli` / `mode gui` 要求切換到哪一種畫面。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    Cli,
    Gui,
}

/// 依 plugin 名稱建立一個新實例。
pub type PluginFactory = Box<dyn Fn(SharedContext) -> Box<dyn Plugin> + Send>;

pub struct Shell {
    active: HashMap<String, Box<dyn Plugin>>,
    mode: Mode,
    should_exit: bool,
    requested_ui: Option<UiMode>,
    /// root mode 執行過 `shell` 之後設成 true，呼叫端（CLI/GUI 迴圈）應該在放開
    /// `Shell` 的鎖之後，把目前的終端機借給一個真正的 host shell 用（見
    /// `has_pending_shell_passthrough`/`take_pending_shell_passthrough`）。跟
    /// `requested_ui` 一樣不能直接在這裡跑（`execute_line` 呼叫時鎖還握著，
    /// 互動 shell 可能跑很久，會卡住其他共用同一個 `Shell` 的執行緒，例如 web）。
    requested_shell_passthrough: bool,
    /// `Mode::Remote` 底下執行過 `shell` 之後設成連線目標的 ip，呼叫端（CLI/GUI
    /// 迴圈）應該在放開 `Shell` 的鎖、把終端機切到 raw mode 之後呼叫
    /// `run_remote_shell` 把終端機接上遠端的 host shell（見
    /// `take_pending_remote_shell`）。跟 `requested_shell_passthrough` 是分開的
    /// 兩個旗標，因為兩者呼叫端要做的收尾動作不一樣（這個不能整個切回 cooked
    /// mode——需要維持 raw mode 才能逐位元組轉送）。
    requested_remote_shell: Option<String>,
    /// root mode 執行過 `upgrade` 之後設成 true，呼叫端（CLI/GUI/web 三個
    /// 呼叫 `execute_line` 的地方都要各自檢查）應該在放開 `Shell` 的鎖之後
    /// 呼叫 `run_upgrade`。跟 `requested_shell_passthrough` 不一樣的是這個不
    /// 需要借用終端機——`git fetch`/`cargo build` 全部在背景執行緒做，服務
    /// 不中斷，只有編譯成功、真的要重啟那一刻才會觸發 `should_exit`（見
    /// `run_upgrade`/`request_exit`）。
    requested_upgrade: bool,
    /// 目前實際顯示中的 UI（跟 `requested_ui` 不同：那個是「待切換」，這個是
    /// 「現在畫面就是這個」），`panel` 指令要不要出現在候選清單裡靠這個判斷。
    current_ui: UiMode,
    /// 各 plugin 的 panel 狀態，key 是 plugin 名稱。只有呼叫過 `rect`/`show`/`hidden`
    /// 的 plugin 才會有 entry，還沒設定過的視同 `PanelState::default()`（不顯示）。
    panels: HashMap<String, PanelState>,
    /// panel 的疊放順序（由下到上），每次 `show` 都把該 plugin 名稱移到最後面
    /// （最上層）。畫圖時依這個順序畫，最後畫的自然蓋在最上面，這樣「最近 show
    /// 的視窗蓋在最上層」才會是固定的行為，而不是依 HashMap 隨機順序決定。
    panel_order: Vec<String>,
    /// 目前處於「最大化」狀態的 panel，key 是 plugin 名稱、value 是最大化之前
    /// 的 rect，讓 Alt-M 再按一次時可以還原回去。有 entry 就表示該 panel 目前是
    /// 最大化的（`panels` 裡的 rect 已經被改成 0 0 100 100）。
    maximized: HashMap<String, PanelState>,
    output: Arc<OutputBuffer>,
    history: Vec<String>,
    /// `remote` plugin 的 `connect <id>` 要查 `ctx.devices` 找目標的 ip，
    /// `Mode::Remote` 斷線時要清掉 `ctx.remote_target`——這是目前唯一需要
    /// `Shell` 自己持有 `ctx` 的地方，其餘 plugin 邏輯都是透過各自建構時拿到的
    /// 那一份存取，不需要 `Shell` 插手。
    ctx: SharedContext,
}

impl Shell {
    /// 建立時就把每個 plugin 都建好實例，不用再另外執行指令加入。
    pub fn new(
        ctx: SharedContext,
        factories: Vec<(&'static str, PluginFactory)>,
        output: Arc<OutputBuffer>,
    ) -> Self {
        let active = factories
            .into_iter()
            .map(|(name, factory)| (name.to_string(), factory(ctx.clone())))
            .collect();
        Self {
            active,
            mode: Mode::Root,
            should_exit: false,
            requested_ui: None,
            requested_shell_passthrough: false,
            requested_remote_shell: None,
            requested_upgrade: false,
            current_ui: UiMode::Cli,
            panels: HashMap::new(),
            panel_order: Vec::new(),
            maximized: HashMap::new(),
            output,
            history: Vec::new(),
            ctx,
        }
    }

    /// 呼叫端（`main`）實際切換到哪個 UI 迴圈之後要呼叫這個同步狀態，
    /// 這樣 `panel` 指令是否列入候選清單才會反映當下真正的畫面。
    pub fn set_current_ui(&mut self, ui: UiMode) {
        self.current_ui = ui;
    }

    /// 目前實際顯示中的 UI，見 `set_current_ui`。給 `main` 背景的 exit 監控執行緒
    /// 判斷用：只有 CLI mode 才會卡在 `rl.readline()` 的阻塞讀取、不會主動檢查
    /// `should_exit`，需要額外用 `std::process::exit` 硬中斷；GUI mode 自己每
    /// 200ms 就會輪詢一次 `should_exit`，能在 raw mode/alternate screen 正常收尾
    /// 後自然離開，這裡不需要（也不應該）搶在它收尾前強制中斷行程。
    pub fn current_ui(&self) -> UiMode {
        self.current_ui
    }

    /// GUI 裡按 Tab 呼叫：把目前疊放順序最底下（最久沒被 activate）的那個可見
    /// panel 拉到最上層，變成新的 active panel。持續按 Tab 就會依序把每個開著
    /// 的 panel 都輪流拉到最上面。少於兩個可見 panel 時沒有意義，不做事。
    pub fn cycle_active_panel(&mut self) {
        let visible: Vec<&String> = self
            .panel_order
            .iter()
            .filter(|name| self.panels.get(*name).is_some_and(|state| state.visible))
            .collect();
        if visible.len() < 2 {
            return;
        }
        let least_recent = visible[0].clone();
        self.raise_panel(&least_recent);
    }

    /// 目前設成 `show` 的 panel 有哪些、各自的 rect 是什麼，依疊放順序（由下到上）
    /// 排列；GUI 畫面依這個清單依序畫圖，最後一個自然蓋在最上面，也是目前的
    /// active panel（GUI 用雙線外框標示）。
    pub fn visible_panels(&self) -> Vec<(String, PanelState)> {
        self.panel_order
            .iter()
            .filter_map(|name| {
                self.panels
                    .get(name)
                    .filter(|state| state.visible)
                    .map(|state| (name.clone(), *state))
            })
            .collect()
    }

    /// 之前執行過的每一行指令，不管是從 `script.cli`、CLI 還是 GUI 輸入的都在裡面，
    /// 依執行順序排列。給 CLI 的 rustyline history 跟 GUI 的上下鍵瀏覽共用。
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// `name` 這個 plugin 的 panel 要顯示的內容（`None` 就是空殼邊框）。GUI 畫面
    /// 依這個決定 panel 裡要畫什麼，取代原本用 plugin 名稱特判內容的作法。
    pub fn plugin_panel_text(&self, name: &str) -> Option<String> {
        self.active.get(name).and_then(|p| p.panel_text())
    }

    /// root mode 執行過 `exit` 之後回傳 true，呼叫端應該停止餵指令給這個 shell。
    pub fn should_exit(&self) -> bool {
        self.should_exit
    }

    /// root mode 執行過 `mode cli` / `mode gui` 之後回傳 true，呼叫端（目前這個
    /// UI 迴圈）應該結束、把控制權交還給外層去真正切換畫面。
    pub fn has_pending_mode_switch(&self) -> bool {
        self.requested_ui.is_some()
    }

    /// 取出並清除待切換的 UI mode，外層依這個結果決定接下來要跑哪個 UI 迴圈。
    pub fn take_requested_ui(&mut self) -> Option<UiMode> {
        self.requested_ui.take()
    }

    /// 取出並清除待執行的 shell passthrough 旗標：root mode 執行過 `shell` 之後
    /// 這裡回傳 true，呼叫端應該在放開鎖之後把終端機借給一個真正的 host shell 用。
    pub fn take_pending_shell_passthrough(&mut self) -> bool {
        std::mem::take(&mut self.requested_shell_passthrough)
    }

    /// 取出並清除待執行的 remote shell 旗標：`Mode::Remote` 底下執行過 `shell`
    /// 之後這裡回傳連線目標的 ip，呼叫端應該在放開鎖之後呼叫 `run_remote_shell`。
    pub fn take_pending_remote_shell(&mut self) -> Option<String> {
        self.requested_remote_shell.take()
    }

    /// 取出並清除待執行的 upgrade 旗標：root mode 執行過 `upgrade` 之後這裡
    /// 回傳 true，呼叫端應該在放開鎖之後呼叫 `run_upgrade`。
    pub fn take_pending_upgrade(&mut self) -> bool {
        std::mem::take(&mut self.requested_upgrade)
    }

    /// `run_upgrade` 背景執行緒編譯成功、真的要重啟時呼叫這個觸發跟 `exit`/
    /// `quit` 同一套的正常關閉流程——不是直接 `std::process::exit`，讓 GUI
    /// 有機會照原本的路徑收尾（離開 raw mode/alternate screen），CLI 也還是
    /// 靠既有的 `spawn_exit_watcher` 機制收掉卡住的 `readline()`。
    pub fn request_exit(&mut self) {
        self.should_exit = true;
    }

    pub fn prompt(&self) -> String {
        match &self.mode {
            Mode::Root => "cng5> ".to_string(),
            Mode::InPlugin(name) => format!("cng5({name})> "),
            Mode::InPanel(name) => format!("cng5({name}/panel)> "),
            Mode::Remote(session) => format!("{}::{}", session.id, session.remote_prompt.lock().unwrap()),
        }
    }

    pub fn execute_line(&mut self, line: &str) -> Result<()> {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            return Ok(());
        }
        if let Some(rest) = line.strip_prefix('!') {
            return match rest.parse::<usize>() {
                Ok(index) => self.execute_history_entry(index),
                // 不是 `!<數字>`，維持原本 `!` 開頭當註解跳過的用法。
                Err(_) => Ok(()),
            };
        }
        self.history.push(line.to_string());
        // `Mode::Remote` 不走底下這一套本地縮寫比對——遠端的指令集本機不知道，
        // 沒有候選清單可以比對，而且要轉發的是使用者輸入的原始文字，不能被
        // `shell_words::split` 重新斷詞/跳脫。
        if matches!(self.mode, Mode::Remote(_)) {
            return self.execute_remote_line(line);
        }
        let tokens = shell_words::split(line).context("指令解析失敗")?;
        let (cmd, args) = tokens.split_first().expect("已檢查過非空行");

        let top_level = self.next_word_candidates(&[]);
        let cmd = Self::resolve(cmd, &top_level)?;

        match (&self.mode, cmd.as_str()) {
            (Mode::Root, "help") => self.print_help(),
            (Mode::Root, "manual") => self.output.push(ROOT_MANUAL_TEXT),
            (Mode::Root, "history") => self.print_history(),
            (Mode::Root, "plugin") => {
                let sub_candidates = self.next_word_candidates(&["plugin"]);
                let sub_token = args
                    .first()
                    .context("plugin 後面要接 show / enter <name>")?;
                let sub = Self::resolve(sub_token, &sub_candidates)?;
                match sub.as_str() {
                    "show" => self.print_plugin_list(),
                    "enter" => {
                        let name = args.get(1).context("plugin enter 後面要接 plugin 名稱")?;
                        let name = self
                            .active
                            .contains_key(name)
                            .then(|| name.clone())
                            .with_context(|| format!("沒有這個 plugin: {name}"))?;
                        self.mode = Mode::InPlugin(name);
                    }
                    _ => unreachable!("resolve 只會回傳 sub_candidates 裡的字"),
                }
            }
            (Mode::Root, "mode") => {
                let sub_candidates = self.next_word_candidates(&["mode"]);
                let target_token = args.first().context("mode 後面要接 cli 或 gui")?;
                let target = Self::resolve(target_token, &sub_candidates)?;
                let target = match target.as_str() {
                    "cli" => UiMode::Cli,
                    "gui" => UiMode::Gui,
                    _ => unreachable!("resolve 只會回傳 sub_candidates 裡的字"),
                };
                self.requested_ui = Some(target);
                // 立刻同步 current_ui，這樣 script 裡 `mode gui` 後面接著的 panel
                // 相關指令馬上就能用，不用等到真的切換到 GUI 迴圈那一刻。
                self.current_ui = target;
            }
            (Mode::Root, "shell") => self.requested_shell_passthrough = true,
            (Mode::Root, "upgrade") => self.requested_upgrade = true,
            (Mode::Root, "exit" | "quit") => self.should_exit = true,
            (Mode::Root, _) => unreachable!("resolve 只會回傳 top_level 裡的字"),
            (Mode::InPlugin(_), "help") => self.print_help(),
            (Mode::InPlugin(name), "manual") => {
                let text = self.active.get(name).expect("mode 對應的 plugin 一定存在").manual_text();
                self.output.push(text);
            }
            (Mode::InPlugin(_), "history") => self.print_history(),
            // `panel` 只會出現在 next_word_candidates 裡（因此才能被 resolve 選到）
            // 當 current_ui 是 Gui 的時候，見 usage_lines；CLI 模式下打這個字
            // 在 resolve 那一步就已經被當成不認得的指令擋掉了，不會走到這裡。
            (Mode::InPlugin(name), "panel") => self.mode = Mode::InPanel(name.clone()),
            // `connect <id>` 只有在 `remote` plugin 裡才有意義：跟 `panel` 一樣是
            // `Shell` 自己攔截處理、不透過 `dispatch()`——因為這個指令需要「切換
            // mode」，`Plugin::dispatch` 的簽名做不到這件事（只能回傳
            // `Result<()>`），所以不能讓 `remote` plugin 自己處理 `connect`。
            (Mode::InPlugin(name), "connect") if name == "remote" => {
                let target = args.first().context("connect 需要接目標機器的 id，跨 domain 用 <domain>/<id>")?;
                // 含 `/` 就當成跨 domain 的 `<domain>/<id>`，否則維持原本查
                // 同網域 `ctx.devices` 的邏輯——這是刻意選的、向後相容的語法
                // 擴充，`global_registry_key` 本來就是這個 `"<domain>/<id>"`
                // 的格式，這裡沿用同一個約定。
                let (id, remote_target) = if let Some((domain, device_id)) = target.split_once('/') {
                    if !self.ctx.lock().unwrap().global.contains_key(target) {
                        bail!("沒有這個跨 domain 裝置: {target}（用 global list 查詢目前看得到的裝置）");
                    }
                    (
                        device_id.to_string(),
                        RemoteTarget::CrossDomain { domain: domain.to_string(), target_id: device_id.to_string() },
                    )
                } else {
                    let ip = self
                        .ctx
                        .lock()
                        .unwrap()
                        .devices
                        .get(target)
                        .map(|entry| entry.report.ip.clone())
                        .with_context(|| format!("沒有這個裝置: {target}（用 device list 查詢目前看得到的機器）"))?;
                    (target.clone(), RemoteTarget::Http { ip })
                };
                // 先假設遠端在 root，真正的初始 prompt 由 `spawn_remote_worker`
                // 背景查詢、查到後更新（跨 domain 沒有這種查詢方式，見
                // `spawn_remote_worker` 的說明）。這裡短暫等一下（上限
                // `INITIAL_PROMPT_WAIT`）背景查詢的結果——同網域在區網內通常
                // 幾十毫秒就會回來，等得到的話 `connect` 剛回傳時顯示的 prompt
                // 就已經是對的；等不到（逾時、或跨 domain 本來就不用等）就直接
                // 放棄繼續，退回原本「先假設 root」的行為，之後第一次轉發指令
                // 的回應一樣會修正，不會讓 `connect` 卡住太久、影響共用的
                // `Shell` 鎖。
                let remote_prompt = Arc::new(Mutex::new("cng5> ".to_string()));
                let (sender, receiver) = mpsc::channel();
                let (ready_tx, ready_rx) = mpsc::channel();
                spawn_remote_worker(
                    remote_target.clone(),
                    receiver,
                    remote_prompt.clone(),
                    self.output.clone(),
                    self.ctx.clone(),
                    ready_tx,
                );
                let _ = ready_rx.recv_timeout(INITIAL_PROMPT_WAIT);
                {
                    let mut inner = self.ctx.lock().unwrap();
                    match &remote_target {
                        RemoteTarget::Http { ip } => inner.remote_target = Some((id.clone(), ip.clone())),
                        RemoteTarget::CrossDomain { domain, target_id } => {
                            inner.cross_domain_remote_target = Some((domain.clone(), target_id.clone()))
                        }
                    }
                }
                self.output.push(&format!("已連線到 {target}\n"));
                self.mode = Mode::Remote(RemoteSession { id, target: remote_target, remote_prompt, sender });
            }
            // `..`：往上一層，這一層底下只有 root，跟 `exit`/`quit` 是同一件事。
            (Mode::InPlugin(_), "exit" | "quit" | "..") => self.mode = Mode::Root,
            // `~`/`...`：不管巢狀多深都直接跳回 root，跟逐層 `exit`/`..` 不同——
            // 這一層往上兩層一樣是 root（最多只有兩層），所以效果跟 `~` 相同。
            (Mode::InPlugin(_), "~" | "...") => self.mode = Mode::Root,
            (Mode::InPlugin(name), other) => {
                self.active
                    .get_mut(name)
                    .expect("mode 對應的 plugin 一定存在")
                    .dispatch(other, args, &self.output)?;
            }
            (Mode::InPanel(_), "help") => self.print_help(),
            (Mode::InPanel(name), "rect") => {
                let name = name.clone();
                self.panel_rect(&name, args)?;
            }
            (Mode::InPanel(name), "show") => {
                let name = name.clone();
                self.panel_show(&name);
            }
            (Mode::InPanel(name), "hidden") => {
                let name = name.clone();
                self.panel_hidden(&name);
            }
            (Mode::InPanel(name), "activate") => {
                let name = name.clone();
                self.panel_activate(&name);
            }
            // `..`：往上一層，回到這個 plugin（不是 root），跟 `exit`/`quit` 一樣。
            (Mode::InPanel(name), "exit" | "quit" | "..") => self.mode = Mode::InPlugin(name.clone()),
            // `~`/`...`：往上兩層，從 panel 這一層剛好是 root，`..` 只能到 plugin
            // 這一層，兩者才會不一樣。
            (Mode::InPanel(_), "~" | "...") => self.mode = Mode::Root,
            (Mode::InPanel(_), _) => unreachable!("resolve 只會回傳 usage_lines 裡的字"),
            // `Mode::Remote` 在到達這裡之前，`execute_line` 開頭就已經提早攔截
            // 轉去 `execute_remote_line` 處理了（見上面那段 `matches!` 判斷），
            // 這個分支只是為了讓 `match` 窮舉 `Mode` 的所有變體，實際不會被執行到。
            (Mode::Remote(_), _) => unreachable!("Mode::Remote 應該已經在 execute_line 開頭被攔截"),
        }
        Ok(())
    }

    /// `Mode::Remote` 底下每一行的處理：攔截在本機處理，或整行原封不動丟進轉發
    /// 佇列（見 `spawn_remote_worker`）。`line` 已經是 `execute_line` 開頭
    /// `strip_comment`/`trim` 過的。這裡不會等轉發真的執行完才回傳——真正的
    /// `remote_exec` HTTP 呼叫在背景執行緒做，`execute_line` 呼叫這裡的時候
    /// 持有共用的 `Shell` 鎖，等網路回應會卡住 GUI 重繪跟 web 的其他請求。
    fn execute_remote_line(&mut self, line: &str) -> Result<()> {
        let Mode::Remote(session) = &self.mode else {
            unreachable!("呼叫端（execute_line）已經確認 self.mode 是 Remote");
        };
        let first_word = line.split_whitespace().next().unwrap_or("");
        if (first_word == "exit" || first_word == "quit") && session.at_root() {
            {
                let mut inner = self.ctx.lock().unwrap();
                inner.remote_target = None;
                inner.cross_domain_remote_target = None;
            }
            self.mode = Mode::InPlugin("remote".to_string());
            self.output.push("已離線，回到 remote plugin\n");
            return Ok(());
        }
        // `upgrade`：一樣透過 `sender` 佇列轉發給遠端執行（worker 執行緒已經
        // 收到這筆，就算本機接下來馬上斷線，還是會把它送完，不會漏掉），但
        // 本機這邊主動斷線回到 remote plugin——遠端接下來會重啟（建置成功
        // 後自己重啟，或交給 daemon.sh 之類的外層機制），繼續留在
        // `<id>::...` 這個 prompt 會讓人誤以為還連得上，之後打的指令很可能
        // 送到還在重啟中途、還沒起來的目標，先斷線比較不會誤會。
        if first_word == "upgrade" {
            if session.sender.send(line.to_string()).is_err() {
                self.output.push("連線失敗: 背景轉發執行緒已經結束\n");
            }
            let id = session.id.clone();
            {
                let mut inner = self.ctx.lock().unwrap();
                inner.remote_target = None;
                inner.cross_domain_remote_target = None;
            }
            self.mode = Mode::InPlugin("remote".to_string());
            self.output.push(&format!("已送出 upgrade 給 {id}，該裝置即將重啟，已斷線回到 remote plugin\n"));
            return Ok(());
        }
        // `shell`：本機攔截處理，不透過 `sender`/`remote_exec` 轉發成一行文字
        // 指令——那個管道對應的是遠端「自己的 Shell/Mode」，跟 `shell` 真正要做的
        // 事（借一整個終端機給遠端的 host shell 用）是兩回事，見
        // `run_remote_shell` 的說明。跨 domain 連線沒有直接可達的 ip，這個
        // WebSocket 接不上去，直接報錯而不是靜靜地失敗。
        if first_word == "shell" {
            match &session.target {
                RemoteTarget::Http { ip } => {
                    self.requested_remote_shell = Some(ip.clone());
                }
                RemoteTarget::CrossDomain { .. } => {
                    self.output.push("跨 domain 連線不支援 shell（沒有直接可達的 ip 可以開 WebSocket）\n");
                }
            }
            return Ok(());
        }
        // 送不出去（worker 執行緒已經結束，理論上不會發生，`sender`/`receiver`
        // 是這個 session 一起建立、一起結束的）就當作連線失敗處理。
        if session.sender.send(line.to_string()).is_err() {
            self.output.push("連線失敗: 背景轉發執行緒已經結束\n");
        }
        Ok(())
    }

    /// `!<index>`：重新執行 `history` 指令列出的第 `index` 筆（從 1 開始算）。
    fn execute_history_entry(&mut self, index: usize) -> Result<()> {
        let cmd = index
            .checked_sub(1)
            .and_then(|i| self.history.get(i))
            .cloned()
            .with_context(|| format!("history 沒有第 {index} 筆"))?;
        self.execute_line(&cmd)
    }

    /// 用前綴比對 `token` 對應到 `candidates` 裡的哪一個：完全相符優先；不然找前綴
    /// 唯一對到誰，找不到或有超過一個都算錯誤（讓使用者可以打縮寫，像 `p e wol`
    /// 表示 `plugin enter wol`，但有歧異時要清楚報錯而不是亂猜）。
    fn resolve(token: &str, candidates: &[&str]) -> Result<String> {
        if candidates.contains(&token) {
            return Ok(token.to_string());
        }
        let matches: Vec<&str> = candidates.iter().copied().filter(|c| c.starts_with(token)).collect();
        match matches.as_slice() {
            [] => bail!("不認得指令: {token}"),
            [single] => Ok(single.to_string()),
            many => bail!("指令不明確: {token}（可能是: {}）", many.join(", ")),
        }
    }

    /// 印出目前所在 mode 的說明，`help` 指令呼叫這個。
    pub fn print_help(&self) {
        self.output.push(&self.help_text());
    }

    /// 按下 `?` 時依游標左側已輸入的內容決定要顯示什麼文字：
    /// - 整行還是空的 -> 跟 `help` 一樣的完整說明
    /// - `<已完整輸入的字> ?`（`?` 前面有空白）-> 只列出前面那些字底下、完全對應的用法
    /// - 其餘情況（`?` 前面沒有空白）-> 比對「正在打的那個字」的前綴，列出符合的下一個字
    ///
    /// 兩種情況都是照「已經打完的字」逐一比對用法字串前面的字，不管打到第幾層都適用。
    /// 回傳文字而不是直接印出，讓呼叫端（互動模式）可以透過 rustyline 的
    /// external printer 顯示，藉此讓提示字元在顯示完之後自動重新出現。
    pub fn context_help_text(&self, before_cursor: &str) -> String {
        if before_cursor.trim().is_empty() {
            return self.help_text();
        }

        if before_cursor.ends_with(char::is_whitespace) {
            let tokens: Vec<&str> = before_cursor.split_whitespace().collect();
            self.word_usages_text(&tokens)
        } else {
            let mut tokens: Vec<&str> = before_cursor.split_whitespace().collect();
            let partial = tokens.pop().unwrap_or("");
            self.name_matches_text(&tokens, partial)
        }
    }

    fn help_text(&self) -> String {
        match &self.mode {
            Mode::Root => self.root_help_text(),
            Mode::InPlugin(name) => self.plugin_help_text(name),
            Mode::InPanel(name) => self.panel_help_text(name),
            Mode::Remote(session) => Self::remote_help_text(session),
        }
    }

    fn remote_help_text(session: &RemoteSession) -> String {
        format!(
            "目前連線到 {}（{}）：打的每一行都會原封不動轉發過去執行，不是本機的\n\
             指令，本機也不知道遠端有哪些指令可以打。\n\
             在遠端的 root 下 exit/quit 會離開這個連線、回到本機的 remote plugin；\n\
             其餘情況下 exit/quit/~/../... 都是在遠端那邊生效（跳回遠端自己的上一層），\n\
             不會離開這個連線。\n",
            session.id,
            session.target.display()
        )
    }

    /// 目前 mode 底下所有指令的用法字串（第一個字是指令名稱）。
    fn usage_lines(&self) -> Vec<&str> {
        match &self.mode {
            Mode::Root => vec![
                "help",
                "manual",
                "history",
                "plugin show",
                "plugin enter <name>",
                "mode cli",
                "mode gui",
                "shell",
                "upgrade",
                "exit",
                "quit",
            ],
            Mode::InPlugin(name) => {
                let plugin = self
                    .active
                    .get(name)
                    .expect("mode 對應的 plugin 一定存在");
                let mut lines = vec!["help", "manual", "history"];
                lines.extend(plugin.commands());
                // panel 只有 GUI 畫面才有意義，CLI 底下不列入候選、也就打不出來。
                if self.current_ui == UiMode::Gui {
                    lines.push("panel");
                }
                lines.push("exit");
                lines.push("quit");
                lines.push("~");
                lines.push("..");
                lines.push("...");
                lines
            }
            Mode::InPanel(_) => vec![
                "help",
                "rect <x> <y> <width> <height>",
                "show",
                "hidden",
                "activate",
                "exit",
                "quit",
                "~",
                "..",
                "...",
            ],
            // `Mode::Remote` 底下實際打的每一行都轉發給遠端（見
            // `execute_remote_line`），遠端有哪些指令本機並不知道、沒辦法窮舉，
            // 這裡只列出本機真的會攔截處理的保留字，給 `?`/`context_help_text`
            // 一個提示用，不是完整的候選清單。
            Mode::Remote(_) => vec!["exit", "quit"],
        }
    }

    /// `line` 開頭的字是否逐一對應 `prefix_tokens`。
    fn line_starts_with(line: &str, prefix_tokens: &[&str]) -> bool {
        let mut words = line.split_whitespace();
        prefix_tokens
            .iter()
            .all(|tok| words.next() == Some(*tok))
    }

    /// 已經打完 `prefix_tokens` 之後，下一個字可能是哪些（去重複）。這既是 `?`
    /// 前綴提示的資料來源，也是 `resolve` 縮寫比對的候選清單——單一事實來源，
    /// 兩邊才不會兜不起來。
    fn next_word_candidates(&self, prefix_tokens: &[&str]) -> Vec<&str> {
        let mut names: Vec<&str> = Vec::new();
        for line in self.usage_lines() {
            if !Self::line_starts_with(line, prefix_tokens) {
                continue;
            }
            if let Some(next_word) = line.split_whitespace().nth(prefix_tokens.len()) {
                if !names.contains(&next_word) {
                    names.push(next_word);
                }
            }
        }
        names
    }

    /// 已經打完 `prefix_tokens`，正在打下一個字（前綴是 `partial`）：
    /// 列出所有符合的下一個字（去重複）。
    fn name_matches_text(&self, prefix_tokens: &[&str], partial: &str) -> String {
        self.next_word_candidates(prefix_tokens)
            .into_iter()
            .filter(|name| name.starts_with(partial))
            .map(|name| format!("{name}\n"))
            .collect()
    }

    /// 已經打完 `prefix_tokens`（後面接著空白）：列出完全對應這個字序列的用法。
    fn word_usages_text(&self, prefix_tokens: &[&str]) -> String {
        let matches: Vec<&str> = self
            .usage_lines()
            .into_iter()
            .filter(|line| Self::line_starts_with(line, prefix_tokens))
            .collect();
        if matches.is_empty() {
            return self.help_text();
        }
        matches.iter().map(|line| format!("{line}\n")).collect()
    }

    fn root_help_text(&self) -> String {
        let mut s = String::new();
        s.push_str("可用指令:\n");
        s.push_str("  help                 顯示這個說明\n");
        s.push_str("  manual               顯示更完整的說明文件與範例\n");
        s.push_str("  history              列出之前執行過的指令\n");
        s.push_str("  !<n>                 重新執行 history 裡第 n 筆指令\n");
        s.push_str("  plugin show          列出可用的 plugin\n");
        s.push_str("  plugin enter <name>  進入 plugin mode\n");
        s.push_str("  mode cli             切換成一般的命令列畫面\n");
        s.push_str("  mode gui             切換成上下兩個 panel 的畫面\n");
        s.push_str("  shell                借用目前終端機開一個真正的 host shell，exit 就會回來\n");
        s.push_str("  upgrade              git fetch/reset + 重新編譯，成功才重啟（manual 有完整說明）\n");
        s.push_str("  exit                 離開程式\n");
        s.push_str("  quit                 跟 exit 一樣，離開程式\n");
        s.push_str(&self.plugin_list_text());
        s
    }

    fn print_plugin_list(&self) {
        self.output.push(&self.plugin_list_text());
    }

    /// `history` 指令：依執行順序列出目前為止跑過的每一行（含 `script.cli` 的部分）。
    fn print_history(&self) {
        let mut s = String::new();
        for (i, line) in self.history.iter().enumerate() {
            s.push_str(&format!("{:5}  {line}\n", i + 1));
        }
        self.output.push(&s);
    }

    /// 依名稱取得某個 plugin 的可變參考，讓外部（目前只有 GUI 的 notepad 編輯
    /// 功能，見 `gui.rs` 的 `with_notepad`）能透過 `Plugin::as_any_mut` 向下轉型成
    /// 具體型別直接操作內部狀態，而不用透過 `execute_line` 逐行送指令字串。
    pub fn plugin_mut(&mut self, name: &str) -> Option<&mut Box<dyn Plugin>> {
        self.active.get_mut(name)
    }

    /// 目前所有 plugin 的名稱（含 `output`），依字母順序排列。CLI 的 `plugin show`
    /// 跟 web 的 `/api/plugins` 共用這一份清單，不各自維護一套。
    pub fn plugin_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.active.keys().cloned().collect();
        names.sort();
        names
    }

    fn plugin_list_text(&self) -> String {
        format!("可用的 plugin: {}\n", self.plugin_names().join(", "))
    }

    fn plugin_help_text(&self, name: &str) -> String {
        let plugin = self
            .active
            .get(name)
            .expect("mode 對應的 plugin 一定存在");

        let mut s = format!("可用指令 ({name}):\n");
        s.push_str("  help               顯示這個說明\n");
        s.push_str("  manual             顯示這個 plugin 更完整的說明文件與範例\n");
        s.push_str("  history            列出之前執行過的指令\n");
        s.push_str("  !<n>               重新執行 history 裡第 n 筆指令\n");
        if self.current_ui == UiMode::Gui {
            s.push_str("  panel              進入 panel 畫面\n");
        }
        s.push_str("  exit               回到 root\n");
        s.push_str("  quit               跟 exit 一樣，回到 root\n");
        s.push_str("  ~                  跳回 root（不管在哪一層都直接回來）\n");
        s.push_str("  ..                 往上一層，回到 root（跟 exit 一樣）\n");
        s.push_str("  ...                往上兩層，這一層底下只有 root，跟 ~ 一樣\n");
        for cmd in plugin.commands() {
            s.push_str(&format!("  {cmd}\n"));
        }
        s
    }

    fn panel_help_text(&self, name: &str) -> String {
        let mut s = format!("可用指令 ({name}/panel):\n");
        s.push_str(&format!("  {:<30} 顯示這個說明\n", "help"));
        s.push_str(&format!(
            "  {:<30} 設定 panel 位置與大小（0-100 的整數）\n",
            "rect <x> <y> <width> <height>"
        ));
        s.push_str(&format!("  {:<30} 顯示這個 panel\n", "show"));
        s.push_str(&format!("  {:<30} 隱藏這個 panel\n", "hidden"));
        s.push_str(&format!("  {:<30} 把這個 panel 拉到最上層（不改變顯示與否）\n", "activate"));
        s.push_str(&format!("  {:<30} 回到 {name} plugin mode\n", "exit"));
        s.push_str(&format!("  {:<30} 跟 exit 一樣，回到 {name} plugin mode\n", "quit"));
        s.push_str(&format!("  {:<30} 跳回 root（不管在哪一層都直接回來）\n", "~"));
        s.push_str(&format!("  {:<30} 往上一層，回到 {name} plugin mode（跟 exit 一樣）\n", ".."));
        s.push_str(&format!("  {:<30} 往上兩層，從 panel 這一層剛好是 root，跟 ~ 一樣\n", "..."));
        s
    }

    /// panel 底下的 `rect <x> <y> <width> <height>`：四個參數都是 0-100 的整數，
    /// 表示這個 panel 在整個畫面裡的位置跟大小各佔的百分比（例如 x=10 代表左邊界在
    /// 整個畫面寬度的 10% 處，width=10 代表寬度是整個畫面寬度的 10%）。
    fn panel_rect(&mut self, name: &str, args: &[String]) -> Result<()> {
        if args.len() != 4 {
            bail!("rect 需要 4 個參數: <x> <y> <width> <height>");
        }
        let labels = ["x", "y", "width", "height"];
        let mut values = [0i64; 4];
        for i in 0..4 {
            let raw = &args[i];
            let value: i64 = raw.parse().with_context(|| format!("{} 必須是整數: {raw}", labels[i]))?;
            if !(0..=100).contains(&value) {
                bail!("{} 必須介於 0-100: {value}", labels[i]);
            }
            values[i] = value;
        }
        let state = self.panels.entry(name.to_string()).or_default();
        state.x = values[0];
        state.y = values[1];
        state.width = values[2];
        state.height = values[3];
        self.output.push(&format!(
            "panel rect 設定為 x={} y={} width={} height={}\n",
            values[0], values[1], values[2], values[3]
        ));
        Ok(())
    }

    /// panel 底下的 `show`：把這個 plugin 的 panel 設成顯示，GUI 畫面下一次繪製時
    /// 就會依 `rect` 設定的位置/大小把它畫出來。同時把它移到疊放順序的最上層——
    /// 跟一般視窗系統一樣，最近顯示的視窗蓋在其他視窗上面。
    fn panel_show(&mut self, name: &str) {
        self.panels.entry(name.to_string()).or_default().visible = true;
        self.raise_panel(name);
        self.output.push("panel 已顯示\n");
    }

    /// panel 底下的 `hidden`：把這個 plugin 的 panel 設成不顯示。
    fn panel_hidden(&mut self, name: &str) {
        self.panels.entry(name.to_string()).or_default().visible = false;
        self.output.push("panel 已隱藏\n");
    }

    /// panel 底下的 `activate`：不改變顯示與否，只是把這個 panel 移到疊放順序的
    /// 最上層。用在「已經開了好幾個 panel，想把某一個重新拉到最上面」的情境。
    fn panel_activate(&mut self, name: &str) {
        self.raise_panel(name);
        self.output.push("panel 已拉到最上層\n");
    }

    /// 把 `name` 從疊放順序清單移除再放到最尾端（最上層），`show`/`activate` 共用。
    fn raise_panel(&mut self, name: &str) {
        self.panel_order.retain(|n| n != name);
        self.panel_order.push(name.to_string());
    }

    /// 把 `name` 從疊放順序清單移除再放到最前端（最下層），`cycle_active_panel_reverse` 用。
    fn lower_panel(&mut self, name: &str) {
        self.panel_order.retain(|n| n != name);
        self.panel_order.insert(0, name.to_string());
    }

    /// GUI 裡按 Shift-Tab 呼叫，方向跟 `cycle_active_panel` 相反：把目前的
    /// active panel（疊放順序最上層）沉到最下層，讓原本次上層的 panel 變成新的
    /// active，等於 undo 一次 Tab 的效果。少於兩個可見 panel 時沒有意義，不做事。
    pub fn cycle_active_panel_reverse(&mut self) {
        let visible: Vec<&String> = self
            .panel_order
            .iter()
            .filter(|name| self.panels.get(*name).is_some_and(|state| state.visible))
            .collect();
        if visible.len() < 2 {
            return;
        }
        let most_recent = visible[visible.len() - 1].clone();
        self.lower_panel(&most_recent);
    }

    /// 目前的 active panel 名稱：疊放順序最上層的可見 panel，跟 GUI 畫雙線外框
    /// 那一個是同一個判斷依據（見 `visible_panels`）。沒有任何可見 panel 時是 `None`。
    fn active_panel_name(&self) -> Option<String> {
        self.visible_panels().into_iter().last().map(|(name, _)| name)
    }

    /// GUI 裡按 Alt-Up/Down/Left/Right 呼叫：把目前的 active panel 往該方向移動
    /// `dx`/`dy` 個百分點（負值表示往左/往上）。往右/往下最多只能到
    /// `PANEL_MAX_POSITION`，確保至少留 100-PANEL_MAX_POSITION% 還在畫面裡，
    /// 不會整個移出去看不見；往左/往上則跟 `rect` 一樣夾在 0。沒有 active panel
    /// （沒開任何 panel）時沒有意義，不做事。
    pub fn move_active_panel(&mut self, dx: i64, dy: i64) {
        let Some(name) = self.active_panel_name() else { return };
        let state = self.panels.entry(name).or_default();
        state.x = (state.x + dx).clamp(0, PANEL_MAX_POSITION);
        state.y = (state.y + dy).clamp(0, PANEL_MAX_POSITION);
    }

    /// GUI 裡按 Alt-W/A/S/D 呼叫：把目前的 active panel 的 width/height 各自
    /// 加減 `dw`/`dh` 個百分點（W 加大 height、S 減小 height、D 加大 width、A
    /// 減小 width）。不管加大還是減小，width/height 都至少留 `PANEL_MIN_SIZE`%、
    /// 最多到 100%。沒有 active panel（沒開任何 panel）時沒有意義，不做事。
    pub fn resize_active_panel(&mut self, dw: i64, dh: i64) {
        let Some(name) = self.active_panel_name() else { return };
        let state = self.panels.entry(name).or_default();
        state.width = (state.width + dw).clamp(PANEL_MIN_SIZE, 100);
        state.height = (state.height + dh).clamp(PANEL_MIN_SIZE, 100);
    }

    /// GUI 裡按 Alt-M 呼叫：把目前的 active panel 最大化（rect 設成 0 0 100 100），
    /// 已經最大化的話則還原回最大化之前的 rect，兩者交替切換。沒有 active panel
    /// （沒開任何 panel）時沒有意義，不做事。
    pub fn toggle_maximize_active_panel(&mut self) {
        let Some(name) = self.active_panel_name() else { return };
        if let Some(saved) = self.maximized.remove(&name) {
            self.panels.insert(name, saved);
        } else {
            let state = self.panels.entry(name.clone()).or_default();
            let saved = *state;
            state.x = 0;
            state.y = 0;
            state.width = 100;
            state.height = 100;
            self.maximized.insert(name, saved);
        }
    }

    /// GUI 裡按 Alt-X 呼叫：把目前的 active panel 關閉（設成不顯示），跟指令列
    /// `panel hidden` 效果一樣。順便清掉最大化紀錄，不然這個 panel 下次被
    /// `show` 重新打開時，會直接沿用舊的最大化前 rect，跟使用者這次關閉前
    /// 看到的大小對不上。沒有 active panel（沒開任何 panel）時沒有意義，不做事。
    pub fn close_active_panel(&mut self) {
        let Some(name) = self.active_panel_name() else { return };
        self.maximized.remove(&name);
        if let Some(state) = self.panels.get_mut(&name) {
            state.visible = false;
        }
    }

    /// 依序執行腳本檔每一行，任何一行出錯就整個中止。
    /// 執行前會先印出 `prompt + 這一行`，看起來就像有人在互動模式下打了這行指令。
    pub fn run_script(&mut self, path: &Path) -> Result<()> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("讀取腳本檔失敗: {}", path.display()))?;
        for (lineno, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
                continue;
            }
            self.output.push(&format!("{}{}\n", self.prompt(), trimmed));
            self.execute_line(line)
                .with_context(|| format!("腳本第 {} 行執行失敗: {line}", lineno + 1))?;
            if self.should_exit {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_logged_command_succeeds() {
        // `git --version` 不會動到任何檔案，純粹確認「指令執行成功」這條路徑。
        assert!(run_logged_command("git", &["--version"]).is_ok());
    }

    #[test]
    fn run_logged_command_reports_nonzero_exit() {
        assert!(run_logged_command("git", &["this-is-not-a-real-subcommand"]).is_err());
    }

    #[test]
    fn run_logged_command_reports_missing_program() {
        assert!(run_logged_command("cng5-definitely-not-a-real-binary", &[]).is_err());
    }
}
