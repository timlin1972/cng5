use std::fs;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

/// 待辦清單存檔位置：放在 `storage/` 底下（跟 `notepad` 的
/// `storage/notepad/` 一樣），這樣既有的 `storage`/`sync` plugin 自動就能
/// 瀏覽/跨機器同步這份清單，不用另外寫一套同步機制。
const TODO_DIR: &str = "storage/todo";
const TODO_FILE: &str = "storage/todo/todo.json";

/// `manual` 指令的說明。
const MANUAL_TEXT: &str = "\
todo：簡單的待辦清單，新增/打勾/刪除都會立刻存到 storage/todo/todo.json。

範例：
  add 買牛奶      新增一筆待辦
  done 3          把 id 3 標成完成
  undone 3        把 id 3 標成還沒做
  remove 3        刪除 id 3
  list            列出目前所有待辦（跟 panel 顯示一樣）
";

#[derive(Clone, Serialize, Deserialize)]
struct TodoItem {
    id: u64,
    text: String,
    done: bool,
}

/// 存檔格式：`items` 之外還存 `next_id`，這樣刪掉幾筆之後新增的 id 還是
/// 一路往上加，不會跟還留著的舊 id 撞在一起。
#[derive(Default, Serialize, Deserialize)]
struct TodoFile {
    next_id: u64,
    items: Vec<TodoItem>,
}

/// `snapshot()`／web API 用的 JSON 結構，跟內部的 `TodoItem` 分開純粹是
/// 慣例一致（`weather.rs` 也是內部結構跟對外 JSON 結構分開），這裡兩者長得
/// 一樣，之後如果要在畫面上加只給前端用的欄位（不想存檔）就有地方放。
#[derive(Clone, Serialize)]
pub(crate) struct TodoItemJson {
    id: u64,
    text: String,
    done: bool,
}

/// 待辦清單不在 struct 裡快取，每次讀寫都直接開檔——`storage`/`sync`
/// plugin 隨時可能在背景把別台機器同步過來的 `todo.json` 蓋掉這個檔案，
/// 如果啟動時讀一次就存進 `self.items`，之後就會一直看著過期的舊資料，
/// 直到整個 plugin 被重新 `new()` 才會發現檔案已經變了。把檔案當成唯一的
/// 真相來源，才能讓 `list`／web 分頁即時反映其他裝置同步過來的異動。
pub struct TodoPlugin {
    #[allow(dead_code)]
    ctx: SharedContext,
}

impl TodoPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx }
    }

    /// 檔案不存在或內容壞掉都當成「還沒有任何待辦」，不是錯誤——跟
    /// `NotepadPlugin::load` 對缺檔的處理方式一致。
    fn load() -> TodoFile {
        fs::read_to_string(TODO_FILE).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
    }

    fn save(file: &TodoFile) -> Result<()> {
        fs::create_dir_all(TODO_DIR).context("建立 todo 目錄失敗")?;
        let text = serde_json::to_string_pretty(file).context("序列化待辦清單失敗")?;
        fs::write(TODO_FILE, text).context("儲存待辦清單失敗")
    }

    fn find_mut(items: &mut [TodoItem], id: u64) -> Option<&mut TodoItem> {
        items.iter_mut().find(|item| item.id == id)
    }

    /// `add`（CLI）／`add_item`（web）共用：內容去頭尾空白，空字串當錯誤。
    fn push_item(text: String) -> Result<u64> {
        let text = text.trim().to_string();
        if text.is_empty() {
            bail!("待辦內容不能空白");
        }
        let mut file = Self::load();
        let id = file.next_id;
        file.next_id += 1;
        file.items.push(TodoItem { id, text, done: false });
        Self::save(&file)?;
        Ok(id)
    }

    fn add(&mut self, args: &[String], out: &OutputBuffer) -> Result<()> {
        let id = Self::push_item(args.join(" "))?;
        out.push(&format!("todo 新增 #{id}\n"));
        Ok(())
    }

    fn set_done(&mut self, args: &[String], done: bool, out: &OutputBuffer) -> Result<()> {
        let id: u64 = args.first().context("需要接 id")?.parse().context("id 要是數字")?;
        let mut file = Self::load();
        let Some(item) = Self::find_mut(&mut file.items, id) else { bail!("找不到 id {id}") };
        item.done = done;
        Self::save(&file)?;
        out.push(&format!("todo #{id} 標成{}\n", if done { "完成" } else { "還沒做" }));
        Ok(())
    }

    fn remove(&mut self, args: &[String], out: &OutputBuffer) -> Result<()> {
        let id: u64 = args.first().context("remove 需要接 id")?.parse().context("id 要是數字")?;
        let mut file = Self::load();
        if !Self::remove_by_id(&mut file.items, id) {
            bail!("找不到 id {id}");
        }
        Self::save(&file)?;
        out.push(&format!("todo 刪除 #{id}\n"));
        Ok(())
    }

    fn remove_by_id(items: &mut Vec<TodoItem>, id: u64) -> bool {
        let before = items.len();
        items.retain(|item| item.id != id);
        items.len() != before
    }

    /// `list`（CLI）跟 `panel_text()`（GUI/web 的純文字 panel）共用的內容。
    fn text(&self) -> String {
        let file = Self::load();
        if file.items.is_empty() {
            return "（沒有待辦事項）".to_string();
        }
        file.items
            .iter()
            .map(|item| format!("[{}] #{} {}", if item.done { "x" } else { " " }, item.id, item.text))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `/api/todo/list` 用的結構化清單，順序跟存檔一樣（新增的排最後）。
    pub(crate) fn snapshot(&self) -> Vec<TodoItemJson> {
        Self::load().items.into_iter().map(|item| TodoItemJson { id: item.id, text: item.text, done: item.done }).collect()
    }

    pub(crate) fn add_item(&mut self, text: String) -> Result<()> {
        Self::push_item(text)?;
        Ok(())
    }

    pub(crate) fn toggle_item(&mut self, id: u64) -> Result<()> {
        let mut file = Self::load();
        let Some(item) = Self::find_mut(&mut file.items, id) else { bail!("找不到 id {id}") };
        item.done = !item.done;
        Self::save(&file)
    }

    pub(crate) fn remove_item(&mut self, id: u64) -> Result<()> {
        let mut file = Self::load();
        if !Self::remove_by_id(&mut file.items, id) {
            bail!("找不到 id {id}");
        }
        Self::save(&file)
    }
}

impl Plugin for TodoPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["add <text>", "done <id>", "undone <id>", "remove <id>", "list"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "add" => self.add(args, out),
            "done" => self.set_done(args, true, out),
            "undone" => self.set_done(args, false, out),
            "remove" => self.remove(args, out),
            "list" => {
                out.push(&format!("{}\n", self.text()));
                Ok(())
            }
            other => bail!("todo 不認得指令: {other}"),
        }
    }

    fn panel_text(&self) -> Option<String> {
        Some(self.text())
    }

    fn manual_text(&self) -> &'static str {
        MANUAL_TEXT
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::ContextInner;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    /// `TODO_FILE` 是相對於目前工作目錄的相對路徑，測試不能直接讓它落在
    /// 真正的 `storage/todo/`（會弄髒開發者自己機器上的待辦清單，`cargo
    /// test` 平行跑測試時彼此也會互相踩到）——跟 `storage.rs` 測試用的
    /// `CwdGuard` 同一招：整個測試期間切到一個獨立的暫存工作目錄，結束
    /// （含 panic）一定會切回來。
    static CWD_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct CwdGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        original: PathBuf,
    }

    impl CwdGuard {
        fn enter(workdir: &Path) -> Self {
            let lock = CWD_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let original = std::env::current_dir().expect("讀取目前工作目錄失敗");
            std::env::set_current_dir(workdir).expect("切換測試用工作目錄失敗");
            Self { _lock: lock, original }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    fn ctx() -> SharedContext {
        Arc::new(Mutex::new(ContextInner::default()))
    }

    #[test]
    fn add_done_remove_persist_across_reload() {
        let dir = std::env::temp_dir().join("cng5-todo-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建立測試用暫存目錄失敗");
        let _guard = CwdGuard::enter(&dir);

        let out = OutputBuffer::new();
        let mut plugin = TodoPlugin::new(ctx());
        assert_eq!(plugin.text(), "（沒有待辦事項）");

        plugin.dispatch("add", &["買牛奶".to_string()], &out).unwrap();
        plugin.dispatch("add", &["倒垃圾".to_string()], &out).unwrap();
        let snap = plugin.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].text, "買牛奶");
        assert!(!snap[0].done);

        plugin.dispatch("done", &[snap[0].id.to_string()], &out).unwrap();
        assert!(plugin.snapshot()[0].done);

        plugin.dispatch("remove", &[snap[1].id.to_string()], &out).unwrap();
        assert_eq!(plugin.snapshot().len(), 1);

        // 重新 `new()` 一次，確認上面的 add/done/remove 真的存到 `TODO_FILE`
        // 了，不是只留在記憶體裡（CLI 的 `remove` 一度忘了呼叫 `save()`，
        // 靠這個檢查抓出來）。
        let reloaded = TodoPlugin::new(ctx());
        let snap = reloaded.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].text, "買牛奶");
        assert!(snap[0].done);
    }

    #[test]
    fn unknown_id_is_an_error() {
        let dir = std::env::temp_dir().join("cng5-todo-test-unknown-id");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建立測試用暫存目錄失敗");
        let _guard = CwdGuard::enter(&dir);

        let out = OutputBuffer::new();
        let mut plugin = TodoPlugin::new(ctx());
        assert!(plugin.dispatch("done", &["999".to_string()], &out).is_err());
        assert!(plugin.dispatch("remove", &["999".to_string()], &out).is_err());
    }

    #[test]
    fn external_file_change_is_picked_up_without_reconstructing_plugin() {
        let dir = std::env::temp_dir().join("cng5-todo-test-external-sync");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建立測試用暫存目錄失敗");
        let _guard = CwdGuard::enter(&dir);

        let plugin = TodoPlugin::new(ctx());
        assert_eq!(plugin.snapshot().len(), 0);

        // 模擬 `sync` plugin 在背景把別台機器同步過來的 todo.json 直接蓋掉
        // 這個檔案，同一個 plugin 實例接下來的讀取要能看到新內容，不能還
        // 停留在建構時讀到的舊狀態。
        fs::create_dir_all(TODO_DIR).unwrap();
        let synced = TodoFile {
            next_id: 5,
            items: vec![TodoItem { id: 4, text: "別台機器加的".to_string(), done: false }],
        };
        fs::write(TODO_FILE, serde_json::to_string_pretty(&synced).unwrap()).unwrap();

        let snap = plugin.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].text, "別台機器加的");
    }
}
