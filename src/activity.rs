use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 一筆網路活動紀錄。`kind` 是固定的幾種分類字串（見各呼叫端），`detail` 是
/// 給人看的一行摘要。時間戳用 unix 秒數而不是好看的日期時間，跟
/// `main::log_respawn` 同樣的理由：不想為了格式化日期另外引入一個 crate。
pub struct ActivityEntry {
    pub ts_secs: u64,
    pub kind: &'static str,
    pub detail: String,
}

/// 一個上限筆數的網路活動流水帳（`mqtt`/`http`/外部服務呼叫），給 `activities`
/// plugin 用。跟 `OutputBuffer`（`src/output.rs`）同一種「`Mutex` 包一個容器,
/// `push`/`clear` 介面」寫法，差別只在多一個可調整的上限，超過就從最舊的開始
/// 丟掉——這是流水帳而不是給人看的完整指令輸出，不需要也不應該無上限成長。
pub struct ActivityLog {
    entries: Mutex<VecDeque<ActivityEntry>>,
    limit: Mutex<usize>,
}

impl ActivityLog {
    pub fn new(default_limit: usize) -> Self {
        Self { entries: Mutex::new(VecDeque::new()), limit: Mutex::new(default_limit) }
    }

    /// 記一筆活動，超過目前上限就從最舊的開始丟掉。
    pub fn push(&self, kind: &'static str, detail: impl Into<String>) {
        let ts_secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let limit = *self.limit.lock().unwrap();
        let mut entries = self.entries.lock().unwrap();
        entries.push_back(ActivityEntry { ts_secs, kind, detail: detail.into() });
        while entries.len() > limit {
            entries.pop_front();
        }
    }

    /// 調整上限；縮小時立刻裁掉多出來的最舊紀錄，而不是等下一筆 `push` 才生效。
    pub fn set_limit(&self, n: usize) {
        *self.limit.lock().unwrap() = n;
        let mut entries = self.entries.lock().unwrap();
        while entries.len() > n {
            entries.pop_front();
        }
    }

    pub fn limit(&self) -> usize {
        *self.limit.lock().unwrap()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    /// 最近 `n` 筆，格式化成一行一筆、由舊到新排列（跟 `list`/panel 顯示順序一致）。
    pub fn recent(&self, n: usize) -> Vec<String> {
        let entries = self.entries.lock().unwrap();
        let skip = entries.len().saturating_sub(n);
        entries.iter().skip(skip).map(format_entry).collect()
    }

    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }
}

impl Default for ActivityLog {
    fn default() -> Self {
        Self::new(1000)
    }
}

fn format_entry(entry: &ActivityEntry) -> String {
    format!("[{}] {:<10} {}", crate::sysinfo::local_hms(entry.ts_secs), entry.kind, entry.detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_recent_preserve_order() {
        let log = ActivityLog::new(10);
        log.push("mqtt-out", "a");
        log.push("http-in", "b");
        log.push("external", "c");
        let lines = log.recent(10);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("mqtt-out") && lines[0].ends_with("a"));
        assert!(lines[1].contains("http-in") && lines[1].ends_with("b"));
        assert!(lines[2].contains("external") && lines[2].ends_with("c"));
    }

    #[test]
    fn push_beyond_limit_drops_oldest() {
        let log = ActivityLog::new(3);
        for i in 0..5 {
            log.push("http-out", format!("entry-{i}"));
        }
        assert_eq!(log.len(), 3);
        let lines = log.recent(10);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].ends_with("entry-2"));
        assert!(lines[1].ends_with("entry-3"));
        assert!(lines[2].ends_with("entry-4"));
    }

    #[test]
    fn set_limit_shrinks_immediately() {
        let log = ActivityLog::new(10);
        for i in 0..5 {
            log.push("mqtt-in", format!("e{i}"));
        }
        assert_eq!(log.len(), 5);
        log.set_limit(2);
        assert_eq!(log.len(), 2);
        let lines = log.recent(10);
        assert!(lines[0].ends_with("e3"));
        assert!(lines[1].ends_with("e4"));
    }

    #[test]
    fn clear_empties_log() {
        let log = ActivityLog::new(10);
        log.push("external", "x");
        log.clear();
        assert_eq!(log.len(), 0);
        assert!(log.recent(10).is_empty());
    }

    #[test]
    fn recent_caps_at_requested_count() {
        let log = ActivityLog::new(10);
        for i in 0..5 {
            log.push("http-out", format!("e{i}"));
        }
        let lines = log.recent(2);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("e3"));
        assert!(lines[1].ends_with("e4"));
    }
}
