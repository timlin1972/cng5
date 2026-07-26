use std::collections::{HashMap, HashSet};

use crate::plugins::storage::SyncEntry;
use crate::plugins::sync_baseline::Baseline;

/// 這一輪同步對某個路徑該做的事。只處理檔案，不處理空資料夾——資料夾是搬檔
/// 案時由執行層視需要自動建立，不會有獨立的「建立資料夾」/「刪除資料夾」
/// 動作。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SyncAction {
    PushToRemote { path: String },
    PullFromRemote { path: String },
    DeleteLocal { path: String },
    DeleteRemote { path: String },
    Conflict { path: String },
}

struct FileState {
    hash: String,
}

/// 把 `walk_with_hashes` 的結果過濾成「只有檔案、路徑對到雜湊」的對照表，
/// 資料夾條目直接跳過（同步演算法不處理空資料夾，見上方 `SyncAction` 的說
/// 明）。
fn file_states(entries: &[SyncEntry]) -> HashMap<String, FileState> {
    entries
        .iter()
        .filter(|e| !e.is_dir)
        .filter_map(|e| e.hash.clone().map(|hash| (e.path.clone(), FileState { hash })))
        .collect()
}

/// 核心分類邏輯：比對本機清單、對方清單、這個同步對象的 baseline，決定每個
/// 路徑該做的事。純函式，不牽涉任何網路/檔案系統 I/O。
pub(crate) fn classify(local: &[SyncEntry], remote: &[SyncEntry], baseline: &Baseline) -> Vec<SyncAction> {
    let local_states = file_states(local);
    let remote_states = file_states(remote);

    let mut all_paths: HashSet<&String> = HashSet::new();
    all_paths.extend(local_states.keys());
    all_paths.extend(remote_states.keys());
    all_paths.extend(baseline.keys());

    let mut paths: Vec<&String> = all_paths.into_iter().collect();
    paths.sort();

    let mut actions = Vec::new();
    for path in paths {
        let local_state = local_states.get(path);
        let remote_state = remote_states.get(path);
        let base = baseline.get(path);

        match (local_state, remote_state, base) {
            (Some(_), None, None) => actions.push(SyncAction::PushToRemote { path: path.clone() }),
            (None, Some(_), None) => actions.push(SyncAction::PullFromRemote { path: path.clone() }),
            (None, None, _) => {} // 兩邊都沒有：不管有沒有舊 baseline 紀錄，都無事可做
            (Some(local), None, Some(base)) => {
                if local.hash == base.local_hash {
                    actions.push(SyncAction::DeleteLocal { path: path.clone() });
                } else {
                    // 對方刪了，但本機這邊同時改過——保留本機的修改，當作
                    // 本機端的變更重新推過去。
                    actions.push(SyncAction::PushToRemote { path: path.clone() });
                }
            }
            (None, Some(remote), Some(base)) => {
                if remote.hash == base.remote_hash {
                    actions.push(SyncAction::DeleteRemote { path: path.clone() });
                } else {
                    actions.push(SyncAction::PullFromRemote { path: path.clone() });
                }
            }
            (Some(local), Some(remote), base) => {
                if local.hash == remote.hash {
                    continue; // 內容一樣，不用管有沒有 baseline
                }
                match base {
                    None => actions.push(SyncAction::Conflict { path: path.clone() }),
                    Some(base) => {
                        // 分開比對本機、對方各自跟「自己那一邊上次記錄的值」
                        // 有沒有變——`base.local_hash` 跟 `base.remote_hash`
                        // 本來就可能不一樣（已經確認過的衝突就是這樣故意留
                        // 著），不能假設兩者相等。
                        let local_changed = local.hash != base.local_hash;
                        let remote_changed = remote.hash != base.remote_hash;
                        match (local_changed, remote_changed) {
                            (true, false) => actions.push(SyncAction::PushToRemote { path: path.clone() }),
                            (false, true) => actions.push(SyncAction::PullFromRemote { path: path.clone() }),
                            (false, false) => {
                                // 兩邊都跟上次記錄的一樣——這是先前已經確認、
                                // 故意保留分歧的衝突，不用再產生一次衝突副本。
                                continue;
                            }
                            (true, true) => actions.push(SyncAction::Conflict { path: path.clone() }),
                        }
                    }
                }
            }
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::storage::SyncEntry;
    use crate::plugins::sync_baseline::{Baseline, BaselineEntry};

    fn file(path: &str, hash: &str) -> SyncEntry {
        SyncEntry { path: path.to_string(), is_dir: false, size: 10, modified: 0, hash: Some(hash.to_string()) }
    }

    /// 每個項目是 `(路徑, local_hash, remote_hash)`——正常同步完成後兩者相等，
    /// 但已經確認過的衝突會刻意留下不同的值，測試裡分開指定才能覆蓋到兩種
    /// 情況。
    fn baseline_of(entries: &[(&str, &str, &str)]) -> Baseline {
        entries
            .iter()
            .map(|(path, local_hash, remote_hash)| {
                (
                    path.to_string(),
                    BaselineEntry { local_hash: local_hash.to_string(), remote_hash: remote_hash.to_string() },
                )
            })
            .collect()
    }

    #[test]
    fn new_local_file_pushes_to_remote() {
        let local = vec![file("new.txt", "h1")];
        let remote = vec![];
        let actions = classify(&local, &remote, &Baseline::new());
        assert_eq!(actions, vec![SyncAction::PushToRemote { path: "new.txt".to_string() }]);
    }

    #[test]
    fn new_remote_file_pulls_from_remote() {
        let local = vec![];
        let remote = vec![file("new.txt", "h1")];
        let actions = classify(&local, &remote, &Baseline::new());
        assert_eq!(actions, vec![SyncAction::PullFromRemote { path: "new.txt".to_string() }]);
    }

    #[test]
    fn unchanged_on_both_sides_produces_no_action() {
        let local = vec![file("same.txt", "h1")];
        let remote = vec![file("same.txt", "h1")];
        let baseline = baseline_of(&[("same.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert!(actions.is_empty());
    }

    #[test]
    fn local_only_change_pushes_to_remote() {
        let local = vec![file("f.txt", "h2")]; // 本機改成 h2
        let remote = vec![file("f.txt", "h1")]; // 對方還是舊的 h1
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]); // 上次同步時雙方都是 h1
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::PushToRemote { path: "f.txt".to_string() }]);
    }

    #[test]
    fn remote_only_change_pulls_from_remote() {
        let local = vec![file("f.txt", "h1")];
        let remote = vec![file("f.txt", "h2")];
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::PullFromRemote { path: "f.txt".to_string() }]);
    }

    #[test]
    fn both_sides_changed_to_same_content_is_no_action() {
        let local = vec![file("f.txt", "h2")];
        let remote = vec![file("f.txt", "h2")]; // 剛好改成一樣
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert!(actions.is_empty());
    }

    #[test]
    fn both_sides_changed_differently_is_conflict() {
        let local = vec![file("f.txt", "h2")];
        let remote = vec![file("f.txt", "h3")];
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::Conflict { path: "f.txt".to_string() }]);
    }

    #[test]
    fn both_sides_independently_created_same_name_different_content_is_conflict() {
        let local = vec![file("new.txt", "hA")];
        let remote = vec![file("new.txt", "hB")];
        let actions = classify(&local, &remote, &Baseline::new()); // 沒有 baseline
        assert_eq!(actions, vec![SyncAction::Conflict { path: "new.txt".to_string() }]);
    }

    #[test]
    fn both_sides_independently_created_same_name_same_content_is_no_action() {
        let local = vec![file("new.txt", "hA")];
        let remote = vec![file("new.txt", "hA")];
        let actions = classify(&local, &remote, &Baseline::new());
        assert!(actions.is_empty());
    }

    #[test]
    fn remote_deleted_unchanged_local_file_deletes_local() {
        let local = vec![file("gone.txt", "h1")]; // 本機沒動過
        let remote = vec![]; // 對方刪掉了
        let baseline = baseline_of(&[("gone.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::DeleteLocal { path: "gone.txt".to_string() }]);
    }

    #[test]
    fn local_deleted_unchanged_remote_file_deletes_remote() {
        let local = vec![]; // 本機刪掉了
        let remote = vec![file("gone.txt", "h1")]; // 對方沒動過
        let baseline = baseline_of(&[("gone.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::DeleteRemote { path: "gone.txt".to_string() }]);
    }

    #[test]
    fn remote_deleted_but_local_edited_keeps_local_edit_by_pushing() {
        // 對方把這個路徑刪了，但本機這邊在同一時間反而編輯過它——保留本機的
        // 修改（推過去），不要因為對方刪除就跟著刪掉本機也改過的內容。
        let local = vec![file("f.txt", "h2")]; // 本機改過
        let remote = vec![]; // 對方刪了
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::PushToRemote { path: "f.txt".to_string() }]);
    }

    #[test]
    fn local_deleted_but_remote_edited_keeps_remote_edit_by_pulling() {
        let local = vec![]; // 本機刪了
        let remote = vec![file("f.txt", "h2")]; // 對方改過
        let baseline = baseline_of(&[("f.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::PullFromRemote { path: "f.txt".to_string() }]);
    }

    #[test]
    fn both_sides_deleted_produces_no_action() {
        let local = vec![];
        let remote = vec![];
        let baseline = baseline_of(&[("gone.txt", "h1", "h1")]);
        let actions = classify(&local, &remote, &baseline);
        assert!(actions.is_empty());
    }

    #[test]
    fn no_baseline_and_content_differs_is_conservatively_a_conflict() {
        // 對應「重啟後完全沒有 baseline」的情境：兩邊都有、內容不一樣、又沒有
        // baseline 可以判斷是誰改的，保守當成衝突處理。
        let local = vec![file("f.txt", "hA")];
        let remote = vec![file("f.txt", "hB")];
        let actions = classify(&local, &remote, &Baseline::new());
        assert_eq!(actions, vec![SyncAction::Conflict { path: "f.txt".to_string() }]);
    }

    #[test]
    fn already_acknowledged_conflict_produces_no_action() {
        // baseline 記錄了「local 是 hA、remote 是 hB」——代表上一輪已經處理過
        // 這個衝突（兩邊故意留下不同內容），這一輪雙方都還是各自原本的值，
        // 不應該再被判定成新衝突、重複產生衝突副本。
        let local = vec![file("f.txt", "hA")];
        let remote = vec![file("f.txt", "hB")];
        let baseline = baseline_of(&[("f.txt", "hA", "hB")]);
        let actions = classify(&local, &remote, &baseline);
        assert!(actions.is_empty());
    }

    #[test]
    fn acknowledged_conflict_then_local_changes_again_pushes_to_remote() {
        // 已經確認過的衝突（baseline: local=hA, remote=hB）之後，本機這邊又
        // 改了一次（hC）——這是相對 baseline 的單向變更，應該推過去，不是
        // 重新判定成衝突。
        let local = vec![file("f.txt", "hC")];
        let remote = vec![file("f.txt", "hB")];
        let baseline = baseline_of(&[("f.txt", "hA", "hB")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::PushToRemote { path: "f.txt".to_string() }]);
    }

    #[test]
    fn acknowledged_conflict_then_both_change_again_is_a_new_conflict() {
        let local = vec![file("f.txt", "hC")];
        let remote = vec![file("f.txt", "hD")];
        let baseline = baseline_of(&[("f.txt", "hA", "hB")]);
        let actions = classify(&local, &remote, &baseline);
        assert_eq!(actions, vec![SyncAction::Conflict { path: "f.txt".to_string() }]);
    }

    #[test]
    fn multiple_independent_paths_each_classified_separately() {
        let local = vec![file("a.txt", "h1"), file("b.txt", "h1")];
        let remote = vec![file("b.txt", "h1")]; // a.txt 只有本機有，b.txt 兩邊一樣
        let actions = classify(&local, &remote, &Baseline::new());
        assert_eq!(actions, vec![SyncAction::PushToRemote { path: "a.txt".to_string() }]);
    }
}
