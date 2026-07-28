use std::collections::HashMap;
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

/// 下載完成的檔案都放在這個資料夾底下（相對於程式執行時的工作目錄）。web 那邊
/// 的播放/刪除功能（見 `web.rs`）直接讀寫這個資料夾，不透過 `Shell`/`MusicPlugin`
/// （純粹是檔案操作，沒有需要協調的狀態），但共用同一個路徑常數，不要各自定義
/// 一份、之後改名字漏改到。
pub(crate) const MUSIC_DIR: &str = "music";

/// 字幕（拿來當歌詞用）的語言優先順序：`download` 抓字幕時用這個組出
/// `--sub-langs`，web 那邊找歌詞檔（`web.rs` 的 `find_lyrics_path`）也要照
/// 同一個順序去找對應檔名——如果一部影片同時有好幾種語言的字幕，`yt-dlp`
/// 會全部抓下來（不是只抓排最前面那個就好），檔名各自帶語言代碼，所以查找端
/// 必須自己照優先順序一個一個試著找檔名，不能對資料夾做無排序的掃描亂配到
/// 第一個剛好符合的字幕檔——不然可能配到抓下來的英文字幕，而不是中文字幕。
pub(crate) const SUBTITLE_LANG_PRIORITY: [&str; 6] = ["ja", "zh-TW", "zh-Hant", "zh-Hans", "zh", "en"];

/// `music/` 底下的 `name` 組出實際路徑，`name` 只接受單一檔名（不能含路徑
/// 分隔符或是 `.`/`..`）——不管是本機組出來的路徑、還是收到別人（哪怕已經
/// 通過 AEAD 解密驗證）請求裡帶的檔名，都要過這一關，這是最後一道防線，不能
/// 只因為訊息通過了加密驗證就信任內容本身沒問題。之前這是給任意白名單資料夾
/// 共用的（`files` plugin 的 `ALLOWED_FOLDERS`），現在 `music` 是唯一會被
/// 跨裝置複製的資料夾，直接寫死不用再帶 `folder` 參數。
pub(crate) fn safe_music_copy_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return None;
    }
    Some(Path::new(MUSIC_DIR).join(name))
}

/// 把檔名塞進 `/api/music/copy/{name}` 這個 URL 之前先 percent-encode——
/// 檔名可能含空白、中文、全形標點這類字元，原封不動塞進 URL 字串會讓 curl
/// 送出的請求列本身就是不合法的（空白提前截斷網址，或整段位元組不是合法的
/// URL 語法），實測遇過的真實案例是含有全形冒號、書名號、括號的中文歌名下載
/// 失敗。只留英數字跟 `-_.~` 不編碼（RFC 3986 的 unreserved 字元集），其餘每個
/// 位元組都編成 `%XX`。伺服器那端（`web.rs` 的 `music_copy_download`/
/// `music_copy_upload`，透過 actix 的 `web::Path` extractor）會自動
/// percent-decode 回原始檔名，不需要額外處理。`sync`/`global` plugin 的其他
/// 端點也共用這個函式，不是 `music` 專屬。
pub(crate) fn url_encode_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `manual` 指令的說明。
const MANUAL_TEXT: &str = "\
music：用 yt-dlp 把 YouTube 網址轉成 mp3 存下來，順便試著抓字幕當歌詞；也能把
music/ 資料夾複製到其他裝置，或反過來拉過來。

範例：
  download https://youtu.be/xxxxxxxxxxx   背景下載+轉檔，不會卡住畫面
  list                                    已下載的檔案 + 進行中/失敗的下載狀態
  copy to B                               把本機 music 資料夾複製到同網域的
                                           裝置 B（用 device list 查得到的 id）
  copy from B                             反過來，把裝置 B 的 music 資料夾複製
                                           過來，覆蓋本機同名檔案
  copy to domainB/B                       跨 domain 版本，語法跟 remote plugin
                                           的 connect 一樣（用 global list 查
                                           得到的裝置）
  status                                  查目前有沒有 copy 在進行、進度到哪、
                                           是哪個檔案

download 是丟到背景執行緒跑的（轉檔可能要花不少時間），下指令後馬上就能繼續做
其他事，用 list 查看目前狀態；下載完成的檔案直接看檔案清單即可，不會重複列在
「進行中」那邊。字幕依序試 ja/zh-TW/zh-Hant/zh-Hans/zh/en，抓不到就算了，不影響
音訊本身下載成不成功。

copy 是「複製」不是「同步」：只會新增/覆蓋檔案，不會刪除目的地多出來的檔案。
目的地已經有同名且大小一樣的檔案就跳過不重傳（用檔案大小當快速判斷，不逐位元組
比對內容），重跑同一個 copy 只會補傳真的缺少/大小不同的檔案。同一時間只能有
一個 copy 在跑，進行中再下一次 copy 會被擋掉，先用 status 確認前一個做完了沒
（完成後 status 還是會顯示上一次的結果，直到下一次 copy 開始才會被蓋掉）。跨
domain 一個 chunk 只送 4 KiB，檔案越大、往返輪數越多，速度會比同網域慢很多——
這是遷就公開 MQTT broker 對單則訊息大小的限制，沒有辦法避免。
";

#[derive(Clone)]
enum DownloadStatus {
    InProgress,
    Done,
    Failed(String),
}

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
    direction: Direction,
    target_display: String,
    /// 目前這個檔案已經傳了幾個 byte、總共幾個 byte——同網域（HTTP 整檔傳輸）
    /// 沒有中間進度可以回報，只會在檔案傳完的那一刻直接跳成
    /// `current_file_total`；跨 domain（一個 chunk 一次往返）則是每個 chunk
    /// 傳完就更新一次，看得到即時進度。
    current_file: Option<String>,
    current_file_done: u64,
    current_file_total: u64,
    completed: usize,
    total: usize,
    done: bool,
    error: Option<String>,
}

pub struct MusicPlugin {
    ctx: SharedContext,
    /// 每個下載目標（YouTube 網址）目前的狀態，背景執行緒（`download` 開的）
    /// 抓完寫回這裡；`download`/`list`/`panel_text` 都只讀，不含任何耗時操作，
    /// 不會卡住持有 `Shell` 鎖的那個執行緒——跟 `WeatherPlugin` 抓天氣資料
    /// 同樣的考量，只是這裡「耗時操作」換成下載影片轉檔，時間更長更不能等。
    downloads: Arc<Mutex<HashMap<String, DownloadStatus>>>,
    /// `copy`/`status` 指令用的跨裝置複製狀態，跟 `downloads` 各自獨立追蹤
    /// （複製 `music/` 資料夾跟下載新歌曲是兩件互不相干的事，沒有互斥的必要）。
    copy_status: Arc<Mutex<Option<TransferStatus>>>,
}

impl MusicPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx, downloads: Arc::new(Mutex::new(HashMap::new())), copy_status: Arc::new(Mutex::new(None)) }
    }

    /// `download <url>`：在背景執行緒跑 `yt-dlp` 把這個 YouTube 網址轉成 mp3
    /// （`-x --audio-format mp3` 只留聲音、`--audio-quality 0` 是 yt-dlp/ffmpeg
    /// 那邊「品質優先」的最高設定，會挑得到的最佳音訊來源轉；`--embed-thumbnail`
    /// 把影片縮圖嵌成 mp3 的封面圖，YouTube 縮圖通常是 webp，mp3 的封面圖
    /// 慣例要 jpg/png，所以額外用 `--convert-thumbnails jpg` 轉一次格式再嵌），
    /// 存到 `music/` 資料夾。順便用 `--write-subs`（只抓真人上傳的字幕，不加
    /// `--write-auto-sub`——YouTube 自動語音辨識歌唱內容常常不準）試著抓字幕
    /// 當歌詞，依序試 `--sub-langs` 列出的語言，抓不到就算了（不是每支影片都有
    /// 字幕，這不影響音訊本身下載成不成功）；轉成 `.srt`（帶時間戳，`web.rs`
    /// 那邊 `/api/music/file/{name}/lyrics` 會解析拿來做同步歌詞）。丟背景
    /// 執行緒而不是當場等它做完，是因為下載+轉檔可能要花不少時間（比這個 app
    /// 其他 plugin 的外部呼叫慢得多），當場等的話會拿著 `Shell` 的鎖卡住其他人
    /// （GUI/CLI/web）一整段時間。
    fn download(&mut self, target: &str, out: &OutputBuffer) -> Result<()> {
        let target = target.trim();
        if target.is_empty() {
            bail!("download 需要接 YouTube 網址");
        }
        std::fs::create_dir_all(MUSIC_DIR).context("建立 music 資料夾失敗")?;

        self.downloads.lock().unwrap().insert(target.to_string(), DownloadStatus::InProgress);
        out.push(&format!("開始下載: {target}（背景執行，用 list 查看進度）\n"));

        let target_owned = target.to_string();
        let downloads = self.downloads.clone();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            ctx.lock().unwrap().log_activity("external", format!("yt-dlp download {target_owned}"));
            let sub_langs = SUBTITLE_LANG_PRIORITY.join(",");
            let status = match Command::new("yt-dlp")
                .args([
                    "-x",
                    "--audio-format",
                    "mp3",
                    "--audio-quality",
                    "0",
                    "--embed-thumbnail",
                    "--convert-thumbnails",
                    "jpg",
                    "--write-subs",
                    "--sub-langs",
                    &sub_langs,
                    "--convert-subs",
                    "srt",
                    "-o",
                    &format!("{MUSIC_DIR}/%(title)s.%(ext)s"),
                    &target_owned,
                ])
                .output()
            {
                Ok(output) if output.status.success() => DownloadStatus::Done,
                Ok(output) => {
                    let message = String::from_utf8_lossy(&output.stderr)
                        .lines()
                        .last()
                        .unwrap_or("未知錯誤")
                        .to_string();
                    DownloadStatus::Failed(message)
                }
                Err(err) => DownloadStatus::Failed(format!("找不到 yt-dlp: {err}")),
            };
            downloads.lock().unwrap().insert(target_owned, status);
        });
        Ok(())
    }

    /// `list` 指令跟 `panel_text()` 共用的內容：`music/` 資料夾裡已經下載好的
    /// 檔案，再接著列出目前追蹤中的下載狀態（進行中/失敗；成功的下載完成後
    /// 已經在檔案清單裡看得到，不重複列）。panel 顯示的就是這一份，跟 `list`
    /// 指令看到的東西一樣。只列 `.mp3`——`download` 現在會順便存一份 `.srt`
    /// 歌詞字幕檔在旁邊（見 `Self::download`），那個是歌曲的附屬品，不是
    /// 一首獨立的歌，不該也被當成清單裡的一個項目。
    fn list_text(&self) -> String {
        let mut lines = Vec::new();
        match std::fs::read_dir(MUSIC_DIR) {
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.to_ascii_lowercase().ends_with(".mp3"))
                    .collect();
                names.sort();
                if names.is_empty() {
                    lines.push("(music 資料夾目前是空的)".to_string());
                } else {
                    lines.extend(names);
                }
            }
            Err(_) => lines.push("(music 資料夾還不存在，還沒下載過任何東西)".to_string()),
        }

        for (target, status) in self.downloads.lock().unwrap().iter() {
            match status {
                DownloadStatus::InProgress => lines.push(format!("下載中: {target}")),
                DownloadStatus::Failed(message) => lines.push(format!("下載失敗: {target}（{message}）")),
                DownloadStatus::Done => {} // 已經在上面的檔案清單裡看得到了。
            }
        }
        lines.join("\n")
    }

    fn list(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push(&format!("{}\n", self.list_text()));
        Ok(())
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

    /// `copy to/from <target>`：驗證通過就立刻在背景執行緒開始真正的傳輸，
    /// 這裡只負責檢查（沒有其他 copy 正在跑、目標解析得出來）跟登記狀態，不做
    /// 任何網路/檔案 I/O——那些可能要跑很久，不能卡在持有共用 `Shell` 鎖的
    /// `dispatch` 裡。
    fn copy(&mut self, direction_word: &str, target_token: &str, out: &OutputBuffer) -> Result<()> {
        let direction = match direction_word {
            "to" => Direction::To,
            "from" => Direction::From,
            other => bail!("copy 第一個參數要是 to 或 from，收到: {other}"),
        };
        {
            let guard = self.copy_status.lock().unwrap();
            if let Some(s) = guard.as_ref()
                && !s.done
            {
                bail!(
                    "已經有一個 copy 正在進行中（{} {}，{}/{}），請等它完成，或用 status 查看進度",
                    s.direction.as_str(),
                    s.target_display,
                    s.completed,
                    s.total
                );
            }
        }
        let target = self.resolve_target(target_token)?;
        let target_display = target.display(target_token);

        *self.copy_status.lock().unwrap() = Some(TransferStatus {
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
            "開始 copy {} {target_display}（背景執行，用 status 查看進度）\n",
            direction.as_str()
        ));

        let status = self.copy_status.clone();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            let result = match direction {
                Direction::To => run_push(&ctx, &target, &status),
                Direction::From => run_pull(&ctx, &target, &status),
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

    fn copy_status_text(&self) -> String {
        match &*self.copy_status.lock().unwrap() {
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
                    "copy {} {}\n狀態: {state}\n目前檔案: {current_file}\n進度: {shown}/{total}\n",
                    s.direction.as_str(),
                    s.target_display,
                )
            }
        }
    }
}

impl Plugin for MusicPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["download <youtube_url>", "list", "copy to <id>", "copy from <id>", "status"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "download" => self.download(args.first().context("download 需要一個 YouTube 網址")?, out),
            "list" => self.list(out),
            "copy" => {
                let direction = args.first().context("copy 需要接 to 或 from")?;
                let target = args.get(1).context("copy 需要接目標裝置的 id")?;
                self.copy(direction, target, out)
            }
            "status" => {
                out.push(&self.copy_status_text());
                Ok(())
            }
            other => bail!("music 不認得指令: {other}"),
        }
    }

    /// panel 顯示的內容跟 `list` 指令看到的一樣：已下載的檔案清單，加上目前
    /// 進行中/失敗的下載狀態——不是 copy 狀態，那個只透過 `status` 指令查看。
    fn panel_text(&self) -> Option<String> {
        Some(self.list_text())
    }

    fn manual_text(&self) -> &'static str {
        MANUAL_TEXT
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
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
/// 資料夾內容，跟 `music/` 資料夾的實際用法一致）。
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

/// 目的地已經有同名檔案、且大小一樣，就當作「已經傳過」跳過不重傳——用檔案
/// 大小當快速判斷依據，不逐位元組算 checksum（那需要整個讀一次檔案內容，對
/// 「重跑同一個 copy，大部分檔案其實都沒變」這種常見情境來說成本不成比例；
/// 大小不同一定代表內容不同，必須重傳，大小剛好相同但內容不同這種理論上可能
/// 但實務上少見的情況，接受這個取捨）。
fn local_file_matches(path: &Path, expected_size: u64) -> bool {
    fs::metadata(path).map(|m| m.len() == expected_size).unwrap_or(false)
}

fn remote_file_matches(remote_files: &[FileMeta], name: &str, expected_size: u64) -> bool {
    remote_files.iter().any(|f| f.name == name && f.size == expected_size)
}

fn run_push(ctx: &SharedContext, target: &CopyTarget, status: &Arc<Mutex<Option<TransferStatus>>>) -> Result<()> {
    let dir = Path::new(MUSIC_DIR);
    let names = list_local_files(dir)?;
    set_total(status, names.len());
    // 先查一次目的地既有的檔案清單，才能跳過已經傳過、大小沒變的檔案；跟
    // `run_pull` 開頭就先拿到完整清單是同一個理由，唯一差別是這裡查的是「對方
    // 已經有什麼」而不是「對方有什麼要拉」。
    let remote_files: Vec<FileMeta> = match target {
        CopyTarget::Http { ip } => {
            ctx.lock().unwrap().log_activity("http-out", format!("GET http://{ip}:{PORT}/api/music/copy"));
            list_remote_files_http(ip)?
        }
        CopyTarget::CrossDomain { domain, target_id } => list_remote_files_mqtt(ctx, domain, target_id)?,
    };
    for name in names {
        let path = dir.join(&name);
        let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        set_current_file(status, &name, size);
        if remote_file_matches(&remote_files, &name, size) {
            finish_current_file(status);
            bump_completed(status);
            continue;
        }
        match target {
            CopyTarget::Http { ip } => {
                ctx.lock()
                    .unwrap()
                    .log_activity("http-out", format!("POST http://{ip}:{PORT}/api/music/copy/{name}"));
                push_file_http(ip, &name, &path)?;
                finish_current_file(status);
            }
            CopyTarget::CrossDomain { domain, target_id } => {
                push_file_mqtt(ctx, domain, target_id, &name, &path, status)?
            }
        }
        bump_completed(status);
    }
    Ok(())
}

fn run_pull(ctx: &SharedContext, target: &CopyTarget, status: &Arc<Mutex<Option<TransferStatus>>>) -> Result<()> {
    fs::create_dir_all(MUSIC_DIR).context("建立資料夾失敗: music")?;
    let files: Vec<FileMeta> = match target {
        CopyTarget::Http { ip } => {
            ctx.lock().unwrap().log_activity("http-out", format!("GET http://{ip}:{PORT}/api/music/copy"));
            list_remote_files_http(ip)?
        }
        CopyTarget::CrossDomain { domain, target_id } => list_remote_files_mqtt(ctx, domain, target_id)?,
    };
    set_total(status, files.len());
    for meta in files {
        set_current_file(status, &meta.name, meta.size);
        let dest = Path::new(MUSIC_DIR).join(&meta.name);
        if local_file_matches(&dest, meta.size) {
            finish_current_file(status);
            bump_completed(status);
            continue;
        }
        match target {
            CopyTarget::Http { ip } => {
                ctx.lock().unwrap().log_activity(
                    "http-out",
                    format!("GET http://{ip}:{PORT}/api/music/copy/{}", meta.name),
                );
                pull_file_http(ip, &meta.name, &dest)?;
                finish_current_file(status);
            }
            CopyTarget::CrossDomain { domain, target_id } => {
                pull_file_mqtt(ctx, domain, target_id, &meta, &dest, status)?
            }
        }
        bump_completed(status);
    }
    Ok(())
}

// --- 同網域：整檔透過既有的 /api/music/copy 端點傳輸，不用切 chunk ---

fn push_file_http(ip: &str, name: &str, path: &Path) -> Result<()> {
    let url = format!("http://{ip}:{PORT}/api/music/copy/{}", url_encode_filename(name));
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

fn list_remote_files_http(ip: &str) -> Result<Vec<FileMeta>> {
    let url = format!("http://{ip}:{PORT}/api/music/copy");
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

fn pull_file_http(ip: &str, name: &str, dest: &Path) -> Result<()> {
    let url = format!("http://{ip}:{PORT}/api/music/copy/{}", url_encode_filename(name));
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
    name: &str,
    path: &Path,
    status: &Arc<Mutex<Option<TransferStatus>>>,
) -> Result<()> {
    let data = fs::read(path).with_context(|| format!("讀取檔案失敗: {}", path.display()))?;
    let mut offset: usize = 0;
    loop {
        let end = (offset + FILE_CHUNK_SIZE).min(data.len());
        let chunk = &data[offset..end];
        let ask = CrossDomainAsk::MusicFilePush {
            target_id: target_id.to_string(),
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

/// 資料夾檔案數量多時，`MusicFileList` 的完整清單一則 MQTT 回覆塞不下（見
/// `FILE_LIST_PAGE_BUDGET`），對面會分頁回，這裡跟 `pull_file_mqtt` 拉檔案
/// 內容一樣，逐頁把 `offset` 往前推，直到湊滿回覆帶的 `total` 筆。多留一個
/// 「這頁是空的就先停」的保險，避免 `total` 剛好跟實際筆數對不上（例如清單
/// 中途被改動）時無窮迴圈下去。
fn list_remote_files_mqtt(ctx: &SharedContext, domain: &str, target_id: &str) -> Result<Vec<FileMeta>> {
    let mut files = Vec::new();
    loop {
        let ask = CrossDomainAsk::MusicFileList { target_id: target_id.to_string(), offset: files.len() };
        match send_cross_domain_request(ctx, domain, ask)? {
            RemoteReply::MusicFileList { files: page, total, .. } => {
                if page.is_empty() {
                    break;
                }
                files.extend(page);
                if files.len() >= total {
                    break;
                }
            }
            RemoteReply::Error { message, .. } => bail!(message),
            _ => bail!("收到不符預期的回覆型別"),
        }
    }
    Ok(files)
}

/// 逐個 chunk 拉一個檔案，用 `MusicFileList` 已經拿到的 `meta.size` 判斷有沒有
/// 拉完——不靠伺服器另外回報「這是不是最後一塊」，這樣檔案大小剛好是
/// `FILE_CHUNK_SIZE` 整數倍時也不會多一輪去問一個超出檔案範圍的 offset。
fn pull_file_mqtt(
    ctx: &SharedContext,
    domain: &str,
    target_id: &str,
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
        let ask = CrossDomainAsk::MusicFilePull { target_id: target_id.to_string(), name: meta.name.clone(), offset };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 單純檔名應該正常組出路徑。
    #[test]
    fn plain_name_ok() {
        assert_eq!(safe_music_copy_path("song.mp3"), Some(Path::new(MUSIC_DIR).join("song.mp3")));
    }

    /// 檔名裡含路徑分隔符、`.`、`..`，或整個是空字串，都必須拒絕——這是防止
    /// 收到的請求即使通過了 AEAD 解密驗證，仍藉由檔名跳脫出 `music/` 資料夾之外
    /// 的最後一道防線（見 `safe_music_copy_path` 上方的說明）。
    #[test]
    fn traversal_attempts_rejected() {
        assert_eq!(safe_music_copy_path(""), None);
        assert_eq!(safe_music_copy_path("."), None);
        assert_eq!(safe_music_copy_path(".."), None);
        assert_eq!(safe_music_copy_path("../secrets.txt"), None);
        assert_eq!(safe_music_copy_path("sub/song.mp3"), None);
        assert_eq!(safe_music_copy_path("sub\\song.mp3"), None);
    }
}
