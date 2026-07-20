use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use actix_ws::Message;
use async_stream::stream;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::output::OutputBuffer;
use crate::plugin::{DeviceEntry, DeviceListItem, DeviceReport, SharedContext};
use crate::plugins::{DEFAULT_NOTEPAD_FILE, MUSIC_DIR, NOTEPAD_DIR, SUBTITLE_LANG_PRIORITY};
use crate::shell::{default_shell_program, lock_shell, Shell};

type SharedShell = Arc<Mutex<Shell>>;

pub(crate) const PORT: u16 = 9759;
/// panel 內容多久重新算一次、有變才推播，見 `broadcast_ticker`。
const TICK: Duration = Duration::from_millis(300);
/// `output` 這個假 panel 只推最新這麼多行，避免瀏覽器端的內容無限長下去。
const OUTPUT_TAIL_LINES: usize = 500;

const FRONTEND_HTML: &str = include_str!("web/frontend.html");

/// 每個 plugin 名稱（含 `output`）各自一條 broadcast channel 供 SSE 訂閱，加上
/// 一份「目前最新內容」的快取——新連進來的分頁不用等下一次內容真的改變，
/// 一開始就先把目前的內容送一次，不然畫面會一直卡在「等待資料...」直到剛好
/// 有變化為止。
struct PanelHub {
    channels: HashMap<String, broadcast::Sender<String>>,
    cache: Mutex<HashMap<String, String>>,
}
type Hub = Arc<PanelHub>;

/// 在背景執行緒起一個獨立的 actix-web server，跟目前終端機是 CLI 還是 GUI mode
/// 無關，整個程式活著期間都在監聽，讓瀏覽器跟終端機共用同一個
/// `Shell`/`OutputBuffer`——這樣終端機下 `mode server` 之後，瀏覽器開著的
/// `system` panel 才能即時看到內容改變。bind 失敗（例如 port 被佔用）只把錯誤
/// 寫進 `output`，不能讓整個程式跟著沒了 CLI/GUI。
pub fn spawn(shell: Arc<Mutex<Shell>>, output: Arc<OutputBuffer>, ctx: SharedContext) {
    std::thread::spawn(move || {
        let out_for_err = output.clone();
        if let Err(err) = actix_web::rt::System::new().block_on(run_server(shell, output, ctx)) {
            out_for_err.push(&format!("web server 啟動失敗: {err:#}\n"));
        }
    });
}

async fn run_server(shell: Arc<Mutex<Shell>>, output: Arc<OutputBuffer>, ctx: SharedContext) -> std::io::Result<()> {
    let names = lock_shell(&shell).plugin_names();
    let channels = names
        .iter()
        .map(|name| (name.clone(), broadcast::channel(16).0))
        .collect();
    let hub: Hub = Arc::new(PanelHub { channels, cache: Mutex::new(HashMap::new()) });

    tokio::spawn(broadcast_ticker(shell.clone(), output.clone(), hub.clone(), names));

    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(hub.clone()))
            .app_data(web::Data::new(shell.clone()))
            .app_data(web::Data::new(output.clone()))
            .app_data(web::Data::new(ctx.clone()))
            .route("/", web::get().to(index))
            .route("/api/plugins", web::get().to(api_plugins))
            .route("/api/version", web::get().to(api_version))
            .route("/api/panel/{name}/stream", web::get().to(panel_stream))
            .route("/api/prompt", web::get().to(prompt))
            .route("/api/exec", web::post().to(exec))
            .route("/api/shell/ws", web::get().to(shell_ws))
            .route("/api/music/files", web::get().to(music_files))
            .route("/api/music/file/{name}/audio", web::get().to(music_file_audio))
            .route("/api/music/file/{name}/cover", web::get().to(music_file_cover))
            .route("/api/music/file/{name}/lyrics", web::get().to(music_file_lyrics))
            .route("/api/music/file/{name}", web::delete().to(music_file_delete))
            .route("/api/notepad/content", web::get().to(notepad_get_content))
            .route("/api/notepad/content", web::post().to(notepad_save_content))
            .route("/api/device/register", web::post().to(device_register))
            .route("/api/device/list", web::get().to(device_list))
    })
    .bind(("0.0.0.0", PORT))?
    .run()
    .await
}

/// `POST /api/device/register`：client 端的 `SystemPlugin` 背景回報執行緒
/// （見 `plugins/system.rs` 的 `push_report`）定期打這個，把自己的資訊寫進/
/// 更新這台 server 本機的 device registry，`last_seen` 用收到這次請求當下的
/// 時間，`DevicePlugin` 顯示的 alive 就是靠這個判斷。
async fn device_register(body: web::Json<DeviceReport>, ctx: web::Data<SharedContext>) -> impl Responder {
    let report = body.into_inner();
    ctx.lock().unwrap().devices.insert(report.id.clone(), DeviceEntry { report, last_seen: Instant::now() });
    HttpResponse::Ok().finish()
}

/// `GET /api/device/list`：回傳這台 server 本機 registry 裡目前所有裝置，給
/// client 端的 `SystemPlugin`（見 `pull_peers`）拉回去合併進自己的清單。
async fn device_list(ctx: web::Data<SharedContext>) -> impl Responder {
    let items: Vec<DeviceListItem> = ctx
        .lock()
        .unwrap()
        .devices
        .values()
        .map(|entry| DeviceListItem { report: entry.report.clone(), age_secs: entry.last_seen.elapsed().as_secs_f64() })
        .collect();
    HttpResponse::Ok().json(items)
}

/// 每 `TICK` 算一次每個 panel 目前該顯示的文字，跟快取裡上一次的比對，只有變了
/// 才更新快取並透過對應的 channel 推播出去。集中在這一個 task 裡算（而不是每條
/// SSE 連線各自算一次），是因為 `system` 的 `panel_text()` 會真的去執行一次
/// `tailscale` 子行程——不集中算的話，開越多瀏覽器分頁看同一個 panel，就會多
/// 重複跑越多次。
async fn broadcast_ticker(shell: Arc<Mutex<Shell>>, output: Arc<OutputBuffer>, hub: Hub, names: Vec<String>) {
    let mut interval = tokio::time::interval(TICK);
    loop {
        interval.tick().await;
        let shell = shell.clone();
        let output = output.clone();
        let names = names.clone();
        // `panel_text()` 可能做阻塞的事（system plugin 就是），丟到 blocking
        // thread pool 執行，不要卡住這個 server 唯一的 ticker task。
        let texts = tokio::task::spawn_blocking(move || {
            names
                .into_iter()
                .map(|name| {
                    let text = panel_text_for(&shell, &output, &name);
                    (name, text)
                })
                .collect::<Vec<_>>()
        })
        .await;
        let Ok(texts) = texts else { continue };
        let mut cache = hub.cache.lock().unwrap();
        for (name, text) in texts {
            if cache.get(&name) == Some(&text) {
                continue;
            }
            if let Some(tx) = hub.channels.get(&name) {
                let _ = tx.send(text.clone());
            }
            cache.insert(name, text);
        }
    }
}

/// `output` 是特例（即時捲動紀錄，直接讀 `OutputBuffer`），其餘 plugin 走
/// `Shell::plugin_panel_text`——跟 `gui.rs` 畫 panel 時用的是同一個判斷規則。
fn panel_text_for(shell: &Mutex<Shell>, output: &OutputBuffer, name: &str) -> String {
    if name == "output" {
        let lines = output.all();
        let start = lines.len().saturating_sub(OUTPUT_TAIL_LINES);
        lines[start..].join("\n")
    } else {
        lock_shell(shell).plugin_panel_text(name).unwrap_or_default()
    }
}

async fn index() -> impl Responder {
    HttpResponse::Ok().content_type("text/html; charset=utf-8").body(FRONTEND_HTML)
}

async fn api_plugins(hub: web::Data<Hub>) -> impl Responder {
    let mut names: Vec<&String> = hub.channels.keys().collect();
    names.sort();
    HttpResponse::Ok().json(names)
}

#[derive(Serialize)]
struct VersionResponse {
    build: &'static str,
}

/// `GET /api/version`：這個 build 的編譯日期/時間（`build.rs` 塞進去的
/// `CNG5_BUILD_TIMESTAMP`，跟 `system` plugin 的 `version` 指令/panel 是同一份），
/// 給前端畫面最左上角顯示用。
async fn api_version() -> impl Responder {
    HttpResponse::Ok().json(VersionResponse { build: env!("CNG5_BUILD_TIMESTAMP") })
}

#[derive(Serialize)]
struct PromptResponse {
    prompt: String,
}

/// `GET /api/prompt`：命令列前綴（例如 `cng5(system)> `），跟終端機的
/// `Shell::prompt()` 是同一份，給輸入框一開始要顯示什麼用。
async fn prompt(shell: web::Data<SharedShell>) -> impl Responder {
    let prompt = lock_shell(&shell).prompt();
    HttpResponse::Ok().json(PromptResponse { prompt })
}

#[derive(Deserialize)]
struct ExecRequest {
    line: String,
}

#[derive(Serialize)]
struct ExecResponse {
    prompt: String,
    error: Option<String>,
}

/// `POST /api/exec`：從 web 的輸入框執行一行指令，跟 `gui.rs` 按下 Enter 時的
/// 邏輯完全一樣——先把 prompt+這一行 echo 進 `OutputBuffer`（讓開著 `output`
/// panel 的分頁看得到），執行，錯誤也 push 進去，最後再 push 一次新的 prompt
/// 當分隔行。因為跟終端機共用同一個 `Shell`，這裡打的指令（包括 `mode`/
/// `plugin enter` 之類會改變狀態的）也會直接影響終端機接下來看到的畫面。
async fn exec(
    body: web::Json<ExecRequest>,
    shell: web::Data<SharedShell>,
    output: web::Data<Arc<OutputBuffer>>,
) -> impl Responder {
    let line = body.line.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut sh = lock_shell(&shell);
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') && !trimmed.starts_with('!') {
            output.push(&format!("{}{}\n", sh.prompt(), trimmed));
        }
        let error = sh.execute_line(&line).err().map(|err| format!("{err:#}"));
        if let Some(msg) = &error {
            output.push(&format!("錯誤: {msg}\n"));
        }
        let prompt = sh.prompt();
        output.push(&format!("{prompt}\n"));
        ExecResponse { prompt, error }
    })
    .await
    .unwrap_or_else(|_| ExecResponse { prompt: String::new(), error: Some("內部錯誤".to_string()) });
    HttpResponse::Ok().json(result)
}

/// 前端 xterm.js 用 JSON 文字訊息傳「resize」這種控制訊息（跟一般鍵盤輸入的
/// binary frame 分開，見 `shell_ws` 的說明），格式是 `{"resize":{"cols":.., "rows":..}}`。
#[derive(Deserialize)]
struct ShellControlMessage {
    resize: Option<ShellResize>,
}

#[derive(Deserialize)]
struct ShellResize {
    cols: u16,
    rows: u16,
}

/// `GET /api/shell/ws`：跟 `Shell`/`Mode` 完全無關、每個連線各自獨立的一個
/// 真正的 host shell（PTY），概念上跟 `music/` 檔案管理那些端點一樣「純粹是
/// 系統操作」，不透過 `lock_shell`——這個功能本來就是要讓使用者拿到一個完全
/// 獨立、不受目前終端機/其他瀏覽器分頁模式影響的 shell，共用 `Shell` 反而沒
/// 意義（而且互動 shell 可能開很久，共用鎖會卡住其他人，跟 CLI/GUI 版本
/// `shell` 指令刻意不做成 `Plugin::dispatch` 是同一個考量，見 `shell.rs` 的
/// `run_host_shell`）。
///
/// 每條 WebSocket 連線各自開一個 PTY + host shell 子行程（見 `default_shell_program`）：
/// - 讀取端：獨立的 OS 執行緒阻塞讀 PTY 的輸出，讀到的位元組透過 channel
///   轉給下面這個 async task，讀到 EOF（子行程離開、pty 關閉）就順便呼叫
///   `child.wait()` 把這個子行程 reap 掉，不留殭屍行程。
/// - 寫入端：也是獨立的 OS 執行緒（`writer` 是同步的 `Write`），async task
///   收到瀏覽器送來的鍵盤輸入（binary frame）就轉丟給這個執行緒寫進 pty。
/// - resize：瀏覽器端的 `addon-fit` 算出新的欄/列數後送一個 JSON text
///   frame，直接用 `master.resize(...)`（快速的 ioctl，不需要額外開執行緒）。
/// - 不管是 PTY 輸出先結束（子行程自己 exit，例如使用者在 shell 裡打
///   `exit`）還是瀏覽器那邊先斷線，都會呼叫 `killer.kill()` 確保子行程
///   一定會被清掉，不會變成孤兒行程。
async fn shell_ws(
    req: HttpRequest,
    body: web::Payload,
    output: web::Data<Arc<OutputBuffer>>,
) -> actix_web::Result<HttpResponse> {
    let (response, mut session, mut msg_stream) = actix_ws::handle(&req, body)?;

    let setup = (|| -> anyhow::Result<_> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })?;
        let child = pair.slave.spawn_command(CommandBuilder::new(default_shell_program()))?;
        // 一定要早點放掉這份 slave 端 handle：只要我們自己的行程還握著一份
        // slave fd，就算子行程已經結束，master 那邊的讀取也不會收到 EOF
        // （見 portable-pty 官方範例都是這樣做的）。
        drop(pair.slave);
        let killer = child.clone_killer();
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        Ok((pair.master, child, killer, reader, writer))
    })();

    let (master, child, killer, reader, writer) = match setup {
        Ok(v) => v,
        Err(err) => {
            output.push(&format!("web shell 開啟失敗: {err:#}\n"));
            let _ = session.close(None).await;
            return Ok(response);
        }
    };

    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (write_tx, write_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    // 寫入執行緒：`writer` 是同步的 `Write`，用一個獨立執行緒把 async task
    // 收到的鍵盤輸入依序寫進去，不會卡住 tokio 的 executor。
    thread::spawn(move || {
        let mut writer = writer;
        while let Ok(bytes) = write_rx.recv() {
            if writer.write_all(&bytes).is_err() {
                break;
            }
        }
    });

    // 讀取＋reap 執行緒：阻塞讀 PTY 輸出轉給 async task；讀到 EOF（不管是
    // 子行程自然結束、還是被下面 async task 的 `killer.kill()` 殺掉）就呼叫
    // `child.wait()` 把它 reap 掉。
    thread::spawn(move || {
        let mut reader = reader;
        let mut child = child;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = child.wait();
    });

    actix_web::rt::spawn(async move {
        let master = master;
        let mut killer = killer;
        loop {
            tokio::select! {
                chunk = out_rx.recv() => {
                    match chunk {
                        Some(bytes) => {
                            if session.binary(bytes).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                msg = msg_stream.recv() => {
                    match msg {
                        Some(Ok(Message::Binary(bytes))) => {
                            let _ = write_tx.send(bytes.to_vec());
                        }
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(ctrl) = serde_json::from_str::<ShellControlMessage>(&text)
                                && let Some(resize) = ctrl.resize
                            {
                                let _ = master.resize(PtySize {
                                    rows: resize.rows,
                                    cols: resize.cols,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                });
                            }
                        }
                        Some(Ok(Message::Ping(bytes))) => {
                            let _ = session.pong(&bytes).await;
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                        _ => {}
                    }
                }
            }
        }
        // 不管是哪一邊先結束（PTY 輸出斷了 or 瀏覽器斷線），都確保子行程
        // 一定會被清掉，不留孤兒行程。
        let _ = killer.kill();
        let _ = session.close(None).await;
    });

    Ok(response)
}

/// SSE 的一個 `data:` frame：內容用 JSON 編碼成單行字串，因為 panel 內容本身
/// 可能含換行，SSE 的 `data:` 一行不能包含字面上的換行。
fn sse_frame(text: &str) -> web::Bytes {
    let payload = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    web::Bytes::from(format!("data: {payload}\n\n"))
}

/// `GET /api/panel/{name}/stream`：先送一次目前快取的內容（讓剛打開的分頁立刻
/// 看到東西），之後每次 `broadcast_ticker` 偵測到內容改變就會再收到一次。
/// `name` 不是已知 plugin 就回 404。
async fn panel_stream(path: web::Path<String>, hub: web::Data<Hub>) -> impl Responder {
    let name = path.into_inner();
    let Some(tx) = hub.channels.get(&name).cloned() else {
        return HttpResponse::NotFound().finish();
    };
    let mut rx = tx.subscribe();
    let initial = hub.cache.lock().unwrap().get(&name).cloned();

    let body = stream! {
        if let Some(text) = initial {
            yield Ok::<_, actix_web::Error>(sse_frame(&text));
        }
        loop {
            match rx.recv().await {
                Ok(text) => yield Ok(sse_frame(&text)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    HttpResponse::Ok().content_type("text/event-stream").streaming(body)
}

/// web 這邊播放/管理 `music/` 資料夾裡的檔案是獨立於 `MusicPlugin`/`Shell` 之外
/// 的功能——純粹是檔案系統操作（列出/串流/刪除），不需要跟 `download` 指令共用
/// 的下載狀態協調，所以不透過 `lock_shell`，直接讀寫磁碟就好，也不會因此卡到
/// 持有 `Shell` 鎖的其他操作。
///
/// `name` 只接受單一檔名（不能含路徑分隔符或是 `.`/`..`），避免有人把檔名做成
/// path traversal 跑到 `music/` 資料夾以外的地方去讀/刪別的檔案。回傳 `None`
/// 代表這個名字不安全，呼叫端應該回 400。
fn safe_music_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return None;
    }
    Some(Path::new(MUSIC_DIR).join(name))
}

/// `GET /api/music/files`：`music/` 資料夾裡目前有的檔案名稱（依字母排序），
/// 資料夾還不存在就當作空清單，不報錯——跟 `MusicPlugin::list_text()` 判斷
/// 資料夾不存在時的容錯邏輯一致。只列 `.mp3`，`download` 順便存的 `.srt`
/// 歌詞字幕檔是附屬品，不該被當成清單裡可以播放/刪除的一個項目（見
/// `MusicPlugin::list_text()` 的同一個理由）。
async fn music_files() -> impl Responder {
    let names: Vec<String> = std::fs::read_dir(MUSIC_DIR)
        .map(|entries| {
            let mut names: Vec<String> = entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.to_ascii_lowercase().ends_with(".mp3"))
                .collect();
            names.sort();
            names
        })
        .unwrap_or_default();
    HttpResponse::Ok().json(names)
}

/// `GET /api/music/file/{name}/audio`：把檔案內容當音訊串流回去，用
/// `actix_files::NamedFile` 是因為它會自動處理 `Range`/條件式請求（`<audio>`
/// 標籤拖拉播放進度需要靠 Range 請求做區段讀取），自己手刻這段容易漏掉細節。
async fn music_file_audio(path: web::Path<String>, req: HttpRequest) -> HttpResponse {
    let name = path.into_inner();
    let Some(file_path) = safe_music_path(&name) else {
        return HttpResponse::BadRequest().finish();
    };
    match actix_files::NamedFile::open(&file_path) {
        Ok(file) => file.into_response(&req),
        Err(_) => HttpResponse::NotFound().finish(),
    }
}

/// `DELETE /api/music/file/{name}`：從 `music/` 資料夾刪掉這個檔案。
async fn music_file_delete(path: web::Path<String>) -> impl Responder {
    let name = path.into_inner();
    let Some(file_path) = safe_music_path(&name) else {
        return HttpResponse::BadRequest().finish();
    };
    match std::fs::remove_file(&file_path) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(_) => HttpResponse::NotFound().finish(),
    }
}

/// `GET /api/music/file/{name}/cover`：讀 mp3 的 ID3 標籤，把 `download` 指令
/// 用 `yt-dlp --embed-thumbnail` 嵌進去的封面圖原始位元組讀出來直接回傳，給
/// web 播放器介面顯示用。檔案不存在、沒有 ID3 標籤、或標籤裡沒有封面圖都算
/// 沒有，回 404（沒有封面圖是正常情況，不是錯誤，例如舊格式音檔或手動放進去
/// 的檔案）。
async fn music_file_cover(path: web::Path<String>) -> HttpResponse {
    let name = path.into_inner();
    let Some(file_path) = safe_music_path(&name) else {
        return HttpResponse::BadRequest().finish();
    };
    let Ok(tag) = id3::Tag::read_from_path(&file_path) else {
        return HttpResponse::NotFound().finish();
    };
    match tag.pictures().next() {
        Some(picture) => HttpResponse::Ok().content_type(picture.mime_type.clone()).body(picture.data.clone()),
        None => HttpResponse::NotFound().finish(),
    }
}

#[derive(Serialize)]
struct LyricLine {
    /// 這一句開始的秒數（可以有小數），前端拿播放進度（`currentTime`）跟這個
    /// 比對，決定目前要把哪一句反白。
    start: f64,
    text: String,
}

/// `download` 用 `yt-dlp --write-subs` 抓字幕時，檔名慣例是
/// `{標題}.{語言代碼}.srt`（例如 `Song.zh-TW.srt`）。同一部影片如果同時有好
/// 幾種語言的字幕，`yt-dlp` 會全部抓下來，不是只抓一個——如果這裡對資料夾做
/// 無排序的掃描、抓到第一個「檔名開頭對得上」的 `.srt` 就用，配到的很可能不是
/// 想要的語言（例如明明中文歌，卻因為資料夾掃描順序配到英文字幕）。所以改成
/// 依照 `SUBTITLE_LANG_PRIORITY` 的順序一個一個組出確切檔名去檢查存在，跟
/// `MusicPlugin::download` 抓字幕用的是同一份優先順序，保證兩邊一致。
/// 都沒有就是 `None`（沒有字幕，很正常，不是每支影片都有）。
fn find_lyrics_path(mp3_name: &str) -> Option<PathBuf> {
    let stem = Path::new(mp3_name).file_stem()?.to_str()?;
    SUBTITLE_LANG_PRIORITY
        .iter()
        .map(|lang| Path::new(MUSIC_DIR).join(format!("{stem}.{lang}.srt")))
        .find(|candidate| candidate.is_file())
}

/// 簡單的 `.srt` 解析：每個字幕塊是「編號」「時間範圍」「一行以上的文字」，
/// 塊與塊之間空一行。這裡只取每塊的開始時間跟文字內容（合成一行），不需要
/// 結束時間——「目前唱到哪一句」只需要知道下一句開始前都算這一句還在唱。
fn parse_srt(content: &str) -> Vec<LyricLine> {
    let normalized = content.replace("\r\n", "\n");
    normalized
        .split("\n\n")
        .filter_map(|block| {
            let mut lines = block.trim().lines();
            let first = lines.next()?;
            // 第一行通常是編號；但保守一點，如果它本身就長得像時間範圍
            // （某些工具轉出來的 `.srt` 省略編號），就直接當時間範圍用。
            let time_line = if first.contains("-->") { first } else { lines.next()? };
            let start = parse_srt_start(time_line)?;
            let text: String = lines.collect::<Vec<_>>().join(" ").trim().to_string();
            if text.is_empty() {
                return None;
            }
            Some(LyricLine { start, text })
        })
        .collect()
}

/// `"00:00:11,960 --> 00:00:15,820"` 這種時間範圍字串，取開始時間換算成秒數。
fn parse_srt_start(time_line: &str) -> Option<f64> {
    let start_str = time_line.split("-->").next()?.trim();
    let (hms, millis_str) = start_str.split_once(',')?;
    let millis: f64 = millis_str.trim().parse().ok()?;
    let mut parts = hms.split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    Some(hours * 3600.0 + minutes * 60.0 + seconds + millis / 1000.0)
}

/// `GET /api/music/file/{name}/lyrics`：找這首歌旁邊 `download` 順便存的
/// `.srt` 歌詞字幕檔（見 `find_lyrics_path`），解析成「開始時間＋文字」的陣列
/// 回傳給前端做同步顯示。沒有字幕檔、或解析不出東西都回 404——沒有歌詞是
/// 正常情況，不是每支影片都有字幕可以當歌詞用。
async fn music_file_lyrics(path: web::Path<String>) -> HttpResponse {
    let name = path.into_inner();
    if safe_music_path(&name).is_none() {
        return HttpResponse::BadRequest().finish();
    }
    let Some(lyrics_path) = find_lyrics_path(&name) else {
        return HttpResponse::NotFound().finish();
    };
    let Ok(content) = std::fs::read_to_string(&lyrics_path) else {
        return HttpResponse::NotFound().finish();
    };
    let lines = parse_srt(&content);
    if lines.is_empty() {
        return HttpResponse::NotFound().finish();
    }
    HttpResponse::Ok().json(lines)
}

/// `name` 只接受單一檔名（不能含路徑分隔符或是 `.`/`..`），避免有人把檔名
/// 做成 path traversal 跑到 `notepad/` 資料夾以外的地方去讀/寫別的檔案——
/// 這裡的檔名是透過瀏覽器打進來的網路輸入（Ctrl-F 切換檔案），跟 CLI/GUI
/// 由本機操作者直接輸入的信任層級不一樣，比照 `safe_music_path` 的防護。
/// 回傳 `None` 代表這個名字不安全，呼叫端應該回 400。
fn safe_notepad_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return None;
    }
    Some(Path::new(NOTEPAD_DIR).join(name))
}

#[derive(Serialize)]
struct NotepadContentResponse {
    name: String,
    content: String,
}

#[derive(Deserialize)]
struct NotepadQuery {
    name: Option<String>,
}

/// web 這邊的 notepad 編輯功能純粹是檔案讀寫、不透過 `Shell`/`NotepadPlugin`
/// ——理由跟 `music/` 檔案管理一樣（見上面的說明）：這是獨立於終端機當下
/// 編輯狀態之外的操作，兩邊各自對同一個檔案讀寫，最後存檔的人為準，不需要
/// （也沒有必要）讓瀏覽器分頁跟終端機的編輯 session 即時同步每一個按鍵。
/// `?name=` 沒帶就是 `DEFAULT_NOTEPAD_FILE`，對應 Ctrl-F 切換檔案的功能。
async fn notepad_get_content(query: web::Query<NotepadQuery>) -> HttpResponse {
    let name = query.name.clone().unwrap_or_else(|| DEFAULT_NOTEPAD_FILE.to_string());
    let Some(path) = safe_notepad_path(&name) else {
        return HttpResponse::BadRequest().finish();
    };
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    HttpResponse::Ok().json(NotepadContentResponse { name, content })
}

#[derive(Deserialize)]
struct NotepadSaveRequest {
    name: Option<String>,
    content: String,
}

/// `POST /api/notepad/content`：把瀏覽器編輯完的內容存回 `name` 指定的檔案
/// （沒帶就是 `DEFAULT_NOTEPAD_FILE`）。
async fn notepad_save_content(body: web::Json<NotepadSaveRequest>) -> HttpResponse {
    let name = body.name.clone().unwrap_or_else(|| DEFAULT_NOTEPAD_FILE.to_string());
    let Some(path) = safe_notepad_path(&name) else {
        return HttpResponse::BadRequest().finish();
    };
    if std::fs::create_dir_all(NOTEPAD_DIR).is_err() {
        return HttpResponse::InternalServerError().finish();
    }
    match std::fs::write(&path, &body.content) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}
