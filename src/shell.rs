use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

pub enum Mode {
    Root,
    InPlugin(String),
    InPanel(String),
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
            current_ui: UiMode::Cli,
            panels: HashMap::new(),
            panel_order: Vec::new(),
            maximized: HashMap::new(),
            output,
            history: Vec::new(),
        }
    }

    /// 呼叫端（`main`）實際切換到哪個 UI 迴圈之後要呼叫這個同步狀態，
    /// 這樣 `panel` 指令是否列入候選清單才會反映當下真正的畫面。
    pub fn set_current_ui(&mut self, ui: UiMode) {
        self.current_ui = ui;
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

    pub fn prompt(&self) -> String {
        match &self.mode {
            Mode::Root => "cng5> ".to_string(),
            Mode::InPlugin(name) => format!("cng5({name})> "),
            Mode::InPanel(name) => format!("cng5({name}/panel)> "),
        }
    }

    pub fn execute_line(&mut self, line: &str) -> Result<()> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
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
        let tokens = shell_words::split(line).context("指令解析失敗")?;
        let (cmd, args) = tokens.split_first().expect("已檢查過非空行");

        let top_level = self.next_word_candidates(&[]);
        let cmd = Self::resolve(cmd, &top_level)?;

        match (&self.mode, cmd.as_str()) {
            (Mode::Root, "help") => self.print_help(),
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
            (Mode::Root, "exit") => self.should_exit = true,
            // 已經在 root，`~` 沒事做，但仍然是一個合法指令（跟其他 mode 底下的
            // `~` 行為一致，不管在哪一層打都不會被當成「不認得的指令」）。
            (Mode::Root, "~") => {}
            (Mode::Root, _) => unreachable!("resolve 只會回傳 top_level 裡的字"),
            (Mode::InPlugin(_), "help") => self.print_help(),
            (Mode::InPlugin(_), "history") => self.print_history(),
            // `panel` 只會出現在 next_word_candidates 裡（因此才能被 resolve 選到）
            // 當 current_ui 是 Gui 的時候，見 usage_lines；CLI 模式下打這個字
            // 在 resolve 那一步就已經被當成不認得的指令擋掉了，不會走到這裡。
            (Mode::InPlugin(name), "panel") => self.mode = Mode::InPanel(name.clone()),
            (Mode::InPlugin(_), "exit") => self.mode = Mode::Root,
            // `~`：不管巢狀多深都直接跳回 root，跟逐層 `exit` 不同。
            (Mode::InPlugin(_), "~") => self.mode = Mode::Root,
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
            (Mode::InPanel(name), "exit") => self.mode = Mode::InPlugin(name.clone()),
            (Mode::InPanel(_), "~") => self.mode = Mode::Root,
            (Mode::InPanel(_), _) => unreachable!("resolve 只會回傳 usage_lines 裡的字"),
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
        }
    }

    /// 目前 mode 底下所有指令的用法字串（第一個字是指令名稱）。
    fn usage_lines(&self) -> Vec<&str> {
        match &self.mode {
            Mode::Root => vec![
                "help",
                "history",
                "plugin show",
                "plugin enter <name>",
                "mode cli",
                "mode gui",
                "exit",
                "~",
            ],
            Mode::InPlugin(name) => {
                let plugin = self
                    .active
                    .get(name)
                    .expect("mode 對應的 plugin 一定存在");
                let mut lines = vec!["help", "history"];
                lines.extend(plugin.commands());
                // panel 只有 GUI 畫面才有意義，CLI 底下不列入候選、也就打不出來。
                if self.current_ui == UiMode::Gui {
                    lines.push("panel");
                }
                lines.push("exit");
                lines.push("~");
                lines
            }
            Mode::InPanel(_) => vec![
                "help",
                "rect <x> <y> <width> <height>",
                "show",
                "hidden",
                "activate",
                "exit",
                "~",
            ],
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
        s.push_str("  history              列出之前執行過的指令\n");
        s.push_str("  !<n>                 重新執行 history 裡第 n 筆指令\n");
        s.push_str("  plugin show          列出可用的 plugin\n");
        s.push_str("  plugin enter <name>  進入 plugin mode\n");
        s.push_str("  mode cli             切換成一般的命令列畫面\n");
        s.push_str("  mode gui             切換成上下兩個 panel 的畫面\n");
        s.push_str("  exit                 離開程式\n");
        s.push_str("  ~                    跳回 root（不管在哪一層都直接回來）\n");
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

    fn plugin_list_text(&self) -> String {
        let mut names: Vec<&str> = self.active.keys().map(String::as_str).collect();
        names.sort();
        format!("可用的 plugin: {}\n", names.join(", "))
    }

    fn plugin_help_text(&self, name: &str) -> String {
        let plugin = self
            .active
            .get(name)
            .expect("mode 對應的 plugin 一定存在");

        let mut s = format!("可用指令 ({name}):\n");
        s.push_str("  help               顯示這個說明\n");
        s.push_str("  history            列出之前執行過的指令\n");
        s.push_str("  !<n>               重新執行 history 裡第 n 筆指令\n");
        if self.current_ui == UiMode::Gui {
            s.push_str("  panel              進入 panel 畫面\n");
        }
        s.push_str("  exit               回到 root\n");
        s.push_str("  ~                  跳回 root（不管在哪一層都直接回來）\n");
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
        s.push_str(&format!("  {:<30} 跳回 root（不管在哪一層都直接回來）\n", "~"));
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
