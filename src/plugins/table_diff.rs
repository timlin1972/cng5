#![allow(dead_code)]

use std::collections::HashMap;
use std::time::Instant;

use unicode_width::UnicodeWidthStr;

/// 一個表格欄位目前的文字，以及「最近一次偵測到跟上一輪不一樣」的時間點。
/// 因為 `RowDiffTracker` 把「這個 row 是第一次出現」也算進「有變化」，一個
/// 出現在 `TableSnapshot` 裡的格子必定至少變化過一次（就是它被建立的那一
/// 刻），所以這裡不用 `Option<Instant>`。
pub(crate) struct TableCell {
    pub text: String,
    pub changed_at: Instant,
}

pub(crate) struct TableRow {
    pub cells: Vec<TableCell>,
}

/// 一次「目前這個 viewer 看到的表格長什麼樣子」。`computed_at` 是這次呼叫
/// `RowDiffTracker::snapshot` 當下的時間，用來判斷哪些格子是「這一次剛好
/// 變化」（給 web 的 `changed` 布林值用，見 `to_json`）；TUI 端要的是持續
/// 淡出的效果，直接用每個 `TableCell::changed_at.elapsed()`，不需要
/// `computed_at`。
pub(crate) struct TableSnapshot {
    pub headers: Vec<String>,
    pub rows: Vec<TableRow>,
    computed_at: Instant,
}

impl TableSnapshot {
    /// 依目前內容算出每欄要多寬（跟表頭/所有格子裡最寬的那個看齊），給 TUI
    /// 逐格畫的時候組固定寬度用，跟 `global.rs`/`device.rs` 原本
    /// `render_table` 的排版邏輯一致。
    pub(crate) fn column_widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = self.headers.iter().map(|h| UnicodeWidthStr::width(h.as_str())).collect();
        for row in &self.rows {
            for (width, cell) in widths.iter_mut().zip(&row.cells) {
                *width = (*width).max(UnicodeWidthStr::width(cell.text.as_str()));
            }
        }
        widths
    }

    /// 轉成給 web 用的 JSON 可序列化版本：`Instant` 不能序列化，改成單純的
    /// `changed` 布林值——`true` 代表這一格在這次 snapshot 裡剛好被偵測到
    /// 變化（`changed_at` 剛好等於這次呼叫的 `computed_at`）。
    pub(crate) fn to_json(&self) -> JsonTableSnapshot {
        JsonTableSnapshot {
            headers: self.headers.clone(),
            rows: self
                .rows
                .iter()
                .map(|row| JsonTableRow {
                    cells: row
                        .cells
                        .iter()
                        .map(|cell| JsonTableCell {
                            text: cell.text.clone(),
                            changed: cell.changed_at == self.computed_at,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

#[derive(serde::Serialize)]
pub(crate) struct JsonTableSnapshot {
    pub headers: Vec<String>,
    pub rows: Vec<JsonTableRow>,
}

#[derive(serde::Serialize)]
pub(crate) struct JsonTableRow {
    pub cells: Vec<JsonTableCell>,
}

#[derive(serde::Serialize)]
pub(crate) struct JsonTableCell {
    pub text: String,
    pub changed: bool,
}

/// 記錄「這個 viewer 上一次看到的表格長什麼樣子」，每次呼叫 `snapshot()`
/// 拿目前資料跟這份記錄比對，算出每一格是不是「剛好變了」，再把記錄整個
/// 換成這一次的內容。
///
/// `global`／`device` 這兩個 plugin 各自需要兩份獨立的 `RowDiffTracker`
/// （一份給 TUI、一份給 web）：TUI 最快 200ms 重繪一次、web 固定 300ms 一個
/// tick，如果共用同一份記錄，先讀到變化的那一邊會把記錄更新掉，另一邊接著
/// 讀就會誤判成「沒變」而漏掉閃爍效果。
#[derive(Default)]
pub(crate) struct RowDiffTracker {
    rows: HashMap<String, Vec<(String, Instant)>>,
}

impl RowDiffTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// `headers` 是欄位名稱、`rows` 是「這次」的 (row key, 每一欄的文字)。
    /// 只要某個 key 這次沒出現在輸入裡，它在下一次呼叫用的記錄裡就會消失
    /// ——之後同一個 key 重新出現時，等同「第一次出現」，會整列重新算成
    /// 「有變化」，不需要另外處理「row 消失後又出現」這種情況（也涵蓋
    /// `global clear`／裝置離線很久之後資料重新回來的情況）。
    pub(crate) fn snapshot(&mut self, headers: &[&str], rows: Vec<(String, Vec<String>)>) -> TableSnapshot {
        let now = Instant::now();
        let mut next = HashMap::with_capacity(rows.len());
        let mut out_rows = Vec::with_capacity(rows.len());
        for (key, values) in rows {
            let previous = self.rows.get(&key);
            let mut cells = Vec::with_capacity(values.len());
            let mut recorded = Vec::with_capacity(values.len());
            for (i, value) in values.into_iter().enumerate() {
                let changed_at = match previous.and_then(|p| p.get(i)) {
                    Some((prev_value, prev_changed_at)) if *prev_value == value => *prev_changed_at,
                    _ => now,
                };
                recorded.push((value.clone(), changed_at));
                cells.push(TableCell { text: value, changed_at });
            }
            next.insert(key, recorded);
            out_rows.push(TableRow { cells });
        }
        self.rows = next;
        TableSnapshot { headers: headers.iter().map(|h| h.to_string()).collect(), rows: out_rows, computed_at: now }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_appearance_counts_as_changed() {
        let mut tracker = RowDiffTracker::new();
        let snapshot =
            tracker.snapshot(&["a", "b"], vec![("k1".to_string(), vec!["1".to_string(), "2".to_string()])]);
        let json = snapshot.to_json();
        assert_eq!(json.headers, vec!["a", "b"]);
        assert_eq!(json.rows.len(), 1);
        assert!(json.rows[0].cells.iter().all(|c| c.changed));
    }

    #[test]
    fn unchanged_value_keeps_old_changed_at() {
        let mut tracker = RowDiffTracker::new();
        let first = tracker.snapshot(&["a"], vec![("k1".to_string(), vec!["1".to_string()])]);
        let first_changed_at = first.rows[0].cells[0].changed_at;
        let second = tracker.snapshot(&["a"], vec![("k1".to_string(), vec!["1".to_string()])]);
        assert_eq!(second.rows[0].cells[0].changed_at, first_changed_at);
        assert!(!second.to_json().rows[0].cells[0].changed);
    }

    #[test]
    fn only_the_differing_column_updates() {
        let mut tracker = RowDiffTracker::new();
        tracker.snapshot(&["a", "b"], vec![("k1".to_string(), vec!["1".to_string(), "x".to_string()])]);
        let second = tracker.snapshot(&["a", "b"], vec![("k1".to_string(), vec!["1".to_string(), "y".to_string()])]);
        let json = second.to_json();
        assert!(!json.rows[0].cells[0].changed);
        assert!(json.rows[0].cells[1].changed);
    }

    #[test]
    fn row_missing_then_reappearing_counts_as_new() {
        let mut tracker = RowDiffTracker::new();
        tracker.snapshot(&["a"], vec![("k1".to_string(), vec!["1".to_string()])]);
        tracker.snapshot(&["a"], vec![]); // k1 這一輪沒出現，等同消失
        let third = tracker.snapshot(&["a"], vec![("k1".to_string(), vec!["1".to_string()])]);
        assert!(third.to_json().rows[0].cells[0].changed);
    }
}
