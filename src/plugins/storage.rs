use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

/// `storage` plugin 管理的檔案都放在這個資料夾底下（相對於程式執行時的工作
/// 目錄），跟 `MUSIC_DIR`/`NOTEPAD_DIR` 同樣的命名慣例。跟那兩個資料夾不同的
/// 是，這個資料夾底下允許任意深度的巢狀子資料夾。
pub(crate) const STORAGE_DIR: &str = "storage";

/// 一個檔案或資料夾的中繼資料，`storage list` 指令跟 `/api/storage/list` 都用
/// 這個格式回傳。
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub(crate) struct StorageEntry {
    pub(crate) name: String,
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
    pub(crate) modified: u64,
}

/// 驗證 `relative`（使用者輸入、或 HTTP 請求帶進來的相對路徑字串）是不是真的
/// 只會落在 `root` 底下，是的話回傳組好的實際路徑，不是就回傳 `None`——不管
/// 呼叫端是 CLI 指令、還是 Web API，都要過這一關才能碰檔案系統，這是唯一的
/// 安全把關點。
///
/// 兩層防護：
/// 1. 逐段檢查 `relative` 的每個路徑片段，只接受 `Component::Normal`（單純的
///    目錄/檔案名稱）——這樣就同時擋掉 `..`、`.`、絕對路徑起始的
///    `RootDir`/`Prefix`，而且用 `std::path::Component` 逐段解析，Windows 的
///    `\` 分隔符也能正確處理，不用自己手動判斷字串裡有沒有斜線。
/// 2. 組出完整路徑後，往上找「目前已經真的存在」的最深一層祖先目錄（candidate
///    本身可能還不存在，例如 mkdir 的目標、上傳/搬移的目的地），對這層祖先做
///    canonicalize，確認它仍然在 `root` 的 canonical 路徑底下——這一步是為了
///    擋掉「路徑字串看起來在 root 裡面、但其中某一段其實是指到 root 外面的
///    symlink」這種繞過純字串檢查的手法。
pub(crate) fn safe_storage_path(root: &Path, relative: &str) -> Option<PathBuf> {
    if relative.is_empty() {
        return None;
    }
    let rel_path = Path::new(relative);
    if rel_path.is_absolute() {
        return None;
    }
    for component in rel_path.components() {
        match component {
            Component::Normal(_) => {}
            _ => return None,
        }
    }
    let candidate = root.join(rel_path);

    let mut existing_ancestor = candidate.as_path();
    while !existing_ancestor.exists() {
        existing_ancestor = existing_ancestor.parent()?;
    }
    let root_canon = fs::canonicalize(root).ok()?;
    let ancestor_canon = fs::canonicalize(existing_ancestor).ok()?;
    if !ancestor_canon.starts_with(&root_canon) {
        return None;
    }
    Some(candidate)
}

/// `dir` 底下的所有項目（檔案跟資料夾都列出來，只列這一層，不遞迴）。資料夾
/// 排在前面、各自再依名稱排序——這是這個功能自己的顯示慣例，不是規格硬性要求，
/// 純粹讓瀏覽時資料夾跟檔案不會混在一起、比較好找。
pub(crate) fn list_dir(dir: &Path) -> Result<Vec<StorageEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("讀取資料夾失敗: {}", dir.display()))? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        entries.push(StorageEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: meta.is_dir(),
            size: meta.len(),
            modified,
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(entries)
}

/// 建立資料夾；已經有同名的檔案或資料夾就報錯，不會覆蓋或合併。
pub(crate) fn make_dir(path: &Path) -> Result<()> {
    if path.exists() {
        bail!("已經有同名的檔案或資料夾: {}", path.display());
    }
    fs::create_dir(path).with_context(|| format!("建立資料夾失敗: {}", path.display()))
}

/// 刪除 `path`：是檔案就直接刪；是資料夾的話，非空且沒有 `recursive` 就報錯
/// 拒絕，避免不小心整個資料夾被刪掉。
pub(crate) fn remove(path: &Path, recursive: bool) -> Result<()> {
    let meta = fs::metadata(path).with_context(|| format!("找不到: {}", path.display()))?;
    if meta.is_dir() {
        let mut children = fs::read_dir(path)?;
        if children.next().is_some() && !recursive {
            bail!("資料夾非空，需要加上 --recursive 才能刪除: {}", path.display());
        }
        fs::remove_dir_all(path).with_context(|| format!("刪除資料夾失敗: {}", path.display()))
    } else {
        fs::remove_file(path).with_context(|| format!("刪除檔案失敗: {}", path.display()))
    }
}

/// 重新命名/搬移 `from` 到 `to`：`to` 已經是檔案就直接覆蓋（`std::fs::rename`
/// 在 Unix 上跟 Windows 上都是「取代目的地」的語意），但 `to` 已經是資料夾就
/// 拒絕——不做資料夾合併，語意含糊，寧可讓使用者自己先處理。
pub(crate) fn rename_path(from: &Path, to: &Path) -> Result<()> {
    if !from.exists() {
        bail!("來源不存在: {}", from.display());
    }
    if to.exists() && to.is_dir() {
        bail!("目的地已經是資料夾，不支援覆蓋資料夾: {}", to.display());
    }
    fs::rename(from, to).with_context(|| format!("搬移/重新命名失敗: {} -> {}", from.display(), to.display()))
}

const MANUAL_TEXT: &str = "\
storage：管理本機這台裝置的儲存（不是跨裝置操作，那是 files plugin 的範圍）。
根目錄底下可以有任意深度的子資料夾。

指令：
  ls [path]              列出目前位置（或指定 path，不會改變目前位置）底下的內容
  cd <path>              切換目前位置；cd .. 回上一層、cd / 回根目錄
  mkdir <name>           在目前位置建立子資料夾
  rm <name> [--recursive] 刪除檔案；刪非空資料夾要加 --recursive，不然會報錯拒絕
  mv <old> <new>         重新命名，或搬到目前位置下的另一個名稱；目的地是檔案就覆蓋，
                         是資料夾就拒絕

沒有 upload/download 指令：這個 plugin 執行的機器本身就是儲存所在地，直接用作業系統
的檔案總管操作 storage/ 資料夾即可；上傳/下載是給瀏覽器（Web UI）用的。

cd .. 只支援單獨這樣打，不支援像 sub/.. 這種複合寫法。
";

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[0])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

fn render_listing(dir: &Path) -> Result<String> {
    let entries = list_dir(dir)?;
    let relative = dir.strip_prefix(STORAGE_DIR).unwrap_or(Path::new(""));
    let mut text = format!("/{}\n", relative.display());
    if entries.is_empty() {
        text.push_str("(空)\n");
        return Ok(text);
    }
    for entry in &entries {
        let kind = if entry.is_dir { "📁" } else { "📄" };
        let size = if entry.is_dir { "—".to_string() } else { format_size(entry.size) };
        text.push_str(&format!("{kind} {:<30} {:>8}\n", entry.name, size));
    }
    Ok(text)
}

/// 記住「目前瀏覽到哪個子路徑」，相對 `STORAGE_DIR` 根目錄；空路徑代表在
/// 根目錄。沒有其他狀態——每個指令都直接讀寫真正的檔案系統，不快取任何內容。
pub struct StoragePlugin {
    current: PathBuf,
}

impl StoragePlugin {
    pub fn new(_ctx: SharedContext) -> Self {
        let _ = fs::create_dir_all(STORAGE_DIR);
        Self { current: PathBuf::new() }
    }

    /// 目前位置對應的真正檔案系統路徑（`STORAGE_DIR` 加上 `self.current`）。
    fn current_real_path(&self) -> PathBuf {
        if self.current.as_os_str().is_empty() {
            Path::new(STORAGE_DIR).to_path_buf()
        } else {
            Path::new(STORAGE_DIR).join(&self.current)
        }
    }

    /// 把一個 CLI 參數（可能是普通名稱、也可能是 `..` 或 `/`）解讀成相對目前
    /// 位置的真正檔案系統路徑，並且驗證過 `safe_storage_path`。`..`/`/` 是
    /// 這個函式自己特殊處理的兩個字面值，不會被當成一般路徑片段送進
    /// `safe_storage_path`（那邊會拒絕任何 `..` 片段）——這樣「回上一層」的
    /// 導覽功能，跟「拒絕路徑裡帶 `..`」的安全檢查不會互相衝突。
    fn resolve_relative(&self, arg: &str) -> Result<PathBuf> {
        let combined_relative: String = if arg == "/" {
            String::new()
        } else if arg == ".." {
            self.current.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
        } else if self.current.as_os_str().is_empty() {
            arg.to_string()
        } else {
            format!("{}/{}", self.current.display(), arg)
        };
        if combined_relative.is_empty() {
            return Ok(Path::new(STORAGE_DIR).to_path_buf());
        }
        safe_storage_path(Path::new(STORAGE_DIR), &combined_relative)
            .with_context(|| format!("不合法的路徑: {arg}"))
    }
}

impl Plugin for StoragePlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["ls [path]", "cd <path>", "mkdir <name>", "rm <name> [--recursive]", "mv <old> <new>"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "ls" => {
                let target = match args.first() {
                    Some(a) => self.resolve_relative(a)?,
                    None => self.current_real_path(),
                };
                out.push(&render_listing(&target)?);
                Ok(())
            }
            "cd" => {
                let arg = args.first().context("cd 需要接路徑")?;
                let target = self.resolve_relative(arg)?;
                if !target.is_dir() {
                    bail!("不是資料夾: {arg}");
                }
                self.current = target.strip_prefix(STORAGE_DIR).unwrap_or(Path::new("")).to_path_buf();
                out.push(&format!("目前位置: /{}\n", self.current.display()));
                Ok(())
            }
            "mkdir" => {
                let name = args.first().context("mkdir 需要接資料夾名稱")?;
                let target = self.resolve_relative(name)?;
                make_dir(&target)?;
                out.push(&format!("已建立: {name}\n"));
                Ok(())
            }
            "rm" => {
                let name = args.first().context("rm 需要接檔案或資料夾名稱")?;
                let recursive = args.iter().any(|a| a == "--recursive");
                let target = self.resolve_relative(name)?;
                remove(&target, recursive)?;
                out.push(&format!("已刪除: {name}\n"));
                Ok(())
            }
            "mv" => {
                let old = args.first().context("mv 需要接來源名稱")?;
                let new = args.get(1).context("mv 需要接目的地名稱")?;
                let from = self.resolve_relative(old)?;
                let to = self.resolve_relative(new)?;
                rename_path(&from, &to)?;
                out.push(&format!("已搬移: {old} -> {new}\n"));
                Ok(())
            }
            other => bail!("storage 不認得指令: {other}"),
        }
    }

    fn panel_text(&self) -> Option<String> {
        render_listing(&self.current_real_path()).ok()
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
    use std::fs;

    /// 每個測試各自用一個獨立命名的暫存資料夾當 root，避免 cargo test 預設多執行緒
    /// 平行跑測試時互相踩到彼此的檔案——不能直接用真正的 `STORAGE_DIR`（那是
    /// 相對於工作目錄的相對路徑，多個測試同時寫入會互相干擾）。先清掉舊的殘留
    /// （上一次測試留下的），再建立乾淨的資料夾。
    fn test_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cng5-storage-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建立測試用暫存根目錄失敗");
        dir
    }

    #[test]
    fn nested_path_under_root_is_accepted() {
        let root = test_root("nested-ok");
        fs::create_dir_all(root.join("photos")).unwrap();
        let result = safe_storage_path(&root, "photos/beach.jpg");
        assert_eq!(result, Some(root.join("photos").join("beach.jpg")));
    }

    #[test]
    fn nonexistent_nested_path_still_accepted_for_new_files() {
        // mkdir/upload 的目的地此刻還不存在很正常（例如要新建的資料夾本身），
        // 只要路徑本身沒有 `..`、父層鏈一路往上追到 root 都合法，就該放行，
        // 不能因為「東西還不存在」就一律拒絕。
        let root = test_root("nonexistent-nested-ok");
        let result = safe_storage_path(&root, "new-folder/new-file.txt");
        assert_eq!(result, Some(root.join("new-folder").join("new-file.txt")));
    }

    #[test]
    fn parent_dir_component_rejected() {
        let root = test_root("parent-dir-rejected");
        assert_eq!(safe_storage_path(&root, "../secrets.txt"), None);
        assert_eq!(safe_storage_path(&root, "photos/../../secrets.txt"), None);
        assert_eq!(safe_storage_path(&root, ".."), None);
    }

    #[test]
    fn absolute_path_rejected() {
        let root = test_root("absolute-rejected");
        assert_eq!(safe_storage_path(&root, "/etc/passwd"), None);
    }

    #[test]
    fn empty_path_rejected() {
        let root = test_root("empty-rejected");
        assert_eq!(safe_storage_path(&root, ""), None);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_root_is_rejected() {
        use std::os::unix::fs::symlink;
        let root = test_root("symlink-escape-rejected");
        let outside = std::env::temp_dir().join("cng5-storage-test-symlink-escape-target");
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"top secret").unwrap();
        symlink(&outside, root.join("escape")).expect("建立測試用 symlink 失敗");
        // 純字串層面「escape/secret.txt」完全合法（沒有 `..`），但 `escape`
        // 這個 symlink 實際指向 root 外面，canonicalize 檢查要抓到這個。
        assert_eq!(safe_storage_path(&root, "escape/secret.txt"), None);
    }

    #[test]
    fn list_dir_reports_files_and_folders_with_dirs_first() {
        let root = test_root("list-dir");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("b.txt"), b"hello").unwrap();
        fs::write(root.join("a.txt"), b"hi").unwrap();
        let entries = list_dir(&root).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["sub", "a.txt", "b.txt"]);
        assert!(entries[0].is_dir);
        assert!(!entries[1].is_dir);
        assert_eq!(entries[2].size, 5); // "hello" 的位元組數
    }

    #[test]
    fn make_dir_creates_new_folder_and_rejects_existing_name() {
        let root = test_root("mkdir");
        make_dir(&root.join("new")).unwrap();
        assert!(root.join("new").is_dir());
        assert!(make_dir(&root.join("new")).is_err());
    }

    #[test]
    fn remove_requires_recursive_flag_for_nonempty_dir() {
        let root = test_root("remove");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/file.txt"), b"x").unwrap();
        assert!(remove(&root.join("sub"), false).is_err());
        assert!(root.join("sub").exists());
        remove(&root.join("sub"), true).unwrap();
        assert!(!root.join("sub").exists());
    }

    #[test]
    fn remove_file_ignores_recursive_flag() {
        let root = test_root("remove-file");
        fs::write(root.join("f.txt"), b"x").unwrap();
        remove(&root.join("f.txt"), false).unwrap();
        assert!(!root.join("f.txt").exists());
    }

    #[test]
    fn rename_overwrites_existing_file_but_rejects_existing_dir_target() {
        let root = test_root("rename");
        fs::write(root.join("old.txt"), b"new content").unwrap();
        fs::write(root.join("existing.txt"), b"will be replaced").unwrap();
        rename_path(&root.join("old.txt"), &root.join("existing.txt")).unwrap();
        assert_eq!(fs::read_to_string(root.join("existing.txt")).unwrap(), "new content");

        fs::write(root.join("another.txt"), b"x").unwrap();
        fs::create_dir_all(root.join("target-dir")).unwrap();
        assert!(rename_path(&root.join("another.txt"), &root.join("target-dir")).is_err());
    }

    #[test]
    fn rename_missing_source_errors() {
        let root = test_root("rename-missing-source");
        assert!(rename_path(&root.join("nope.txt"), &root.join("dest.txt")).is_err());
    }

    #[test]
    fn ls_cd_mkdir_rm_mv_round_trip() {
        // 這個測試操作真正的 `StoragePlugin`，但要讓它在一個獨立的暫存目錄
        // 裡運作，不能真的去動相對於工作目錄的 `storage/`（那會跟其他平行
        // 跑的測試/正式使用互相干擾）——用 `std::env::set_current_dir` 切
        // 到一個乾淨的暫存目錄，plugin 內部用的相對路徑 `STORAGE_DIR` 就會
        // 落在這個暫存目錄底下。`cargo test` 預設多執行緒平行跑測試，改變
        // 工作目錄是 process 全域的狀態，所以這個測試不能跟其他也會
        // `set_current_dir` 的測試同時執行；這個專案目前沒有其他測試會動
        // 工作目錄，所以先接受這個限制，不用額外做執行緒鎖。
        let workdir = std::env::temp_dir().join("cng5-storage-plugin-test-workdir");
        let _ = fs::remove_dir_all(&workdir);
        fs::create_dir_all(&workdir).unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(&workdir).unwrap();

        let ctx: SharedContext = std::sync::Arc::new(std::sync::Mutex::new(crate::plugin::ContextInner::default()));
        let mut plugin = StoragePlugin::new(ctx);
        let out = OutputBuffer::new();

        plugin.dispatch("mkdir", &["photos".to_string()], &out).unwrap();
        plugin.dispatch("cd", &["photos".to_string()], &out).unwrap();
        plugin.dispatch("mkdir", &["2026".to_string()], &out).unwrap();
        plugin.dispatch("cd", &["2026".to_string()], &out).unwrap();

        fs::write(Path::new(STORAGE_DIR).join("photos/2026/beach.jpg"), b"fake jpg bytes").unwrap();
        let listing = plugin.panel_text().unwrap();
        assert!(listing.contains("/photos/2026"));
        assert!(listing.contains("beach.jpg"));

        plugin.dispatch("cd", &["..".to_string()], &out).unwrap();
        let listing = plugin.panel_text().unwrap();
        assert!(listing.contains("/photos"));
        assert!(listing.contains("2026"));

        plugin.dispatch("mv", &["2026".to_string(), "2026-renamed".to_string()], &out).unwrap();
        assert!(Path::new(STORAGE_DIR).join("photos/2026-renamed/beach.jpg").exists());

        let err = plugin.dispatch("rm", &["2026-renamed".to_string()], &out).unwrap_err();
        assert!(err.to_string().contains("--recursive"));
        plugin.dispatch("rm", &["2026-renamed".to_string(), "--recursive".to_string()], &out).unwrap();
        assert!(!Path::new(STORAGE_DIR).join("photos/2026-renamed").exists());

        std::env::set_current_dir(original).unwrap();
        let _ = fs::remove_dir_all(&workdir);
    }

    #[test]
    fn dispatch_unknown_command_errors() {
        let ctx: SharedContext = std::sync::Arc::new(std::sync::Mutex::new(crate::plugin::ContextInner::default()));
        let workdir = std::env::temp_dir().join("cng5-storage-plugin-test-unknown-cmd");
        let _ = fs::remove_dir_all(&workdir);
        fs::create_dir_all(&workdir).unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(&workdir).unwrap();

        let mut plugin = StoragePlugin::new(ctx);
        let out = OutputBuffer::new();
        let err = plugin.dispatch("frobnicate", &[], &out).unwrap_err();
        assert!(err.to_string().contains("storage 不認得指令"));

        std::env::set_current_dir(original).unwrap();
        let _ = fs::remove_dir_all(&workdir);
    }

    #[test]
    fn commands_list_matches_documented_syntax() {
        let ctx: SharedContext = std::sync::Arc::new(std::sync::Mutex::new(crate::plugin::ContextInner::default()));
        let plugin = StoragePlugin::new(ctx);
        assert_eq!(
            plugin.commands(),
            &["ls [path]", "cd <path>", "mkdir <name>", "rm <name> [--recursive]", "mv <old> <new>"]
        );
    }
}
