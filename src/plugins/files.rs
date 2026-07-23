use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{bail, Context, Result};
use data_encoding::BASE64;

use crate::output::OutputBuffer;
use crate::plugin::{CrossDomainAsk, FileMeta, Plugin, RemoteReply, SharedContext, FILE_CHUNK_SIZE};
use crate::shell::send_cross_domain_request;
use crate::web::PORT;

/// 目前允許 `copy` 操作的資料夾白名單。同時是 same-domain 的 HTTP 端點
/// （`web.rs` 的 `/api/files/{folder}...`）跟跨 domain 的 MQTT 請求
/// （`global.rs` 的 `build_remote_reply`）兩邊都要檢查的同一份清單——不管是
/// 本機發起、還是收到別人的請求，`folder` 都要驗證過在這裡面才能碰，不能被
/// 請求內容的字串繞過去存取任意路徑。目前只有 `music`（跟
/// `plugins::MUSIC_DIR` 剛好是同一個名字），之後要開放別的資料夾就在這裡加。
pub const ALLOWED_FOLDERS: &[&str] = &["music"];

/// `folder`（先確認在白名單內）底下的 `name` 組出實際路徑，`name` 只接受單一
/// 檔名（不能含路徑分隔符或是 `.`/`..`）——不管是本機組出來的路徑、還是收到
/// 別人（哪怕已經通過 AEAD 解密驗證）請求裡帶的檔名，都要過這一關，這是最後
/// 一道防線，不能只因為訊息通過了加密驗證就信任內容本身沒問題。
pub(crate) fn safe_file_path(folder: &str, name: &str) -> Option<PathBuf> {
    if !ALLOWED_FOLDERS.contains(&folder) {
        return None;
    }
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return None;
    }
    Some(Path::new(folder).join(name))
}

const MANUAL_TEXT: &str = "\
files：在裝置之間複製整個資料夾。同網域直接用 HTTP 整檔傳輸；跨 domain 透過
global plugin 既有的 MQTT bridge，一個 chunk 一次請求/回覆的往返慢慢傳（公開
broker 對單則訊息大小有限制，塞不下整個檔案）。

範例：
  copy music to B             把本機 music 資料夾複製到同網域的裝置 B（用
                               device list 查得到的 id）
  copy music from B            反過來，把裝置 B 的 music 資料夾複製過來，覆蓋
                               本機同名檔案
  copy music to domainB/B      跨 domain 版本，語法跟 remote plugin 的
                               connect 一樣（用 global list 查得到的裝置）
  status                       查目前有沒有 copy 在進行、進度到哪、是哪個檔案

注意事項：
  - 目前只開放 music 這個資料夾，其他名字會被拒絕。
  - 是「複製」不是「同步」：只會新增/覆蓋檔案，不會刪除目的地多出來的檔案。
  - 同一時間只能有一個 copy 在跑，進行中再下一次 copy 會被擋掉，先用 status
    確認前一個做完了沒（完成後 status 還是會顯示上一次的結果，直到下一次
    copy 開始才會被蓋掉）。
  - 跨 domain 一個 chunk 只送 4 KiB，檔案越大、往返輪數越多，速度會比同網域
    慢很多——這是遷就公開 MQTT broker 對單則訊息大小的限制，沒有辦法避免。
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    To,
    From,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::To => "to",
            Direction::From => "from",
        }
    }
}

/// `copy` 的目標：跟 `remote` plugin 的 `connect` 沿用同一個語法約定——不含
/// `/` 就是同網域（用 `device list` 查得到的 id 找 ip），含 `/` 就是跨 domain
/// 的 `<domain>/<id>`（用 `global list` 查得到的裝置）。
#[derive(Clone)]
enum CopyTarget {
    Http { ip: String },
    CrossDomain { domain: String, target_id: String },
}

impl CopyTarget {
    fn display(&self, raw: &str) -> String {
        match self {
            CopyTarget::Http { .. } => raw.to_string(),
            CopyTarget::CrossDomain { domain, target_id } => format!("{domain}/{target_id}"),
        }
    }
}

/// 目前（或上一次）的 copy 狀態，`status` 指令跟 panel 都讀這個。`done` 是
/// `true` 時，`error`（`None` 就是成功）才有意義；還在跑的時候 `error` 一律是
/// `None`。特意在完成之後不清掉這個狀態，讓使用者晚一點才下 `status` 也還能
/// 看到上一輪的結果，直到下一次 `copy` 開始才會被蓋掉。
struct TransferStatus {
    folder: String,
    direction: Direction,
    target_display: String,
    current_file: Option<String>,
    /// 目前這個檔案已經傳了幾個 byte、總共幾個 byte——同網域（HTTP 整檔傳輸）
    /// 沒有中間進度可以回報，只會在檔案傳完的那一刻直接跳成
    /// `current_file_total`；跨 domain（一個 chunk 一次往返）則是每個 chunk
    /// 傳完就更新一次，看得到即時進度。
    current_file_done: u64,
    current_file_total: u64,
    completed: usize,
    total: usize,
    done: bool,
    error: Option<String>,
}

pub struct FilesPlugin {
    ctx: SharedContext,
    status: Arc<Mutex<Option<TransferStatus>>>,
}

impl FilesPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx, status: Arc::new(Mutex::new(None)) }
    }

    fn resolve_target(&self, token: &str) -> Result<CopyTarget> {
        if let Some((domain, target_id)) = token.split_once('/') {
            if !self.ctx.lock().unwrap().global.contains_key(token) {
                bail!("沒有這個跨 domain 裝置: {token}（用 global list 查詢目前看得到的裝置）");
            }
            Ok(CopyTarget::CrossDomain { domain: domain.to_string(), target_id: target_id.to_string() })
        } else {
            let ip = self
                .ctx
                .lock()
                .unwrap()
                .devices
                .get(token)
                .map(|entry| entry.report.ip.clone())
                .with_context(|| format!("沒有這個裝置: {token}（用 device list 查詢目前看得到的機器）"))?;
            Ok(CopyTarget::Http { ip })
        }
    }

    /// `copy <folder> to/from <target>`：驗證通過就立刻在背景執行緒開始真正的
    /// 傳輸，這裡只負責檢查（資料夾在白名單裡、沒有其他 copy 正在跑、目標
    /// 解析得出來）跟登記狀態，不做任何網路/檔案 I/O——那些可能要跑很久，不能
    /// 卡在持有共用 `Shell` 鎖的 `dispatch` 裡。
    fn copy(&mut self, folder: &str, direction_word: &str, target_token: &str, out: &OutputBuffer) -> Result<()> {
        if !ALLOWED_FOLDERS.contains(&folder) {
            bail!("不支援的資料夾: {folder}（目前只開放: {}）", ALLOWED_FOLDERS.join(", "));
        }
        let direction = match direction_word {
            "to" => Direction::To,
            "from" => Direction::From,
            other => bail!("copy 第二個參數要是 to 或 from，收到: {other}"),
        };
        {
            let guard = self.status.lock().unwrap();
            if let Some(s) = guard.as_ref()
                && !s.done
            {
                bail!(
                    "已經有一個 copy 正在進行中（{} {} {}，{}/{}），請等它完成，或用 status 查看進度",
                    s.direction.as_str(),
                    s.folder,
                    s.target_display,
                    s.completed,
                    s.total
                );
            }
        }
        let target = self.resolve_target(target_token)?;
        let target_display = target.display(target_token);

        *self.status.lock().unwrap() = Some(TransferStatus {
            folder: folder.to_string(),
            direction,
            target_display: target_display.clone(),
            current_file: None,
            current_file_done: 0,
            current_file_total: 0,
            completed: 0,
            total: 0,
            done: false,
            error: None,
        });
        out.push(&format!(
            "開始 copy {folder} {} {target_display}（背景執行，用 status 查看進度）\n",
            direction.as_str()
        ));

        let status = self.status.clone();
        let ctx = self.ctx.clone();
        let folder_owned = folder.to_string();
        thread::spawn(move || {
            let result = match direction {
                Direction::To => run_push(&ctx, &target, &folder_owned, &status),
                Direction::From => run_pull(&ctx, &target, &folder_owned, &status),
            };
            if let Some(s) = status.lock().unwrap().as_mut() {
                s.done = true;
                s.current_file = None;
                if let Err(err) = result {
                    s.error = Some(format!("{err:#}"));
                }
            }
        });
        Ok(())
    }

    fn status_text(&self) -> String {
        match &*self.status.lock().unwrap() {
            None => "目前沒有任何 copy 動作\n".to_string(),
            Some(s) => {
                let state = if !s.done {
                    "進行中".to_string()
                } else if let Some(err) = &s.error {
                    format!("失敗: {err}")
                } else {
                    "已完成".to_string()
                };
                // 還在跑的時候，「進度」要顯示的是第幾個檔案*正在處理*
                // （`completed + 1`），不是已經處理完的數量——不然明明在傳第一個
                // 檔案，卻顯示「0/2」，看起來像什麼都還沒開始。跑完之後（`done`）
                // 才單純顯示 `completed/total`（`current_file` 這時已經被清成
                // `None`，也沒有「正在處理第幾個」這回事了）。`total == 0`（資料夾
                // 是空的）兩種情況都直接顯示 0/0，不用特別判斷。
                let (shown, total) = if s.total == 0 {
                    (0, 0)
                } else if s.done {
                    (s.completed, s.total)
                } else {
                    ((s.completed + 1).min(s.total), s.total)
                };
                let current_file = match &s.current_file {
                    Some(name) if s.current_file_total > 0 => {
                        format!("{name}（{}/{}）", format_bytes(s.current_file_done), format_bytes(s.current_file_total))
                    }
                    Some(name) => name.clone(),
                    None => "(無)".to_string(),
                };
                format!(
                    "copy {} {} {}\n狀態: {state}\n目前檔案: {current_file}\n進度: {shown}/{total}\n",
                    s.direction.as_str(),
                    s.folder,
                    s.target_display,
                )
            }
        }
    }
}

/// 換成 B/K/M/G 這種好讀的單位，只取到小數點後一位——這裡只是給人看進度用，
/// 不需要精確到位元組。
fn format_bytes(bytes: u64) -> String {
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

/// 開始處理下一個檔案：設定檔名跟已知的總大小（同網域整檔傳輸、跨 domain
/// 分塊傳輸都是一開始就知道總大小——前者是本機 `fs::metadata`，後者是
/// `FileList` 回覆帶回來的），目前已傳大小歸零。
fn set_current_file(status: &Arc<Mutex<Option<TransferStatus>>>, name: &str, total: u64) {
    if let Some(s) = status.lock().unwrap().as_mut() {
        s.current_file = Some(name.to_string());
        s.current_file_done = 0;
        s.current_file_total = total;
    }
}

/// 目前這個檔案又傳了 `delta` 個 byte——跨 domain 分塊傳輸每個 chunk 往返
/// 完成後呼叫一次，同網域整檔傳輸沒有中間進度，不會呼叫這個，直接在傳完那
/// 一刻呼叫 `finish_current_file`。
fn add_current_file_bytes(status: &Arc<Mutex<Option<TransferStatus>>>, delta: u64) {
    if let Some(s) = status.lock().unwrap().as_mut() {
        s.current_file_done += delta;
    }
}

/// 同網域整檔傳輸沒有中間進度可以回報，檔案整個傳完的那一刻直接把「已傳」
/// 補成「總共」，讓 `status` 顯示的是完整、對得起來的數字，而不是卡在 0。
fn finish_current_file(status: &Arc<Mutex<Option<TransferStatus>>>) {
    if let Some(s) = status.lock().unwrap().as_mut() {
        s.current_file_done = s.current_file_total;
    }
}

fn set_total(status: &Arc<Mutex<Option<TransferStatus>>>, total: usize) {
    if let Some(s) = status.lock().unwrap().as_mut() {
        s.total = total;
    }
}

fn bump_completed(status: &Arc<Mutex<Option<TransferStatus>>>) {
    if let Some(s) = status.lock().unwrap().as_mut() {
        s.completed += 1;
    }
}

/// 本機資料夾底下有哪些「檔案」（跳過子目錄，這個功能目前只處理單層、扁平的
/// 資料夾內容，跟 `music`/`notepad` 資料夾的實際用法一致）。
fn list_local_files(dir: &Path) -> Result<Vec<String>> {
    let entries = fs::read_dir(dir).with_context(|| format!("讀取資料夾失敗: {}", dir.display()))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

fn run_push(ctx: &SharedContext, target: &CopyTarget, folder: &str, status: &Arc<Mutex<Option<TransferStatus>>>) -> Result<()> {
    let dir = Path::new(folder);
    let names = list_local_files(dir)?;
    set_total(status, names.len());
    for name in names {
        let path = dir.join(&name);
        let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        set_current_file(status, &name, size);
        match target {
            CopyTarget::Http { ip } => {
                push_file_http(ip, folder, &name, &path)?;
                finish_current_file(status);
            }
            CopyTarget::CrossDomain { domain, target_id } => {
                push_file_mqtt(ctx, domain, target_id, folder, &name, &path, status)?
            }
        }
        bump_completed(status);
    }
    Ok(())
}

fn run_pull(ctx: &SharedContext, target: &CopyTarget, folder: &str, status: &Arc<Mutex<Option<TransferStatus>>>) -> Result<()> {
    fs::create_dir_all(folder).with_context(|| format!("建立資料夾失敗: {folder}"))?;
    let files: Vec<FileMeta> = match target {
        CopyTarget::Http { ip } => list_remote_files_http(ip, folder)?,
        CopyTarget::CrossDomain { domain, target_id } => list_remote_files_mqtt(ctx, domain, target_id, folder)?,
    };
    set_total(status, files.len());
    for meta in files {
        set_current_file(status, &meta.name, meta.size);
        let dest = Path::new(folder).join(&meta.name);
        match target {
            CopyTarget::Http { ip } => {
                pull_file_http(ip, folder, &meta.name, &dest)?;
                finish_current_file(status);
            }
            CopyTarget::CrossDomain { domain, target_id } => {
                pull_file_mqtt(ctx, domain, target_id, folder, &meta, &dest, status)?
            }
        }
        bump_completed(status);
    }
    Ok(())
}

// --- 同網域：整檔透過既有的 /api/files 端點傳輸，不用切 chunk ---

fn push_file_http(ip: &str, folder: &str, name: &str, path: &Path) -> Result<()> {
    let url = format!("http://{ip}:{PORT}/api/files/{folder}/{name}");
    let output = Command::new("curl")
        .args([
            "--silent",
            "--fail",
            "--max-time",
            "120",
            "-X",
            "POST",
            "--data-binary",
            &format!("@{}", path.display()),
            &url,
        ])
        .output()
        .context("執行 curl 失敗")?;
    if !output.status.success() {
        bail!("上傳失敗: {name}");
    }
    Ok(())
}

fn list_remote_files_http(ip: &str, folder: &str) -> Result<Vec<FileMeta>> {
    let url = format!("http://{ip}:{PORT}/api/files/{folder}");
    let output = Command::new("curl")
        .args(["--silent", "--fail", "--max-time", "10", &url])
        .output()
        .context("執行 curl 失敗")?;
    if !output.status.success() {
        bail!("查詢遠端檔案清單失敗");
    }
    let body = String::from_utf8(output.stdout).context("回應不是合法的 UTF-8")?;
    serde_json::from_str(&body).context("回應格式不對")
}

fn pull_file_http(ip: &str, folder: &str, name: &str, dest: &Path) -> Result<()> {
    let url = format!("http://{ip}:{PORT}/api/files/{folder}/{name}");
    let output = Command::new("curl")
        .args(["--silent", "--fail", "--max-time", "120", "-o", &dest.display().to_string(), &url])
        .output()
        .context("執行 curl 失敗")?;
    if !output.status.success() {
        bail!("下載失敗: {name}");
    }
    Ok(())
}

// --- 跨 domain：透過 global 既有的 MQTT bridge，一個 chunk 一次請求/回覆 ---

fn push_file_mqtt(
    ctx: &SharedContext,
    domain: &str,
    target_id: &str,
    folder: &str,
    name: &str,
    path: &Path,
    status: &Arc<Mutex<Option<TransferStatus>>>,
) -> Result<()> {
    let data = fs::read(path).with_context(|| format!("讀取檔案失敗: {}", path.display()))?;
    let mut offset: usize = 0;
    loop {
        let end = (offset + FILE_CHUNK_SIZE).min(data.len());
        let chunk = &data[offset..end];
        let ask = CrossDomainAsk::FilePush {
            target_id: target_id.to_string(),
            folder: folder.to_string(),
            name: name.to_string(),
            offset: offset as u64,
            data: BASE64.encode(chunk),
        };
        match send_cross_domain_request(ctx, domain, ask)? {
            RemoteReply::FilePushAck { .. } => {}
            RemoteReply::Error { message, .. } => bail!(message),
            _ => bail!("收到不符預期的回覆型別"),
        }
        add_current_file_bytes(status, (end - offset) as u64);
        offset = end;
        // 空檔案（`data` 一開始就是空的）也要送這一次「第 0 個 chunk、內容是
        // 空的」請求，讓對面至少建立一個空檔案出來，不能因為迴圈條件一開始就
        // 不成立而整個跳過——所以用 `offset < data.len()` 當繼續條件，而不是
        // 先判斷 `data.is_empty()` 直接略過。
        if offset >= data.len() {
            break;
        }
    }
    Ok(())
}

fn list_remote_files_mqtt(ctx: &SharedContext, domain: &str, target_id: &str, folder: &str) -> Result<Vec<FileMeta>> {
    let ask = CrossDomainAsk::FileList { target_id: target_id.to_string(), folder: folder.to_string() };
    match send_cross_domain_request(ctx, domain, ask)? {
        RemoteReply::FileList { files, .. } => Ok(files),
        RemoteReply::Error { message, .. } => bail!(message),
        _ => bail!("收到不符預期的回覆型別"),
    }
}

/// 逐個 chunk 拉一個檔案，用 `FileList` 已經拿到的 `meta.size` 判斷有沒有拉完
/// ——不靠伺服器另外回報「這是不是最後一塊」，這樣檔案大小剛好是
/// `FILE_CHUNK_SIZE` 整數倍時也不會多一輪去問一個超出檔案範圍的 offset。
fn pull_file_mqtt(
    ctx: &SharedContext,
    domain: &str,
    target_id: &str,
    folder: &str,
    meta: &FileMeta,
    dest: &Path,
    status: &Arc<Mutex<Option<TransferStatus>>>,
) -> Result<()> {
    let mut file = fs::File::create(dest).with_context(|| format!("建立檔案失敗: {}", dest.display()))?;
    if meta.size == 0 {
        return Ok(()); // 空檔案：建立完就結束，不需要真的要任何 chunk。
    }
    let mut offset: u64 = 0;
    while offset < meta.size {
        let ask = CrossDomainAsk::FilePull {
            target_id: target_id.to_string(),
            folder: folder.to_string(),
            name: meta.name.clone(),
            offset,
        };
        let data = match send_cross_domain_request(ctx, domain, ask)? {
            RemoteReply::FileChunk { data, .. } => data,
            RemoteReply::Error { message, .. } => bail!(message),
            _ => bail!("收到不符預期的回覆型別"),
        };
        let bytes = BASE64.decode(data.as_bytes()).context("chunk 不是合法的 base64")?;
        if bytes.is_empty() {
            bail!("遠端回傳空的 chunk（檔案可能在傳輸過程中被改動），已知大小: {}", meta.size);
        }
        file.write_all(&bytes)?;
        offset += bytes.len() as u64;
        add_current_file_bytes(status, bytes.len() as u64);
    }
    Ok(())
}

impl Plugin for FilesPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["copy <folder> to <id>", "copy <folder> from <id>", "status"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "copy" => {
                let folder = args.first().context("copy 需要接資料夾名稱")?;
                let direction = args.get(1).context("copy 需要接 to 或 from")?;
                let target = args.get(2).context("copy 需要接目標裝置的 id")?;
                self.copy(folder, direction, target, out)
            }
            "status" => {
                out.push(&self.status_text());
                Ok(())
            }
            other => bail!("files 不認得指令: {other}"),
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
