use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use epub::doc::{EpubDoc, NavPoint};
use serde::Serialize;

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};

/// 電子書檔案放這裡（相對於程式執行時的工作目錄）。特意放在 `storage/`
/// 底下（不是像 `music/` 那樣獨立一個頂層資料夾）：`storage` plugin 本來就
/// 有真正的二進位上傳端點（`POST /api/storage/upload`），一開始就能透過
/// 現有的 storage 網頁介面把 `.epub` 檔案丟進來；後來 ereader 自己另外加了
/// `POST /api/ereader/upload/{name}`（見 `web.rs`）跟 `haodoo_import`（好讀
/// 網站一鍵下載，見下方），但檔案還是統一放在這裡，書架清單不用管檔案是
/// 怎麼進來的。
pub(crate) const EBOOK_DIR: &str = "storage/ebooks";

/// 閱讀進度存放位置，跟 `todo`/`notepad` 同一套「放在 `storage/` 底下自己
/// 的子資料夾」慣例——順便讓 `sync` plugin 能自動把閱讀進度同步到其他裝置，
/// 不用另外寫一套同步邏輯。
const PROGRESS_DIR: &str = "storage/ereader";
const PROGRESS_FILE: &str = "storage/ereader/progress.json";

const MANUAL_TEXT: &str = "\
ereader：讀 storage/ebooks/ 資料夾裡的 epub 電子書。

範例：
  list                 列出目前有的書（含每本讀到多少%）
  open 三國演義.epub    開始讀（或接續上次讀到的地方）
  next                 下一章
  prev                 上一章

epub 檔案要自己先用 storage plugin（網頁介面的 storage 分頁）上傳進
storage/ebooks/ 資料夾，這個 plugin 不會自己抓/下載電子書。

panel 顯示目前讀到的章節內容（純文字，HTML 標籤會被剝掉，不保留原始排版）。
直式（vertical-rl）書在這裡一律當成橫式純文字瀏覽——終端機沒有辦法真的排出
直式效果，要看真正的直式排版請用瀏覽器版的 ereader 分頁（webui/tablet/
iphone 三邊都有，直接讓瀏覽器原生渲染 epub 自己的 XHTML/CSS）。

每本書讀到第幾章會自動存到 storage/ereader/progress.json，跟 todo/notepad
用同一套「放在 storage/ 底下讓 sync plugin 自動同步」的慣例。
";

/// 檔名安全性檢查，跟 `web.rs` 的 `safe_music_path`／`safe_storage_path`
/// 同一套規則：只接受單一檔名，不能含路徑分隔符或是 `.`/`..`——不管是這裡
/// 組出來的路徑、還是網頁那邊收到的檔名參數，都要過這一關，這是最後一道
/// 防線。
pub(crate) fn safe_ebook_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return None;
    }
    Some(Path::new(EBOOK_DIR).join(name))
}

/// 好讀（haodoo.net）書籍一鍵匯入：使用者把書籍詳細頁（`?M=book&P=<代號>`）
/// 的網址貼過來，這裡把「抓頁面 → 解析下載代號 → 抓 epub 檔案 → 存進
/// `storage/ebooks/`」整套流程做完，不用使用者自己先下載到本機再手動上傳
/// 一輪。
///
/// 好讀書籍詳細頁裡的下載按鈕是 `<input type=button onClick=
/// "DownloadEpub('<檔案代號>')">`（直式是 `DownloadVEpub`），不是普通的
/// `<a href>` 連結，沒辦法直接把按鈕的網址抓出來——而且這個檔案代號比網址
/// 上的 `P=` 代號多一個字首字母（不同分類前綴不同，例如武俠類是 `G`、科幻
/// 類是 `J`），這個字首是網站樣板直接寫死在 HTML 裡，猜不出來，只能從頁面
/// 內容解析。解析出代號後跟該網站自己的 `d.js`（`DownloadEpub`/
/// `DownloadVEpub`）做一樣的事：把代號從第一個字元切開，組成
/// `https://haodoo.net/PDB/<字首>/<代號去掉字首>.epub` 這個真正的檔案
/// 網址（這個網址是公開、不需要登入的靜態檔案，用 curl 直接抓即可）。
///
/// 這裡先擋 host 一定要是 `haodoo.net`，避免這個功能被拿去當成「叫伺服器
/// 幫忙打任意網址」的跳板（SSRF）——好讀本體 `haodoo.net` 跟後來另外註冊
/// 的 `haodoo.org` 其實是完全不同的兩個網站（後者是 WordPress.com 上的
/// 部落格，没有這裡假設的下載連結格式），所以刻意不放行 `haodoo.org`。
pub(crate) fn haodoo_import(page_url: &str, vertical: bool) -> Result<String> {
    let is_haodoo_net = ["https://haodoo.net/", "http://haodoo.net/", "https://www.haodoo.net/", "http://www.haodoo.net/"]
        .iter()
        .any(|prefix| page_url.starts_with(prefix));
    if !is_haodoo_net {
        bail!("只接受 haodoo.net 書籍詳細頁的網址");
    }
    let html_bytes = curl_get(page_url)?;
    let html = String::from_utf8_lossy(&html_bytes).into_owned();
    let needle = if vertical { "DownloadVEpub('" } else { "DownloadEpub('" };
    let start = html.find(needle).context("這個網址找不到下載按鈕，確認是好讀的書籍詳細頁網址（?M=book&P=...）")? + needle.len();
    let end = html[start..].find('\'').context("下載代號解析失敗")? + start;
    let code = &html[start..end];
    let Some((prefix, rest)) = code.split_at_checked(1) else {
        bail!("下載代號是空的");
    };
    let epub_url = format!("https://haodoo.net/PDB/{prefix}/{rest}.epub");
    let epub_bytes = curl_get(&epub_url)?;
    if epub_bytes.is_empty() {
        bail!("下載電子書失敗");
    }
    let title = extract_book_title(&html).unwrap_or_else(|| code.to_string());
    // 同一本書的橫式/直式是兩個不同檔案（epub 內部 `writing-mode` 不同，
    // `chapter_is_vertical` 判斷出來的翻頁方式也會不一樣），檔名不加這個
    // 標記的話，使用者兩種都下載會後蓋前，書架上只留得住最後下載那一種。
    let variant_suffix = if vertical { "（直式）" } else { "" };
    let file_name = format!("{}{variant_suffix}.epub", sanitize_ebook_filename(&title));
    let dest = safe_ebook_path(&file_name).context("檔名不合法")?;
    fs::write(&dest, &epub_bytes).context("寫入檔案失敗")?;
    Ok(file_name)
}

fn curl_get(url: &str) -> Result<Vec<u8>> {
    let output = Command::new("curl").args(["--silent", "--fail", "--max-time", "20", url]).output().context("執行 curl 失敗")?;
    if !output.status.success() {
        bail!("下載失敗：{url}");
    }
    Ok(output.stdout)
}

/// 書籍詳細頁裡書名夾在 `《...》` 之間（例如 `《少年衛斯理》`），頁面上這種
/// 標記不只書名一處會用到（網站自己的 logo/頁尾也是），但書名固定是頁面裡
/// 第一個出現的，足夠當檔名用——抓不到、或抓到的剛好是網站自己的名稱就
/// 放棄，讓呼叫端退回用下載代號當檔名。
fn extract_book_title(html: &str) -> Option<String> {
    let start = html.find('《')? + '《'.len_utf8();
    let end = html[start..].find('》')? + start;
    let title = html[start..end].trim();
    if title.is_empty() || title == "好讀" {
        None
    } else {
        Some(title.to_string())
    }
}

/// 書名拿來當檔名前先擋掉路徑分隔符——`safe_ebook_path` 本來就會擋，這裡
/// 先做一次是避免書名剛好含有 `/`（真實存在，例如叢書合輯常見「上/下」
/// 這種寫法）時，直接被 `safe_ebook_path` 拒絕、白白抓了老半天的檔案卻存
/// 不進去。
fn sanitize_ebook_filename(name: &str) -> String {
    name.chars().map(|c| if c == '/' || c == '\\' { '_' } else { c }).collect()
}

type Doc = EpubDoc<std::io::BufReader<std::fs::File>>;

fn open_book(path: &Path) -> Result<Doc> {
    EpubDoc::new(path).context("開啟電子書失敗（檔案不存在，或不是合法的 epub）")
}

/// 讀 OPF 的 `<spine page-progression-direction="rtl">` 屬性，決定這本書是
/// 從左到右還是從右到左翻頁（直式中日文書通常是 `rtl`）。`epub` crate 沒有
/// 公開方法讀這個屬性，但 `doc.root_file`（OPF 檔案本身在 epub 內部的路徑）
/// 是公開欄位，直接拿它的原始位元組做簡單字串搜尋就好——這個屬性只會出現
/// 在單一個 `<spine>` 標籤上，不需要真的解析整份 XML 結構。
fn page_progression_direction(doc: &mut Doc) -> &'static str {
    let root_file = doc.root_file.clone();
    let opf = doc.get_resource_by_path(&root_file).and_then(|bytes| String::from_utf8(bytes).ok()).unwrap_or_default();
    if opf.contains("page-progression-direction=\"rtl\"") || opf.contains("page-progression-direction='rtl'") {
        "rtl"
    } else {
        "ltr"
    }
}

fn load_progress() -> HashMap<String, usize> {
    fs::read_to_string(PROGRESS_FILE).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn save_progress(progress: &HashMap<String, usize>) -> Result<()> {
    fs::create_dir_all(PROGRESS_DIR).context("建立 ereader 進度目錄失敗")?;
    fs::write(PROGRESS_FILE, serde_json::to_string_pretty(progress)?).context("儲存 ereader 進度失敗")
}

/// `name` 這本書目前讀到第幾章（spine index，從 0 開始），沒讀過就是 0。
pub(crate) fn chapter_progress(name: &str) -> usize {
    load_progress().get(name).copied().unwrap_or(0)
}

pub(crate) fn save_chapter_progress(name: &str, chapter: usize) -> Result<()> {
    let mut progress = load_progress();
    progress.insert(name.to_string(), chapter);
    save_progress(&progress)
}

#[derive(Clone, Serialize)]
pub(crate) struct SpineEntry {
    /// 這一章在 epub 內部的資源路徑，直接餵給 `book_resource`／
    /// `GET /api/ereader/book/{name}/resource/{href}` 就能拿到內容。
    pub(crate) href: String,
}

/// 目錄裡的一條項目：顯示名稱＋點了要跳去 `spine` 的第幾章。epub 的目錄
/// （`toc.ncx`）本來就是獨立於 `spine` 之外的一份「人類看得懂的章節標題」
/// 清單，跟章節內容檔案的實際順序（`spine`）是兩回事，所以要各自轉換成
/// 同一個座標（spine index）才能對得起來。
#[derive(Clone, Serialize)]
pub(crate) struct TocEntry {
    pub(crate) label: String,
    pub(crate) chapter: usize,
}

/// `GET /api/ereader/book/{name}/meta` 的回應，也是 CLI 的 `open`/`next`/
/// `prev` 共用的內部結構。
#[derive(Clone, Serialize)]
pub(crate) struct BookMeta {
    pub(crate) title: String,
    pub(crate) author: Option<String>,
    /// `"ltr"` 或 `"rtl"`，見 `page_progression_direction`。
    pub(crate) direction: &'static str,
    pub(crate) spine: Vec<SpineEntry>,
    pub(crate) toc: Vec<TocEntry>,
    pub(crate) current_chapter: usize,
    pub(crate) progress_percent: u32,
}

/// 遞迴走訪 `doc.toc`（`epub` crate 從 `toc.ncx` 解析出來的巢狀
/// `NavPoint` 樹），把每個節點轉成 `TocEntry`——巢狀的子節點（通常是同一個
/// 章節內的子標題）攤平成同一份清單，不做縮排分層，跳章節用不到那麼細的
/// 結構。`resolve` 負責把 `NavPoint.content`（章節內部資源路徑，可能帶
/// `#anchor` 片段，例如指向同一個檔案內的某個子標題）轉成 `spine` 的
/// index；片段拿掉再查是因為 `resources`/`spine` 記錄的都是檔案路徑本身，
/// 不含 `#anchor`，帶著片段查一定查不到。查不到的節點（理論上不該發生，
/// 但目錄壞掉的 epub 現實中真的存在）直接跳過，不中斷其餘節點的處理。
fn flatten_toc(points: &[NavPoint], resolve: &impl Fn(&Path) -> Option<usize>, out: &mut Vec<TocEntry>) {
    for point in points {
        let content = point.content.to_string_lossy();
        let path_only = content.split('#').next().unwrap_or(&content);
        if let Some(chapter) = resolve(Path::new(path_only)) {
            let label = point.label.trim();
            if !label.is_empty() {
                out.push(TocEntry { label: label.to_string(), chapter });
            }
        }
        flatten_toc(&point.children, resolve, out);
    }
}

/// 沒有目錄（`toc.ncx` 缺失或解析不出東西，現實中真的有這種 epub）時的
/// 退路：直接拿 `spine` 順序生成「第 N 章」這種通用標題，讓目錄面板永遠
/// 有東西可以選，不會因為某本書的目錄資料不完整就整個開天窗。
fn default_toc(spine_len: usize) -> Vec<TocEntry> {
    (0..spine_len).map(|i| TocEntry { label: format!("第 {} 章", i + 1), chapter: i }).collect()
}

fn build_toc(doc: &Doc, spine_len: usize) -> Vec<TocEntry> {
    let mut entries = Vec::new();
    flatten_toc(&doc.toc, &|path| doc.resource_uri_to_chapter(&path.to_path_buf()), &mut entries);
    if entries.is_empty() {
        default_toc(spine_len)
    } else {
        entries
    }
}

fn progress_percent(current_chapter: usize, total_chapters: usize) -> u32 {
    if total_chapters == 0 {
        return 0;
    }
    (((current_chapter + 1) as f64 / total_chapters as f64) * 100.0).round() as u32
}

/// 開一本書、湊出標題/作者/翻頁方向/章節清單/目前進度——CLI 的 `list`/
/// `open`/`next`/`prev` 跟網頁的 `/meta` 端點都靠這個。每次呼叫都重新開檔
/// 解析，不快取：epub 解析是本機檔案 I/O，成本低，不需要像 `weather` 那樣
/// 為了省網路成本另外做快取層。
pub(crate) fn book_meta(name: &str) -> Result<BookMeta> {
    let path = safe_ebook_path(name).context("不合法的檔名")?;
    let mut doc = open_book(&path)?;
    let title = doc.get_title().unwrap_or_else(|| name.to_string());
    let author = doc.mdata("creator").map(|item| item.value.clone());
    let direction = page_progression_direction(&mut doc);
    let spine: Vec<SpineEntry> = doc
        .spine
        .iter()
        .filter_map(|item| doc.resources.get(&item.idref).map(|r| SpineEntry { href: r.path.to_string_lossy().into_owned() }))
        .collect();
    let toc = build_toc(&doc, spine.len());
    let current_chapter = chapter_progress(name).min(spine.len().saturating_sub(1));
    let percent = progress_percent(current_chapter, spine.len());
    Ok(BookMeta { title, author, direction, spine, toc, current_chapter, progress_percent: percent })
}

/// 給 `web.rs` 的 `/api/ereader/book/{name}/resource/{path}` 用：回傳
/// `(原始位元組, mime type)`。這個端點的 URL 路徑刻意跟 epub 內部路徑一樣，
/// 章節 XHTML 裡原有的相對路徑（圖片/CSS/字型）瀏覽器會自動解析到對應的
/// `/resource/...` URL，不需要重寫任何 HTML 內容。任何一步查不到都回傳
/// `None`，不 panic。
pub(crate) fn book_resource(name: &str, resource_path: &str) -> Option<(Vec<u8>, String)> {
    let path = safe_ebook_path(name)?;
    let mut doc = open_book(&path).ok()?;
    let bytes = doc.get_resource_by_path(resource_path)?;
    let mime = doc.get_resource_mime_by_path(resource_path).unwrap_or_else(|| "application/octet-stream".to_string());
    Some((bytes, mime))
}

pub(crate) fn book_cover(name: &str) -> Option<(Vec<u8>, String)> {
    let path = safe_ebook_path(name)?;
    let mut doc = open_book(&path).ok()?;
    doc.get_cover()
}

/// 章節 HTML 插入這段 CSS，讓瀏覽器自動把整章內容排成一頁一頁。`vertical`
/// 是這本書是不是直式（`writing-mode: vertical-rl`，見 `book_direction`），
/// 兩種書要用完全不同的做法：
///
/// - **橫式**：`column-width: 100vw` 配合固定的 `height: 100vh`、
///   `column-fill: auto`（多欄依序填滿、不是平均分配到每欄——這樣才是
///   「第一頁滿了才排第二頁」的真正分頁行為，不是幾欄並排擠在一起）。
///   CSS 多欄排版的 `column-width` 是依「行進方向」（inline 軸）定義的，
///   橫式文字的行進方向是水平，剛好對應到我們想要的「每欄一個畫面寬」。
/// - **直式**：完全不套用任何 `column-*` 屬性——直式書的行進方向（inline
///   軸）是垂直，同一組 `column-width: 100vw` 套在直式書身上，瀏覽器會
///   解讀成「每欄一整個畫面高、寬度窄到只容得下一行直書文字」，等於把
///   每一行文字都拆成獨立一欄，跟「一頁一個畫面」完全對不上——這正是
///   直式書「最左邊那一行被截斷、翻頁行為整個錯亂」的真正原因。直式書
///   根本不需要多欄排版：文字寫滿一個畫面高就自然往左換下一行，這個
///   行為本身就會把內容往寬度方向延伸，跟橫式書靠多欄排版做到的事效果
///   一樣，只要把高度鎖住（`height: 100vh; overflow: hidden;`）就有這個
///   效果。
///
/// 兩種書都一樣故意不設 `width`：無論靠多欄排版（橫式）還是靠直式書自己
/// 的換行機制（直式），都要讓內容能「長」出超過一個畫面寬度的額外內容
/// 才能形成後面的頁面，如果連 `body` 自己的寬度都釘死成 `100vw`，等於不
/// 准它比一個畫面寬，超過一頁的章節會被截斷在原地，只有第一頁排得出來。
///
/// 真正的翻頁動作在前端：外層頁面（不是這個 iframe 裡面，iframe 本身禁止
/// 跑 script）直接把 iframe 內容往左/右捲動。`vw`/`vh` 單位讓分頁尺寸自動
/// 跟著視窗（面板）大小走——面板被拖拉縮放時瀏覽器自己重新排版，不需要
/// 額外的 JS 介入去重新計算欄寬。插入位置找 `</head>`（不分大小寫），
/// 找不到就原封不動回傳——不是每個 epub 的 XHTML 檔案格式都完全規範，
/// 找不到就不冒然亂塞位置，避免弄壞原本能正常顯示的內容。
pub(crate) fn inject_pagination_style(html: &str, vertical: bool) -> String {
    let body_rule = if vertical {
        "body { margin: 0 !important; height: 100vh !important; overflow: hidden !important; padding: 24px !important; box-sizing: border-box !important; }\n"
    } else {
        "body { margin: 0 !important; height: 100vh !important; overflow: hidden !important; column-width: 100vw !important; column-gap: 24px !important; column-fill: auto !important; padding: 24px !important; box-sizing: border-box !important; }\n"
    };
    let style = format!(
        "<style>\n\
html {{ margin: 0 !important; padding: 0 !important; height: 100vh !important; overflow: hidden !important; }}\n\
{body_rule}\
img, svg, table {{ max-width: 100% !important; }}\n\
</style>\n"
    );
    let Some(pos) = html.to_ascii_lowercase().find("</head>") else {
        return html.to_string();
    };
    let mut out = String::with_capacity(html.len() + style.len());
    out.push_str(&html[..pos]);
    out.push_str(&style);
    out.push_str(&html[pos..]);
    out
}

/// `name` 這本書是不是直式（`page-progression-direction="rtl"`，見
/// `page_progression_direction`）——`web.rs` 的 `/resource/{path}` 端點決定
/// 要不要注入直式版的分頁 CSS（見 `inject_pagination_style`）用這個判斷，
/// 不用像 `book_meta` 那樣把整本書的標題/作者/章節清單都解析一次，只開檔
/// 讀這一個屬性即可。查不到（檔名不合法、開檔失敗）就當作橫式書處理，
/// 跟 `book_meta` 抓不到就退回檔名當標題的容錯邏輯一致。
///
/// 這個屬性只用來決定「翻頁時 spine 往前還是往後走」，跟文字是不是直式排版
/// 是兩件不相關的事——不是每本直式書的 OPF 都會設這個屬性（reader 軟體不一
/// 定依賴它，很多只靠 CSS 的 `writing-mode` 就能讓瀏覽器直式渲染），所以
/// 這個函式**不能**單獨拿來判斷要不要注入直式分頁 CSS，那個判斷要用
/// `chapter_is_vertical`，直接看章節實際套用的 `writing-mode`。
pub(crate) fn book_direction(name: &str) -> &'static str {
    let Some(path) = safe_ebook_path(name) else {
        return "ltr";
    };
    let Ok(mut doc) = open_book(&path) else {
        return "ltr";
    };
    page_progression_direction(&mut doc)
}

/// 掃 `writing-mode`/`-epub-writing-mode` 屬性宣告，值裡有沒有含
/// `vertical`（`vertical-rl`/`vertical-lr`）。單純字串掃描，跟
/// `normalize_vertical_css` 同一套「不需要真的解析 CSS」的理由——這個
/// 屬性的值只會是幾個固定字。
fn css_declares_vertical(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    for marker in ["writing-mode", "-epub-writing-mode"] {
        let mut search_from = 0;
        while let Some(rel_pos) = lower[search_from..].find(marker) {
            let pos = search_from + rel_pos;
            let after_marker = &lower[pos + marker.len()..];
            let Some(colon_rel) = after_marker.find(':') else {
                break;
            };
            let after_colon = &after_marker[colon_rel + 1..];
            let end = after_colon.find([';', '"', '\'', '}']).unwrap_or(after_colon.len());
            if after_colon[..end].contains("vertical") {
                return true;
            }
            search_from = pos + marker.len() + colon_rel + 1 + end;
        }
    }
    false
}

/// 從 `<link rel="stylesheet" href="...">` 標籤挖出所有外部 CSS 的 `href`
/// （原始相對路徑，還沒解析成 epub 內部絕對路徑）。
fn extract_stylesheet_hrefs(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut hrefs = Vec::new();
    let mut search_from = 0;
    while let Some(rel_pos) = lower[search_from..].find("<link") {
        let tag_start = search_from + rel_pos;
        let Some(tag_end_rel) = lower[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + tag_end_rel;
        if lower[tag_start..=tag_end].contains("stylesheet")
            && let Some(href) = extract_attr(&html[tag_start..=tag_end], &lower[tag_start..=tag_end], "href")
        {
            hrefs.push(href);
        }
        search_from = tag_end + 1;
    }
    hrefs
}

fn extract_attr(tag: &str, tag_lower: &str, attr: &str) -> Option<String> {
    let marker = format!("{attr}=");
    let pos = tag_lower.find(&marker)? + marker.len();
    let rest = &tag[pos..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let end = rest[1..].find(quote)?;
    Some(rest[1..1 + end].to_string())
}

/// 把章節 XHTML 裡的相對路徑（`href="../Styles/style.css"` 之類）解析成
/// epub 內部的絕對資源路徑，這樣才能拿去餵 `doc.get_resource_by_path`——
/// 跟瀏覽器解析 `<link href>` 相對於當前文件路徑的規則一樣，用
/// `chapter_path` 所在的目錄當作基準目錄，`..`/`.` 自己處理掉（`PathBuf`
/// 的 `join` 不會自動正規化這些片段）。
fn resolve_relative_resource_path(chapter_path: &str, href: &str) -> Option<String> {
    let href = href.split(['?', '#']).next().unwrap_or(href);
    if href.is_empty() {
        return None;
    }
    let base_dir = Path::new(chapter_path).parent().unwrap_or_else(|| Path::new(""));
    let joined = base_dir.join(href);
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::CurDir => {}
            std::path::Component::Normal(seg) => parts.push(seg),
            _ => {}
        }
    }
    let mut result = PathBuf::new();
    for part in parts {
        result.push(part);
    }
    Some(result.to_string_lossy().replace('\\', "/"))
}

/// `web.rs` 的 `/resource/{path}` 端點用這個判斷章節要不要套用直式分頁
/// CSS——直接看章節本身（inline `<style>`）跟它透過 `<link>` 引用的外部
/// CSS 有沒有宣告 `writing-mode: vertical-*`，不透過 `book_direction`
/// 那種間接推論（該屬性管的是翻頁方向，不是排版方向，見它的說明）。查不到
/// 任何直式宣告才退回用 `book_direction`（`rtl` 的書多半也是直式）當保底，
/// 兩者都查不到才當橫式書處理。
pub(crate) fn chapter_is_vertical(name: &str, chapter_path: &str, html: &str) -> bool {
    if css_declares_vertical(html) {
        return true;
    }
    if let Some(path) = safe_ebook_path(name)
        && let Ok(mut doc) = open_book(&path)
    {
        for href in extract_stylesheet_hrefs(html) {
            let Some(resolved) = resolve_relative_resource_path(chapter_path, &href) else {
                continue;
            };
            if let Some(bytes) = doc.get_resource_by_path(&resolved)
                && let Ok(css) = String::from_utf8(bytes)
                && css_declares_vertical(&css)
            {
                return true;
            }
        }
    }
    book_direction(name) == "rtl"
}

/// 舊格式（多半是 iBooks 匯出的）直式書用 Apple 自家的 `-epub-writing-mode`
/// vendor prefix，現代瀏覽器不認得這個舊寫法，直式排版會整個跑掉。偵測到
/// 就在後面多補一份一模一樣的值、改成標準的 `writing-mode`——原本的
/// vendor prefix 保留不動（以防真有某個舊閱讀器只認得那個寫法），單純字串
/// 掃描，不需要真的解析 CSS：這個屬性的值只會是 `vertical-rl`/
/// `horizontal-tb` 這幾個固定字，不是需要語法分析的複雜表達式。
pub(crate) fn normalize_vertical_css(css: &str) -> String {
    const OLD_PROP: &str = "-epub-writing-mode";
    if !css.contains(OLD_PROP) {
        return css.to_string();
    }
    let mut out = String::with_capacity(css.len() + 64);
    let mut rest = css;
    while let Some(pos) = rest.find(OLD_PROP) {
        out.push_str(&rest[..pos]);
        out.push_str(OLD_PROP);
        rest = &rest[pos + OLD_PROP.len()..];
        let Some(colon) = rest.find(':') else {
            break;
        };
        let after_colon = &rest[colon + 1..];
        let end = after_colon.find(';').map(|i| i + 1).unwrap_or(after_colon.len());
        let value_decl = &after_colon[..end];
        out.push(':');
        out.push_str(value_decl);
        out.push_str("writing-mode:");
        out.push_str(value_decl);
        rest = &after_colon[end..];
    }
    out.push_str(rest);
    out
}

/// 把章節的 XHTML 內容轉成純文字，給 CLI/GUI 的 panel 顯示（終端機沒有
/// HTML 引擎，只能剝掉標籤留文字）。不追求完全還原排版，只求可讀：段落/
/// 標題/列表項目這類區塊標籤轉成換行，其餘標籤直接丟掉，順手解幾個最常見
/// 的 HTML entity，其他 entity 沒特別處理、保留原樣。
pub(crate) fn strip_html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut tag_buf = String::new();
    for ch in html.chars() {
        match ch {
            '<' => {
                in_tag = true;
                tag_buf.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let tag_name = tag_buf
                    .trim()
                    .trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let block_tag =
                    matches!(tag_name.as_str(), "p" | "br" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "li" | "tr");
                if block_tag {
                    out.push('\n');
                }
            }
            _ if in_tag => tag_buf.push(ch),
            _ => out.push(ch),
        }
    }
    let out =
        out.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'");
    // HTML 原始碼裡的縮排/換行、加上區塊標籤各自補的換行（例如相鄰的
    // `<p>...</p><p>...</p>` 開始標籤跟結束標籤各補一次，兩個換行疊在一起）
    // 會留下大量空白行，逐行 trim 之後整段丟掉，不保留任何空行——這個函式
    // 只求把文字接起來給人看，不是要還原原始排版的段落間距。
    let lines: Vec<&str> = out.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    lines.join("\n").trim().to_string()
}

#[derive(Clone, Serialize)]
pub(crate) struct BookSummary {
    pub(crate) name: String,
    pub(crate) title: String,
    pub(crate) author: Option<String>,
    pub(crate) progress_percent: u32,
}

/// 列出 `EBOOK_DIR` 底下目前有的 `.epub` 檔案，各自帶標題/作者/目前進度%。
/// 資料夾還不存在、或某本書解析失敗都不報錯——資料夾不存在當作空清單（跟
/// `music_files` 判斷 `music/` 資料夾不存在時的容錯邏輯一致），單本書解析
/// 失敗就退回用檔名當標題，不讓一本壞掉的書拖累整份清單顯示。
pub(crate) fn list_books() -> Vec<BookSummary> {
    let Ok(entries) = fs::read_dir(EBOOK_DIR) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.to_ascii_lowercase().ends_with(".epub"))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| match book_meta(&name) {
            Ok(meta) => BookSummary { name, title: meta.title, author: meta.author, progress_percent: meta.progress_percent },
            Err(_) => BookSummary { title: name.clone(), name, author: None, progress_percent: 0 },
        })
        .collect()
}

/// CLI/GUI session 目前焦點在哪一本書——跟 `chapter_progress`/
/// `save_chapter_progress` 存的「讀到第幾章」是分開的兩件事：這個只是
/// 「這個終端機 session 現在在讀哪本」，換一本書、或程式重啟後就沒了，
/// 不用持久化（跟 `WolPlugin` 的 `devices` 記在記憶體裡同一種考量）。
pub struct EReaderPlugin {
    #[allow(dead_code)]
    ctx: SharedContext,
    current_book: Option<String>,
}

impl EReaderPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx, current_book: None }
    }

    fn list_text() -> String {
        let books = list_books();
        if books.is_empty() {
            return format!("({EBOOK_DIR}/ 底下還沒有任何 .epub 檔案，先用 storage plugin 的網頁介面上傳)");
        }
        books
            .iter()
            .map(|b| {
                let author = b.author.as_ref().map(|a| format!(" by {a}")).unwrap_or_default();
                format!("{}｜{}{}（{}%）", b.name, b.title, author, b.progress_percent)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn list(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push(&format!("{}\n", Self::list_text()));
        Ok(())
    }

    fn open(&mut self, args: &[String], out: &OutputBuffer) -> Result<()> {
        let name = args.join(" ");
        if name.is_empty() {
            bail!("open 需要接書名（檔名）");
        }
        let meta = book_meta(&name).with_context(|| format!("打不開 {name}"))?;
        self.current_book = Some(name);
        let total = meta.spine.len().max(1);
        out.push(&format!(
            "開始讀「{}」，目前第 {}/{total} 章（{}%）\n",
            meta.title,
            meta.current_chapter + 1,
            meta.progress_percent
        ));
        Ok(())
    }

    fn offset_chapter(&mut self, delta: i64, out: &OutputBuffer) -> Result<()> {
        let name = self.current_book.clone().context("還沒開始讀任何書，先用 open <書名>")?;
        let meta = book_meta(&name)?;
        let total = meta.spine.len();
        if total == 0 {
            bail!("這本書沒有任何章節");
        }
        let next = (meta.current_chapter as i64 + delta).clamp(0, total as i64 - 1) as usize;
        save_chapter_progress(&name, next)?;
        let percent = progress_percent(next, total);
        out.push(&format!("第 {}/{total} 章（{percent}%）\n", next + 1));
        Ok(())
    }

    /// `panel_text()` 用：還沒開始讀任何書就顯示書架清單，開始讀了就顯示
    /// 目前這一章的純文字內容（見 `strip_html_to_text`）。
    fn reading_text(&self) -> String {
        let Some(name) = &self.current_book else {
            return Self::list_text();
        };
        let Ok(meta) = book_meta(name) else {
            return format!("讀取「{name}」失敗\n\n{}", Self::list_text());
        };
        let Some(entry) = meta.spine.get(meta.current_chapter) else {
            return "(這本書沒有任何章節)".to_string();
        };
        let content = book_resource(name, &entry.href)
            .and_then(|(bytes, _)| String::from_utf8(bytes).ok())
            .map(|html| strip_html_to_text(&html))
            .unwrap_or_else(|| "(讀取章節內容失敗)".to_string());
        format!("{} - 第 {}/{} 章（{}%）\n\n{}", meta.title, meta.current_chapter + 1, meta.spine.len(), meta.progress_percent, content)
    }
}

impl Plugin for EReaderPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["list", "open <書名>", "next", "prev"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "list" => self.list(out),
            "open" => self.open(args, out),
            "next" => self.offset_chapter(1, out),
            "prev" => self.offset_chapter(-1, out),
            other => bail!("ereader 不認得指令: {other}"),
        }
    }

    fn panel_text(&self) -> Option<String> {
        Some(self.reading_text())
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
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    /// `EBOOK_DIR`/`PROGRESS_FILE` 是相對於目前工作目錄的相對路徑，測試不能
    /// 直接讓它落在真正的 `storage/`（會弄髒開發者自己機器上的資料，
    /// `cargo test` 平行跑測試時彼此也會互相踩到）——跟 `todo.rs`/
    /// `storage.rs` 測試用的 `CwdGuard` 同一招：整個測試期間切到一個獨立的
    /// 暫存工作目錄，結束（含 panic）一定會切回來。
    static CWD_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct CwdGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        original: PathBuf,
    }

    impl CwdGuard {
        fn enter(workdir: &Path) -> Self {
            let lock = CWD_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let original = std::env::current_dir().expect("讀取目前工作目錄失敗");
            std::env::set_current_dir(workdir).expect("切換測試用工作目錄失敗");
            Self { _lock: lock, original }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    #[test]
    fn safe_ebook_path_rejects_traversal_and_separators() {
        assert!(safe_ebook_path("").is_none());
        assert!(safe_ebook_path(".").is_none());
        assert!(safe_ebook_path("..").is_none());
        assert!(safe_ebook_path("a/b.epub").is_none());
        assert!(safe_ebook_path("a\\b.epub").is_none());
        assert_eq!(safe_ebook_path("book.epub"), Some(Path::new(EBOOK_DIR).join("book.epub")));
    }

    #[test]
    fn extract_book_title_finds_first_bracketed_title_and_skips_site_name() {
        let html = r#"<font color="CC0000">倪匡</font>《少年衛斯理》<br>連結到《好讀》首頁"#;
        assert_eq!(extract_book_title(html), Some("少年衛斯理".to_string()));
        assert_eq!(extract_book_title("沒有書名標記的頁面"), None);
        assert_eq!(extract_book_title("《好讀》"), None);
    }

    #[test]
    fn default_toc_generates_generic_labels_from_spine_length() {
        let toc = default_toc(3);
        assert_eq!(toc.len(), 3);
        assert_eq!(toc[0].label, "第 1 章");
        assert_eq!(toc[0].chapter, 0);
        assert_eq!(toc[2].label, "第 3 章");
        assert_eq!(toc[2].chapter, 2);
        assert!(default_toc(0).is_empty());
    }

    #[test]
    fn flatten_toc_resolves_content_paths_and_strips_anchor_fragments() {
        let points = vec![
            NavPoint { label: " 第一章 ".to_string(), content: PathBuf::from("OEBPS/c1.xhtml"), children: vec![], play_order: Some(1) },
            NavPoint {
                label: "第二章".to_string(),
                // 目錄項目常常帶 `#anchor` 指向同一個檔案內的子標題，查
                // spine index 要先把這段去掉，不然一定查不到。
                content: PathBuf::from("OEBPS/c2.xhtml#section2"),
                children: vec![],
                play_order: Some(2),
            },
        ];
        let resolve = |path: &Path| match path.to_str() {
            Some("OEBPS/c1.xhtml") => Some(0),
            Some("OEBPS/c2.xhtml") => Some(1),
            _ => None,
        };
        let mut out = Vec::new();
        flatten_toc(&points, &resolve, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].label, "第一章");
        assert_eq!(out[0].chapter, 0);
        assert_eq!(out[1].label, "第二章");
        assert_eq!(out[1].chapter, 1);
    }

    #[test]
    fn flatten_toc_flattens_nested_children_and_skips_unresolvable_entries() {
        let points = vec![NavPoint {
            label: "父章節".to_string(),
            content: PathBuf::from("OEBPS/parent.xhtml"),
            play_order: Some(1),
            children: vec![
                NavPoint { label: "子小節".to_string(), content: PathBuf::from("OEBPS/parent.xhtml#sub"), children: vec![], play_order: Some(2) },
                NavPoint { label: "查無此檔".to_string(), content: PathBuf::from("OEBPS/missing.xhtml"), children: vec![], play_order: Some(3) },
            ],
        }];
        let resolve = |path: &Path| if path.to_str() == Some("OEBPS/parent.xhtml") { Some(0) } else { None };
        let mut out = Vec::new();
        flatten_toc(&points, &resolve, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].label, "父章節");
        assert_eq!(out[1].label, "子小節");
        assert_eq!(out[1].chapter, 0);
    }

    #[test]
    fn sanitize_ebook_filename_replaces_path_separators() {
        assert_eq!(sanitize_ebook_filename("上/下集"), "上_下集");
        assert_eq!(sanitize_ebook_filename("a\\b"), "a_b");
        assert_eq!(sanitize_ebook_filename("正常書名"), "正常書名");
    }

    #[test]
    fn haodoo_import_rejects_non_haodoo_host() {
        let err = haodoo_import("https://evil.example.com/?M=book&P=1", false).unwrap_err();
        assert!(err.to_string().contains("haodoo.net"));
    }

    #[test]
    #[ignore = "手動驗證用，會真的打 haodoo.net，不放進一般 CI/測試流程"]
    fn haodoo_import_real_network_smoke_test() {
        let dir = std::env::temp_dir().join("cng5-ereader-test-haodoo");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(EBOOK_DIR)).expect("建立測試用暫存目錄失敗");
        let _guard = CwdGuard::enter(&dir);
        let name = haodoo_import("https://haodoo.net/?M=book&P=13H8", false).expect("下載失敗");
        assert!(Path::new(EBOOK_DIR).join(&name).exists());
        let name_v = haodoo_import("https://haodoo.net/?M=book&P=13H8", true).expect("直式下載失敗");
        assert!(Path::new(EBOOK_DIR).join(&name_v).exists());
    }

    #[test]
    fn progress_round_trips_across_reload() {
        let dir = std::env::temp_dir().join("cng5-ereader-test-progress");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建立測試用暫存目錄失敗");
        let _guard = CwdGuard::enter(&dir);

        assert_eq!(chapter_progress("book.epub"), 0);
        save_chapter_progress("book.epub", 3).unwrap();
        assert_eq!(chapter_progress("book.epub"), 3);
        save_chapter_progress("book.epub", 5).unwrap();
        assert_eq!(chapter_progress("book.epub"), 5);
        assert_eq!(chapter_progress("other.epub"), 0);
    }

    #[test]
    fn progress_percent_matches_expected_rounding() {
        assert_eq!(progress_percent(0, 0), 0);
        assert_eq!(progress_percent(0, 4), 25);
        assert_eq!(progress_percent(3, 4), 100);
        assert_eq!(progress_percent(1, 3), 67);
    }

    #[test]
    fn normalize_vertical_css_adds_standard_property_next_to_vendor_prefix() {
        let css = "body { -epub-writing-mode: vertical-rl; color: red; }";
        let normalized = normalize_vertical_css(css);
        assert!(normalized.contains("-epub-writing-mode: vertical-rl;"));
        assert!(normalized.contains("writing-mode: vertical-rl;"));
        assert!(normalized.contains("color: red;"));
    }

    #[test]
    fn normalize_vertical_css_leaves_unrelated_css_untouched() {
        let css = "body { color: red; }";
        assert_eq!(normalize_vertical_css(css), css);
    }

    #[test]
    fn inject_pagination_style_adds_style_before_head_close() {
        let html = "<html><head><title>x</title></head><body>hi</body></html>";
        let out = inject_pagination_style(html, false);
        assert!(out.contains("column-width"));
        assert!(out.find("<style>").unwrap() < out.find("</head>").unwrap());
        // 原本的內容都還在，只是多插入一段 `<style>`，不是取代掉什麼。
        assert!(out.contains("<title>x</title>"));
        assert!(out.contains("<body>hi</body>"));
    }

    #[test]
    fn inject_pagination_style_is_case_insensitive_for_head_close_tag() {
        let html = "<HTML><HEAD></HEAD><body>hi</body></HTML>";
        let out = inject_pagination_style(html, false);
        assert!(out.contains("column-width"));
    }

    #[test]
    fn inject_pagination_style_leaves_html_untouched_without_head_close() {
        let html = "<body>no head here</body>";
        assert_eq!(inject_pagination_style(html, false), html);
    }

    #[test]
    fn inject_pagination_style_skips_column_properties_for_vertical_books() {
        // 直式書不能套用 `column-width`：CSS 多欄排版是依「行進方向」（inline
        // 軸）定義寬度，直式書的行進方向是垂直，同一組屬性套上去會把每一行
        // 文字拆成獨立一欄，見函式說明。
        let html = "<html><head></head><body>hi</body></html>";
        let out = inject_pagination_style(html, true);
        assert!(!out.contains("column-width"));
        assert!(!out.contains("column-fill"));
        assert!(out.contains("height: 100vh"));
    }

    #[test]
    fn css_declares_vertical_detects_standard_and_vendor_prefixed_property() {
        assert!(css_declares_vertical("body { writing-mode: vertical-rl; }"));
        assert!(css_declares_vertical("body { -epub-writing-mode: vertical-rl; }"));
        assert!(!css_declares_vertical("body { writing-mode: horizontal-tb; }"));
        assert!(!css_declares_vertical("body { color: red; }"));
    }

    #[test]
    fn extract_stylesheet_hrefs_finds_only_stylesheet_links() {
        let html = r#"<head><link rel="stylesheet" href="../Styles/style.css"/><link rel="icon" href="cover.png"/></head>"#;
        assert_eq!(extract_stylesheet_hrefs(html), vec!["../Styles/style.css".to_string()]);
    }

    #[test]
    fn resolve_relative_resource_path_normalizes_parent_segments() {
        assert_eq!(
            resolve_relative_resource_path("OEBPS/Text/chapter1.xhtml", "../Styles/style.css"),
            Some("OEBPS/Styles/style.css".to_string())
        );
        assert_eq!(resolve_relative_resource_path("OEBPS/Text/chapter1.xhtml", "style.css"), Some("OEBPS/Text/style.css".to_string()));
    }

    #[test]
    fn chapter_is_vertical_detects_inline_style_without_relying_on_opf() {
        // 章節自己內嵌 `<style>` 宣告直式，就算書名/OPF 都查不到（不合法檔名
        // 直接回 false 分支的 `book_direction` 保底），也要偵測成直式——這是
        // 這次修的重點：不能只靠 `page-progression-direction` 這種間接、非
        // 必填的屬性判斷排版方向。
        let html = "<html><head><style>body{writing-mode:vertical-rl;}</style></head><body>x</body></html>";
        assert!(chapter_is_vertical("not a valid name/", "chapter1.xhtml", html));
    }

    #[test]
    fn chapter_is_vertical_false_when_no_signal_present() {
        let html = "<html><head></head><body>x</body></html>";
        assert!(!chapter_is_vertical("not a valid name/", "chapter1.xhtml", html));
    }

    #[test]
    fn strip_html_to_text_keeps_paragraphs_and_decodes_entities() {
        let html = "<p>Hello &amp; welcome</p><p>Second line</p>";
        let text = strip_html_to_text(html);
        assert_eq!(text, "Hello & welcome\nSecond line");
    }

    #[test]
    fn dispatch_offset_chapter_without_open_errors() {
        let dir = std::env::temp_dir().join("cng5-ereader-test-noopen");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建立測試用暫存目錄失敗");
        let _guard = CwdGuard::enter(&dir);

        let mut plugin = EReaderPlugin::new(Arc::new(Mutex::new(crate::plugin::ContextInner::default())));
        let out = OutputBuffer::new();
        assert!(plugin.dispatch("next", &[], &out).is_err());
        assert!(plugin.dispatch("prev", &[], &out).is_err());
    }

    #[test]
    fn dispatch_unknown_command_errors() {
        let mut plugin = EReaderPlugin::new(Arc::new(Mutex::new(crate::plugin::ContextInner::default())));
        let out = OutputBuffer::new();
        let err = plugin.dispatch("bogus", &[], &out).unwrap_err();
        assert!(err.to_string().contains("ereader 不認得指令"));
    }

    #[test]
    fn list_text_without_any_book_shows_placeholder() {
        let dir = std::env::temp_dir().join("cng5-ereader-test-emptylist");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建立測試用暫存目錄失敗");
        let _guard = CwdGuard::enter(&dir);

        assert!(EReaderPlugin::list_text().contains("還沒有任何 .epub 檔案"));
    }
}
