use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use unicode_width::UnicodeWidthStr;

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

/// 筆記檔案都放在這個資料夾底下（相對於程式執行時的工作目錄），跟 `MUSIC_DIR`
/// 同等位階。web 那邊的編輯功能（見 `web.rs`）直接讀寫這個資料夾（固定操作
/// `DEFAULT_NOTEPAD_FILE`），不透過 `Shell`/`NotepadPlugin`。
pub(crate) const NOTEPAD_DIR: &str = "notepad";

/// 一開始（還沒按 Ctrl-F 切換過檔案）預設開這個檔案。
pub(crate) const DEFAULT_NOTEPAD_FILE: &str = "notepad.md";

/// 編輯中的緩衝區：用一行一個 `String` 存，而不是單一大字串——游標的「第幾行
/// 第幾個字」這種操作（換行/上下移動/合併相鄰行）在這種表示法下不用每次都
/// 重新切割整段文字找換行字元，操作起來單純很多。`cursor_col` 是「第幾個字元」
/// （不是 byte offset），含中文這種多 byte 字元時兩者不一樣，每次要動字串內容
/// 前都得先用 `char_byte_index` 換算成 byte offset。
struct EditState {
    filename: String,
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
}

pub struct NotepadPlugin {
    #[allow(dead_code)]
    ctx: SharedContext,
    current_name: String,
    /// 目前顯示（非編輯狀態）用的內容，是上次讀檔/存檔當下的樣子。
    content: String,
    edit: Option<EditState>,
    /// Ctrl-F 按下後，正在輸入的檔名（還沒按 Enter 確認）；`Some(String::new())`
    /// 是剛按下 Ctrl-F、還沒打任何字元的狀態。`None` 代表目前沒有在輸入檔名。
    file_prompt: Option<String>,
}

impl NotepadPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        let mut plugin =
            Self { ctx, current_name: String::new(), content: String::new(), edit: None, file_prompt: None };
        plugin.load(DEFAULT_NOTEPAD_FILE);
        plugin
    }

    fn path_for(name: &str) -> PathBuf {
        Path::new(NOTEPAD_DIR).join(name)
    }

    /// 切換目前顯示的檔案：讀檔失敗（多半是檔案還不存在）就當作空白新檔案，
    /// 不報錯——這是「新筆記」的正常情境，不是真的錯誤。
    fn load(&mut self, name: &str) {
        self.current_name = name.to_string();
        self.content = fs::read_to_string(Self::path_for(name)).unwrap_or_default();
        self.edit = None;
    }

    // --- 給 GUI/web 直接操作編輯狀態用（見 `plugin::Plugin::as_any_mut`） ---

    /// 目前顯示（或正在編輯）的檔名，panel 右下角顯示用。
    pub fn current_name(&self) -> &str {
        &self.current_name
    }

    pub fn is_editing(&self) -> bool {
        self.edit.is_some()
    }

    pub fn is_prompting_file(&self) -> bool {
        self.file_prompt.is_some()
    }

    pub fn file_prompt_text(&self) -> Option<&str> {
        self.file_prompt.as_deref()
    }

    /// Ctrl-F：開始輸入要切換到的檔名。已經在編輯中，或已經在輸入檔名時不
    /// 做事（不會打斷正在編輯的內容，也不會重設已經打了一半的檔名）。
    pub fn start_file_prompt(&mut self) {
        if self.edit.is_some() || self.file_prompt.is_some() {
            return;
        }
        self.file_prompt = Some(String::new());
    }

    pub fn file_prompt_insert(&mut self, c: char) {
        if let Some(buf) = &mut self.file_prompt {
            buf.push(c);
        }
    }

    pub fn file_prompt_backspace(&mut self) {
        if let Some(buf) = &mut self.file_prompt {
            buf.pop();
        }
    }

    /// 放棄輸入，不切換檔案。
    pub fn cancel_file_prompt(&mut self) {
        self.file_prompt = None;
    }

    /// Enter：確認目前輸入的檔名，切換目前顯示的檔案並離開輸入狀態。輸入
    /// 空字串（或只有空白）就當作放棄，不切換——不然按到空白 Enter 會把
    /// 檔名切成一個沒有意義的空字串。
    pub fn confirm_file_prompt(&mut self) {
        let Some(name) = self.file_prompt.take() else { return };
        let name = name.trim();
        if !name.is_empty() {
            self.load(name);
        }
    }

    /// 進入編輯模式：把目前內容複製一份到編輯緩衝區，游標預設放在最後一行的
    /// 最後面（筆記本常見的用法是接著往下寫，直接從結尾開始最順手）。已經在
    /// 編輯中的話不做事，不會弄丟正在編輯的緩衝區。
    pub fn start_editing(&mut self) {
        if self.edit.is_some() {
            return;
        }
        let lines: Vec<String> =
            if self.content.is_empty() { vec![String::new()] } else { self.content.lines().map(str::to_string).collect() };
        let cursor_row = lines.len() - 1;
        let cursor_col = lines[cursor_row].chars().count();
        self.edit = Some(EditState { filename: self.current_name.clone(), lines, cursor_row, cursor_col });
    }

    /// 放棄編輯緩衝區內容，離開編輯模式，不寫檔。
    pub fn cancel_editing(&mut self) {
        self.edit = None;
    }

    /// 儲存編輯緩衝區內容到檔案、離開編輯模式。儲存失敗（例如目錄建立失敗、
    /// 沒有寫入權限）時保留編輯緩衝區讓使用者能重試，不要因為一次失敗就弄丟
    /// 正在編輯的內容。
    pub fn save_editing(&mut self) -> Result<()> {
        let Some(state) = &self.edit else { return Ok(()) };
        let filename = state.filename.clone();
        let text = state.lines.join("\n");
        fs::create_dir_all(NOTEPAD_DIR).context("建立 notepad 目錄失敗")?;
        fs::write(Self::path_for(&filename), &text).context("儲存檔案失敗")?;
        self.content = text;
        self.edit = None;
        Ok(())
    }

    /// 編輯畫面要顯示的內容：每一行文字、游標所在的行號，以及游標左邊內容的
    /// 「顯示寬度」（欄數，已經算好中文字寬度，GUI 直接拿來當終端機座標用，
    /// 不用自己重算一次 byte/char/顯示寬度的轉換）。不在編輯狀態時回 `None`。
    pub fn editing_view(&self) -> Option<(&[String], usize, usize)> {
        self.edit.as_ref().map(|s| {
            let line = &s.lines[s.cursor_row];
            let byte_idx = char_byte_index(line, s.cursor_col);
            let col = UnicodeWidthStr::width(&line[..byte_idx]);
            (s.lines.as_slice(), s.cursor_row, col)
        })
    }

    pub fn insert_char(&mut self, c: char) {
        let Some(state) = &mut self.edit else { return };
        let line = &mut state.lines[state.cursor_row];
        let byte_idx = char_byte_index(line, state.cursor_col);
        line.insert(byte_idx, c);
        state.cursor_col += 1;
    }

    pub fn insert_newline(&mut self) {
        let Some(state) = &mut self.edit else { return };
        let line = &mut state.lines[state.cursor_row];
        let byte_idx = char_byte_index(line, state.cursor_col);
        let rest = line.split_off(byte_idx);
        state.lines.insert(state.cursor_row + 1, rest);
        state.cursor_row += 1;
        state.cursor_col = 0;
    }

    /// 游標前一個字元：在行首時改成跟上一行合併（游標移到「原本上一行結尾」）。
    pub fn backspace(&mut self) {
        let Some(state) = &mut self.edit else { return };
        if state.cursor_col > 0 {
            let line = &mut state.lines[state.cursor_row];
            let start = char_byte_index(line, state.cursor_col - 1);
            let end = char_byte_index(line, state.cursor_col);
            line.replace_range(start..end, "");
            state.cursor_col -= 1;
        } else if state.cursor_row > 0 {
            let current = state.lines.remove(state.cursor_row);
            state.cursor_row -= 1;
            let prev = &mut state.lines[state.cursor_row];
            state.cursor_col = prev.chars().count();
            prev.push_str(&current);
        }
    }

    /// 游標後一個字元（Delete 鍵）：在行尾時改成跟下一行合併。
    pub fn delete_forward(&mut self) {
        let Some(state) = &mut self.edit else { return };
        let line_len = state.lines[state.cursor_row].chars().count();
        if state.cursor_col < line_len {
            let line = &mut state.lines[state.cursor_row];
            let start = char_byte_index(line, state.cursor_col);
            let end = char_byte_index(line, state.cursor_col + 1);
            line.replace_range(start..end, "");
        } else if state.cursor_row + 1 < state.lines.len() {
            let next = state.lines.remove(state.cursor_row + 1);
            state.lines[state.cursor_row].push_str(&next);
        }
    }

    pub fn move_left(&mut self) {
        let Some(state) = &mut self.edit else { return };
        if state.cursor_col > 0 {
            state.cursor_col -= 1;
        } else if state.cursor_row > 0 {
            state.cursor_row -= 1;
            state.cursor_col = state.lines[state.cursor_row].chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let Some(state) = &mut self.edit else { return };
        let line_len = state.lines[state.cursor_row].chars().count();
        if state.cursor_col < line_len {
            state.cursor_col += 1;
        } else if state.cursor_row + 1 < state.lines.len() {
            state.cursor_row += 1;
            state.cursor_col = 0;
        }
    }

    pub fn move_up(&mut self) {
        let Some(state) = &mut self.edit else { return };
        if state.cursor_row > 0 {
            state.cursor_row -= 1;
            state.cursor_col = state.cursor_col.min(state.lines[state.cursor_row].chars().count());
        }
    }

    pub fn move_down(&mut self) {
        let Some(state) = &mut self.edit else { return };
        if state.cursor_row + 1 < state.lines.len() {
            state.cursor_row += 1;
            state.cursor_col = state.cursor_col.min(state.lines[state.cursor_row].chars().count());
        }
    }

    pub fn move_home(&mut self) {
        if let Some(state) = &mut self.edit {
            state.cursor_col = 0;
        }
    }

    pub fn move_end(&mut self) {
        if let Some(state) = &mut self.edit {
            state.cursor_col = state.lines[state.cursor_row].chars().count();
        }
    }
}

/// 把「第幾個字元」換算成這個字串裡對應的 byte offset（`String` 的索引/切割都
/// 要用 byte offset，但游標位置語意上是「第幾個字元」，含中文這種多 byte 字元
/// 時兩者不一樣）。`char_idx` 等於字元總數時（游標在行尾）回傳 `s.len()`。
fn char_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(s.len())
}

impl Plugin for NotepadPlugin {
    // 沒有任何指令：換檔案是 GUI/web panel 裡的 Ctrl-F（見
    // `start_file_prompt`），不透過 `execute_line` 送指令字串。
    fn commands(&self) -> &'static [&'static str] {
        &[]
    }

    fn dispatch(&mut self, cmd: &str, _args: &[String], _out: &OutputBuffer) -> Result<()> {
        anyhow::bail!("notepad 不認得指令: {cmd}")
    }

    fn panel_text(&self) -> Option<String> {
        Some(self.content.clone())
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
