use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use serde_json::Value;

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

/// utc offset 多久重新抓一次——理論上一個地點的 offset 只有跨過 DST 切換
/// 那一刻才會變，但沒必要另外做一套「算下次切換時間」的邏輯，跟 `weather`
/// 用同一種「固定間隔重抓、過期背景刷新」的作法比較簡單一致。
const CACHE_TTL: Duration = Duration::from_secs(300);

const MANUAL_TEXT: &str = "\
worldclock：顯示 add 過的城市目前的當地時間（HH:MM:SS + UTC 偏移）。

範例：
  add Tokyo      加一個城市，之後 show/panel 都會列出來
  add New York   城市名稱含空白直接打，不用加引號
  remove Tokyo   移除一個城市
  show           顯示清單

透過 Open-Meteo 的地理編碼 API 把城市名換成座標，再用座標查目前的 UTC 偏移
（`utc_offset_seconds`），本地時間 = 現在的 UTC 時間 + 該地點的偏移，不是用
本機系統時區換算——查不到座標/沒有網路時該地點會顯示「抓取中」或錯誤訊息。
資料每 300 秒（CACHE_TTL）重新抓一次，跟 weather plugin 是同一套快取邏輯。

中文地名要打完整名稱（例如「台北市」，不能只打「台北」），且有些地名
Open-Meteo 的中文資料庫查不到（例如「清邁」，其資料庫裡存的是簡體「清迈」），
查不到就改用英文名稱（例如 Chiang Mai）。
";

#[derive(Clone)]
struct CityOffset {
    /// 地理編碼查到的座標，`text()`/CLI 不需要，`snapshot()` 給 webui 的世界
    /// 地圖畫城市座標點用（見 `WorldClockCityJson`）。
    lat: f64,
    lon: f64,
    /// IANA 時區名稱（例如 `Asia/Tokyo`），純粹顯示用。
    timezone: String,
    /// 該地點目前跟 UTC 差多少秒，已經含 DST——`fetch` 直接讀 Open-Meteo
    /// forecast API 回應裡的 `utc_offset_seconds`，不用自己維護時區規則表。
    offset_secs: i64,
}

/// `GET /api/worldclock/list` 的一筆回應（見 `WorldClockPlugin::snapshot`）：
/// 只給世界地圖畫點用的座標跟 `offset_secs`，前端自己每秒用 `offset_secs`
/// 算當地時間、持續跳動，不需要每秒重打這條 API；不帶 `timezone` 名稱——
/// 地圖上只標城市名稱跟當地時間，不用像 `text()` 那樣額外顯示時區資訊。
#[derive(Clone, serde::Serialize)]
pub(crate) struct WorldClockCityJson {
    city: String,
    lat: f64,
    lon: f64,
    offset_secs: i64,
}

struct CacheEntry {
    fetched_at: Instant,
    /// 抓失敗（地點查無資料、網路異常）存 `Err`，帶一句給人看的訊息。
    result: Result<CityOffset, String>,
}

pub struct WorldClockPlugin {
    #[allow(dead_code)]
    ctx: SharedContext,
    /// `add`/`remove` 維護的城市清單，依加入順序排列。
    cities: Vec<String>,
    /// 每個城市最後一次抓到的結果，背景執行緒抓完就寫進來，`text()` 只負責
    /// 讀，不含任何網路呼叫，不會卡住持有 `Shell` 鎖的那個執行緒。
    cache: Arc<Mutex<HashMap<String, CacheEntry>>>,
    /// 目前正在背景抓的城市集合，避免同一個城市快取一過期，短時間內被連續
    /// 呼叫（例如 GUI 每次畫面重繪）就開出一堆重複的 curl 行程。
    pending: Arc<Mutex<HashSet<String>>>,
}

impl WorldClockPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx, cities: Vec::new(), cache: Arc::new(Mutex::new(HashMap::new())), pending: Arc::new(Mutex::new(HashSet::new())) }
    }

    fn add(&mut self, args: &[String], out: &OutputBuffer) -> Result<()> {
        let city = args.join(" ");
        if city.is_empty() {
            bail!("add 需要接城市名稱");
        }
        if self.cities.iter().any(|c| c == &city) {
            out.push(&format!("worldclock 已經有 {city} 了\n"));
            return Ok(());
        }
        self.cities.push(city.clone());
        out.push(&format!("worldclock 新增 {city}\n"));
        Ok(())
    }

    fn remove(&mut self, args: &[String], out: &OutputBuffer) -> Result<()> {
        let city = args.join(" ");
        let before = self.cities.len();
        self.cities.retain(|c| c != &city);
        if self.cities.len() == before {
            out.push(&format!("worldclock 沒有 {city}\n"));
        } else {
            self.cache.lock().unwrap().remove(&city);
            out.push(&format!("worldclock 移除 {city}\n"));
        }
        Ok(())
    }

    fn show(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push(&format!("{}\n", self.text()));
        Ok(())
    }

    /// `show`/`panel_text()` 共用的內容：每個 `add` 過的城市一行。
    fn text(&self) -> String {
        if self.cities.is_empty() {
            return "(還沒有用 add 加過任何城市)".to_string();
        }
        self.cities.iter().map(|city| format!("{city}: {}", self.display(city))).collect::<Vec<_>>().join("\n")
    }

    /// 只讀快取，不做任何網路呼叫：回傳目前的抓取結果（成功/失敗/還沒抓過），
    /// 同時判斷是否過期該重抓；真正的抓取一律丟給 `spawn_refresh`。`display`/
    /// `snapshot` 共用這個，各自決定要把結果轉成什麼形式（純文字 vs JSON）。
    fn lookup(&self, city: &str) -> Option<Result<CityOffset, String>> {
        let cached = self.cache.lock().unwrap().get(city).map(|entry| (entry.fetched_at.elapsed(), entry.result.clone()));
        let stale = match &cached {
            None => true,
            Some((age, _)) => *age >= CACHE_TTL,
        };
        if stale {
            self.spawn_refresh(city);
        }
        cached.map(|(_, result)| result)
    }

    /// `text()` 用的一行內容：有資料就依 `offset_secs` 算出現在的 `HH:MM:SS`，
    /// 帶上時區名稱／UTC 偏移；抓失敗顯示錯誤訊息；還沒抓過顯示「抓取中」。
    fn display(&self, city: &str) -> String {
        match self.lookup(city) {
            Some(Ok(offset)) => {
                format!("{} {}", local_hms(offset.offset_secs), format_utc_offset(&offset.timezone, offset.offset_secs))
            }
            Some(Err(message)) => message,
            None => "抓取中...".to_string(),
        }
    }

    /// 給 webui 的世界地圖用的結構化清單（`web.rs` 的 `/api/worldclock/list`
    /// 呼叫這個轉成 JSON）：跳過還在抓取中／抓失敗的城市——地圖上本來就沒有
    /// 座標可以畫點，跟 `text()` 用一句狀態訊息占著那一行不一樣，這裡直接
    /// 略過，等下一輪 poll 抓到座標後自然會出現。
    pub(crate) fn snapshot(&self) -> Vec<WorldClockCityJson> {
        self.cities
            .iter()
            .filter_map(|city| match self.lookup(city) {
                Some(Ok(offset)) => {
                    Some(WorldClockCityJson { city: city.clone(), lat: offset.lat, lon: offset.lon, offset_secs: offset.offset_secs })
                }
                _ => None,
            })
            .collect()
    }

    /// 開一個背景執行緒去查 `city` 目前的 UTC 偏移，抓完寫回 `cache`；如果這個
    /// 城市已經有一個背景執行緒在抓了就不重複開。
    fn spawn_refresh(&self, city: &str) {
        let mut pending = self.pending.lock().unwrap();
        if !pending.insert(city.to_string()) {
            return; // 已經有背景執行緒在抓這個城市了。
        }
        drop(pending);

        let city = city.to_string();
        let cache = self.cache.clone();
        let pending = self.pending.clone();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            ctx.lock().unwrap().log_activity("external", format!("GET Open-Meteo timezone for {city}"));
            let result = Self::fetch(&city).ok_or_else(|| "無法取得時區資訊（網路異常，或城市查無資料）".to_string());
            cache.lock().unwrap().insert(city.clone(), CacheEntry { fetched_at: Instant::now(), result });
            pending.lock().unwrap().remove(&city);
        });
    }

    /// 用 `curl` 打一個網址、把回應內容當 JSON 解析，網路/curl 不存在/逾時
    /// （5 秒）/回應不是合法 JSON 都回傳 `None`，不 panic。跟 `weather` plugin
    /// 同一套小工具，這裡沒有共用模組（各 plugin 各自的網路呼叫細節不同：
    /// header、逾時策略之後可能分岔），先各留一份。
    fn curl_json(url: &str) -> Option<Value> {
        let output = Command::new("curl").args(["--silent", "--max-time", "5", url]).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// 手動 percent-encode（逐 byte 處理，UTF-8 的中文字元自然也會被正確
    /// 編碼）：城市名可能是中文（例如「台北市」），直接塞進網址、只把空白換成
    /// `+`（像 weather plugin 原本那樣）打不到中文地名，會直接查無結果。
    fn url_encode(s: &str) -> String {
        let mut out = String::new();
        for b in s.as_bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    /// 先用 Open-Meteo 的地理編碼 API 把城市名換成座標，再拿座標查
    /// `utc_offset_seconds`（forecast API 只要求指定 `current_weather=true`
    /// 這種最輕量的區塊即可回應這個欄位，不需要真的要天氣資料）。任何一步
    /// 查不到都回傳 `None`，只在 `spawn_refresh` 開的背景執行緒裡呼叫，不會
    /// 卡住持有 `Shell` 鎖的執行緒。`language=zh` 讓中文地名（例如「台北市」／
    /// 「西雅圖」）查得到——沒有這個參數 Open-Meteo 的地理編碼只認拼音/英文
    /// 名稱，中文輸入會直接查無結果；不影響本來就打英文名稱的查詢。
    fn fetch(city: &str) -> Option<CityOffset> {
        let name = Self::url_encode(city);
        let geo_url = format!("https://geocoding-api.open-meteo.com/v1/search?name={name}&count=1&language=zh");
        let geo = Self::curl_json(&geo_url)?;
        let result = geo.get("results")?.as_array()?.first()?;
        let lat = result.get("latitude")?.as_f64()?;
        let lon = result.get("longitude")?.as_f64()?;
        let forecast_url =
            format!("https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}&current_weather=true&timezone=auto");
        let forecast = Self::curl_json(&forecast_url)?;
        let offset_secs = forecast.get("utc_offset_seconds")?.as_i64()?;
        let timezone = forecast.get("timezone")?.as_str()?.to_string();
        Some(CityOffset { lat, lon, timezone, offset_secs })
    }
}

/// 現在的 UTC 時間加上 `offset_secs`，換算成該地點當地的 `HH:MM:SS`。不透過
/// 本機系統時區（`sysinfo::local_hms` 查的是這台機器自己的時區，跟 `add` 的
/// 城市無關），offset 直接來自 Open-Meteo，已經含當地的 DST 規則。
fn local_hms(offset_secs: i64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let secs_of_day = (now + offset_secs).rem_euclid(86400);
    format!("{:02}:{:02}:{:02}", secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60)
}

/// `"(Asia/Tokyo, UTC+9)"` 這種顯示用字串；分鐘不是 0 時（例如印度 UTC+5:30）
/// 才帶出來，大多數地點只需要看到整數小時。
fn format_utc_offset(timezone: &str, offset_secs: i64) -> String {
    let sign = if offset_secs < 0 { "-" } else { "+" };
    let abs = offset_secs.unsigned_abs();
    let hours = abs / 3600;
    let minutes = (abs % 3600) / 60;
    if minutes == 0 {
        format!("({timezone}, UTC{sign}{hours})")
    } else {
        format!("({timezone}, UTC{sign}{hours}:{minutes:02})")
    }
}

impl Plugin for WorldClockPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["add <city>", "remove <city>", "show"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "add" => self.add(args, out),
            "remove" => self.remove(args, out),
            "show" => self.show(out),
            other => bail!("worldclock 不認得指令: {other}"),
        }
    }

    fn panel_text(&self) -> Option<String> {
        Some(self.text())
    }

    fn manual_text(&self) -> &'static str {
        MANUAL_TEXT
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_plugin() -> WorldClockPlugin {
        WorldClockPlugin::new(Arc::new(Mutex::new(crate::plugin::ContextInner::default())))
    }

    #[test]
    fn add_then_remove_updates_city_list_and_messages() {
        let mut plugin = new_plugin();
        let out = OutputBuffer::new();
        plugin.add(&["Tokyo".to_string()], &out).unwrap();
        assert_eq!(plugin.cities, vec!["Tokyo".to_string()]);
        plugin.remove(&["Tokyo".to_string()], &out).unwrap();
        assert!(plugin.cities.is_empty());
        let lines = out.lines_from(0);
        assert!(lines[0].contains("新增 Tokyo"));
        assert!(lines[1].contains("移除 Tokyo"));
    }

    #[test]
    fn add_joins_multi_word_city_names() {
        let mut plugin = new_plugin();
        let out = OutputBuffer::new();
        plugin.add(&["New".to_string(), "York".to_string()], &out).unwrap();
        assert_eq!(plugin.cities, vec!["New York".to_string()]);
    }

    #[test]
    fn add_without_city_name_errors() {
        let mut plugin = new_plugin();
        let out = OutputBuffer::new();
        assert!(plugin.add(&[], &out).is_err());
    }

    #[test]
    fn add_same_city_twice_does_not_duplicate() {
        let mut plugin = new_plugin();
        let out = OutputBuffer::new();
        plugin.add(&["Tokyo".to_string()], &out).unwrap();
        plugin.add(&["Tokyo".to_string()], &out).unwrap();
        assert_eq!(plugin.cities.len(), 1);
    }

    #[test]
    fn remove_unknown_city_does_not_error() {
        let mut plugin = new_plugin();
        let out = OutputBuffer::new();
        assert!(plugin.remove(&["Nowhere".to_string()], &out).is_ok());
    }

    #[test]
    fn text_without_any_city_shows_placeholder() {
        let plugin = new_plugin();
        assert_eq!(plugin.text(), "(還沒有用 add 加過任何城市)");
    }

    #[test]
    fn dispatch_unknown_command_errors() {
        let mut plugin = new_plugin();
        let out = OutputBuffer::new();
        let err = plugin.dispatch("bogus", &[], &out).unwrap_err();
        assert!(err.to_string().contains("worldclock 不認得指令"));
    }

    #[test]
    fn local_hms_wraps_across_midnight() {
        // 86399 秒當天最後一秒，加 2 秒偏移應該跨到次日 00:00:01。
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let secs_of_day_now = now.rem_euclid(86400);
        let offset_to_wrap = 86400 - secs_of_day_now + 1;
        let hms = local_hms(offset_to_wrap);
        assert_eq!(hms, "00:00:01");
    }

    #[test]
    fn format_utc_offset_hides_minutes_when_whole_hour() {
        assert_eq!(format_utc_offset("Asia/Tokyo", 9 * 3600), "(Asia/Tokyo, UTC+9)");
    }

    #[test]
    fn format_utc_offset_shows_minutes_and_negative_sign() {
        assert_eq!(format_utc_offset("America/New_York", -5 * 3600), "(America/New_York, UTC-5)");
        assert_eq!(format_utc_offset("Asia/Kolkata", 5 * 3600 + 30 * 60), "(Asia/Kolkata, UTC+5:30)");
    }
}
