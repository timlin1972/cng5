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


/// `manual` 指令印出來的說明，比 `commands()`/`help` 那種一行式的用法字串完整，
/// 帶使用情境跟範例——指令一多，光看 `add <dir>` 這種簽名不容易想起整套流程
/// 是怎麼運作的（尤其是 scan 跟目錄變動之間的關係）。
const MANUAL_TEXT: &str = "\
gitrepo：監控本機一堆 git repo 有沒有異動，手動 scan 一次，不會背景 polling。

監控目錄可以是「本身就是一個 repo」（例如 buildroot/moxa），也可以是「底下一層
每個子目錄各自是一個 repo」（例如 buildroot/dl）。add 的時候不用自己分辨是哪一
種，程式會自動判斷。「有異動」只要中以下任一種就算：
  - 有未提交的變更（git status，untracked 的新檔案也算）。
  - 目前 checkout 的 branch不是 develop——包括真的切到別的 branch、或
    detached HEAD 但剛好在別的 remote branch tip 上；如果是 detached HEAD
    且剛好在 origin/develop 的 tip 上，視同在 develop 上，不算異動。抓「AI
    幫忙開了新 branch 並直接 commit 過去」這種工作目錄本身乾淨、但其實有
    東西的情況。
  - 目前 branch 領先它的 upstream 至少一個 commit（本地已經 commit，但還沒
    push）——抓「AI 直接 commit 在原本的 branch，但還沒 push」的情況。

範例：
  add ~/MoxaBuild.mds.role/buildroot/dl     dl 底下每個子目錄各自是一個 repo
  add ~/MoxaBuild.mds.role/buildroot/moxa   moxa 本身就是一個 repo
  scan                                      手動觸發一次掃描
  list                                      監控目錄清單 + 上一次 scan 的結果
  remove ~/MoxaBuild.mds.role/buildroot/dl  移除一個監控目錄
  clear                                     移除所有監控目錄

注意事項：
  - scan 進行中再下一次 scan 會被擋掉，等上一輪跑完再重來。
  - add/remove 之後、下一次 scan 完成之前，list 只會顯示「目錄有更動，等待
    scan...」，不顯示舊資料，因為那已經不代表目前這份監控目錄清單了。
  - scan 進行中如果又 add/remove，這一輪 scan 的結果會作廢，等於中途停止。
  - 預期的 branch 名稱（develop）是寫死的常數，適用所有監控目錄底下的每個
    repo，不能個別指定。
";

/// 平行掃描 repo 的執行緒數上限。先設成 1（等於循序執行）是使用者刻意選的保守
/// 值——`buildroot/dl` 底下可能上百個 repo，先確認正確性，真的太慢了再調高；
/// 調高這裡就會自動變成平行掃描，不需要改其他程式碼。
const MAX_CONCURRENCY: usize = 1;

/// 所有監控目錄底下每個 repo「正常」時應該在的 branch，寫死的常數，不能個別
/// 指定——使用者刻意要求：不管是哪個 repo，只要不是這條 branch（或 detached
/// HEAD 剛好在它的 remote tip 上），就算異動，不要再靠「第一次看到這個 repo
/// 當下記住的 branch」當基準（那個舊規格會讓每個 repo 記住不同的基準，沒辦法
/// 一眼看出「這個 repo 現在到底該不該在 develop 上」）。
const EXPECTED_BRANCH: &str = "develop";

/// 一次 `scan` 掃出來「有異動」的 repo，三種情況只要中一種就會出現在這裡：
/// 有未提交變更、branch 不是 `EXPECTED_BRANCH`、或領先 upstream 還沒 push。
/// `error` 標記 `git status` 執行失敗（例如 `.git` 損毀、git 指令找不到）的
/// 情況——這種也列出來讓使用者知道，不悄悄當成乾淨略過。
struct FlaggedRepo {
    path: PathBuf,
    error: bool,
    uncommitted: bool,
    /// `Some(目前的 branch 名稱)`：跟 `EXPECTED_BRANCH` 不一樣。完全解析不出
    /// branch 名稱的 detached HEAD（不在任何 remote branch 的 tip 上）用
    /// `"(detached HEAD)"` 表示。
    branch_changed: Option<String>,
    /// 目前 branch 領先它的 upstream 幾個 commit；0 表示沒有這個情況。
    ahead: usize,
}

/// 組出一個「有異動」的 repo 後面要附的原因說明。同一個 repo 可能同時中好幾
/// 種情況（例如又換了 branch、又有未提交變更），全部列出來、用頓號分隔，不是
/// 只顯示第一個命中的原因就跳過其他。
fn describe_reasons(entry: &FlaggedRepo) -> String {
    if entry.error {
        return " (git status 失敗)".to_string();
    }
    let mut reasons = Vec::new();
    if entry.uncommitted {
        reasons.push("未提交變更".to_string());
    }
    if let Some(current) = &entry.branch_changed {
        reasons.push(format!("branch 是 {current}，不是 {EXPECTED_BRANCH}"));
    }
    if entry.ahead > 0 {
        reasons.push(format!("領先 upstream {} 個 commit 還沒 push", entry.ahead));
    }
    if reasons.is_empty() {
        String::new()
    } else {
        format!(" ({})", reasons.join("、"))
    }
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
    Idle(Vec<FlaggedRepo>),
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

/// 一個 repo 目前的狀態快照，`git status --porcelain=v2 --branch` 一次呼叫就
/// 拿到全部需要的資訊，不用分開跑三次 `git` 子行程——`buildroot/dl` 底下可能
/// 上百個 repo，多一種判斷不該讓子行程數量跟著變多倍。
struct RepoState {
    /// 目前 checkout 的 branch 名稱；detached HEAD（不在任何 branch 上）時是
    /// `None`。
    branch: Option<String>,
    /// 目前 branch 領先它的 upstream 幾個 commit（本地已經 commit、還沒
    /// push）；沒有設定 upstream 的 branch 恆為 0。
    ahead: usize,
    /// 工作目錄有沒有未提交的變更（含 untracked 的新檔案）。
    uncommitted: bool,
}

/// 解析 `git status --porcelain=v2 --branch` 的輸出。輸出格式（有 upstream
/// 時）大致長這樣：
/// ```text
/// # branch.oid <commit>
/// # branch.head <branch-name>
/// # branch.upstream <upstream-branch>
/// # branch.ab +<ahead> -<behind>
/// 1 <xy> ... <path>          (變更過的追蹤檔案)
/// ? <path>                  (untracked)
/// ```
/// 沒有 upstream 時沒有 `branch.upstream`/`branch.ab` 這兩行；detached HEAD
/// 時 `branch.head` 的值是字面上的 `(detached)`。任何不是 `#` 開頭的行都代表
/// 工作目錄有變更（不管是追蹤檔案的變更還是 untracked），不需要細分是哪一種。
fn parse_status_v2(stdout: &str) -> RepoState {
    let mut branch = None;
    let mut ahead = 0usize;
    let mut uncommitted = false;
    for line in stdout.lines() {
        if let Some(name) = line.strip_prefix("# branch.head ") {
            branch = if name == "(detached)" { None } else { Some(name.to_string()) };
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            // 格式是 "+<ahead> -<behind>"，只在意領先的那個數字。
            if let Some(plus) = ab.split_whitespace().next() {
                ahead = plus.trim_start_matches('+').parse().unwrap_or(0);
            }
        } else if !line.starts_with('#') {
            uncommitted = true;
        }
    }
    RepoState { branch, ahead, uncommitted }
}

/// `"origin/develop"` → `"develop"`：去掉 remote 名稱那一段前綴，沒有 `/`
/// 就照原樣傳回。純字串處理，不呼叫 git，方便寫單元測試。
fn short_branch_name(remote_ref: &str) -> &str {
    remote_ref.split_once('/').map(|(_, rest)| rest).unwrap_or(remote_ref)
}

/// 從 `git for-each-ref --points-at=HEAD --format=%(refname:short) refs/remotes/`
/// 的輸出裡，挑出真正代表某個 branch 的候選，回傳去掉 remote 前綴的名稱。
///
/// 一定要濾掉 remote 自己的 `HEAD` symref（指向該 remote 預設 branch的指標，
/// 不是真正的 branch）——依 git 版本不同，它的短名稱可能是純粹的 `<remote>`
/// （沒有 `/`），也可能是 `<remote>/HEAD`，兩種都要排除，不然會誤把字面上的
/// `HEAD` 當成解析出來的 branch 名稱回傳（實測遇到的真實案例：`origin/HEAD`
/// 排序在 `origin/develop` 前面，「優先選 origin/*」那條規則會先選到它）。
///
/// 多個候選都指向同一個 commit 時，優先選 `origin/*`，找不到就選字母排序
/// 第一個——多數情況只有一個 remote，這條規則只是避免結果不確定。純字串處理，
/// 不呼叫 git，方便寫單元測試。
fn pick_detached_branch_name(stdout: &str) -> Option<String> {
    let names: Vec<&str> =
        stdout.lines().filter(|l| !l.is_empty() && l.contains('/') && !l.ends_with("/HEAD")).collect();
    if names.is_empty() {
        return None;
    }
    // vendor/3rdparty 這類 repo 常見同一個 commit 同時是好幾個 release
    // branch（例如某個產品線專用的 branch）的起點，跟 `EXPECTED_BRANCH`
    // 剛好也指到同一個 commit——這種情況要直接認定「在 develop 上」，不能因為
    // 其他巧合也指到同一個 commit 的 branch 名稱排序比較前面，就蓋掉真正的
    // develop（實測遇到的真實案例：`MDS-G4000-4XGS_v5.0.2_develop` 字母排序
    // 排在 `develop` 前面，被誤選成「目前的 branch」）。
    if names.iter().any(|n| short_branch_name(n) == EXPECTED_BRANCH) {
        return Some(EXPECTED_BRANCH.to_string());
    }
    let mut names = names;
    names.sort();
    let chosen = names.iter().find(|n| n.starts_with("origin/")).copied().unwrap_or(names[0]);
    Some(short_branch_name(chosen).to_string())
}

/// detached HEAD 時，查目前這個 commit 是不是剛好是某個 remote branch 的最新
/// commit（CI/build 流程常見的 `git checkout origin/develop` 就是這種情況，
/// 這其實就是「在 develop 上」，不是異常）——如果是，回傳去掉 remote 前綴的
/// branch 名稱，視同 checkout 在那個 branch 上，用一般 branch 的邏輯去跟基準
/// 比對；查不到任何對得上的 remote branch 就回傳 `None`（真正意義上的
/// detached，例如卡在某個歷史 commit），維持「跟基準不一樣」的判斷。
fn resolve_detached_branch(repo: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["for-each-ref", "--points-at=HEAD", "--format=%(refname:short)", "refs/remotes/"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    pick_detached_branch_name(&String::from_utf8_lossy(&output.stdout))
}

/// 對 `repo` 跑一次 `git status --porcelain=v2 --branch`，解析成 `RepoState`。
/// detached HEAD（`branch` 解析成 `None`）時多查一次
/// `resolve_detached_branch`，只在真的 detached 時才多花這次 git 呼叫，不影響
/// 一般有 branch 的 repo 的效能。
fn repo_state(repo: &Path) -> Result<RepoState> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain=v2", "--branch"])
        .output()
        .with_context(|| format!("執行 git status 失敗: {}", repo.display()))?;
    if !output.status.success() {
        bail!("git status 回傳非 0: {}", repo.display());
    }
    let mut state = parse_status_v2(&String::from_utf8_lossy(&output.stdout));
    if state.branch.is_none() {
        state.branch = resolve_detached_branch(repo);
    }
    Ok(state)
}

impl GitRepoPlugin {
    pub fn new(_ctx: SharedContext) -> Self {
        // 剛啟動、還沒 scan 過，跟「目錄有更動」是同一種「資料不可信」的狀態，
        // 用同一個 `Stale` 表示，不用另外分兩種訊息。監控目錄清單不做磁碟
        // 持久化——使用者改成把 `add` 指令寫進 `script-local.cli`，每次啟動
        // 都會重新執行，這裡直接從空清單開始就好。
        Self { watched: Vec::new(), scan: Arc::new(Mutex::new(ScanState::Stale)), generation: Arc::new(AtomicU64::new(0)) }
    }

    /// `add`/`remove` 真的改動了監控目錄清單之後呼叫：上一次的 scan 結果（不管
    /// 是已經完成的還是正在跑的）都不再代表目前這份目錄清單，所以要作廢。
    fn mark_stale(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        *self.scan.lock().unwrap() = ScanState::Stale;
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
            ScanState::Idle(flagged) => {
                let mut s = self.watched_list_text();
                s.push('\n');
                if flagged.is_empty() {
                    s.push_str("(尚未發現有異動的 repo)\n");
                } else {
                    s.push_str("有異動的 repo:\n");
                    for entry in flagged {
                        s.push_str(&format!("  {}{}\n", display_path(&entry.path), describe_reasons(entry)));
                    }
                }
                s
            }
        }
    }

    /// 掃描已加入的每個目錄底下的 repo，找出有未提交變更、branch 跟基準不同、
    /// 或領先 upstream 還沒 push 的 repo（見 `FlaggedRepo`）。真正的掃描丟到
    /// 背景執行緒跑，`dispatch` 立刻回傳，不卡住共用的 `Shell` 鎖；`scan` 執行
    /// 中拒絕開新的一輪，避免兩輪同時跑互相干擾同一份結果。
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
            let flagged = Arc::new(Mutex::new(Vec::new()));
            // 每個 worker 從同一個共用佇列裡搶下一個 repo 來處理，直到佇列空了
            // 才結束；`MAX_CONCURRENCY` 調高就會有更多 worker 同時搶，不用改
            // 這段邏輯本身。處理下一個之前先看 `generation` 有沒有變，變了就表示
            // scan 途中被 `add`/`remove` 打斷，不繼續處理剩下的 repo（沒辦法中斷
            // 已經在跑的那一個，但不會再啟動新的）。
            let handles: Vec<_> = (0..MAX_CONCURRENCY)
                .map(|_| {
                    let queue = Arc::clone(&queue);
                    let flagged = Arc::clone(&flagged);
                    let generation = Arc::clone(&generation);
                    let done = Arc::clone(&done);
                    thread::spawn(move || loop {
                        if generation.load(Ordering::SeqCst) != my_generation {
                            break;
                        }
                        let next = queue.lock().unwrap().pop_front();
                        let Some(repo) = next else { break };
                        match repo_state(&repo) {
                            Ok(state) => {
                                let branch_changed = match &state.branch {
                                    Some(current) if current == EXPECTED_BRANCH => None,
                                    Some(current) => Some(current.clone()),
                                    None => Some("(detached HEAD)".to_string()),
                                };
                                if state.uncommitted || branch_changed.is_some() || state.ahead > 0 {
                                    flagged.lock().unwrap().push(FlaggedRepo {
                                        path: repo,
                                        error: false,
                                        uncommitted: state.uncommitted,
                                        branch_changed,
                                        ahead: state.ahead,
                                    });
                                }
                            }
                            Err(_) => flagged.lock().unwrap().push(FlaggedRepo {
                                path: repo,
                                error: true,
                                uncommitted: false,
                                branch_changed: None,
                                ahead: 0,
                            }),
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
            let mut flagged = Arc::try_unwrap(flagged)
                .ok()
                .expect("所有 worker 都已經 join 完，只剩下這裡的參照")
                .into_inner()
                .unwrap();
            flagged.sort_by(|a, b| a.path.cmp(&b.path));
            *scan.lock().unwrap() = ScanState::Idle(flagged);
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

    fn manual_text(&self) -> &'static str {
        MANUAL_TEXT
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod status_parsing_tests {
    use super::*;

    /// 乾淨、沒有設定 upstream 的 repo：只有 branch 名稱，沒有 ahead/dirty。
    #[test]
    fn clean_repo_no_upstream() {
        let state = parse_status_v2("# branch.oid abc123\n# branch.head main\n");
        assert_eq!(state.branch.as_deref(), Some("main"));
        assert_eq!(state.ahead, 0);
        assert!(!state.uncommitted);
    }

    /// 有 upstream 且領先幾個 commit，`branch.ab` 那行要正確解出 ahead 數字。
    #[test]
    fn ahead_of_upstream_parsed() {
        let state = parse_status_v2(
            "# branch.oid abc123\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -0\n",
        );
        assert_eq!(state.ahead, 2);
        assert!(!state.uncommitted);
    }

    /// 追蹤檔案有變更（`1` 開頭那種格式）算未提交。
    #[test]
    fn uncommitted_change_detected() {
        let state = parse_status_v2(
            "# branch.oid abc123\n# branch.head main\n1 .M N... 100644 100644 100644 abc def file.txt\n",
        );
        assert!(state.uncommitted);
    }

    /// untracked 的新檔案（`?` 開頭）一樣算未提交——這是使用者刻意要求的行為。
    #[test]
    fn untracked_file_counts_as_uncommitted() {
        let state = parse_status_v2("# branch.oid abc123\n# branch.head main\n? new-file.txt\n");
        assert!(state.uncommitted);
    }

    /// detached HEAD 時 `branch.head` 的值是字面上的 `(detached)`，要解成
    /// `None`，不能誤當成一個叫 `(detached)` 的 branch 名稱。
    #[test]
    fn detached_head_has_no_branch() {
        let state = parse_status_v2("# branch.oid abc123\n# branch.head (detached)\n");
        assert_eq!(state.branch, None);
    }

    /// `resolve_detached_branch` 找到 remote branch 之後，要去掉 remote 名稱
    /// 那一段前綴——`"origin/develop"` 視同 `"develop"`。
    #[test]
    fn short_branch_name_strips_remote_prefix() {
        assert_eq!(short_branch_name("origin/develop"), "develop");
        assert_eq!(short_branch_name("upstream/feature/x"), "feature/x");
        assert_eq!(short_branch_name("no-remote-prefix"), "no-remote-prefix");
    }

    #[test]
    fn pick_detached_branch_name_basic() {
        assert_eq!(pick_detached_branch_name("origin/develop\n"), Some("develop".to_string()));
    }

    /// git 版本較新時，remote 的 HEAD symref 短名稱會收斂成純粹的
    /// `<remote>`（沒有 `/`）——這種一定要濾掉，不然會被誤判成一個真正的
    /// branch 名稱回傳。
    #[test]
    fn pick_detached_branch_name_ignores_collapsed_remote_head() {
        assert_eq!(pick_detached_branch_name("origin\norigin/develop\n"), Some("develop".to_string()));
    }

    /// git 版本較舊（或不同平台）時，remote 的 HEAD symref 短名稱是
    /// `<remote>/HEAD`——這是這次真正踩到的 bug：排序後 `origin/HEAD` 會排在
    /// `origin/develop` 前面，「優先選 origin/*」那條規則會先選到它，
    /// `short_branch_name` 再把它切成字面上的 `"HEAD"` 回傳，誤判成
    /// branch 不是 develop。
    #[test]
    fn pick_detached_branch_name_ignores_head_suffix() {
        assert_eq!(pick_detached_branch_name("origin/HEAD\norigin/develop\n"), Some("develop".to_string()));
    }

    #[test]
    fn pick_detached_branch_name_prefers_origin_over_other_remotes() {
        assert_eq!(
            pick_detached_branch_name("upstream/develop\norigin/develop\n"),
            Some("develop".to_string())
        );
    }

    /// vendor/3rdparty repo 常見同一個 commit 同時是好幾個 release branch 的
    /// 起點——`origin/develop` 也指到這個 commit 時，不能因為另一個 branch
    /// 名稱字母排序比較前面（大寫字母在 ASCII 排序中排在小寫字母前面）就蓋掉
    /// 真正的 develop。這是實測遇到的真實 bug：
    /// `MDS-G4000-4XGS_v5.0.2_develop` 排在 `develop` 前面，被誤判成目前的
    /// branch，即使 `git status` 明確顯示 `HEAD detached at origin/develop`。
    #[test]
    fn pick_detached_branch_name_prefers_develop_over_alphabetically_earlier_sibling() {
        assert_eq!(
            pick_detached_branch_name("origin/MDS-G4000-4XGS_v5.0.2_develop\norigin/develop\n"),
            Some("develop".to_string())
        );
    }

    /// 只有 remote 的 HEAD symref、沒有任何真正的候選 branch 時，回傳
    /// `None`——維持「真正意義上的 detached」的判斷，不要硬湊一個結果出來。
    #[test]
    fn pick_detached_branch_name_none_when_only_head_symref() {
        assert_eq!(pick_detached_branch_name("origin\n"), None);
        assert_eq!(pick_detached_branch_name("origin/HEAD\n"), None);
    }

    #[test]
    fn pick_detached_branch_name_empty_input() {
        assert_eq!(pick_detached_branch_name(""), None);
    }
}
