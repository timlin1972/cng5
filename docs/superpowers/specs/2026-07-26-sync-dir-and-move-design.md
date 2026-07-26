# sync plugin：空資料夾建立/刪除傳遞 + 單側檔案改名/搬移偵測

日期：2026-07-26

## 目的

現有 `sync` plugin（見 `docs/superpowers/specs/2026-07-26-sync-plugin-design.md`）的同步演算法
只追蹤檔案層級的新增/修改/刪除/衝突，資料夾完全不進入 `classify` 的 diff 邏輯——資料夾只在
搬檔案時被動建立，從不被獨立建立或刪除。這造成一個實際的行為缺陷：client 端刪除一個目錄
（含裡面的檔案）之後，server 端該目錄下的檔案都會被正確跟著刪除，但目錄本身留著變成孤兒
空目錄。

同時，改名/搬移一個有內容的目錄（例如把 `photos/` 改名成 `pictures/`，裡面有好幾百張照片）
在目前的演算法下，會被當成「舊路徑下所有檔案都被刪除、新路徑下所有檔案都是新檔案」處理，
造成整批內容重新上傳/下載——這在跨 domain（MQTT 傳輸效能較差）的情境下代價特別高。

這次一併解決這兩個問題：**空資料夾的建立/刪除傳遞**，以及**單側檔案改名/搬移的內容重用**
（偵測到「刪除的舊路徑」跟「新增的新路徑」內容相同時，改用直接 rename，不重傳內容）。

## 範圍

- 空資料夾的建立與刪除會傳遞到同步對象（同網域、跨 domain 皆適用）。
- 單側（同一台裝置這一輪自己造成的變化）的檔案改名/搬移會被偵測出來，避免重傳內容。資料夾
  整個改名，是靠底下多個檔案各自被偵測成 move 疊加達成效果，不另外做「整個資料夾搬移」的
  專用邏輯。
- 不處理「雙邊同時各自把不同檔案改成同一組新舊路徑」這種複雜情況——那種情況照舊回歸成普通
  的刪除+新增，不強求偵測成 move。
- 不做 rename 偵測（這裡指的是通用的「路徑 A 換成路徑 B」推斷）以外的內容比對優化，例如
  「內容相似但不完全相同」的差異同步（delta sync）不在範圍內。

## 資料結構

- `SyncEntry`/`walk_with_hashes`（`storage.rs`）不用改——目錄條目（`is_dir: true`）已經存在
  於走訪結果中，只是先前 `sync.rs` 的 `file_states()` 主動把它們濾掉。
- `Baseline`（`sync_baseline.rs`）**目前是型別別名**（`pub(crate) type Baseline =
  HashMap<String, BaselineEntry>;`），不是 struct。這次改成真正的 struct：

  ```rust
  #[derive(Serialize, Deserialize, Clone, Debug, Default)]
  pub(crate) struct Baseline {
      #[serde(default)]
      pub(crate) files: HashMap<String, BaselineEntry>,
      #[serde(default)]
      pub(crate) known_dirs: HashSet<String>,
  }
  ```

  `known_dirs` 記錄「上一輪同步完成時，雙邊都存在的所有目錄路徑」——**不限於當時是空的**，
  一個原本有檔案、後來檔案被清空的目錄，只要它在上一輪同步完成時雙邊都存在過，就會在
  `known_dirs` 裡；這是修正原始需求（client 砍掉「有內容」的目錄）的關鍵，如果只追蹤「一直
  是空的」目錄，最早提出的那個 bug 沒辦法真正修好。
  兩個欄位都加 `#[serde(default)]`，讓沒有 `known_dirs`（甚至整個舊格式就是扁平
  `{路徑: BaselineEntry}`、沒有 `files`/`known_dirs` 這層包裝）的舊 baseline JSON 檔案讀進來
  時能安全降級，不是解析失敗——跟現有「缺欄位就用預設值」的慣例一致，細節見下方「向後相容」
  一節。

  **這是一個會影響既有程式碼的結構性改動**，不是單純加欄位：`Baseline` 從能直接當
  `HashMap` 用（`.get()`、`.insert()`、`.remove()`、`.keys()`、`Baseline::new()`）變成要透過
  `.files` 存取這些方法（例如 `baseline.files.get(path)`、`baseline.files.insert(...)`）。
  `sync.rs` 的 `classify`（`baseline.keys()` → `baseline.files.keys()`、
  `baseline.get(path)` → `baseline.files.get(path)`）、`run_sync_pass`（多處
  `baseline.insert(...)`/`baseline.remove(...)` → `baseline.files.insert(...)`/
  `baseline.files.remove(...)`）、以及 `sync.rs`/`sync_baseline.rs` 現有測試裡所有
  `Baseline::new()`（改成 `Baseline::default()`，或幫 `Baseline` 補一個等效的 `new()`）都要
  跟著改，這些都只是把「直接操作 HashMap」換成「操作 `.files` 這個 HashMap」，不改變任何既有
  行為/測試斷言的內容。

  **向後相容**：舊格式的 baseline JSON 檔案本身就是扁平的 `{路徑: BaselineEntry}`
  （沒有 `files`/`known_dirs` 這層外殼），新的 `Baseline` struct 直接用 `serde_json::from_str`
  讀舊格式檔案**會解析失敗**（結構對不上）。`load_baseline` 現有的行為是「讀取失敗（檔案不
  存在、格式壞掉）一律當作這個對象還沒有 baseline，回傳空的 `Baseline`」——舊格式檔案觸發的
  解析失敗，會被這個既有的錯誤處理路徑正確接住，安全降級成空 baseline（不會讓程式掛掉），
  跟「格式壞掉」走同一條路徑，不需要另外寫舊格式轉新格式的遷移程式碼。代價是：升級後第一輪
  同步，所有同步對象的 baseline 都會被當成「還沒同步過」，保守地把當時的差異都當成潛在衝突
  處理（這是 `load_baseline` 原本就有的既有行為，這次沒有讓它變得更差，只是換了一種會觸發它
  的情境）。

## 分類演算法

`classify` 的輸出從純粹的檔案動作，擴充成「檔案動作（含 Move）＋目錄動作」兩批，執行時先跑
完所有檔案動作、再跑目錄動作（原因見下方「執行順序」）。

### 第一步：單側改名/搬移配對

沿用今天既有的分類邏輯，先算出「這一輪，如果不做任何改名偵測」原本會产生的動作清單：

- **同一側**（例如「這輪本機這邊」）裡，凡是原本會變成 `DeleteRemote`（本機刪了一個
  baseline 有記錄的路徑）的路徑，收集成「刪除候選」，帶著 baseline 記錄的 hash；凡是原本會
  變成 `PushToRemote` 且該路徑**在 baseline 裡完全沒有記錄**（純新檔案，不是「單邊修改」）
  的路徑，收集成「新增候選」，帶著這一輪算出來的 hash。
- 把「刪除候選」跟「新增候選」依 hash 配對：同一個 hash 如果雙邊各自只有一個候選，直接配成
  一對；同一個 hash 有多個刪除候選跟多個新增候選時（例如兩張內容完全相同的照片），把各自的
  路徑字串排序後依序配對（結果穩定、可重現），數量配不完的部分，剩下的刪除候選回歸
  `DeleteRemote`、剩下的新增候選回歸 `PushToRemote`。
- 每一對配對成功的（舊路徑, 新路徑）不再輸出 `DeleteRemote`/`PushToRemote`，改輸出一個
  `SyncAction::MoveRemote { from, to }`（告訴對方直接在自己那邊 rename 現有檔案，不重傳
  內容）。
- 對稱地，本機這一側原本會變成 `DeleteLocal`/`PullFromRemote`（純新檔案）的路徑組，用同樣的
  邏輯配對，配成 `SyncAction::MoveLocal { from, to }`。
- **不參與配對的路徑**：任何被分類成 `Conflict` 的路徑，或本來就是「單邊修改既有路徑」
  （baseline 有記錄、hash 跟 baseline 不同，這種不是「新檔案」）的路徑，都不進入配對池——只
  有乾淨的「刪除」跟「全新新增」才可能是同一次改名/搬移的兩端。
- 這個配對只在同一側（本機自己的刪除候選配自己的新增候選，或對方的刪除候選配對方的新增
  候選）進行；如果刪除候選出現在一側、新增候選出現在另一側，不配對（那本來就是正常的
  單邊同步，不是改名）。

### 第二步：目錄建立/刪除

用這一輪已經抓到的雙邊 manifest（`walk_with_hashes` 的本機結果、這輪從對方拿到的
manifest）判斷，不額外重新查詢：

- 目錄路徑只有一邊存在、`known_dirs` 沒有這個路徑、且**這一輪的雙邊 manifest 底下都沒有任何
  檔案**（遞迴）→ 新建的空目錄，輸出 `SyncAction::CreateLocalDir`/`CreateRemoteDir`（依哪一邊
  有而定）。
- 目錄路徑在 `known_dirs` 裡（上一輪雙邊都確認存在過）、這一輪只剩一邊還找得到這個目錄、且
  **這一輪的雙邊 manifest 底下都沒有任何檔案**→ 傳遞刪除，輸出
  `SyncAction::DeleteLocalDir`/`DeleteRemoteDir`。
- **保護規則**：如果另一邊這一輪的 manifest 顯示該目錄底下還有檔案（例如檔案層級的刪除還沒
  執行完），這一輪直接跳過這個目錄的建立/刪除判斷，不輸出任何目錄動作——等下一輪（檔案動作
  執行完之後）雙邊 manifest 都會顯示 0 檔案，那時保護規則才會放行。這代表目錄消失最多會比
  它底下最後一個檔案消失晚一輪（例如 60 秒），這是可接受的延遲，換來不需要「執行完檔案動作
  後重新查詢對方目錄狀態」這種要多一次網路往返的機制。
- 目錄本身不會產生 `Conflict`——目錄沒有內容，雙邊各自獨立新建同名空目錄不算衝突，直接視為
  一致（更新 `known_dirs`，不輸出動作）。

## 執行順序

`run_sync_pass` 執行時，**先執行所有檔案層級動作**（`PushToRemote`/`PullFromRemote`/
`DeleteLocal`/`DeleteRemote`/`Conflict`/`MoveLocal`/`MoveRemote`），全部跑完之後，**才執行
目錄動作**：

- 目錄建立動作依路徑深度**由淺到深**執行（父目錄要先存在，`mkdir` 才不會失敗）。
- 目錄刪除動作依路徑深度**由深到淺**執行（巢狀空目錄要先刪子層，才能刪父層）。
- 目錄刪除呼叫既有的 `remove(path, recursive=false)`（`storage.rs`）當安全網：如果保護規則
  誤判、執行當下目錄其實還有檔案（罕見的競態窗口——這一輪抓 manifest 之後、執行前的極短時間
  內對方剛好新增了檔案），非遞迴刪除會直接失敗，記錄到 activities log、跳過，不影響其他目錄
  動作，也不會真的把檔案殺掉；下一輪重新 `classify` 會修正。這個安全網已經足夠處理這個機率
  極低的窗口，不需要再做「執行後重新查詢確認」。

## 新增傳輸原語

**目錄建立/刪除完全沿用現成的 `mkdir`/`delete` 端點**（`storage.rs` 的 `make_dir`/`remove`、
HTTP 的 `/api/storage/mkdir`/`/api/storage/delete`、跨 domain 的
`CrossDomainAsk::StorageMkdir`/`StorageDelete`），不需要新增任何東西。

**同網域的 rename 端點已經存在，不用新增**：`storage.rs` 的 `rename_path(from, to)`（`storage mv`
指令在用）、`web.rs` 的 `POST /api/storage/rename?from=<path>&to=<path>`（驗證 `from`/`to` 都通過
`safe_storage_path`）都已經是現成的。`rename_path`／這個端點都**不會**自動建立 `to` 的父目錄
（`from` 不存在、或 `to` 已經是資料夾就報錯，其餘直接 `fs::rename`），所以 `to` 的上層目錄要在
呼叫前確保存在——用法比照現有 `PushToRemote` 呼叫 `ensure_remote_parent_dirs` 的方式，不改
`rename_path`/`storage_rename` 本身的行為（那是共用給互動式 `storage mv` 指令的，維持原樣）。

**只有跨 domain 這條路徑真的需要新增東西**：新增 `CrossDomainAsk::StorageRename { from: String,
to: String }`（`plugin.rs`，比照 `StorageMkdir`/`StorageDelete` 同一組模式：`RemoteRequest`
新增對應變體＋`source_domain()` 分支、`shell.rs` 的 `cross_domain_timeout`／`send_via_mqtt`
新增分支、`global.rs` 的 `build_remote_reply`／`request_kind` 新增分支，執行時一樣先驗證
`from`/`to` 都通過 `safe_storage_path`，再呼叫 `rename_path`，成功回 `RemoteReply::Ack`）。

`SyncTransport` trait（`sync.rs`）新增 `fn rename(&self, from: &str, to: &str) -> Result<()>`，
`HttpTransport` 打現成的 `/api/storage/rename`，`CrossDomainTransport` 送新的
`CrossDomainAsk::StorageRename`。呼叫端（`run_sync_pass`）在呼叫 `transport.rename` 之前，用
`ensure_remote_parent_dirs` 確保 `to` 的父目錄存在（跟現有 `PushToRemote` 的呼叫方式一致，共用
同一個 `known_remote_dirs` 集合）。

本機側的 `MoveLocal` 不用透過 `SyncTransport`——直接呼叫 `std::fs::create_dir_all`（`to` 的
父目錄）+ `crate::plugins::storage::rename_path`（本機端也沿用現成函式，不重寫 `fs::rename`
邏輯）。

## Baseline 更新

- `MoveRemote`/`MoveLocal` 執行成功後：從 `baseline.files` 移除舊路徑的 `BaselineEntry`，用
  配對到的 hash 新增一筆新路徑的 `BaselineEntry`（`local_hash`、`remote_hash` 都設成這個
  hash，因為 rename 之後雙邊內容一致，等同剛同步完成）。
- 目錄建立/刪除動作執行成功後：對應更新 `known_dirs`（建立就加入路徑、刪除就移除路徑）。
- 任何一個動作（檔案或目錄）執行失敗，保留原本的 baseline/`known_dirs` 狀態不變，下一輪會
  重新被分類、重新嘗試——跟現有錯誤處理慣例一致。

## 測試

**分類演算法**（`classify`，純函式，優先度最高）：

- 目錄：新建空目錄（單邊有、`known_dirs` 沒有、雙邊底下都無檔案）→ 產生建立動作。
- 目錄：`known_dirs` 有記錄、單邊消失、雙邊底下都無檔案 → 產生刪除動作。
- 目錄：保護規則命中（另一邊底下這輪還有檔案）→ 不產生任何目錄動作。
- 目錄：巢狀多層空目錄的建立順序（父先於子）、刪除順序（子先於父）。
- 目錄：雙邊各自獨立新建同名空目錄、沒有 `known_dirs` 記錄 → 不是衝突，只更新 `known_dirs`。
- Move：單側一組刪除候選＋新增候選 hash 相同 → 配成 `MoveRemote`/`MoveLocal`，不再各自輸出
  `DeleteRemote`/`PushToRemote`（或對稱的 local 版本）。
- Move：重複 hash（多對多）依路徑排序依序配對，配不完的部分回歸普通刪除/新增。
- Move：`Conflict` 分類的路徑、或「單邊修改既有路徑」（非全新路徑）的路徑，不進入配對池。
- Move：刪除候選跟新增候選分別出現在不同側 → 不配對，維持原本各自的動作。

**Baseline 持久化**：`known_dirs` 欄位的序列化/反序列化；讀取舊格式（改版前那種扁平
`{路徑: BaselineEntry}`、沒有 `files`/`known_dirs` 外殼）的 baseline JSON 檔案時，`load_baseline`
安全降級成空的 `Baseline`（走既有的「格式壞掉當作沒有 baseline」錯誤處理路徑，不是解析失敗
導致 panic）；既有的 `save_then_load_round_trips`/`load_missing_file_returns_empty_baseline`/
`load_corrupt_file_returns_empty_baseline_not_panic`/`save_creates_state_dir_if_missing`/
`save_leaves_no_temp_file_behind`/`different_partners_get_different_files` 這幾個測試，改成透過
`.files`/`Baseline::default()` 操作後應該一樣能通過（行為不變，只是存取方式改變）。

**傳輸層**：新增的 `/api/storage/rename` 端點、`CrossDomainAsk::StorageRename` 不另外寫測試，
跟這個專案現有的 web handler／跨 domain 端點慣例一致。

## 不做的事

- 不處理「雙邊同時各自改名不同檔案」的偵測——那種情況照舊回歸普通的刪除+新增。
- 不做「整個資料夾搬移」的專用單一動作——資料夾改名是靠底下多個檔案各自被偵測成 move 疊加
  達成效果。
- 不做內容相似但不完全相同的差異同步（delta sync）。
- 不做 rename 失敗後除了「下一輪重新分類」以外的自動重試/補償機制。
- 不改動既有的檔案層級分類邏輯（新增、單邊修改、雙邊改成一樣、真衝突等）——這次是在其上
  疊加「配對成 Move」跟「目錄動作」這兩層新邏輯，不動原本已經測過的部分。
