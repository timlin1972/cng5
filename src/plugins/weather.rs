use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

/// 把 `s` 用空白補到 `width` 個「顯示寬度」——用 `UnicodeWidthStr` 而不是直接數
/// `chars().count()`，是因為中文字在等寬字型（終端機、web panel 的 `<pre>`）裡
/// 佔兩格，直接數字元數會對不齊。
fn pad(s: &str, width: usize) -> String {
    let extra = width.saturating_sub(UnicodeWidthStr::width(s));
    format!("{s}{}", " ".repeat(extra))
}

/// 組一個純文字表格（表頭 + 分隔線 + 每一列），欄寬依這一欄裡最寬的內容決定。
/// 每個儲存格可以是多行（`Vec<String>`，例如溫度跟降雨機率各佔一行，欄位才不用
/// 留給「溫度(降雨機率)」這種寫法的寬度），同一列裡的儲存格行數不同時，矮的會
/// 自動補空白行，讓那一列印出來高度一致。列跟列之間空一行，界線清楚一點。
/// GUI/CLI/web 三邊顯示天氣用的都是同一份等寬字型純文字，沒有 HTML `<table>`
/// 可用，所以「表格」在這裡就是對齊好的純文字。
fn render_table(headers: &[&str], rows: &[Vec<Vec<String>>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| UnicodeWidthStr::width(*h)).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            for line in cell {
                *width = (*width).max(UnicodeWidthStr::width(line.as_str()));
            }
        }
    }
    let header_line = headers.iter().zip(&widths).map(|(h, w)| pad(h, *w)).collect::<Vec<_>>().join(" | ");
    let separator = widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("-+-");
    let mut lines = vec![header_line, separator];
    for (i, row) in rows.iter().enumerate() {
        let height = row.iter().map(|cell| cell.len()).max().unwrap_or(1).max(1);
        for line_idx in 0..height {
            let rendered: Vec<String> = row
                .iter()
                .zip(&widths)
                .map(|(cell, w)| pad(cell.get(line_idx).map(String::as_str).unwrap_or(""), *w))
                .collect();
            lines.push(rendered.join(" | "));
        }
        if i + 1 < rows.len() {
            lines.push(String::new());
        }
    }
    lines.join("\n")
}

/// 天氣資訊多久重新抓一次。過期後不是在呼叫端（`show`/`panel_text()`，可能是
/// GUI 畫面、web 的 ticker，也可能是 CLI）當場去打 Open-Meteo，而是丟給背景
/// 執行緒抓（見 `spawn_refresh`），呼叫端先拿現有資料（或「抓取中」字樣），
/// 不會被網路卡住——跟 `SystemPlugin` 每次都直接查 `tailscale`（本機、夠快）
/// 不一樣，天氣是真的網路請求，可能要好幾秒，不能讓大家等它，等於是拿著
/// `Shell` 的鎖去等外部網站回應。
const CACHE_TTL: Duration = Duration::from_secs(300);

/// `manual` 指令的說明。
const MANUAL_TEXT: &str = "\
weather：抓天氣資料，用純文字表格顯示現在/今天剩下時段/未來幾天。台灣的
縣市＋行政區（例如「新北市新莊區」）走中央氣象署開放資料平台（CWA），
其他地點走 Open-Meteo。

範例：
  show               顯示表格（列出 add 加過的城市）
  add Tokyo          加一個城市，之後 show/panel 都會列出來
  add 新北市新莊區    台灣地點要打完整的縣市＋行政區
  remove Tokyo        移除一個城市

CWA 那條路徑需要一個 cwa-api-key 檔案（放在跑這個程式的當前目錄，內容是
https://opendata.cwa.gov.tw/ 註冊會員申請到的授權碼，不進版控），沒有這個
檔案的話那個地點會一直顯示「無法取得天氣資訊」。

資料每 300 秒（CACHE_TTL）快取一次，過期後背景執行緒重新抓，show/panel 顯示的
都是目前的快取值（或「抓取中」），不會讓你等網路回應卡住畫面。
";

/// CWA API 授權碼檔案路徑（見 `.gitignore` 的 `/cwa-api-key`），跟
/// `crypto.rs` 的 `KEY_FILE`／`remote-key` 同一個做法：純文字放一份、
/// 不進版控，去 <https://opendata.cwa.gov.tw/> 註冊會員申請。讀不到就讓
/// `fetch_cwa` 直接回 `None`，跟其他步驟查不到值時的處理方式一致，不特別
/// 報錯。
const CWA_KEY_FILE: &str = "cwa-api-key";

/// 每個縣市對應的 CWA 逐 3 小時天氣預報 dataset id（見 `fetch_cwa` 的
/// 說明）。`add` 打的字串如果是這裡任何一個縣市名開頭（用「台」正規化成
/// 「臺」比對，`台北市`/`臺北市` 兩種寫法都認得），就不查 Open-Meteo，改走
/// CWA——`match_taiwan_location` 負責判斷。
const CWA_COUNTIES: &[(&str, &str)] = &[
    ("宜蘭縣", "F-D0047-001"),
    ("桃園市", "F-D0047-005"),
    ("新竹縣", "F-D0047-009"),
    ("苗栗縣", "F-D0047-013"),
    ("彰化縣", "F-D0047-017"),
    ("南投縣", "F-D0047-021"),
    ("雲林縣", "F-D0047-025"),
    ("嘉義縣", "F-D0047-029"),
    ("屏東縣", "F-D0047-033"),
    ("臺東縣", "F-D0047-037"),
    ("花蓮縣", "F-D0047-041"),
    ("澎湖縣", "F-D0047-045"),
    ("基隆市", "F-D0047-049"),
    ("新竹市", "F-D0047-053"),
    ("嘉義市", "F-D0047-057"),
    ("臺北市", "F-D0047-061"),
    ("高雄市", "F-D0047-065"),
    ("新北市", "F-D0047-069"),
    ("臺中市", "F-D0047-073"),
    ("臺南市", "F-D0047-077"),
    ("連江縣", "F-D0047-081"),
    ("金門縣", "F-D0047-085"),
];

/// 一欄的內容種類：`now`／今天剩下的時段／未來的日期，各自需要的數值不一樣
/// （`now`／`hour` 是單一氣溫，`day` 是氣溫範圍），拆成 enum 而不是塞進共用的
/// `Vec<String>`，是因為 `snapshot()`（給 tablet 前端畫圖示用）需要保留數字
/// 跟分類（`WeatherColumn::slug`），不能只有 `cells()` 拼好的顯示字串。
#[derive(Clone)]
enum WeatherColumnKind {
    /// `local_time`：`"HH:MM"`，該地點當地的現在時間（`current.time`，
    /// `timezone=auto` 換算過），不同地點時區不同，所以放在欄位內容裡而不是
    /// 表頭（表頭 `"now"` 是所有地點共用的）。
    Now { temp_c: i32, feels_like_c: i32, humidity: i32, local_time: String },
    Hour { temp_c: i32 },
    /// `is_today`：`weather[]` 第一筆本來就是今天，`snapshot()` 的 `days`
    /// （給前端畫「未來預報」用）要排除這一筆——今天已經有 `now`／`today`
    /// （今天剩下的時段）可以看了，不需要在「未來」那排重複列一次。`text()`
    /// 的純文字表格不受影響，兩者都照樣列出來（跟改之前的行為一致）。
    Day { min_c: i32, max_c: i32, is_today: bool },
}

/// 一欄天氣資料：`header` 是表格表頭（"now"／"15:00"／"7/18"…），`slug` 是給
/// `snapshot()` 用的圖示/背景分類鍵（例如 `"rain"`），`desc` 是對應的英文描述
/// （跟改之前的 `weather_text()` 回傳值一樣，維持 `text()` 純文字輸出不變）。
#[derive(Clone)]
struct WeatherColumn {
    header: String,
    slug: &'static str,
    desc: &'static str,
    chance_of_rain: u32,
    kind: WeatherColumnKind,
}

impl WeatherColumn {
    /// `text()`/CLI/TUI 用的純文字表格儲存格：跟改之前的 `cell()` 輸出基本上
    /// 一樣（描述/氣溫/降雨機率各一行），只是資料來源從原本三個各自獨立的
    /// `(String, Vec<String>)` 改成從這個結構算出來。`Now` 多補一行當地時間
    /// （見 `WeatherColumnKind::Now` 的說明），`Hour` 不需要——它的表頭本身
    /// 就是時間點。
    fn cells(&self) -> Vec<String> {
        match &self.kind {
            WeatherColumnKind::Now { temp_c, local_time, .. } => {
                vec![local_time.clone(), self.desc.to_string(), format!("{temp_c}°C"), format!("{}%", self.chance_of_rain)]
            }
            WeatherColumnKind::Hour { temp_c } => {
                vec![self.desc.to_string(), format!("{temp_c}°C"), format!("{}%", self.chance_of_rain)]
            }
            WeatherColumnKind::Day { min_c, max_c, .. } => {
                vec![self.desc.to_string(), format!("{min_c}~{max_c}°C"), format!("{}%", self.chance_of_rain)]
            }
        }
    }
}

/// 一個地點抓回來、已經整理好的報告：`status` 是 `Some` 代表還在抓取中／抓失敗
/// （`columns` 這時一定是空的，`text()`/`snapshot()` 都只顯示這個狀態訊息）；
/// 抓到真正資料時 `status` 是 `None`，`columns` 依序是 `now`、今天剩下的時段、
/// 然後未來幾天。
#[derive(Clone)]
struct LocationReport {
    place: String,
    status: Option<String>,
    columns: Vec<WeatherColumn>,
}

/// `snapshot()` 給 `/api/weather/list` 用的 JSON 結構，見該函式的說明。
#[derive(Clone, serde::Serialize)]
pub(crate) struct WeatherNowJson {
    category: String,
    desc: String,
    temp_c: i32,
    feels_like_c: i32,
    humidity: i32,
    chance_of_rain: u32,
    /// 該地點當地的現在時間，`"HH:MM"`，見 `WeatherColumnKind::Now` 的說明。
    local_time: String,
}

#[derive(Clone, serde::Serialize)]
pub(crate) struct WeatherHourJson {
    label: String,
    category: String,
    desc: String,
    temp_c: i32,
    chance_of_rain: u32,
}

#[derive(Clone, serde::Serialize)]
pub(crate) struct WeatherDayJson {
    label: String,
    category: String,
    desc: String,
    min_c: i32,
    max_c: i32,
    chance_of_rain: u32,
}

#[derive(Clone, serde::Serialize)]
pub(crate) struct LocationJson {
    /// 地點清單裡的識別碼：就是 `add` 時用的城市名字串本身（也是
    /// `remove <city>` 要打的那個名字），前端拿這個當「切換地區」時要
    /// 記住/送出的 key，`place` 只是給人看的顯示名稱。
    key: String,
    place: String,
    status: Option<String>,
    now: Option<WeatherNowJson>,
    today: Vec<WeatherHourJson>,
    days: Vec<WeatherDayJson>,
}

struct CacheEntry {
    fetched_at: Instant,
    report: LocationReport,
}

pub struct WeatherPlugin {
    #[allow(dead_code)]
    ctx: SharedContext,
    /// `add`/`remove` 維護的城市清單，依加入順序排列。這個欄位
    /// 本身還是只在持有 `Shell` 鎖的時候被讀寫（跟其他 plugin 的欄位一樣），
    /// 真正需要拆開來的是下面兩個會被背景執行緒同時存取的欄位。
    locations: Vec<String>,
    /// 每個地點最後一次抓到的報告，背景執行緒抓完就寫進來，`display()` 只負責
    /// 讀，不含任何網路呼叫，所以不會卡住持有 `Shell` 鎖的那個執行緒。
    cache: Arc<Mutex<HashMap<String, CacheEntry>>>,
    /// 目前正在背景抓的地點集合，避免同一個地點快取一過期，短時間內被連續
    /// 呼叫（例如 GUI 每次畫面重繪、web 每個 tick）就開出一堆重複的 curl 行程。
    pending: Arc<Mutex<HashSet<String>>>,
}

impl WeatherPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self {
            ctx,
            locations: Vec::new(),
            cache: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn show(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push(&format!("{}\n", self.text()));
        Ok(())
    }

    /// 判斷 `location` 是不是「台灣縣市＋行政區」，是的話回傳（該縣市對應
    /// 的 CWA dataset id, 行政區名稱）。行政區名稱可能是空字串（使用者只打
    /// 了縣市沒打區），呼叫端（`add`／`fetch_inner`）要自己擋，這裡不擋是
    /// 因為兩邊擋掉之後要做的事不一樣（`add` 直接 `bail!`，`fetch_inner`
    /// 顯示狀態訊息）。
    fn match_taiwan_location(location: &str) -> Option<(&'static str, String)> {
        let normalized = location.replace('台', "臺");
        CWA_COUNTIES
            .iter()
            .find_map(|&(county, resource_id)| normalized.strip_prefix(county).map(|rest| (resource_id, rest.trim().to_string())))
    }

    fn add(&mut self, args: &[String], out: &OutputBuffer) -> Result<()> {
        let city = args.join(" ");
        if city.is_empty() {
            bail!("add 需要接城市名稱");
        }
        if let Some((_, district)) = Self::match_taiwan_location(&city)
            && district.is_empty()
        {
            bail!("台灣地點要打完整的縣市＋行政區，例如 新北市新莊區");
        }
        if self.locations.iter().any(|l| l == &city) {
            out.push(&format!("weather 已經有 {city} 了\n"));
            return Ok(());
        }
        self.locations.push(city.clone());
        out.push(&format!("weather 新增 {city}\n"));
        Ok(())
    }

    fn remove(&mut self, args: &[String], out: &OutputBuffer) -> Result<()> {
        let city = args.join(" ");
        let before = self.locations.len();
        self.locations.retain(|l| l != &city);
        if self.locations.len() == before {
            out.push(&format!("weather 沒有 {city}\n"));
        } else {
            self.cache.lock().unwrap().remove(&city);
            out.push(&format!("weather 移除 {city}\n"));
        }
        Ok(())
    }

    /// 把每個 `add` 加進去的城市併成同一張表格：第一欄是 `location`，
    /// 後面依序是 `now`/今天剩下的時段/未來幾天。
    ///
    /// 表頭不能只挑「欄數最多的那一份」直接套用——`hourly_columns` 是依每個
    /// 地點自己的當地時間各自獨立過濾出「今天剩下的時段」（`timezone=auto`
    /// 讓 Open-Meteo 依座標換算當地時區），不同時區的地點今天剩下的時段數量
    /// 本來就會不一樣（歐洲凌晨可能還有 7 個時段沒過，亞洲下午可能只剩 5
    /// 個），直接拿其中一份的欄位當表頭、其他列硬塞進同樣位置會對不齊、資料
    /// 移位。改成依「表頭文字」對齊：把所有有資料地點的欄位表頭做聯集（`now`
    /// 固定第一欄，時段類表頭依文字排序，日期類表頭依第一次出現的順序），
    /// 每一列再依表頭文字去找自己有沒有那一欄，沒有就留空——這樣同一欄
    /// 「12:00」在不同列代表的是各自的當地時間中午，語意上仍然一致，只是
    /// 不同列不保證是同一個絕對時刻。還在抓取中或抓失敗的地點整列留空
    /// （只在第一個資料欄放狀態訊息）。
    fn text(&self) -> String {
        let mut reports = Vec::with_capacity(self.locations.len());
        for city in &self.locations {
            reports.push(self.display(city));
        }

        let mut hour_headers: Vec<String> = Vec::new();
        let mut day_headers: Vec<String> = Vec::new();
        let mut any_success = false;
        for report in &reports {
            let Some(columns) = report.status.is_none().then(|| &report.columns) else { continue };
            any_success = true;
            for column in columns {
                match &column.kind {
                    WeatherColumnKind::Now { .. } => {}
                    WeatherColumnKind::Hour { .. } => {
                        if !hour_headers.contains(&column.header) {
                            hour_headers.push(column.header.clone());
                        }
                    }
                    WeatherColumnKind::Day { .. } => {
                        if !day_headers.contains(&column.header) {
                            day_headers.push(column.header.clone());
                        }
                    }
                }
            }
        }
        hour_headers.sort();
        let headers: Vec<String> = if any_success {
            let mut h = vec!["now".to_string()];
            h.extend(hour_headers);
            h.extend(day_headers);
            h
        } else {
            vec!["狀態".to_string()]
        };

        let rows: Vec<Vec<Vec<String>>> = reports
            .into_iter()
            .map(|report| {
                let mut cells = vec![vec![report.place]];
                match report.status {
                    None => {
                        for header in &headers {
                            let cell =
                                report.columns.iter().find(|c| &c.header == header).map(WeatherColumn::cells).unwrap_or_default();
                            cells.push(cell);
                        }
                    }
                    Some(message) => {
                        cells.push(vec![message]);
                        cells.extend(std::iter::repeat_with(|| vec![String::new()]).take(headers.len().saturating_sub(1)));
                    }
                }
                cells
            })
            .collect();

        let mut full_headers = vec!["location".to_string()];
        full_headers.extend(headers);
        let header_refs: Vec<&str> = full_headers.iter().map(String::as_str).collect();
        render_table(&header_refs, &rows)
    }

    /// 給 tablet 前端用的結構化清單（`web.rs` 的 `/api/weather/list` 呼叫這個
    /// 轉成 JSON）：跟 `text()` 一樣依序是 `add` 加過的城市；跟 `text()`
    /// 不一樣的是這裡回傳的是可以直接拿去畫圖示/背景動畫的數字跟分類，不是
    /// 拼好的顯示字串。
    pub(crate) fn snapshot(&self) -> Vec<LocationJson> {
        self.locations.iter().map(|city| Self::location_json(city, self.display(city))).collect()
    }

    /// 把一個地點的 `LocationReport` 轉成 JSON 結構：`now`／`today`（今天剩下
    /// 的時段）／`days`（未來預報，跳過 `is_today` 那一筆，見
    /// `WeatherColumnKind::Day` 的說明）。
    fn location_json(key: &str, report: LocationReport) -> LocationJson {
        let Some(status) = report.status else {
            let mut now = None;
            let mut today = Vec::new();
            let mut days = Vec::new();
            for column in report.columns {
                match column.kind {
                    WeatherColumnKind::Now { temp_c, feels_like_c, humidity, local_time } => {
                        now = Some(WeatherNowJson {
                            category: column.slug.to_string(),
                            desc: column.desc.to_string(),
                            temp_c,
                            feels_like_c,
                            humidity,
                            chance_of_rain: column.chance_of_rain,
                            local_time,
                        });
                    }
                    WeatherColumnKind::Hour { temp_c } => {
                        today.push(WeatherHourJson {
                            label: column.header,
                            category: column.slug.to_string(),
                            desc: column.desc.to_string(),
                            temp_c,
                            chance_of_rain: column.chance_of_rain,
                        });
                    }
                    WeatherColumnKind::Day { min_c, max_c, is_today: true } => {
                        // 今天已經有 `now`／`today` 可以看，這裡不重複列。
                        let _ = (min_c, max_c);
                    }
                    WeatherColumnKind::Day { min_c, max_c, is_today: false } => {
                        days.push(WeatherDayJson {
                            label: column.header,
                            category: column.slug.to_string(),
                            desc: column.desc.to_string(),
                            min_c,
                            max_c,
                            chance_of_rain: column.chance_of_rain,
                        });
                    }
                }
            }
            return LocationJson { key: key.to_string(), place: report.place, status: None, now, today, days };
        };
        LocationJson { key: key.to_string(), place: report.place, status: Some(status), now: None, today: Vec::new(), days: Vec::new() }
    }

    /// 只讀快取，不做任何網路呼叫：有資料就回傳（同時判斷是否過期該重抓），
    /// 沒資料就先回傳「抓取中」的狀態列。真正的抓取一律丟給 `spawn_refresh`。
    fn display(&self, location: &str) -> LocationReport {
        let cached = self
            .cache
            .lock()
            .unwrap()
            .get(location)
            .map(|entry| (entry.fetched_at.elapsed(), entry.report.clone()));
        let stale = match &cached {
            None => true,
            Some((age, _)) => *age >= CACHE_TTL,
        };
        if stale {
            self.spawn_refresh(location);
        }
        match cached {
            Some((_, report)) => report,
            None => Self::placeholder(location, "抓取中..."),
        }
    }

    /// 只有一句狀態訊息、沒有任何欄位的報告，`display()`（還沒抓過）跟
    /// `fetch()`（抓失敗）共用。
    fn placeholder(location: &str, message: &str) -> LocationReport {
        LocationReport { place: location.to_string(), status: Some(message.to_string()), columns: Vec::new() }
    }

    /// 開一個背景執行緒去抓 `location` 的天氣，抓完寫回 `cache`；如果這個地點
    /// 已經有一個背景執行緒在抓了就不重複開，抓取本身（`Self::fetch`）完全不會
    /// 碰到 `Shell` 的鎖，呼叫端也不用等它做完。
    fn spawn_refresh(&self, location: &str) {
        let mut pending = self.pending.lock().unwrap();
        if !pending.insert(location.to_string()) {
            return; // 已經有背景執行緒在抓這個地點了。
        }
        drop(pending);

        let location = location.to_string();
        let cache = self.cache.clone();
        let pending = self.pending.clone();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            ctx.lock().unwrap().log_activity("external", format!("GET Open-Meteo forecast for {location}"));
            let report = Self::fetch(&location);
            cache.lock().unwrap().insert(location.clone(), CacheEntry { fetched_at: Instant::now(), report });
            pending.lock().unwrap().remove(&location);
        });
    }

    /// 用 `curl` 打一個網址、把回應內容當 JSON 解析，網路/curl 不存在/逾時
    /// （5 秒）/回應不是合法 JSON 都回傳 `None`，不 panic。`resolve_location`／
    /// `fetch` 共用這個小工具，避免兩個地方各寫一份幾乎一樣的 `Command::new`
    /// 邏輯。
    fn curl_json(url: &str) -> Option<Value> {
        let output = Command::new("curl").args(["--silent", "--max-time", "5", url]).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// 把使用者輸入的地點字串換成 (緯度, 經度, 顯示名稱)：Open-Meteo 的
    /// forecast API 只吃座標，不吃地名，透過 Open-Meteo 自己的地理編碼 API
    /// 查座標，顯示名稱維持使用者打的原始字串（不管查到的官方名稱是什麼，
    /// 這樣 `remove` 時打的名字才會跟畫面上看到的一致，也不用另外存一份
    /// 「原始輸入 -> 官方名稱」的對照）。
    fn resolve_location(location: &str) -> Option<(f64, f64, String)> {
        let name = location.replace(' ', "+");
        let url = format!("https://geocoding-api.open-meteo.com/v1/search?name={name}&count=1");
        let body = Self::curl_json(&url)?;
        let result = body.get("results")?.as_array()?.first()?;
        let lat = result.get("latitude")?.as_f64()?;
        let lon = result.get("longitude")?.as_f64()?;
        Some((lat, lon, location.to_string()))
    }

    /// 先查座標（`resolve_location`），再拿座標打 Open-Meteo 的 forecast API：
    /// `current`（現在）／`hourly`（逐小時，`forecast_days` 天數內都有）／
    /// `daily`（每日彙總）三個區塊一次要齊，`timezone=auto` 讓 Open-Meteo 依
    /// 座標算出當地時區，回應裡的所有時間字串都已經換算成當地時間（不是
    /// UTC）——這比原本 wttr.in 那套「拿這台機器自己的本地時間當『現在』去猜
    /// 今天過了幾點」準確，因為現在是直接用查詢地點真正的當地時間，`add` 加
    /// 的外國城市不會再有時差誤差。`forecast_days=4` 是「今天 + 未來 3
    /// 天」，`daily[]` 第一筆固定是今天（見 `WeatherColumnKind::Day` 的
    /// `is_today` 說明）。任何一步查不到都回傳 `None`，讓呼叫端顯示狀態訊息，
    /// 不 panic。只會在 `spawn_refresh` 開的背景執行緒裡呼叫，不會卡住任何
    /// 持有鎖的執行緒。
    fn fetch(location: &str) -> LocationReport {
        Self::fetch_inner(location).unwrap_or_else(|| Self::placeholder(location, "無法取得天氣資訊（網路異常、cwa-api-key 檔案不存在，或地點查無資料）"))
    }

    /// 台灣縣市＋行政區（`match_taiwan_location` 判斷出來）走 `fetch_cwa`，
    /// 其他地點照舊走 Open-Meteo。
    fn fetch_inner(location: &str) -> Option<LocationReport> {
        if let Some((resource_id, district)) = Self::match_taiwan_location(location) {
            if district.is_empty() {
                return None;
            }
            return Self::fetch_cwa(resource_id, &district, location);
        }
        let (lat, lon, place) = Self::resolve_location(location)?;
        let url = format!(
            "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}&current=temperature_2m,apparent_temperature,relative_humidity_2m,weather_code&hourly=temperature_2m,weather_code,precipitation_probability&daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max&timezone=auto&forecast_days=4"
        );
        let body = Self::curl_json(&url)?;
        let current = body.get("current")?;
        let hourly = body.get("hourly")?;
        let daily = body.get("daily")?;
        let mut columns = vec![Self::now_column(current, hourly)?];
        columns.extend(Self::hourly_columns(current, hourly));
        columns.extend(Self::daily_columns(daily));
        Some(LocationReport { place, status: None, columns })
    }

    /// `now` 那一欄：氣溫/天氣描述直接用 `current` 區塊，降雨機率取「現在
    /// 這個整點」的 `hourly.precipitation_probability`（`current` 區塊本身
    /// 沒有降雨機率這個欄位，只有 `hourly` 才有）。
    fn now_column(current: &Value, hourly: &Value) -> Option<WeatherColumn> {
        let temp_c = current.get("temperature_2m")?.as_f64()?.round() as i32;
        let feels_like_c = current.get("apparent_temperature").and_then(|v| v.as_f64()).map(|v| v.round() as i32).unwrap_or(temp_c);
        let humidity = current.get("relative_humidity_2m").and_then(|v| v.as_f64()).unwrap_or(0.0) as i32;
        let code = current.get("weather_code").and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
        let (slug, desc) = Self::classify(code);
        let now_time = current.get("time")?.as_str()?;
        let chance = Self::hour_chance_at(hourly, now_time).unwrap_or(0);
        let local_time = now_time.get(11..16)?.to_string();
        Some(WeatherColumn {
            header: "now".to_string(),
            slug,
            desc,
            chance_of_rain: chance,
            kind: WeatherColumnKind::Now { temp_c, feels_like_c, humidity, local_time },
        })
    }

    /// `now_time`（例如 `"2026-08-06T07:30"`）落在哪個整點，就用那個整點的
    /// `precipitation_probability` 當「現在」的降雨機率——`hourly[]` 只有
    /// 整點資料，沒有精確對到分鐘的資料可用，取所在整點已經是最接近的近似值。
    fn hour_chance_at(hourly: &Value, now_time: &str) -> Option<u32> {
        let bucket = format!("{}:00", now_time.get(0..13)?);
        let times = hourly.get("time")?.as_array()?;
        let rains = hourly.get("precipitation_probability")?.as_array()?;
        let idx = times.iter().position(|t| t.as_str() == Some(bucket.as_str()))?;
        rains.get(idx)?.as_f64().map(|v| v as u32)
    }

    /// 今天剩下的時段：`hourly[]` 是逐小時資料，篩「日期跟 `current.time`
    /// 同一天、時間不早於現在」，再取每 3 小時一筆（跟改之前 wttr.in 3 小時
    /// 一筆的密度一致，逐小時全部列出來一天會有到 20 幾筆，太密）。任何一筆
    /// 資料格式不對就跳過那一筆，不影響其他筆。
    fn hourly_columns(current: &Value, hourly: &Value) -> Vec<WeatherColumn> {
        let mut out = Vec::new();
        let Some(now_time) = current.get("time").and_then(|v| v.as_str()) else { return out };
        let Some(today) = now_time.get(0..10) else { return out };
        let Some(times) = hourly.get("time").and_then(|v| v.as_array()) else { return out };
        let temps = hourly.get("temperature_2m").and_then(|v| v.as_array());
        let codes = hourly.get("weather_code").and_then(|v| v.as_array());
        let rains = hourly.get("precipitation_probability").and_then(|v| v.as_array());

        for (i, t) in times.iter().enumerate() {
            let Some(t) = t.as_str() else { continue };
            if t.get(0..10) != Some(today) || t < now_time {
                continue;
            }
            let Some(hour) = t.get(11..13).and_then(|h| h.parse::<u32>().ok()) else { continue };
            if hour % 3 != 0 {
                continue;
            }
            let Some(temp_c) = temps.and_then(|a| a.get(i)).and_then(|v| v.as_f64()) else { continue };
            let code = codes.and_then(|a| a.get(i)).and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
            let chance = rains.and_then(|a| a.get(i)).and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
            let (slug, desc) = Self::classify(code);
            out.push(WeatherColumn {
                header: format!("{hour:02}:00"),
                slug,
                desc,
                chance_of_rain: chance,
                kind: WeatherColumnKind::Hour { temp_c: temp_c.round() as i32 },
            });
        }
        out
    }

    /// `daily[]` 依序是今天、明天、後天……（`forecast_days=4` 給今天 + 未來
    /// 3 天），第一筆（`i == 0`）固定是今天，`is_today` 見
    /// `WeatherColumnKind::Day` 的說明。
    fn daily_columns(daily: &Value) -> Vec<WeatherColumn> {
        let mut out = Vec::new();
        let Some(times) = daily.get("time").and_then(|v| v.as_array()) else { return out };
        let codes = daily.get("weather_code").and_then(|v| v.as_array());
        let maxes = daily.get("temperature_2m_max").and_then(|v| v.as_array());
        let mins = daily.get("temperature_2m_min").and_then(|v| v.as_array());
        let rains = daily.get("precipitation_probability_max").and_then(|v| v.as_array());

        for (i, t) in times.iter().enumerate() {
            let Some(date) = t.as_str() else { continue };
            let Some(max_c) = maxes.and_then(|a| a.get(i)).and_then(|v| v.as_f64()) else { continue };
            let Some(min_c) = mins.and_then(|a| a.get(i)).and_then(|v| v.as_f64()) else { continue };
            let code = codes.and_then(|a| a.get(i)).and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
            let chance = rains.and_then(|a| a.get(i)).and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
            let (slug, desc) = Self::classify(code);
            out.push(WeatherColumn {
                header: Self::short_date(date),
                slug,
                desc,
                chance_of_rain: chance,
                kind: WeatherColumnKind::Day { min_c: min_c.round() as i32, max_c: max_c.round() as i32, is_today: i == 0 },
            });
        }
        out
    }

    /// `"2026-07-18"` 這種 ISO 日期簡化成 `"7/18"`（月/日，不補零），當表頭用。
    fn short_date(date: &str) -> String {
        let mut parts = date.split('-');
        let _year = parts.next();
        let month: u32 = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
        let day: u32 = parts.next().and_then(|d| d.parse().ok()).unwrap_or(0);
        format!("{month}/{day}")
    }

    /// Open-Meteo 用的 WMO Weather interpretation code（<https://open-meteo.com/en/docs>
    /// 有完整對照表）分類成 (slug, 英文描述)：`desc` 沿用改之前
    /// `weather_text()` 的英文描述，給 `text()` 純文字輸出用（原因見改之前的
    /// 說明：emoji 兩邊寬度不一致、中文字型容易跑掉，純 ASCII 最穩）；`slug`
    /// 給 `snapshot()` 的 JSON 輸出用，tablet／webui 前端拿這個字串選圖示／
    /// 背景動畫。沒對到的代碼（含查不到值時傳進來的 `u64::MAX`）給一個中性的
    /// 分類，不讓整格空著。
    fn classify(code: u64) -> (&'static str, &'static str) {
        match code {
            0 => ("sunny", "Sunny"),
            1 => ("partly-cloudy", "Partly cloudy"),
            2 => ("cloudy", "Cloudy"),
            3 => ("overcast", "Overcast"),
            45 | 48 => ("fog", "Fog"),
            51 | 53 | 55 | 56 | 57 | 61 | 80 => ("showers", "Showers"),
            63 | 65 | 82 => ("rain", "Rain"),
            66 | 67 => ("ice-pellets", "Ice pellets"),
            71 | 73 | 75 | 77 | 85 | 86 => ("snow", "Snow"),
            95 | 96 | 99 => ("thunderstorm", "Thunderstorm"),
            _ => ("unknown", "Unknown"),
        }
    }

    /// 台灣行政區改用中央氣象署開放資料平台（CWA），不查 Open-Meteo：
    /// `resource_id`／`district` 是 `match_taiwan_location` 查出來的縣市
    /// dataset id／行政區名稱，`place` 保留使用者輸入的原始字串當顯示名稱
    /// （跟 Open-Meteo 那條路徑一致，`remove` 打的名字才會跟畫面上看到的
    /// 一致）。
    ///
    /// `now` 用最近氣象站的即時觀測（`nearest_cwa_station`），不是模型
    /// 預報——這是這次改用 CWA 的理由：Open-Meteo 的 `now` 只是全球模型算
    /// 出來的「這個時刻」數值，跟氣象站實測的落差在台灣夏天午後對流雨這種
    /// 很局部的天氣上特別明顯（同一個縣市，這裡在下、那裡沒下）。
    /// `今天剩下時段`／`未來幾天` 吃同一份逐 3 小時預報（`溫度`/`天氣現象`/
    /// `3小時降雨機率`/`體感溫度`），不用再多打一次 API。讀不到
    /// `CWA_KEY_FILE`（見該常數的說明）就直接回傳 `None` 讓呼叫端顯示狀態
    /// 訊息。
    fn fetch_cwa(resource_id: &str, district: &str, place: &str) -> Option<LocationReport> {
        let api_key = Self::cwa_api_key()?;
        let now = Self::cwa_now()?;
        let url = format!(
            "https://opendata.cwa.gov.tw/api/v1/rest/datastore/{resource_id}?Authorization={api_key}&format=JSON&LocationName={}",
            Self::url_encode(district)
        );
        let body = Self::curl_json(&url)?;
        let location = body.get("records")?.get("Locations")?.as_array()?.first()?.get("Location")?.as_array()?.first()?;
        let elements = location.get("WeatherElement")?.as_array()?;
        let element = |name: &str| elements.iter().find(|e| e.get("ElementName").and_then(|v| v.as_str()) == Some(name));
        let temps = element("溫度")?;
        let feels = element("體感溫度");
        let rains = element("3小時降雨機率")?;
        let codes = element("天氣現象")?;
        let lat: f64 = location.get("Latitude")?.as_str()?.parse().ok()?;
        let lon: f64 = location.get("Longitude")?.as_str()?.parse().ok()?;

        let mut columns = vec![Self::cwa_now_column(&api_key, lat, lon, rains, feels, &now)?];
        columns.extend(Self::cwa_hourly_columns(temps, codes, rains, &now));
        columns.extend(Self::cwa_daily_columns(temps, codes, rains, &now));
        Some(LocationReport { place: place.to_string(), status: None, columns })
    }

    /// 讀 `CWA_KEY_FILE` 拿授權碼，去頭尾空白（含檔案結尾常見的換行）；
    /// 檔案不存在或內容整個是空白都算「沒有 key」。
    fn cwa_api_key() -> Option<String> {
        let text = std::fs::read_to_string(CWA_KEY_FILE).ok()?;
        let key = text.trim();
        if key.is_empty() {
            None
        } else {
            Some(key.to_string())
        }
    }

    /// 台灣（Asia/Taipei，沒有夏令時間）現在的當地時間，格式
    /// `"YYYY-MM-DDTHH:MM"`，用來跟 CWA 回應裡的時間字串比對。跟
    /// `curl_json` 一樣叫系統指令（`date`）取，不用為了這一個換算拉時區/
    /// 日期函式庫進來。
    fn cwa_now() -> Option<String> {
        let output = Command::new("date").env("TZ", "Asia/Taipei").args(["+%Y-%m-%dT%H:%M"]).output().ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8(output.stdout).ok().map(|s| s.trim().to_string())
    }

    /// 手動 percent-encode（逐 byte 處理，UTF-8 的中文字元自然也會被正確
    /// 編碼）：CWA 的 `LocationName` 參數要吃中文行政區名，Open-Meteo
    /// geocoding 那條路徑原本只用 `.replace(' ', "+")` 打發（只會是英文
    /// 拼音），這裡沒有現成的 URL-encoding 函式庫依賴，資料量小，自己寫比
    /// 多拉一個 crate 進來划算。
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

    /// 找離 `(lat, lon)` 最近的中央氣象署自動氣象站，回傳該站的完整記錄
    /// （`records.Station[]` 其中一筆）。用經緯度歐幾里得距離找最近站——
    /// 台灣範圍小，這個近似不會有太大誤差，不需要真正的球面距離公式。
    /// 忽略沒有回傳氣溫的站（sentinel 值或缺欄位），不然可能選到故障中的
    /// 測站。
    fn nearest_cwa_station(api_key: &str, lat: f64, lon: f64) -> Option<Value> {
        let url = format!("https://opendata.cwa.gov.tw/api/v1/rest/datastore/O-A0003-001?Authorization={api_key}&format=JSON");
        let body = Self::curl_json(&url)?;
        let stations = body.get("records")?.get("Station")?.as_array()?;
        stations
            .iter()
            .filter_map(|s| {
                let temp = s.get("WeatherElement")?.get("AirTemperature")?.as_str()?.parse::<f64>().ok()?;
                if temp < -90.0 {
                    return None;
                }
                let coords = s.get("GeoInfo")?.get("Coordinates")?.as_array()?;
                let wgs = coords.iter().find(|c| c.get("CoordinateName").and_then(|v| v.as_str()) == Some("WGS84"))?;
                let slat = wgs.get("StationLatitude")?.as_str()?.parse::<f64>().ok()?;
                let slon = wgs.get("StationLongitude")?.as_str()?.parse::<f64>().ok()?;
                let dist = (slat - lat).powi(2) + (slon - lon).powi(2);
                Some((dist, s.clone()))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, s)| s)
    }

    /// `now` 欄：氣溫/濕度/天氣描述用最近氣象站的即時觀測（見
    /// `nearest_cwa_station`），降雨機率跟體感溫度氣象站沒有測，借用同一份
    /// 3 小時預報覆蓋「現在」這個時間點的值頂替。
    fn cwa_now_column(api_key: &str, lat: f64, lon: f64, rains: &Value, feels: Option<&Value>, now: &str) -> Option<WeatherColumn> {
        let station = Self::nearest_cwa_station(api_key, lat, lon)?;
        let we = station.get("WeatherElement")?;
        let temp_c = we.get("AirTemperature")?.as_str()?.parse::<f64>().ok()?.round() as i32;
        let humidity = we.get("RelativeHumidity").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0) as i32;
        let weather_text = we.get("Weather").and_then(|v| v.as_str()).unwrap_or("");
        let (slug, desc) = Self::classify_cwa(weather_text);
        let chance = Self::cwa_block_value_at(rains, now, "ProbabilityOfPrecipitation").unwrap_or(0.0).round() as u32;
        let feels_like_c = feels
            .and_then(|f| Self::cwa_hourly_value_at(f, now, "ApparentTemperature"))
            .map(|v| v.round() as i32)
            .unwrap_or(temp_c);
        let obs_time = station.get("ObsTime")?.get("DateTime")?.as_str()?;
        let local_time = obs_time.get(11..16)?.to_string();
        Some(WeatherColumn {
            header: "now".to_string(),
            slug,
            desc,
            chance_of_rain: chance,
            kind: WeatherColumnKind::Now { temp_c, feels_like_c, humidity, local_time },
        })
    }

    /// 在 `element`（`StartTime`/`EndTime` 3 小時區塊格式）裡找覆蓋 `now`
    /// 這個時間點的那一格，回傳 `key` 欄位的值。`cwa_now_column` 拿這個借
    /// 「現在」所在時段的預報值頂替氣象站沒測的欄位（降雨機率/體感溫度）。
    fn cwa_block_value_at(element: &Value, now: &str, key: &str) -> Option<f64> {
        let times = element.get("Time")?.as_array()?;
        for t in times {
            let Some(start) = t.get("StartTime").and_then(|v| v.as_str()).and_then(|s| s.get(0..16)) else { continue };
            let Some(end) = t.get("EndTime").and_then(|v| v.as_str()).and_then(|s| s.get(0..16)) else { continue };
            if !(start <= now && now < end) {
                continue;
            }
            let Some(raw) = t.get("ElementValue").and_then(|v| v.as_array()).and_then(|a| a.first()).and_then(|ev| ev.get(key)).and_then(|v| v.as_str())
            else {
                continue;
            };
            if let Ok(value) = raw.parse::<f64>() {
                return Some(value);
            }
        }
        None
    }

    /// 在 `element`（`DataTime` 逐時格式，例如 `溫度`/`體感溫度`）裡找 `now`
    /// 所在整點的那一筆，回傳 `key` 欄位的值。
    fn cwa_hourly_value_at(element: &Value, now: &str, key: &str) -> Option<f64> {
        let bucket = format!("{}:00", now.get(0..13)?);
        let times = element.get("Time")?.as_array()?;
        for t in times {
            let Some(time) = t.get("DataTime").and_then(|v| v.as_str()) else { continue };
            if time.get(0..16) != Some(bucket.as_str()) {
                continue;
            }
            let Some(raw) = t.get("ElementValue").and_then(|v| v.as_array()).and_then(|a| a.first()).and_then(|ev| ev.get(key)).and_then(|v| v.as_str())
            else {
                continue;
            };
            if let Ok(value) = raw.parse::<f64>() {
                return Some(value);
            }
        }
        None
    }

    /// 把 `element`（`DataTime` 逐時格式）整份轉成 `time -> value` 的表，
    /// `cwa_hourly_columns`／`cwa_daily_columns` 拿這個查逐時溫度，比每欄
    /// 都線性掃一次 `Time` 陣列快。
    fn cwa_hourly_map(element: &Value, key: &str) -> HashMap<String, f64> {
        let mut map = HashMap::new();
        let Some(times) = element.get("Time").and_then(|v| v.as_array()) else { return map };
        for t in times {
            let Some(time) = t.get("DataTime").and_then(|v| v.as_str()) else { continue };
            let Some(raw) = t.get("ElementValue").and_then(|v| v.as_array()).and_then(|a| a.first()).and_then(|ev| ev.get(key)).and_then(|v| v.as_str())
            else {
                continue;
            };
            if let Ok(value) = raw.parse::<f64>() {
                map.insert(time.to_string(), value);
            }
        }
        map
    }

    /// 把 `element`（`StartTime`/`EndTime` 3 小時區塊格式，例如
    /// `3小時降雨機率`）轉成 `StartTime -> value` 的表。
    fn cwa_block_map(element: &Value, key: &str) -> HashMap<String, f64> {
        let mut map = HashMap::new();
        let Some(times) = element.get("Time").and_then(|v| v.as_array()) else { return map };
        for t in times {
            let Some(start) = t.get("StartTime").and_then(|v| v.as_str()) else { continue };
            let Some(raw) = t.get("ElementValue").and_then(|v| v.as_array()).and_then(|a| a.first()).and_then(|ev| ev.get(key)).and_then(|v| v.as_str())
            else {
                continue;
            };
            if let Ok(value) = raw.parse::<f64>() {
                map.insert(start.to_string(), value);
            }
        }
        map
    }

    /// 把 `天氣現象` 拆成 `(StartTime, EndTime, Weather 文字)` 的清單，順序
    /// 跟 API 回應一樣（由早到晚）。`cwa_hourly_columns`／`cwa_daily_columns`
    /// 都以這份清單為主軸去對應溫度/降雨機率。
    fn cwa_weather_blocks(element: &Value) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        let Some(times) = element.get("Time").and_then(|v| v.as_array()) else { return out };
        for t in times {
            let Some(start) = t.get("StartTime").and_then(|v| v.as_str()) else { continue };
            let Some(end) = t.get("EndTime").and_then(|v| v.as_str()) else { continue };
            let Some(text) = t.get("ElementValue").and_then(|v| v.as_array()).and_then(|a| a.first()).and_then(|ev| ev.get("Weather")).and_then(|v| v.as_str())
            else {
                continue;
            };
            out.push((start.to_string(), end.to_string(), text.to_string()));
        }
        out
    }

    /// 今天剩下時段：`codes`（天氣現象）本身就是 3 小時一格，不用像
    /// Open-Meteo 那樣再篩「每 3 小時取一筆」，直接篩「跟現在同一天、還沒
    /// 開始（`now` 之後才開始）」就好——已經開始的那一格由 `now` 欄代表。
    /// 氣溫抓同一個起始時間點的逐時溫度（3 小時格線剛好對得上整點溫度
    /// 資料），降雨機率抓同一個起始時間的預報值，兩者缺資料就跳過那一欄，
    /// 不影響其他欄。
    fn cwa_hourly_columns(temps: &Value, codes: &Value, rains: &Value, now: &str) -> Vec<WeatherColumn> {
        let mut out = Vec::new();
        let Some(today) = now.get(0..10) else { return out };
        let temp_map = Self::cwa_hourly_map(temps, "Temperature");
        let rain_map = Self::cwa_block_map(rains, "ProbabilityOfPrecipitation");
        for (start, _end, text) in Self::cwa_weather_blocks(codes) {
            if start.get(0..10) != Some(today) {
                continue;
            }
            let Some(start16) = start.get(0..16) else { continue };
            if start16 <= now {
                continue;
            }
            let Some(hour) = start.get(11..13).and_then(|h| h.parse::<u32>().ok()) else { continue };
            let Some(&temp_c) = temp_map.get(&start) else { continue };
            let chance = rain_map.get(&start).copied().unwrap_or(0.0).round() as u32;
            let (slug, desc) = Self::classify_cwa(&text);
            out.push(WeatherColumn {
                header: format!("{hour:02}:00"),
                slug,
                desc,
                chance_of_rain: chance,
                kind: WeatherColumnKind::Hour { temp_c: temp_c.round() as i32 },
            });
        }
        out
    }

    /// 未來幾天：把 3 小時一格的資料依日期分組，`min_c`/`max_c` 用同一天
    /// 所有逐時溫度的最小/最大值，`slug`/`desc` 挑白天（06 點開始那一格）
    /// 當代表，沒有就用當天第一格頂替，降雨機率取當天所有時段的最大值。
    /// 跟現在同一天的那一筆 `is_today=true`，理由跟 Open-Meteo 那條路徑
    /// 一樣（見 `WeatherColumnKind::Day` 的說明）。當天完全沒有溫度資料
    /// （超出這份預報覆蓋的時間範圍）就跳過，不硬湊一筆假資料。
    fn cwa_daily_columns(temps: &Value, codes: &Value, rains: &Value, now: &str) -> Vec<WeatherColumn> {
        let mut out = Vec::new();
        let Some(today) = now.get(0..10) else { return out };
        let temp_map = Self::cwa_hourly_map(temps, "Temperature");
        let rain_map = Self::cwa_block_map(rains, "ProbabilityOfPrecipitation");
        let blocks = Self::cwa_weather_blocks(codes);

        let mut dates: Vec<&str> = Vec::new();
        for (start, _, _) in &blocks {
            let Some(date) = start.get(0..10) else { continue };
            if !dates.contains(&date) {
                dates.push(date);
            }
        }

        for date in dates {
            let day_temps: Vec<f64> = temp_map.iter().filter(|(t, _)| t.get(0..10) == Some(date)).map(|(_, v)| *v).collect();
            if day_temps.is_empty() {
                continue;
            }
            let min_c = day_temps.iter().cloned().fold(f64::INFINITY, f64::min);
            let max_c = day_temps.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let chance = blocks
                .iter()
                .filter(|(start, _, _)| start.get(0..10) == Some(date))
                .filter_map(|(start, _, _)| rain_map.get(start))
                .cloned()
                .fold(0.0, f64::max) as u32;
            let representative = blocks
                .iter()
                .find(|(start, _, _)| start.get(0..10) == Some(date) && start.get(11..13) == Some("06"))
                .or_else(|| blocks.iter().find(|(start, _, _)| start.get(0..10) == Some(date)));
            let Some((_, _, text)) = representative else { continue };
            let (slug, desc) = Self::classify_cwa(text);
            out.push(WeatherColumn {
                header: Self::short_date(date),
                slug,
                desc,
                chance_of_rain: chance,
                kind: WeatherColumnKind::Day { min_c: min_c.round() as i32, max_c: max_c.round() as i32, is_today: date == today },
            });
        }
        out
    }

    /// CWA 的天氣描述是固定詞彙組合出來的中文字串（例如「短暫陣雨」、
    /// 「多雲時晴」），不像 Open-Meteo 給一個 WMO 代碼可以直接查表，這裡改
    /// 用關鍵字比對分類成跟 `classify` 一樣的 (slug, 英文描述) 組合，兩個
    /// 資料源共用同一套圖示/背景分類鍵，前端不用分 Open-Meteo/CWA 兩套。
    /// 關鍵字判斷順序有意義：先比對「雷」「雪」「霧」這些沒有歧義的字，再
    /// 比對「陣雨」/「雨」（陣雨要先比對，不然「短暫陣雨」會先被「雨」比
    /// 對到但分類邏輯其實一樣，純粹保留語意上的優先順序），最後才是單純的
    /// 晴/多雲/陰組合，避免「多雲時晴短暫陣雨」這種複合描述被歸到錯的
    /// 分類。
    fn classify_cwa(text: &str) -> (&'static str, &'static str) {
        if text.contains('雷') {
            ("thunderstorm", "Thunderstorm")
        } else if text.contains('雪') {
            ("snow", "Snow")
        } else if text.contains('霧') {
            ("fog", "Fog")
        } else if text.contains("陣雨") {
            ("showers", "Showers")
        } else if text.contains('雨') {
            ("rain", "Rain")
        } else if text.contains('晴') && text.contains("多雲") {
            ("partly-cloudy", "Partly cloudy")
        } else if text.contains('陰') {
            ("overcast", "Overcast")
        } else if text.contains("多雲") {
            ("cloudy", "Cloudy")
        } else if text.contains('晴') {
            ("sunny", "Sunny")
        } else {
            ("unknown", "Unknown")
        }
    }
}

impl Plugin for WeatherPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["show", "add <city>", "remove <city>"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "show" => self.show(out),
            "add" => self.add(args, out),
            "remove" => self.remove(args, out),
            other => bail!("weather 不認得指令: {other}"),
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
