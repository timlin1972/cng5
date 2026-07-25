# Clock Plugin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `clock` plugin whose panel shows the current local time as `hh:mm:ss` rendered in fixed-width ASCII block art (no date), with a blinking colon, and a constant total width regardless of what digits are showing.

**Architecture:** A new stateless plugin file `src/plugins/clock.rs` holds a pure rendering function (digit bitmap font → ASCII art string) plus the `Plugin` trait implementation. `panel_text()` reads the wall clock on every call (no caching, no timer thread — the GUI already redraws every 200ms) and reuses the existing `sysinfo::local_hms` helper for local-time formatting.

**Tech Stack:** Rust, existing `Plugin` trait (`src/plugin.rs`), existing `sysinfo::local_hms` (`src/sysinfo.rs:149`/`:130`), no new dependencies.

## Global Constraints

- No date, no 12-hour/AM-PM support — 24-hour `hh:mm:ss` only.
- Do not add a date/time crate dependency — reuse `sysinfo::local_hms(ts_secs: u64) -> String`.
- No narrow-panel fallback — if the panel is too narrow to show the full ASCII art, let it be truncated by the existing `Paragraph` rendering; the plugin has no way to know the panel width anyway.
- No interactive commands and no GUI keybindings — `commands()` returns `&[]`, `dispatch()` always errors.
- Every digit glyph is a fixed 3-column × 5-row block; colon glyph is a fixed 1-column × 5-row block. Glyphs are joined with a 2-column gap. This is what keeps the total rendered width constant (34 columns × 5 rows for `hh:mm:ss`) no matter what time it is.

---

### Task 1: Digit font & pure rendering function

**Files:**
- Create: `src/plugins/clock.rs`
- Modify: `src/plugins/mod.rs:14` (add `mod clock;` declaration only — no `pub use` yet, `ClockPlugin` doesn't exist until Task 2)

**Interfaces:**
- Produces: `pub fn render_hms(hms: &str, colon_on: bool) -> String` — takes an `"HH:MM:SS"`-shaped string and a colon-blink flag, returns a `\n`-joined 5-row ASCII art string. Task 2's `ClockPlugin::panel_text()` calls this directly.
- Produces (private, used only by tests and by `render_hms` itself): `fn glyph_rows(ch: char, colon_on: bool) -> [String; 5]`.

- [ ] **Step 1: Write the failing tests**

Create `src/plugins/clock.rs` with only this content (no implementation yet, so it fails to compile — that's the expected "red" state):

```rust
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
}
```

Add the module declaration so `cargo test` can find it — in `src/plugins/mod.rs`, add `mod clock;` as a new first line (alphabetical order with the existing `mod activities;` … `mod wol;` block, so it goes between `mod activities;` and `mod device;`):

```rust
mod activities;
mod clock;
mod device;
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test clock:: 2>&1 | tail -30`
Expected: compile error — `cannot find function 'glyph_rows'` / `'render_hms'` in this scope (they don't exist yet).

- [ ] **Step 3: Implement the font table and rendering functions**

Add this above the `#[cfg(test)]` block in `src/plugins/clock.rs`:

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test clock:: 2>&1 | tail -30`
Expected: all 7 tests in `plugins::clock::tests` pass (`test result: ok. 7 passed`).

- [ ] **Step 5: Commit**

```bash
git add src/plugins/clock.rs src/plugins/mod.rs
git commit -m "$(cat <<'EOF'
新增 clock plugin 的 ASCII 字型與 render_hms

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: `ClockPlugin` struct, registration, and manual verification

**Files:**
- Modify: `src/plugins/clock.rs` (add `ClockPlugin` struct + `Plugin` impl + tests, above or below the Task 1 content but inside the same `#[cfg(test)] mod tests` block)
- Modify: `src/plugins/mod.rs:14-27` (add `pub use clock::ClockPlugin;`, alphabetically after `pub(crate) use files::...`/`pub use files::FilesPlugin;` and before `pub use gitrepo::GitRepoPlugin;` — i.e. right where a `c` entry belongs)
- Modify: `src/main.rs:29-32` (add `ClockPlugin` to the `use plugins::{...}` import list) and `src/main.rs:52-103` (add a `"clock"` entry to the `factories` vec)

**Interfaces:**
- Consumes: `render_hms(hms: &str, colon_on: bool) -> String` from Task 1 (`src/plugins/clock.rs`); `crate::plugin::{Plugin, SharedContext}`; `crate::output::OutputBuffer`; `crate::sysinfo::local_hms(ts_secs: u64) -> String` (already exists, `src/sysinfo.rs`).
- Produces: `pub struct ClockPlugin` with `pub fn new(_ctx: SharedContext) -> Self`, used by `main.rs`'s plugin factory closure exactly like every other plugin (see `QrPlugin::new` at `src/main.rs:102` for the pattern).

- [ ] **Step 1: Write the failing test**

Add this test inside the existing `#[cfg(test)] mod tests { use super::*; ... }` block in `src/plugins/clock.rs` (append it after the last test from Task 1, still inside the same `mod tests` braces):

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test clock:: 2>&1 | tail -30`
Expected: compile error — `ClockPlugin` and `OutputBuffer` aren't in scope yet (both the struct and the `use` statements are added together in Step 3), so you'll see something like `cannot find type 'ClockPlugin' in this scope` and/or `cannot find type 'OutputBuffer' in this scope`. Any compile error here is the expected "red" state — Step 3 fixes all of it at once.

- [ ] **Step 3: Implement `ClockPlugin`**

Add these `use` statements at the very top of `src/plugins/clock.rs` (before the `DIGIT_FONT` const from Task 1):

```rust
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};
use crate::sysinfo;
```

Add this after `render_hms` (still before the `#[cfg(test)]` block):

```rust
const MANUAL_TEXT: &str = "\
clock：panel 用 ASCII 大字顯示現在時間（hh:mm:ss，本地時區、24 小時制，不含日期）。

沒有指令，全部都在 panel 裡自動顯示：
  冒號每秒閃爍一次（單數秒顯示、雙數秒隱藏），數字用固定大小的方塊字型畫，
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test clock:: 2>&1 | tail -30`
Expected: all 10 tests in `plugins::clock::tests` pass (`test result: ok. 10 passed`).

- [ ] **Step 5: Register the plugin**

In `src/plugins/mod.rs`, the `pub use` block is currently:

```rust
pub use activities::ActivitiesPlugin;
pub use device::DevicePlugin;
pub(crate) use files::{safe_file_path, url_encode_filename, ALLOWED_FOLDERS};
pub use files::FilesPlugin;
pub use gitrepo::GitRepoPlugin;
```

Change it to (only change: inserted `pub use clock::ClockPlugin;` as the second line):

```rust
pub use activities::ActivitiesPlugin;
pub use clock::ClockPlugin;
pub use device::DevicePlugin;
pub(crate) use files::{safe_file_path, url_encode_filename, ALLOWED_FOLDERS};
pub use files::FilesPlugin;
pub use gitrepo::GitRepoPlugin;
```

In `src/main.rs`, add `ClockPlugin` to the import list (`main.rs:29-32`):

```rust
use plugins::{
    ActivitiesPlugin, ClockPlugin, DevicePlugin, FilesPlugin, GitRepoPlugin, GlobalPlugin, MusicPlugin,
    NotepadPlugin, OutputPlugin, QrPlugin, RemoteOutputPlugin, RemotePlugin, SystemPlugin, WeatherPlugin,
    WolPlugin,
};
```

Add a factory entry to the `factories` vec in `src/main.rs` (right after the `"activities"` entry, to keep it near the top since it's simple — exact position in the vec doesn't matter functionally):

```rust
        (
            "activities",
            Box::new(|ctx| Box::new(ActivitiesPlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "clock",
            Box::new(|ctx| Box::new(ClockPlugin::new(ctx)) as Box<dyn Plugin>),
        ),
        (
            "device",
```

- [ ] **Step 6: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -30`
Expected: builds with no errors and no new warnings.

Run: `cargo test 2>&1 | tail -15`
Expected: all tests pass, including the 10 new `plugins::clock::tests::*`.

- [ ] **Step 7: Manual smoke test**

This plugin is visual and time-based, so also verify it by hand:

Run: `cargo run`

At the root shell prompt (`cng5>`), type:
```
clock
panel
```
The first line switches into the `clock` plugin (`Mode::InPlugin`), the second opens its panel (`Mode::InPanel`) — this is the same two-step navigation every plugin uses (see `src/shell.rs:889` and `src/shell.rs:921`).

Confirm:
1. The panel shows a 5-row ASCII-art clock matching the current local wall-clock time.
2. The colon blinks once per second (visible on/off).
3. Resizing the panel narrower truncates the art (no crash, no layout corruption).

- [ ] **Step 8: Commit**

```bash
git add src/plugins/clock.rs src/plugins/mod.rs src/main.rs
git commit -m "$(cat <<'EOF'
新增 clock plugin：panel 顯示 ASCII 大字時鐘

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```
