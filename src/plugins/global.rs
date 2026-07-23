use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use data_encoding::BASE64;
use rumqttc::{Client, Event, MqttOptions, Packet, QoS};
use unicode_width::UnicodeWidthStr;

use crate::crypto;
use crate::output::OutputBuffer;
use crate::plugin::{
    global_registry_key, merged_global_view, DeviceReport, FileMeta, GlobalListItem, GlobalRegistryEntry, Plugin,
    RemoteReply, RemoteRequest, SharedContext, FILE_CHUNK_SIZE, FILE_LIST_PAGE_BUDGET,
};
use crate::plugins::{safe_file_path, url_encode_filename, ALLOWED_FOLDERS, REPORT_INTERVAL};
use crate::shell;
use crate::sysinfo;
use crate::web::PORT;

const BROKER_HOST: &str = "broker.emqx.io";
const BROKER_PORT: u16 = 1883;

/// 多久沒收到一筆資料就算離線（不管是 server 角色透過 MQTT 收到的、還是
/// client 角色透過 `/api/global/list` 拉到的），跟 `DevicePlugin::ALIVE_TTL`
/// 同一個理由，設成回報間隔（`REPORT_INTERVAL`，publish/pull 都用這個週期）的
/// 3 倍，容許偶爾漏一兩次還不算離線。
const ALIVE_TTL: Duration = Duration::from_secs(REPORT_INTERVAL.as_secs() * 3);

const MANUAL_TEXT: &str = "\
global：串連好幾個「domain」互相知道對方存在。一個 domain 是一台 system server + \
若干 client 組成的一組機器（見 system plugin 的 mode/server）；global 讓不同 domain \
的 server 之間也能互相看到彼此的裝置清單。

運作方式：
  - server 角色：把自己 domain 目前的裝置清單（跟 device plugin 看到的一樣）發布到
    公開的 MQTT broker（broker.emqx.io），同時訂閱同一個 bridge-id 底下所有 domain
    的發布，這樣其他 domain 的裝置也會出現在這裡的清單裡。
  - client 角色：不會自己連 MQTT，改成定期跟自己的 server（system 的
    server <ip> 設定的目標）要一份現成的全域清單（GET /api/global/list）。
  - standalone 角色：domain/bridge 設定了也不會生效，跟沒設定一樣。

範例：
  domain office       設定這個 domain 的名字（只有 system mode server 時才有意義）
  bridge my-bridge-1  設定要加入的 bridge-id（同一個 bridge-id 底下的所有 domain
                      互相看得到；只有 server 才會真的去連 MQTT）
  status              顯示目前的 domain/bridge 設定，以及現在是不是 server 角色、
                      MQTT 有沒有真的生效
  list                列出目前看得到的所有跨 domain 裝置
  clear               重置整個 global 快取（清掉本機收到的所有跨 domain 資料，
                      等重新收到才會補回來）＋清掉自己這個 topic 在 broker 上的
                      殘留
  clear <bridge-id> <domain-name>
                      只清這一個 domain 的本機快取＋broker 上的殘留，其他
                      domain 的資料不受影響

MQTT topic 設計：<bridge-id>/<domain-name>/device，payload 是這個 domain 目前
的裝置清單（JSON 陣列）。故意不用 retain：活著的 domain 本來就每 REPORT_INTERVAL
秒 publish 一次，新加入的訂閱者最多等一輪就能看到所有目前還活著的 domain，用 retain
換來的「即時看到上一筆」不划算——broker 上留著的 retained 訊息不會因為那個 domain
真的下線就消失，重新訂閱（例如網路重連）時會被重新送一次，讓早就死掉的 domain
看起來像剛剛才有資料一樣「詐屍」。不用 retain 就沒有這個問題：一個 domain 停止
publish，不會有任何東西讓它的資料看起來又活過來。

注意事項：
  - bridge-id 用的是公開的 broker.emqx.io，建議取一個不容易被別人猜到/撞名的
    字串，不然可能會混進不相干的資料。
  - domain/bridge 中途改掉、或角色從 server 切成別的，都會在下一輪（最多
    REPORT_INTERVAL 秒）自動重新連線/停止，不需要重啟程式。
  - 早期版本（改成不用 retain 之前）發布過的訊息會留在 broker 上，不會自動消失。
    用 clear 清掉：發布一個空 payload（retain=true）到那個 topic，這是 MQTT
    官方定義的清除 retained 訊息的標準做法。clear 是背景執行（連線+等 broker
    確認可能要幾秒），結果用 status 查看。
";

pub struct GlobalPlugin {
    ctx: SharedContext,
    /// `bridge <id>` 設定的值。放在這裡（而不是 `ContextInner`）是因為除了
    /// 背景執行緒（`spawn_supervisor`/`run_mqtt_session`）之外沒有其他地方
    /// 需要讀它——跟需要給 `web::global_list` 讀的 `domain_name`（放在
    /// `ContextInner`）不一樣。
    bridge: Arc<Mutex<Option<String>>>,
    /// 目前的 MQTT session 是不是真的連上 broker——`run_mqtt_session` 收到
    /// `ConnAck` 才會設成 `true`，session 結束（不管是因為設定改變、還是連線
    /// 出錯）就設回 `false`。`status_text()` 讀這個顯示真實連線狀態，而不是
    /// 用「有沒有設定 domain/bridge」推論。
    connected: Arc<Mutex<bool>>,
    /// 上一次 `clear` 指令的結果訊息，`None` 代表還沒執行過。`clear` 本身丟到
    /// 背景執行緒跑（連線＋publish＋等 broker 確認可能要幾秒，不能卡住共用的
    /// `Shell` 鎖），跟 `MusicPlugin.downloads` 是同樣的做法：背景執行緒只更新
    /// 這個共用狀態，使用者用 `status` 查看結果。
    clear_status: Arc<Mutex<Option<String>>>,
}

impl GlobalPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        let bridge = Arc::new(Mutex::new(None));
        let connected = Arc::new(Mutex::new(false));
        Self::spawn_supervisor(ctx.clone(), bridge.clone(), connected.clone());
        Self { ctx, bridge, connected, clear_status: Arc::new(Mutex::new(None)) }
    }

    /// 背景監督執行緒，整個程式活著期間持續跑，每 `REPORT_INTERVAL` 檢查一次
    /// 目前的角色：
    /// - server：如果設定過 `bridge`，就跑一輪 MQTT session（`run_mqtt_session`
    ///   會一直跑到角色/bridge 設定改變或連線出錯才回傳，回傳後這裡會在下一次
    ///   迴圈重新檢查、視需要重新連線）。
    /// - 不是 server：如果 `system` plugin 設定過 `server <ip>`，就跟那台
    ///   server 要一份 `/api/global/list`（`pull_global_from_server`），跟
    ///   `system::pull_peers` 是同樣的邏輯，只是端點換成 global。
    fn spawn_supervisor(ctx: SharedContext, bridge: Arc<Mutex<Option<String>>>, connected: Arc<Mutex<bool>>) {
        thread::spawn(move || loop {
            let is_server = ctx.lock().unwrap().is_server;
            if is_server {
                let bridge_id = bridge.lock().unwrap().clone();
                if let Some(bridge_id) = bridge_id {
                    run_mqtt_session(bridge_id, ctx.clone(), bridge.clone(), connected.clone());
                }
            } else {
                let server_addr = ctx.lock().unwrap().server_addr.clone();
                if let Some(addr) = server_addr {
                    pull_global_from_server(&addr, &ctx);
                }
            }
            thread::sleep(REPORT_INTERVAL);
        });
    }

    fn set_domain(&mut self, name: &str, out: &OutputBuffer) -> Result<()> {
        self.ctx.lock().unwrap().domain_name = Some(name.to_string());
        out.push(&format!("domain 設定為 {name}（只有這台機器是 system mode server 時才會生效）\n"));
        Ok(())
    }

    fn set_bridge(&mut self, id: &str, out: &OutputBuffer) -> Result<()> {
        *self.bridge.lock().unwrap() = Some(id.to_string());
        out.push(&format!("bridge 設定為 {id}（只有這台機器是 system mode server 時才會生效）\n"));
        Ok(())
    }

    fn list(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push(&format!("{}\n", self.table_text()));
        Ok(())
    }

    /// `clear`：不接參數 vs. 接 `<bridge-id> <domain-name>` 是兩種不同範圍的清除：
    ///
    /// - 不接參數：重置**整個** `ctx.global`（不只自己這個 domain）——本機收到
    ///   的所有跨 domain 資料都清掉，等於「重新來過」：其他 domain 如果還活著，
    ///   下一輪（最多 REPORT_INTERVAL 秒）收到新資料就會重新出現；真的死了的
    ///   就不會再出現。自己這個 domain 一律現算 `ctx.devices`、不受影響，馬上
    ///   還是看得到。同時也發布空 payload 清掉自己這個 topic 在 broker 上的殘留
    ///   （避免其他 domain 的 server 之後收到我方的舊資料）。
    /// - 接 `<bridge-id> <domain-name>`：只清「這一個」domain 的本機快取＋
    ///   broker 上的殘留，其餘 domain 的資料保留——用在只想清掉某個已知是舊的/
    ///   別人家的 topic，不想連自己剛收到的其他 domain 資料也一起洗掉。
    ///
    /// broker 端的清除（發布空 payload，retain=true，MQTT 官方定義的清除
    /// retained 訊息的標準做法）丟到背景執行緒跑（連線＋等確認可能要幾秒，
    /// 不能卡住共用的 `Shell` 鎖），結果存進 `clear_status`，之後用 `status` 查看。
    fn clear(&mut self, args: &[String], out: &OutputBuffer) -> Result<()> {
        match args {
            [] => {
                let bridge = self.bridge.lock().unwrap().clone();
                let domain = self.ctx.lock().unwrap().domain_name.clone();
                let (Some(bridge), Some(domain)) = (bridge, domain) else {
                    bail!(
                        "目前沒有設定 domain/bridge，要嘛先設定，要嘛用 \
                         clear <bridge-id> <domain-name> 指定要清除哪個 topic"
                    );
                };
                self.ctx.lock().unwrap().global.clear();
                out.push("已清除本機收到的所有跨 domain 資料，等待重新建立\n");
                self.spawn_clear_broker(bridge, domain, out);
            }
            [bridge, domain] => {
                let (bridge, domain) = (bridge.clone(), domain.clone());
                self.ctx.lock().unwrap().global.retain(|_, entry| entry.domain != domain);
                out.push(&format!("已清除本機的 {domain} 資料（其他 domain 保留）\n"));
                self.spawn_clear_broker(bridge, domain, out);
            }
            _ => bail!(
                "clear 不接參數（重置整個 global 快取＋清自己這個 topic），\
                 或接 <bridge-id> <domain-name>（只清指定的 topic）"
            ),
        }
        Ok(())
    }

    /// 背景發布空 payload 清掉 `<bridge>/<domain>/device` 在 broker 上的殘留，
    /// `clear` 的兩種範圍（重置全部/只清一個 domain）都靠這個做實際的 MQTT 清除。
    fn spawn_clear_broker(&self, bridge: String, domain: String, out: &OutputBuffer) {
        let topic = format!("{bridge}/{domain}/device");
        out.push(&format!("正在清除 broker 上的殘留: {topic}（背景執行，稍後用 status 查看結果）\n"));

        let status = self.clear_status.clone();
        thread::spawn(move || {
            let result = clear_retained_topic(&topic);
            *status.lock().unwrap() = Some(match result {
                Ok(()) => format!("成功清除 {topic}"),
                Err(err) => format!("清除失敗 {topic}: {err:#}"),
            });
        });
    }

    fn status(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push(&self.status_text());
        Ok(())
    }

    /// 目前的 `domain`/`bridge` 設定，以及這台機器現在實際上是不是 server 角色、
    /// MQTT 有沒有真的在跑（`domain`/`bridge` 指令本身不檢查 `is_server`，設定
    /// 隨時可以下，這裡才是使用者確認「現在到底有沒有生效」的地方）。`connected`
    /// 是 `run_mqtt_session` 收到 `ConnAck` 才會設成 `true` 的真實連線狀態，不是
    /// 用「有沒有設定 domain/bridge」推論出來的。
    fn status_text(&self) -> String {
        let (domain, is_server, server_addr) = {
            let inner = self.ctx.lock().unwrap();
            (inner.domain_name.clone(), inner.is_server, inner.server_addr.clone())
        };
        let bridge = self.bridge.lock().unwrap().clone();
        let connected = *self.connected.lock().unwrap();

        let role = if is_server { "server" } else { "不是 server" };
        let mqtt_line = if !is_server {
            format!(
                "不連 MQTT，改成定期跟自己的 server 要 /api/global/list（server: {}）",
                server_addr.as_deref().unwrap_or("未設定")
            )
        } else if bridge.is_none() {
            "尚未生效（還沒設定 bridge）".to_string()
        } else if connected {
            "已連線（broker.emqx.io）".to_string()
        } else {
            "尚未連線（連線中，或連不上正在自動重試）".to_string()
        };

        let mut s = format!(
            "domain: {}\nbridge: {}\n角色: {role}（依 system mode 判斷）\nMQTT: {mqtt_line}\n",
            domain.as_deref().unwrap_or("未設定"),
            bridge.as_deref().unwrap_or("未設定"),
        );
        if let Some(clear_status) = &*self.clear_status.lock().unwrap() {
            s.push_str(&format!("上次 clear: {clear_status}\n"));
        }
        s
    }

    fn table_text(&self) -> String {
        let mut items = merged_global_view(&self.ctx.lock().unwrap());
        if items.is_empty() {
            return "(還沒有任何跨 domain 裝置資料——確認 domain/bridge 有沒有設定，\n\
                     或這台機器目前是不是 client 角色、它的 server 有沒有設定過)"
                .to_string();
        }
        items.sort_by(|a, b| (&a.domain, &a.report.id).cmp(&(&b.domain, &b.report.id)));

        let headers = ["domain", "id", "ip", "os", "version", "mode", "device uptime", "app uptime", "alive"];
        let rows: Vec<[String; 9]> = items
            .into_iter()
            .map(|item| {
                let alive = item.age_secs < ALIVE_TTL.as_secs_f64();
                [
                    item.domain,
                    item.report.id,
                    item.report.ip,
                    item.report.os,
                    item.report.version,
                    item.report.mode,
                    sysinfo::format_uptime(item.report.device_uptime_secs),
                    sysinfo::format_uptime(item.report.app_uptime_secs),
                    if alive { "*".to_string() } else { String::new() },
                ]
            })
            .collect();
        render_table(&headers, &rows)
    }
}

/// 一輪 MQTT session：訂閱 `<bridge-id>/+/device`（同一個 bridge 底下所有
/// domain 的發布都收得到），並另外開一個執行緒每 `REPORT_INTERVAL` 發布一次
/// 本機這個 domain 目前的裝置清單。會一直跑到下列任何一種情況才回傳，讓呼叫端
/// （`spawn_supervisor`）視情況重新連線：
/// - `bridge` 被改成別的值，或這台機器不再是 server 角色（發布執行緒偵測到後
///   呼叫 `client.disconnect()`，讓下面收訊息的迴圈自然結束）。
/// - 連線本身出錯（`connection.iter()` 回傳 `Err`）。
///
/// `connected` 是收到 `ConnAck`（代表真的跟 broker 交握成功）才設成 `true`；
/// session 不管怎麼結束，回傳前都會設回 `false`，`status_text()` 讀的是這個
/// 真實狀態，不是「有沒有設定 domain/bridge」這種推論。
fn run_mqtt_session(bridge_id: String, ctx: SharedContext, bridge: Arc<Mutex<Option<String>>>, connected: Arc<Mutex<bool>>) {
    // process id 讓每次啟動都是不同的 client id，避免上一次沒正常收尾的連線
    // （例如程式被強制中斷）在 broker 那邊還沒過期時，跟這一次的連線 id 衝突。
    let client_id = format!("cng5-{}-{}", sysinfo::hostname(), std::process::id());
    let mut opts = MqttOptions::new(client_id, BROKER_HOST, BROKER_PORT);
    opts.set_keep_alive(Duration::from_secs(30));
    let (client, mut connection) = Client::new(opts, 10);

    let sub_topic = format!("{bridge_id}/+/device");
    if client.subscribe(&sub_topic, QoS::AtMostOnce).is_err() {
        return;
    }
    // 跨 domain remote（見 `plugin.rs` 的 `RemoteRequest`/`RemoteReply`）額外
    // 訂閱的兩個 topic：`request` 是別的 domain 發過來要轉給我們自己裝置執行的
    // 請求，`reply` 是我們發出去的請求得到的回覆。跟 `device` 一樣訂 `+`
    // 萬用字元收下所有 domain 的訊息，`handle_incoming_publish` 再依內容裡的
    // `domain_name` 判斷是不是真的給我們的。
    if client.subscribe(format!("{bridge_id}/+/remote/request"), QoS::AtMostOnce).is_err()
        || client.subscribe(format!("{bridge_id}/+/remote/reply"), QoS::AtMostOnce).is_err()
    {
        return;
    }

    let publisher_client = client.clone();
    let publisher_ctx = ctx.clone();
    let session_bridge_id = bridge_id.clone();
    thread::spawn(move || loop {
        thread::sleep(REPORT_INTERVAL);
        let still_server = publisher_ctx.lock().unwrap().is_server;
        let current_bridge = bridge.lock().unwrap().clone();
        if !still_server || current_bridge.as_deref() != Some(session_bridge_id.as_str()) {
            let _ = publisher_client.disconnect();
            break;
        }
        let (domain, reports) = {
            let inner = publisher_ctx.lock().unwrap();
            (inner.domain_name.clone(), inner.devices.values().map(|entry| entry.report.clone()).collect::<Vec<_>>())
        };
        let Some(domain) = domain else { continue };
        let Ok(payload) = serde_json::to_string(&reports) else { continue };
        let topic = format!("{session_bridge_id}/{domain}/device");
        // 故意不用 retain，見檔案開頭 `MANUAL_TEXT` 的說明——不然一個 domain
        // 下線後，broker 上留著的最後一筆 retained 訊息會在別人重新訂閱時
        // 被重播，讓它看起來像剛剛才有資料一樣「詐屍」。
        let _ = publisher_client.publish(topic, QoS::AtMostOnce, false, payload);
    });

    // 把 `mqtt_client` 這個共用欄位（`Arc<Mutex<Option<(String, Client)>>>`）
    // 先拿出來，之後設定/清空都只鎖這個小 Mutex，不用每次都重新鎖一次整個 `ctx`。
    let mqtt_client_slot = ctx.lock().unwrap().mqtt_client.clone();
    for notification in connection.iter() {
        match notification {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                *connected.lock().unwrap() = true;
                // 真的連上之後才把 client 交給共用狀態，讓 `shell.rs`/`web.rs`
                // 需要發跨 domain 請求時能借用這個 client publish（連同
                // bridge_id 一起存，組 topic 要用）——這樣同一時間只會有這一條
                // MQTT 連線，不用自己另外開一條。
                *mqtt_client_slot.lock().unwrap() = Some((bridge_id.clone(), client.clone()));
            }
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                handle_incoming_publish(&publish.topic, &publish.payload, &client, &ctx);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    *connected.lock().unwrap() = false;
    // session 結束（不管什麼原因）就把共用的 client 清掉，不然 `shell.rs`/
    // `web.rs` 會拿著一個已經斷線、發布也不會有效果的 client 去用。
    *mqtt_client_slot.lock().unwrap() = None;
}

/// 收到一筆 MQTT 發布，依 topic 的形狀分流成三種：既有的裝置清單廣播、跨
/// domain remote 的請求、跨 domain remote 的回覆。哪一種都比對不上（不是這
/// 幾個約定的 topic 形狀）就當作雜訊直接丟掉，不報錯——公開 broker 上理論上
/// 不會有別人發，但沒有必要因為格式異常就讓整個 session 掛掉。
fn handle_incoming_publish(topic: &str, payload: &[u8], client: &Client, ctx: &SharedContext) {
    let parts: Vec<&str> = topic.split('/').collect();
    match parts.as_slice() {
        [_bridge, domain, "device"] => handle_device_publish(domain, payload, ctx),
        [bridge, domain, "remote", "request"] => handle_remote_request(bridge, domain, payload, client, ctx),
        [_bridge, domain, "remote", "reply"] => handle_remote_reply(domain, payload, ctx),
        _ => {}
    }
}

/// topic 格式是 `<bridge-id>/<domain-name>/device`，`domain` 是發布者自己的
/// domain，payload 是這個 domain 目前的裝置清單（JSON 陣列）。
fn handle_device_publish(domain: &str, payload: &[u8], ctx: &SharedContext) {
    let Ok(reports) = serde_json::from_slice::<Vec<DeviceReport>>(payload) else { return };
    let mut inner = ctx.lock().unwrap();
    for report in reports {
        let key = global_registry_key(domain, &report.id);
        inner.global.insert(key, GlobalRegistryEntry { domain: domain.to_string(), report, last_seen: Instant::now() });
    }
}

/// topic 格式是 `<bridge-id>/<domain>/remote/request`，這裡的 `domain` 是這則
/// 請求「要給誰」，不是發起者的 domain（發起者的 domain 在解密後的
/// `RemoteRequest::source_domain()` 裡，回覆要送到那裡）。因為訂閱時用的是
/// `+` 萬用字元，會收到所有 domain 的請求，只有 `domain` 等於自己現在設定的
/// `domain_name` 才需要處理，其餘（別人家的請求）直接忽略。
fn handle_remote_request(bridge_id: &str, domain: &str, payload: &[u8], client: &Client, ctx: &SharedContext) {
    let my_domain = ctx.lock().unwrap().domain_name.clone();
    if my_domain.as_deref() != Some(domain) {
        return;
    }
    let Ok(request) = crypto::open::<RemoteRequest>(payload) else { return };
    let reply = build_remote_reply(&request, ctx);
    let Ok(sealed) = crypto::seal(&reply) else { return };
    let reply_topic = format!("{bridge_id}/{}/remote/reply", request.source_domain());
    let _ = client.publish(reply_topic, QoS::AtMostOnce, false, sealed);
}

/// 實際處理一則跨 domain 請求：查自己的 `ctx.devices`（這是「自己這個
/// domain 裡有哪些裝置」，不是 `ctx.global`）找 `target_id` 對應的 ip，找不到
/// 就直接回錯誤，不用把「查不到」跟「轉發本身失敗」混在一起讓發起端猜。
fn build_remote_reply(request: &RemoteRequest, ctx: &SharedContext) -> RemoteReply {
    match request {
        RemoteRequest::Exec { request_id, target_id, line, .. } => {
            let Some(ip) = target_ip(ctx, target_id) else {
                return RemoteReply::Error {
                    request_id: request_id.clone(),
                    message: format!("目標裝置不存在: {target_id}"),
                };
            };
            match shell::remote_exec(&ip, line) {
                Ok((prompt, error)) => RemoteReply::Exec { request_id: request_id.clone(), prompt, error },
                Err(err) => RemoteReply::Error { request_id: request_id.clone(), message: format!("{err:#}") },
            }
        }
        RemoteRequest::Panel { request_id, target_id, panel_name, .. } => {
            let Some(ip) = target_ip(ctx, target_id) else {
                return RemoteReply::Error {
                    request_id: request_id.clone(),
                    message: format!("目標裝置不存在: {target_id}"),
                };
            };
            RemoteReply::Panel { request_id: request_id.clone(), text: fetch_panel_text_once(&ip, panel_name) }
        }
        RemoteRequest::FileList { request_id, target_id, folder, offset, .. } => {
            let Some(ip) = target_ip(ctx, target_id) else {
                return RemoteReply::Error {
                    request_id: request_id.clone(),
                    message: format!("目標裝置不存在: {target_id}"),
                };
            };
            if !ALLOWED_FOLDERS.contains(&folder.as_str()) {
                return RemoteReply::Error {
                    request_id: request_id.clone(),
                    message: format!("不支援的資料夾: {folder}"),
                };
            }
            match fetch_remote_file_list(&ip, folder) {
                Ok(all_files) => {
                    let total = all_files.len();
                    let files = paginate_file_list(&all_files, *offset);
                    RemoteReply::FileList { request_id: request_id.clone(), files, total }
                }
                Err(err) => RemoteReply::Error { request_id: request_id.clone(), message: format!("{err:#}") },
            }
        }
        RemoteRequest::FilePull { request_id, target_id, folder, name, offset, .. } => {
            let Some(ip) = target_ip(ctx, target_id) else {
                return RemoteReply::Error {
                    request_id: request_id.clone(),
                    message: format!("目標裝置不存在: {target_id}"),
                };
            };
            if safe_file_path(folder, name).is_none() {
                return RemoteReply::Error {
                    request_id: request_id.clone(),
                    message: format!("不支援的資料夾或檔名: {folder}/{name}"),
                };
            }
            match fetch_file_chunk(&ip, folder, name, *offset) {
                Ok(data) => RemoteReply::FileChunk { request_id: request_id.clone(), data },
                Err(err) => RemoteReply::Error { request_id: request_id.clone(), message: format!("{err:#}") },
            }
        }
        RemoteRequest::FilePush { request_id, target_id, folder, name, offset, data, .. } => {
            let Some(ip) = target_ip(ctx, target_id) else {
                return RemoteReply::Error {
                    request_id: request_id.clone(),
                    message: format!("目標裝置不存在: {target_id}"),
                };
            };
            if safe_file_path(folder, name).is_none() {
                return RemoteReply::Error {
                    request_id: request_id.clone(),
                    message: format!("不支援的資料夾或檔名: {folder}/{name}"),
                };
            }
            match push_file_chunk(&ip, folder, name, *offset, data) {
                Ok(()) => RemoteReply::FilePushAck { request_id: request_id.clone() },
                Err(err) => RemoteReply::Error { request_id: request_id.clone(), message: format!("{err:#}") },
            }
        }
    }
}

fn target_ip(ctx: &SharedContext, target_id: &str) -> Option<String> {
    ctx.lock().unwrap().devices.get(target_id).map(|entry| entry.report.ip.clone())
}

/// 中繼一次 `FileList` 請求：對 `target_id` 那台裝置既有的 `GET /api/files/{folder}`
/// 端點查一次，跟 `fetch_panel_text_once` 一樣透過 `curl` 子行程打 HTTP。
fn fetch_remote_file_list(ip: &str, folder: &str) -> Result<Vec<FileMeta>> {
    let url = format!("http://{ip}:{PORT}/api/files/{folder}");
    let output = Command::new("curl")
        .args(["--silent", "--fail", "--max-time", "10", &url])
        .output()
        .context("執行 curl 失敗")?;
    if !output.status.success() {
        bail!("查詢檔案清單失敗");
    }
    let body = String::from_utf8(output.stdout).context("回應不是合法的 UTF-8")?;
    serde_json::from_str(&body).context("回應格式不對")
}

/// 把完整的檔案清單從 `offset` 開始切一頁：累計的原始位元組數（以檔名長度
/// 估算，`+ 24` 粗抓 `size` 數字跟 JSON 標點的開銷）一旦會超過
/// `FILE_LIST_PAGE_BUDGET` 就切下一頁——用 `>` 而不是 `>=`，且第一筆一定收，
/// 這樣就算單一檔名長到自己就超過預算，也不會回傳空頁面卡住呼叫端的分頁
/// 迴圈（見 `plugins::files::list_remote_files_mqtt`）。
fn paginate_file_list(all: &[FileMeta], offset: usize) -> Vec<FileMeta> {
    let mut page = Vec::new();
    let mut size = 0usize;
    for item in all.iter().skip(offset) {
        let item_size = item.name.len() + 24;
        if !page.is_empty() && size + item_size > FILE_LIST_PAGE_BUDGET {
            break;
        }
        size += item_size;
        page.push(item.clone());
    }
    page
}

/// 中繼一次 `FilePull` 請求：對 `target_id` 那台裝置既有的
/// `GET /api/files/{folder}/{name}` 端點送一個 `Range` 請求，只拿 `offset` 開始
/// 的一個 chunk（見 `plugin::FILE_CHUNK_SIZE`），不用整個檔案讀進記憶體。
/// `actix_files::NamedFile`（`web.rs` 那個端點的實作）本來就支援 `Range`，一個
/// 超出檔案實際範圍的 range 會回傳「從 offset 到真正檔尾」那一段（不是錯誤）——
/// `plugins::files::pull_file_mqtt` 靠事先知道的檔案大小判斷要不要再要下一個
/// chunk，不會真的送出一個完全落在檔案範圍外的請求，所以這裡不需要特別處理
/// 416 那種情況。
fn fetch_file_chunk(ip: &str, folder: &str, name: &str, offset: u64) -> Result<String> {
    let end = offset + FILE_CHUNK_SIZE as u64 - 1;
    let url = format!("http://{ip}:{PORT}/api/files/{folder}/{}", url_encode_filename(name));
    let output = Command::new("curl")
        .args(["--silent", "--fail", "--max-time", "10", "--range", &format!("{offset}-{end}"), &url])
        .output()
        .context("執行 curl 失敗")?;
    if !output.status.success() {
        bail!("讀取遠端檔案失敗: {name}");
    }
    Ok(BASE64.encode(&output.stdout))
}

/// 中繼一次 `FilePush` 請求：把 `data`（base64）解碼後的原始位元組轉送給
/// `target_id` 那台裝置既有的 `POST /api/files/{folder}/{name}?offset=<offset>`
/// 端點，由那個端點負責寫入正確的位置（見 `web.rs` 的 `files_upload`）。
fn push_file_chunk(ip: &str, folder: &str, name: &str, offset: u64, data: &str) -> Result<()> {
    let bytes = BASE64.decode(data.as_bytes()).context("chunk 不是合法的 base64")?;
    let url = format!("http://{ip}:{PORT}/api/files/{folder}/{}?offset={offset}", url_encode_filename(name));
    let output = Command::new("curl")
        .args(["--silent", "--fail", "--max-time", "10", "-X", "POST", "--data-binary", "@-", &url])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(stdin) = child.stdin.take() {
                let mut stdin = stdin;
                let _ = stdin.write_all(&bytes);
            }
            child.wait_with_output()
        })
        .context("執行 curl 失敗")?;
    if !output.status.success() {
        bail!("寫入遠端檔案失敗: {name}");
    }
    Ok(())
}

/// 跟 `remote-output` 的即時鏡射不同，這裡只需要「現在這一刻」的內容一次性
/// 讀一份，不是持續訂閱——沿用既有的 `/api/panel/{name}/stream` SSE 端點，
/// 用短逾時的 `curl -N` 連上去，撈到第一個 `data:` frame（`web.rs` 的
/// `panel_stream` 一訂閱就會先送一次目前快取的內容）就當結果，不用為了這個
/// 一次性用途另外開一個新端點。逾時或格式不對都當作沒有內容（`None`）。
fn fetch_panel_text_once(ip: &str, panel_name: &str) -> Option<String> {
    let url = format!("http://{ip}:9759/api/panel/{panel_name}/stream");
    let output = Command::new("curl").args(["--silent", "-N", "--max-time", "2", &url]).output().ok()?;
    let body = String::from_utf8(output.stdout).ok()?;
    let line = body.lines().find(|line| line.starts_with("data: "))?;
    serde_json::from_str::<String>(&line["data: ".len()..]).ok()
}

/// topic 格式是 `<bridge-id>/<domain>/remote/reply`，`domain` 是這則回覆
/// 「要給誰」（我們發起請求時用的 `source_domain`）。跟 `handle_remote_request`
/// 一樣先過濾掉不是給自己的，再靠解密後的 `request_id` 到
/// `ctx.cross_domain_pending` 查表，把結果送給正在等的呼叫端；查無此表（呼叫
/// 端已經逾時放棄）就直接丟棄。
fn handle_remote_reply(domain: &str, payload: &[u8], ctx: &SharedContext) {
    let (my_domain, pending) = {
        let inner = ctx.lock().unwrap();
        (inner.domain_name.clone(), inner.cross_domain_pending.clone())
    };
    if my_domain.as_deref() != Some(domain) {
        return;
    }
    let Ok(reply) = crypto::open::<RemoteReply>(payload) else { return };
    if let Some(sender) = pending.lock().unwrap().remove(reply.request_id()) {
        let _ = sender.send(reply);
    }
}

/// client 角色定期跟自己的 server 要一份 `/api/global/list`，**整批取代**本機的
/// `global` registry（不是逐筆 upsert）——client 對這份資料完全信任、鏡射自己
/// server 目前回報的內容（client 自己通常沒設定 `domain_name`，`merged_global_view`
/// 裡「本機這個 domain」那段是 no-op，顯示的資料 100% 來自這裡），所以 server 那邊
/// 用 `clear` 讓某個 domain 消失之後，client 下一輪 pull 到的清單也應該跟著變短。
/// 如果逐筆 upsert，只會覆蓋/新增收到的項目，server 端已經沒有的舊資料不會被清掉，
/// 本機就會一直卡著 server 早就不認的舊資料。連不上 server（暫時網路不通）就直接
/// 提早 return、不動本機現有的資料——只有真的成功拿到一份新清單才整批取代。
fn pull_global_from_server(addr: &str, ctx: &SharedContext) {
    let url = format!("http://{addr}:{PORT}/api/global/list");
    let Ok(output) = Command::new("curl").args(["--silent", "--max-time", "5", &url]).output() else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let Ok(body) = String::from_utf8(output.stdout) else { return };
    let Ok(items) = serde_json::from_str::<Vec<GlobalListItem>>(&body) else { return };
    let mut fresh = HashMap::new();
    for item in items {
        let key = global_registry_key(&item.domain, &item.report.id);
        let last_seen =
            Instant::now().checked_sub(Duration::from_secs_f64(item.age_secs.max(0.0))).unwrap_or_else(Instant::now);
        fresh.insert(key, GlobalRegistryEntry { domain: item.domain, report: item.report, last_seen });
    }
    ctx.lock().unwrap().global = fresh;
}

/// `clear` 指令的實作：開一個短命的 MQTT 連線，對 `topic` 發布一個空 payload
/// （retain=true）——這是 MQTT 官方定義的「清掉這個 topic 上 retained 訊息」的
/// 標準做法。用 QoS 1（`AtLeastOnce`）並等到收到 `PubAck` 才斷線，確保這筆清除
/// 訊息真的送達 broker，不是還沒送出去就斷線、變成沒有效果。連線本身出錯（或
/// broker 遲遲沒回應，靠 `keep_alive` 的內建逾時機制讓連線報錯）都會讓
/// `connection.iter()` 提早結束，當作失敗處理。
fn clear_retained_topic(topic: &str) -> Result<()> {
    let client_id = format!("cng5-clear-{}-{}", sysinfo::hostname(), std::process::id());
    let mut opts = MqttOptions::new(client_id, BROKER_HOST, BROKER_PORT);
    opts.set_keep_alive(Duration::from_secs(10));
    let (client, mut connection) = Client::new(opts, 10);
    client.publish(topic, QoS::AtLeastOnce, true, Vec::new()).context("publish 清除訊息失敗")?;

    for notification in connection.iter() {
        match notification {
            Ok(Event::Incoming(Packet::PubAck(_))) => {
                let _ = client.disconnect();
                return Ok(());
            }
            Ok(_) => {}
            Err(err) => bail!("連線失敗: {err}"),
        }
    }
    bail!("連線提早結束，沒有收到 broker 的確認")
}

/// 組一個純文字表格，跟 `DevicePlugin` 的 `render_table` 是同一個寫法（欄寬依
/// 這一欄裡最寬的內容決定，用 `UnicodeWidthStr` 對齊），各 plugin 各自維護一份
/// 精簡版而不是共用，理由見 `plugins::device` 的同名函式。
fn render_table(headers: &[&str], rows: &[[String; 9]]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| UnicodeWidthStr::width(*h)).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(UnicodeWidthStr::width(cell.as_str()));
        }
    }
    let pad = |s: &str, w: usize| format!("{s}{}", " ".repeat(w.saturating_sub(UnicodeWidthStr::width(s))));
    let header_line = headers.iter().zip(&widths).map(|(h, w)| pad(h, *w)).collect::<Vec<_>>().join(" | ");
    let separator = widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("-+-");
    let mut lines = vec![header_line, separator];
    for row in rows {
        lines.push(row.iter().zip(&widths).map(|(c, w)| pad(c, *w)).collect::<Vec<_>>().join(" | "));
    }
    lines.join("\n")
}

impl Plugin for GlobalPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["domain <name>", "bridge <id>", "status", "list", "clear"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "domain" => self.set_domain(args.first().context("domain 需要接一個 domain 名稱")?, out),
            "bridge" => self.set_bridge(args.first().context("bridge 需要接一個 bridge id")?, out),
            "status" => self.status(out),
            "list" => self.list(out),
            "clear" => self.clear(args, out),
            other => bail!("global 不認得指令: {other}"),
        }
    }

    fn panel_text(&self) -> Option<String> {
        Some(self.table_text())
    }

    fn manual_text(&self) -> &'static str {
        MANUAL_TEXT
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
