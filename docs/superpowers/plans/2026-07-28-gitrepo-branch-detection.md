# gitrepo：偵測 branch 換過／commit 但還沒 push Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `gitrepo` plugin 的「有異動」判斷從單純「有未提交的變更」擴大成三種
情況（未提交變更／branch 跟第一次看到時記住的基準不同／目前 branch 領先
upstream 至少一個 commit），抓到「AI 開新 branch 並直接 commit」這種工作目錄
乾淨、但其實有新東西的情況。

**Architecture:** 用一次 `git status --porcelain=v2 --branch` 取代原本的
`git status --porcelain`，單一 subprocess 呼叫解析出 branch 名稱、
ahead-of-upstream 數量、有沒有未提交變更三件事。新增一個 `baseline_branches`
記憶體內的表，記錄每個 repo 第一次被看到時的 branch，之後 scan 時拿來比對。
`DirtyRepo` 換成攜帶更多資訊的 `FlaggedRepo`。詳見
`docs/superpowers/specs/2026-07-28-gitrepo-branch-detection-design.md`。

**Tech Stack:** Rust，既有的 `Plugin` trait，無新增外部 crate 依賴（`git` 指令
透過 `std::process::Command` 呼叫，跟原本一致）。

## Global Constraints

- 基準 branch 只存記憶體，不落地存檔（跟 `2026-07-26-remove-gitrepo-wol-persistence`
  那次拿掉磁碟持久化的決定一致，不要重新引入檔案 I/O）。
- 不改動 `add`/`remove`/`clear`/`list`/`scan` 這幾個指令的參數/用法，只改內部
  判斷邏輯跟 `list`/panel 顯示的內容。
- 不動 `wol` 或其他 plugin。
- `MAX_CONCURRENCY`/整體並行掃描架構不變，只換每個 repo 內部做的事。

---

### Task 1: 換掉底層的 git 狀態判斷

**Files:**
- Modify: `src/plugins/gitrepo.rs`

**Interfaces:**
- 新函式 `parse_status_v2(stdout: &str) -> RepoState`：純字串解析，不呼叫
  `git`，方便寫單元測試。
- 新函式 `repo_state(repo: &Path) -> Result<RepoState>`：呼叫
  `git -C <repo> status --porcelain=v2 --branch`，取代原本的 `is_dirty`。
- 刪除 `is_dirty`（不再被使用）。

- [x] **Step 1: 新增 `RepoState` 型別跟解析函式**

在 `fn is_dirty` 前面（原本第 145-159 行那個函式的位置）插入：

```rust
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

/// 對 `repo` 跑一次 `git status --porcelain=v2 --branch`，解析成 `RepoState`。
/// 取代原本只跑 `--porcelain`（沒有 branch/ahead 資訊）的 `is_dirty`。
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
    Ok(parse_status_v2(&String::from_utf8_lossy(&output.stdout)))
}
```

刪除原本的 `is_dirty` 函式（第 145-159 行，`/// Ok(true)` 開頭的註解到函式結尾）：

```rust
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
```

- [x] **Step 2: 加上單元測試**

在檔案最後（目前沒有 `#[cfg(test)]` 模組）新增：

```rust
#[cfg(test)]
mod status_parsing_tests {
    use super::*;

    #[test]
    fn clean_repo_no_upstream() {
        let state = parse_status_v2("# branch.oid abc123\n# branch.head main\n");
        assert_eq!(state.branch.as_deref(), Some("main"));
        assert_eq!(state.ahead, 0);
        assert!(!state.uncommitted);
    }

    #[test]
    fn ahead_of_upstream_parsed() {
        let state = parse_status_v2(
            "# branch.oid abc123\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -0\n",
        );
        assert_eq!(state.ahead, 2);
        assert!(!state.uncommitted);
    }

    #[test]
    fn uncommitted_change_detected() {
        let state = parse_status_v2(
            "# branch.oid abc123\n# branch.head main\n1 .M N... 100644 100644 100644 abc def file.txt\n",
        );
        assert!(state.uncommitted);
    }

    #[test]
    fn untracked_file_counts_as_uncommitted() {
        let state = parse_status_v2("# branch.oid abc123\n# branch.head main\n? new-file.txt\n");
        assert!(state.uncommitted);
    }

    #[test]
    fn detached_head_has_no_branch() {
        let state = parse_status_v2("# branch.oid abc123\n# branch.head (detached)\n");
        assert_eq!(state.branch, None);
    }
}
```

---

### Task 2: 加入基準 branch 記憶

**Files:**
- Modify: `src/plugins/gitrepo.rs`

**Interfaces:**
- `GitRepoPlugin` 新增欄位 `baseline_branches: Arc<Mutex<HashMap<PathBuf, String>>>`。
- 新方法 `GitRepoPlugin::record_baseline_if_missing(&self, repo: &Path)`。
- 新方法 `GitRepoPlugin::prune_baselines(&self)`。
- 需要新增 `use std::collections::HashMap;`（檔案目前沒有這個 import）。

- [x] **Step 1: 加上 `HashMap` import**

檔案開頭的 `use std::collections::VecDeque;` 改成：

```rust
use std::collections::{HashMap, HashSet, VecDeque};
```

（`HashSet` 是 `prune_baselines` 要用的，一起加。）

- [x] **Step 2: `GitRepoPlugin` 加欄位**

```rust
pub struct GitRepoPlugin {
    watched: Vec<PathBuf>,
    scan: Arc<Mutex<ScanState>>,
    generation: Arc<AtomicU64>,
    /// 每個 repo 第一次被看到（`add` 當下，或後續 scan 才發現的新 repo）時
    /// 記住的 branch，當作「正常」的基準——之後 scan 時目前 branch 只要跟這裡
    /// 記的不一樣，就算「有異動」（例如 AI 開了新 branch 並切過去）。純記憶體
    /// 內，不落地存檔，跟 `watched` 本身一致（見
    /// `2026-07-26-remove-gitrepo-wol-persistence` 那次拿掉磁碟持久化的決定）
    /// ——重開程式後，這裡會用重開當下的 branch 重新當作基準，這是刻意的
    /// 取捨，見 design doc「已知限制」。
    baseline_branches: Arc<Mutex<HashMap<PathBuf, String>>>,
}
```

- [x] **Step 3: `new()` 初始化新欄位**

```rust
impl GitRepoPlugin {
    pub fn new(_ctx: SharedContext) -> Self {
        Self {
            watched: Vec::new(),
            scan: Arc::new(Mutex::new(ScanState::Stale)),
            generation: Arc::new(AtomicU64::new(0)),
            baseline_branches: Arc::new(Mutex::new(HashMap::new())),
        }
    }
```

- [x] **Step 4: 新增 `record_baseline_if_missing`/`prune_baselines`**

在 `mark_stale` 後面（`add` 前面）插入：

```rust
    /// 第一次看到某個 repo（`add` 當下、或後續 scan 才發現的新 repo）就記住
    /// 目前 checkout 的 branch 當基準；已經記過的不覆蓋——不然使用者自己手動
    /// 切 branch 也會被誤當成新基準，之後真的被 AI 換掉反而偵測不出來（見
    /// design doc 的取捨說明）。`repo_state` 失敗（例如 repo 損毀）或是
    /// detached HEAD（沒有 branch 名稱可記）就先不記，等下次有機會成功時再補。
    fn record_baseline_if_missing(&self, repo: &Path) {
        let mut baselines = self.baseline_branches.lock().unwrap();
        if baselines.contains_key(repo) {
            return;
        }
        if let Ok(RepoState { branch: Some(branch), .. }) = repo_state(repo) {
            baselines.insert(repo.to_path_buf(), branch);
        }
    }

    /// `remove`/`clear` 之後，把不再屬於任何監控目錄底下的 repo 的基準資料
    /// 丟掉——不然移除又重新加回同一個目錄時，會誤用很久以前記住的舊基準，
    /// 跟「基準是第一次看到當下的 branch」這個設計初衷不符。
    fn prune_baselines(&self) {
        let valid: HashSet<PathBuf> = self.watched.iter().flat_map(|dir| repos_under(dir)).collect();
        self.baseline_branches.lock().unwrap().retain(|path, _| valid.contains(path));
    }
```

- [x] **Step 5: `add()` 記錄新加入的 repo 的基準**

現在的 `add`（記錄新目錄之後）：

```rust
        self.watched.push(canonical.clone());
        self.mark_stale();
        let count = repos_under(&canonical).len();
        out.push(&format!("已加入監控目錄: {} ({count} 個 git repo)\n", display_path(&canonical)));
        Ok(())
```

改成：

```rust
        self.watched.push(canonical.clone());
        self.mark_stale();
        let repos = repos_under(&canonical);
        for repo in &repos {
            self.record_baseline_if_missing(repo);
        }
        out.push(&format!("已加入監控目錄: {} ({} 個 git repo)\n", display_path(&canonical), repos.len()));
        Ok(())
```

- [x] **Step 6: `remove()`/`clear()` 呼叫 `prune_baselines`**

`remove`：

```rust
        self.watched.retain(|watched_dir| watched_dir != &canonical);
        if self.watched.len() == before {
            bail!("沒有監控這個目錄: {}", display_path(&canonical));
        }
        self.mark_stale();
        out.push(&format!("已移除監控目錄: {}\n", display_path(&canonical)));
```

改成：

```rust
        self.watched.retain(|watched_dir| watched_dir != &canonical);
        if self.watched.len() == before {
            bail!("沒有監控這個目錄: {}", display_path(&canonical));
        }
        self.mark_stale();
        self.prune_baselines();
        out.push(&format!("已移除監控目錄: {}\n", display_path(&canonical)));
```

`clear`：

```rust
        self.watched.clear();
        self.mark_stale();
        out.push("已清除所有監控目錄\n");
```

改成：

```rust
        self.watched.clear();
        self.mark_stale();
        self.baseline_branches.lock().unwrap().clear();
        out.push("已清除所有監控目錄\n");
```

---

### Task 3: 換掉 scan 邏輯跟顯示格式

**Files:**
- Modify: `src/plugins/gitrepo.rs`

**Interfaces:**
- `DirtyRepo` 換成 `FlaggedRepo`（多了 `uncommitted`/`branch_changed`/`ahead`
  欄位）。
- `ScanState::Idle(Vec<DirtyRepo>)` 改成 `ScanState::Idle(Vec<FlaggedRepo>)`。
- 新增 `describe_reasons(&FlaggedRepo) -> String`，`status_text` 用它組出每個
  repo 後面的說明文字。

- [x] **Step 1: `DirtyRepo` 換成 `FlaggedRepo`**

原本：

```rust
/// 一次 `scan` 掃出來的「不乾淨」的 repo。`error` 標記 `git status` 執行失敗
/// （例如 `.git` 損毀、git 指令找不到）的情況——這種也列出來讓使用者知道，
/// 不悄悄當成乾淨略過。
struct DirtyRepo {
    path: PathBuf,
    error: bool,
}
```

改成：

```rust
/// 一次 `scan` 掃出來「有異動」的 repo，三種情況只要中一種就會出現在這裡：
/// 有未提交變更、branch 跟基準不一樣、或領先 upstream 還沒 push。`error`
/// 標記 `git status` 執行失敗（例如 `.git` 損毀、git 指令找不到）的情況——這種
/// 也列出來讓使用者知道，不悄悄當成乾淨略過。
struct FlaggedRepo {
    path: PathBuf,
    error: bool,
    uncommitted: bool,
    /// `Some((基準, 目前))`：branch 跟第一次看到這個 repo 時記住的基準不一樣
    /// （含 detached HEAD，這種情況基準固定顯示成 `(有 branch)`、目前顯示成
    /// `(detached HEAD)`）。
    branch_changed: Option<(String, String)>,
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
    if let Some((baseline, current)) = &entry.branch_changed {
        reasons.push(format!("branch 從 {baseline} 換成 {current}"));
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
```

- [x] **Step 2: `ScanState::Idle` 換型別**

```rust
enum ScanState {
    Stale,
    Running { done: Arc<AtomicUsize>, total: usize },
    Idle(Vec<FlaggedRepo>),
}
```

- [x] **Step 3: `status_text()` 的 `Idle` 分支**

原本：

```rust
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
```

改成：

```rust
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
```

- [x] **Step 4: `scan()` 背景執行緒的判斷邏輯**

原本 worker 閉包裡：

```rust
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
```

改成（多 clone 一份 `baseline_branches`，變數名稱從 `dirty` 換成 `flagged`）：

```rust
                    let queue = Arc::clone(&queue);
                    let flagged = Arc::clone(&flagged);
                    let generation = Arc::clone(&generation);
                    let done = Arc::clone(&done);
                    let baseline_branches = Arc::clone(&baseline_branches);
                    thread::spawn(move || loop {
                        if generation.load(Ordering::SeqCst) != my_generation {
                            break;
                        }
                        let next = queue.lock().unwrap().pop_front();
                        let Some(repo) = next else { break };
                        match repo_state(&repo) {
                            Ok(state) => {
                                let branch_changed = {
                                    let mut baselines = baseline_branches.lock().unwrap();
                                    match (&state.branch, baselines.get(&repo).cloned()) {
                                        (Some(current), Some(baseline)) if *current != baseline => {
                                            Some((baseline, current.clone()))
                                        }
                                        (Some(current), None) => {
                                            // 第一次看到這個 repo（例如 dl 底下事後才新增的子
                                            // repo），記錄現在的 branch 當基準，這一輪不算換過。
                                            baselines.insert(repo.clone(), current.clone());
                                            None
                                        }
                                        (None, _) => {
                                            Some(("(有 branch)".to_string(), "(detached HEAD)".to_string()))
                                        }
                                        _ => None,
                                    }
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
```

`scan()` 函式前段（宣告 `dirty`/取得 `self.scan`/`self.generation` 的地方）也要把
變數名稱跟 clone 對象一併更新：

```rust
        thread::spawn(move || {
            let queue = Arc::new(Mutex::new(VecDeque::from(repos)));
            let flagged = Arc::new(Mutex::new(Vec::new()));
            let baseline_branches = Arc::clone(&self_baseline_branches); // 見下方 Step 5 的說明
```

（`self.baseline_branches` 需要在 `thread::spawn` 之前先 clone 出一份
`Arc`，作法跟 `scan`/`generation` 現有的 `Arc::clone(&self.scan)` 一致，下一步
會整理完整的 `scan()` 函式全文。）

- [x] **Step 5: 整理 `scan()` 完整函式**

把整個 `scan()` 方法換成：

```rust
    fn scan(&mut self, out: &OutputBuffer) -> Result<()> {
        {
            let state = self.scan.lock().unwrap();
            if matches!(*state, ScanState::Running { .. }) {
                out.push("已經有一個 scan 正在進行，請稍候\n");
                return Ok(());
            }
        }
        let my_generation = self.generation.load(Ordering::SeqCst);
        let mut repos: Vec<PathBuf> = self.watched.iter().flat_map(|dir| repos_under(dir)).collect();
        repos.sort();
        repos.dedup();
        let total = repos.len();
        let done = Arc::new(AtomicUsize::new(0));
        *self.scan.lock().unwrap() = ScanState::Running { done: Arc::clone(&done), total };
        let scan = Arc::clone(&self.scan);
        let generation = Arc::clone(&self.generation);
        let baseline_branches = Arc::clone(&self.baseline_branches);
        thread::spawn(move || {
            let queue = Arc::new(Mutex::new(VecDeque::from(repos)));
            let flagged = Arc::new(Mutex::new(Vec::new()));
            let handles: Vec<_> = (0..MAX_CONCURRENCY)
                .map(|_| {
                    let queue = Arc::clone(&queue);
                    let flagged = Arc::clone(&flagged);
                    let generation = Arc::clone(&generation);
                    let done = Arc::clone(&done);
                    let baseline_branches = Arc::clone(&baseline_branches);
                    thread::spawn(move || loop {
                        if generation.load(Ordering::SeqCst) != my_generation {
                            break;
                        }
                        let next = queue.lock().unwrap().pop_front();
                        let Some(repo) = next else { break };
                        match repo_state(&repo) {
                            Ok(state) => {
                                let branch_changed = {
                                    let mut baselines = baseline_branches.lock().unwrap();
                                    match (&state.branch, baselines.get(&repo).cloned()) {
                                        (Some(current), Some(baseline)) if *current != baseline => {
                                            Some((baseline, current.clone()))
                                        }
                                        (Some(current), None) => {
                                            baselines.insert(repo.clone(), current.clone());
                                            None
                                        }
                                        (None, _) => {
                                            Some(("(有 branch)".to_string(), "(detached HEAD)".to_string()))
                                        }
                                        _ => None,
                                    }
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
```

（跟原本的差異只有：變數名稱 `dirty` → `flagged`、多 clone/傳入
`baseline_branches`、`match is_dirty(&repo)` 換成上面 Step 4 那段更複雜的
`match repo_state(&repo)` 邏輯。並行掃描架構、`generation` 中止機制、
`MAX_CONCURRENCY` 都完全不變。）

---

### Task 4: 更新 `MANUAL_TEXT`

**Files:**
- Modify: `src/plugins/gitrepo.rs`

- [x] **Step 1**

原本：

```rust
const MANUAL_TEXT: &str = "\
gitrepo：監控本機一堆 git repo 的乾淨/髒狀態，手動 scan 一次，不會背景 polling。

監控目錄可以是「本身就是一個 repo」（例如 buildroot/moxa），也可以是「底下一層
每個子目錄各自是一個 repo」（例如 buildroot/dl）。add 的時候不用自己分辨是哪一
種，程式會自動判斷。「不乾淨」的定義是 git status --porcelain 不是空的，
untracked 的新檔案也算。

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
";
```

改成：

```rust
const MANUAL_TEXT: &str = "\
gitrepo：監控本機一堆 git repo 有沒有異動，手動 scan 一次，不會背景 polling。

監控目錄可以是「本身就是一個 repo」（例如 buildroot/moxa），也可以是「底下一層
每個子目錄各自是一個 repo」（例如 buildroot/dl）。add 的時候不用自己分辨是哪一
種，程式會自動判斷。「有異動」只要中以下任一種就算：
  - 有未提交的變更（git status，untracked 的新檔案也算）。
  - 目前 checkout 的 branch，跟第一次看到這個 repo時（add 當下、或後續 scan
    才發現的新 repo）記住的 branch 不一樣——抓「AI 幫忙開了新 branch 並直接
    commit 過去」這種工作目錄本身乾淨、但其實有東西的情況。
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
  - 「基準 branch」只存在記憶體裡，不會存檔：重開程式後，下一次 add 會用當下
    checkout 的 branch 重新當基準，如果在你發現之前程式剛好重開過，這一輪就
    偵測不到「branch 換過」了（未提交變更、領先 upstream 這兩種偵測不受
    影響）。
";
```

---

### Task 5: Build、測試、手動驗證、commit

- [x] **Step 1: Build**

Run: `cargo build 2>&1 | tail -30`
Expected: clean build，沒有 unused import 之類的警告。

- [x] **Step 2: 自動測試**

Run: `cargo test 2>&1 | tail -30`
Expected: Task 1 新增的 `status_parsing_tests` 全過，其餘既有測試不受影響。

- [ ] **Step 3: 手動驗證**

在一個測試用的 git repo（不要用真正在用的 repo，避免搞亂實際狀態）操作：

```
gitrepo
add <測試用的 repo 上層目錄>
scan
list                                  # 確認一開始是乾淨的
```

然後在另一個終端機，對這個測試 repo：

```bash
git checkout -b ai-made-this
echo x >> somefile
git add somefile
git commit -m "simulate AI commit"
```

回到 cng5：

```
scan
list                                  # 應該列出這個 repo，原因包含「branch 從
                                       # <原本的 branch> 換成 ai-made-this」
```

再驗證「commit 但沒換 branch」的情況：切回原本的 branch、直接 commit（不開新
branch），確認 `scan`/`list` 顯示「領先 upstream N 個 commit 還沒 push」（如果
這個測試 repo 有設定 upstream 的話；沒有 upstream 的話這條規則不會觸發，只有
branch 名稱不同才會被抓到，這是預期行為，見 design doc 的已知限制）。

- [x] **Step 4: Commit**

```bash
git add src/plugins/gitrepo.rs
git commit -m "$(cat <<'EOF'
gitrepo：把 branch 換過、commit 但還沒 push 也算進「有異動」

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

Pushed to origin/main as `f8eed2b`.

---

### Task 6（追加，2026-07-28）：detached HEAD 剛好在 remote branch tip 上時視同該 branch

**背景：** 上線後發現，CI/build 流程常見的 `git checkout origin/develop` 會讓
repo 進入 detached HEAD 狀態，但這不是異常——就是「在 develop 上」。Task 3
當時的實作把所有 detached HEAD 一律當成「跟基準不一樣」，導致這些其實沒有
真的被改過的 repo 全部被列進「有異動」，是誤判，不是預期行為。見 design doc
新增的「Detached HEAD 恰好在某個 remote branch 的 tip 上，視同該 branch」一節。

**Files:**
- Modify: `src/plugins/gitrepo.rs`

**Interfaces:**
- 新函式 `short_branch_name(remote_ref: &str) -> &str`：純字串處理（去掉
  remote 名稱前綴，例如 `"origin/develop"` → `"develop"`），可單元測試。
- 新函式 `resolve_detached_branch(repo: &Path) -> Option<String>`：呼叫
  `git for-each-ref --points-at=HEAD --format=%(refname:short) refs/remotes/`，
  查目前這個 commit 是不是剛好是某個 remote branch 的 tip。
- `repo_state` 改成：`parse_status_v2` 解出 `branch: None`（detached）時，多
  呼叫一次 `resolve_detached_branch` 嘗試解出對應的 branch 名稱。

- [x] **Step 1: 新增 `short_branch_name`/`resolve_detached_branch`**

在 `repo_state` 前面插入：

```rust
/// `"origin/develop"` → `"develop"`：去掉 remote 名稱那一段前綴，沒有 `/`
/// 就照原樣傳回。純字串處理，不呼叫 git，方便寫單元測試。
fn short_branch_name(remote_ref: &str) -> &str {
    remote_ref.split_once('/').map(|(_, rest)| rest).unwrap_or(remote_ref)
}

/// detached HEAD 時，查目前這個 commit 是不是剛好是某個 remote branch 的最新
/// commit（CI/build 流程常見的 `git checkout origin/develop` 就是這種情況）
/// ——如果是，回傳去掉 remote 前綴的 branch 名稱，視同 checkout 在那個 branch
/// 上；查不到任何對得上的 remote branch 就回傳 `None`（真正意義上的 detached，
/// 例如卡在某個歷史 commit），維持「跟基準不一樣」的原本判斷。
///
/// 多個 remote 剛好都指到同一個 commit 時，優先選 `origin/*`，找不到就選字母
/// 排序第一個——多數情況只有一個 remote，這條規則只是避免結果不確定。
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut names: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    if names.is_empty() {
        return None;
    }
    names.sort();
    let chosen = names.iter().find(|n| n.starts_with("origin/")).copied().unwrap_or(names[0]);
    Some(short_branch_name(chosen).to_string())
}
```

- [x] **Step 2: `repo_state` 補上 detached 的額外解析**

原本：

```rust
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
    Ok(parse_status_v2(&String::from_utf8_lossy(&output.stdout)))
}
```

改成：

```rust
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
```

（`parse_status_v2` 本身不變，純粹解析 `git status` 輸出的 `branch: None` 仍然
代表「這次輸出沒有具體 branch 名稱」——`resolve_detached_branch` 是額外一層，
只在真的 detached 時才多跑一次 git 指令，不影響一般有 branch 的 repo 的效能。）

- [x] **Step 3: 加上 `short_branch_name` 的單元測試**

在 `status_parsing_tests` 模組裡加：

```rust
    #[test]
    fn short_branch_name_strips_remote_prefix() {
        assert_eq!(short_branch_name("origin/develop"), "develop");
        assert_eq!(short_branch_name("upstream/feature/x"), "feature/x");
        assert_eq!(short_branch_name("no-remote-prefix"), "no-remote-prefix");
    }
```

- [x] **Step 4: Build + 測試**

Run: `cargo build 2>&1 | tail -30`（clean）
Run: `cargo test 2>&1 | tail -30`（全過，含新增的 `short_branch_name_strips_remote_prefix`）

- [x] **Step 5: Commit**

```bash
git add src/plugins/gitrepo.rs docs/superpowers/specs/2026-07-28-gitrepo-branch-detection-design.md docs/superpowers/plans/2026-07-28-gitrepo-branch-detection.md
git commit -m "$(cat <<'EOF'
gitrepo：detached HEAD 剛好在 remote branch tip 上時視同該 branch，不算異動

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

Committed as `3bb2a96`，已 push 到 origin/main。

---

### Task 7（追加，2026-07-28）：拿掉「第一次看到的基準」，改成寫死 `develop`

**背景：** 使用者重新校準規格，決定不要「每個 repo 各自記住第一次看到時的
branch」這套機制，改成單一寫死的 `EXPECTED_BRANCH = "develop"`，套用到所有
監控目錄底下的每個 repo。理由：規則更簡單、不用擔心「重開程式基準被重設」這
種邊界情況，代價是沒辦法針對個別 repo 指定不同的預期 branch（使用者確認接受
這個取捨）。見 design doc 的「「正常」的 branch 是寫死的 `develop`」一節。

**Files:**
- Modify: `src/plugins/gitrepo.rs`

**Interfaces：**
- 新增常數 `const EXPECTED_BRANCH: &str = "develop";`。
- **移除**：`GitRepoPlugin.baseline_branches` 欄位、
  `record_baseline_if_missing`/`prune_baselines` 方法、`use std::collections::HashMap`/
  `HashSet` import（不再需要）。
- `FlaggedRepo.branch_changed` 型別從 `Option<(String, String)>` 改成
  `Option<String>`。

- [x] **Step 1: 拿掉 `HashMap`/`HashSet` import**

```rust
use std::collections::VecDeque;
```

- [x] **Step 2: 新增 `EXPECTED_BRANCH` 常數，簡化 `FlaggedRepo`/`describe_reasons`**

```rust
const EXPECTED_BRANCH: &str = "develop";

struct FlaggedRepo {
    path: PathBuf,
    error: bool,
    uncommitted: bool,
    branch_changed: Option<String>,
    ahead: usize,
}

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
```

- [x] **Step 3: `GitRepoPlugin` 移除 `baseline_branches` 欄位跟相關方法**

`struct GitRepoPlugin` 只剩 `watched`/`scan`/`generation` 三個欄位；`new()`
恢復成單行 `Self { watched: Vec::new(), scan: ..., generation: ... }`；刪除
`record_baseline_if_missing`/`prune_baselines` 兩個方法；`add()` 拿掉
`for repo in &repos { self.record_baseline_if_missing(repo); }` 那段，恢復
成單純 `let count = repos_under(&canonical).len();`；`remove()` 拿掉
`self.prune_baselines();`；`clear()` 拿掉
`self.baseline_branches.lock().unwrap().clear();`。

- [x] **Step 4: `scan()` 背景執行緒簡化**

原本要拿 `baseline_branches` 這個 `Arc<Mutex<HashMap<...>>>` 的鎖去查/記基準，
改成單純比較：

```rust
let branch_changed = match &state.branch {
    Some(current) if current == EXPECTED_BRANCH => None,
    Some(current) => Some(current.clone()),
    None => Some("(detached HEAD)".to_string()),
};
```

`scan()` 開頭跟 worker 閉包裡 `Arc::clone(&self.baseline_branches)`/
`Arc::clone(&baseline_branches)` 這兩處 clone 整個拿掉。

- [x] **Step 5: 更新 `MANUAL_TEXT`**

「有異動」第二條規則的說明文字改成「目前 checkout 的 branch 不是
develop……」，注意事項拿掉「基準 branch 不落地存檔」那條，改成「預期的
branch 名稱（develop）是寫死的常數，適用所有監控目錄底下的每個 repo，不能
個別指定」。

- [x] **Step 6: Build + 測試**

Run: `cargo build 2>&1 | tail -60`（clean）
Run: `cargo test gitrepo 2>&1 | tail -20` 跟 `cargo test 2>&1 | tail -6`（全過，
123 個測試，沒有因為拿掉 baseline 機制而需要調整既有的
`status_parsing_tests`）。

另外拿一個丟棄式的真實 git repo 手動確認：在 `develop` 上、工作目錄乾淨 →
`git status --porcelain=v2 --branch` 印出 `# branch.head develop`（符合預期，
不會被列為異動）；切到 `feature/x`、工作目錄一樣乾淨 → 印出
`# branch.head feature/x`（跟 `EXPECTED_BRANCH` 不同，會被列為異動）。驗證
完刪除測試用的 repo，不留痕跡。

- [ ] **Step 7: Commit**

```bash
git add src/plugins/gitrepo.rs docs/superpowers/specs/2026-07-28-gitrepo-branch-detection-design.md docs/superpowers/plans/2026-07-28-gitrepo-branch-detection.md
git commit -m "$(cat <<'EOF'
gitrepo：拿掉「第一次看到的基準」，改成單一寫死的 develop

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```
