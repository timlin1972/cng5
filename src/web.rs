use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use actix_web::dev::Service;
use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use actix_ws::Message;
use async_stream::stream;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::output::OutputBuffer;
use crate::plugin::{
    merged_global_view, CrossDomainAsk, DeviceEntry, DeviceListItem, DeviceReport, FileMeta, SharedContext,
    APP_VERSION,
};
use crate::plugins::{
    book_cover, book_meta, book_resource, chapter_is_vertical, current_wallpaper, haodoo_import,
    inject_pagination_style, list_books, list_dir, list_wallpapers, make_dir, normalize_vertical_css, remove,
    rename_path, rotate_enabled, safe_ebook_path, safe_music_copy_path, safe_storage_path, save_chapter_progress,
    select_wallpaper, set_rotate_enabled, walk_with_hashes, DevicePlugin, GlobalPlugin, TodoPlugin, WeatherPlugin,
    WorldClockPlugin, DEFAULT_NOTEPAD_FILE, MUSIC_DIR, NOTEPAD_DIR, STORAGE_DIR, SUBTITLE_LANG_PRIORITY,
};
use crate::shell::{default_shell_program, lock_shell, run_upgrade, send_cross_domain_request, Shell};
use crate::sysinfo;

type SharedShell = Arc<Mutex<Shell>>;

pub(crate) const PORT: u16 = 9759;
/// panel 內容多久重新算一次、有變才推播，見 `broadcast_ticker`。
const TICK: Duration = Duration::from_millis(300);
/// `output` 這個假 panel 只推最新這麼多行，避免瀏覽器端的內容無限長下去。
const OUTPUT_TAIL_LINES: usize = 500;
/// `web::Bytes` extractor 預設 payload 上限只有 256KB（`actix_web` 的
/// `PayloadConfig` 預設值），epub 電子書常常上百 MB（附圖多的書更大），
/// 所以上傳電子書這條路徑要單獨調大上限，不能跟其他 API 共用預設值。
const EREADER_UPLOAD_LIMIT: usize = 512 * 1024 * 1024;

const FRONTEND_HTML: &str = include_str!("web/frontend.html");
const TABLET_HTML: &str = include_str!("web/tablet.html");
const IPHONE_HTML: &str = include_str!("web/iphone.html");

/// PWA manifest：`display: standalone` 讓「加入主畫面」開起來是全螢幕、
/// 沒有 Safari 網址列的獨立 app 外觀；`start_url` 固定指到 `/iphone`（不是
/// `/`），不然從主畫面點開會跑去桌面版版面。深色 `background_color`/
/// `theme_color` 跟 `iphone.html` 的 `#111318` 主題一致，避免啟動畫面/
/// 狀態列出現一閃而過的白色背景。
const IPHONE_MANIFEST_JSON: &str = r##"{
  "name": "cng5",
  "short_name": "cng5",
  "start_url": "/iphone",
  "display": "standalone",
  "background_color": "#111318",
  "theme_color": "#111318",
  "icons": [
    { "src": "/iphone-icon-192.png", "sizes": "192x192", "type": "image/png" },
    { "src": "/iphone-icon-512.png", "sizes": "512x512", "type": "image/png" }
  ]
}
"##;

/// 三張 app icon（180/192/512px）：這台機器沒有 Pillow/ImageMagick 之類的
/// 圖像工具，用一支一次性的 Python 腳本手工組 PNG（IHDR + zlib 壓縮
/// IDAT + IEND），深色圓角方塊背景配一組跟 topology 節點同色系
/// （`#6f9dff`）的圓點，不是真正設計過的 logo，純粹讓「加入主畫面」有個
/// 一致的圖示可用，不用退回 iOS 預設的頁面截圖。180px 給
/// `apple-touch-icon`，192/512px 給 PWA manifest 的 `icons` 陣列。
const IPHONE_ICON_180: &[u8] = include_bytes!("web/assets/iphone-icon-180.png");
const IPHONE_ICON_192: &[u8] = include_bytes!("web/assets/iphone-icon-192.png");
const IPHONE_ICON_512: &[u8] = include_bytes!("web/assets/iphone-icon-512.png");

/// 每個 plugin 名稱（含 `output`）各自一條 broadcast channel 供 SSE 訂閱，加上
/// 一份「目前最新內容」的快取——新連進來的分頁不用等下一次內容真的改變，
/// 一開始就先把目前的內容送一次，不然畫面會一直卡在「等待資料...」直到剛好
/// 有變化為止。
struct PanelHub {
    channels: HashMap<String, broadcast::Sender<String>>,
    cache: Mutex<HashMap<String, String>>,
    /// `global`／`device` 專用的結構化 JSON snapshot 頻道／快取，跟上面
    /// `channels`/`cache`（純文字）分開——那組是既有的公用端點
    /// （`/api/panel/{name}/stream`），`remote_output`（跨機器鏡射）跟
    /// `global` plugin 的跨 domain `Panel` 查詢都在訂閱／讀取，硬改成 JSON
    /// payload 會讓它們顯示出未預期的原始 JSON 字串（曾經真的這樣改過，
    /// 這次修正就是拆成獨立的一組）。
    snapshot_channels: HashMap<String, broadcast::Sender<String>>,
    snapshot_cache: Mutex<HashMap<String, String>>,
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
    // `global`／`device` 另外開一組獨立頻道，專門推結構化 JSON snapshot 給
    // web 前端的逐格閃爍效果用，理由見 `PanelHub` 的欄位註解。
    let snapshot_names: Vec<String> = vec!["global".to_string(), "device".to_string()];
    let snapshot_channels = snapshot_names
        .iter()
        .map(|name| (name.clone(), broadcast::channel(16).0))
        .collect();
    let hub: Hub = Arc::new(PanelHub {
        channels,
        cache: Mutex::new(HashMap::new()),
        snapshot_channels,
        snapshot_cache: Mutex::new(HashMap::new()),
    });

    tokio::spawn(broadcast_ticker(shell.clone(), output.clone(), hub.clone(), names));
    tokio::spawn(snapshot_broadcast_ticker(shell.clone(), hub.clone(), snapshot_names));

    HttpServer::new(move || {
        let activity_ctx = ctx.clone();
        App::new()
            .wrap_fn(move |req, srv| {
                activity_ctx.lock().unwrap().log_activity("http-in", format!("{} {}", req.method(), req.path()));
                srv.call(req)
            })
            .app_data(web::Data::new(hub.clone()))
            .app_data(web::Data::new(shell.clone()))
            .app_data(web::Data::new(output.clone()))
            .app_data(web::Data::new(ctx.clone()))
            .route("/", web::get().to(index))
            .route("/tablet", web::get().to(tablet_index))
            .route("/iphone", web::get().to(iphone_index))
            .route("/iphone-manifest.json", web::get().to(iphone_manifest))
            .route("/iphone-icon-180.png", web::get().to(iphone_icon_180))
            .route("/iphone-icon-192.png", web::get().to(iphone_icon_192))
            .route("/iphone-icon-512.png", web::get().to(iphone_icon_512))
            .route("/api/plugins", web::get().to(api_plugins))
            .route("/api/version", web::get().to(api_version))
            .route("/api/panel/{name}/stream", web::get().to(panel_stream))
            .route("/api/panel/{name}/snapshot-stream", web::get().to(panel_snapshot_stream))
            .route("/api/prompt", web::get().to(prompt))
            .route("/api/exec", web::post().to(exec))
            .route("/api/shell/ws", web::get().to(shell_ws))
            .route("/api/music/files", web::get().to(music_files))
            .route("/api/music/file/{name}/audio", web::get().to(music_file_audio))
            .route("/api/music/file/{name}/cover", web::get().to(music_file_cover))
            .route("/api/music/file/{name}/lyrics", web::get().to(music_file_lyrics))
            .route("/api/music/file/{name}", web::delete().to(music_file_delete))
            .route("/api/music/file/{name}/favorite", web::post().to(music_file_favorite))
            .route("/api/ereader/books", web::get().to(ereader_books))
            .route("/api/ereader/book/{name}/meta", web::get().to(ereader_book_meta))
            .route("/api/ereader/book/{name}/cover", web::get().to(ereader_book_cover))
            .route("/api/ereader/book/{name}/progress", web::post().to(ereader_book_progress))
            .route("/api/ereader/book/{name}/resource/{path:.*}", web::get().to(ereader_book_resource))
            .service(
                web::resource("/api/ereader/upload/{name}")
                    .app_data(web::PayloadConfig::new(EREADER_UPLOAD_LIMIT))
                    .route(web::post().to(ereader_upload)),
            )
            .route("/api/ereader/haodoo-import", web::post().to(ereader_haodoo_import))
            .route("/api/wallpaper/list", web::get().to(wallpaper_list))
            .route("/api/wallpaper/current", web::get().to(wallpaper_current))
            .route("/api/wallpaper/select", web::post().to(wallpaper_select))
            .route("/api/wallpaper/rotate", web::post().to(wallpaper_rotate))
            .route("/api/notepad/content", web::get().to(notepad_get_content))
            .route("/api/notepad/content", web::post().to(notepad_save_content))
            .route("/api/device/register", web::post().to(device_register))
            .route("/api/device/list", web::get().to(device_list))
            .route("/api/global/list", web::get().to(global_list))
            .route("/api/weather/list", web::get().to(weather_list))
            .route("/api/worldclock/list", web::get().to(worldclock_list))
            .route("/api/todo/list", web::get().to(todo_list))
            .route("/api/todo/add", web::post().to(todo_add))
            .route("/api/todo/toggle", web::post().to(todo_toggle))
            .route("/api/todo/remove", web::post().to(todo_remove))
            .route("/api/remote/cross-relay", web::post().to(remote_cross_relay))
            .route("/api/music/copy", web::get().to(music_copy_list))
            .route("/api/music/copy/{name}", web::get().to(music_copy_download))
            .route("/api/music/copy/{name}", web::post().to(music_copy_upload))
            .route("/api/storage/list", web::get().to(storage_list))
            .route("/api/storage/download", web::get().to(storage_download))
            .route("/api/storage/upload", web::post().to(storage_upload))
            .route("/api/storage/mkdir", web::post().to(storage_mkdir))
            .route("/api/storage/delete", web::post().to(storage_delete))
            .route("/api/storage/rename", web::post().to(storage_rename))
            .route("/api/storage/sync-manifest", web::get().to(storage_sync_manifest))
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

/// `GET /api/global/list`：client 端的 `global` plugin 定期打這個，跟自己的
/// server 要一份「目前看得到的所有 domain 裝置清單」（本機這個 domain 的 +
/// MQTT 收到的其他 domain 的，見 `merged_global_view`）拉回去合併進自己的
/// registry，不需要 client 自己連 MQTT——只有 server 才會真的連上
/// `broker.emqx.io`。
async fn global_list(ctx: web::Data<SharedContext>) -> impl Responder {
    let items = merged_global_view(&ctx.lock().unwrap());
    HttpResponse::Ok().json(items)
}

/// `GET /api/weather/list`：tablet 前端的 weather 分頁用，回傳每個 `add` 過的
/// 城市目前的結構化天氣資料（分類/數字，不是拼好的顯示字串），見
/// `WeatherPlugin::snapshot` 的說明。跟 `table_snapshot_json` 同一個
/// 「向下轉型拿具體型別」的做法，這裡不用比對「有沒有變化才推播」（不像
/// `global`/`device` 那樣走 SSE snapshot channel），因為 weather 是每 300 秒才
/// 換一次資料（`CACHE_TTL`），前端定期 poll 就夠了，不需要另開一組 broadcast
/// channel。
async fn weather_list(shell: web::Data<SharedShell>) -> impl Responder {
    let items = lock_shell(&shell)
        .plugin_mut("weather")
        .and_then(|p| p.as_any_mut().downcast_mut::<WeatherPlugin>())
        .map(|w| w.snapshot())
        .unwrap_or_default();
    HttpResponse::Ok().json(items)
}

/// `GET /api/worldclock/list`：webui 的世界地圖用，回傳每個 `add` 過的城市
/// 目前的座標＋UTC 偏移（見 `WorldClockPlugin::snapshot` 的說明），跟
/// `weather_list` 同一個「向下轉型拿具體型別」的做法。前端自己用
/// `offset_secs` 每秒算一次當地時間讓畫面上的時鐘持續跳動，不需要靠這條
/// API 的 poll 頻率決定顯示更新的頻率。
async fn worldclock_list(shell: web::Data<SharedShell>) -> impl Responder {
    let items = lock_shell(&shell)
        .plugin_mut("worldclock")
        .and_then(|p| p.as_any_mut().downcast_mut::<WorldClockPlugin>())
        .map(|w| w.snapshot())
        .unwrap_or_default();
    HttpResponse::Ok().json(items)
}

/// `GET /api/todo/list`：tablet/webui 的 todo 分頁用，回傳目前所有待辦事項
/// （結構化 JSON，不是 `panel_text()` 那份純文字），跟 `weather_list` 一樣
/// 「向下轉型拿具體型別」直接讀 `TodoPlugin` 的記憶體狀態，不用另外走 SSE
/// snapshot channel——待辦清單只會因為使用者自己在這個分頁操作而改變，前端
/// 自己在新增/打勾/刪除後主動重新 `GET` 一次就夠了，不需要背景 push。
async fn todo_list(shell: web::Data<SharedShell>) -> impl Responder {
    let items = lock_shell(&shell)
        .plugin_mut("todo")
        .and_then(|p| p.as_any_mut().downcast_mut::<TodoPlugin>())
        .map(|t| t.snapshot())
        .unwrap_or_default();
    HttpResponse::Ok().json(items)
}

#[derive(Deserialize)]
struct TodoAddQuery {
    text: String,
}

/// `POST /api/todo/add?text=<內容>`：新增一筆待辦。跟 `storage` 的寫入端點
/// 一樣只回成功/失敗、不夾帶資料，前端收到 200 之後自己重新 `GET
/// /api/todo/list` 拿最新清單。
async fn todo_add(shell: web::Data<SharedShell>, query: web::Query<TodoAddQuery>) -> HttpResponse {
    let mut sh = lock_shell(&shell);
    let Some(todo) = sh.plugin_mut("todo").and_then(|p| p.as_any_mut().downcast_mut::<TodoPlugin>()) else {
        return HttpResponse::InternalServerError().finish();
    };
    match todo.add_item(query.text.clone()) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(_) => HttpResponse::BadRequest().finish(),
    }
}

#[derive(Deserialize)]
struct TodoIdQuery {
    id: u64,
}

/// `POST /api/todo/toggle?id=<id>`：切換完成/未完成。
async fn todo_toggle(shell: web::Data<SharedShell>, query: web::Query<TodoIdQuery>) -> HttpResponse {
    let mut sh = lock_shell(&shell);
    let Some(todo) = sh.plugin_mut("todo").and_then(|p| p.as_any_mut().downcast_mut::<TodoPlugin>()) else {
        return HttpResponse::InternalServerError().finish();
    };
    match todo.toggle_item(query.id) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(_) => HttpResponse::BadRequest().finish(),
    }
}

/// `POST /api/todo/remove?id=<id>`：刪除一筆待辦。
async fn todo_remove(shell: web::Data<SharedShell>, query: web::Query<TodoIdQuery>) -> HttpResponse {
    let mut sh = lock_shell(&shell);
    let Some(todo) = sh.plugin_mut("todo").and_then(|p| p.as_any_mut().downcast_mut::<TodoPlugin>()) else {
        return HttpResponse::InternalServerError().finish();
    };
    match todo.remove_item(query.id) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(_) => HttpResponse::BadRequest().finish(),
    }
}

#[derive(Deserialize)]
struct CrossRelayRequest {
    domain: String,
    ask: CrossDomainAsk,
}

/// `POST /api/remote/cross-relay`：同網域內、client 角色的機器（`system` 沒
/// 設成 server，沒有自己連 MQTT）發起跨 domain remote 時，打這個給自己的
/// `server <ip>` 中繼——實際加密/發布到 MQTT 的邏輯完全共用
/// `shell::send_cross_domain_request`（見該函式的說明），這裡只是它對外的
/// HTTP 入口。限定呼叫這個端點的機器自己必須真的是 server：不然
/// `send_cross_domain_request` 判斷「不是 server」又會想去中繼給*它自己*設定
/// 的 `server_addr`，變成沒有意義的一直往下轉——這裡直接擋掉，只允許中繼
/// 這一跳，不是無限鏈。
async fn remote_cross_relay(body: web::Json<CrossRelayRequest>, ctx: web::Data<SharedContext>) -> impl Responder {
    let is_server = ctx.lock().unwrap().is_server;
    if !is_server {
        return HttpResponse::BadRequest().body("這台機器不是 system server，沒辦法中繼跨 domain 請求");
    }
    let CrossRelayRequest { domain, ask } = body.into_inner();
    let ctx = ctx.get_ref().clone();
    let result = tokio::task::spawn_blocking(move || send_cross_domain_request(&ctx, &domain, ask))
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("內部錯誤")));
    match result {
        Ok(reply) => HttpResponse::Ok().json(reply),
        Err(err) => HttpResponse::InternalServerError().body(format!("{err:#}")),
    }
}

/// `GET /api/music/copy`：`plugins::music` 的 `copy ... from <id>` 同網域
/// 版本用這個查詢目標裝置 `music/` 資料夾裡有哪些檔案（連同大小，讓對方能靠
/// 已知的檔案大小判斷拉到哪裡算拉完，不需要伺服器另外回報「這是不是最後
/// 一塊」）。`music/` 資料夾還不存在就當作空清單，不報錯——跟 `music_files`
/// 判斷資料夾不存在時的容錯邏輯一致。
///
/// 依檔名排序回傳：`global.rs` 的 `fetch_remote_file_list` 分頁時（見
/// `paginate_file_list`）每一頁都是各自重新呼叫這個端點，`fs::read_dir` 本身
/// 不保證每次呼叫順序一致，沒排序的話跨頁可能漏掉或重複某些檔案。
async fn music_copy_list() -> impl Responder {
    let mut files: Vec<FileMeta> = std::fs::read_dir(MUSIC_DIR)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let size = entry.metadata().ok()?.len();
                    Some(FileMeta { name, size })
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort_by(|a, b| a.name.cmp(&b.name));
    HttpResponse::Ok().json(files)
}

/// `GET /api/music/copy/{name}`：下載一個檔案的原始位元組。用
/// `actix_files::NamedFile` 是因為它會自動處理 `Range` 請求（跟
/// `music_file_audio` 同樣的理由）——跨 domain 中繼（`global.rs` 的
/// `fetch_file_chunk`）就是靠對這個端點送 `Range` 請求，一次只拿一個 chunk，
/// 不用整個檔案讀進記憶體再自己切。
async fn music_copy_download(path: web::Path<String>, req: HttpRequest) -> HttpResponse {
    let name = path.into_inner();
    let Some(file_path) = safe_music_copy_path(&name) else {
        return HttpResponse::BadRequest().finish();
    };
    match actix_files::NamedFile::open(&file_path) {
        Ok(file) => file.into_response(&req),
        Err(_) => HttpResponse::NotFound().finish(),
    }
}

/// `POST /api/music/copy/{name}?offset=<n>`：把 request body 的原始位元組
/// 寫進檔案的 `offset` 位置。`offset` 沒帶就當 0（同網域直接整檔上傳的簡單
/// 情境，見 `plugins::music::push_file_http`——一次 POST、body 是整個檔案）；
/// 跨 domain 中繼（`global.rs` 的 `push_file_chunk`）則是每個 chunk 各自帶自己
/// 的 `offset`，靠這個組回原本的檔案，不需要另外一個「這是不是最後一塊」的
/// 參數——`offset == 0` 那一次順便建立/清空檔案（同一個檔案的第一個 chunk
/// 保證一定是 offset 0，因為 `plugins::music` 是照順序、等前一個 chunk 的
/// 回應之後才送下一個），之後的 chunk 只要 seek 到對的位置寫入即可。
#[derive(Deserialize)]
struct MusicCopyUploadQuery {
    offset: Option<u64>,
}

async fn music_copy_upload(
    path: web::Path<String>,
    query: web::Query<MusicCopyUploadQuery>,
    body: web::Bytes,
) -> HttpResponse {
    let name = path.into_inner();
    let Some(file_path) = safe_music_copy_path(&name) else {
        return HttpResponse::BadRequest().finish();
    };
    if std::fs::create_dir_all(MUSIC_DIR).is_err() {
        return HttpResponse::InternalServerError().finish();
    }
    let offset = query.offset.unwrap_or(0);
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new().write(true).create(true).truncate(offset == 0).open(&file_path)?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(&body)?;
        Ok(())
    })();
    match result {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}

/// `GET /api/storage/list?path=<相對路徑>`：列出該路徑底下的項目。`path` 是
/// 空字串時代表根目錄本身（`safe_storage_path` 一律拒絕空字串，因為那對「檔案
/// 名稱」來說沒有意義，但對「要不要列根目錄」這個情境需要特別放行，所以這裡
/// 直接特判，不透過 `safe_storage_path`）。
#[derive(Deserialize)]
struct StoragePathQuery {
    path: String,
}

async fn storage_list(query: web::Query<StoragePathQuery>) -> HttpResponse {
    let target = if query.path.is_empty() {
        Some(PathBuf::from(STORAGE_DIR))
    } else {
        safe_storage_path(Path::new(STORAGE_DIR), &query.path)
    };
    let Some(target) = target else {
        return HttpResponse::BadRequest().finish();
    };
    match list_dir(&target) {
        Ok(entries) => HttpResponse::Ok().json(entries),
        Err(_) => HttpResponse::NotFound().finish(),
    }
}

/// `GET /api/storage/download?path=<相對檔案路徑>`：下載單一檔案。用
/// `actix_files::NamedFile` 是因為它會自動處理 `Range` 請求，跟現有
/// `music_copy_download`/`music_file_audio` 同樣的理由——大檔案/影片也能拖拉進度。
async fn storage_download(query: web::Query<StoragePathQuery>, req: HttpRequest) -> HttpResponse {
    let Some(file_path) = safe_storage_path(Path::new(STORAGE_DIR), &query.path) else {
        return HttpResponse::BadRequest().finish();
    };
    match actix_files::NamedFile::open(&file_path) {
        Ok(file) => file.into_response(&req),
        Err(_) => HttpResponse::NotFound().finish(),
    }
}

/// `POST /api/storage/upload?path=<目的地相對路徑，含檔名>`：把 request body
/// 的原始位元組整個寫成一個檔案，同名直接覆蓋。目的地的上層資料夾必須已經
/// 存在（不會自動建立中間目錄——要先用 `storage/mkdir` 建立），這跟真實 NAS
/// 產品「先建資料夾、才能上傳進去」的使用順序一致。
async fn storage_upload(query: web::Query<StoragePathQuery>, body: web::Bytes) -> HttpResponse {
    let Some(file_path) = safe_storage_path(Path::new(STORAGE_DIR), &query.path) else {
        return HttpResponse::BadRequest().finish();
    };
    let Some(parent) = file_path.parent() else {
        return HttpResponse::BadRequest().finish();
    };
    if !parent.is_dir() {
        return HttpResponse::BadRequest().finish();
    }
    match std::fs::write(&file_path, &body) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}

/// `POST /api/storage/mkdir?path=<相對路徑>`：建立資料夾。
async fn storage_mkdir(query: web::Query<StoragePathQuery>) -> HttpResponse {
    let Some(dir_path) = safe_storage_path(Path::new(STORAGE_DIR), &query.path) else {
        return HttpResponse::BadRequest().finish();
    };
    match make_dir(&dir_path) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(_) => HttpResponse::BadRequest().finish(),
    }
}

/// `POST /api/storage/delete?path=<相對路徑>&recursive=<bool>`：刪除檔案/資料夾。
#[derive(Deserialize)]
struct StorageDeleteQuery {
    path: String,
    #[serde(default)]
    recursive: bool,
}

async fn storage_delete(query: web::Query<StorageDeleteQuery>) -> HttpResponse {
    let Some(target) = safe_storage_path(Path::new(STORAGE_DIR), &query.path) else {
        return HttpResponse::BadRequest().finish();
    };
    match remove(&target, query.recursive) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(_) => HttpResponse::BadRequest().finish(),
    }
}

/// `POST /api/storage/rename?from=<相對路徑>&to=<相對路徑>`：重新命名/搬移。
#[derive(Deserialize)]
struct StorageRenameQuery {
    from: String,
    to: String,
}

async fn storage_rename(query: web::Query<StorageRenameQuery>) -> HttpResponse {
    let (Some(from), Some(to)) = (
        safe_storage_path(Path::new(STORAGE_DIR), &query.from),
        safe_storage_path(Path::new(STORAGE_DIR), &query.to),
    ) else {
        return HttpResponse::BadRequest().finish();
    };
    match rename_path(&from, &to) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(_) => HttpResponse::BadRequest().finish(),
    }
}

/// `GET /api/storage/sync-manifest`：回傳整棵 `storage/` 樹（含子資料夾、每個
/// 檔案的 hash）攤平後的清單，同網域的 `sync` plugin 用這個端點取得對方的完
/// 整清單去跟本機清單、baseline 比對。同網域走 HTTP，沒有 MQTT 那種單則訊息
/// 大小限制，所以不分頁，直接回傳全部，跟這個檔案裡其他既有的
/// `storage_list`/`music_copy_list`/`music_files` 端點一樣不分頁。
async fn storage_sync_manifest() -> HttpResponse {
    match walk_with_hashes(Path::new(STORAGE_DIR)) {
        Ok(entries) => HttpResponse::Ok().json(entries),
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
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

/// 跟 `broadcast_ticker` 同樣的節奏／比對邏輯，但只服務 `global`／`device`
/// 這兩個名字，推的是 `table_snapshot_json` 算出來的 JSON 字串，走獨立的
/// `hub.snapshot_channels`／`hub.snapshot_cache`，不影響 `channels`／`cache`
/// 那組既有純文字頻道的消費者（`remote_output`、跨 domain `Panel` 查詢）。
async fn snapshot_broadcast_ticker(shell: Arc<Mutex<Shell>>, hub: Hub, names: Vec<String>) {
    let mut interval = tokio::time::interval(TICK);
    loop {
        interval.tick().await;
        let shell = shell.clone();
        let names = names.clone();
        let texts = tokio::task::spawn_blocking(move || {
            names
                .into_iter()
                .map(|name| {
                    let text = table_snapshot_json(&shell, &name);
                    (name, text)
                })
                .collect::<Vec<_>>()
        })
        .await;
        let Ok(texts) = texts else { continue };
        let mut cache = hub.snapshot_cache.lock().unwrap();
        for (name, text) in texts {
            if cache.get(&name) == Some(&text) {
                continue;
            }
            if let Some(tx) = hub.snapshot_channels.get(&name) {
                let _ = tx.send(text.clone());
            }
            cache.insert(name, text);
        }
    }
}

/// `global`／`device` 這兩個 panel 在 web 這邊不是推純文字，是推結構化的
/// JSON（表頭＋每一格的文字＋「這一格剛剛變了沒」），讓前端能逐格套用閃爍
/// 效果，見 `frontend.html` 的 `renderTableSnapshot`。跟 `gui.rs` 的
/// `with_global`／`with_device` 是同一個「向下轉型拿具體型別」的做法，只是
/// 這裡要的是 web 專用的那份 `web_snapshot()`（TUI／web 各自獨立的比對狀態，
/// 見 `table_diff::RowDiffTracker` 的說明）。由 `snapshot_broadcast_ticker`
/// 呼叫，走獨立的 `/api/panel/{name}/snapshot-stream`，不再是
/// `panel_text_for`／既有的 `/stream` 端點（見該函式的說明）。
fn table_snapshot_json(shell: &Mutex<Shell>, name: &str) -> String {
    let mut sh = lock_shell(shell);
    let snapshot = if name == "global" {
        sh.plugin_mut("global").and_then(|p| p.as_any_mut().downcast_mut::<GlobalPlugin>()).map(|g| g.web_snapshot())
    } else {
        sh.plugin_mut("device").and_then(|p| p.as_any_mut().downcast_mut::<DevicePlugin>()).map(|d| d.web_snapshot())
    };
    drop(sh);
    match snapshot {
        Some(snapshot) => serde_json::to_string(&snapshot.to_json()).unwrap_or_default(),
        None => String::new(),
    }
}

async fn index() -> impl Responder {
    HttpResponse::Ok().content_type("text/html; charset=utf-8").body(FRONTEND_HTML)
}

/// `GET /tablet`：跟桌面版 `/` 完全獨立的平板/觸控用頁面（`src/web/tablet.html`），
/// 打同一組既有 API，這個 handler 本身不需要 `ctx`/`hub` 之類的依賴。
async fn tablet_index() -> impl Responder {
    HttpResponse::Ok().content_type("text/html; charset=utf-8").body(TABLET_HTML)
}

/// `GET /iphone`：跟桌面版 `/`／`/tablet` 完全獨立的 iPhone PWA 頁面
/// （`src/web/iphone.html`），打同一組既有 API，這個 handler 本身不需要
/// `ctx`/`hub` 之類的依賴。
async fn iphone_index() -> impl Responder {
    HttpResponse::Ok().content_type("text/html; charset=utf-8").body(IPHONE_HTML)
}

/// `GET /iphone-manifest.json`：PWA manifest，見 `IPHONE_MANIFEST_JSON` 的說明。
async fn iphone_manifest() -> impl Responder {
    HttpResponse::Ok().content_type("application/manifest+json").body(IPHONE_MANIFEST_JSON)
}

async fn iphone_icon_180() -> impl Responder {
    HttpResponse::Ok().content_type("image/png").body(IPHONE_ICON_180)
}

async fn iphone_icon_192() -> impl Responder {
    HttpResponse::Ok().content_type("image/png").body(IPHONE_ICON_192)
}

async fn iphone_icon_512() -> impl Responder {
    HttpResponse::Ok().content_type("image/png").body(IPHONE_ICON_512)
}

async fn api_plugins(hub: web::Data<Hub>) -> impl Responder {
    let mut names: Vec<&String> = hub.channels.keys().collect();
    names.sort();
    HttpResponse::Ok().json(names)
}

#[derive(Serialize)]
struct VersionResponse {
    id: String,
    version: &'static str,
}

/// `GET /api/version`：這台機器的 id（`sysinfo::hostname()`，跟 `device`／
/// `global` 清單裡的 id 是同一個值）＋版本號（`plugin::APP_VERSION`，寫死在
/// 原始碼裡，跟 `system` plugin 的 `version` 指令/panel 是同一份資料），給
/// 前端畫面最左上角顯示用。
async fn api_version() -> impl Responder {
    HttpResponse::Ok().json(VersionResponse { id: sysinfo::hostname(), version: APP_VERSION })
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
    let shell_for_blocking = shell.get_ref().clone();
    let output_for_blocking = output.get_ref().clone();
    let (result, upgrade_requested) = tokio::task::spawn_blocking(move || {
        let mut sh = lock_shell(&shell_for_blocking);
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') && !trimmed.starts_with('!') {
            output_for_blocking.push(&format!("{}{}\n", sh.prompt(), trimmed));
        }
        let error = sh.execute_line(&line).err().map(|err| format!("{err:#}"));
        if let Some(msg) = &error {
            output_for_blocking.push(&format!("錯誤: {msg}\n"));
        }
        let prompt = sh.prompt();
        output_for_blocking.push(&format!("{prompt}\n"));
        // `upgrade`（見 `shell::run_upgrade`）跟 `exit` 一樣是透過
        // `execute_line` 處理的旗標，這裡也要跟 CLI/GUI 一樣接住——這正是
        // 「透過 remote plugin 轉發到別台機器的 /api/exec 觸發 upgrade」這個
        // 使用情境實際會走到的地方。
        let upgrade_requested = sh.take_pending_upgrade();
        (ExecResponse { prompt, error }, upgrade_requested)
    })
    .await
    .unwrap_or_else(|_| (ExecResponse { prompt: String::new(), error: Some("內部錯誤".to_string()) }, false));
    if upgrade_requested {
        run_upgrade(shell.get_ref().clone(), output.get_ref().clone());
    }
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

/// `GET /api/panel/{name}/snapshot-stream`：跟 `panel_stream` 同樣的「先送
/// 一次快取內容、之後有變化再推」邏輯，但只有 `global`／`device` 有對應的
/// channel（其他名字回 404），送的是結構化 JSON snapshot，給
/// `frontend.html` 的逐格閃爍表格用。
async fn panel_snapshot_stream(path: web::Path<String>, hub: web::Data<Hub>) -> impl Responder {
    let name = path.into_inner();
    let Some(tx) = hub.snapshot_channels.get(&name).cloned() else {
        return HttpResponse::NotFound().finish();
    };
    let mut rx = tx.subscribe();
    let initial = hub.snapshot_cache.lock().unwrap().get(&name).cloned();

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

/// `/api/music/files` 一筆回應：`favorite` 是 webui/tablet/iphone 三邊播放器
/// 共用的「非收藏歌曲輪到下一首時機率性跳過」功能要用的旗標（見
/// `crate::plugins::load_favorites`），播放清單本身仍然照檔名排序，跳不跳過
/// 完全是前端播放邏輯的事，這裡只負責把目前的收藏狀態一起帶出去。
#[derive(Serialize)]
struct MusicFileJson {
    name: String,
    favorite: bool,
}

/// `GET /api/music/files`：`music/` 資料夾裡目前有的檔案名稱（依字母排序），
/// 資料夾還不存在就當作空清單，不報錯——跟 `MusicPlugin::list_text()` 判斷
/// 資料夾不存在時的容錯邏輯一致。只列 `.mp3`，`download` 順便存的 `.srt`
/// 歌詞字幕檔是附屬品，不該被當成清單裡可以播放/刪除的一個項目（見
/// `MusicPlugin::list_text()` 的同一個理由）。
async fn music_files() -> impl Responder {
    let favorites = crate::plugins::load_favorites();
    let items: Vec<MusicFileJson> = std::fs::read_dir(MUSIC_DIR)
        .map(|entries| {
            let mut items: Vec<MusicFileJson> = entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.to_ascii_lowercase().ends_with(".mp3"))
                .map(|name| MusicFileJson { favorite: favorites.contains(&name), name })
                .collect();
            items.sort_by(|a, b| a.name.cmp(&b.name));
            items
        })
        .unwrap_or_default();
    HttpResponse::Ok().json(items)
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

/// `DELETE /api/music/file/{name}`：從 `music/` 資料夾刪掉這個檔案。順手把
/// 收藏索引裡對應的項目也清掉（見 `crate::plugins::remove_favorite`），不留
/// 著指向已經不存在的檔案。
async fn music_file_delete(path: web::Path<String>) -> impl Responder {
    let name = path.into_inner();
    let Some(file_path) = safe_music_path(&name) else {
        return HttpResponse::BadRequest().finish();
    };
    match std::fs::remove_file(&file_path) {
        Ok(()) => {
            crate::plugins::remove_favorite(&name);
            HttpResponse::Ok().finish()
        }
        Err(_) => HttpResponse::NotFound().finish(),
    }
}

/// `POST /api/music/file/{name}/favorite`：切換一首歌是不是收藏，回傳切換後
/// 的狀態 `{"favorite": bool}`（前端不用再另外 `GET /api/music/files` 才知道
/// 切換成功與否）。跟 `music_file_delete`/`music_file_cover` 一樣先用
/// `safe_music_path` 驗證檔名，並且要求檔案真的存在——收藏一個不存在的檔名
/// 沒有意義，也會讓索引裡累積垃圾。
async fn music_file_favorite(path: web::Path<String>) -> HttpResponse {
    let name = path.into_inner();
    let Some(file_path) = safe_music_path(&name) else {
        return HttpResponse::BadRequest().finish();
    };
    if !file_path.is_file() {
        return HttpResponse::NotFound().finish();
    }
    match crate::plugins::toggle_favorite(&name) {
        Ok(favorite) => HttpResponse::Ok().json(serde_json::json!({ "favorite": favorite })),
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}

/// `GET /api/ereader/books`：`storage/ebooks/` 資料夾裡目前有的 `.epub`
/// 檔案，各自帶標題/作者/目前讀到多少%（見 `crate::plugins::list_books`）。
async fn ereader_books() -> impl Responder {
    HttpResponse::Ok().json(list_books())
}

/// `GET /api/ereader/book/{name}/meta`：這本書的標題/作者/翻頁方向
/// （`"ltr"`/`"rtl"`）/章節清單/目前進度（見 `crate::plugins::book_meta`）。
/// 檔名不合法或這本書打不開都回 404——跟其他 `music_file_*` 端點一樣，不用
/// 特別區分「檔名不合法」跟「檔案不存在/解析失敗」，對呼叫端來說結果一樣
/// 是「這個端點目前沒東西可以給」。
async fn ereader_book_meta(path: web::Path<String>) -> HttpResponse {
    let name = path.into_inner();
    match book_meta(&name) {
        Ok(meta) => HttpResponse::Ok().json(meta),
        Err(_) => HttpResponse::NotFound().finish(),
    }
}

/// `GET /api/ereader/book/{name}/cover`：封面圖，`epub` crate 的
/// `get_cover()`。沒有封面（或書打不開）都回 404，跟 `music_file_cover`
/// 同一套「沒有封面是正常情況，不是錯誤」的處理方式。
async fn ereader_book_cover(path: web::Path<String>) -> HttpResponse {
    let name = path.into_inner();
    match book_cover(&name) {
        Some((bytes, mime)) => HttpResponse::Ok().content_type(mime).body(bytes),
        None => HttpResponse::NotFound().finish(),
    }
}

#[derive(Deserialize)]
struct EreaderProgressBody {
    chapter: usize,
}

/// `POST /api/ereader/book/{name}/progress`：存目前讀到第幾章（spine
/// index，從 0 開始）。跟 `todo_toggle` 一樣只回成功/失敗、不夾帶資料，前端
/// 收到 200 之後自己決定要不要重新 `GET .../meta` 拿最新進度%。
async fn ereader_book_progress(path: web::Path<String>, body: web::Json<EreaderProgressBody>) -> HttpResponse {
    let name = path.into_inner();
    if safe_ebook_path(&name).is_none() {
        return HttpResponse::BadRequest().finish();
    }
    match save_chapter_progress(&name, body.chapter) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}

/// `GET /api/ereader/book/{name}/resource/{path:.*}`：epub 內部任意檔案
/// （章節 XHTML、圖片、CSS、字型），原封不動吐出去，`Content-Type` 用 epub
/// manifest 記錄的 mime type。這個端點的 URL 路徑刻意跟 epub 內部路徑一樣
/// （見 `crate::plugins::book_resource` 的說明），章節 XHTML 裡原有的相對
/// 路徑（圖片/CSS/字型）瀏覽器會自動解析到對應的 `/resource/...` URL，不
/// 需要重寫任何 HTML 內容，直式書的 `writing-mode: vertical-rl` 也因此完全
/// 不用特別處理，瀏覽器原生渲染就會生效。`text/css` 資源額外做
/// `normalize_vertical_css`：舊格式直式書用 Apple 的 `-epub-writing-mode`
/// vendor prefix，現代瀏覽器不認得，這裡補一份標準寫法的 `writing-mode`，
/// 這種舊格式的直式排版才會生效（見該函式的說明）。章節 XHTML/HTML 資源
/// 額外做 `inject_pagination_style`：依 `chapter_is_vertical` 判斷這一章是
/// 不是直式，插入對應的分頁 CSS，讓瀏覽器自動把整章內容依視窗大小切成一頁
/// 一頁，前端靠左右捲動來翻頁（橫式/直式套用的 CSS 完全不同，見該函式的
/// 說明）。
async fn ereader_book_resource(path: web::Path<(String, String)>) -> HttpResponse {
    let (name, resource_path) = path.into_inner();
    let Some((bytes, mime)) = book_resource(&name, &resource_path) else {
        return HttpResponse::NotFound().finish();
    };
    if mime == "text/css" {
        let Ok(css) = String::from_utf8(bytes) else {
            return HttpResponse::NotFound().finish();
        };
        return HttpResponse::Ok().content_type(mime).body(normalize_vertical_css(&css));
    }
    if mime == "application/xhtml+xml" || mime == "text/html" {
        let Ok(html) = String::from_utf8(bytes) else {
            return HttpResponse::NotFound().finish();
        };
        let vertical = chapter_is_vertical(&name, &resource_path, &html);
        return HttpResponse::Ok().content_type(mime).body(inject_pagination_style(&html, vertical));
    }
    HttpResponse::Ok().content_type(mime).body(bytes)
}

/// `POST /api/ereader/upload/{name}`：把使用者從外部書源（例如好讀）下載好
/// 的 epub 檔案存進 `storage/ebooks/`，同名直接覆蓋。檔名（含副檔名）由
/// 呼叫端決定——前端固定帶瀏覽器選到的檔案原始檔名，`safe_ebook_path` 擋
/// 路徑分隔符/上層目錄，額外要求副檔名是 `.epub`，避免書架裡混進不相關的
/// 檔案（`list_books` 本來就只認 `.epub`，這裡先擋掉單純是不留垃圾檔案在
/// 資料夾裡）。這條路徑額外設了比預設值大很多的 payload 上限（見
/// `EREADER_UPLOAD_LIMIT`），電子書常常上百 MB，不能跟其他 API 共用
/// 256KB 的預設上限。
async fn ereader_upload(path: web::Path<String>, body: web::Bytes) -> HttpResponse {
    let name = path.into_inner();
    if !name.to_ascii_lowercase().ends_with(".epub") {
        return HttpResponse::BadRequest().finish();
    }
    let Some(file_path) = safe_ebook_path(&name) else {
        return HttpResponse::BadRequest().finish();
    };
    match std::fs::write(&file_path, &body) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}

#[derive(Deserialize)]
struct HaodooImportBody {
    url: String,
    #[serde(default)]
    vertical: bool,
}

#[derive(Serialize)]
struct HaodooImportResponse {
    name: String,
}

/// `POST /api/ereader/haodoo-import`：貼一個好讀（haodoo.net）書籍詳細頁的
/// 網址，伺服器端直接把 epub 抓下來存進 `storage/ebooks/`，前端不用先下載
/// 到本機再手動上傳一輪（見 `crate::plugins::haodoo_import` 的說明）。這裡
/// 會實際發出 HTTP 請求（用 `curl`），用 `spawn_blocking` 丟去背景執行緒，
/// 跟 `remote_cross_relay` 同一套「阻塞式 I/O 不要卡住 async runtime」的
/// 做法。
async fn ereader_haodoo_import(body: web::Json<HaodooImportBody>) -> HttpResponse {
    let HaodooImportBody { url, vertical } = body.into_inner();
    let result = tokio::task::spawn_blocking(move || haodoo_import(&url, vertical))
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("內部錯誤")));
    match result {
        Ok(name) => HttpResponse::Ok().json(HaodooImportResponse { name }),
        Err(err) => HttpResponse::BadRequest().body(format!("{err:#}")),
    }
}

/// `GET /api/wallpaper/list`：`storage/wallpaper/` 資料夾裡目前有的圖片
/// 檔名（見 `crate::plugins::list_wallpapers`）。圖片本身的位元組不用另外
/// 做端點，直接沿用既有的 `GET /api/storage/download?path=wallpaper/<檔名>`
/// 就能拿到（`actix_files::NamedFile` 對圖片類型的 mime 預設就是
/// `Content-Disposition: inline`，直接當 `<img src>`/CSS 背景圖用沒問題）。
async fn wallpaper_list() -> impl Responder {
    HttpResponse::Ok().json(list_wallpapers())
}

#[derive(Serialize)]
struct WallpaperCurrentResponse {
    selected: Option<String>,
    rotate: bool,
}

/// `GET /api/wallpaper/current`：目前選的桌布檔名（沒選過是 `null`）跟
/// 自動輪播開關現在的狀態——前端開頁面時靠這個決定要不要啟動輪播計時器
/// （見 `crate::plugins::rotate_enabled` 的說明，計時器本身在前端跑）。
async fn wallpaper_current() -> impl Responder {
    HttpResponse::Ok().json(WallpaperCurrentResponse { selected: current_wallpaper(), rotate: rotate_enabled() })
}

#[derive(Deserialize)]
struct WallpaperSelectBody {
    name: String,
}

/// `POST /api/wallpaper/select`：換桌布，`name` 必須是 `list_wallpaper`
/// 清單裡真的存在的檔名（`select_wallpaper` 裡面會擋，見該函式的說明）。
async fn wallpaper_select(body: web::Json<WallpaperSelectBody>) -> HttpResponse {
    match select_wallpaper(&body.name) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(err) => HttpResponse::BadRequest().body(format!("{err:#}")),
    }
}

#[derive(Deserialize)]
struct WallpaperRotateBody {
    enabled: bool,
}

/// `POST /api/wallpaper/rotate`：開關自動輪播（實際計時器在前端跑，見
/// `crate::plugins::set_rotate_enabled` 的說明），這裡只負責存這個開關的
/// 狀態，讓重新整理/其他裝置也能讀到目前是開是關。
async fn wallpaper_rotate(body: web::Json<WallpaperRotateBody>) -> HttpResponse {
    match set_rotate_enabled(body.enabled) {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(err) => HttpResponse::InternalServerError().body(format!("{err:#}")),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 從一段 HTML/JS 原始碼裡擷取 `function computeLayout(...) { ... }` 的完整
    /// 函式本體（含開頭的 `function computeLayout(` 到跟它配對的最後一個 `}`）。
    /// 不能靠固定行數切，用最簡單的括號計數：從函式名稱後第一個 `{` 開始數，
    /// 每個 `{` +1、每個 `}` -1，數到 0 就是配對的結尾。
    fn extract_compute_layout(src: &str) -> String {
        const NEEDLE: &str = "function computeLayout(";
        let start = src.find(NEEDLE).expect("找不到 computeLayout 函式");
        let brace_start = src[start..].find('{').map(|i| start + i).expect("computeLayout 少了開頭的 {");
        let bytes = src.as_bytes();
        let mut depth = 0i32;
        let mut end = brace_start;
        for (offset, &b) in bytes[brace_start..].iter().enumerate() {
            match b {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = brace_start + offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        src[start..=end].to_string()
    }

    /// `frontend.html`（桌面版）跟 `tablet.html`（`/tablet`）各自獨立一份
    /// topology 佈局程式碼（既有慣例，不共用模組），但兩邊的 `computeLayout`
    /// 函式本體必須實作同一套演算法、逐字相同——這個測試就是那道 drift
    /// guard：改了一邊的 `computeLayout` 卻忘了同步改另一邊，這裡就會炸掉。
    #[test]
    fn frontend_and_tablet_compute_layout_are_identical() {
        let frontend = extract_compute_layout(FRONTEND_HTML);
        let tablet = extract_compute_layout(TABLET_HTML);
        assert_eq!(
            frontend, tablet,
            "frontend.html and tablet.html's computeLayout() have drifted apart — this project intentionally keeps two independent copies of the topology layout code, but they must implement the identical algorithm; if you changed one, apply the same change to the other."
        );
    }
}
