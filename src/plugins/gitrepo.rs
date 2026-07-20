use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{bail, Context, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

/// 監控目錄清單存放位置，跟 `NotepadPlugin`/`NOTEPAD_DIR` 一樣的作法：存在程式
/// 執行目錄底下，重啟後不用重新 `add` 一次。
const GITREPO_DIR: &str = "gitrepo";
const WATCHED_FILE: &str = "watched.txt";

/// 平行掃描 repo 的執行緒數上限。先設成 1（等於循序執行）是使用者刻意選的保守
/// 值——`buildroot/dl` 底下可能上百個 repo，先確認正確性，真的太慢了再調高；
/// 調高這裡就會自動變成平行掃描，不需要改其他程式碼。
const MAX_CONCURRENCY: usize = 1;

/// 一次 `scan` 掃出來的「不乾淨」的 repo。`error` 標記 `git status` 執行失敗
/// （例如 `.git` 損毀、git 指令找不到）的情況——這種也列出來讓使用者知道，
/// 不悄悄當成乾淨略過。
struct DirtyRepo {
    path: PathBuf,
    error: bool,
}

/// 目前的 scan 狀態。
/// - `Stale`：監控目錄清單有變動（或程式剛啟動、還沒 scan 過），上一次的結果
///   已經不能代表現在的目錄清單，`panel_text`/`list` 這時只顯示「等待 scan」，
///   其餘（監控目錄清單、上次結果）都不顯示，避免被誤認成是目前這份目錄清單
///   的正確結果。
/// - `Running`：scan 正在跑，`panel_text`/`list` 只顯示「掃描中...(完成/總數)」。
///   `done` 是 worker 執行緒即時更新的計數（`AtomicUsize` 才能在不用另外拿鎖的
///   情況下讓 `status_text` 隨時讀到最新進度），`total` 一開始就固定，不會變。
/// - `Idle`：上一次 scan 完成、期間沒有任何 `add`/`remove` 干擾的結果，可以
///   放心顯示。
enum ScanState {
    Stale,
    Running { done: Arc<AtomicUsize>, total: usize },
    Idle(Vec<DirtyRepo>),
}

pub struct GitRepoPlugin {
    /// 使用者 `add` 的頂層目錄（已 `canonicalize`）。panel 只列這一層，不展開
    /// 子目錄——不管是 `moxa` 這種本身就是 repo 的，還是 `dl` 這種底下一堆 repo
    /// 的，都是同一份清單，差異留給 `repos_under` 處理。
    watched: Vec<PathBuf>,
    /// 背景 scan 執行緒（`scan`）跟 `panel_text` 共用，需要 `Arc<Mutex<_>>`。
    scan: Arc<Mutex<ScanState>>,
    /// 每次 `add`/`remove` 成功都會 +1，`scan` 開始時記下當下的值：跑完之後如果
    /// 這個值已經變了，代表跑到一半監控目錄被改過，這一輪的結果作廢不寫回
    /// `scan`（`add`/`remove` 那邊已經把狀態設成 `Stale` 了）。worker 執行緒也會
    /// 拿這個值跟自己記住的比對，發現不一樣就提早結束、不繼續處理剩下的 repo，
    /// 這就是「停止 scan」的實作方式——沒辦法真的中斷已經在跑的 `git status`
    /// 子行程，但不會再啟動新的。
    generation: Arc<AtomicU64>,
}

/// 使用者家目錄，`canonicalize` 過（`HOME` 環境變數本身可能含符號連結，跟
/// `add`/`remove` 存進 `watched` 的路徑一樣都先解過才比較，`display_path` 的
/// 前綴比對才會準）。家目錄一定存在，`canonicalize` 失敗就退回原始值。
fn home_dir() -> Option<PathBuf> {
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let raw = PathBuf::from(std::env::var(home_var).ok()?);
    Some(fs::canonicalize(&raw).unwrap_or(raw))
}

/// `~` 或 `~/...` 展開成使用者家目錄，`shell_words` 只負責斷詞不會展開這個，
/// 得自己處理使用者輸入的 `~/MoxaBuild.mds.role/buildroot/dl` 這種路徑。
fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    } else if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(input)
}

/// 顯示路徑用：家目錄底下的路徑一律顯示成 `~/...`，內部儲存/比對還是用完整
/// 的 canonical 路徑，只有印給使用者看的時候才轉換。
fn display_path(path: &Path) -> String {
    if let Some(home) = home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    path.display().to_string()
}

fn is_git_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// `watched` 本身是 repo（像 `moxa`）就回傳它自己；不是的話（像 `dl`）就掃第一層
/// 子目錄，只留下是 repo 的——這樣 `add` 的時候使用者不用自己分辨這兩種目錄。
fn repos_under(watched: &Path) -> Vec<PathBuf> {
    if is_git_repo(watched) {
        return vec![watched.to_path_buf()];
    }
    let Ok(entries) = fs::read_dir(watched) else { return Vec::new() };
    let mut repos: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && is_git_repo(path))
        .collect();
    repos.sort();
    repos
}

/// `Ok(true)` 表示這個 repo 有未提交的變更（含 untracked——使用者刻意要求算
/// 進去，因為新增檔案也算數），`Ok(false)` 是乾淨，`Err` 是 `git status` 執行
/// 失敗。
fn is_dirty(repo: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()
        .with_context(|| format!("執行 git status 失敗: {}", repo.display()))?;
    if !output.status.success() {
        bail!("git status 回傳非 0: {}", repo.display());
    }
    Ok(!output.stdout.is_empty())
}

impl GitRepoPlugin {
    pub fn new(_ctx: SharedContext) -> Self {
        // 剛啟動、還沒 scan 過，跟「目錄有更動」是同一種「資料不可信」的狀態，
        // 用同一個 `Stale` 表示，不用另外分兩種訊息。
        Self { watched: Self::load_watched(), scan: Arc::new(Mutex::new(ScanState::Stale)), generation: Arc::new(AtomicU64::new(0)) }
    }

    /// `add`/`remove` 真的改動了監控目錄清單之後呼叫：上一次的 scan 結果（不管
    /// 是已經完成的還是正在跑的）都不再代表目前這份目錄清單，所以要作廢。
    fn mark_stale(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        *self.scan.lock().unwrap() = ScanState::Stale;
    }

    fn watched_path() -> PathBuf {
        Path::new(GITREPO_DIR).join(WATCHED_FILE)
    }

    fn load_watched() -> Vec<PathBuf> {
        fs::read_to_string(Self::watched_path())
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(PathBuf::from)
            .collect()
    }

    fn save_watched(&self) -> Result<()> {
        fs::create_dir_all(GITREPO_DIR).context("建立 gitrepo 目錄失敗")?;
        let content: String = self.watched.iter().map(|dir| format!("{}\n", dir.display())).collect();
        fs::write(Self::watched_path(), content).context("儲存監控目錄清單失敗")?;
        Ok(())
    }

    fn add(&mut self, dir: &str, out: &OutputBuffer) -> Result<()> {
        let expanded = expand_tilde(dir);
        let canonical = fs::canonicalize(&expanded)
            .with_context(|| format!("目錄不存在或無法讀取: {}", display_path(&expanded)))?;
        if !canonical.is_dir() {
            bail!("不是一個目錄: {}", display_path(&canonical));
        }
        if self.watched.contains(&canonical) {
            out.push(&format!("已經加入過: {}\n", display_path(&canonical)));
            return Ok(());
        }
        self.watched.push(canonical.clone());
        self.save_watched()?;
        self.mark_stale();
        let count = repos_under(&canonical).len();
        out.push(&format!("已加入監控目錄: {} ({count} 個 git repo)\n", display_path(&canonical)));
        Ok(())
    }

    fn remove(&mut self, dir: &str, out: &OutputBuffer) -> Result<()> {
        let expanded = expand_tilde(dir);
        // 目錄可能已經被刪掉了，`canonicalize` 會失敗，這種情況退回用展開後的
        // 原始路徑比對，使用者才有辦法移除一個已經消失的監控目錄。
        let canonical = fs::canonicalize(&expanded).unwrap_or(expanded);
        let before = self.watched.len();
        self.watched.retain(|watched_dir| watched_dir != &canonical);
        if self.watched.len() == before {
            bail!("沒有監控這個目錄: {}", display_path(&canonical));
        }
        self.save_watched()?;
        self.mark_stale();
        out.push(&format!("已移除監控目錄: {}\n", display_path(&canonical)));
        Ok(())
    }

    fn clear(&mut self, out: &OutputBuffer) -> Result<()> {
        if self.watched.is_empty() {
            out.push("目前沒有任何監控目錄\n");
            return Ok(());
        }
        self.watched.clear();
        self.save_watched()?;
        self.mark_stale();
        out.push("已清除所有監控目錄\n");
        Ok(())
    }

    fn list(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push(&self.status_text());
        Ok(())
    }

    /// 監控目錄清單，每個目錄後面附上底下目前有幾個 git repo，讓使用者
    /// `add` 完馬上就能確認有沒有指到正確的目錄，不用等一次 `scan` 才知道。
    fn watched_list_text(&self) -> String {
        if self.watched.is_empty() {
            return "(還沒有加入任何監控目錄)\n".to_string();
        }
        let mut s = String::from("監控目錄:\n");
        for dir in &self.watched {
            let count = repos_under(dir).len();
            s.push_str(&format!("  {} ({count} 個 git repo)\n", display_path(dir)));
        }
        s
    }

    /// `list` 指令跟 `panel_text` 共用的內容：監控目錄清單 + 上一次 scan 的結果。
    /// 掃描中只回傳「掃描中」這一行，其餘都不顯示，避免把還沒跑完、不完整的
    /// 資料當成最新結果——不管是透過 `list` 指令看、還是開著 panel 看，這個判斷
    /// 都要一致。`Stale` 時監控目錄清單本身還是正確的（只是 scan 結果失效），
    /// 所以照樣顯示，讓使用者能確認 `add`/`remove` 有沒有生效，只是不顯示上一次
    /// （已經不能代表現在這份目錄清單的）scan 結果。
    fn status_text(&self) -> String {
        let state = self.scan.lock().unwrap();
        match &*state {
            ScanState::Stale => format!("{}\n目錄有更動，等待 scan...\n", self.watched_list_text()),
            ScanState::Running { done, total } => {
                format!("掃描中...({}/{total})\n", done.load(Ordering::SeqCst))
            }
            ScanState::Idle(dirty) => {
                let mut s = self.watched_list_text();
                s.push('\n');
                if dirty.is_empty() {
                    s.push_str("(尚未發現有未提交變更的 repo)\n");
                } else {
                    s.push_str("未提交變更的 repo:\n");
                    for entry in dirty {
                        let marker = if entry.error { " (git status 失敗)" } else { "" };
                        s.push_str(&format!("  {}{marker}\n", display_path(&entry.path)));
                    }
                }
                s
            }
        }
    }

    /// 掃描已加入的每個目錄底下的 repo，找出 `git status --porcelain` 不是空的
    /// （含 untracked）。真正的掃描丟到背景執行緒跑，`dispatch` 立刻回傳，不
    /// 卡住共用的 `Shell` 鎖；`scan` 執行中拒絕開新的一輪，避免兩輪同時跑互相
    /// 干擾同一份結果。
    fn scan(&mut self, out: &OutputBuffer) -> Result<()> {
        {
            let state = self.scan.lock().unwrap();
            if matches!(*state, ScanState::Running { .. }) {
                out.push("已經有一個 scan 正在進行，請稍候\n");
                return Ok(());
            }
        }
        let my_generation = self.generation.load(Ordering::SeqCst);
        // 總數在開始跑之前就先算好（只是列目錄、不用跑 `git status`，很快），
        // 這樣一開始顯示「掃描中」就能馬上帶出「完成/總數」，不用等第一個 repo
        // 掃完才知道總數是多少。兩個監控目錄可能重疊（例如同時 `add` 了 `dl` 跟
        // `dl/pkg-a`），先排序去重避免同一個 repo 被排進佇列兩次——不然不只多跑
        // 一次 `git status`，髒掉的話還會在結果裡重複出現同一行。
        let mut repos: Vec<PathBuf> = self.watched.iter().flat_map(|dir| repos_under(dir)).collect();
        repos.sort();
        repos.dedup();
        let total = repos.len();
        let done = Arc::new(AtomicUsize::new(0));
        *self.scan.lock().unwrap() = ScanState::Running { done: Arc::clone(&done), total };
        let scan = Arc::clone(&self.scan);
        let generation = Arc::clone(&self.generation);
        thread::spawn(move || {
            let queue = Arc::new(Mutex::new(VecDeque::from(repos)));
            let dirty = Arc::new(Mutex::new(Vec::new()));
            // 每個 worker 從同一個共用佇列裡搶下一個 repo 來處理，直到佇列空了
            // 才結束；`MAX_CONCURRENCY` 調高就會有更多 worker 同時搶，不用改
            // 這段邏輯本身。處理下一個之前先看 `generation` 有沒有變，變了就表示
            // scan 途中被 `add`/`remove` 打斷，不繼續處理剩下的 repo（沒辦法中斷
            // 已經在跑的那一個，但不會再啟動新的）。
            let handles: Vec<_> = (0..MAX_CONCURRENCY)
                .map(|_| {
                    let queue = Arc::clone(&queue);
                    let dirty = Arc::clone(&dirty);
                    let generation = Arc::clone(&generation);
                    let done = Arc::clone(&done);
                    thread::spawn(move || loop {
                        if generation.load(Ordering::SeqCst) != my_generation {
                            break;
                        }
                        let next = queue.lock().unwrap().pop_front();
                        let Some(repo) = next else { break };
                        match is_dirty(&repo) {
                            Ok(true) => dirty.lock().unwrap().push(DirtyRepo { path: repo, error: false }),
                            Ok(false) => {}
                            Err(_) => dirty.lock().unwrap().push(DirtyRepo { path: repo, error: true }),
                        }
                        done.fetch_add(1, Ordering::SeqCst);
                    })
                })
                .collect();
            for handle in handles {
                let _ = handle.join();
            }
            // 跑到一半被打斷的話，`scan` 這個共用狀態已經在 `mark_stale` 那邊被
            // 設成 `Stale` 了，這裡的結果不完整、不能拿來覆蓋掉它。
            if generation.load(Ordering::SeqCst) != my_generation {
                return;
            }
            let mut dirty = Arc::try_unwrap(dirty)
                .ok()
                .expect("所有 worker 都已經 join 完，只剩下這裡的參照")
                .into_inner()
                .unwrap();
            dirty.sort_by(|a, b| a.path.cmp(&b.path));
            *scan.lock().unwrap() = ScanState::Idle(dirty);
        });
        out.push(&format!("開始 scan...(共 {total} 個 git repo)\n"));
        Ok(())
    }
}

impl Plugin for GitRepoPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["add <dir>", "remove <dir>", "clear", "list", "scan"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "add" => self.add(args.first().context("add 需要一個目錄參數")?, out),
            "remove" => self.remove(args.first().context("remove 需要一個目錄參數")?, out),
            "clear" => self.clear(out),
            "list" => self.list(out),
            "scan" => self.scan(out),
            other => bail!("gitrepo 不認得指令: {other}"),
        }
    }

    /// 跑完後 GUI 每 200ms 重繪一次會自然拿到新資料，不需要額外的刷新機制。
    fn panel_text(&self) -> Option<String> {
        Some(self.status_text())
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
