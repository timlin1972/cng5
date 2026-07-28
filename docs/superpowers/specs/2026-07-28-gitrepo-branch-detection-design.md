# gitrepo：把「branch 換過」「commit 但還沒 push」也算進「有異動」

日期：2026-07-28

## 目的

`gitrepo` plugin 原本的意圖是「列出所有有修改的 repo」，但現有的 `is_dirty()`
只檢查 `git status --porcelain`，也就是工作目錄有沒有未提交的變更。使用者發現
一個漏洞：用 AI 工作時，AI 常常會直接新建一個 branch 並把變更 commit 上去——
一旦 commit 完成，工作目錄就變乾淨，`is_dirty` 回傳 `false`，這個 repo 就從
「有異動」清單裡消失，即使它其實有使用者還沒看過的新東西。

這次要把「有異動」的定義從單純「有未提交的變更」擴大成三種情況，中一種就算：

1. 有未提交的變更（含 untracked 的新檔案）——沿用原本的判斷。
2. 目前 checkout 的 branch，跟第一次看到這個 repo 時記住的 branch 不一樣——抓
   「AI 開了新 branch 並切過去」的情況。
3. 目前 branch 領先它的 upstream 至少一個 commit（本地已經 commit，但還沒
   push）——抓「AI 直接 commit 在原本的 branch，但還沒 push」的情況。

## 「基準 branch」怎麼決定

用「第一次看到這個 repo 當下記住的 branch」當基準，不是寫死 `main`/`master`
這種固定名稱——因為監控目錄底下的 repo（尤其是 `buildroot/dl` 這種一層一堆
repo 的情況）預設 branch 名稱不見得都一樣。

「第一次看到」有兩種時機都算：
- `add` 一個目錄的當下，對它底下（透過 `repos_under`）找到的每個 repo。
- 後續 `scan` 才第一次發現的新 repo（例如 `dl` 底下事後才新增的子目錄）——
  這種在被發現的那一次 scan，就用當下的 branch 當基準，那一輪不算「換過」。

已經記過基準的 repo，之後不會被覆蓋——即使使用者自己手動切了 branch，也會被
當成「換過」列出來（沒辦法區分是使用者自己切的還是 AI 切的，這是設計上刻意
接受的取捨：抓到永遠比漏掉安全）。

## 已知限制（刻意的取捨，不是之後要修的 bug）

- 基準 branch 只存在記憶體裡，不落地存檔，跟 `watched` 清單本身一致（見
  `2026-07-26-remove-gitrepo-wol-persistence-design.md` 那次拿掉磁碟持久化的
  決定，這次不重新引入）。代價：如果 AI 建立新 branch 並 commit 之後，在
  使用者發現之前程式剛好重開過（`script-local.cli` 重新跑一次 `add`），基準
  會被重設成重開當下的 branch，這一輪就偵測不到「branch 換過」了——但「未
  提交變更」「領先 upstream」這兩種偵測不受影響，仍然抓得到。
- Detached HEAD（不在任何 branch 上）一律當作「跟基準不一樣」處理，不特別
  記錄／比對基準——這種狀態本身就值得使用者注意。
- 不處理 rebase/merge 進行中之類的中繼狀態，`git status --porcelain=v2` 印出
  什麼就是什麼，不特別分類成不同的錯誤訊息。
- 「領先 upstream」只在這個 branch 有設定 upstream（tracking branch）時才有
  意義；沒有 upstream 的 branch（例如從來沒 push 過的全新 branch）這一項恆為
  0，只靠「branch 跟基準不同」這條規則去抓。

## 範圍

- `src/plugins/gitrepo.rs`：
  - 新增 `RepoState`（一次 `git status --porcelain=v2 --branch` 解析出的
    branch/ahead/uncommitted）跟解析函式 `parse_status_v2`/`repo_state`，
    取代原本只跑 `git status --porcelain` 的 `is_dirty`——一個 repo 一次
    subprocess 呼叫拿到全部需要的資訊，不會因為多加了兩個判斷就多開兩倍的
    `git` 子行程（`buildroot/dl` 底下可能上百個 repo，這點很重要）。
  - `DirtyRepo` 換成 `FlaggedRepo`（多了 `uncommitted`/`branch_changed`/
    `ahead` 欄位，取代單純的「是不是 dirty」布林值），`ScanState::Idle` 存的
    型別跟著換。
  - `GitRepoPlugin` 新增 `baseline_branches: Arc<Mutex<HashMap<PathBuf,
    String>>>` 欄位；`new()` 從空的 `HashMap` 開始。
  - `add()`：加入監控目錄之後，對底下每個 repo 呼叫新增的
    `record_baseline_if_missing`，記錄目前的 branch 當基準。
  - `remove()`/`clear()`：把不再屬於任何監控目錄的 repo 的基準資料丟掉
    （`prune_baselines`/直接 `clear`），避免移除又重新加回同一個目錄時，
    誤用很久以前記住的舊基準。
  - `scan()` 背景執行緒：改成呼叫 `repo_state`，比對/記錄基準
    branch，任何一種情況成立就記進 `flagged`；`git status` 執行失敗的情況
    照舊記錄（`error: true`）。
  - `status_text()`：`Idle` 分支改成列出「有異動的 repo」，每個 repo 後面
    用頓號列出實際中了哪幾種原因（可能同時中好幾種）。
  - `MANUAL_TEXT`：更新「不乾淨」的定義說明成三種情況，並在注意事項補上
    「基準 branch 不落地存檔」這條限制。

## 不做的事

- 不對 `wol`/其他 plugin 做任何改動。
- 不新增磁碟持久化——基準 branch 維持純記憶體，重開程式會重新建立（見上面
  「已知限制」）。
- 不做「push」或任何會寫入 repo 的動作，這個 plugin 純粹只讀取狀態。
- 不特別處理 rebase/merge 等中繼狀態的分類顯示。
- 不改變 `add`/`remove`/`clear`/`list`/`scan` 這幾個指令本身的介面（參數、
  用法不變），只改內部判斷邏輯跟顯示內容。

## 測試

- 新增 `#[cfg(test)]` 模組，針對純函式 `parse_status_v2` 寫單元測試（不需要
  真的建立 git repo）：
  - 乾淨、沒有 upstream 的 repo → `branch = Some("main")`、`ahead = 0`、
    `uncommitted = false`。
  - 有 upstream 且領先幾個 commit → 正確解出 `ahead`。
  - 有未提交變更（含 untracked）→ `uncommitted = true`。
  - detached HEAD → `branch = None`。
- 其餘（`add`/`scan`/`status_text` 顯示格式）沒有既有測試模組，這次也不新增
  ——跟這個 plugin 一直以來的做法一致，靠 `cargo build`/`cargo test` 確認
  無回歸，加上手動操作（`add` 一個 repo、故意開一個新 branch 並 commit、
  `scan`、確認 `list` 有把它列出來且原因正確）驗證行為。
