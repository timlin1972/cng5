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
/// GUI 畫面、web 的 ticker，也可能是 CLI）當場去打 wttr.in，而是丟給背景執行緒
/// 抓（見 `spawn_refresh`），呼叫端先拿現有資料（或「抓取中」字樣），不會被網路
/// 卡住——跟 `SystemPlugin` 每次都直接查 `tailscale`（本機、夠快）不一樣，天氣
/// 是真的網路請求，可能要好幾秒，不能讓大家等它，等於是拿著 `Shell` 的鎖去等
/// 外部網站回應。
const CACHE_TTL: Duration = Duration::from_secs(300);

/// `manual` 指令的說明。
const MANUAL_TEXT: &str = "\
weather：抓 wttr.in 的天氣資料，用純文字表格顯示現在/今天剩下時段/未來幾天。

範例：
  show          顯示表格（第一列永遠是依來源 IP 自動判斷的地點，後面接著
                add 加過的城市）
  add Tokyo     加一個城市，之後 show/panel 都會列出來
  remove Tokyo  移除一個城市

資料每 300 秒（CACHE_TTL）快取一次，過期後背景執行緒重新抓，show/panel 顯示的
都是目前的快取值（或「抓取中」），不會讓你等網路回應卡住畫面。
";

/// 空字串這個 key 代表讓 wttr.in 依來源 IP 自動判斷地點，`text()` 永遠把它排
/// 表格第一列，`locations` 裡 `add` 加進來的城市依序排在後面。
const AUTO_DETECT: &str = "";

/// 一個地點抓回來、已經整理好的報告：`headers`/`row` 一一對應（`now`、今天剩下
/// 的時段、未來幾天），還在抓取中或抓失敗時只有一欄狀態訊息（`headers` 長度是
/// 1），`text()` 組合表格時靠這個長度差異分辨要不要當成「有資料」的一列。
#[derive(Clone)]
struct LocationReport {
    place: String,
    headers: Vec<String>,
    row: Vec<Vec<String>>,
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

    fn add(&mut self, args: &[String], out: &OutputBuffer) -> Result<()> {
        let city = args.join(" ");
        if city.is_empty() {
            bail!("add 需要接城市名稱");
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

    /// 把「IP 反查」跟每個 `add` 加進去的城市合併成同一張表格：第一欄
    /// 是 `location`，後面依序是 `now`/今天剩下的時段/未來幾天。欄位名稱取自
    /// 任何一個已經抓到真正資料的地點（各地點理論上算出來的欄位都一樣，因為都
    /// 是用同一個「現在」去篩今天剩下哪些時段）；還在抓取中或抓失敗的地點，
    /// `headers` 長度只有 1，就只在第一個資料欄放狀態訊息，其餘欄位留空。
    fn text(&self) -> String {
        let mut reports = Vec::with_capacity(1 + self.locations.len());
        reports.push(self.display(AUTO_DETECT));
        for city in &self.locations {
            reports.push(self.display(city));
        }

        let headers = reports
            .iter()
            .max_by_key(|report| report.row.len())
            .map(|report| report.headers.clone())
            .unwrap_or_else(|| vec!["狀態".to_string()]);

        let rows: Vec<Vec<Vec<String>>> = reports
            .into_iter()
            .map(|report| {
                let mut cells = vec![vec![report.place]];
                if report.row.len() == headers.len() {
                    cells.extend(report.row);
                } else {
                    let message = report.row.into_iter().next().unwrap_or_default();
                    cells.push(message);
                    cells.extend(std::iter::repeat_with(|| vec![String::new()]).take(headers.len().saturating_sub(1)));
                }
                cells
            })
            .collect();

        let mut full_headers = vec!["location".to_string()];
        full_headers.extend(headers);
        let header_refs: Vec<&str> = full_headers.iter().map(String::as_str).collect();
        render_table(&header_refs, &rows)
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

    /// 只有一欄狀態訊息的報告，`display()`（還沒抓過）跟 `fetch()`（抓失敗）共用。
    fn placeholder(location: &str, message: &str) -> LocationReport {
        let place = if location.is_empty() { "自動偵測".to_string() } else { location.to_string() };
        LocationReport { place, headers: vec!["狀態".to_string()], row: vec![vec![message.to_string()]] }
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
            ctx.lock().unwrap().log_activity("external", format!("GET https://wttr.in/{}?format=j1", location.replace(' ', "+")));
            let report = Self::fetch(&location);
            cache.lock().unwrap().insert(location.clone(), CacheEntry { fetched_at: Instant::now(), report });
            pending.lock().unwrap().remove(&location);
        });
    }

    /// 用 `curl` 打 wttr.in 拿 JSON（`?format=j1`）並解析。`location` 空字串時
    /// 網址不帶地點，讓 wttr.in 依來源 IP 自動判斷。沒裝 curl、沒網路、逾時
    /// （5 秒）或回應格式不對都算沒有，回傳看得懂的狀態訊息，不 panic。只會在
    /// `spawn_refresh` 開的背景執行緒裡呼叫，不會卡住任何持有鎖的執行緒。
    fn fetch(location: &str) -> LocationReport {
        let place = location.replace(' ', "+");
        let url = format!("https://wttr.in/{place}?format=j1");
        Command::new("curl")
            .args(["--silent", "--max-time", "5", &url])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|body| Self::parse(&body, location))
            .unwrap_or_else(|| Self::placeholder(location, "無法取得天氣資訊（沒有網路或未安裝 curl）"))
    }

    /// 解析 wttr.in `j1` JSON，組出一個地點的報告：欄位依序是 `now`（現在）、
    /// 今天剩下還沒過去的每 3 小時時段、然後 `weather[]` 每一天（wttr.in 預設給
    /// 今天起 3 天，含今天）；每一欄的內容都是三行——天氣描述、溫度、降雨機率
    /// 各自獨立一行。
    fn parse(body: &str, requested_location: &str) -> Option<LocationReport> {
        let json: Value = serde_json::from_str(body).ok()?;
        let current = json.get("current_condition")?.get(0)?;
        let days = json.get("weather")?.as_array()?;
        let today = days.first()?;
        let now_minutes = Self::now_minutes();

        let place = if requested_location.is_empty() {
            json.get("nearest_area")?
                .get(0)?
                .get("areaName")?
                .get(0)?
                .get("value")?
                .as_str()?
                .to_string()
        } else {
            requested_location.to_string()
        };

        let mut columns: Vec<(String, Vec<String>)> =
            vec![("now".to_string(), Self::now_column(current, today, now_minutes))];

        if let Some(hourly) = today.get("hourly").and_then(|h| h.as_array()) {
            columns.extend(hourly.iter().filter_map(|hour| Self::hourly_column(hour, now_minutes)));
        }
        columns.extend(days.iter().filter_map(Self::daily_column));

        let (headers, row): (Vec<String>, Vec<Vec<String>>) = columns.into_iter().unzip();
        Some(LocationReport { place, headers, row })
    }

    /// `now` 那一欄：氣溫/天氣描述直接用 `current_condition`，降雨機率取「現在
    /// 這個時段」（見 `current_chance_of_rain`）。
    fn now_column(current: &Value, today: &Value, now_minutes: Option<u32>) -> Vec<String> {
        let temp = current.get("temp_C").and_then(|v| v.as_str()).unwrap_or("?");
        let desc = current.get("weatherCode").and_then(|v| v.as_str()).map(Self::weather_text).unwrap_or("");
        let chance = Self::current_chance_of_rain(today, now_minutes);
        Self::cell(desc, temp, chance)
    }

    /// 今天單一個 3 小時時段那一欄：時間已經過去（早於 `now_minutes`）就回傳
    /// `None`，讓呼叫端直接跳過這一欄，不列進表格裡。
    fn hourly_column(hour: &Value, now_minutes: Option<u32>) -> Option<(String, Vec<String>)> {
        let hhmm = Self::hour_hhmm(hour)?;
        let slot_minutes = Self::hhmm_to_minutes(&hhmm)?;
        if now_minutes.is_some_and(|now| slot_minutes < now) {
            return None;
        }
        let temp = hour.get("tempC")?.as_str()?;
        let chance: u32 = hour.get("chanceofrain")?.as_str()?.parse().ok()?;
        let desc = hour.get("weatherCode").and_then(|v| v.as_str()).map(Self::weather_text).unwrap_or("");
        let header = format!("{}:00", &hhmm[0..2]);
        Some((header, Self::cell(desc, temp, chance)))
    }

    /// `weather[]` 裡一整天那一欄：表頭是「月/日」短日期，內容是天氣描述/氣溫
    /// 範圍/當天最高降雨機率；天氣描述挑中午（`1200`）那個時段代表這一天，
    /// wttr.in 自己選每日圖示也是這樣挑的，沒有中午這筆就退回第一筆。
    fn daily_column(day: &Value) -> Option<(String, Vec<String>)> {
        let date = day.get("date")?.as_str()?;
        let min = day.get("mintempC")?.as_str()?;
        let max = day.get("maxtempC")?.as_str()?;
        let chance = Self::day_chance_of_rain(day);
        let hourly = day.get("hourly").and_then(|h| h.as_array());
        let noon = hourly.and_then(|hourly| {
            hourly
                .iter()
                .find(|hour| hour.get("time").and_then(|v| v.as_str()) == Some("1200"))
                .or_else(|| hourly.first())
        });
        let desc = noon.and_then(|hour| hour.get("weatherCode")).and_then(|v| v.as_str()).map(Self::weather_text).unwrap_or("");
        Some((Self::short_date(date), vec![desc.to_string(), format!("{min}~{max}°C"), format!("{chance}%")]))
    }

    /// 一個儲存格的內容：天氣描述、溫度、降雨機率各自獨立一行。
    fn cell(desc: &str, temp: &str, chance: u32) -> Vec<String> {
        vec![desc.to_string(), format!("{temp}°C"), format!("{chance}%")]
    }

    /// `"2026-07-18"` 這種 ISO 日期簡化成 `"7/18"`（月/日，不補零），當表頭用。
    fn short_date(date: &str) -> String {
        let mut parts = date.split('-');
        let _year = parts.next();
        let month: u32 = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
        let day: u32 = parts.next().and_then(|d| d.parse().ok()).unwrap_or(0);
        format!("{month}/{day}")
    }

    /// wttr.in（World Weather Online）的 `weatherCode` 對應到一句簡短的英文天氣
    /// 描述。原本試過 emoji 圖示，但 emoji 在終端機/瀏覽器兩邊的實際顯示寬度不
    /// 保證一致（各自的字型/渲染引擎決定，不是我們這邊寬度算得準不準的問題）；
    /// 中文描述則是中文字在瀏覽器裡容易被换成跟西文等寬字型沒對齊好的替代字型
    /// （見 `body` 的 `font-family` 註解）。純 ASCII 文字兩邊都不會有這些問題。
    /// 沒對到的代碼給一個中性的說法，不讓整格空著。
    fn weather_text(code: &str) -> &'static str {
        match code {
            "113" => "Sunny",
            "116" => "Partly cloudy",
            "119" => "Cloudy",
            "122" => "Overcast",
            "143" | "248" | "260" => "Fog",
            "176" | "263" | "266" | "293" | "353" => "Showers",
            "179" | "182" | "227" | "230" | "317" | "320" | "323" | "326" | "329" | "332" | "335" | "338" | "362"
            | "365" | "368" | "371" => "Snow",
            "185" | "281" | "284" | "296" | "299" | "302" | "305" | "308" | "311" | "314" | "356" => "Rain",
            "200" | "359" | "386" | "389" | "392" | "395" => "Thunderstorm",
            "350" | "374" | "377" => "Ice pellets",
            _ => "Unknown",
        }
    }

    /// `hour.time` 那種數字字串（`"0"`/`"300"`/`"1500"`…）補 0 成 4 碼的
    /// `"HHMM"`，`hourly_column`/`current_chance_of_rain` 都要拿它去算分鐘數。
    fn hour_hhmm(hour: &Value) -> Option<String> {
        let time = hour.get("time")?.as_str()?;
        Some(format!("{time:0>4}"))
    }

    /// 一天裡每個時段 `chanceofrain` 的最大值，當作那一天的降雨機率代表值
    /// （未來預報那幾欄用這個，「今天」那一整天也是）。
    fn day_chance_of_rain(day: &Value) -> u32 {
        day.get("hourly")
            .and_then(|h| h.as_array())
            .map(|hourly| {
                hourly
                    .iter()
                    .filter_map(|hour| hour.get("chanceofrain")?.as_str()?.parse::<u32>().ok())
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }

    /// 「現在這個時段」的降雨機率：拿 `now_minutes`（`now_minutes()` 這台機器
    /// 自己的本地時間）跟 `today.hourly[]` 每一筆（每 3 小時一筆）的時間比對，
    /// 取最接近的那一筆的 `chanceofrain`。`now_minutes` 是 `None`（抓不到本機
    /// 時間）就退回「今天最高機率」，不要讓整個 panel 因為這個而顯示不出來。
    fn current_chance_of_rain(today: &Value, now_minutes: Option<u32>) -> u32 {
        let Some(now) = now_minutes else {
            return Self::day_chance_of_rain(today);
        };
        let hourly = today.get("hourly").and_then(|h| h.as_array());
        let Some(hourly) = hourly else {
            return Self::day_chance_of_rain(today);
        };

        hourly
            .iter()
            .filter_map(|hour| {
                let slot_minutes = Self::hhmm_to_minutes(&Self::hour_hhmm(hour)?)?;
                let diff = slot_minutes.abs_diff(now).min(1440 - slot_minutes.abs_diff(now));
                let chance = hour.get("chanceofrain")?.as_str()?.parse::<u32>().ok()?;
                Some((diff, chance))
            })
            .min_by_key(|(diff, _)| *diff)
            .map(|(_, chance)| chance)
            .unwrap_or_else(|| Self::day_chance_of_rain(today))
    }

    /// 把 wttr.in `hourly[].time` 那種 4 位數字字串（`"0"`/`"300"`/`"1500"`…，
    /// 已經補好 0 成 4 碼的 `"HHMM"`）換算成當天的分鐘數。
    fn hhmm_to_minutes(hhmm: &str) -> Option<u32> {
        let value: u32 = hhmm.parse().ok()?;
        Some((value / 100) * 60 + value % 100)
    }

    /// 這台機器目前的本地時間（分鐘數，0-1439），拿來判斷「今天每 3 小時」欄位
    /// 哪些時段已經過去。wttr.in 的回應裡查得到的時間欄位（`observation_time`）
    /// 實際上是 UTC，不是查詢地點的當地時間，而且回應裡也沒有任何時區資訊可以
    /// 換算——所以退而求其次用這台機器自己的本地時間當「現在」：對 auto-detect
    /// （依來源 IP 定位，通常就是這台機器所在地）會很準，對用 `add`
    /// 手動加的其他城市則只是近似值（可能跟當地實際時間差到時差那麼多）。
    fn now_minutes() -> Option<u32> {
        let output = Command::new("date").arg("+%H%M").output().ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        Self::hhmm_to_minutes(text.trim())
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
