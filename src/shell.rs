use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use anyhow::{bail, Context, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

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

/// `Mode::Remote` 底下的連線狀態。`remote_prompt` 是目前已知的遠端 prompt 字串
/// （例如 `"cng5> "`、`"cng5(wol)> "`），每次轉發一行、拿到遠端回應後更新，本機
/// 的 `Shell::prompt` 拿它組成 `"<id>::<remote_prompt>"`，感覺像真的在遠端那台
/// 機器前面打字。用 `Arc<Mutex<_>>` 而不是普通欄位，是因為實際轉發（`remote_exec`）
/// 透過 `curl` 打 HTTP、最壞情況要等到 `--max-time` 的上限，不能卡在
/// `execute_line` 裡——那是在持有共用的 `Shell` 鎖的情況下呼叫的，卡住的話 GUI
/// 重繪、web 的其他請求全部都會跟著卡住等鎖。轉發改成丟進 `sender` 這個佇列，
/// 背景的 `spawn_remote_worker` 執行緒依序（一次一個，不會並行送出多個請求打亂
/// 順序）真正呼叫 `remote_exec`，完成後直接更新這個 `Arc`，不需要重新拿
/// `Shell` 自己的鎖。
pub struct RemoteSession {
    pub id: String,
    pub ip: String,
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
///    會卡住 `Shell` 的鎖，現在搬進背景執行緒，`connect` 本身立刻回傳。
/// 2. 依序處理 `receiver` 收到的每一行（`execute_remote_line` 只是 `send`
///    進來，不等結果），呼叫 `remote_exec` 轉發、更新 `remote_prompt`／把
///    錯誤訊息 push 進 `output`。
/// 3. `sender`（`RemoteSession` 那一份）被丟掉（斷線、`Mode` 換掉）時，
///    `receiver` 的疊代會自然結束，這個執行緒跟著結束，不需要額外的收尾訊號。
fn spawn_remote_worker(
    ip: String,
    receiver: mpsc::Receiver<String>,
    remote_prompt: Arc<Mutex<String>>,
    output: Arc<OutputBuffer>,
) {
    thread::spawn(move || {
        if let Some(prompt) = fetch_remote_prompt(&ip) {
            *remote_prompt.lock().unwrap() = prompt;
        }
        for line in receiver {
            match remote_exec(&ip, &line) {
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
/// client crate。
fn remote_exec(ip: &str, line: &str) -> Result<(String, Option<String>)> {
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
  history               列出之前執行過的指令
  !3                    重新執行 history 裡第 3 筆指令

進到某個 plugin 之後：
  help                  這個 plugin 底下的指令清單（一行式簽名）
  manual                這個 plugin 更完整的說明與範例
  panel                 （只有 GUI 畫面才有）進去設定這個 plugin 的 panel
                        位置/大小/顯示與否
  ~                     不管在哪一層都直接跳回 root
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
            (Mode::Root, "exit" | "quit") => self.should_exit = true,
            // 已經在 root，`~`/`..`/`...` 都沒事做，但仍然是合法指令（跟其他
            // mode 底下的行為一致，不管在哪一層打都不會被當成「不認得的指令」）。
            (Mode::Root, "~" | ".." | "...") => {}
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
                let target = args.first().context("connect 需要接目標機器的 id")?;
                let ip = self
                    .ctx
                    .lock()
                    .unwrap()
                    .devices
                    .get(target)
                    .map(|entry| entry.report.ip.clone())
                    .with_context(|| format!("沒有這個裝置: {target}（用 device list 查詢目前看得到的機器）"))?;
                // 先假設遠端在 root，真正的初始 prompt 由 `spawn_remote_worker`
                // 背景查詢、查到後更新——不在這裡同步等待（見 `RemoteSession`
                // 的說明），`connect` 才能立刻回傳、不卡住共用的 `Shell` 鎖。
                let remote_prompt = Arc::new(Mutex::new("cng5> ".to_string()));
                let (sender, receiver) = mpsc::channel();
                spawn_remote_worker(ip.clone(), receiver, remote_prompt.clone(), self.output.clone());
                self.ctx.lock().unwrap().remote_target = Some((target.clone(), ip.clone()));
                self.output.push(&format!("已連線到 {target} ({ip})\n"));
                self.mode = Mode::Remote(RemoteSession { id: target.clone(), ip, remote_prompt, sender });
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
            self.ctx.lock().unwrap().remote_target = None;
            self.mode = Mode::InPlugin("remote".to_string());
            self.output.push("已離線，回到 remote plugin\n");
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
            session.id, session.ip
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
                "exit",
                "quit",
                "~",
                "..",
                "...",
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
        s.push_str("  exit                 離開程式\n");
        s.push_str("  quit                 跟 exit 一樣，離開程式\n");
        s.push_str("  ~                    跳回 root（不管在哪一層都直接回來）\n");
        s.push_str("  ..                   往上一層（已經在 root，沒事做）\n");
        s.push_str("  ...                  往上兩層（已經在 root，沒事做）\n");
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
