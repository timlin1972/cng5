use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Terminal;

use crate::output::OutputBuffer;
use crate::shell::{lock_shell, run_host_shell, PanelState, Shell};

/// Alt-方向鍵每按一下，active panel 移動幾個百分點（見 `run_loop` 裡的按鍵處理）。
const PANEL_MOVE_STEP: i64 = 5;

/// 把 `PanelState` 的百分比（相對於 `area`）換算成實際的終端機座標，
/// 並確保右邊界/下邊界不會超出 `area`，避免 ratatui 畫到畫面外。
fn scaled_rect(area: Rect, state: &PanelState) -> Rect {
    let pct = |v: i64| v.clamp(0, 100) as u32;
    let x = area.x + (area.width as u32 * pct(state.x) / 100) as u16;
    let y = area.y + (area.height as u32 * pct(state.y) / 100) as u16;
    let max_width = (area.x + area.width).saturating_sub(x);
    let max_height = (area.y + area.height).saturating_sub(y);
    let width = ((area.width as u32 * pct(state.width) / 100) as u16).min(max_width);
    let height = ((area.height as u32 * pct(state.height) / 100) as u16).min(max_height);
    Rect { x, y, width, height }
}

/// `mode gui`：預設畫面只有下面那條輸入指令的地方（單行、含外框共 3 行）。
/// 想看目前為止的互動紀錄（原本固定顯示在上面的內容）要另外進 `output` 這個
/// plugin，用 `panel` -> `rect ...` -> `show` 把它開出來；跟其他 plugin 的 panel
/// 一樣依 `rect` 的百分比疊在畫面上，只是 `output` 的 panel 內容是即時的互動紀錄，
/// 其餘 plugin 目前還只是空殼（邊框 + 標題）。輸入框永遠最後畫、蓋在所有 panel
/// 之上，就算某個 panel 佔滿整個畫面也不會擋住打字。可以同時開好幾個 panel，
/// 疊放順序最上層的是 active panel（雙線外框），按 Tab 可以循環切換哪個是
/// active（等同對它執行 `activate`），Shift-Tab 則是反方向循環。按 Alt-Up/Down/Left/Right 可以移動目前
/// active panel 的位置（改 `rect` 的 x/y，超出 0-100 會被夾住）；按 Alt-W/A/S/D
/// 可以縮放目前 active panel 的大小（W 加大 height、S 減小 height、D 加大
/// width、A 減小 width，width/height 都至少留 10%）；按 Alt-M 可以把目前 active
/// panel 最大化，再按一次還原回原本的大小，如此交替切換。一直跑到使用者輸入 `exit` 或 `mode cli`
/// 為止，再把畫面交還給呼叫端。
pub fn run(shell: &Arc<Mutex<Shell>>, output: &Arc<OutputBuffer>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // 用 catch_unwind 包住整個迴圈：現在 `shell`/`output` 是跟背景 web server
    // （見 `web::spawn`）共用的，任何一邊的 panic 都可能透過 `lock_shell` 波及
    // 到這裡（見 `shell::lock_shell` 的說明）。不管 panic 是從哪裡來的，都要先
    // 把終端機還原（關掉 raw mode、離開 alternate screen）再往外報錯，不然使用者
    // 會卡在一個看不到自己打字、換行也不會回到最前面的畫面。
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_loop(&mut terminal, shell, output)
    }));

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    match result {
        Ok(inner) => inner,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "未知的 panic".to_string());
            anyhow::bail!("GUI 執行緒發生 panic，已還原終端機狀態: {msg}");
        }
    }
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    shell: &Arc<Mutex<Shell>>,
    output: &Arc<OutputBuffer>,
) -> Result<()> {
    let mut input = String::new();
    // 指令歷史紀錄本身放在 Shell 裡（`script.cli`、CLI、GUI 執行過的都在裡面，
    // 三邊共用），這裡只記「瀏覽到第幾筆」跟「開始瀏覽前原本在打的內容」。
    // history_index 是 Some(i) 表示目前正顯示 history[i]。
    let mut history_index: Option<usize> = None;
    let mut draft = String::new();

    loop {
        // 每一圈都先檢查一次：web 端（`/api/exec` 執行 `exit`）跟終端機共用同一個
        // `Shell`，觸發 `should_exit` 不會經過下面的按鍵事件，如果只在
        // `KeyCode::Enter` 分支裡檢查（原本的寫法），畫面會一直重畫、卡著不動，
        // 直到使用者自己在 GUI 裡按一下 Enter 才會離開。這裡提早在最上面檢查，
        // 讓 `event::poll` 逾時、沒有任何按鍵事件時也能在 200ms 內自然發現並
        // 正常返回，讓 `run()` 收尾（`disable_raw_mode`/離開 alternate screen）
        // 走一般流程。
        if lock_shell(shell).should_exit() {
            return Ok(());
        }
        let prompt = lock_shell(shell).prompt();
        let panels = lock_shell(shell).visible_panels();

        terminal.draw(|frame| {
            // 輸入框固定佔畫面最下面 3 行（含外框），panel 的 rect 百分比要以扣掉
            // 這塊之後的區域為準——不然 `rect 0 0 100 100` 會把 100% 算成整個終端機
            // 高度，蓋到輸入框那幾行去。
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(3)])
                .split(frame.area());
            let content_area = chunks[0];
            let input_area = chunks[1];

            // 各 plugin 用 `panel show` 打開的 panel 先畫，輸入框最後畫、蓋在最上層。
            // 清單最後一個（疊放順序最上層）是目前的 active panel，用雙線外框標示；
            // 按 Tab 可以循環切換哪個是 active（見下面 KeyCode::Tab）。
            for (i, (name, state)) in panels.iter().enumerate() {
                let rect = scaled_rect(content_area, state);
                if rect.width == 0 || rect.height == 0 {
                    continue;
                }
                let is_active = i == panels.len() - 1;
                // 每個 panel 只畫自己內容有寫到的格子，沒寫到的格子會留著同一畫面
                // 裡先前畫過的東西（例如蓋滿整個畫面的 output panel）。先清空這個
                // panel 的 rect，才不會被其他 panel 的殘留內容穿透。
                frame.render_widget(Clear, rect);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(if is_active { BorderType::Double } else { BorderType::Plain })
                    .title(name.as_str());
                if name == "output" {
                    let inner = block.inner(rect);
                    let lines = output.all();
                    let height = inner.height as usize;
                    let start = lines.len().saturating_sub(height);
                    let visible: Vec<Line> = lines[start..].iter().map(|l| Line::raw(l.as_str())).collect();
                    frame.render_widget(Paragraph::new(visible).block(block), rect);
                } else if let Some(text) = lock_shell(shell).plugin_panel_text(name) {
                    frame.render_widget(Paragraph::new(text).block(block), rect);
                } else {
                    frame.render_widget(block, rect);
                }
            }

            // 輸入 panel 內容只有一行，但外框上下各佔一行，所以整個 panel 高度是 3。
            frame.render_widget(Clear, input_area);
            let input_block = Block::default().borders(Borders::ALL).border_type(BorderType::Plain);
            let input_inner = input_block.inner(input_area);

            let input_line = format!("{prompt}{input}");
            frame.render_widget(Paragraph::new(input_line.as_str()).block(input_block), input_area);
            frame.set_cursor_position((
                input_inner.x + input_line.chars().count() as u16,
                input_inner.y,
            ));
        })?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Enter => {
                let line = std::mem::take(&mut input);
                history_index = None;
                draft.clear();
                let mut sh = lock_shell(shell);
                let trimmed = line.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('#') && !trimmed.starts_with('!') {
                    output.push(&format!("{}{}\n", sh.prompt(), trimmed));
                }
                if let Err(err) = sh.execute_line(&line) {
                    output.push(&format!("錯誤: {err:#}\n"));
                }
                // 用「執行完之後的 prompt」當分隔行，而不是空白行——這樣看起來
                // 就像真的終端機一樣，前一個指令的輸出結束、下一個 prompt 出現。
                output.push(&format!("{}\n", sh.prompt()));
                let done = sh.should_exit() || sh.has_pending_mode_switch();
                let shell_passthrough = sh.take_pending_shell_passthrough();
                drop(sh);
                if shell_passthrough {
                    // 把終端機借給真正的 host shell 用之前，先跟 `run()` 結尾的
                    // 收尾動作一樣把畫面還原成一般終端機模式，不然子行程會直接
                    // 把輸出畫到 alternate screen 上（使用者切回來之後看不到）。
                    disable_raw_mode()?;
                    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                    terminal.show_cursor()?;
                    run_host_shell();
                    enable_raw_mode()?;
                    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
                    // 子行程離開前直接寫過真正的畫面，ratatui 內部的 diff buffer
                    // 並不知道，需要重置才不會誤以為某些格子沒變而跳過重繪，留下
                    // 子行程殘留的畫面內容。不能用 `terminal.clear()`——ratatui
                    // 0.30 的 `clear()` 為了「清完畫面後把游標放回原位」會先查詢
                    // 目前游標位置（送出 DSR escape code 再等終端機回覆），但這裡
                    // 剛重新進入 alternate screen、根本不需要保留游標位置；用
                    // `Terminal::new` 直接重建一個乾淨的 buffer 狀態最單純，也不會
                    // 卡在等一個不需要的游標位置回應上（某些終端機/multiplexer
                    // 環境下這個查詢可能遲遲收不到回應，讓整個 GUI 卡死或報錯）。
                    *terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
                }
                if done {
                    return Ok(());
                }
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => {
                lock_shell(shell).move_active_panel(0, -PANEL_MOVE_STEP);
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => {
                lock_shell(shell).move_active_panel(0, PANEL_MOVE_STEP);
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => {
                lock_shell(shell).move_active_panel(-PANEL_MOVE_STEP, 0);
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => {
                lock_shell(shell).move_active_panel(PANEL_MOVE_STEP, 0);
            }
            KeyCode::Up => {
                let history = lock_shell(shell).history().to_vec();
                if !history.is_empty() {
                    let new_index = match history_index {
                        None => {
                            draft = input.clone();
                            history.len() - 1
                        }
                        Some(i) => i.saturating_sub(1),
                    };
                    history_index = Some(new_index);
                    input = history[new_index].clone();
                }
            }
            KeyCode::Down => {
                let history = lock_shell(shell).history().to_vec();
                match history_index {
                    None => {}
                    Some(i) if i + 1 < history.len() => {
                        history_index = Some(i + 1);
                        input = history[i + 1].clone();
                    }
                    Some(_) => {
                        history_index = None;
                        input = std::mem::take(&mut draft);
                    }
                }
            }
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::ALT) => {
                match c.to_ascii_lowercase() {
                    'w' => lock_shell(shell).resize_active_panel(0, PANEL_MOVE_STEP),
                    's' => lock_shell(shell).resize_active_panel(0, -PANEL_MOVE_STEP),
                    'd' => lock_shell(shell).resize_active_panel(PANEL_MOVE_STEP, 0),
                    'a' => lock_shell(shell).resize_active_panel(-PANEL_MOVE_STEP, 0),
                    'm' => lock_shell(shell).toggle_maximize_active_panel(),
                    _ => {}
                }
            }
            // `?`只有在「正要開始打一個新的字」（開頭或前一個字元是空白）時才當
            // 求助鍵，其餘情況（正在打一個字的中間，例如網址 `watch?v=...`）就是
            // 字面上的問號，直接插入——不然像 URL 這種本來就含有 `?` 的內容永遠
            // 打不出來，`?` 會被這個鍵直接吃掉。
            KeyCode::Char('?') if input.is_empty() || input.ends_with(char::is_whitespace) => {
                let sh = lock_shell(shell);
                output.push(&format!("{}{}?\n", sh.prompt(), input));
                let text = sh.context_help_text(&input);
                drop(sh);
                output.push(&text);
            }
            KeyCode::Tab => {
                lock_shell(shell).cycle_active_panel();
            }
            KeyCode::BackTab => {
                lock_shell(shell).cycle_active_panel_reverse();
            }
            KeyCode::Char(c) => input.push(c),
            _ => {}
        }
    }
}
