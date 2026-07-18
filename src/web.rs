use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use async_stream::stream;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::output::OutputBuffer;
use crate::shell::{lock_shell, Shell};

type SharedShell = Arc<Mutex<Shell>>;

const PORT: u16 = 9759;
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
pub fn spawn(shell: Arc<Mutex<Shell>>, output: Arc<OutputBuffer>) {
    std::thread::spawn(move || {
        let out_for_err = output.clone();
        if let Err(err) = actix_web::rt::System::new().block_on(run_server(shell, output)) {
            out_for_err.push(&format!("web server 啟動失敗: {err:#}\n"));
        }
    });
}

async fn run_server(shell: Arc<Mutex<Shell>>, output: Arc<OutputBuffer>) -> std::io::Result<()> {
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
            .route("/", web::get().to(index))
            .route("/api/plugins", web::get().to(api_plugins))
            .route("/api/panel/{name}/stream", web::get().to(panel_stream))
            .route("/api/prompt", web::get().to(prompt))
            .route("/api/exec", web::post().to(exec))
    })
    .bind(("0.0.0.0", PORT))?
    .run()
    .await
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
