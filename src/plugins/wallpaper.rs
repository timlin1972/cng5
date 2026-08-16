use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

/// 桌布圖片放這裡（相對於程式執行時的工作目錄），跟 `ereader` 的
/// `storage/ebooks/` 同一套慣例——使用者已經可以透過 storage 網頁介面把
/// 圖片丟進來，這個 plugin 不需要另外做一套上傳功能，只負責「列出有哪些
/// 圖片、記住選了哪一張」。
pub(crate) const WALLPAPER_DIR: &str = "storage/wallpaper";

/// 目前選的桌布存放位置，故意跟 `WALLPAPER_DIR` 分開一個資料夾（不是直接
/// 寫進 `storage/wallpaper/` 裡面）：跟 `ereader` 的 `storage/ereader/
/// progress.json` 分開存一樣的考量——`WALLPAPER_DIR` 底下列出來的檔案要
/// 全部都是使用者自己放的圖片，狀態檔混在一起的話，`list_wallpapers`
/// 掃資料夾時要嘛得知道怎麼排除它，要嘛使用者自己瀏覽這個資料夾時會看到
/// 一個莫名其妙的檔案。
const PREFS_DIR: &str = "storage/wallpaper-prefs";
const PREFS_FILE: &str = "storage/wallpaper-prefs/selected.json";

/// 只認這些副檔名（大小寫不拘）是圖片——跟 `ereader` 的 `list_books` 只認
/// `.epub` 同樣道理，`storage/wallpaper/` 底下如果混進不相關的檔案（使用者
/// 手滑丟錯資料夾），不會被誤認成一張可選的桌布。
const IMAGE_EXTENSIONS: [&str; 6] = ["jpg", "jpeg", "png", "gif", "webp", "bmp"];

const MANUAL_TEXT: &str = "\
wallpaper：webui 桌面背景圖，從 storage/wallpaper/ 資料夾裡選一張。

範例：
  list            列出目前有的圖片（目前選的那張前面會標 *）
  select <檔名>   換成這一張
  rotate on       開啟自動輪播（webui 每 5 分鐘自動換下一張）
  rotate off      關閉自動輪播

圖片檔案要自己先用 storage plugin（網頁介面的 storage 分頁）上傳進
storage/wallpaper/ 資料夾，這個 plugin 不會自己抓/下載圖片。

webui 顯示時會維持圖片原始比例，不會變形，也不會露出底色——視窗比例跟圖片
比例對不上的部分，是圖片本身被裁掉（比較長的那一邊左右或上下各裁一半），
不是整張塞進去再補顏色墊底。

自動輪播的計時器在瀏覽器那邊跑（不是這個程式本身），只有 webui 開著的時候
才會真的換下一張，跟 sync plugin 之類「就算沒人在看也要一直跑」的背景工作
不一樣——桌布本來就只有畫面顯示著的時候才有意義。
";

fn is_image_file(name: &str) -> bool {
    let Some(ext) = Path::new(name).extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    IMAGE_EXTENSIONS.contains(&ext.as_str())
}

/// 檔名安全性檢查，跟 `ereader` 的 `safe_ebook_path` 同一套規則：只接受
/// 單一檔名，不能含路徑分隔符或是 `.`/`..`。
pub(crate) fn safe_wallpaper_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return None;
    }
    Some(Path::new(WALLPAPER_DIR).join(name))
}

/// `storage/wallpaper/` 底下目前有的圖片檔名，依檔名排序。
pub(crate) fn list_wallpapers() -> Vec<String> {
    let Ok(entries) = fs::read_dir(WALLPAPER_DIR) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| is_image_file(name))
        .collect();
    names.sort();
    names
}

#[derive(Default, Serialize, Deserialize)]
struct WallpaperPrefs {
    selected: Option<String>,
    /// 開著的話 webui 每 5 分鐘自動換下一張（見 `web.rs` 前端那段輪播
    /// 邏輯，計時器在瀏覽器端，不是這裡）。`#[serde(default)]`：舊版存的
    /// 設定檔沒有這個欄位，讀回來當作 `false`，不會因為多了新欄位就整份
    /// 解析失敗、回退成完全預設值（連 `selected` 都不見）。
    #[serde(default)]
    rotate: bool,
}

fn load_prefs() -> WallpaperPrefs {
    fs::read_to_string(PREFS_FILE).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn save_prefs(prefs: &WallpaperPrefs) -> Result<()> {
    fs::create_dir_all(PREFS_DIR).context("建立桌布設定目錄失敗")?;
    fs::write(PREFS_FILE, serde_json::to_string_pretty(prefs)?).context("儲存桌布設定失敗")
}

/// 目前選的桌布檔名，`None` 代表沒選過（webui 就不套用任何背景圖，維持
/// 原本純色背景）。選過的那張檔案如果後來被刪掉了（例如使用者跑去
/// storage 分頁手動刪除），這裡直接當作沒選過，不會讓 webui 一直嘗試載入
/// 一張不存在的圖片。
pub(crate) fn current_wallpaper() -> Option<String> {
    let selected = load_prefs().selected?;
    safe_wallpaper_path(&selected).filter(|path| path.is_file())?;
    Some(selected)
}

/// 換桌布：檔名必須是 `list_wallpapers()` 裡真的有的檔案，不能亂填一個不
/// 存在的名字進去（webui 那邊會直接拿這個名字組圖片網址，選了不存在的
/// 檔案只會讓桌面背景整個消失，不如一開始就擋掉）。先讀出目前存的設定再
/// 只改 `selected` 這一欄，不是整份用新的覆蓋——不然自動輪播開著的時候，
/// 輪播本身每次換下一張都會呼叫這個函式，順手就把 `rotate` 重設回
/// `false`，變成換一張就自動關掉輪播。
pub(crate) fn select_wallpaper(name: &str) -> Result<()> {
    let path = safe_wallpaper_path(name).context("不合法的檔名")?;
    if !path.is_file() {
        bail!("找不到這張桌布: {name}");
    }
    let mut prefs = load_prefs();
    prefs.selected = Some(name.to_string());
    save_prefs(&prefs)
}

/// 自動輪播目前是不是開著的。
pub(crate) fn rotate_enabled() -> bool {
    load_prefs().rotate
}

/// 開關自動輪播，同樣只改 `rotate` 這一欄，不動 `selected`。
pub(crate) fn set_rotate_enabled(enabled: bool) -> Result<()> {
    let mut prefs = load_prefs();
    prefs.rotate = enabled;
    save_prefs(&prefs)
}

fn list_text() -> String {
    let rotate_line = format!("自動輪播: {}", if rotate_enabled() { "開" } else { "關" });
    let names = list_wallpapers();
    if names.is_empty() {
        return format!("{rotate_line}\n(storage/wallpaper/ 底下還沒有任何圖片檔案)");
    }
    let current = current_wallpaper();
    let list = names
        .into_iter()
        .map(|name| {
            let marker = if current.as_deref() == Some(name.as_str()) { "* " } else { "  " };
            format!("{marker}{name}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{rotate_line}\n{list}")
}

pub struct WallpaperPlugin {
    #[allow(dead_code)]
    ctx: SharedContext,
}

impl WallpaperPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx }
    }
}

impl Plugin for WallpaperPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["list", "select <檔名>", "rotate <on|off>"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "list" => {
                out.push(&format!("{}\n", list_text()));
                Ok(())
            }
            "select" => {
                let Some(name) = args.first() else { bail!("用法: select <檔名>") };
                select_wallpaper(name)?;
                out.push(&format!("已選擇桌布: {name}\n"));
                Ok(())
            }
            "rotate" => {
                let enabled = match args.first().map(String::as_str) {
                    Some("on") => true,
                    Some("off") => false,
                    _ => bail!("用法: rotate <on|off>"),
                };
                set_rotate_enabled(enabled)?;
                out.push(&format!("自動輪播: {}\n", if enabled { "開" } else { "關" }));
                Ok(())
            }
            other => bail!("wallpaper 不認得指令: {other}"),
        }
    }

    fn panel_text(&self) -> Option<String> {
        Some(list_text())
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
    use std::sync::{Arc, Mutex};

    /// `WALLPAPER_DIR`/`PREFS_FILE` 是相對於目前工作目錄的相對路徑，測試
    /// 不能直接讓它落在真正的 `storage/`（會弄髒開發者自己機器上的資料，
    /// `cargo test` 平行跑測試時彼此也會互相踩到）——跟 `todo.rs`/
    /// `ereader.rs` 測試用的 `CwdGuard` 同一招：整個測試期間切到一個獨立的
    /// 暫存工作目錄，結束（含 panic）一定會切回來。
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
    fn safe_wallpaper_path_rejects_traversal_and_separators() {
        assert!(safe_wallpaper_path("").is_none());
        assert!(safe_wallpaper_path(".").is_none());
        assert!(safe_wallpaper_path("..").is_none());
        assert!(safe_wallpaper_path("a/b.jpg").is_none());
        assert!(safe_wallpaper_path("a\\b.jpg").is_none());
        assert_eq!(safe_wallpaper_path("beach.jpg"), Some(Path::new(WALLPAPER_DIR).join("beach.jpg")));
    }

    #[test]
    fn is_image_file_only_accepts_known_extensions_case_insensitive() {
        assert!(is_image_file("beach.jpg"));
        assert!(is_image_file("beach.JPEG"));
        assert!(is_image_file("beach.PNG"));
        assert!(!is_image_file("notes.txt"));
        assert!(!is_image_file("no-extension"));
    }

    #[test]
    fn list_and_select_round_trip_across_reload() {
        let dir = std::env::temp_dir().join("cng5-wallpaper-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(WALLPAPER_DIR)).expect("建立測試用暫存目錄失敗");
        let _guard = CwdGuard::enter(&dir);

        fs::write(Path::new(WALLPAPER_DIR).join("beach.jpg"), b"fake").unwrap();
        fs::write(Path::new(WALLPAPER_DIR).join("forest.png"), b"fake").unwrap();
        fs::write(Path::new(WALLPAPER_DIR).join("notes.txt"), b"not an image").unwrap();

        assert_eq!(list_wallpapers(), vec!["beach.jpg".to_string(), "forest.png".to_string()]);
        assert_eq!(current_wallpaper(), None);

        select_wallpaper("forest.png").unwrap();
        assert_eq!(current_wallpaper(), Some("forest.png".to_string()));
        assert!(list_text().contains("* forest.png"));
        assert!(list_text().contains("  beach.jpg"));

        assert!(select_wallpaper("missing.jpg").is_err());
        // 選失敗不會動到原本已經選好的那張。
        assert_eq!(current_wallpaper(), Some("forest.png".to_string()));

        // 選過的檔案被刪掉之後，`current_wallpaper` 要自動當作沒選過，不是
        // 一直回傳一個已經不存在的檔名。
        fs::remove_file(Path::new(WALLPAPER_DIR).join("forest.png")).unwrap();
        assert_eq!(current_wallpaper(), None);
    }

    #[test]
    fn select_wallpaper_preserves_rotate_flag_and_vice_versa() {
        let dir = std::env::temp_dir().join("cng5-wallpaper-rotate-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(WALLPAPER_DIR)).expect("建立測試用暫存目錄失敗");
        let _guard = CwdGuard::enter(&dir);

        fs::write(Path::new(WALLPAPER_DIR).join("beach.jpg"), b"fake").unwrap();
        fs::write(Path::new(WALLPAPER_DIR).join("forest.png"), b"fake").unwrap();

        assert!(!rotate_enabled());
        set_rotate_enabled(true).unwrap();
        assert!(rotate_enabled());

        // 換桌布（輪播每一輪都會呼叫這個）不能把 rotate 開關重設掉。
        select_wallpaper("beach.jpg").unwrap();
        assert!(rotate_enabled());
        select_wallpaper("forest.png").unwrap();
        assert!(rotate_enabled());

        // 反過來，關掉輪播也不能動到目前選的桌布。
        set_rotate_enabled(false).unwrap();
        assert!(!rotate_enabled());
        assert_eq!(current_wallpaper(), Some("forest.png".to_string()));
    }

    #[test]
    fn dispatch_unknown_command_errors() {
        let out = OutputBuffer::new();
        let mut plugin = WallpaperPlugin::new(ctx());
        assert!(plugin.dispatch("bogus", &[], &out).is_err());
    }

    #[test]
    fn dispatch_select_without_name_errors() {
        let out = OutputBuffer::new();
        let mut plugin = WallpaperPlugin::new(ctx());
        assert!(plugin.dispatch("select", &[], &out).is_err());
    }
}
