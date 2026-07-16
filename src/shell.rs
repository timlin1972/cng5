use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, ensure, Context, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

pub enum Mode {
    Root,
    InPlugin(String),
}

/// root mode 的 `mode cli` / `mode gui` 要求切換到哪一種畫面。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    Cli,
    Gui,
}

/// 依 plugin 名稱建立一個新實例，讓 `plugin add <name>` 可以延後建立。
pub type PluginFactory = Box<dyn Fn(SharedContext) -> Box<dyn Plugin> + Send>;

pub struct Shell {
    ctx: SharedContext,
    factories: HashMap<String, PluginFactory>,
    active: HashMap<String, Box<dyn Plugin>>,
    mode: Mode,
    should_exit: bool,
    requested_ui: Option<UiMode>,
    output: Arc<OutputBuffer>,
}

impl Shell {
    pub fn new(
        ctx: SharedContext,
        factories: Vec<(&'static str, PluginFactory)>,
        output: Arc<OutputBuffer>,
    ) -> Self {
        let factories = factories
            .into_iter()
            .map(|(name, factory)| (name.to_string(), factory))
            .collect();
        Self {
            ctx,
            factories,
            active: HashMap::new(),
            mode: Mode::Root,
            should_exit: false,
            requested_ui: None,
            output,
        }
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

    pub fn prompt(&self) -> String {
        match &self.mode {
            Mode::Root => "cng5> ".to_string(),
            Mode::InPlugin(name) => format!("cng5({name})> "),
        }
    }

    pub fn execute_line(&mut self, line: &str) -> Result<()> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            return Ok(());
        }
        let tokens = shell_words::split(line).context("指令解析失敗")?;
        let (cmd, args) = tokens.split_first().expect("已檢查過非空行");

        match (&self.mode, cmd.as_str()) {
            (Mode::Root, "help") => self.print_help(),
            (Mode::Root, "plugin") => {
                let sub = args
                    .first()
                    .context("plugin 後面要接 add <name> / show / enter <name>")?;
                match sub.as_str() {
                    "add" => {
                        let name = args.get(1).context("plugin add 後面要接 plugin 名稱")?;
                        let factory = self
                            .factories
                            .get(name)
                            .with_context(|| format!("沒有這個 plugin: {name}"))?;
                        ensure!(!self.active.contains_key(name), "plugin 已經加入過了: {name}");
                        let instance = factory(self.ctx.clone());
                        self.active.insert(name.clone(), instance);
                    }
                    "show" => self.print_plugin_list(),
                    "enter" => {
                        let name = args.get(1).context("plugin enter 後面要接 plugin 名稱")?;
                        ensure!(
                            self.active.contains_key(name),
                            "plugin 尚未加入，請先執行: plugin add {name}"
                        );
                        self.mode = Mode::InPlugin(name.clone());
                    }
                    other => bail!("plugin 不認得子指令: {other}"),
                }
            }
            (Mode::Root, "mode") => {
                let target = args.first().context("mode 後面要接 cli 或 gui")?;
                self.requested_ui = Some(match target.as_str() {
                    "cli" => UiMode::Cli,
                    "gui" => UiMode::Gui,
                    other => bail!("mode 不認得: {other}"),
                });
            }
            (Mode::Root, "exit") => self.should_exit = true,
            (Mode::Root, other) => bail!("root mode 不認得指令: {other}"),
            (Mode::InPlugin(_), "help") => self.print_help(),
            (Mode::InPlugin(_), "exit") => self.mode = Mode::Root,
            (Mode::InPlugin(name), other) => {
                self.active
                    .get_mut(name)
                    .expect("mode 對應的 plugin 一定存在")
                    .dispatch(other, args, &self.output)?;
            }
        }
        Ok(())
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
        }
    }

    /// 目前 mode 底下所有指令的用法字串（第一個字是指令名稱）。
    fn usage_lines(&self) -> Vec<&str> {
        match &self.mode {
            Mode::Root => vec![
                "help",
                "plugin add <name>",
                "plugin show",
                "plugin enter <name>",
                "mode cli",
                "mode gui",
                "exit",
            ],
            Mode::InPlugin(name) => {
                let plugin = self
                    .active
                    .get(name)
                    .expect("mode 對應的 plugin 一定存在");
                let mut lines = vec!["help"];
                lines.extend(plugin.commands());
                lines.push("exit");
                lines
            }
        }
    }

    /// `line` 開頭的字是否逐一對應 `prefix_tokens`。
    fn line_starts_with(line: &str, prefix_tokens: &[&str]) -> bool {
        let mut words = line.split_whitespace();
        prefix_tokens
            .iter()
            .all(|tok| words.next() == Some(*tok))
    }

    /// 已經打完 `prefix_tokens`，正在打下一個字（前綴是 `partial`）：
    /// 列出所有符合的下一個字（去重複）。
    fn name_matches_text(&self, prefix_tokens: &[&str], partial: &str) -> String {
        let mut names: Vec<&str> = Vec::new();
        for line in self.usage_lines() {
            if !Self::line_starts_with(line, prefix_tokens) {
                continue;
            }
            if let Some(next_word) = line.split_whitespace().nth(prefix_tokens.len()) {
                if next_word.starts_with(partial) && !names.contains(&next_word) {
                    names.push(next_word);
                }
            }
        }
        names.iter().map(|name| format!("{name}\n")).collect()
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
        s.push_str("  plugin add <name>    加入指定的 plugin\n");
        s.push_str("  plugin show          列出可用/已加入的 plugin\n");
        s.push_str("  plugin enter <name>  進入已加入的 plugin mode\n");
        s.push_str("  mode cli             切換成一般的命令列畫面\n");
        s.push_str("  mode gui             切換成上下兩個 panel 的畫面\n");
        s.push_str("  exit                 離開程式\n");
        s.push_str(&self.plugin_list_text());
        s
    }

    fn print_plugin_list(&self) {
        self.output.push(&self.plugin_list_text());
    }

    fn plugin_list_text(&self) -> String {
        let mut names: Vec<&str> = self.factories.keys().map(String::as_str).collect();
        names.sort();
        let mut active: Vec<&str> = self.active.keys().map(String::as_str).collect();
        active.sort();

        format!(
            "可用的 plugin: {}\n已加入的 plugin: {}\n",
            names.join(", "),
            if active.is_empty() {
                "(無)".to_string()
            } else {
                active.join(", ")
            }
        )
    }

    fn plugin_help_text(&self, name: &str) -> String {
        let plugin = self
            .active
            .get(name)
            .expect("mode 對應的 plugin 一定存在");

        let mut s = format!("可用指令 ({name}):\n");
        s.push_str("  help               顯示這個說明\n");
        s.push_str("  exit               回到 root\n");
        for cmd in plugin.commands() {
            s.push_str(&format!("  {cmd}\n"));
        }
        s
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
