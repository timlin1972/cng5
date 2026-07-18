use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{bail, Context, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

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
pub(crate) const SUBTITLE_LANG_PRIORITY: [&str; 6] = ["zh-TW", "zh-Hant", "zh-Hans", "zh", "ja", "en"];

#[derive(Clone)]
enum DownloadStatus {
    InProgress,
    Done,
    Failed(String),
}

pub struct MusicPlugin {
    #[allow(dead_code)]
    ctx: SharedContext,
    /// 每個下載目標（YouTube 網址）目前的狀態，背景執行緒（`download` 開的）
    /// 抓完寫回這裡；`download`/`list`/`panel_text` 都只讀，不含任何耗時操作，
    /// 不會卡住持有 `Shell` 鎖的那個執行緒——跟 `WeatherPlugin` 抓天氣資料
    /// 同樣的考量，只是這裡「耗時操作」換成下載影片轉檔，時間更長更不能等。
    downloads: Arc<Mutex<HashMap<String, DownloadStatus>>>,
}

impl MusicPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx, downloads: Arc::new(Mutex::new(HashMap::new())) }
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
        thread::spawn(move || {
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
}

impl Plugin for MusicPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["download <youtube_url>", "list"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "download" => self.download(args.first().context("download 需要一個 YouTube 網址")?, out),
            "list" => self.list(out),
            other => bail!("music 不認得指令: {other}"),
        }
    }

    /// panel 顯示的內容跟 `list` 指令看到的一樣：已下載的檔案清單，加上目前
    /// 進行中/失敗的下載狀態。
    fn panel_text(&self) -> Option<String> {
        Some(self.list_text())
    }
}
