use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use rumqttc::Client;
use serde::{Deserialize, Serialize};

use crate::activity::ActivityLog;
use crate::output::OutputBuffer;

/// app 版本號，寫死在原始碼裡（不是從 `Cargo.toml`/git tag 自動帶，單純一個
/// 常數）。之後要發新版本時，**只需要改這一行**：`system` plugin 的
/// `version` 指令/panel、`DeviceReport`（因此 `device`/`global` 的清單也一
/// 起）、`/api/version` 都是讀這裡，不用到處改。
pub const APP_VERSION: &str = "1.4.0";

/// 一台裝置目前回報的資訊——不管是這台機器自己（見 `plugins::system` 背景
/// 回報執行緒直接寫入本機 registry），還是透過 `/api/device/register` 收到
/// 其他機器回報的，格式都一樣，這樣 `DevicePlugin` 顯示時不用區分來源。
#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceReport {
    pub id: String,
    pub ip: String,
    /// 舊版（還沒有這個欄位）的機器傳過來的 JSON 不會有 `os` 這個 key，
    /// `#[serde(default)]` 讓這種情況解析成 `"N/A"` 而不是讓整筆（甚至整份
    /// 清單，視收到的是 `/api/device/register` 單筆還是 `/api/device/list`/
    /// `/api/global/list` 陣列而定）因為缺欄位解析失敗被整個丟掉——新增欄位
    /// 不該讓還沒升級的裝置從清單上消失。
    #[serde(default = "default_os")]
    pub os: String,
    /// 跟 `os` 同一套理由：舊版（還沒有 `version` 這個欄位的 build）傳過來的
    /// JSON 缺這個 key 時，解析成 `"N/A"`，不會讓整筆資料解析失敗。
    #[serde(default = "default_version")]
    pub version: String,
    pub tailscale: bool,
    pub mode: String,
    pub device_uptime_secs: u64,
    pub app_uptime_secs: u64,
}

fn default_os() -> String {
    "N/A".to_string()
}

fn default_version() -> String {
    "N/A".to_string()
}

/// `GET /api/device/list` 的一筆回應：`age_secs` 是伺服器收到這筆回報後過了
/// 幾秒，而不是直接帶 `last_seen`（`Instant` 沒辦法序列化，而且不同機器的
/// `Instant` 本來就不能互相比較）。拉這份清單的一方（client 端的
/// `plugins::system::pull_peers`）憑這個秒數重建一個本機的 `Instant`，之後
/// 判斷 alive 的邏輯（`DevicePlugin`）就能跟本機自己的資料一致。
#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceListItem {
    pub report: DeviceReport,
    pub age_secs: f64,
}

/// registry 裡的一筆：上一次收到/寫入的時間，搭配 `DevicePlugin` 顯示時判斷
/// alive。曾經回報過、後來斷線的裝置，`report` 保留最後一次收到的內容不清
/// 掉，只有 alive 會變成 false（見 `DevicePlugin` 的 `ALIVE_TTL`）。
pub struct DeviceEntry {
    pub report: DeviceReport,
    pub last_seen: Instant,
}

/// `global` plugin registry 裡的一筆，多了 `domain`——同一個 `bridge-id` 底下
/// 可能有好幾個 domain，各自的 server 都在回報，`id` 只在同一個 domain 內保證
/// 不重複（不同 domain 剛好取到同名 hostname 也不奇怪），所以 key 要是
/// `"<domain>/<id>"` 而不能只用 `id`，見 `global_registry_key`。
pub struct GlobalRegistryEntry {
    pub domain: String,
    pub report: DeviceReport,
    pub last_seen: Instant,
}

/// `GET /api/global/list` 的一筆回應（可序列化版本），client 端的 `global`
/// plugin 拉回去合併用，跟 `DeviceListItem`/`system::pull_peers` 是同一個套路。
#[derive(Clone, Serialize, Deserialize)]
pub struct GlobalListItem {
    pub domain: String,
    pub report: DeviceReport,
    pub age_secs: f64,
}

pub fn global_registry_key(domain: &str, id: &str) -> String {
    format!("{domain}/{id}")
}

/// `plugins::files` 一個 chunk 送多少原始位元組（base64 編碼、包上 JSON/AEAD
/// 之前）。跨 domain 檔案傳輸是一個 chunk 一次 MQTT 請求/回覆的往返（見
/// `shell::send_cross_domain_request`），不是整個檔案塞進一則訊息——公開
/// broker 對單則訊息大小有限制。
///
/// 這個限制不是憑空猜的：實測過 `broker.emqx.io`，直接發布不同大小的原始
/// payload，10000 bytes 收得到，11000 bytes 以上完全收不到（既不是連線出錯、
/// 也不是 publish() 呼叫本身失敗，就是悄悄不見了——broker 端直接丟棄，沒有
/// 任何錯誤回報）。原本設 16 KiB（base64 編碼後膨脹到約 22 KiB，加密+JSON
/// 包裝前）遠遠超過這個門檻，導致每個 `FilePull`/`FilePush` 回覆都被
/// broker 悄悄丟掉，發起端只能眼睜睜等到逾時——這正是曾經發生過的實際 bug
/// （跨 domain copy 永遠卡在逾時，不管把逾時時間調多長都一樣，因為根本沒有
/// 回覆會抵達）。4 KiB 原始資料 base64 編碼後約 5.5 KiB，加上 JSON/AEAD 的
/// 額外開銷，總共送上 broker 的還是遠低於實測的 10 KiB 門檻，留了接近一倍
/// 的安全餘裕（避免這個未公開文件的限制之後又更嚴格，或不同時間/連線有些
/// 微差異）。代價是大檔案要傳更多輪、速度更慢——使用者已經預期跨 domain
/// 檔案傳輸「可能要切檔慢慢傳」，換取「真的傳得成」比「傳快一點」更重要。
pub const FILE_CHUNK_SIZE: usize = 4 * 1024;

/// 一則 `FileList` 回覆最多塞多少原始位元組（以檔名長度估算，不含 JSON/AEAD
/// 包裝開銷）——資料夾檔案一多，整份清單一次塞進一則 MQTT 回覆一樣會被
/// broker 悄悄丟掉（跟 `FILE_CHUNK_SIZE` 要解決的是同一個門檻問題，只是這裡
/// 撐爆訊息大小的是「檔案數量」而不是「單一檔案內容」）。收到請求那一端
/// （`global.rs` 的 `build_remote_reply`）把完整清單依這個預算切成一頁一頁，
/// 呼叫端（`plugins::files::list_remote_files_mqtt`）用回覆帶的 `total` 判斷
/// 還有沒有下一頁，逐頁把 `offset` 往前推，直到湊滿 `total` 筆。
pub const FILE_LIST_PAGE_BUDGET: usize = 4 * 1024;

/// 一個檔案的基本資訊，`FileList` 回覆裡每個檔案一筆，`plugins::files` 用來
/// 算「這次 copy 總共幾個檔案」；也是 `GET /api/files/{folder}` 的 JSON 回應
/// 形狀，同網域跟跨 domain 兩條路徑共用同一個型別。
#[derive(Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub name: String,
    pub size: u64,
}

/// `shell::send_cross_domain_request` 要送出的內容，不含 `request_id`/
/// `source_domain`——這兩個要等真正發布出去那一刻才由該函式填上（見它的
/// 說明），呼叫端（`shell.rs`/`plugins::remote_output`/`plugins::files`）不用、
/// 也不應該自己決定這兩個值。同時也是 `web.rs` 的 `/api/remote/cross-relay`
/// 端點收 client 中繼請求時的 body 格式。
///
/// `FileList`/`FilePull`/`FilePush` 是 `plugins::files` 的 `copy` 指令跨
/// domain 時用的：`FileList` 查目標裝置某個資料夾有哪些檔案（順便拿到每個
/// 檔案的大小，`plugins::files` 靠這個算「拉到 offset 有沒有超過檔案大小」，
/// 不需要伺服器另外回報「這是不是最後一塊」），`FilePull` 跟目標要某個檔案
/// 某個位移開始的一個 chunk（見 `FILE_CHUNK_SIZE`），`FilePush` 反過來把本機
/// 的一個 chunk 送給目標寫入（是不是最後一塊發送端自己知道——讀本機檔案時
/// 就知道總長度了，不需要告訴接收端，接收端只需要照 `offset` 寫入）。
/// `folder` 收到那一端一定要驗證過在允許清單裡（見 `plugins::files::ALLOWED_FOLDERS`），
/// 不能只因為請求通過了 AEAD 解密就信任內容——加密只保證「這是持有同一把
/// key 的裝置送的」，不代表內容本身沒有問題（例如打錯字的資料夾名稱，或
/// 未來版本間協定不一致）。
///
/// `FileList` 的 `offset` 是「清單裡第幾筆開始」（不是位元組位移，跟
/// `FilePull`/`FilePush` 的 `offset` 意義不同）——資料夾檔案數量多時一頁裝
/// 不下（見 `FILE_LIST_PAGE_BUDGET`），要像 `FilePull` 分 chunk 一樣分頁拉。
#[derive(Clone, Serialize, Deserialize)]
pub enum CrossDomainAsk {
    Exec { target_id: String, line: String },
    Panel { target_id: String, panel_name: String },
    FileList { target_id: String, folder: String, offset: usize },
    FilePull { target_id: String, folder: String, name: String, offset: u64 },
    FilePush { target_id: String, folder: String, name: String, offset: u64, data: String },
}

/// 跨 domain remote 的請求——透過 `global` plugin 既有的 MQTT session 加密
/// 發布到 `<bridge-id>/<target-domain>/remote/request`（見 `crypto::seal`/
/// `open`，這裡的型別是加密前/解密後的內容，不是 wire bytes 本身）。收到的
/// 那一方（`target_domain` 的 server）查自己的 `ctx.devices[target_id]` 找到
/// 對應 ip 之後，`Exec` 轉呼叫既有的 `shell::remote_exec`、`Panel` 則是查一次
/// 那台裝置目前某個 panel 的內容，結果包成 `RemoteReply` 加密回覆到
/// `<bridge-id>/<source_domain>/remote/reply`。
///
/// `request_id` 是發起端產生的關聯 id（不需要密碼學等級的隨機性，只要在
/// 「這個 process 目前還沒處理完的請求」裡不重複即可），讓 `RemoteReply` 送
/// 回來的時候，發起端能配對到當初是哪一個呼叫在等——同一時間可能有好幾個
/// 跨 domain 請求在飛（例如 `remote` 的指令轉發跟 `remote-output` 的輪詢同時
/// 進行），不能只憑 topic 判斷。
#[derive(Clone, Serialize, Deserialize)]
pub enum RemoteRequest {
    Exec { request_id: String, source_domain: String, target_id: String, line: String },
    Panel { request_id: String, source_domain: String, target_id: String, panel_name: String },
    FileList { request_id: String, source_domain: String, target_id: String, folder: String, offset: usize },
    FilePull { request_id: String, source_domain: String, target_id: String, folder: String, name: String, offset: u64 },
    FilePush { request_id: String, source_domain: String, target_id: String, folder: String, name: String, offset: u64, data: String },
}

impl RemoteRequest {
    pub fn source_domain(&self) -> &str {
        match self {
            RemoteRequest::Exec { source_domain, .. }
            | RemoteRequest::Panel { source_domain, .. }
            | RemoteRequest::FileList { source_domain, .. }
            | RemoteRequest::FilePull { source_domain, .. }
            | RemoteRequest::FilePush { source_domain, .. } => source_domain,
        }
    }
}

/// `RemoteRequest` 的回覆。`Error` 涵蓋所有處理失敗的情況（`target_id` 在
/// 目標 domain 裡查不到、轉發本身失敗……），讓發起端能看到明確的錯誤訊息，
/// 而不是讓請求默默逾時、搞不清楚是網路問題還是目標真的不存在。
#[derive(Clone, Serialize, Deserialize)]
pub enum RemoteReply {
    Exec { request_id: String, prompt: String, error: Option<String> },
    Panel { request_id: String, text: Option<String> },
    Error { request_id: String, message: String },
    FileList { request_id: String, files: Vec<FileMeta>, total: usize },
    FileChunk { request_id: String, data: String },
    FilePushAck { request_id: String },
}

impl RemoteReply {
    pub fn request_id(&self) -> &str {
        match self {
            RemoteReply::Exec { request_id, .. }
            | RemoteReply::Panel { request_id, .. }
            | RemoteReply::Error { request_id, .. }
            | RemoteReply::FileList { request_id, .. }
            | RemoteReply::FileChunk { request_id, .. }
            | RemoteReply::FilePushAck { request_id, .. } => request_id,
        }
    }
}

/// 各 plugin 之後要共用的資源放這裡。
#[derive(Default)]
pub struct ContextInner {
    /// `system` plugin（不管是本機自己、還是透過 web 收到其他機器的回報，見
    /// `web::device_register`）寫入，`device` plugin 讀出來顯示。
    pub devices: HashMap<String, DeviceEntry>,
    /// `system` plugin 的 `server <ip>` 設定的目標。放在這裡（而不是
    /// `SystemPlugin` 自己的私有欄位）是因為 `qr` plugin 也需要讀它組「server
    /// 的 web UI 網址」QR code，兩個 plugin 都要碰得到，符合 `ContextInner`
    /// 本來就是「各 plugin 共用資源」的定位。
    pub server_addr: Option<String>,
    /// 這台機器目前是不是 `system` plugin 的 server mode（`SystemPlugin::set_mode`
    /// 寫入）。`global` plugin 用這個決定要不要真的連 MQTT——`domain`/`bridge`
    /// 設定只有 server 才有意義，client 角色改成跟自己的 server 要現成的
    /// `/api/global/list`（見 `merged_global_view`）。
    pub is_server: bool,
    /// `global` plugin 的 `domain <name>` 設定。放在這裡（而不是 `GlobalPlugin`
    /// 私有欄位）是因為 `web::global_list` 組回應時也需要知道「這台 server 自己
    /// 的 domain 叫什麼」，才能把 `devices` 標成正確的 domain 一併回傳。
    pub domain_name: Option<String>,
    /// 透過 MQTT 從其他 domain 收到的裝置資料（server 角色才會真的收到），
    /// key 用 `global_registry_key`。client 角色則是從自己的 server 拉
    /// `/api/global/list` 填進來，跟 server 角色收到 MQTT 訊息填進來的邏輯
    /// 分開但存進同一個地方，`global` plugin 顯示時不用區分來源。
    pub global: HashMap<String, GlobalRegistryEntry>,
    /// `remote` plugin 的 `connect <id>` 設定的目前連線目標 `(id, ip)`。放在這裡
    /// （而不是 `RemotePlugin` 私有欄位）是因為 `RemoteOutputPlugin` 的背景執行緒
    /// 也需要知道「現在該對誰開 SSE 串流」，兩個 plugin 都要碰得到。
    pub remote_target: Option<(String, String)>,
    /// `remote` plugin 的 `connect <domain>/<id>` 設定的目前跨 domain 連線目標
    /// `(domain, id)`，跟 `remote_target`（同網域用）並列、互斥——`Mode::Remote`
    /// 連線時只會設定其中一個。`RemoteOutputPlugin` 兩個都檢查，決定要用既有
    /// 的 SSE 訂閱、還是輪詢式的加密請求/回應（見 `plugins::remote_output`）。
    pub cross_domain_remote_target: Option<(String, String)>,
    /// 目前活著的 MQTT client 跟它用的 bridge-id（`plugins::global::run_mqtt_session`
    /// 連上時設定、session 結束前清成 `None`）。兩者綁在同一個 `Option` 裡一起
    /// 設定/清除，避免各自存一份、讀取時兩邊剛好不同步（例如 client 已經是新的
    /// 一輪連線、bridge-id 還是舊的）。`shell.rs`/`web.rs` 要發跨 domain請求時
    /// 借用這個直接 publish（topic 需要 bridge-id 組），不用自己另外開一條
    /// MQTT 連線——本機同一時間只需要（也只應該有）一條 MQTT 連線。
    pub mqtt_client: Arc<Mutex<Option<(String, Client)>>>,
    /// 跨 domain 請求的關聯表：`request_id -> 一次性 channel`。發出請求的一方
    /// （`shell.rs`/`web.rs`）先在這裡登記一個 channel 再 publish，`global.rs`
    /// 的 MQTT session 收到對應的 `RemoteReply` 時查表把解密後的內容送過去；
    /// 找不到表示呼叫端已經逾時放棄，直接丟棄即可。
    pub cross_domain_pending: Arc<Mutex<HashMap<String, mpsc::Sender<RemoteReply>>>>,
    /// 網路活動流水帳（mqtt/http/外部服務呼叫），給 `activities` plugin 顯示，
    /// 見 `ContextInner::log_activity`、`src/activity.rs`。
    pub activities: Arc<ActivityLog>,
}

impl ContextInner {
    /// 記一筆網路活動。`kind` 用固定的幾種分類字串（`"mqtt-out"`/`"mqtt-in"`/
    /// `"http-out"`/`"http-in"`/`"external"`），`detail` 是給人看的一行摘要。
    pub fn log_activity(&self, kind: &'static str, detail: impl Into<String>) {
        self.activities.push(kind, detail);
    }
}

/// `global` plugin 的 panel/`list` 指令跟 `web::global_list` 共用的內容：本機
/// 自己這個 domain 的裝置（來自 `devices`，標上 `domain_name`）+ 透過 MQTT/HTTP
/// 收到的其他 domain 的裝置（`global`）。本機這個 domain 的部分故意不從
/// `global`（它也收得到自己發布出去、又訂閱回來的 echo）取用，而是直接讀
/// `devices` 現算——這樣不用等一輪 MQTT 往返、也不會因為連線暫時斷線就顯示
/// 過期資料，所以顯示/回應時要排除 `global` 裡 domain 等於自己的部分，避免同一台
/// 機器出現兩筆（一筆現算的、一筆較舊的 echo）。`domain_name` 沒設定（例如
/// client 角色，或 server 還沒下 `domain` 指令）就不會有本機這一段。
pub fn merged_global_view(inner: &ContextInner) -> Vec<GlobalListItem> {
    let mut items: Vec<GlobalListItem> = inner
        .global
        .values()
        .filter(|entry| Some(&entry.domain) != inner.domain_name.as_ref())
        .map(|entry| GlobalListItem {
            domain: entry.domain.clone(),
            report: entry.report.clone(),
            age_secs: entry.last_seen.elapsed().as_secs_f64(),
        })
        .collect();
    if let Some(domain) = &inner.domain_name {
        items.extend(inner.devices.values().map(|entry| GlobalListItem {
            domain: domain.clone(),
            report: entry.report.clone(),
            age_secs: entry.last_seen.elapsed().as_secs_f64(),
        }));
    }
    items
}

pub type SharedContext = Arc<Mutex<ContextInner>>;

/// 要求 Send 是因為互動模式下 `?` 按鍵的 callback（rustyline 的
/// `ConditionalEventHandler`）需要 Send + Sync 才能綁定。
pub trait Plugin: Send {
    /// 給 `help` 指令顯示用，每一項是一行「指令 <參數說明>」。
    fn commands(&self) -> &'static [&'static str];
    /// `out` 是輸出的地方，不要直接 `println!`——CLI 跟 GUI 模式顯示的方式不一樣。
    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()>;
    /// 這個 plugin 的 panel 要顯示的內容；預設 `None`（空殼，只有邊框標題）。
    /// `output` panel 的內容是即時捲動紀錄，由 GUI 直接處理，不走這個。
    fn panel_text(&self) -> Option<String> {
        None
    }

    /// `manual` 指令印出來的完整說明：用途、範例、注意事項，比 `commands()` 那種
    /// 一行式的用法簽名詳細。跟 `help`/`history` 一樣是每個 plugin mode 底下都有
    /// 的通用指令（見 `shell::Shell::execute_line`），不是各 plugin 自己在
    /// `dispatch` 裡處理，這樣才不用每個 plugin 都重複寫一次「印出 manual 文字」
    /// 這種樣板邏輯。
    fn manual_text(&self) -> &'static str {
        "這個 plugin 還沒有寫 manual，可以用 help 看指令清單。\n"
    }

    /// 把 `Box<dyn Plugin>` 向下轉型回具體型別，讓外部（目前只有 GUI 的
    /// notepad 編輯功能，見 `gui.rs` 的 `with_notepad`）能直接操作某個 plugin
    /// 的內部狀態，而不是只能透過 `dispatch` 送指令字串——逐字元編輯這種高
    /// 頻率、內容含任意字元（含空白/引號）的操作，透過 `execute_line`/
    /// `shell_words` 指令解析既麻煩也沒必要。這是 Rust trait object 向下轉型
    /// 的標準寫法，沒辦法只在這裡寫一次預設實作套用到所有型別，每個 plugin
    /// 都要各自實作成 `{ self }`。
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 舊版（還沒有 `os`/`version` 欄位的 build）傳過來的 `DeviceReport` JSON
    /// 不會有這兩個 key；`#[serde(default = "default_os"/"default_version")]`
    /// 應該讓它們解析成 `"N/A"`，而不是讓整筆資料解析失敗（見欄位上方的說明）。
    #[test]
    fn device_report_missing_os_and_version_defaults_to_na() {
        let json = r#"{
            "id": "old-machine",
            "ip": "10.0.0.5",
            "tailscale": false,
            "mode": "standalone",
            "device_uptime_secs": 100,
            "app_uptime_secs": 10
        }"#;
        let report: DeviceReport = serde_json::from_str(json).expect("缺少 os/version 欄位不該解析失敗");
        assert_eq!(report.os, "N/A");
        assert_eq!(report.version, "N/A");
        assert_eq!(report.id, "old-machine");
    }

    /// 新版（有帶 `os`/`version`）的 JSON 應該正常保留原值，確認
    /// `#[serde(default)]` 只在欄位缺席時才套用預設值，不會蓋掉正常收到的內容。
    #[test]
    fn device_report_with_os_and_version_keeps_values() {
        let json = r#"{
            "id": "new-machine",
            "ip": "10.0.0.6",
            "os": "linux",
            "version": "1.3.0",
            "tailscale": true,
            "mode": "server",
            "device_uptime_secs": 200,
            "app_uptime_secs": 20
        }"#;
        let report: DeviceReport = serde_json::from_str(json).unwrap();
        assert_eq!(report.os, "linux");
        assert_eq!(report.version, "1.3.0");
    }
}
