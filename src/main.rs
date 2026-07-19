mod gui;
mod output;
mod plugin;
mod plugins;
mod shell;
mod web;

use std::env;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use rustyline::error::ReadlineError;
use rustyline::{
    Cmd, ConditionalEventHandler, DefaultEditor, Event, EventContext, EventHandler,
    ExternalPrinter, KeyEvent, RepeatCount,
};

use output::OutputBuffer;
use plugin::{ContextInner, Plugin};
use plugins::{DevicePlugin, MusicPlugin, NotepadPlugin, OutputPlugin, SystemPlugin, WeatherPlugin, WolPlugin};
use shell::{lock_shell, run_host_shell, PluginFactory, Shell, UiMode};

fn main() -> Result<()> {
    let ctx = Arc::new(Mutex::new(ContextInner::default()));
    let output = Arc::new(OutputBuffer::new());

    let factories: Vec<(&'static str, PluginFactory)> = vec![
        ("wol", Box::new(|ctx| Box::new(WolPlugin::new(ctx)) as Box<dyn Plugin>)),
        (
            "device",
            Box::new(|ctx| Box::new(DevicePlugin::new(ctx)) as Box<dyn Plugin>),
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
            "system",
            Box::new(|ctx| Box::new(SystemPlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "weather",
            Box::new(|ctx| Box::new(WeatherPlugin::new(ctx)) as Box<dyn Plugin>),
        ),
    ];
    let shell = Arc::new(Mutex::new(Shell::new(ctx, factories, output.clone())));
    // 跟目前終端機是 CLI 還是 GUI mode 無關，整個程式活著期間都在背景監聽
    // port 9759，跟終端機共用同一個 shell/output，讓瀏覽器上開的 panel 能
    // 即時反映終端機這邊做的變更（例如 `mode server`）。
    web::spawn(shell.clone(), output.clone());
    spawn_exit_watcher(shell.clone());
    let mut printed = 0usize;

    let script_path = env::args().nth(1).unwrap_or_else(|| "script.cli".to_string());
    let mut ui = UiMode::Cli;
    if Path::new(&script_path).exists() {
        lock_shell(&shell).run_script(Path::new(&script_path))?;
        flush_new_lines(&output, &mut printed);
        // script 裡如果下過 `mode gui`/`mode cli`，一開始就照那個模式啟動，
        // 而不是永遠先固定開 CLI。
        if let Some(requested) = lock_shell(&shell).take_requested_ui() {
            ui = requested;
        }
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
                drop(sh);
                flush_new_lines(output, printed);
                if shell_passthrough {
                    run_host_shell();
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
