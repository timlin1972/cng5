# gitrepo：把「branch 不是 develop」「commit 但還沒 push」也算進「有異動」

日期：2026-07-28

## 目的

`gitrepo` plugin 原本的意圖是「列出所有有修改的 repo」，但現有的 `is_dirty()`
只檢查 `git status --porcelain`，也就是工作目錄有沒有未提交的變更。使用者發現
一個漏洞：用 AI 工作時，AI 常常會直接新建一個 branch 並把變更 commit 上去——
一旦 commit 完成，工作目錄就變乾淨，`is_dirty` 回傳 `false`，這個 repo 就從
「有異動」清單裡消失，即使它其實有使用者還沒看過的新東西。

把「有異動」的定義從單純「有未提交的變更」擴大成三種情況，中一種就算：

1. 有未提交的變更（含 untracked 的新檔案）——沿用原本的判斷。
2. 目前 checkout 的 branch 不是 `develop`——抓「AI 開了新 branch 並切過去」的
   情況。
3. 目前 branch 領先它的 upstream 至少一個 commit（本地已經 commit，但還沒
   push）——抓「AI 直接 commit 在原本的 branch，但還沒 push」的情況。

## 「正常」的 branch 是寫死的 `develop`，不是每個 repo 各自記住的基準

這個規格經過一次重新校準：一開始的版本是「用第一次看到這個 repo 當下記住的
branch」當基準（每個 repo 各自記自己的），後來使用者明確要求拿掉這個機制，改
成單一寫死的 `EXPECTED_BRANCH = "develop"` 常數，套用到所有監控目錄底下的
每一個 repo——不管是哪個 repo，只要目前不是 `develop`，就算異動，不再有
「這個 repo 本來就習慣待在別的 branch，所以不算」這種例外。

這個決定的取捨很清楚：如果之後監控目錄裡出現一個原本就該待在別的 branch
（例如 `main`）的 repo，它會一直被列成「有異動」，沒有辦法個別排除——使用者
確認過這是可以接受的，比起維護一份「每個目錄該對應哪個預期 branch」的設定更
簡單。

好處是這個規則完全是無狀態的：不需要記住任何東西，`scan` 每次都用同一個固定
基準比對，程式重開、`add`/`remove`/`clear` 怎麼操作都不影響判斷結果——先前
版本「基準只存在記憶體、重開程式會被重設」那個限制，這次直接不存在了。

## Detached HEAD 恰好在某個 remote branch 的 tip 上，視同該 branch

CI/build 流程常見的操作是 `git checkout origin/develop` 之類的指令，結果是
detached HEAD，但這其實就是「在 develop 上」，不是異常狀態。所以偵測到
detached HEAD 時，額外查一次 `git for-each-ref --points-at=HEAD ... refs/remotes/`
——如果目前這個 commit 剛好是某個 remote branch 的最新 commit，就把這個狀態
當成「checkout 在 `<branch>`」（remote 名稱前綴去掉，例如 `origin/develop` 視同
`develop`），再拿去跟 `EXPECTED_BRANCH` 比對，跟一般 branch 完全相同的邏輯，
不特別處理。真正查不到任何 remote branch 對得上這個 commit（例如卡在某個歷史
commit、或 bisect 中途）才算「異動」（顯示成 `(detached HEAD)`）。

多個 remote 的 branch 剛好指到同一個 commit 時，只要其中有一個剛好就是
`EXPECTED_BRANCH`（例如 `origin/develop`），就直接認定「在 develop 上」，不管
還有沒有其他巧合也指到同一個 commit 的 branch——vendor/3rdparty 這類 repo
常見同一個 commit 同時是好幾個 release branch 的起點（見下面「踩到的真實
bug」）。真的沒有任何候選是 `EXPECTED_BRANCH` 才需要挑一個代表性的名稱顯示
在「有異動」的原因裡：優先選 `origin/*`，找不到才選字母排序第一個。

**踩到的真實 bug（已修正）：** `git for-each-ref --points-at=HEAD ... refs/remotes/`
也會列出 remote 自己的 `HEAD` symref（指向該 remote 預設 branch的指標，不是
真正的 branch），依 git 版本不同，短名稱可能是 `<remote>`（沒有 `/`）或
`<remote>/HEAD`。後者排序會排在 `origin/develop` 前面，「優先選 `origin/*`」
那條規則沒有排除它，於是誤選到 `origin/HEAD`，`short_branch_name` 再把它切成
字面上的 `"HEAD"` 回傳，導致實際在 `origin/develop` tip 上、完全乾淨的 repo
被誤判成「branch 是 HEAD，不是 develop」。修法：`pick_detached_branch_name`
（拆出來的純函式，方便測試）明確濾掉沒有 `/` 的候選跟結尾是 `/HEAD` 的候選，
兩種 git 版本的行為都排除掉。

**第二個踩到的真實 bug（已修正）：** 修完上面那個之後，緊接著遇到另一個案例
——`3rdparty_cpss` 這種 vendor repo，同一個 commit 同時是 `origin/develop`
跟其他好幾個產品線專用 release branch（例如
`origin/MDS-G4000-4XGS_v5.0.2_develop`）的起點。原本「多個候選排序後選
`origin/*` 裡字母排序第一個」這條規則，因為大寫字母在 ASCII 排序中排在小寫
字母前面，選到了 `MDS-G4000-4XGS_v5.0.2_develop`，而不是 `develop`——即使
`git status` 明確顯示 `HEAD detached at origin/develop`。使用者確認的期望
行為：只要 `git status` 顯示 detached 在 `origin/develop` 上，就該算是在
develop 上，不管同一個 commit 上還掛著多少其他巧合的 branch。修法：
`pick_detached_branch_name` 先檢查候選裡有沒有任何一個剛好是
`EXPECTED_BRANCH`，有就直接回傳，不受其他候選字母排序影響；真的沒有才落回
「選 `origin/*` 裡字母排序第一個」這條規則去挑一個顯示用的名稱。

## 已知限制（刻意的取捨，不是之後要修的 bug）

- `EXPECTED_BRANCH` 是單一寫死的常數（`"develop"`），套用到所有監控目錄底下
  的所有 repo，不能個別指定。如果某個 repo 本來就該待在別的 branch，它會一直
  顯示成「有異動」——使用者明確要求這樣做，換取規則簡單、不用維護額外設定。
- Detached HEAD 且查不到任何 remote branch 對得上目前 commit（見上一節）才算
  異動——這種狀態本身就值得使用者注意。
- 不處理 rebase/merge 進行中之類的中繼狀態，`git status --porcelain=v2` 印出
  什麼就是什麼，不特別分類成不同的錯誤訊息。
- 「領先 upstream」只在這個 branch 有設定 upstream（tracking branch）時才有
  意義；沒有 upstream 的 branch（例如從來沒 push 過的全新 branch）這一項恆為
  0，只靠「branch 不是 develop」這條規則去抓。

## 範圍

- `src/plugins/gitrepo.rs`：
  - `RepoState`/`parse_status_v2`/`repo_state`/`resolve_detached_branch`/
    `short_branch_name` 維持不變（一次 `git status --porcelain=v2 --branch`
    拿到 branch/ahead/uncommitted，detached 時額外解析 remote branch tip）。
  - 新增 `EXPECTED_BRANCH: &str = "develop"` 常數。
  - **移除**先前版本新增的 `baseline_branches` 欄位跟
    `record_baseline_if_missing`/`prune_baselines` 方法——不再需要記住任何
    per-repo 的狀態，`GitRepoPlugin` 回到只有 `watched`/`scan`/`generation`
    三個欄位。`add()`/`remove()`/`clear()` 也拿掉對應的呼叫，恢復成原本的
    簡單版本。
  - `FlaggedRepo.branch_changed` 型別從 `Option<(String, String)>`（基準,
    目前）簡化成 `Option<String>`（只需要記「目前是什麼」，因為預期值永遠是
    同一個 `EXPECTED_BRANCH`，不用重複存）。
  - `scan()` 背景執行緒：branch 比對邏輯從「查/更新 per-repo 基準表」簡化成
    單純比較 `state.branch` 跟 `EXPECTED_BRANCH` 是否相等，不再需要
    `baseline_branches` 這個 `Arc<Mutex<HashMap<...>>>`，也不用再拿額外的鎖。
  - `describe_reasons`：顯示文字從「branch 從 X 換成 Y」改成「branch 是 X，
    不是 develop」。
  - `MANUAL_TEXT`：更新成新的三條規則說明，拿掉「基準 branch 不落地存檔」這條
    （已經不適用，規則本身無狀態）。

## 不做的事

- 不對 `wol`/其他 plugin 做任何改動。
- 不做「push」或任何會寫入 repo 的動作，這個 plugin 純粹只讀取狀態。
- 不特別處理 rebase/merge 等中繼狀態的分類顯示。
- 不改變 `add`/`remove`/`clear`/`list`/`scan` 這幾個指令本身的介面（參數、
  用法不變），只改內部判斷邏輯跟顯示內容。
- 不讓 `EXPECTED_BRANCH` 可設定（不做「每個監控目錄各自指定預期 branch」這種
  彈性）——使用者明確要求維持單一寫死的值，見上面「已知限制」。

## 測試

- `#[cfg(test)]` 模組裡針對純函式 `parse_status_v2`/`short_branch_name` 的
  單元測試維持不變（不需要真的建立 git repo）：
  - 乾淨、沒有 upstream 的 repo → `branch = Some("main")`、`ahead = 0`、
    `uncommitted = false`。
  - 有 upstream 且領先幾個 commit → 正確解出 `ahead`。
  - 有未提交變更（含 untracked）→ `uncommitted = true`。
  - detached HEAD → `branch = None`。
  - `short_branch_name` 正確去掉 remote 名稱前綴。
- 其餘（`add`/`scan`/`status_text` 顯示格式）沒有既有測試模組，這次也不新增
  ——跟這個 plugin 一直以來的做法一致，靠 `cargo build`/`cargo test` 確認
  無回歸，加上手動操作（在真實 git repo 分別驗證「在 develop 上、乾淨」「在
  別的 branch 上、乾淨」「detached 在 origin/develop 的 tip 上」三種情況，
  確認 `scan`/`list` 的判斷結果符合預期）驗證行為。
