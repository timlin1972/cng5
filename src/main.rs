mod activity;
mod crypto;
mod gui;
mod output;
mod plugin;
mod plugins;
mod shell;
mod sysinfo;
mod web;

use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use rustyline::error::ReadlineError;
use rustyline::{
    Cmd, ConditionalEventHandler, DefaultEditor, Event, EventContext, EventHandler,
    ExternalPrinter, KeyEvent, RepeatCount,
};

use output::OutputBuffer;
use plugin::{ContextInner, Plugin};
use plugins::{
    ActivitiesPlugin, DevicePlugin, FilesPlugin, GitRepoPlugin, GlobalPlugin, MusicPlugin, NotepadPlugin,
    OutputPlugin, QrPlugin, RemoteOutputPlugin, RemotePlugin, SystemPlugin, WeatherPlugin, WolPlugin,
};
use shell::{lock_shell, run_host_shell, run_remote_shell, run_upgrade, PluginFactory, Shell, UiMode};

fn main() -> Result<()> {
    // `upgrade` 指令編譯成功之後，重新呼叫自己一份帶這個旗標——這個小助手不
    // 進入平常的 Shell/plugin/web server 啟動流程，只單純負責「等舊行程真的
    // 結束、放開 port 9759，才啟動新編出來的執行檔」，見 `respawn_after` 的
    // 說明。要搶在下面 `script_path` 那段（把 argv[1] 當成 script 路徑）之前
    // 攔截，不然會被誤判成一個不存在的 script 檔案名稱直接忽略、繼續走正常
    // 啟動流程。
    if let Some(pid) = env::args()
        .nth(1)
        .and_then(|arg| arg.strip_prefix("--respawn-after=").and_then(|s| s.parse::<u32>().ok()))
    {
        return respawn_after(pid);
    }

    let ctx = Arc::new(Mutex::new(ContextInner::default()));
    let output = Arc::new(OutputBuffer::new());

    let factories: Vec<(&'static str, PluginFactory)> = vec![
        ("wol", Box::new(|ctx| Box::new(WolPlugin::new(ctx)) as Box<dyn Plugin>)),
        (
            "activities",
            Box::new(|ctx| Box::new(ActivitiesPlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "device",
            Box::new(|ctx| Box::new(DevicePlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "files",
            Box::new(|ctx| Box::new(FilesPlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "gitrepo",
            Box::new(|ctx| Box::new(GitRepoPlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "global",
            Box::new(|ctx| Box::new(GlobalPlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "music",
            Box::new(|ctx| Box::new(MusicPlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "notepad",
            Box::new(|ctx| Box::new(NotepadPlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "output",
            Box::new(|ctx| Box::new(OutputPlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "remote",
            Box::new(|ctx| Box::new(RemotePlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "remote-output",
            Box::new(|ctx| Box::new(RemoteOutputPlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "system",
            Box::new(|ctx| Box::new(SystemPlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "weather",
            Box::new(|ctx| Box::new(WeatherPlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        ("qr", Box::new(|ctx| Box::new(QrPlugin::new(ctx)) as Box<dyn Plugin>)),
    ];
    let shell = Arc::new(Mutex::new(Shell::new(ctx.clone(), factories, output.clone())));
    // 跟目前終端機是 CLI 還是 GUI mode 無關，整個程式活著期間都在背景監聽
    // port 9759，跟終端機共用同一個 shell/output，讓瀏覽器上開的 panel 能
    // 即時反映終端機這邊做的變更（例如 `mode server`）；`ctx` 另外傳一份是
    // 因為 device registry 的讀寫（`/api/device/register`、`/api/device/list`）
    // 直接操作共用的 `ContextInner`，不透過 `Shell`，避免這種高頻率的網路
    // 端點跟 CLI/GUI 操作搶同一把 `Shell` 鎖。
    web::spawn(shell.clone(), output.clone(), ctx);
    spawn_exit_watcher(shell.clone());
    let mut printed = 0usize;

    let script_path = env::args().nth(1).unwrap_or_else(|| "script.cli".to_string());
    let mut ui = UiMode::Cli;
    if Path::new(&script_path).exists() {
        lock_shell(&shell).run_script(Path::new(&script_path))?;
        flush_new_lines(&output, &mut printed);
    }
    // `script.cli` 之外，額外讀一份不進版控的 `script-local.cli`（例如每台機器
    // 各自不同的 server 位址、panel 位置），純粹是本機覆蓋用，檔案不存在就跳過。
    let local_script_path = "script-local.cli";
    if Path::new(local_script_path).exists() {
        lock_shell(&shell).run_script(Path::new(local_script_path))?;
        flush_new_lines(&output, &mut printed);
    }
    // 兩份 script 裡如果下過 `mode gui`/`mode cli`，一開始就照那個模式啟動，
    // 而不是永遠先固定開 CLI。
    if let Some(requested) = lock_shell(&shell).take_requested_ui() {
        ui = requested;
    }

    // 不是目前終端機的 foreground process group（`upgrade` 重新呼叫自己那個
    // 新 process 就是典型例子：原本的 shell 在中間那段 process 結束的瞬間就把
    // 終端機控制權收回去了），CLI/GUI 需要的 raw mode 這類操作會直接撞上
    // `Input/output error`——這種情況下不啟動 CLI/GUI 迴圈，改成 headless
    // 模式（見 `run_headless`），至少讓 web server／背景回報（`system`/
    // `global` 的背景執行緒，已經在上面 `web::spawn` 那一步一起啟動了）繼續
    // 正常運作，不要讓整個 process 跟著 CLI/GUI 一起死掉。
    if !sysinfo::is_foreground_tty() {
        output.push("目前不是終端機的 foreground process（例如透過 upgrade 重新啟動），改成 headless 模式：不會有 CLI/GUI 畫面，但 web server／背景回報照常運作，可以用 web UI 或 remote 操作\n");
        flush_new_lines(&output, &mut printed);
        run_headless(&shell);
        return Ok(());
    }

    loop {
        if lock_shell(&shell).should_exit() {
            break;
        }
        lock_shell(&shell).set_current_ui(ui);
        match ui {
            UiMode::Cli => run_cli_ui(&shell, &output, &mut printed)?,
            UiMode::Gui => gui::run(&shell, &output)?,
        }
        if lock_shell(&shell).should_exit() {
            break;
        }
        match lock_shell(&shell).take_requested_ui() {
            Some(next) => ui = next,
            None => break,
        }
    }
    Ok(())
}

/// `--respawn-after=<pid>` 模式：等 `old_pid`（呼叫 `upgrade` 的那個舊行程）
/// 真的結束、放開 port 9759 之後，才啟動新編出來的執行檔（跟自己是同一個
/// 路徑，`cargo build` 已經覆蓋成新版本）。一個行程沒辦法在自己完全結束
/// 之後還繼續做事，所以需要這個獨立重新呼叫的小助手，不能讓舊行程自己做到
/// 底。到這裡之前 git/cargo 那些步驟已經在舊行程裡確認成功過了，這裡完全
/// 不重做，只單純負責「等待 -> 啟動新的」；等超過 30 秒還沒等到（理論上不
/// 會發生）就放棄，避免真的出狀況時卡死在這裡——這裡沒有終端機/`OutputBuffer`
/// 可以回報，只能寫 stderr。
/// `respawn_after` 是獨立的另一個 process，它的 stdout/stderr 不會出現在
/// `OutputBuffer` 裡（那是同一個 process 內共用的狀態）——如果是透過 `remote`
/// 連線觸發 `upgrade`，使用者能看到的只有原本那個行程結束前 push 進
/// `OutputBuffer` 的內容，這個小助手自己之後成功/失敗完全沒有地方看得到。
/// 寫一份簡單的 log 檔案，至少事後能上去查發生了什麼事。時間戳用 unix 秒數
/// （不是好看的日期時間），單純為了不想為了格式化日期另外引入一個 crate。
const RESPAWN_LOG: &str = "upgrade-respawn.log";

fn log_respawn(line: &str) {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(RESPAWN_LOG) {
        let _ = writeln!(file, "[{ts}] {line}");
    }
}

fn respawn_after(old_pid: u32) -> Result<()> {
    log_respawn(&format!("啟動，等待舊行程（pid {old_pid}）結束"));
    let deadline = Instant::now() + Duration::from_secs(30);
    while sysinfo::pid_alive(old_pid) {
        if Instant::now() >= deadline {
            log_respawn(&format!("等待舊行程（pid {old_pid}）結束逾時，放棄重啟"));
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
    log_respawn(&format!("舊行程（pid {old_pid}）已結束"));
    // 舊行程剛消失那一瞬間，它繼承的終端機/pty（例如透過 `cargo run` 啟動時）
    // 可能還在收尾，緊接著就去動同一個終端機有機會撞上暫時的狀態、拿到
    // `Input/output error` 這種跟「新執行檔本身」完全無關的錯誤。多等一點
    // 緩衝時間再動手，避免搶在那個空檔。
    thread::sleep(Duration::from_millis(500));
    let exe = match env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            log_respawn(&format!("找不到目前執行檔的路徑: {err:#}"));
            return Err(err).context("respawn-after: 找不到目前執行檔的路徑");
        }
    };
    log_respawn(&format!("準備啟動: {}", exe.display()));
    match std::process::Command::new(&exe).spawn() {
        Ok(child) => {
            log_respawn(&format!("已啟動新執行檔（pid {}）", child.id()));
            Ok(())
        }
        Err(err) => {
            log_respawn(&format!("啟動 {} 失敗: {err:#}", exe.display()));
            Err(err).with_context(|| format!("respawn-after: 啟動 {} 失敗", exe.display()))
        }
    }
}

/// 沒有終端機控制權時的執行模式（見 `sysinfo::is_foreground_tty`）：不啟動
/// CLI/GUI 迴圈——那些都需要真正能拿到控制權的終端機，硬要跑會撞上
/// `enable_raw_mode` 之類操作回傳 `Input/output error`。`web::spawn` 開的
/// web server、`system`/`global` plugin 自己的背景回報執行緒都已經在這之前
/// 啟動、不需要 CLI/GUI 迴圈才能運作，這裡只需要讓這個 process 保持活著、
/// 定期檢查 `should_exit`（例如透過 web 的 `/api/exec` 打 `exit`，或
/// `remote` 連線轉發過來的 `exit`）就好——不需要 `spawn_exit_watcher` 那套
/// 硬中斷機制，這裡的迴圈本來就沒有阻塞在任何需要被打斷的呼叫上。
fn run_headless(shell: &Arc<Mutex<Shell>>) {
    loop {
        if lock_shell(shell).should_exit() {
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// 從 web（`/api/exec` 執行 `exit`）觸發的離開只會設定 `Shell` 的 `should_exit`
/// 旗標，不會去動終端機這邊的 CLI 迴圈——但 CLI 迴圈卡在 `rl.readline()` 的阻塞
/// 讀取上，只有使用者在終端機自己再按一下 Enter（讓 `readline` 返回）才有機會
/// 檢查到這個旗標並離開。額外開一個背景執行緒定期輪詢，一偵測到「該離開了、
/// 而且現在卡著的是 CLI」就直接用 `std::process::exit` 硬中斷整個行程。
/// 限定只在 `current_ui` 是 `Cli` 時才這樣做：GUI mode 自己每 200ms 會輪詢一次
/// `should_exit`（見 `gui::run_loop`），可以在 `disable_raw_mode`/離開 alternate
/// screen 正常收尾後才自然離開，這裡搶著在收尾前用 `process::exit` 中斷會讓
/// 終端機卡在 raw mode 出不來。
fn spawn_exit_watcher(shell: Arc<Mutex<Shell>>) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(200));
        let sh = lock_shell(&shell);
        let stuck_in_cli = sh.should_exit() && sh.current_ui() == UiMode::Cli;
        drop(sh);
        if stuck_in_cli {
            std::process::exit(0);
        }
    });
}

/// 把 `output` 裡從 `printed` 開始、還沒印到 stdout 的行印出來，並更新游標。
fn flush_new_lines(output: &OutputBuffer, printed: &mut usize) {
    for line in output.lines_from(*printed) {
        println!("{line}");
    }
    *printed = output.len();
}

/// 按下 `?` 立刻顯示當前 mode 的 help，不需要按 Enter。只有在「正要開始打一個
/// 新的字」（游標前面是空字串或以空白結尾）時才這樣做；如果游標前面那個字還沒
/// 打完（例如網址 `watch?v=...` 打到一半），就回傳 `None` 讓 rustyline 照預設
/// 行為把 `?` 當一般字元插進去——不然像 URL 這種本來就含有 `?` 的內容會被這個
/// 按鍵整個吃掉，永遠打不出來。
/// 用 rustyline 的 external printer 輸出，這樣顯示完之後 prompt 會自動重新出現，
/// 不用像直接 `println!` 那樣得等使用者按下一次 Enter 才會重繪。
struct HelpKeyHandler<P> {
    shell: Arc<Mutex<Shell>>,
    printer: Mutex<P>,
}

impl<P: ExternalPrinter + Send> ConditionalEventHandler for HelpKeyHandler<P> {
    fn handle(&self, _evt: &Event, _n: RepeatCount, _positive: bool, ctx: &EventContext) -> Option<Cmd> {
        let before_cursor = &ctx.line()[..ctx.pos()];
        if !before_cursor.is_empty() && !before_cursor.ends_with(char::is_whitespace) {
            return None;
        }
        let text = lock_shell(&self.shell).context_help_text(before_cursor);
        let _ = self.printer.lock().unwrap().print(text);
        Some(Cmd::Noop)
    }
}

/// `mode cli`：一般的 rustyline 互動模式，單行出錯只印錯誤、不中止整個 session。
/// 遇到 `exit` 或 `mode gui` 就回傳，讓 `main` 決定接下來要結束程式還是切去 GUI。
fn run_cli_ui(shell: &Arc<Mutex<Shell>>, output: &Arc<OutputBuffer>, printed: &mut usize) -> Result<()> {
    let mut rl = DefaultEditor::new()?;
    // 非 tty（例如把指令用 pipe 導進 stdin）拿不到 external printer，
    // 這種情境下 `?` 這個按鍵綁定本來就用不到，略過即可，不影響其餘功能。
    if let Ok(printer) = rl.create_external_printer() {
        rl.bind_sequence(
            KeyEvent::from('?'),
            EventHandler::Conditional(Box::new(HelpKeyHandler {
                shell: shell.clone(),
                printer: Mutex::new(printer),
            })),
        );
    }

    // 從 GUI 切回來時，GUI 期間累積的內容（包含切換指令本身的 echo）都還沒印到
    // 這個新的 CLI session 上，先補印出來，不用等使用者按下一行才觸發。
    flush_new_lines(output, printed);

    // 把之前執行過的指令（包含 script.cli 跑過的、以及先前 GUI session 打的）
    // 灌進 rustyline 自己的 history，這樣一開始按上下鍵就找得到。
    for past in lock_shell(shell).history() {
        let _ = rl.add_history_entry(past.as_str());
    }

    loop {
        let prompt = lock_shell(shell).prompt();
        match rl.readline(&prompt) {
            Ok(line) => {
                let _ = rl.add_history_entry(line.as_str());
                let mut sh = lock_shell(shell);
                let trimmed = line.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('#') && !trimmed.starts_with('!') {
                    // 終端機自己的 echo 已經讓使用者看到這行了，這裡寫進 output
                    // 只是為了讓之後切去 GUI 時歷史記錄還在，所以順便把 printed
                    // 往前推過這一行，CLI 這邊才不會重複印一次。
                    output.push(&format!("{}{}\n", sh.prompt(), trimmed));
                    *printed = output.len();
                }
                if let Err(err) = sh.execute_line(&line) {
                    output.push(&format!("錯誤: {err:#}\n"));
                }
                let done = sh.should_exit() || sh.has_pending_mode_switch();
                let shell_passthrough = sh.take_pending_shell_passthrough();
                let remote_shell_ip = sh.take_pending_remote_shell();
                let upgrade_requested = sh.take_pending_upgrade();
                drop(sh);
                flush_new_lines(output, printed);
                if shell_passthrough {
                    run_host_shell();
                }
                if let Some(ip) = remote_shell_ip {
                    enable_raw_mode()?;
                    run_remote_shell(&ip, output);
                    disable_raw_mode()?;
                }
                if upgrade_requested {
                    run_upgrade(shell.clone(), output.clone());
                }
                if done {
                    break;
                }
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => break,
            Err(err) => {
                output.push(&format!("讀取輸入失敗: {err}\n"));
                flush_new_lines(output, printed);
                break;
            }
        }
    }
    Ok(())
}
