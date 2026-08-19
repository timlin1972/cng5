use std::fs;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{bail, Context, Result};
use data_encoding::BASE64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

/// `storage` plugin 管理的檔案都放在這個資料夾底下（相對於程式執行時的工作
/// 目錄），跟 `MUSIC_DIR` 同樣的命名慣例。跟那個資料夾不同的是，這個資料夾
/// 底下允許任意深度的巢狀子資料夾——`NOTEPAD_DIR`（`storage/notepad`）就是
/// 巢狀在這裡面的其中一個子資料夾。
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

/// 一個檔案或資料夾在同步演算法裡的狀態：`path` 是相對於被走訪的根目錄、用
/// `/` 分隔的相對路徑（不管實際作業系統的分隔符是什麼，統一轉成 `/`，這樣
/// 兩台不同作業系統的機器比對路徑字串時才不會因為分隔符不同而誤判成不同路
/// 徑）。`hash` 資料夾一律是 `None`，檔案才有 `Some(<64 字元小寫十六進位
/// SHA-256>)`。
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub(crate) struct SyncEntry {
    pub(crate) path: String,
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
    pub(crate) modified: u64,
    pub(crate) hash: Option<String>,
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
///
/// 已知的理論限制：如果 `storage/` 底下已經有一個「目標不存在」的 dangling
/// symlink，`existing_ancestor` 的判斷會停在這個 symlink 本身還存在的那一層
/// （`Path::exists()` 對 dangling symlink 回傳 false，所以會繼續往上一層找），
/// canonicalize 檢查因此不會真的追蹤這個 symlink 實際指到哪裡；之後如果真的對
/// 那個路徑寫入，會透過 symlink 寫到 root 外面。這個功能本身完全不會建立
/// symlink（`make_dir` 只建普通資料夾、上傳只寫普通檔案），所以要利用這個限制
/// 得先有辦法直接在檔案系統裡塞一個 dangling symlink 進 `storage/`——不是這個
/// API 本身能做到的攻擊路徑，因此接受這個取捨，不用 `symlink_metadata` 另外
/// 逐層特殊處理。
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

/// 遞迴走訪 `root` 底下所有檔案/資料夾，攤平成一份清單，每個檔案都算好
/// SHA-256（同步演算法拿這份清單去跟對方、跟 baseline 比對）。每次同步都
/// 重新算一次，不做「只在懷疑衝突時才算 hash」這種提前優化。
pub(crate) fn walk_with_hashes(root: &Path) -> Result<Vec<SyncEntry>> {
    let mut out = Vec::new();
    walk_with_hashes_inner(root, root, &mut out)?;
    Ok(out)
}

fn walk_with_hashes_inner(root: &Path, dir: &Path, out: &mut Vec<SyncEntry>) -> Result<()> {
    for entry in list_dir(dir)? {
        let full_path = dir.join(&entry.name);
        let relative = full_path.strip_prefix(root).unwrap_or(&full_path);
        let path_str = relative.to_string_lossy().replace('\\', "/");
        if entry.is_dir {
            out.push(SyncEntry { path: path_str, is_dir: true, size: 0, modified: entry.modified, hash: None });
            walk_with_hashes_inner(root, &full_path, out)?;
        } else {
            let hash = hash_file(&full_path)?;
            out.push(SyncEntry {
                path: path_str,
                is_dir: false,
                size: entry.size,
                modified: entry.modified,
                hash: Some(hash),
            });
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    let data = fs::read(path).with_context(|| format!("讀取檔案失敗: {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(format!("{:x}", hasher.finalize()))
}

/// 遞迴走訪 `dir` 底下所有檔案，收集檔名裡含有 `needle` 的路徑——只找
/// 檔名，不算 hash：`walk_with_hashes` 那份是同步比對用的，每個檔案都要
/// 讀整個內容算 SHA-256，這裡單純找檔名，檔案可能很大（電子書/音樂/
/// 桌布），沒必要付那個成本。
fn find_files_containing(dir: &Path, needle: &str, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in list_dir(dir)? {
        let full_path = dir.join(&entry.name);
        if entry.is_dir {
            find_files_containing(&full_path, needle, out)?;
        } else if entry.name.contains(needle) {
            out.push(full_path);
        }
    }
    Ok(())
}

/// `sync` plugin 偵測到同一個檔案在兩台機器上有不同修改時，會把其中一份
/// 改名成「<原檔名> (衝突自 <裝置>，<日期>)」保留下來，不會自動選一份丟
/// 掉（見 `sync.rs` 的說明）——使用者自己看過、確認不需要留著之後，才
/// 透過這個功能一次清掉所有這種衝突檔案，不用一個個資料夾翻找。單一
/// 檔案刪除失敗（例如權限問題）不會擋住其他檔案繼續刪，回傳值是「實際
/// 刪掉幾個」，不是「找到幾個」。
pub(crate) fn remove_conflict_files(root: &Path) -> Result<usize> {
    let mut files = Vec::new();
    find_files_containing(root, "衝突自", &mut files)?;
    let removed = files.iter().filter(|path| remove(path, false).is_ok()).count();
    Ok(removed)
}

/// 從 `path` 這個檔案的 `offset` 開始讀最多 `FILE_CHUNK_SIZE`（見
/// `crate::plugin::FILE_CHUNK_SIZE`）個位元組，回傳 base64 編碼——給跨 domain
/// 的 `RemoteRequest::StorageFilePull` 處理用（`global.rs` 的
/// `build_remote_reply`），單則 MQTT 訊息塞不下整個檔案，一次只回一個 chunk。
pub(crate) fn read_chunk(path: &Path, offset: u64) -> Result<String> {
    let data = fs::read(path).with_context(|| format!("讀取檔案失敗: {}", path.display()))?;
    let start = (offset as usize).min(data.len());
    let end = (start + crate::plugin::FILE_CHUNK_SIZE).min(data.len());
    Ok(BASE64.encode(&data[start..end]))
}

/// 把 base64 編碼的 `data_b64` 解碼後寫進 `path` 的 `offset` 位置；`offset == 0`
/// 那一次順便建立（或清空）檔案，並確保上層資料夾存在——給跨 domain 的
/// `RemoteRequest::StorageFilePush` 處理用，做法跟 `web.rs` 既有的
/// `files_upload`（同網域版本）一樣是「每個 chunk 各自帶自己的 offset，seek
/// 到對的位置寫入」。
pub(crate) fn write_chunk(path: &Path, offset: u64, data_b64: &str) -> Result<()> {
    let bytes = BASE64.decode(data_b64.as_bytes()).context("chunk 不是合法的 base64")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("建立資料夾失敗: {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(offset == 0)
        .open(path)
        .with_context(|| format!("開啟檔案失敗: {}", path.display()))?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&bytes)?;
    Ok(())
}

/// 分頁工具，給同網域的 `/api/storage/sync-manifest`（Task 5）跟跨 domain 的
/// `RemoteRequest::StorageManifest` 處理（Task 4）共用，兩邊都需要把
/// `walk_with_hashes` 算出來的完整清單，依照跟現有 `files`/`global` 的
/// `FileList` 分頁一樣的位元組預算切成一頁一頁回傳，公開 MQTT broker 對單則
/// 訊息大小有限制，不能整份塞進一則回覆。
pub(crate) fn paginate_sync_entries(all: &[SyncEntry], offset: usize) -> Vec<SyncEntry> {
    const PAGE_BUDGET: usize = 4 * 1024;
    let mut page = Vec::new();
    let mut size = 0usize;
    for item in all.iter().skip(offset) {
        let item_size = item.path.len() + item.hash.as_ref().map(|h| h.len()).unwrap_or(0) + 32;
        if !page.is_empty() && size + item_size > PAGE_BUDGET {
            break;
        }
        size += item_size;
        page.push(item.clone());
    }
    page
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

    static CWD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 在同一個 process 裡改目前工作目錄是全域狀態：測試結束時（不管是正常結束
    /// 還是中途 panic）都要換回原本的目錄，而且不能跟其他也會動工作目錄的測試
    /// 同時執行——這個 guard 用一個共用的 `Mutex` 序列化這幾個測試，`Drop` 保證
    /// 離開作用域（含 panic 展開）一定會換回原本的工作目錄、釋放鎖。`lock()`
    /// 用 `unwrap_or_else` 接住 poisoned mutex（前一個測試如果真的 panic 了，
    /// mutex 會變成 poisoned，這裡選擇繼續拿到鎖而不是讓後面的測試跟著整個
    /// panic，畢竟這個鎖本來就是「同時只能有一個測試在動全域工作目錄」這件事的
    /// 保護，不是在保護什麼跨測試共享的資料正確性）。
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
        // `set_current_dir` 的測試（`dispatch_unknown_command_errors`）同時
        // 執行——用 `CwdGuard` 序列化，並且保證 panic 時也會換回原本的工作
        // 目錄。
        let workdir = std::env::temp_dir().join("cng5-storage-plugin-test-workdir");
        let _ = fs::remove_dir_all(&workdir);
        fs::create_dir_all(&workdir).unwrap();
        let _cwd_guard = CwdGuard::enter(&workdir);

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

        let _ = fs::remove_dir_all(&workdir);
    }

    #[test]
    fn dispatch_unknown_command_errors() {
        let ctx: SharedContext = std::sync::Arc::new(std::sync::Mutex::new(crate::plugin::ContextInner::default()));
        let workdir = std::env::temp_dir().join("cng5-storage-plugin-test-unknown-cmd");
        let _ = fs::remove_dir_all(&workdir);
        fs::create_dir_all(&workdir).unwrap();
        let _cwd_guard = CwdGuard::enter(&workdir);

        let mut plugin = StoragePlugin::new(ctx);
        let out = OutputBuffer::new();
        let err = plugin.dispatch("frobnicate", &[], &out).unwrap_err();
        assert!(err.to_string().contains("storage 不認得指令"));

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

    #[test]
    fn walk_with_hashes_covers_nested_files_and_dirs() {
        let root = test_root("walk-nested");
        fs::create_dir_all(root.join("photos/2026")).unwrap();
        fs::write(root.join("a.txt"), b"top level").unwrap();
        fs::write(root.join("photos/2026/beach.jpg"), b"jpg bytes").unwrap();
        let entries = walk_with_hashes(&root).unwrap();
        let mut paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, vec!["a.txt", "photos", "photos/2026", "photos/2026/beach.jpg"]);

        let dir_entry = entries.iter().find(|e| e.path == "photos").unwrap();
        assert!(dir_entry.is_dir);
        assert_eq!(dir_entry.hash, None);

        let file_entry = entries.iter().find(|e| e.path == "a.txt").unwrap();
        assert!(!file_entry.is_dir);
        assert!(file_entry.hash.is_some());
        assert_eq!(file_entry.size, "top level".len() as u64);
    }

    #[test]
    fn walk_with_hashes_uses_forward_slash_paths() {
        let root = test_root("walk-forward-slash");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/c.txt"), b"x").unwrap();
        let entries = walk_with_hashes(&root).unwrap();
        let file_entry = entries.iter().find(|e| !e.is_dir).unwrap();
        assert_eq!(file_entry.path, "a/b/c.txt");
        assert!(!file_entry.path.contains('\\'));
    }

    #[test]
    fn identical_content_produces_identical_hash() {
        let root = test_root("walk-hash-identical");
        fs::write(root.join("one.txt"), b"same content").unwrap();
        fs::write(root.join("two.txt"), b"same content").unwrap();
        let entries = walk_with_hashes(&root).unwrap();
        let hash_one = entries.iter().find(|e| e.path == "one.txt").unwrap().hash.clone();
        let hash_two = entries.iter().find(|e| e.path == "two.txt").unwrap().hash.clone();
        assert_eq!(hash_one, hash_two);
        assert!(hash_one.unwrap().len() == 64);
    }

    #[test]
    fn different_content_produces_different_hash() {
        let root = test_root("walk-hash-different");
        fs::write(root.join("one.txt"), b"content A").unwrap();
        fs::write(root.join("two.txt"), b"content B").unwrap();
        let entries = walk_with_hashes(&root).unwrap();
        let hash_one = entries.iter().find(|e| e.path == "one.txt").unwrap().hash.clone();
        let hash_two = entries.iter().find(|e| e.path == "two.txt").unwrap().hash.clone();
        assert_ne!(hash_one, hash_two);
    }

    #[test]
    fn walk_with_hashes_on_empty_root_returns_empty() {
        let root = test_root("walk-empty");
        let entries = walk_with_hashes(&root).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn remove_conflict_files_deletes_only_matching_nested_files() {
        let root = test_root("remove-conflicts");
        fs::create_dir_all(root.join("photos")).unwrap();
        fs::write(root.join("normal.jpg"), b"x").unwrap();
        fs::write(root.join("桌布 (衝突自 TimHCLin-PC，2026-08-16).jpg"), b"x").unwrap();
        fs::write(root.join("photos/照片 (衝突自 другое-PC，2026-08-17).jpg"), b"x").unwrap();

        let removed = remove_conflict_files(&root).unwrap();
        assert_eq!(removed, 2);
        assert!(root.join("normal.jpg").exists());
        assert!(!root.join("桌布 (衝突自 TimHCLin-PC，2026-08-16).jpg").exists());
        assert!(!root.join("photos/照片 (衝突自 другое-PC，2026-08-17).jpg").exists());
        // 資料夾本身（就算名字也含有這個字串）不該被當成檔案處理。
        assert!(root.join("photos").is_dir());
    }

    #[test]
    fn remove_conflict_files_on_tree_without_conflicts_removes_nothing() {
        let root = test_root("remove-conflicts-none");
        fs::write(root.join("a.txt"), b"x").unwrap();
        let removed = remove_conflict_files(&root).unwrap();
        assert_eq!(removed, 0);
        assert!(root.join("a.txt").exists());
    }

    #[test]
    fn paginate_sync_entries_splits_by_budget_and_offset() {
        let entries: Vec<SyncEntry> = (0..2000)
            .map(|i| SyncEntry {
                path: format!("file-{i}.txt"),
                is_dir: false,
                size: 1,
                modified: 0,
                hash: Some("a".repeat(64)),
            })
            .collect();
        let page1 = paginate_sync_entries(&entries, 0);
        assert!(!page1.is_empty());
        assert!(page1.len() < entries.len(), "應該分頁，不會一次全部塞進一頁");
        let page2 = paginate_sync_entries(&entries, page1.len());
        assert_eq!(page2.first().unwrap().path, entries[page1.len()].path);
    }

    #[test]
    fn paginate_sync_entries_past_end_returns_empty() {
        let entries = vec![SyncEntry {
            path: "only.txt".to_string(),
            is_dir: false,
            size: 1,
            modified: 0,
            hash: Some("h".to_string()),
        }];
        assert!(paginate_sync_entries(&entries, entries.len()).is_empty());
    }
}
