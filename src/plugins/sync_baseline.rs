use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 頂層資料夾，跟 `storage/`、`music/`、`notepad/` 同一層——不放在 `storage/`
/// 裡面，放裡面會被當成使用者可見的同步內容，甚至被誤判成也要同步的檔案。
pub(crate) const SYNC_STATE_DIR: &str = "sync-state";

/// 某個路徑上次同步完成時，本機、對方各自的內容 hash——特意分開記錄兩邊，
/// 不是共用一個值。正常同步完成後兩者會相等（雙方內容一致），但「已經確認
/// 過的衝突」是特例：兩邊故意保留不同的內容，這時候 `local_hash`/
/// `remote_hash` 就會不一樣，讓 `classify`（Task 3）能認得出「這個分歧是已知
/// 且已經處理過的」，不會每一輪都重新判定成新衝突、瘋狂產生新的衝突副本。
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub(crate) struct BaselineEntry {
    pub(crate) local_hash: String,
    pub(crate) remote_hash: String,
}

/// 一個同步對象（同 domain 的某個 client id、或跨 domain 的某個 domain 名稱）
/// 的完整 baseline：相對路徑 -> 上次同步完成時的狀態。
pub(crate) type Baseline = HashMap<String, BaselineEntry>;

pub(crate) fn baseline_path(state_dir: &Path, partner_key: &str) -> PathBuf {
    state_dir.join(format!("{partner_key}.json"))
}

/// 讀取失敗（檔案不存在、格式壞掉）一律當作「這個對象還沒有 baseline」，回傳
/// 空的 `Baseline`，不會讓呼叫端因此整個失敗——保守處理，往後遇到的差異會被
/// 當成潛在衝突，見同步演算法的說明。
pub(crate) fn load_baseline(state_dir: &Path, partner_key: &str) -> Baseline {
    let path = baseline_path(state_dir, partner_key);
    match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Baseline::new(),
    }
}

/// 先寫暫存檔、再 rename 成正式檔名，避免中途當機留下寫壞一半的檔案。
pub(crate) fn save_baseline(state_dir: &Path, partner_key: &str, baseline: &Baseline) -> Result<()> {
    fs::create_dir_all(state_dir).with_context(|| format!("建立 {} 失敗", state_dir.display()))?;
    let path = baseline_path(state_dir, partner_key);
    let tmp_path = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(baseline).context("序列化 baseline 失敗")?;
    fs::write(&tmp_path, content).with_context(|| format!("寫入暫存檔失敗: {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &path).with_context(|| format!("重新命名暫存檔失敗: {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 每個測試各自用一個獨立命名的暫存資料夾，避免 cargo test 平行跑測試時
    /// 互相踩到彼此的檔案（跟 `storage.rs` 的 `test_root` 同一個理由）。
    fn test_state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cng5-sync-baseline-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn save_then_load_round_trips() {
        let state_dir = test_state_dir("round-trip");
        let mut baseline = Baseline::new();
        baseline.insert(
            "photos/beach.jpg".to_string(),
            BaselineEntry { local_hash: "abc123".to_string(), remote_hash: "abc123".to_string() },
        );
        save_baseline(&state_dir, "client-office-pc", &baseline).unwrap();
        let loaded = load_baseline(&state_dir, "client-office-pc");
        assert_eq!(loaded, baseline);
    }

    #[test]
    fn load_missing_file_returns_empty_baseline() {
        let state_dir = test_state_dir("missing-file");
        let loaded = load_baseline(&state_dir, "nonexistent-partner");
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_corrupt_file_returns_empty_baseline_not_panic() {
        let state_dir = test_state_dir("corrupt-file");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(baseline_path(&state_dir, "broken"), b"{ this is not valid json").unwrap();
        let loaded = load_baseline(&state_dir, "broken");
        assert!(loaded.is_empty());
    }

    #[test]
    fn save_creates_state_dir_if_missing() {
        let state_dir = test_state_dir("creates-dir");
        assert!(!state_dir.exists());
        save_baseline(&state_dir, "domain-branch-b", &Baseline::new()).unwrap();
        assert!(state_dir.is_dir());
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let state_dir = test_state_dir("no-temp-leftover");
        save_baseline(&state_dir, "client-a", &Baseline::new()).unwrap();
        let tmp_path = baseline_path(&state_dir, "client-a").with_extension("json.tmp");
        assert!(!tmp_path.exists());
        assert!(baseline_path(&state_dir, "client-a").exists());
    }

    #[test]
    fn different_partners_get_different_files() {
        let state_dir = test_state_dir("different-partners");
        let mut baseline_a = Baseline::new();
        baseline_a.insert("x.txt".to_string(), BaselineEntry { local_hash: "h1".to_string(), remote_hash: "h1".to_string() });
        save_baseline(&state_dir, "client-a", &baseline_a).unwrap();
        save_baseline(&state_dir, "client-b", &Baseline::new()).unwrap();
        assert_eq!(load_baseline(&state_dir, "client-a"), baseline_a);
        assert!(load_baseline(&state_dir, "client-b").is_empty());
    }
}
