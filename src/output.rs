use std::sync::Mutex;

/// 所有指令輸出（`help`、plugin 的 `dispatch`、腳本執行時的 echo...）都往這裡寫，
/// 不直接 print 到 stdout，這樣 CLI 模式跟 GUI 模式才能各自決定要怎麼把這些內容畫出來。
#[derive(Default)]
pub struct OutputBuffer {
    lines: Mutex<Vec<String>>,
}

impl OutputBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// `text` 可以包含多行（用 `\n` 分隔），每一行會各自成為緩衝區裡的一筆。
    pub fn push(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut lines = self.lines.lock().unwrap();
        for line in text.strip_suffix('\n').unwrap_or(text).split('\n') {
            lines.push(line.to_string());
        }
    }

    /// 目前為止累積的所有行數。
    pub fn len(&self) -> usize {
        self.lines.lock().unwrap().len()
    }

    /// 從索引 `from` 開始（含）到目前為止新增的所有行。
    pub fn lines_from(&self, from: usize) -> Vec<String> {
        let lines = self.lines.lock().unwrap();
        let from = from.min(lines.len());
        lines[from..].to_vec()
    }

    pub fn all(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
}
