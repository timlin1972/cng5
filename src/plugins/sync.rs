use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::plugins::storage::SyncEntry;
use crate::plugins::sync_baseline::{Baseline, BaselineEntry};
use crate::plugins::url_encode_filename;
use crate::web::PORT;

/// 這一輪同步對某個路徑（檔案）或某個目錄該做的事。檔案動作（`classify`）
/// 跟目錄動作（`classify_directories`）是分開算的兩批，執行時檔案動作全部
/// 先跑完才處理目錄動作——見 `run_sync_pass`。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SyncAction {
    PushToRemote { path: String },
    PullFromRemote { path: String },
    DeleteLocal { path: String },
    DeleteRemote { path: String },
    Conflict { path: String },
    MoveLocal { from: String, to: String },
    MoveRemote { from: String, to: String },
    CreateLocalDir { path: String },
    CreateRemoteDir { path: String },
    DeleteLocalDir { path: String },
    DeleteRemoteDir { path: String },
}

struct FileState {
    hash: String,
}

/// 把 `walk_with_hashes` 的結果過濾成「只有檔案、路徑對到雜湊」的對照表，
/// 資料夾條目直接跳過（同步演算法不處理空資料夾，見上方 `SyncAction` 的說
/// 明）。
fn file_states(entries: &[SyncEntry]) -> HashMap<String, FileState> {
    entries
        .iter()
        .filter(|e| !e.is_dir)
        .filter_map(|e| e.hash.clone().map(|hash| (e.path.clone(), FileState { hash })))
        .collect()
}

/// 核心分類邏輯：比對本機清單、對方清單、這個同步對象的 baseline，決定每個
/// 路徑該做的事。純函式，不牽涉任何網路/檔案系統 I/O。
pub(crate) fn classify(local: &[SyncEntry], remote: &[SyncEntry], baseline: &Baseline) -> Vec<SyncAction> {
    let local_states = file_states(local);
    let remote_states = file_states(remote);

    let mut all_paths: HashSet<&String> = HashSet::new();
    all_paths.extend(local_states.keys());
    all_paths.extend(remote_states.keys());
    all_paths.extend(baseline.files.keys());

    let mut paths: Vec<&String> = all_paths.into_iter().collect();
    paths.sort();

    let mut actions = Vec::new();
    for path in paths {
        let local_state = local_states.get(path);
        let remote_state = remote_states.get(path);
        let base = baseline.files.get(path);

        match (local_state, remote_state, base) {
            (Some(_), None, None) => actions.push(SyncAction::PushToRemote { path: path.clone() }),
            (None, Some(_), None) => actions.push(SyncAction::PullFromRemote { path: path.clone() }),
            (None, None, _) => {} // 兩邊都沒有：不管有沒有舊 baseline 紀錄，都無事可做
            (Some(local), None, Some(base)) => {
                if local.hash == base.local_hash {
                    actions.push(SyncAction::DeleteLocal { path: path.clone() });
                } else {
                    // 對方刪了，但本機這邊同時改過——保留本機的修改，當作
                    // 本機端的變更重新推過去。
                    actions.push(SyncAction::PushToRemote { path: path.clone() });
                }
            }
            (None, Some(remote), Some(base)) => {
                if remote.hash == base.remote_hash {
                    actions.push(SyncAction::DeleteRemote { path: path.clone() });
                } else {
                    actions.push(SyncAction::PullFromRemote { path: path.clone() });
                }
            }
            (Some(local), Some(remote), base) => {
                if local.hash == remote.hash {
                    continue; // 內容一樣，不用管有沒有 baseline
                }
                match base {
                    None => actions.push(SyncAction::Conflict { path: path.clone() }),
                    Some(base) => {
                        // 分開比對本機、對方各自跟「自己那一邊上次記錄的值」
                        // 有沒有變——`base.local_hash` 跟 `base.remote_hash`
                        // 本來就可能不一樣（已經確認過的衝突就是這樣故意留
                        // 著），不能假設兩者相等。
                        let local_changed = local.hash != base.local_hash;
                        let remote_changed = remote.hash != base.remote_hash;
                        match (local_changed, remote_changed) {
                            (true, false) => actions.push(SyncAction::PushToRemote { path: path.clone() }),
                            (false, true) => actions.push(SyncAction::PullFromRemote { path: path.clone() }),
                            (false, false) => {
                                // 兩邊都跟上次記錄的一樣——這是先前已經確認、
                                // 故意保留分歧的衝突，不用再產生一次衝突副本。
                                continue;
                            }
                            (true, true) => actions.push(SyncAction::Conflict { path: path.clone() }),
                        }
                    }
                }
            }
        }
    }
    pair_moves(actions, &local_states, &remote_states, baseline)
}

/// 單側改名/搬移配對：把「這輪本來要各自輸出的 `DeleteRemote`」（本機刪了一
/// 個 baseline 有記錄的路徑）跟「這輪本來要各自輸出的 `PushToRemote`、且該
/// 路徑在 baseline 裡完全沒有記錄」（本機的全新檔案，不是單邊修改既有路徑）
/// 依內容 hash 配對，配成功的合併成一個 `MoveRemote`，不再各自輸出。對稱地，
/// `DeleteLocal` 配「baseline 沒有記錄的 `PullFromRemote`」，合併成
/// `MoveLocal`。只在同一側配對（不會拿 `DeleteRemote` 去配
/// `PullFromRemote`），`Conflict`、或「baseline 有記錄的單邊修改」都不參與
/// 配對——只有乾淨的「刪除」跟「全新新增」才可能是同一次改名/搬移的兩端。
fn pair_moves(
    actions: Vec<SyncAction>,
    local_states: &HashMap<String, FileState>,
    remote_states: &HashMap<String, FileState>,
    baseline: &Baseline,
) -> Vec<SyncAction> {
    let local_side = pair_side(
        &actions,
        |a| match a {
            SyncAction::DeleteRemote { path } => baseline.files.get(path).map(|b| (path.clone(), b.local_hash.clone())),
            _ => None,
        },
        |a| match a {
            SyncAction::PushToRemote { path } if !baseline.files.contains_key(path) => {
                local_states.get(path).map(|s| (path.clone(), s.hash.clone()))
            }
            _ => None,
        },
    );
    let remote_side = pair_side(
        &actions,
        |a| match a {
            SyncAction::DeleteLocal { path } => baseline.files.get(path).map(|b| (path.clone(), b.remote_hash.clone())),
            _ => None,
        },
        |a| match a {
            SyncAction::PullFromRemote { path } if !baseline.files.contains_key(path) => {
                remote_states.get(path).map(|s| (path.clone(), s.hash.clone()))
            }
            _ => None,
        },
    );

    let mut moved_away: HashSet<String> = HashSet::new();
    let mut moved_new: HashSet<String> = HashSet::new();
    let mut moves = Vec::new();
    for (from, to) in local_side {
        moved_away.insert(from.clone());
        moved_new.insert(to.clone());
        moves.push(SyncAction::MoveRemote { from, to });
    }
    for (from, to) in remote_side {
        moved_away.insert(from.clone());
        moved_new.insert(to.clone());
        moves.push(SyncAction::MoveLocal { from, to });
    }

    let mut result: Vec<SyncAction> = actions
        .into_iter()
        .filter(|a| match a {
            SyncAction::DeleteRemote { path } | SyncAction::DeleteLocal { path } => !moved_away.contains(path),
            SyncAction::PushToRemote { path } | SyncAction::PullFromRemote { path } => !moved_new.contains(path),
            _ => true,
        })
        .collect();
    result.extend(moves);
    result
}

/// `pair_moves` 的配對核心：把 `actions` 篩成「刪除候選」跟「新增候選」(各
/// 自用 `del_of`/`add_of` 從單一個 `SyncAction` 抽出 `(路徑, hash)`，不符合
/// 就回 `None`)、依 hash 分組、組內依路徑字串排序後依序配對(結果穩定、可
/// 重現)，回傳 `(舊路徑, 新路徑)` 配對清單；配不完的候選不會出現在回傳值
/// 裡，維持原本各自的動作。
fn pair_side(
    actions: &[SyncAction],
    del_of: impl Fn(&SyncAction) -> Option<(String, String)>,
    add_of: impl Fn(&SyncAction) -> Option<(String, String)>,
) -> Vec<(String, String)> {
    let mut deletes: HashMap<String, Vec<String>> = HashMap::new();
    let mut adds: HashMap<String, Vec<String>> = HashMap::new();
    for action in actions {
        if let Some((path, hash)) = del_of(action) {
            deletes.entry(hash).or_default().push(path);
        }
        if let Some((path, hash)) = add_of(action) {
            adds.entry(hash).or_default().push(path);
        }
    }
    let mut pairs = Vec::new();
    for (hash, mut del_paths) in deletes {
        let Some(mut add_paths) = adds.remove(&hash) else { continue };
        del_paths.sort();
        add_paths.sort();
        for (from, to) in del_paths.into_iter().zip(add_paths) {
            pairs.push((from, to));
        }
    }
    pairs
}

/// `classify_directories` 的輸出：目錄動作，加上這一輪雙邊都確認一致、應該
/// （重新）記進 `known_dirs` 的路徑，以及雙邊都已經不存在、應該從
/// `known_dirs` 移除的路徑（不移除的話，之後任何一邊重新建立同名空目錄會被
/// 誤判成「known_dirs 有記錄、只剩一邊有」而錯誤觸發刪除，而不是建立）。
#[derive(Debug, PartialEq)]
pub(crate) struct DirClassification {
    pub(crate) actions: Vec<SyncAction>,
    pub(crate) confirmed_dirs: Vec<String>,
    pub(crate) stale_dirs: Vec<String>,
}

fn dir_paths(entries: &[SyncEntry]) -> HashSet<String> {
    entries.iter().filter(|e| e.is_dir).map(|e| e.path.clone()).collect()
}

/// `entries` 裡有沒有任何檔案的路徑是 `dir_path` 底下（遞迴）——目錄建立/
/// 刪除的保護規則用這個判斷「這輪還不能動這個目錄」，見 `classify_directories`。
fn has_files_under(entries: &[SyncEntry], dir_path: &str) -> bool {
    let prefix = format!("{dir_path}/");
    entries.iter().any(|e| !e.is_dir && e.path.starts_with(&prefix))
}

/// 目錄層級的分類：只看目錄本身的建立/刪除，不牽涉任何檔案內容，純函式。
/// 用這一輪已經抓到的雙邊 manifest（`local`/`remote`）判斷，不額外重新查詢。
pub(crate) fn classify_directories(
    local: &[SyncEntry],
    remote: &[SyncEntry],
    known_dirs: &HashSet<String>,
) -> DirClassification {
    let local_dirs = dir_paths(local);
    let remote_dirs = dir_paths(remote);

    let mut all_dirs: HashSet<&String> = HashSet::new();
    all_dirs.extend(local_dirs.iter());
    all_dirs.extend(remote_dirs.iter());
    all_dirs.extend(known_dirs.iter());
    let mut dirs: Vec<&String> = all_dirs.into_iter().collect();
    dirs.sort();

    let mut actions = Vec::new();
    let mut confirmed_dirs = Vec::new();
    let mut stale_dirs = Vec::new();

    for path in dirs {
        let has_local = local_dirs.contains(path);
        let has_remote = remote_dirs.contains(path);
        let known = known_dirs.contains(path);

        // 保護規則：這輪的雙邊 manifest，只要有一邊底下還有檔案，就完全跳過
        // 這個目錄的建立/刪除判斷，讓檔案層級的動作先處理（見設計文件「執行
        // 順序」一節）。
        if has_files_under(local, path) || has_files_under(remote, path) {
            continue;
        }

        match (has_local, has_remote, known) {
            (true, true, _) => confirmed_dirs.push(path.clone()),
            (true, false, false) => actions.push(SyncAction::CreateRemoteDir { path: path.clone() }),
            (false, true, false) => actions.push(SyncAction::CreateLocalDir { path: path.clone() }),
            (true, false, true) => actions.push(SyncAction::DeleteLocalDir { path: path.clone() }),
            (false, true, true) => actions.push(SyncAction::DeleteRemoteDir { path: path.clone() }),
            (false, false, true) => stale_dirs.push(path.clone()),
            (false, false, false) => {} // 從沒被雙邊同時擁有過，無事可做
        }
    }

    DirClassification { actions, confirmed_dirs, stale_dirs }
}

/// 跟一個同步對象「怎麼溝通」的抽象介面——分類演算法（`classify`）算完要做
/// 什麼事之後，實際執行搬檔/建資料夾/刪除都透過這個介面，不管對方是同網域
/// （`HttpTransport`）還是跨 domain（Task 6 的 `CrossDomainTransport`），呼叫端
/// 的程式碼完全一樣。物件安全（沒有泛型方法、沒有 `Self: Sized` 限制），可以
/// 用 `Box<dyn SyncTransport>` 依角色動態選擇實作。
pub(crate) trait SyncTransport {
    /// 取得對方目前整棵 `storage/` 樹的清單（含每個檔案的 hash）。
    fn manifest(&self) -> Result<Vec<SyncEntry>>;
    /// 把對方 `path` 這個檔案下載到本機的 `dest` 路徑；`expected_size` 是這個
    /// 檔案在對方那邊的大小（來自觸發這次下載的 manifest 條目），`HttpTransport`
    /// 用不到（HTTP 下載讀到 EOF 就結束），`CrossDomainTransport`（Task 6）需要
    /// 靠它判斷分段下載何時結束。
    fn download_to(&self, path: &str, expected_size: u64, dest: &Path) -> Result<()>;
    /// 把本機 `src` 這個檔案上傳成對方的 `path`。
    fn upload_from(&self, path: &str, src: &Path) -> Result<()>;
    /// 在對方建立 `path` 這個資料夾。
    fn mkdir(&self, path: &str) -> Result<()>;
    /// 刪除對方的 `path`（檔案或資料夾，`recursive` 語意跟 `storage` plugin
    /// 的 `remove` 一致）。
    fn delete(&self, path: &str, recursive: bool) -> Result<()>;
    /// 在對方那邊直接把 `from` 重新命名/搬移成 `to`，不重傳內容——呼叫端要
    /// 先確保 `to` 的上層目錄在對方那邊存在（見 `ensure_remote_parent_dirs`），
    /// 這個方法本身不會自動建立父目錄。
    fn rename(&self, from: &str, to: &str) -> Result<()>;
}

/// 同網域的同步對象，透過既有的 `/api/storage/...` 端點溝通——不重做傳輸層，
/// 直接呼叫 `storage` plugin 已經做好的 HTTP API。
pub(crate) struct HttpTransport {
    pub(crate) ip: String,
}

impl SyncTransport for HttpTransport {
    fn manifest(&self) -> Result<Vec<SyncEntry>> {
        let url = format!("http://{}:{PORT}/api/storage/sync-manifest", self.ip);
        let output = Command::new("curl")
            .args(["--silent", "--fail", "--max-time", "30", &url])
            .output()
            .context("執行 curl 失敗")?;
        if !output.status.success() {
            bail!("查詢 sync-manifest 失敗");
        }
        let body = String::from_utf8(output.stdout).context("回應不是合法的 UTF-8")?;
        serde_json::from_str(&body).context("回應格式不對")
    }

    fn download_to(&self, path: &str, _expected_size: u64, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("建立資料夾失敗: {}", parent.display()))?;
        }
        let url = format!("http://{}:{PORT}/api/storage/download?path={}", self.ip, url_encode_filename(path));
        let output = Command::new("curl")
            .args(["--silent", "--fail", "--max-time", "120", "-o", &dest.display().to_string(), &url])
            .output()
            .context("執行 curl 失敗")?;
        if !output.status.success() {
            bail!("下載失敗: {path}");
        }
        Ok(())
    }

    fn upload_from(&self, path: &str, src: &Path) -> Result<()> {
        let url = format!("http://{}:{PORT}/api/storage/upload?path={}", self.ip, url_encode_filename(path));
        let output = Command::new("curl")
            .args([
                "--silent",
                "--fail",
                "--max-time",
                "120",
                "-X",
                "POST",
                "--data-binary",
                &format!("@{}", src.display()),
                &url,
            ])
            .output()
            .context("執行 curl 失敗")?;
        if !output.status.success() {
            bail!("上傳失敗: {path}");
        }
        Ok(())
    }

    fn mkdir(&self, path: &str) -> Result<()> {
        let url = format!("http://{}:{PORT}/api/storage/mkdir?path={}", self.ip, url_encode_filename(path));
        let output = Command::new("curl")
            .args(["--silent", "--fail", "--max-time", "10", "-X", "POST", &url])
            .output()
            .context("執行 curl 失敗")?;
        if !output.status.success() {
            bail!("建立資料夾失敗: {path}");
        }
        Ok(())
    }

    fn delete(&self, path: &str, recursive: bool) -> Result<()> {
        let url =
            format!("http://{}:{PORT}/api/storage/delete?path={}&recursive={recursive}", self.ip, url_encode_filename(path));
        let output = Command::new("curl")
            .args(["--silent", "--fail", "--max-time", "30", "-X", "POST", &url])
            .output()
            .context("執行 curl 失敗")?;
        if !output.status.success() {
            bail!("刪除失敗: {path}");
        }
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let url = format!(
            "http://{}:{PORT}/api/storage/rename?from={}&to={}",
            self.ip,
            url_encode_filename(from),
            url_encode_filename(to)
        );
        let output = Command::new("curl")
            .args(["--silent", "--fail", "--max-time", "10", "-X", "POST", &url])
            .output()
            .context("執行 curl 失敗")?;
        if !output.status.success() {
            bail!("搬移/重新命名失敗: {from} -> {to}");
        }
        Ok(())
    }
}

use std::io::Write;

use crate::plugin::{CrossDomainAsk, RemoteReply, SharedContext};
use crate::shell::send_cross_domain_request;

/// 跨 domain 的同步對象，透過 Task 4 新增的 `CrossDomainAsk::Storage*` 系列，走
/// 既有的 MQTT 中繼機制（`send_cross_domain_request`）溝通，做法比照
/// `files.rs` 的 `list_remote_files_mqtt`/`push_file_mqtt`/`pull_file_mqtt`
/// （4KB chunk、逐段往返），只是路徑換成 storage 底下的巢狀相對路徑，而且不需
/// 要 `target_id`（見 Task 4 的設計說明：跨 domain 同步是 server 對 server，
/// 對方直接回答自己的 `storage/`，不用再往下 relay 給某個特定裝置）。
pub(crate) struct CrossDomainTransport {
    pub(crate) ctx: SharedContext,
    pub(crate) domain: String,
}

impl SyncTransport for CrossDomainTransport {
    fn manifest(&self) -> Result<Vec<SyncEntry>> {
        let mut entries = Vec::new();
        loop {
            let ask = CrossDomainAsk::StorageManifest { offset: entries.len() };
            match send_cross_domain_request(&self.ctx, &self.domain, ask)? {
                RemoteReply::StorageManifest { entries: page, total, .. } => {
                    if page.is_empty() {
                        break;
                    }
                    entries.extend(page);
                    if entries.len() >= total {
                        break;
                    }
                }
                RemoteReply::Error { message, .. } => bail!(message),
                _ => bail!("收到不符預期的回覆型別"),
            }
        }
        Ok(entries)
    }

    fn download_to(&self, path: &str, expected_size: u64, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("建立資料夾失敗: {}", parent.display()))?;
        }
        let mut file = fs::File::create(dest).with_context(|| format!("建立檔案失敗: {}", dest.display()))?;
        if expected_size == 0 {
            return Ok(()); // 空檔案：建立完就結束，不需要真的要任何 chunk。
        }
        let mut offset: u64 = 0;
        while offset < expected_size {
            let ask = CrossDomainAsk::StorageFilePull { path: path.to_string(), offset };
            let data = match send_cross_domain_request(&self.ctx, &self.domain, ask)? {
                RemoteReply::FileChunk { data, .. } => data,
                RemoteReply::Error { message, .. } => bail!(message),
                _ => bail!("收到不符預期的回覆型別"),
            };
            let bytes = data_encoding::BASE64.decode(data.as_bytes()).context("chunk 不是合法的 base64")?;
            if bytes.is_empty() {
                bail!("遠端回傳空的 chunk（檔案可能在傳輸過程中被改動），已知大小: {expected_size}");
            }
            file.write_all(&bytes)?;
            offset += bytes.len() as u64;
        }
        Ok(())
    }

    fn upload_from(&self, path: &str, src: &Path) -> Result<()> {
        let data = fs::read(src).with_context(|| format!("讀取檔案失敗: {}", src.display()))?;
        let mut offset: usize = 0;
        loop {
            let end = (offset + crate::plugin::FILE_CHUNK_SIZE).min(data.len());
            let chunk = &data[offset..end];
            let ask = CrossDomainAsk::StorageFilePush {
                path: path.to_string(),
                offset: offset as u64,
                data: data_encoding::BASE64.encode(chunk),
            };
            match send_cross_domain_request(&self.ctx, &self.domain, ask)? {
                RemoteReply::FilePushAck { .. } => {}
                RemoteReply::Error { message, .. } => bail!(message),
                _ => bail!("收到不符預期的回覆型別"),
            }
            offset = end;
            // 空檔案也要送這一次「第 0 個 chunk、內容是空的」請求，讓對面至少
            // 建立一個空檔案出來，見 `files.rs` 的 `push_file_mqtt` 同樣的理由。
            if offset >= data.len() {
                break;
            }
        }
        Ok(())
    }

    fn mkdir(&self, path: &str) -> Result<()> {
        let ask = CrossDomainAsk::StorageMkdir { path: path.to_string() };
        match send_cross_domain_request(&self.ctx, &self.domain, ask)? {
            RemoteReply::Ack { .. } => Ok(()),
            RemoteReply::Error { message, .. } => bail!(message),
            _ => bail!("收到不符預期的回覆型別"),
        }
    }

    fn delete(&self, path: &str, recursive: bool) -> Result<()> {
        let ask = CrossDomainAsk::StorageDelete { path: path.to_string(), recursive };
        match send_cross_domain_request(&self.ctx, &self.domain, ask)? {
            RemoteReply::Ack { .. } => Ok(()),
            RemoteReply::Error { message, .. } => bail!(message),
            _ => bail!("收到不符預期的回覆型別"),
        }
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let ask = CrossDomainAsk::StorageRename { from: from.to_string(), to: to.to_string() };
        match send_cross_domain_request(&self.ctx, &self.domain, ask)? {
            RemoteReply::Ack { .. } => Ok(()),
            RemoteReply::Error { message, .. } => bail!(message),
            _ => bail!("收到不符預期的回覆型別"),
        }
    }
}

/// 從 unix epoch 秒數算出 UTC 曆法日期字串 `YYYY-MM-DD`，純算術、不依賴任何
/// 時間函式庫——沿用這個專案 `build.rs` 算編譯時間戳同樣的原則：不為了這裡
/// 需要一個日期字串就額外引入日期時間 crate。演算法出處：Howard Hinnant 的
/// `civil_from_days`（把「距離 1970-01-01 過了幾天」換算成西曆年月日，是這個
/// 換算方向被廣泛驗證過的標準寫法）。
pub(crate) fn epoch_to_utc_date(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86400) as i64;
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// 把一個路徑改成帶衝突標記的新檔名，標記插在副檔名前面（沒有副檔名就接在
/// 檔名後面），資料夾前綴維持不變。
pub(crate) fn conflict_copy_name(path: &str, label: &str, date: &str) -> String {
    let (dir, filename) = match path.rsplit_once('/') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, path),
    };
    let (stem, ext) = match filename.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (filename, None),
    };
    let new_name = match ext {
        Some(ext) => format!("{stem} (衝突自 {label}，{date}).{ext}"),
        None => format!("{stem} (衝突自 {label}，{date})"),
    };
    match dir {
        Some(dir) => format!("{dir}/{new_name}"),
        None => new_name,
    }
}

use std::time::{SystemTime, UNIX_EPOCH};

use crate::plugins::sync_baseline::{load_baseline, save_baseline, SYNC_STATE_DIR};
use crate::plugins::{walk_with_hashes, STORAGE_DIR};

/// 一輪同步對某個對象的結果摘要，`sync status` 指令跟 panel 都讀這個。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SyncOutcome {
    pub(crate) pushed: usize,
    pub(crate) pulled: usize,
    pub(crate) deleted_local: usize,
    pub(crate) deleted_remote: usize,
    pub(crate) conflicts: usize,
    pub(crate) moved_local: usize,
    pub(crate) moved_remote: usize,
    pub(crate) error: Option<String>,
}

fn find_hash<'a>(entries: &'a [SyncEntry], path: &str) -> Option<&'a str> {
    entries.iter().find(|e| e.path == path && !e.is_dir).and_then(|e| e.hash.as_deref())
}

fn find_size(entries: &[SyncEntry], path: &str) -> u64 {
    entries.iter().find(|e| e.path == path && !e.is_dir).map(|e| e.size).unwrap_or(0)
}

/// 處理一個真衝突：把對方的版本下載成本機一份帶衝突標記的新檔案，同時把本機
/// 原本的版本上傳成對方一份帶衝突標記的新檔案，兩邊各自原本的檔案都不動。
fn resolve_conflict(
    transport: &dyn SyncTransport,
    path: &str,
    my_label: &str,
    partner_label: &str,
    remote_size: u64,
) -> Result<()> {
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let date = epoch_to_utc_date(epoch);
    let local_root = Path::new(STORAGE_DIR);

    let local_copy_path = conflict_copy_name(path, partner_label, &date);
    let local_copy_dest = local_root.join(&local_copy_path);
    transport.download_to(path, remote_size, &local_copy_dest)?;

    let remote_copy_path = conflict_copy_name(path, my_label, &date);
    let local_original = local_root.join(path);
    transport.upload_from(&remote_copy_path, &local_original)?;

    Ok(())
}

/// 對 `path` 這個目的地確保上層資料夾在對方那邊存在——`upload_from`/
/// `HttpTransport` 底層的 `/api/storage/upload` 不會自動建立中間目錄（見
/// `storage_upload` 的文件），推新檔案進一個新的子資料夾之前要先在對方那邊
/// `mkdir` 過。`known_dirs` 由呼叫端在整個 `run_sync_pass` 的動作迴圈之外建立、
/// 持續傳入同一個可變集合——只在資料夾還沒出現在這個集合裡時才呼叫 `mkdir`，
/// 呼叫完馬上把它記進去。這樣同一輪同步裡，先推的檔案剛建立好的資料夾，後面
/// 推進同一個資料夾的檔案就不會重複呼叫 `mkdir`（對方的 `make_dir`/
/// `StorageMkdir` 對已存在的路徑會回報錯誤，不是單純的 no-op）。
fn ensure_remote_parent_dirs(
    transport: &dyn SyncTransport,
    path: &str,
    known_dirs: &mut std::collections::HashSet<String>,
) -> Result<()> {
    let Some((dir, _name)) = path.rsplit_once('/') else { return Ok(()) };
    let mut acc = String::new();
    for segment in dir.split('/') {
        acc = if acc.is_empty() { segment.to_string() } else { format!("{acc}/{segment}") };
        if !known_dirs.contains(&acc) {
            transport.mkdir(&acc)?;
            known_dirs.insert(acc.clone());
        }
    }
    Ok(())
}

/// 對一個同步對象跑完整的一輪：算本機清單、拿對方清單、讀 baseline、分類、
/// 執行、把成功的部分寫回 baseline。任何一步失敗都記錄在回傳的 `error`
/// 裡，不會 `panic`，讓背景執行緒可以繼續處理下一個對象。
pub(crate) fn run_sync_pass(
    _ctx: &SharedContext,
    transport: &dyn SyncTransport,
    partner_key: &str,
    my_label: &str,
    partner_label: &str,
) -> SyncOutcome {
    let mut outcome = SyncOutcome::default();
    let local_root = Path::new(STORAGE_DIR);

    let local = match walk_with_hashes(local_root) {
        Ok(l) => l,
        Err(err) => {
            outcome.error = Some(format!("讀取本機清單失敗: {err:#}"));
            return outcome;
        }
    };
    let remote = match transport.manifest() {
        Ok(r) => r,
        Err(err) => {
            outcome.error = Some(format!("取得對方清單失敗: {err:#}"));
            return outcome;
        }
    };

    let state_dir = Path::new(SYNC_STATE_DIR);
    let mut baseline = load_baseline(state_dir, partner_key);
    let actions = classify(&local, &remote, &baseline);

    // 從 `remote` 快照建立這一輪的初始已知資料夾集合，之後在迴圈裡持續更新
    // （見 `ensure_remote_parent_dirs` 的文件）——不能每次呼叫都重新從
    // `remote` 建一份，那份快照在整輪同步開始時就固定了，不會反映這一輪已經
    // 建立過的資料夾。
    let mut known_remote_dirs: std::collections::HashSet<String> =
        remote.iter().filter(|e| e.is_dir).map(|e| e.path.clone()).collect();

    for action in actions {
        match action {
            SyncAction::PushToRemote { path } => {
                let result = ensure_remote_parent_dirs(transport, &path, &mut known_remote_dirs)
                    .and_then(|_| transport.upload_from(&path, &local_root.join(&path)));
                match result {
                    Ok(()) => {
                        if let Some(hash) = find_hash(&local, &path) {
                            baseline.files.insert(
                                path,
                                BaselineEntry { local_hash: hash.to_string(), remote_hash: hash.to_string() },
                            );
                        }
                        outcome.pushed += 1;
                    }
                    Err(err) => outcome.error = Some(format!("推送 {path} 失敗: {err:#}")),
                }
            }
            SyncAction::PullFromRemote { path } => {
                let dest = local_root.join(&path);
                let size = find_size(&remote, &path);
                match transport.download_to(&path, size, &dest) {
                    Ok(()) => {
                        if let Some(hash) = find_hash(&remote, &path) {
                            baseline.files.insert(
                                path,
                                BaselineEntry { local_hash: hash.to_string(), remote_hash: hash.to_string() },
                            );
                        }
                        outcome.pulled += 1;
                    }
                    Err(err) => outcome.error = Some(format!("拉取 {path} 失敗: {err:#}")),
                }
            }
            SyncAction::DeleteLocal { path } => match crate::plugins::remove(&local_root.join(&path), true) {
                Ok(()) => {
                    baseline.files.remove(&path);
                    outcome.deleted_local += 1;
                }
                Err(err) => outcome.error = Some(format!("刪除本機 {path} 失敗: {err:#}")),
            },
            SyncAction::DeleteRemote { path } => match transport.delete(&path, true) {
                Ok(()) => {
                    baseline.files.remove(&path);
                    outcome.deleted_remote += 1;
                }
                Err(err) => outcome.error = Some(format!("刪除對方 {path} 失敗: {err:#}")),
            },
            SyncAction::Conflict { path } => {
                let remote_size = find_size(&remote, &path);
                match resolve_conflict(transport, &path, my_label, partner_label, remote_size) {
                    Ok(()) => {
                        // 只更新原本這個路徑的 baseline（承認兩邊從此故意分歧），
                        // 不替新產生的兩份衝突副本建立 baseline 條目——見本
                        // task 開頭的「已知取捨」說明。
                        if let (Some(local_hash), Some(remote_hash)) =
                            (find_hash(&local, &path), find_hash(&remote, &path))
                        {
                            baseline.files.insert(
                                path,
                                BaselineEntry {
                                    local_hash: local_hash.to_string(),
                                    remote_hash: remote_hash.to_string(),
                                },
                            );
                        }
                        outcome.conflicts += 1;
                    }
                    Err(err) => outcome.error = Some(format!("處理衝突 {path} 失敗: {err:#}")),
                }
            }
            SyncAction::MoveRemote { from, to } => {
                let result = ensure_remote_parent_dirs(transport, &to, &mut known_remote_dirs)
                    .and_then(|_| transport.rename(&from, &to));
                match result {
                    Ok(()) => {
                        if let Some(hash) = find_hash(&local, &to) {
                            baseline.files.remove(&from);
                            baseline.files.insert(
                                to,
                                BaselineEntry { local_hash: hash.to_string(), remote_hash: hash.to_string() },
                            );
                        }
                        outcome.moved_remote += 1;
                    }
                    Err(err) => outcome.error = Some(format!("搬移對方 {from} -> {to} 失敗: {err:#}")),
                }
            }
            SyncAction::MoveLocal { from, to } => {
                let from_path = local_root.join(&from);
                let to_path = local_root.join(&to);
                let result = (|| -> Result<()> {
                    if let Some(parent) = to_path.parent() {
                        fs::create_dir_all(parent)
                            .with_context(|| format!("建立資料夾失敗: {}", parent.display()))?;
                    }
                    crate::plugins::rename_path(&from_path, &to_path)
                })();
                match result {
                    Ok(()) => {
                        if let Some(hash) = find_hash(&remote, &to) {
                            baseline.files.remove(&from);
                            baseline.files.insert(
                                to,
                                BaselineEntry { local_hash: hash.to_string(), remote_hash: hash.to_string() },
                            );
                        }
                        outcome.moved_local += 1;
                    }
                    Err(err) => outcome.error = Some(format!("搬移本機 {from} -> {to} 失敗: {err:#}")),
                }
            }
            SyncAction::CreateLocalDir { .. }
            | SyncAction::CreateRemoteDir { .. }
            | SyncAction::DeleteLocalDir { .. }
            | SyncAction::DeleteRemoteDir { .. } => {
                unreachable!(
                    "classify() never produces directory actions — classify_directories() does, and its \
                     output is executed in a separate block after this loop (see a later task in this plan)"
                )
            }
        }
    }

    if let Err(err) = save_baseline(state_dir, partner_key, &baseline) {
        outcome.error = Some(format!("寫入 baseline 失敗: {err:#}"));
    }
    outcome
}

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::output::OutputBuffer;
use crate::plugin::{merged_global_view, Plugin};
use crate::sysinfo;

/// 背景輪詢間隔——獨立於 `system` plugin 的 10 秒裝置回報間隔（`REPORT_INTERVAL`），
/// 同步整棵樹（含每個檔案重新算 hash）比單純回報裝置狀態貴得多，不用共用同一個
/// 頻率。
const SYNC_POLL_INTERVAL: Duration = Duration::from_secs(60);

const MANUAL_TEXT: &str = "\
sync：把本機 storage plugin 管理的整棵 storage/ 樹，跟其他裝置雙向同步（含刪除
傳遞、衝突處理），完全複用 system plugin 既有的 client/server/domain 角色，不用
另外設定要跟誰同步。

沒有手動觸發指令：只有這台裝置是 mode server 時，才會啟動背景輪詢（預設每 60
秒），自動對同 domain 底下每一個已知的 client、以及 global registry 看得到的
每一個別的 domain 的 server，各自跑一次同步。client 角色完全被動，不會主動做
任何事，最終還是會透過 server 端的輪詢自動同步到，只是有接力延遲。

指令：
  status              列出每個同步對象上次同步的時間、結果、搬了幾個檔案、
                       刪了幾個、產生幾個衝突副本、偵測到幾個改名/搬移

真的衝突（雙方自上次同步後都改過同一個檔案）不會覆蓋任何一邊，兩邊都會保留
自己原本的檔案，並且各自多一份帶「(衝突自 <對方>，日期)」標記的對方版本副
本，需要使用者自己手動整理。

單側改名/搬移一個檔案時，會被偵測出來直接在對方那邊重新命名，不會重傳整份
內容——資料夾整個改名，是靠底下每個檔案各自被偵測成改名疊加達成效果。
";

#[derive(Clone, Debug, Default)]
struct PartnerStatus {
    last_run: Option<Instant>,
    outcome: SyncOutcome,
}

/// 沒有任何持久化狀態以外的欄位——`statuses` 只是給 `status` 指令/panel 顯示
/// 用的執行期摘要，重啟就沒了（真正需要跨重啟保留的是 baseline，見 Task 2，
/// 那個已經寫到磁碟）。
pub struct SyncPlugin {
    #[allow(dead_code)]
    ctx: SharedContext,
    statuses: Arc<Mutex<HashMap<String, PartnerStatus>>>,
}

impl SyncPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        let statuses = Arc::new(Mutex::new(HashMap::new()));
        Self::spawn_engine(ctx.clone(), statuses.clone());
        Self { ctx, statuses }
    }

    fn spawn_engine(ctx: SharedContext, statuses: Arc<Mutex<HashMap<String, PartnerStatus>>>) {
        thread::spawn(move || loop {
            let is_server = ctx.lock().unwrap().is_server;
            if is_server {
                run_all_partners(&ctx, &statuses);
            }
            thread::sleep(SYNC_POLL_INTERVAL);
        });
    }

    fn status_text(&self) -> String {
        let statuses = self.statuses.lock().unwrap();
        if statuses.is_empty() {
            return "目前還沒有任何同步紀錄\n".to_string();
        }
        let mut keys: Vec<&String> = statuses.keys().collect();
        keys.sort();
        let mut text = String::new();
        for key in keys {
            let status = &statuses[key];
            let elapsed = status.last_run.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            let result = match &status.outcome.error {
                Some(err) => format!("失敗: {err}"),
                None => "成功".to_string(),
            };
            text.push_str(&format!(
                "{key}: {result}（{elapsed} 秒前）推 {} 拉 {} 刪本機 {} 刪對方 {} 衝突 {} 搬本機 {} 搬對方 {}\n",
                status.outcome.pushed,
                status.outcome.pulled,
                status.outcome.deleted_local,
                status.outcome.deleted_remote,
                status.outcome.conflicts,
                status.outcome.moved_local,
                status.outcome.moved_remote,
            ));
        }
        text
    }
}

impl Plugin for SyncPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["status"]
    }

    fn dispatch(&mut self, cmd: &str, _args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "status" => {
                out.push(&self.status_text());
                Ok(())
            }
            other => bail!("sync 不認得指令: {other}"),
        }
    }

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

fn record_status(
    statuses: &Arc<Mutex<HashMap<String, PartnerStatus>>>,
    key: &str,
    outcome: SyncOutcome,
) {
    statuses
        .lock()
        .unwrap()
        .insert(key.to_string(), PartnerStatus { last_run: Some(Instant::now()), outcome });
}

/// 一輪背景輪詢：對同 domain 底下每一個已知的 client、以及看得到的每一個別
/// 的 domain（套用字典序 tie-break），各自跑一次 `run_sync_pass`，並把結果
/// 記進 `statuses`、寫進 activities log。任何一個對象失敗都不影響其他對象
/// 繼續跑。
fn run_all_partners(ctx: &SharedContext, statuses: &Arc<Mutex<HashMap<String, PartnerStatus>>>) {
    let my_hostname = sysinfo::hostname();

    let (clients, my_domain, peer_domains): (Vec<(String, String)>, Option<String>, Vec<String>) = {
        let inner = ctx.lock().unwrap();
        let clients: Vec<(String, String)> = inner
            .devices
            .iter()
            .filter(|(id, _)| id.as_str() != my_hostname.as_str())
            .map(|(id, entry)| (id.clone(), entry.report.ip.clone()))
            .collect();
        let my_domain = inner.domain_name.clone();
        let peer_domains: HashSet<String> = merged_global_view(&inner).into_iter().map(|item| item.domain).collect();
        (clients, my_domain, peer_domains.into_iter().collect())
    };

    for (client_id, ip) in clients {
        let transport = HttpTransport { ip };
        let partner_key = format!("client-{client_id}");
        let outcome = run_sync_pass(ctx, &transport, &partner_key, &my_hostname, &client_id);
        log_outcome(ctx, &partner_key, &outcome);
        record_status(statuses, &partner_key, outcome);
    }

    if let Some(my_domain) = &my_domain {
        for peer_domain in peer_domains {
            if my_domain.as_str() >= peer_domain.as_str() {
                // Tie-break：兩個 domain 互相看得到彼此時，只有名稱字典序比較
                // 小的那一方才主動發起，避免兩邊同時互相發起造成競態（見設計
                // 文件「拓撲與觸發時機」一節）。`>=`（不是單純 `>`）這個等號
                // 也順便處理了另一件事：`merged_global_view` 回傳的清單裡本來
                // 就包含「自己 domain 底下的裝置」這一份（用自己的
                // `domain_name` 當 `domain` 欄位），`peer_domain == my_domain`
                // 這個情況一定會被這行擋掉，不會誤把自己的 domain 當成一個要
                // 主動發起同步的跨 domain 對象。
                continue;
            }
            let transport = CrossDomainTransport { ctx: ctx.clone(), domain: peer_domain.clone() };
            let partner_key = format!("domain-{peer_domain}");
            let outcome = run_sync_pass(ctx, &transport, &partner_key, my_domain, &peer_domain);
            log_outcome(ctx, &partner_key, &outcome);
            record_status(statuses, &partner_key, outcome);
        }
    }
}

/// 只在這一輪真的有事發生（有搬檔/刪除/衝突，或失敗）時才寫進 activities
/// log——每 60 秒輪詢一次、每個對象都寫一筆「推0拉0刪本機0刪對方0衝突0」的話
/// 幾乎全是雜訊，會把真正的事件淹沒，所以完全沒動靜的一輪就不記錄。
fn log_outcome(ctx: &SharedContext, partner_key: &str, outcome: &SyncOutcome) {
    let had_activity = outcome.pushed > 0
        || outcome.pulled > 0
        || outcome.deleted_local > 0
        || outcome.deleted_remote > 0
        || outcome.conflicts > 0
        || outcome.moved_local > 0
        || outcome.moved_remote > 0
        || outcome.error.is_some();
    if !had_activity {
        return;
    }
    let detail = match &outcome.error {
        Some(err) => format!("{partner_key} 同步失敗: {err}"),
        None => format!(
            "{partner_key} 同步完成：推 {} 拉 {} 刪本機 {} 刪對方 {} 衝突 {} 搬本機 {} 搬對方 {}",
            outcome.pushed,
            outcome.pulled,
            outcome.deleted_local,
            outcome.deleted_remote,
            outcome.conflicts,
            outcome.moved_local,
            outcome.moved_remote,
        ),
    };
    ctx.lock().unwrap().log_activity("sync", detail);
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::plugins::storage::SyncEntry;
    use crate::plugins::sync_baseline::{Baseline, BaselineEntry};

    fn file(path: &str, hash: &str) -> SyncEntry {
        SyncEntry { path: path.to_string(), is_dir: false, size: 10, modified: 0, hash: Some(hash.to_string()) }
    }

    /// 每個項目是 `(路徑, local_hash, remote_hash)`——正常同步完成後兩者相等，
    /// 但已經確認過的衝突會刻意留下不同的值，測試裡分開指定才能覆蓋到兩種
    /// 情況。
    fn baseline_of(entries: &[(&str, &str, &str)]) -> Baseline {
        let files = entries
            .iter()
            .map(|(path, local_hash, remote_hash)| {
                (
                    path.to_string(),
                    BaselineEntry { local_hash: local_hash.to_string(), remote_hash: remote_hash.to_string() },
                )
            })
            .collect();
        Baseline { files, known_dirs: HashSet::new() }
    }

    #[test]
    fn new_local_file_pushes_to_remote() {
        let local = vec![file("new.txt", "h1")];
        let remote = vec![];
        let actions = classify(&local, &remote, &Baseline::default());
        assert_eq!(actions, vec![SyncAction::PushToRemote { path: "new.txt".to_string() }]);
    }

    #[test]
    fn new_remote_file_pulls_from_remote() {
        let local = vec![];
        let remote = vec![file("new.txt", "h1")];
        let actions = classify(&local, &remote, &Baseline::default());
        assert_eq!(actions, vec![SyncAction::PullFromRemote { path: "new.txt".to_string() }]);
    }

    #[test]
    fn unchanged_on_both_sides_produces_no_action() {
        let local = vec![file("same.txt", "h1")];
        let remote = vec![file("same.txt", "h1")];
        let baseline = baseline_of(&[("same.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert!(actions.is_empty());
    }

    #[test]
    fn local_only_change_pushes_to_remote() {
        let local = vec![file("f.txt", "h2")]; // 本機改成 h2
        let remote = vec![file("f.txt", "h1")]; // 對方還是舊的 h1
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]); // 上次同步時雙方都是 h1
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::PushToRemote { path: "f.txt".to_string() }]);
    }

    #[test]
    fn remote_only_change_pulls_from_remote() {
        let local = vec![file("f.txt", "h1")];
        let remote = vec![file("f.txt", "h2")];
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::PullFromRemote { path: "f.txt".to_string() }]);
    }

    #[test]
    fn both_sides_changed_to_same_content_is_no_action() {
        let local = vec![file("f.txt", "h2")];
        let remote = vec![file("f.txt", "h2")]; // 剛好改成一樣
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert!(actions.is_empty());
    }

    #[test]
    fn both_sides_changed_differently_is_conflict() {
        let local = vec![file("f.txt", "h2")];
        let remote = vec![file("f.txt", "h3")];
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::Conflict { path: "f.txt".to_string() }]);
    }

    #[test]
    fn both_sides_independently_created_same_name_different_content_is_conflict() {
        let local = vec![file("new.txt", "hA")];
        let remote = vec![file("new.txt", "hB")];
        let actions = classify(&local, &remote, &Baseline::default()); // 沒有 baseline
        assert_eq!(actions, vec![SyncAction::Conflict { path: "new.txt".to_string() }]);
    }

    #[test]
    fn both_sides_independently_created_same_name_same_content_is_no_action() {
        let local = vec![file("new.txt", "hA")];
        let remote = vec![file("new.txt", "hA")];
        let actions = classify(&local, &remote, &Baseline::default());
        assert!(actions.is_empty());
    }

    #[test]
    fn remote_deleted_unchanged_local_file_deletes_local() {
        let local = vec![file("gone.txt", "h1")]; // 本機沒動過
        let remote = vec![]; // 對方刪掉了
        let baseline = baseline_of(&[("gone.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::DeleteLocal { path: "gone.txt".to_string() }]);
    }

    #[test]
    fn local_deleted_unchanged_remote_file_deletes_remote() {
        let local = vec![]; // 本機刪掉了
        let remote = vec![file("gone.txt", "h1")]; // 對方沒動過
        let baseline = baseline_of(&[("gone.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::DeleteRemote { path: "gone.txt".to_string() }]);
    }

    #[test]
    fn remote_deleted_but_local_edited_keeps_local_edit_by_pushing() {
        // 對方把這個路徑刪了，但本機這邊在同一時間反而編輯過它——保留本機的
        // 修改（推過去），不要因為對方刪除就跟著刪掉本機也改過的內容。
        let local = vec![file("f.txt", "h2")]; // 本機改過
        let remote = vec![]; // 對方刪了
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::PushToRemote { path: "f.txt".to_string() }]);
    }

    #[test]
    fn local_deleted_but_remote_edited_keeps_remote_edit_by_pulling() {
        let local = vec![]; // 本機刪了
        let remote = vec![file("f.txt", "h2")]; // 對方改過
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::PullFromRemote { path: "f.txt".to_string() }]);
    }

    #[test]
    fn both_sides_deleted_produces_no_action() {
        let local = vec![];
        let remote = vec![];
        let baseline = baseline_of(&[("gone.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert!(actions.is_empty());
    }

    #[test]
    fn no_baseline_and_content_differs_is_conservatively_a_conflict() {
        // 對應「重啟後完全沒有 baseline」的情境：兩邊都有、內容不一樣、又沒有
        // baseline 可以判斷是誰改的，保守當成衝突處理。
        let local = vec![file("f.txt", "hA")];
        let remote = vec![file("f.txt", "hB")];
        let actions = classify(&local, &remote, &Baseline::default());
        assert_eq!(actions, vec![SyncAction::Conflict { path: "f.txt".to_string() }]);
    }

    #[test]
    fn already_acknowledged_conflict_produces_no_action() {
        // baseline 記錄了「local 是 hA、remote 是 hB」——代表上一輪已經處理過
        // 這個衝突（兩邊故意留下不同內容），這一輪雙方都還是各自原本的值，
        // 不應該再被判定成新衝突、重複產生衝突副本。
        let local = vec![file("f.txt", "hA")];
        let remote = vec![file("f.txt", "hB")];
        let baseline = baseline_of(&[("f.txt", "hA", "hB")]);
        let actions = classify(&local, &remote, &baseline);
        assert!(actions.is_empty());
    }

    #[test]
    fn acknowledged_conflict_then_local_changes_again_pushes_to_remote() {
        // 已經確認過的衝突（baseline: local=hA, remote=hB）之後，本機這邊又
        // 改了一次（hC）——這是相對 baseline 的單向變更，應該推過去，不是
        // 重新判定成衝突。
        let local = vec![file("f.txt", "hC")];
        let remote = vec![file("f.txt", "hB")];
        let baseline = baseline_of(&[("f.txt", "hA", "hB")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::PushToRemote { path: "f.txt".to_string() }]);
    }

    #[test]
    fn acknowledged_conflict_then_both_change_again_is_a_new_conflict() {
        let local = vec![file("f.txt", "hC")];
        let remote = vec![file("f.txt", "hD")];
        let baseline = baseline_of(&[("f.txt", "hA", "hB")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::Conflict { path: "f.txt".to_string() }]);
    }

    #[test]
    fn multiple_independent_paths_each_classified_separately() {
        let local = vec![file("a.txt", "h1"), file("b.txt", "h1")];
        let remote = vec![file("b.txt", "h1")]; // a.txt 只有本機有，b.txt 兩邊一樣
        let actions = classify(&local, &remote, &Baseline::default());
        assert_eq!(actions, vec![SyncAction::PushToRemote { path: "a.txt".to_string() }]);
    }

    #[test]
    fn epoch_to_utc_date_known_values() {
        assert_eq!(epoch_to_utc_date(0), "1970-01-01");
        assert_eq!(epoch_to_utc_date(1785031586), "2026-07-26");
    }

    #[test]
    fn conflict_copy_name_inserts_marker_before_extension() {
        assert_eq!(
            conflict_copy_name("photos/beach.jpg", "office-pc", "2026-07-26"),
            "photos/beach (衝突自 office-pc，2026-07-26).jpg"
        );
    }

    #[test]
    fn conflict_copy_name_handles_no_extension() {
        assert_eq!(conflict_copy_name("README", "office-pc", "2026-07-26"), "README (衝突自 office-pc，2026-07-26)");
    }

    #[test]
    fn conflict_copy_name_handles_root_level_file() {
        assert_eq!(
            conflict_copy_name("notes.txt", "branch-b", "2026-07-26"),
            "notes (衝突自 branch-b，2026-07-26).txt"
        );
    }

    #[test]
    fn conflict_copy_name_preserves_directory_prefix() {
        let name = conflict_copy_name("a/b/c/deep.txt", "x", "2026-01-01");
        assert_eq!(name, "a/b/c/deep (衝突自 x，2026-01-01).txt");
    }

    /// 只實作測試會用到的 `mkdir`，其他方法一律 `unimplemented!`——這個假
    /// transport 專門用來模擬真實 `make_dir`/`StorageMkdir` 對「已存在的路徑」
    /// 回報錯誤的行為，藉此證明 `ensure_remote_parent_dirs` 不會在同一輪同步
    /// 裡對同一個資料夾重複呼叫 `mkdir`。
    struct MkdirTrackingTransport {
        created: RefCell<HashSet<String>>,
    }

    impl SyncTransport for MkdirTrackingTransport {
        fn manifest(&self) -> Result<Vec<SyncEntry>> {
            unimplemented!("not exercised by this test")
        }
        fn download_to(&self, _path: &str, _expected_size: u64, _dest: &Path) -> Result<()> {
            unimplemented!("not exercised by this test")
        }
        fn upload_from(&self, _path: &str, _src: &Path) -> Result<()> {
            unimplemented!("not exercised by this test")
        }
        fn mkdir(&self, path: &str) -> Result<()> {
            let mut created = self.created.borrow_mut();
            if created.contains(path) {
                bail!("資料夾已存在（模擬 make_dir 對已存在路徑回報的錯誤）: {path}");
            }
            created.insert(path.to_string());
            Ok(())
        }
        fn delete(&self, _path: &str, _recursive: bool) -> Result<()> {
            unimplemented!("not exercised by this test")
        }
        fn rename(&self, _from: &str, _to: &str) -> Result<()> {
            unimplemented!("not exercised by this test")
        }
    }

    #[test]
    fn ensure_remote_parent_dirs_is_idempotent_within_one_pass() {
        let transport = MkdirTrackingTransport { created: RefCell::new(HashSet::new()) };
        let mut known_dirs: HashSet<String> = HashSet::new();
        ensure_remote_parent_dirs(&transport, "photos/2026/a.jpg", &mut known_dirs).unwrap();
        // 第二個檔案進的是這一輪才剛建立的同一個資料夾——修好之前，這一行會
        // 因為重複呼叫 mkdir("photos") / mkdir("photos/2026") 而失敗。
        ensure_remote_parent_dirs(&transport, "photos/2026/b.jpg", &mut known_dirs).unwrap();
    }

    #[test]
    fn ensure_remote_parent_dirs_seeded_with_existing_dirs_skips_them() {
        let transport = MkdirTrackingTransport { created: RefCell::new(HashSet::new()) };
        let mut known_dirs: HashSet<String> = ["existing".to_string()].into_iter().collect();
        // "existing" 已經在 known_dirs 裡（模擬從 remote manifest 建好的初始
        // 狀態），不應該再呼叫一次 mkdir。
        ensure_remote_parent_dirs(&transport, "existing/new-file.txt", &mut known_dirs).unwrap();
        assert!(!transport.created.borrow().contains("existing"));
    }

    fn dir(path: &str) -> SyncEntry {
        SyncEntry { path: path.to_string(), is_dir: true, size: 0, modified: 0, hash: None }
    }

    #[test]
    fn new_empty_dir_only_local_creates_remote_dir() {
        let local = vec![dir("newdir")];
        let remote = vec![];
        let result = classify_directories(&local, &remote, &HashSet::new());
        assert_eq!(result.actions, vec![SyncAction::CreateRemoteDir { path: "newdir".to_string() }]);
        assert!(result.confirmed_dirs.is_empty());
        assert!(result.stale_dirs.is_empty());
    }

    #[test]
    fn new_empty_dir_only_remote_creates_local_dir() {
        let local = vec![];
        let remote = vec![dir("newdir")];
        let result = classify_directories(&local, &remote, &HashSet::new());
        assert_eq!(result.actions, vec![SyncAction::CreateLocalDir { path: "newdir".to_string() }]);
    }

    #[test]
    fn known_dir_missing_from_remote_deletes_local_dir() {
        // known_dirs 有紀錄（上一輪雙邊都有）、這輪 remote 找不到這個目錄了、
        // 雙邊底下都沒有檔案 → 傳遞刪除到 local，讓 local 跟上 remote 已經
        // 刪除的狀態。
        let local = vec![dir("olddir")];
        let remote = vec![];
        let known_dirs: HashSet<String> = ["olddir".to_string()].into_iter().collect();
        let result = classify_directories(&local, &remote, &known_dirs);
        assert_eq!(result.actions, vec![SyncAction::DeleteLocalDir { path: "olddir".to_string() }]);
    }

    #[test]
    fn known_dir_missing_from_local_deletes_remote_dir() {
        let local = vec![];
        let remote = vec![dir("olddir")];
        let known_dirs: HashSet<String> = ["olddir".to_string()].into_iter().collect();
        let result = classify_directories(&local, &remote, &known_dirs);
        assert_eq!(result.actions, vec![SyncAction::DeleteRemoteDir { path: "olddir".to_string() }]);
    }

    #[test]
    fn dir_guard_skips_when_remote_still_has_files_under_it() {
        // photos 在 local 這輪已經完全消失（含檔案），但 remote 的 manifest
        // 還沒反映出檔案層級的刪除（下一輪才會）——保護規則：remote 底下還有
        // 檔案，這輪完全跳過這個目錄的建立/刪除判斷。
        let local = vec![];
        let remote = vec![dir("photos"), file("photos/img.jpg", "h1")];
        let known_dirs: HashSet<String> = ["photos".to_string()].into_iter().collect();
        let result = classify_directories(&local, &remote, &known_dirs);
        assert!(result.actions.is_empty());
        assert!(result.confirmed_dirs.is_empty());
        assert!(result.stale_dirs.is_empty());
    }

    #[test]
    fn dir_guard_skips_when_local_still_has_files_under_it() {
        let local = vec![dir("photos"), file("photos/img.jpg", "h1")];
        let remote = vec![];
        let known_dirs: HashSet<String> = ["photos".to_string()].into_iter().collect();
        let result = classify_directories(&local, &remote, &known_dirs);
        assert!(result.actions.is_empty());
    }

    #[test]
    fn both_sides_have_new_dir_confirms_without_conflict() {
        // 雙邊各自獨立新建同名空目錄，known_dirs 沒有記錄——不是衝突，直接
        // 確認一致，之後應該被記進 known_dirs，不輸出任何動作。
        let local = vec![dir("shared")];
        let remote = vec![dir("shared")];
        let result = classify_directories(&local, &remote, &HashSet::new());
        assert!(result.actions.is_empty());
        assert_eq!(result.confirmed_dirs, vec!["shared".to_string()]);
    }

    #[test]
    fn both_sides_missing_previously_known_dir_marks_stale() {
        // 雙邊都已經不再有這個目錄了（先前各自都刪過），known_dirs 裡的舊
        // 紀錄應該被清掉，不然之後任何一邊重新建立同名空目錄時，會被誤判成
        // 「known_dirs 有記錄、只剩一邊有」而錯誤地觸發刪除，而不是建立。
        let local = vec![];
        let remote = vec![];
        let known_dirs: HashSet<String> = ["gone".to_string()].into_iter().collect();
        let result = classify_directories(&local, &remote, &known_dirs);
        assert!(result.actions.is_empty());
        assert_eq!(result.stale_dirs, vec!["gone".to_string()]);
    }

    #[test]
    fn dir_present_on_both_sides_and_known_produces_no_action() {
        let local = vec![dir("stable")];
        let remote = vec![dir("stable")];
        let known_dirs: HashSet<String> = ["stable".to_string()].into_iter().collect();
        let result = classify_directories(&local, &remote, &known_dirs);
        assert!(result.actions.is_empty());
        assert_eq!(result.confirmed_dirs, vec!["stable".to_string()]);
        assert!(result.stale_dirs.is_empty());
    }

    #[test]
    fn local_rename_pairs_into_move_remote() {
        // 本機把 old.jpg 改名成 new.jpg（同一份內容 h1）：baseline 有
        // old.jpg，這一輪 local 沒有 old.jpg、有 new.jpg（baseline 沒有
        // new.jpg 的記錄）。不應該各自變成 DeleteRemote/PushToRemote，應該
        // 合併成一個 MoveRemote。
        let local = vec![file("new.jpg", "h1")];
        let remote = vec![file("old.jpg", "h1")];
        let baseline = baseline_of(&[("old.jpg", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::MoveRemote { from: "old.jpg".to_string(), to: "new.jpg".to_string() }]);
    }

    #[test]
    fn remote_rename_pairs_into_move_local() {
        let local = vec![file("old.jpg", "h1")];
        let remote = vec![file("new.jpg", "h1")];
        let baseline = baseline_of(&[("old.jpg", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::MoveLocal { from: "old.jpg".to_string(), to: "new.jpg".to_string() }]);
    }

    #[test]
    fn duplicate_hash_rename_pairs_by_sorted_path() {
        // 兩張內容完全相同的照片（都是 h1）同時被改名：a-old/b-old 都刪了，
        // a-new/b-new 都是新的。依路徑字串排序後依序配對：a-new 配 a-old，
        // b-new 配 b-old（不是隨機順序）。
        let local = vec![file("a-new.jpg", "h1"), file("b-new.jpg", "h1")];
        let remote = vec![file("a-old.jpg", "h1"), file("b-old.jpg", "h1")];
        let baseline = baseline_of(&[("a-old.jpg", "h1", "h1"), ("b-old.jpg", "h1", "h1")]);
        let mut actions = classify(&local, &remote, &baseline);
        actions.sort_by_key(|a| match a {
            SyncAction::MoveRemote { from, .. } => from.clone(),
            _ => String::new(),
        });
        assert_eq!(
            actions,
            vec![
                SyncAction::MoveRemote { from: "a-old.jpg".to_string(), to: "a-new.jpg".to_string() },
                SyncAction::MoveRemote { from: "b-old.jpg".to_string(), to: "b-new.jpg".to_string() },
            ]
        );
    }

    #[test]
    fn duplicate_hash_uneven_counts_falls_back_for_leftover() {
        // 兩個刪除候選（同 hash h1）只有一個新增候選——只配對得出一對 Move，
        // 剩下那個刪除候選維持原本的 DeleteRemote。
        let local = vec![file("a-new.jpg", "h1")];
        let remote = vec![file("a-old.jpg", "h1"), file("b-old.jpg", "h1")];
        let baseline = baseline_of(&[("a-old.jpg", "h1", "h1"), ("b-old.jpg", "h1", "h1")]);
        let mut actions = classify(&local, &remote, &baseline);
        actions.sort_by_key(|a| match a {
            SyncAction::MoveRemote { from, .. } => format!("0{from}"),
            SyncAction::DeleteRemote { path } => format!("1{path}"),
            _ => String::new(),
        });
        assert_eq!(
            actions,
            vec![
                SyncAction::MoveRemote { from: "a-old.jpg".to_string(), to: "a-new.jpg".to_string() },
                SyncAction::DeleteRemote { path: "b-old.jpg".to_string() },
            ]
        );
    }

    #[test]
    fn conflict_paths_are_not_paired_into_moves() {
        // f.txt 在 baseline 有記錄、雙邊都改過而且改成不一樣（真衝突），同一
        // 輪 local 又冒出一個內容剛好等於 baseline 舊值的全新路徑——不應該把
        // 這個衝突誤配成「改名」。
        let local = vec![file("f.txt", "h2"), file("new.txt", "h1")];
        let remote = vec![file("f.txt", "h3")];
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let mut actions = classify(&local, &remote, &baseline);
        actions.sort_by_key(|a| format!("{a:?}"));
        assert_eq!(
            actions,
            vec![
                SyncAction::Conflict { path: "f.txt".to_string() },
                SyncAction::PushToRemote { path: "new.txt".to_string() },
            ]
        );
    }

    #[test]
    fn single_side_modification_is_not_treated_as_new_for_pairing() {
        // f.txt 是 baseline 已經記錄的路徑、本機單邊修改過（PushToRemote），
        // 不是「全新路徑」，不該被拿去跟任何刪除候選配對成 Move。
        let local = vec![file("f.txt", "h2"), file("moved-away.txt", "hX")];
        let remote = vec![file("gone.txt", "hX")];
        let baseline = baseline_of(&[("f.txt", "h1", "h1"), ("gone.txt", "hX", "hX")]);
        let actions = classify(&local, &remote, &baseline);
        // moved-away.txt (新路徑, hash hX) 跟 gone.txt (刪除候選, hash hX)
        // 內容相同、都在「local 這一側」——這才是預期會配對成的 Move；f.txt
        // 的單邊修改必須維持是 PushToRemote，不能被誤吃進配對邏輯。
        let mut sorted = actions.clone();
        sorted.sort_by_key(|a| format!("{a:?}"));
        assert_eq!(
            sorted,
            vec![
                SyncAction::MoveRemote { from: "gone.txt".to_string(), to: "moved-away.txt".to_string() },
                SyncAction::PushToRemote { path: "f.txt".to_string() },
            ]
        );
    }

    #[test]
    fn cross_side_delete_and_add_do_not_pair() {
        // 刪除候選在 remote 那一側（DeleteLocal：local 還留著 gone.txt、內容
        // 沒變，remote 已經沒有它了），新增候選在 local 那一側（PushToRemote：
        // new.txt 是全新路徑）——就算 hash 剛好一樣，這兩個屬於不同的配對
        // bucket（DeleteLocal 只會去配 PullFromRemote，不會去配
        // PushToRemote），不該被誤配成 Move。
        let local = vec![file("gone.txt", "h1"), file("new.txt", "h1")];
        let remote = vec![];
        let baseline = baseline_of(&[("gone.txt", "h1", "h1")]);
        let mut actions = classify(&local, &remote, &baseline);
        actions.sort_by_key(|a| format!("{a:?}"));
        assert_eq!(
            actions,
            vec![
                SyncAction::DeleteLocal { path: "gone.txt".to_string() },
                SyncAction::PushToRemote { path: "new.txt".to_string() },
            ]
        );
    }
}
