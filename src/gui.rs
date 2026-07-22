use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Terminal;
use unicode_width::UnicodeWidthStr;

use crate::output::OutputBuffer;
use crate::plugins::{NotepadPlugin, QrPlugin};
use crate::shell::{lock_shell, run_host_shell, run_remote_shell, run_upgrade, PanelState, Shell};

/// 借出 `notepad` plugin 的具體型別可變參考執行 `f`：`Shell::plugin_mut` 只給
/// `&mut Box<dyn Plugin>`，這裡用 `Plugin::as_any_mut` 向下轉型成
/// `&mut NotepadPlugin`，讓 GUI 能直接呼叫逐字元編輯用的方法（見
/// `plugins::notepad`），不用透過 `execute_line` 把每個按鍵都包成一行指令字串
/// 送進 `shell_words` 解析（那樣任意字元——含空白、引號——會很難處理）。
/// plugin 不存在或型別對不上（理論上不會發生，`main.rs` 一定有註冊 notepad）
/// 就回傳 `None`，呼叫端安全地當作「沒有 notepad 可用」處理。
fn with_notepad<R>(shell: &Arc<Mutex<Shell>>, f: impl FnOnce(&mut NotepadPlugin) -> R) -> Option<R> {
    let mut sh = lock_shell(shell);
    let plugin = sh.plugin_mut("notepad")?;
    let notepad = plugin.as_any_mut().downcast_mut::<NotepadPlugin>()?;
    Some(f(notepad))
}

/// 跟 `with_notepad`同一個套路，借出 `qr` plugin 的具體型別可變參考，讓 GUI
/// 能呼叫 PgUp/PgDn 用的 `cycle_next`/`cycle_prev`。
fn with_qr<R>(shell: &Arc<Mutex<Shell>>, f: impl FnOnce(&mut QrPlugin) -> R) -> Option<R> {
    let mut sh = lock_shell(shell);
    let plugin = sh.plugin_mut("qr")?;
    let qr = plugin.as_any_mut().downcast_mut::<QrPlugin>()?;
    Some(f(qr))
}

/// `line` 是不是標題（`#`..`######` 開頭接空白），是的話回傳 `#` 的個數
/// （1-6）。`render_notepad_lines` 用這個決定要不要在標題底下多畫一條分隔
/// 線，`style_line` 用這個決定標題本身的樣式，兩邊共用同一份判斷才不會兜
/// 不起來。
fn heading_level(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
        Some(hashes)
    } else {
        None
    }
}

/// `trimmed` 是不是一個 GFM 風格的 checklist 項目（`- [ ] 內容`／`- [x] 內容`，
/// 項目符號 `-`/`*`/`+` 皆可），是的話回傳 (項目符號含後面空白, 是否已勾選,
/// 內容)。
fn parse_checkbox(trimmed: &str) -> Option<(&str, bool, &str)> {
    if !(trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ")) {
        return None;
    }
    let bullet = &trimmed[..2];
    let rest = &trimmed[2..];
    if let Some(content) = rest.strip_prefix("[ ] ") {
        Some((bullet, false, content))
    } else if let Some(content) = rest.strip_prefix("[x] ").or_else(|| rest.strip_prefix("[X] ")) {
        Some((bullet, true, content))
    } else {
        None
    }
}

/// 比對開頭是 `[` 的一段文字是不是 `[連結文字](網址)`：回傳文字/網址在
/// `rest`（也就是從這個 `[` 開始算）裡的 byte 範圍，以及整段（含頭尾符號）
/// 佔用的長度，讓呼叫端知道要往前跳過幾個 byte。
fn parse_markdown_link(rest: &str) -> Option<(usize, usize, usize, usize, usize)> {
    debug_assert!(rest.starts_with('['));
    let bracket_end = rest.find(']')?;
    let after_bracket = &rest[bracket_end + 1..];
    after_bracket.strip_prefix('(')?;
    let paren_rel_end = after_bracket[1..].find(')')?;
    let text_start = 1;
    let text_end = bracket_end;
    let url_start = bracket_end + 2; // 跳過 "]("
    let url_end = url_start + paren_rel_end;
    let total_len = url_end + 1; // 含結尾的 ")"
    Some((text_start, text_end, url_start, url_end, total_len))
}

/// 掃一行文字，把 `` **粗體** ``、`*斜體*`、`` `code` ``、`[文字](網址)` 這幾種
/// 行內語法切成對應樣式的 `Span`；純粹掃描字元找配對的結尾符號，不是完整的
/// CommonMark 剖析器（不處理巢狀、跳脫字元等）。`hide_markers` 是「這一行要
/// 不要把符號本身藏起來，只留樣式化後的內容」——不是游標所在那一行時藏起來
/// （比較接近真正的所見即所得：**粗體** 直接顯示成粗體字，看不到 `**`；連結
/// 只顯示 `[文字]` 裡的文字本身，網址跟中括號都藏起來）；游標所在那一行則
/// 保留原始符號（用比較淡的顏色），這樣才能逐字元精準編輯，包括「粗體字後面
/// 按 backspace 就少一個符號」這種直覺行為——游標那一行本來就是照原始字元
/// 1:1 顯示，backspace 刪的也是原始字元，畫面自然跟著變，不需要另外寫特殊
/// 邏輯去處理「該不該恢復成有符號的樣子」。
fn style_inline(line: &str, hide_markers: bool) -> Vec<Span<'_>> {
    let marker_style = Style::default().fg(Color::DarkGray);
    let link_style = Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let mut spans = Vec::new();
    let mut plain_start = 0usize;
    let mut i = 0usize;
    while i < line.len() {
        let rest = &line[i..];

        // 圖片語法 `![替代文字](網址)`——跟連結一樣只留替代文字、藏起網址。
        // 有些論壇（例如 Discourse）的替代文字裡會夾帶 `|690x460, 50%` 這種
        // 縮放/尺寸提示，這裡只取 `|` 前面那一段當作真正要顯示的文字。
        if let Some(after_bang) = rest.strip_prefix('!') {
            if after_bang.starts_with('[') {
                if let Some((text_start_rel, text_end_rel, url_start_rel, url_end_rel, total_len_rel)) =
                    parse_markdown_link(after_bang)
                {
                    let text_start = 1 + text_start_rel;
                    let text_end = 1 + text_end_rel;
                    let url_start = 1 + url_start_rel;
                    let url_end = 1 + url_end_rel;
                    let total_len = 1 + total_len_rel;

                    let alt = &line[i + text_start..i + text_end];
                    let display_len = alt[..alt.find('|').unwrap_or(alt.len())].trim_end().len();
                    let display_end = i + text_start + display_len;

                    if plain_start < i {
                        spans.push(Span::raw(&line[plain_start..i]));
                    }
                    if !hide_markers {
                        spans.push(Span::styled(&line[i..i + text_start], marker_style)); // "!["
                    }
                    spans.push(Span::styled(&line[i + text_start..display_end], link_style));
                    if !hide_markers {
                        spans.push(Span::styled(&line[display_end..i + text_end], marker_style)); // "|尺寸提示"
                        spans.push(Span::styled(&line[i + text_end..i + url_start], marker_style)); // "]("
                        spans.push(Span::styled(&line[i + url_start..i + url_end], marker_style)); // 網址
                        spans.push(Span::styled(&line[i + url_end..i + total_len], marker_style)); // ")"
                    }
                    i += total_len;
                    plain_start = i;
                    continue;
                }
            }
        }

        if rest.starts_with('[') {
            if let Some((text_start, text_end, url_start, url_end, total_len)) = parse_markdown_link(rest) {
                if plain_start < i {
                    spans.push(Span::raw(&line[plain_start..i]));
                }
                // 依序畫出 `[`、文字、`](`、網址、`)`——`hide_markers` 時只留
                // 中間的文字這一段，其餘符號（含網址）整段跳過不顯示。
                if !hide_markers {
                    spans.push(Span::styled(&line[i..i + text_start], marker_style));
                }
                spans.push(Span::styled(&line[i + text_start..i + text_end], link_style));
                if !hide_markers {
                    spans.push(Span::styled(&line[i + text_end..i + url_start], marker_style));
                    spans.push(Span::styled(&line[i + url_start..i + url_end], marker_style));
                    spans.push(Span::styled(&line[i + url_end..i + total_len], marker_style));
                }
                i += total_len;
                plain_start = i;
                continue;
            }
        }

        // `***` 要在 `**` 之前比對，不然 `***文字***` 會被 `**` 先搶走、多出
        // 來的一個 `*` 變成殘留的一般文字。
        let matched = if let Some(after) = rest.strip_prefix("***") {
            after.find("***").map(|rel_end| {
                (3, i + 3 + rel_end, 3, Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC))
            })
        } else if let Some(after) = rest.strip_prefix("**") {
            after.find("**").map(|rel_end| (2, i + 2 + rel_end, 2, Style::default().add_modifier(Modifier::BOLD)))
        } else if let Some(after) = rest.strip_prefix("~~") {
            after.find("~~").map(|rel_end| (2, i + 2 + rel_end, 2, Style::default().add_modifier(Modifier::CROSSED_OUT)))
        } else if let Some(after) = rest.strip_prefix("++") {
            after.find("++").map(|rel_end| (2, i + 2 + rel_end, 2, Style::default().add_modifier(Modifier::UNDERLINED)))
        } else if let Some(after) = rest.strip_prefix("==") {
            after.find("==").map(|rel_end| (2, i + 2 + rel_end, 2, Style::default().add_modifier(Modifier::REVERSED)))
        } else if let Some(after) = rest.strip_prefix('`') {
            after
                .find('`')
                .map(|rel_end| (1, i + 1 + rel_end, 1, Style::default().fg(Color::Rgb(139, 0, 0)).bg(Color::Gray)))
        } else if let Some(after) = rest.strip_prefix('*') {
            after.find('*').map(|rel_end| (1, i + 1 + rel_end, 1, Style::default().add_modifier(Modifier::ITALIC)))
        } else {
            None
        };

        if let Some((open_len, inner_end, close_len, style)) = matched {
            let inner_start = i + open_len;
            let marker_end = inner_end + close_len;
            if plain_start < i {
                spans.push(Span::raw(&line[plain_start..i]));
            }
            if !hide_markers {
                spans.push(Span::styled(&line[i..inner_start], marker_style));
            }
            spans.push(Span::styled(&line[inner_start..inner_end], style));
            if !hide_markers {
                spans.push(Span::styled(&line[inner_end..marker_end], marker_style));
            }
            i = marker_end;
            plain_start = i;
            continue;
        }

        // 沒有配對到任何行內語法：往前跳過目前這個字元，用 char 的 byte 長度
        // 而不是固定 +1——中文字在 UTF-8 裡是多個 byte，直接 +1 會切在字元
        // 中間導致字串切片 panic。
        let ch_len = rest.chars().next().map(char::len_utf8).unwrap_or(1);
        i += ch_len;
    }
    if plain_start < line.len() {
        spans.push(Span::raw(&line[plain_start..]));
    }
    if spans.is_empty() {
        spans.push(Span::raw(""));
    }
    spans
}

/// 單一行的樣式：標題（`#`.. `######` 開頭接空白）整行粗體＋顏色、引言
/// （`> ` 開頭）整行斜體＋灰色、分隔線（整行只有 `---` 或 `***`）畫成撐滿
/// 寬度的一條橫線，其餘交給 `style_inline` 處理行內語法；`hide_markers`
/// 意義跟 `style_inline` 一樣，標題/引言的前導符號（`#`/`> `）、分隔線的
/// 原始符號也一併套用同一個規則。`width` 是分隔線要撐滿的寬度（跟
/// `render_notepad_lines` 給 code block 外框用的是同一個寬度）。code fence
/// （` ``` ` 開頭）的整段內容需要跨行的「目前在不在區塊裡」狀態，由
/// `render_notepad_lines` 處理，不在這裡。
fn style_line(line: &str, hide_markers: bool, width: usize) -> Line<'_> {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    if let Some(hashes) = heading_level(line) {
        let heading_style = Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan);
        let content = &trimmed[hashes + 1..];
        if hide_markers {
            return Line::from(Span::styled(content, heading_style));
        }
        let marker_style = Style::default().fg(Color::DarkGray);
        return Line::from(vec![
            Span::raw(&line[..indent]),
            Span::styled(&trimmed[..hashes + 1], marker_style),
            Span::styled(content, heading_style),
        ]);
    }
    if let Some(content) = trimmed.strip_prefix("> ") {
        let quote_style = Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC);
        if hide_markers {
            return Line::from(Span::styled(content, quote_style));
        }
        return Line::from(vec![
            Span::raw(&line[..indent]),
            Span::styled("> ", Style::default().fg(Color::DarkGray)),
            Span::styled(content, quote_style),
        ]);
    }
    if let Some((bullet, checked, content)) = parse_checkbox(trimmed) {
        if hide_markers {
            // 特意維持 `[ ]`/`[x]` 這種純 ASCII 文字（保證單一寬度），不用
            // ☐/☑ 這類 Unicode 符號——那類符號在不少終端機字型裡實際畫出來
            // 比一格寬，會佔到後面那個空白的位置，看起來就像圖示跟文字之間
            // 沒有空格。
            let (marker, marker_style, content_style) = if checked {
                (
                    "[x]",
                    Style::default().fg(Color::Green),
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::CROSSED_OUT),
                )
            } else {
                ("[ ]", Style::default(), Style::default())
            };
            return Line::from(vec![
                Span::raw(&line[..indent]),
                Span::raw(bullet),
                Span::styled(marker, marker_style),
                Span::raw(" "),
                Span::styled(content, content_style),
            ]);
        }
        // 游標在這一行：保留原始的 `[ ]`/`[x]` 文字，才能把空白改成 x 之類的
        // 逐字元編輯，不然游標一直停在被藏起來的符號上，打字會很奇怪。
        return Line::from(Span::raw(line));
    }
    if line.trim() == "---" || line.trim() == "***" {
        if hide_markers {
            return Line::from(Span::styled("─".repeat(width), Style::default().fg(Color::DarkGray)));
        }
        // 游標在這一行：保留原始的 `---`/`***` 文字，才能逐字元編輯（例如
        // 還沒打完、只打了 `--` 的時候，不該被誤判成分隔線）。
        return Line::from(Span::raw(line));
    }
    Line::from(style_inline(line, hide_markers))
}

/// 撐滿 `width` 欄的水平框線，兩端用 `left`/`right` 這兩個角落符號（`┌`/`┐`
/// 或 `└`/`┘`）。
fn horizontal_border(width: usize, left: &str, right: &str) -> String {
    let corners_width = UnicodeWidthStr::width(left) + UnicodeWidthStr::width(right);
    let dashes = width.saturating_sub(corners_width);
    format!("{left}{}{right}", "─".repeat(dashes))
}

/// notepad panel 最下面那一行：快速鍵提示靠左、目前的檔名靠右，中間至少留
/// 一格空白隔開。`width` 比兩邊加起來還窄（面板被縮得很小）時就不勉強對齊，
/// 讓文字自然疊在一起，總比整行位置算錯還好。
fn hint_line_with_filename(hint: &str, filename: &str, width: usize) -> String {
    let gap = width.saturating_sub(UnicodeWidthStr::width(hint) + UnicodeWidthStr::width(filename)).max(1);
    format!("{hint}{}{filename}", " ".repeat(gap))
}

/// 所有 panel 共通的操作快速鍵，畫在每個 panel 最下面一行（notepad 有自己
/// 專屬的提示，見 `hint_line_with_filename`，不用這個）。這些鍵不管哪個
/// panel 是 active 都能按，所以提示文字也不分 panel，統一顯示同一行。
const PANEL_HINT: &str = "Tab 切換面板　Alt-方向鍵 移動　Alt-WASD 縮放　Alt-M 最大化　Alt-X 關閉";

/// 幫一行 code block 內容產生要顯示的 `Span`：有語法上色規則（`spec`，見
/// `language_spec`）就用 `highlight_code_line` 依關鍵字/字串/註解/數字分類
/// 上色，沒有（沒標語言，或標了但不認得）就整行套用預設的單一顏色，維持
/// 原本的樣子。後面補空白讓總寬度撐滿外框內側寬度（`│ ` 跟 ` │` 各佔 2 欄，
/// 內容區是 `width - 4`）；內容本身比這個寬就不補，讓它自然超出框線，比硬要
/// 截斷看不到完整內容好。
fn code_content_spans<'a>(
    line: &'a str,
    width: usize,
    spec: Option<&LanguageSpec>,
    in_block_comment: &mut bool,
) -> Vec<Span<'a>> {
    let mut spans = match spec {
        Some(spec) => highlight_code_line(line, spec, in_block_comment),
        None => vec![Span::styled(line, Style::default().fg(Color::Green))],
    };
    let inner_width = width.saturating_sub(4);
    let pad = inner_width.saturating_sub(UnicodeWidthStr::width(line));
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans
}

/// 一種語言的語法上色規則：關鍵字清單、單行註解的開頭符號、區塊註解的
/// 頭尾符號。不是完整的詞法分析器（不處理跳脫字元、原始字串、巢狀區塊
/// 註解這些），但字串/註解/數字/關鍵字這幾類最常見、最有用的分類已經
/// 涵蓋，對「盡量還原」已經足夠。
struct LanguageSpec {
    keywords: &'static [&'static str],
    line_comment: Option<&'static str>,
    block_comment: Option<(&'static str, &'static str)>,
}

/// 依 code fence 開頭標注的語言名稱（例如 ```rust 的 `rust`）查對應的語法
/// 上色規則；沒標語言或標了但不認得的語言都回傳 `None`，呼叫端在這種情況
/// 下維持原本單一顏色的樣子，不強行套用錯誤的規則。
fn language_spec(lang: &str) -> Option<LanguageSpec> {
    match lang.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Some(LanguageSpec {
            keywords: &[
                "fn", "let", "mut", "pub", "struct", "enum", "impl", "trait", "use", "mod", "if", "else", "match",
                "for", "while", "loop", "return", "break", "continue", "const", "static", "self", "Self", "super",
                "crate", "as", "in", "move", "ref", "where", "async", "await", "dyn", "unsafe", "true", "false",
            ],
            line_comment: Some("//"),
            block_comment: Some(("/*", "*/")),
        }),
        "python" | "py" => Some(LanguageSpec {
            keywords: &[
                "def", "class", "if", "elif", "else", "for", "while", "return", "import", "from", "as", "with",
                "try", "except", "finally", "raise", "pass", "break", "continue", "lambda", "yield", "in", "is",
                "not", "and", "or", "None", "True", "False", "self", "global", "nonlocal", "async", "await",
            ],
            line_comment: Some("#"),
            block_comment: None,
        }),
        "javascript" | "js" | "typescript" | "ts" | "jsx" | "tsx" => Some(LanguageSpec {
            keywords: &[
                "function", "const", "let", "var", "if", "else", "for", "while", "return", "class", "extends",
                "new", "this", "import", "export", "from", "as", "try", "catch", "finally", "throw", "break",
                "continue", "switch", "case", "default", "typeof", "instanceof", "in", "of", "async", "await",
                "yield", "null", "undefined", "true", "false", "super",
            ],
            line_comment: Some("//"),
            block_comment: Some(("/*", "*/")),
        }),
        "c" | "cpp" | "c++" | "h" | "hpp" => Some(LanguageSpec {
            keywords: &[
                "int", "char", "float", "double", "void", "if", "else", "for", "while", "return", "struct",
                "typedef", "enum", "union", "static", "const", "unsigned", "signed", "long", "short", "sizeof",
                "switch", "case", "default", "break", "continue", "class", "public", "private", "protected",
                "namespace", "template", "new", "delete", "this", "virtual", "override", "nullptr", "true", "false",
            ],
            line_comment: Some("//"),
            block_comment: Some(("/*", "*/")),
        }),
        "java" => Some(LanguageSpec {
            keywords: &[
                "public", "private", "protected", "class", "interface", "extends", "implements", "static", "final",
                "void", "int", "long", "float", "double", "boolean", "char", "String", "if", "else", "for", "while",
                "return", "new", "this", "super", "try", "catch", "finally", "throw", "throws", "import", "package",
                "break", "continue", "switch", "case", "default", "true", "false", "null",
            ],
            line_comment: Some("//"),
            block_comment: Some(("/*", "*/")),
        }),
        "go" | "golang" => Some(LanguageSpec {
            keywords: &[
                "func", "package", "import", "var", "const", "type", "struct", "interface", "if", "else", "for",
                "range", "return", "go", "chan", "select", "case", "default", "switch", "break", "continue", "defer",
                "map", "nil", "true", "false",
            ],
            line_comment: Some("//"),
            block_comment: Some(("/*", "*/")),
        }),
        "bash" | "sh" | "shell" | "zsh" => Some(LanguageSpec {
            keywords: &[
                "if", "then", "else", "elif", "fi", "for", "while", "do", "done", "case", "esac", "function",
                "return", "exit", "export", "local", "echo", "break", "continue", "in",
            ],
            line_comment: Some("#"),
            block_comment: None,
        }),
        "json" => Some(LanguageSpec { keywords: &["true", "false", "null"], line_comment: None, block_comment: None }),
        "sql" => Some(LanguageSpec {
            keywords: &[
                "SELECT", "FROM", "WHERE", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "JOIN", "LEFT",
                "RIGHT", "INNER", "OUTER", "ON", "GROUP", "BY", "ORDER", "HAVING", "AS", "AND", "OR", "NOT", "NULL",
                "IS", "IN", "LIKE", "LIMIT", "CREATE", "TABLE", "DROP", "ALTER", "PRIMARY", "KEY", "FOREIGN",
                "REFERENCES", "DEFAULT", "DISTINCT",
            ],
            line_comment: Some("--"),
            block_comment: Some(("/*", "*/")),
        }),
        "ruby" | "rb" => Some(LanguageSpec {
            keywords: &[
                "def", "end", "class", "module", "if", "elsif", "else", "unless", "while", "until", "for", "in",
                "do", "return", "break", "next", "yield", "begin", "rescue", "ensure", "raise", "require", "true",
                "false", "nil", "self",
            ],
            line_comment: Some("#"),
            block_comment: None,
        }),
        "yaml" | "yml" | "toml" => {
            Some(LanguageSpec { keywords: &["true", "false", "null"], line_comment: Some("#"), block_comment: None })
        }
        _ => None,
    }
}

/// 把一行程式碼掃成字串/註解/數字/關鍵字/其餘（plain）幾類樣式化的
/// `Span`。`in_block_comment` 是跨行的狀態（區塊註解可能橫跨好幾行），呼叫端
/// 每個 code block 開頭要重設成 `false`，同一個 code block 內逐行往下傳。
fn highlight_code_line<'a>(line: &'a str, spec: &LanguageSpec, in_block_comment: &mut bool) -> Vec<Span<'a>> {
    let plain_style = Style::default().fg(Color::Green);
    let comment_style = Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC);
    let string_style = Style::default().fg(Color::Yellow);
    let number_style = Style::default().fg(Color::Magenta);
    let keyword_style = Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD);

    let mut spans = Vec::new();
    let mut plain_start = 0usize;
    let mut i = 0usize;

    if *in_block_comment {
        if let Some((_, close)) = spec.block_comment {
            match line.find(close) {
                Some(end_rel) => {
                    let end = end_rel + close.len();
                    spans.push(Span::styled(&line[..end], comment_style));
                    i = end;
                    plain_start = end;
                    *in_block_comment = false;
                }
                None => return vec![Span::styled(line, comment_style)],
            }
        }
    }

    while i < line.len() {
        let rest = &line[i..];

        if let Some((open, close)) = spec.block_comment {
            if rest.starts_with(open) {
                if plain_start < i {
                    spans.push(Span::styled(&line[plain_start..i], plain_style));
                }
                match rest[open.len()..].find(close) {
                    Some(end_rel) => {
                        let end = i + open.len() + end_rel + close.len();
                        spans.push(Span::styled(&line[i..end], comment_style));
                        i = end;
                    }
                    None => {
                        spans.push(Span::styled(&line[i..], comment_style));
                        *in_block_comment = true;
                        i = line.len();
                    }
                }
                plain_start = i;
                continue;
            }
        }
        if let Some(lc) = spec.line_comment {
            if rest.starts_with(lc) {
                if plain_start < i {
                    spans.push(Span::styled(&line[plain_start..i], plain_style));
                }
                spans.push(Span::styled(rest, comment_style));
                plain_start = line.len();
                break;
            }
        }
        if rest.starts_with('"') || rest.starts_with('\'') {
            let quote = &rest[..1];
            if let Some(end_rel) = rest[1..].find(quote) {
                if plain_start < i {
                    spans.push(Span::styled(&line[plain_start..i], plain_style));
                }
                let end = i + 1 + end_rel + 1;
                spans.push(Span::styled(&line[i..end], string_style));
                i = end;
                plain_start = i;
                continue;
            }
        }

        let ch = rest.chars().next().expect("i < line.len()");
        if ch.is_ascii_alphabetic() || ch == '_' {
            let word_len: usize =
                rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').map(char::len_utf8).sum();
            let word = &rest[..word_len];
            if spec.keywords.contains(&word) {
                if plain_start < i {
                    spans.push(Span::styled(&line[plain_start..i], plain_style));
                }
                spans.push(Span::styled(word, keyword_style));
                i += word_len;
                plain_start = i;
                continue;
            }
            i += word_len;
            continue;
        }
        if ch.is_ascii_digit() {
            let num_len: usize = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').map(char::len_utf8).sum();
            if plain_start < i {
                spans.push(Span::styled(&line[plain_start..i], plain_style));
            }
            spans.push(Span::styled(&rest[..num_len], number_style));
            i += num_len;
            plain_start = i;
            continue;
        }

        i += ch.len_utf8();
    }
    if plain_start < line.len() {
        spans.push(Span::styled(&line[plain_start..], plain_style));
    }
    if spans.is_empty() {
        spans.push(Span::styled("", plain_style));
    }
    spans
}

/// 找出整份筆記裡「完整」的 code fence 配對（開頭/結尾都找得到的 ` ``` `
/// 位置），回傳每一對的 (開頭行號, 結尾行號)。落單、還沒封閉的 fence（只有
/// 開頭沒有結尾）不算在內——那種情況沒辦法畫出一個有頭有尾的框。
fn find_fence_pairs(lines: &[impl AsRef<str>]) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    let mut open: Option<usize> = None;
    for (i, raw) in lines.iter().enumerate() {
        if raw.as_ref().trim_start().starts_with("```") {
            match open.take() {
                None => open = Some(i),
                Some(start) => pairs.push((start, i)),
            }
        }
    }
    pairs
}

/// 是不是一行 table 的資料列（含 header）——粗略判斷成「trim 過後含有至少
/// 一個 `|`」，實際是不是 table 由呼叫端搭配下一行是不是分隔列一起判斷。
fn is_table_row(line: &str) -> bool {
    line.contains('|') && !line.trim().is_empty()
}

/// 是不是 table 的分隔列（header 底下那一行，例如 `| --- | :---: | ---: |`）：
/// 只由 `-`、`:`、`|`、空白組成，且至少要有一個 `-`，不然一行空白或只有 `|`
/// 也會被誤判。
fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    t.contains('-') && t.chars().all(|c| matches!(c, '-' | ':' | '|' | ' ' | '\t'))
}

/// 把一行 table 資料列拆成每一欄的內容（已經 trim 過前後空白）；行首/行尾的
/// `|` 是可有可無的分隔符號，不當作多一欄空白內容。
fn split_table_row(line: &str) -> Vec<&str> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(str::trim).collect()
}

/// 找出整份筆記裡的 markdown table：一行含 `|` 的資料列，緊接著一行分隔列
/// （`is_table_separator`），就當作 table 的開頭；後面連續含 `|` 的行都算資料
/// 列，直到遇到不符合的行或緩衝區結尾為止。回傳每個 table 的
/// (header 行號, 最後一筆資料行號含)——只有 header+分隔列、沒有任何資料列
/// 也算合法的 table。
fn find_tables(lines: &[impl AsRef<str>]) -> Vec<(usize, usize)> {
    let mut tables = Vec::new();
    let mut row = 0usize;
    while row + 1 < lines.len() {
        let header = lines[row].as_ref();
        let separator = lines[row + 1].as_ref();
        if is_table_row(header) && !is_table_separator(header) && is_table_separator(separator) {
            let mut end = row + 1;
            let mut next = row + 2;
            while next < lines.len() && is_table_row(lines[next].as_ref()) {
                end = next;
                next += 1;
            }
            tables.push((row, end));
            row = end + 1;
        } else {
            row += 1;
        }
    }
    tables
}

/// 一個儲存格內容裡如果有 `<br>`，算「這格要佔幾格顯示寬度」時要分開量每一
/// 段、取其中最寬的那段（不能把 `<br>` 當成一般字元跟著算進同一段的寬度，
/// 那樣其他行沒有 `<br>` 的儲存格會被撐得過寬）。
fn cell_display_width(cell: &str) -> usize {
    cell.split("<br>").map(UnicodeWidthStr::width).max().unwrap_or(0)
}

/// 畫一列 table 資料，可能要佔好幾條實際的顯示行——儲存格內容裡如果有
/// `<br>`，那一格會被拆成好幾行，這一整列（要跟其他欄對齊）的高度就是所有
/// 儲存格裡最多的行數，比較矮的儲存格底下補空白行。`widths` 是每一欄事先
/// 算好的顯示寬度（`cell_display_width` 算出來的，已經考慮 `<br>` 拆開後、
/// 中文字佔兩格），欄位不夠寬的內容補空白對齊，欄與欄之間留空白隔開就好，
/// 不畫豎線。
fn render_table_row(cells: &[&str], widths: &[usize]) -> Vec<String> {
    let cell_lines: Vec<Vec<&str>> = cells.iter().map(|cell| cell.split("<br>").collect()).collect();
    let height = cell_lines.iter().map(Vec::len).max().unwrap_or(1).max(1);
    (0..height)
        .map(|line_idx| {
            let mut s = String::new();
            for (col, width) in widths.iter().enumerate() {
                let text = cell_lines.get(col).and_then(|lines| lines.get(line_idx)).copied().unwrap_or("");
                let pad = width.saturating_sub(UnicodeWidthStr::width(text));
                s.push_str(text);
                s.push_str(&" ".repeat(pad));
                if col + 1 < widths.len() {
                    s.push_str("   ");
                }
            }
            s
        })
        .collect()
}

/// 把整份筆記內容轉成畫面要顯示的 `Line`，回傳游標對應到「顯示用列表」裡的
/// 第幾行（不是原始緩衝區的行號——完整的 code block/table 都會被畫成佔用
/// 顯示行數比原始行數多的東西，兩者從這裡開始就不再是 1:1）。`width` 是外框/
/// table 分隔線要撐滿的寬度（notepad panel 內容區的實際欄數）。
///
/// 游標所在那一行本身（標題/引言/分隔線的符號），或游標所在的完整
/// code block/table（如果游標剛好在裡面），都維持原始樣子不做任何轉換，這樣
/// 才能逐字元精準編輯；其餘的完整 code block 才會被畫成外框、table 才會被
/// 排版對齊、標題/引言/分隔線的符號也才會被隱藏，呈現排版好的乾淨畫面。
/// `cursor_row` 是 `None`（沒有在編輯，單純檢視內容）時，每一個完整
/// code block/table 都會被轉換、每一行都藏符號。
fn render_notepad_lines<'a>(
    lines: &'a [impl AsRef<str>],
    cursor_row: Option<usize>,
    width: usize,
) -> (Vec<Line<'a>>, Option<usize>) {
    let border_style = Style::default().fg(Color::DarkGray);
    let code_style = Style::default().fg(Color::Green);

    let pairs = find_fence_pairs(lines);
    let cursor_pair = cursor_row.and_then(|row| pairs.iter().copied().find(|&(open, close)| open <= row && row <= close));
    let tables = find_tables(lines);
    let cursor_table = cursor_row.and_then(|row| tables.iter().copied().find(|&(h, e)| h <= row && row <= e));

    let mut display = Vec::new();
    let mut display_cursor_row = None;
    let mut row = 0usize;
    // 已經進入一個還沒封閉的 code block（落單、沒有配對到的 fence）：從那裡
    // 開始到緩衝區結尾都當作還在區塊裡，維持綠色純文字，不畫框（沒有結尾
    // 可以畫）。
    let mut dangling = false;
    while row < lines.len() {
        let line = lines[row].as_ref();
        let is_cursor_line = cursor_row == Some(row);

        if dangling {
            display.push(Line::from(Span::styled(line, code_style)));
            if is_cursor_line {
                display_cursor_row = Some(display.len() - 1);
            }
            row += 1;
            continue;
        }

        if let Some(&(open, close)) = pairs.iter().find(|&&(open, _)| open == row) {
            // 開頭 fence 行（例如 ```rust）標注的語言決定語法上色規則；語言
            // 名稱後面接一個 `=`（例如 ```rust=）表示要加行號，兩者是各自
            // 獨立的標注，`=` 拿掉之後剩下的部分才是真正的語言名稱。沒標
            // 語言或標了但不認得的語言都是 `None`，維持單一顏色的樣子。
            // `in_block_comment` 是這個 code block 專屬的狀態，區塊註解可能
            // 橫跨好幾行，每個 code block 都要重新從 `false` 開始，不能沿用
            // 上一個 code block 剩下的狀態。
            let raw_lang = lines[open].as_ref().trim_start().trim_start_matches("```").trim();
            let show_line_numbers = raw_lang.ends_with('=');
            let lang = raw_lang.trim_end_matches('=');
            let spec = language_spec(lang);
            let mut in_block_comment = false;
            // 行號靠右對齊的寬度：依這個 code block 實際的內容行數決定位數，
            // 例如 12 行內容就是 2 位數寬（" 1".." 9"/"10".."12"），不會因為
            // 固定寫死 3 位數而在只有幾行的小區塊裡留下一堆用不到的空白。
            let gutter_width = (close - open - 1).to_string().len().max(1);
            let line_number_span =
                |n: usize| Span::styled(format!("{n:>gutter_width$} "), Style::default().fg(Color::DarkGray));
            if Some((open, close)) == cursor_pair {
                for r in open..=close {
                    let mut styled = if r == open || r == close {
                        Vec::new()
                    } else if show_line_numbers {
                        vec![line_number_span(r - open)]
                    } else {
                        Vec::new()
                    };
                    if r == open || r == close {
                        styled.push(Span::styled(lines[r].as_ref(), code_style));
                    } else {
                        styled.extend(match &spec {
                            Some(spec) => highlight_code_line(lines[r].as_ref(), spec, &mut in_block_comment),
                            None => vec![Span::styled(lines[r].as_ref(), code_style)],
                        });
                    }
                    display.push(Line::from(styled));
                    if cursor_row == Some(r) {
                        display_cursor_row = Some(display.len() - 1);
                    }
                }
            } else {
                display.push(Line::from(Span::styled(horizontal_border(width, "┌", "┐"), border_style)));
                for r in (open + 1)..close {
                    let mut row_spans = vec![Span::styled("│ ", border_style)];
                    if show_line_numbers {
                        row_spans.push(line_number_span(r - open));
                    }
                    let gutter_used = if show_line_numbers { gutter_width + 1 } else { 0 };
                    row_spans.extend(code_content_spans(
                        lines[r].as_ref(),
                        width.saturating_sub(gutter_used),
                        spec.as_ref(),
                        &mut in_block_comment,
                    ));
                    row_spans.push(Span::styled(" │", border_style));
                    display.push(Line::from(row_spans));
                }
                display.push(Line::from(Span::styled(horizontal_border(width, "└", "┘"), border_style)));
            }
            row = close + 1;
            continue;
        }

        if line.trim_start().starts_with("```") {
            dangling = true;
            display.push(Line::from(Span::styled(line, code_style)));
            if is_cursor_line {
                display_cursor_row = Some(display.len() - 1);
            }
            row += 1;
            continue;
        }

        if let Some(&(header, end)) = tables.iter().find(|&&(h, _)| h == row) {
            if Some((header, end)) == cursor_table {
                for r in header..=end {
                    display.push(Line::from(Span::raw(lines[r].as_ref())));
                    if cursor_row == Some(r) {
                        display_cursor_row = Some(display.len() - 1);
                    }
                }
            } else {
                let header_cells = split_table_row(lines[header].as_ref());
                let separator_cols = split_table_row(lines[header + 1].as_ref()).len();
                let col_count = header_cells.len().max(separator_cols);
                let data_rows: Vec<Vec<&str>> =
                    ((header + 2)..=end).map(|r| split_table_row(lines[r].as_ref())).collect();

                let mut widths = vec![0usize; col_count];
                for (col, w) in widths.iter_mut().enumerate() {
                    *w = cell_display_width(header_cells.get(col).copied().unwrap_or(""));
                }
                for row_cells in &data_rows {
                    for (col, w) in widths.iter_mut().enumerate() {
                        *w = (*w).max(cell_display_width(row_cells.get(col).copied().unwrap_or("")));
                    }
                }

                let header_lines = render_table_row(&header_cells, &widths);
                let table_width = header_lines.iter().map(|l| UnicodeWidthStr::width(l.as_str())).max().unwrap_or(0);
                for line in header_lines {
                    display.push(Line::from(Span::styled(line, Style::default().add_modifier(Modifier::BOLD))));
                }
                if !data_rows.is_empty() {
                    // header 跟第一筆資料之間的分隔線用粗體橫線字元
                    // （`━`，比一般的 `─` 粗，兩者字元本身粗細不同，不是靠
                    // `Modifier::BOLD` 讓 `─` 變粗——大部分終端字型裡橫線字元
                    // 加粗體修飾看不太出來差異），資料列跟資料列之間才是
                    // 一般粗細的分隔線；最上面、最下面都不畫線。
                    display.push(Line::from(Span::styled(
                        "━".repeat(table_width),
                        Style::default().add_modifier(Modifier::BOLD),
                    )));
                    for (idx, row_cells) in data_rows.iter().enumerate() {
                        for line in render_table_row(row_cells, &widths) {
                            display.push(Line::from(Span::raw(line)));
                        }
                        if idx + 1 < data_rows.len() {
                            display.push(Line::from(Span::styled("─".repeat(table_width), border_style)));
                        }
                    }
                }
            }
            row = end + 1;
            continue;
        }

        if let Some(level) = heading_level(line) {
            display.push(style_line(line, !is_cursor_line, width));
            if is_cursor_line {
                display_cursor_row = Some(display.len() - 1);
            }
            // 一級、二級標題底下自動加一條分隔線，讓標題更醒目；這是額外畫
            // 出來的裝飾、不是原始緩衝區內容，不影響逐字元編輯，所以不管
            // 游標在不在這一行都加，不需要跟著 hide_markers 規則藏起來。
            if level <= 2 {
                display.push(Line::from(Span::styled("─".repeat(width), border_style)));
            }
            row += 1;
            continue;
        }

        if line.contains("<br>") {
            if is_cursor_line {
                // 游標在這一行：維持原始一整行，`<br>` 當作字面文字顯示（不
                // 觸發真的換行），才能逐字元精準編輯。
                display.push(style_line(line, false, width));
                display_cursor_row = Some(display.len() - 1);
            } else {
                for part in line.split("<br>") {
                    display.push(Line::from(style_inline(part, true)));
                }
            }
            row += 1;
            continue;
        }

        display.push(style_line(line, !is_cursor_line, width));
        if is_cursor_line {
            display_cursor_row = Some(display.len() - 1);
        }
        row += 1;
    }
    (display, display_cursor_row)
}

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
                    frame.render_widget(block, rect);
                    if inner.height > 0 {
                        let body_height = inner.height.saturating_sub(1);
                        let hint_area = Rect { x: inner.x, y: inner.y + body_height, width: inner.width, height: 1 };
                        let body_area = Rect { x: inner.x, y: inner.y, width: inner.width, height: body_height };
                        let lines = output.all();
                        let start = lines.len().saturating_sub(body_area.height as usize);
                        let visible: Vec<Line> = lines[start..].iter().map(|l| Line::raw(l.as_str())).collect();
                        frame.render_widget(Paragraph::new(visible), body_area);
                        let hint_style = Style::default().fg(Color::DarkGray);
                        frame.render_widget(Paragraph::new(PANEL_HINT).style(hint_style), hint_area);
                    }
                } else if name == "notepad" {
                    // 最下面一行固定顯示快速鍵提示（Ctrl-E/Ctrl-S/Ctrl-X），其餘才是
                    // 內容/編輯區域——跟一般 Paragraph 直接塞進整個 `rect` 不一樣，
                    // 所以邊框（`block`）跟內容分開畫：先畫邊框，再把扣掉邊框的
                    // `inner` 拆成「內容」跟「提示」兩塊分別畫。
                    let inner = block.inner(rect);
                    frame.render_widget(block, rect);
                    if inner.height > 0 {
                        let body_height = inner.height.saturating_sub(1);
                        let hint_area = Rect { x: inner.x, y: inner.y + body_height, width: inner.width, height: 1 };
                        let hint_style = Style::default().fg(Color::DarkGray);
                        let body_area = Rect { x: inner.x, y: inner.y, width: inner.width, height: body_height };

                        let editing = with_notepad(shell, |np| {
                            np.editing_view().map(|(lines, row, col)| (lines.to_vec(), row, col))
                        })
                        .flatten();
                        let current_name =
                            with_notepad(shell, |np| np.current_name().to_string()).unwrap_or_default();
                        let file_prompt =
                            with_notepad(shell, |np| np.file_prompt_text().map(str::to_string)).flatten();

                        match editing {
                            Some((lines, cursor_row, cursor_col)) => {
                                // 完整的 code block 會被畫成外框，佔用的顯示行數比原始
                                // 行數多，所以捲動視窗要用「顯示列表」裡游標對應到的
                                // 行號（`cursor_disp_row`），不能直接用原始的 `cursor_row`。
                                let (display, display_cursor_row) =
                                    render_notepad_lines(&lines, Some(cursor_row), body_area.width as usize);
                                let cursor_disp_row = display_cursor_row.unwrap_or(0);
                                // 讓游標所在那一行永遠在可視範圍內：捲動起點只在游標
                                // 超出目前這一頁的時候才往下移，不會每次畫面都跳回開頭。
                                let height = body_height.max(1) as usize;
                                let start =
                                    if cursor_disp_row >= height { cursor_disp_row + 1 - height } else { 0 };
                                let end = (start + height).min(display.len());
                                let visible: Vec<Line> = display[start..end].to_vec();
                                frame.render_widget(Paragraph::new(visible), body_area);
                                let hint = hint_line_with_filename(
                                    "Ctrl-S 儲存並離開　Ctrl-X 放棄修改",
                                    &current_name,
                                    hint_area.width as usize,
                                );
                                frame.render_widget(Paragraph::new(hint).style(hint_style), hint_area);
                                // 編輯中時終端機的實體游標要顯示在這裡（不是輸入框），
                                // 見迴圈最後 `frame.set_cursor_position` 前的判斷。游標
                                // 所在那一行（不管在不在 code block 裡）永遠維持原始
                                // 顯示、沒有加外框前綴，`cursor_col` 不需要另外調整。
                                frame.set_cursor_position((
                                    inner.x + cursor_col as u16,
                                    inner.y + (cursor_disp_row - start) as u16,
                                ));
                            }
                            None => {
                                let text = lock_shell(shell).plugin_panel_text(name).unwrap_or_default();
                                let text_lines: Vec<&str> = text.lines().collect();
                                let (display, _) =
                                    render_notepad_lines(&text_lines, None, body_area.width as usize);
                                frame.render_widget(Paragraph::new(display), body_area);
                                match &file_prompt {
                                    Some(typed) => {
                                        // Ctrl-F 按下後正在輸入檔名：內容區維持顯示原本
                                        // 那個檔案（還沒真的切換過去），底下這一行換成
                                        // 輸入用的提示列，終端機的實體游標也要移到這裡
                                        // （見迴圈最後 `frame.set_cursor_position` 前的
                                        // 判斷），不然使用者打字打到一半看不到游標在哪。
                                        let prompt_line = format!("檔名: {typed}");
                                        frame.render_widget(Paragraph::new(prompt_line.clone()), hint_area);
                                        frame.set_cursor_position((
                                            inner.x + UnicodeWidthStr::width(prompt_line.as_str()) as u16,
                                            inner.y + body_height,
                                        ));
                                    }
                                    None => {
                                        let hint = hint_line_with_filename(
                                            "Ctrl-E 編輯　Ctrl-F 切換檔案",
                                            &current_name,
                                            hint_area.width as usize,
                                        );
                                        frame.render_widget(Paragraph::new(hint).style(hint_style), hint_area);
                                    }
                                }
                            }
                        }
                    }
                } else if let Some(text) = lock_shell(shell).plugin_panel_text(name) {
                    let inner = block.inner(rect);
                    frame.render_widget(block, rect);
                    if inner.height > 0 {
                        let body_height = inner.height.saturating_sub(1);
                        let hint_area = Rect { x: inner.x, y: inner.y + body_height, width: inner.width, height: 1 };
                        let body_area = Rect { x: inner.x, y: inner.y, width: inner.width, height: body_height };
                        // `Paragraph`沒有 `.scroll()` 的話一律從第一行開始畫，超過
                        // `body_area` 高度的部分不會自動捲到最後幾行——跟 `output`
                        // panel 那個分支一樣，只取「最後塞得進去的那幾行」畫，不然
                        // 內容一多（例如 remote-output 鏡射的是別台機器整段
                        // scrollback），新內容一直被卡在畫面看不到的下面，
                        // 畫面看起來像是「沒有更新」。
                        let lines: Vec<&str> = text.lines().collect();
                        let start = lines.len().saturating_sub(body_area.height as usize);
                        let visible: Vec<Line> = lines[start..].iter().map(|l| Line::raw(*l)).collect();
                        frame.render_widget(Paragraph::new(visible), body_area);
                        let hint_style = Style::default().fg(Color::DarkGray);
                        frame.render_widget(Paragraph::new(PANEL_HINT).style(hint_style), hint_area);
                    }
                } else {
                    let inner = block.inner(rect);
                    frame.render_widget(block, rect);
                    if inner.height > 0 {
                        let hint_area = Rect { x: inner.x, y: inner.y + inner.height - 1, width: inner.width, height: 1 };
                        let hint_style = Style::default().fg(Color::DarkGray);
                        frame.render_widget(Paragraph::new(PANEL_HINT).style(hint_style), hint_area);
                    }
                }
            }

            // 輸入 panel 內容只有一行，但外框上下各佔一行，所以整個 panel 高度是 3。
            frame.render_widget(Clear, input_area);
            let input_block = Block::default().borders(Borders::ALL).border_type(BorderType::Plain);
            let input_inner = input_block.inner(input_area);

            let input_line = format!("{prompt}{input}");
            frame.render_widget(Paragraph::new(input_line.as_str()).block(input_block), input_area);
            // 正在編輯 notepad、或正在輸入要切換的檔名時，終端機的實體游標
            // 已經在上面那個迴圈裡被擺到 notepad 面板裡了（見 `name ==
            // "notepad"` 分支）——這裡不能再無條件蓋回輸入框，不然使用者會
            // 看到游標一直停在輸入框，但打字其實都送去別的地方，位置對不上。
            let notepad_editing = with_notepad(shell, |np| np.is_editing()).unwrap_or(false);
            let notepad_prompting = with_notepad(shell, |np| np.is_prompting_file()).unwrap_or(false);
            if !notepad_editing && !notepad_prompting {
                frame.set_cursor_position((
                    input_inner.x + input_line.chars().count() as u16,
                    input_inner.y,
                ));
            }
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

        // notepad 是不是目前的 active panel（疊放順序最上層、可見），以及是不是
        // 正在編輯中——決定接下來這個按鍵要走「編輯緩衝區」還是原本「指令輸入
        // 列」的路。`notepad_active` 只在還沒開始編輯（Ctrl-E）時需要判斷；
        // 已經在編輯中的話，不管使用者按 Tab 換了哪個 panel 當 active，既有的
        // 編輯緩衝區依然有效、應該繼續接收按鍵（`notepad_editing` 才是接下來
        // 每個按鍵判斷要用的旗標）。
        let notepad_active = panels.last().is_some_and(|(name, _)| name == "notepad");
        let notepad_editing = with_notepad(shell, |np| np.is_editing()).unwrap_or(false);
        let notepad_prompting = with_notepad(shell, |np| np.is_prompting_file()).unwrap_or(false);
        // qr 是不是目前的 active panel，決定 PgUp/PgDn 要不要拿去切換它顯示的
        // 是 local 還是 server（見 `with_qr`）——不是 active 的話這兩個鍵沒有
        // 意義，跟 notepad 的 Ctrl-E/Ctrl-F 只在 `notepad_active` 時才生效同理。
        let qr_active = panels.last().is_some_and(|(name, _)| name == "qr");

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Char('e')
                if notepad_active && !notepad_editing && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                with_notepad(shell, |np| np.start_editing());
            }
            KeyCode::Char('f')
                if notepad_active
                    && !notepad_editing
                    && !notepad_prompting
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                with_notepad(shell, |np| np.start_file_prompt());
            }
            KeyCode::Enter if notepad_prompting => {
                with_notepad(shell, |np| np.confirm_file_prompt());
            }
            KeyCode::Esc if notepad_prompting => {
                with_notepad(shell, |np| np.cancel_file_prompt());
            }
            KeyCode::Backspace if notepad_prompting => {
                with_notepad(shell, |np| np.file_prompt_backspace());
            }
            KeyCode::Char(c)
                if notepad_prompting
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                with_notepad(shell, |np| np.file_prompt_insert(c));
            }
            KeyCode::Char('s') if notepad_editing && key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(Err(err)) = with_notepad(shell, |np| np.save_editing()) {
                    output.push(&format!("notepad 儲存失敗: {err:#}\n"));
                }
            }
            KeyCode::Char('x') if notepad_editing && key.modifiers.contains(KeyModifiers::CONTROL) => {
                with_notepad(shell, |np| np.cancel_editing());
            }
            KeyCode::Left if notepad_editing && !key.modifiers.contains(KeyModifiers::ALT) => {
                with_notepad(shell, |np| np.move_left());
            }
            KeyCode::Right if notepad_editing && !key.modifiers.contains(KeyModifiers::ALT) => {
                with_notepad(shell, |np| np.move_right());
            }
            KeyCode::Up if notepad_editing && !key.modifiers.contains(KeyModifiers::ALT) => {
                with_notepad(shell, |np| np.move_up());
            }
            KeyCode::Down if notepad_editing && !key.modifiers.contains(KeyModifiers::ALT) => {
                with_notepad(shell, |np| np.move_down());
            }
            KeyCode::Home if notepad_editing => {
                with_notepad(shell, |np| np.move_home());
            }
            KeyCode::End if notepad_editing => {
                with_notepad(shell, |np| np.move_end());
            }
            KeyCode::Backspace if notepad_editing => {
                with_notepad(shell, |np| np.backspace());
            }
            KeyCode::Delete if notepad_editing => {
                with_notepad(shell, |np| np.delete_forward());
            }
            KeyCode::Enter if notepad_editing => {
                with_notepad(shell, |np| np.insert_newline());
            }
            // 編輯中時 Tab 讓編輯器自己吃掉（插入空白），不要拿去切換 active
            // panel——不然正在打字打到一半，按個 Tab 縮排卻把畫面切走，會很意外。
            KeyCode::Tab if notepad_editing => {
                with_notepad(shell, |np| {
                    np.insert_char(' ');
                    np.insert_char(' ');
                });
            }
            KeyCode::Char(c)
                if notepad_editing
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                with_notepad(shell, |np| np.insert_char(c));
            }
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
                let remote_shell_ip = sh.take_pending_remote_shell();
                let upgrade_requested = sh.take_pending_upgrade();
                drop(sh);
                if upgrade_requested {
                    run_upgrade(shell.clone(), output.clone());
                }
                if let Some(ip) = remote_shell_ip {
                    // 跟本地 `shell` passthrough 一樣先離開 alternate screen，讓
                    // 遠端的畫面用一般的、可以捲動回看的螢幕；但不能跟本地那邊
                    // 一樣 disable_raw_mode——`run_remote_shell` 要靠 raw mode
                    // 才能逐位元組把鍵盤輸入轉送給遠端，不能讓本地終端機自己先
                    // 做行編輯/echo。
                    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                    terminal.show_cursor()?;
                    run_remote_shell(&ip, output);
                    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
                    // 見下面 shell_passthrough 分支裡同樣一行的說明。
                    *terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
                }
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
                    'x' => lock_shell(shell).close_active_panel(),
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
            KeyCode::PageUp if qr_active => {
                with_qr(shell, |qr| qr.cycle_prev());
            }
            KeyCode::PageDown if qr_active => {
                with_qr(shell, |qr| qr.cycle_next());
            }
            KeyCode::Char(c) => input.push(c),
            _ => {}
        }
    }
}
