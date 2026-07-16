use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Terminal;

use crate::output::OutputBuffer;
use crate::shell::Shell;

/// `mode gui`：上面一個 panel 顯示到目前為止所有輸出，下面是輸入指令的地方，
/// 兩個 panel 都是終端機的全寬、都有單線外框；輸入 panel 內容維持一行（含外框共 3
/// 行），其餘空間都給輸出 panel。一直跑到使用者輸入 `exit` 或 `mode cli` 為止，
/// 再把畫面交還給呼叫端。
pub fn run(shell: &Arc<Mutex<Shell>>, output: &Arc<OutputBuffer>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, shell, output);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    shell: &Arc<Mutex<Shell>>,
    output: &Arc<OutputBuffer>,
) -> Result<()> {
    let mut input = String::new();

    loop {
        let prompt = shell.lock().unwrap().prompt();
        let lines = output.all();

        terminal.draw(|frame| {
            // 輸入 panel 內容還是只有一行，但外框上下各佔一行，所以整個 panel 高度是 3。
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(3)])
                .split(frame.area());

            let output_area = chunks[0];
            let input_area = chunks[1];

            let output_block = Block::default().borders(Borders::ALL).border_type(BorderType::Plain);
            let input_block = Block::default().borders(Borders::ALL).border_type(BorderType::Plain);

            let output_inner = output_block.inner(output_area);
            let input_inner = input_block.inner(input_area);

            let height = output_inner.height as usize;
            let start = lines.len().saturating_sub(height);
            let visible: Vec<Line> = lines[start..].iter().map(|l| Line::raw(l.as_str())).collect();
            frame.render_widget(Paragraph::new(visible).block(output_block), output_area);

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
                let mut sh = shell.lock().unwrap();
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
                drop(sh);
                if done {
                    return Ok(());
                }
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char('?') => {
                let sh = shell.lock().unwrap();
                output.push(&format!("{}{}?\n", sh.prompt(), input));
                let text = sh.context_help_text(&input);
                drop(sh);
                output.push(&text);
            }
            KeyCode::Char(c) => input.push(c),
            _ => {}
        }
    }
}
