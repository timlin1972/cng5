use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};
use crate::sysinfo;

/// 每個數字（`0`-`9`）固定 3 欄 × 5 列的點陣字型，`'1'` 代表填滿、`'0'` 代表留白。
const DIGIT_FONT: [[&str; 5]; 10] = [
    ["111", "101", "101", "101", "111"], // 0
    ["010", "010", "010", "010", "010"], // 1
    ["111", "001", "111", "100", "111"], // 2
    ["111", "001", "111", "001", "111"], // 3
    ["101", "101", "111", "001", "001"], // 4
    ["111", "100", "111", "001", "111"], // 5
    ["111", "100", "111", "101", "111"], // 6
    ["111", "001", "001", "001", "001"], // 7
    ["111", "101", "111", "101", "111"], // 8
    ["111", "101", "111", "001", "111"], // 9
];

/// 冒號亮的時候：1 欄寬、5 列高，只有第 2、4 列（0-indexed 為 1、3）各一個 `█`。
const COLON_ON: [&str; 5] = [" ", "1", " ", "1", " "];
const COLON_OFF: [&str; 5] = [" ", " ", " ", " ", " "];

fn glyph_rows(ch: char, colon_on: bool) -> [String; 5] {
    let bitmap: [&str; 5] = if ch == ':' {
        if colon_on { COLON_ON } else { COLON_OFF }
    } else {
        let digit = ch.to_digit(10).expect("clock 字型只認得 0-9 跟 ':'") as usize;
        DIGIT_FONT[digit]
    };
    bitmap.map(|row| row.replace('1', "█").replace('0', " "))
}

/// 把 `"HH:MM:SS"` 轉成 5 行 ASCII art；冒號依 `colon_on` 決定亮/暗（隱藏時用等寬
/// 的空白取代，不是把那個欄位整個拿掉），glyph 之間補 2 欄空白分隔——不管
/// `hms` 內容是什麼，因為每個 glyph 寬度固定，回傳的每一行字元數都一樣。
pub fn render_hms(hms: &str, colon_on: bool) -> String {
    let glyphs: Vec<[String; 5]> = hms.chars().map(|c| glyph_rows(c, colon_on)).collect();
    (0..5)
        .map(|row| glyphs.iter().map(|g| g[row].as_str()).collect::<Vec<_>>().join("  "))
        .collect::<Vec<_>>()
        .join("\n")
}

const MANUAL_TEXT: &str = "\
clock：panel 用 ASCII 大字顯示現在時間（hh:mm:ss，本地時區、24 小時制，不含日期）。

沒有指令，全部都在 panel 裡自動顯示：
  冒號每秒閃爍一次（雙數秒顯示、單數秒隱藏），數字用固定大小的方塊字型畫，
  不管當下是幾點幾分幾秒，畫出來的寬度都一樣（不會因為數字不同而跳動）。

panel 拉太窄畫不下的話會直接被裁切，不會自動縮小字型或換行。
";

/// 沒有任何狀態（沒有計時器、沒有快取）——`panel_text()` 每次被呼叫都直接讀
/// 當下的系統時間即時算，反正 GUI 本來就每 200ms 重繪一次面板。
pub struct ClockPlugin;

impl ClockPlugin {
    pub fn new(_ctx: SharedContext) -> Self {
        Self
    }
}

impl Plugin for ClockPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &[]
    }

    fn dispatch(&mut self, cmd: &str, _args: &[String], _out: &OutputBuffer) -> Result<()> {
        bail!("clock 不認得指令: {cmd}")
    }

    fn panel_text(&self) -> Option<String> {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let hms = sysinfo::local_hms(ts);
        Some(render_hms(&hms, ts % 2 == 0))
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

    #[test]
    fn digit_zero_matches_expected_bitmap() {
        let rows = glyph_rows('0', true);
        assert_eq!(
            rows,
            ["███".to_string(), "█ █".to_string(), "█ █".to_string(), "█ █".to_string(), "███".to_string()]
        );
    }

    #[test]
    fn digit_one_matches_expected_bitmap() {
        let rows = glyph_rows('1', true);
        assert_eq!(
            rows,
            [" █ ".to_string(), " █ ".to_string(), " █ ".to_string(), " █ ".to_string(), " █ ".to_string()]
        );
    }

    #[test]
    fn digit_seven_matches_expected_bitmap() {
        let rows = glyph_rows('7', true);
        assert_eq!(
            rows,
            ["███".to_string(), "  █".to_string(), "  █".to_string(), "  █".to_string(), "  █".to_string()]
        );
    }

    #[test]
    fn every_digit_glyph_is_three_columns_wide_and_five_rows_tall() {
        for d in '0'..='9' {
            let rows = glyph_rows(d, true);
            assert_eq!(rows.len(), 5);
            for row in rows {
                assert_eq!(row.chars().count(), 3, "digit {d} row {row:?} 不是 3 欄寬");
            }
        }
    }

    #[test]
    fn colon_on_has_two_dots_colon_off_is_blank_same_width() {
        let on = glyph_rows(':', true);
        let off = glyph_rows(':', false);
        assert_eq!(on, [" ".to_string(), "█".to_string(), " ".to_string(), "█".to_string(), " ".to_string()]);
        assert_eq!(off, [" ".to_string(), " ".to_string(), " ".to_string(), " ".to_string(), " ".to_string()]);
    }

    #[test]
    fn width_is_constant_across_different_times() {
        let start_of_day = render_hms("00:00:00", true);
        let end_of_day = render_hms("23:59:59", true);
        let widths_a: Vec<usize> = start_of_day.lines().map(|l| l.chars().count()).collect();
        let widths_b: Vec<usize> = end_of_day.lines().map(|l| l.chars().count()).collect();
        assert_eq!(widths_a, widths_b);
        assert_eq!(widths_a, vec![34, 34, 34, 34, 34]);
        assert_eq!(start_of_day.lines().count(), 5);
    }

    #[test]
    fn colon_blink_only_changes_colon_columns() {
        let on = render_hms("12:34:56", true);
        let off = render_hms("12:34:56", false);
        let on_lines: Vec<Vec<char>> = on.lines().map(|l| l.chars().collect()).collect();
        let off_lines: Vec<Vec<char>> = off.lines().map(|l| l.chars().collect()).collect();
        assert_eq!(on_lines.len(), 5);
        for (row_idx, (on_row, off_row)) in on_lines.iter().zip(off_lines.iter()).enumerate() {
            assert_eq!(on_row.len(), off_row.len());
            let diff_cols = (0..on_row.len()).filter(|&i| on_row[i] != off_row[i]).count();
            if row_idx == 1 || row_idx == 3 {
                assert_eq!(diff_cols, 2, "row {row_idx} 應該剛好有 2 個冒號欄位不同（兩個冒號各一欄）");
            } else {
                assert_eq!(diff_cols, 0, "row {row_idx} 不應該因為冒號閃爍而改變");
            }
        }
    }

    #[test]
    fn dispatch_always_errors() {
        let mut plugin = ClockPlugin::new(std::sync::Arc::new(std::sync::Mutex::new(crate::plugin::ContextInner::default())));
        let out = OutputBuffer::new();
        let err = plugin.dispatch("anything", &[], &out).unwrap_err();
        assert!(err.to_string().contains("clock 不認得指令"));
    }

    #[test]
    fn panel_text_is_five_lines_of_constant_width() {
        let plugin = ClockPlugin::new(std::sync::Arc::new(std::sync::Mutex::new(crate::plugin::ContextInner::default())));
        let text = plugin.panel_text().expect("clock panel 應該永遠有內容");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 5);
        for line in &lines {
            assert_eq!(line.chars().count(), 34, "每一行都應該是 34 欄寬: {line:?}");
        }
    }

    #[test]
    fn commands_list_is_empty() {
        let plugin = ClockPlugin::new(std::sync::Arc::new(std::sync::Mutex::new(crate::plugin::ContextInner::default())));
        assert!(plugin.commands().is_empty());
    }
}
