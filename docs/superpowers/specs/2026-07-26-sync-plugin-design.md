# sync plugin（多裝置同步備份）設計

日期：2026-07-26

## 目的

「NAS 功能」第二個子項目：讓 `storage` plugin 管理的本機儲存，在多台裝置之間自動、
雙向同步（含刪除傳遞、衝突處理），並支援跨 domain。不做版本歷史（那是獨立的另一個
子項目，之後再看要不要做）。

## 範圍

- 建立在 `storage` plugin（本機檔案總管，含巢狀子資料夾）之上，同步的對象永遠是整個
  `storage/` 根目錄，不做多個具名共用資料夾的細粒度設定（那是「共享資料夾/權限」子
  項目的範圍）。
- 完全複用 `system` plugin 既有的 client/server/domain 角色，**不另外設計配對設定**：
  - `mode server` 的裝置：同步的中心節點（hub）
  - `mode client` + `server <ip>` 的裝置：同步的目標之一，完全被動
  - `domain <name>` + 現有 `global` 跨 domain 註冊：決定 server 之間彼此看不看得到
- 不做手動 `sync` 指令——同步完全靠背景輪詢自動發生，不需要任何人手動觸發（見「觸發
  時機」一節）。
- 不做版本歷史/保留舊版本。

## 拓撲與觸發時機

**Hub 模式**：同一個 domain 裡，server 是中心節點，同時跟自己 domain 底下每一個已知
的 client 保持同步（client 之間不直接互相同步）；跨 domain 則是 server 跟 `global`
registry 裡看得到的每一個別的 domain 的 server 同步。一個變更要傳到「所有裝置」，會
經過最多三段接力（client → 自己的 server → 別的 domain 的 server → 那個 domain的
client），不是立即生效，最壞情況大約是三倍輪詢間隔的時間。

**只有 `mode server` 的裝置會啟動背景輪詢執行緒**（沿用 `system` plugin 背景回報執行
緒的做法，但用獨立的輪詢間隔，例如 60 秒，不跟現有裝置回報的 10 秒共用）：

- 每一輪：對同 domain 底下每一個已知的 client、以及 `global` registry 看得到的每一
  個別的 domain 的 server，各自跑一次完整的同步流程（見下方「同步演算法」）。
- 「看得到就同步」的風險：跨 domain 只有共用同一把加密金鑰（`remote-key`）的裝置才會
  互相可見，範圍限定在使用者已經信任的裝置內，不是對公網任何人開放。
- **跨 domain 雙向可見時，避免兩邊同時互相發起同步**：如果 domain A 跟 domain B 互相
  看得到彼此，兩邊的 server 都會各自定期輪詢、都可能主動對對方發起同步，如果剛好兩邊
  在差不多時間都對彼此發起，會變成同時跑兩份 diff/傳輸/寫 baseline，容易產生競態。
  規則：**只有 domain 名稱字典序比較小的那一方才會主動對另一方發起跨 domain 同步**
  （例如 domain 名稱 `"alpha"` 只會對 `"beta"` 發起，`"beta"` 不會對 `"alpha"` 發起，
  但 `"beta"` 還是會被動接受來自 `"alpha"` 的同步請求）——這樣任兩個 domain 之間永遠
  只有單一方向會主動發起，不會互相搶著同步同一個對象。
- `mode client` 的裝置完全被動，不啟動任何背景執行緒、不主動連線任何人；同步永遠是
  server 端主動發起連到 client 端已經常駐在跑的 web server（storage API）。
- 沒有手動觸發指令：使用者不用做任何事，最終所有裝置都會透過背景輪詢自動同步到，只是
  有上述的接力延遲。

## Baseline 持久化

同步的核心比對需要「上次同步完成時，這個路徑在雙方的狀態」這個基準（baseline），否則
沒辦法分辨「只有一邊改過」跟「兩邊各自改過、真的衝突」。

- 新增頂層資料夾 `sync-state/`（跟 `storage/`、`music/`、`notepad/` 同一層，不放在
  `storage/` 裡面——放裡面會被當成使用者可見的同步內容，甚至被誤判成也要同步的檔案）。
- 每個同步對象（同 domain 的某個 client id、或跨 domain 的某個 domain 名稱）各自一個
  JSON 檔案，例如 `sync-state/client-<id>.json`、`sync-state/domain-<domain>.json`，
  內容是 `{相對路徑: {hash, size, modified}}` 的對照表，記錄上次同步完成時雙方一致的
  狀態。
- 寫入：每一輪同步跑完，把這次確定同步成功的路徑更新進記錄，用「先寫暫存檔、再
  rename」的方式寫入，避免中途當機寫壞檔案（這個專案第一次需要把設定寫到磁碟並持久
  化，這是新加的能力，不是沿用既有模式）。
- 讀取：`sync` plugin 啟動（`mode server` 時）讀取 `sync-state/` 底下對應的檔案；讀不
  到、格式壞掉都當作「這個對象還沒有 baseline」處理，不會讓程式掛掉，只是保守地把之後
  遇到的差異都當成潛在衝突處理（見下方）。
- `sync-state/` 整個資料夾加進 `.gitignore`。

## 同步演算法

**內容比對**：新增 `sha2` 依賴，每個檔案在同步時算一份 hash。新增一個遞迴走訪函式
`walk_with_hashes(root)`，把整棵 `storage/` 樹攤平成 `{相對路徑, 是否資料夾, 大小,
mtime, hash}` 的清單（資料夾沒有 hash）——每次同步都重新算，不做「只在懷疑衝突時才
算」這種提前優化（手動觸發已經拿掉、改成定期輪詢，效能成本可以接受）。

**分類邏輯**（純函式，輸入本機清單、對方清單、這個對象的 baseline，輸出每個路徑該做
的動作）：

- 只有一邊有、baseline 也沒有 → 新檔案，單向搬過去
- baseline 有記錄、現在只剩一邊有 → 另一邊刪除過，跟著刪除
- 兩邊都有、baseline 也有：只有一邊的 hash 跟 baseline 不同 → 那一邊改過，單向搬過
  去；兩邊都跟 baseline 不同但彼此 hash 相同 → 剛好改成一樣，不用搬，只更新 baseline；
  兩邊都跟 baseline 不同且彼此也不同 → 真衝突
- 兩邊都有、baseline 沒有記錄（雙方各自獨立新建同名檔案，從沒同步過）→ hash 相同就當
  作沒事（更新 baseline），不同就是衝突
- 如果因為程式重啟導致這個對象完全沒有 baseline，往後每一輪只要雙方內容不一樣，一律
  保守當成衝突處理（不去猜測哪一邊比較新），直到重新累積出 baseline 為止

**衝突處理**：兩邊各自保留自己原本的檔案不動，額外把對方的版本存成一份
`檔名 (衝突自 <對方 id/domain>，日期).副檔名`——本機存一份對方版本的衝突副本，同時
把自己這份也推一份衝突副本過去給對方，兩邊同步結束後都各自有「自己原本的」+「對方的
衝突副本」兩個檔案。這兩個新產生的衝突副本路徑立刻寫進 baseline，避免下一輪被誤判成
新差異。

**傳輸**：
- 同網域：新增 `GET /api/storage/sync-manifest`（分頁，比照 `files` plugin 的
  `FileList` 分頁方式）回傳 `walk_with_hashes` 的結果；實際搬檔案／建資料夾／刪除都
  直接呼叫既有的 `/api/storage/download`、`/api/storage/upload`、`/api/storage/mkdir`、
  `/api/storage/delete`（`storage` plugin 已經做好的端點，不重做）。
- 跨 domain：新增 `CrossDomainAsk::StorageManifest`（分頁）＋
  `StorageFilePull`/`StorageFilePush`/`StorageMkdir`/`StorageDelete`，做法比照現有
  `FileList`/`FilePull`/`FilePush`（4KB chunk、逐段往返），路徑換成 storage 底下的巢狀
  相對路徑，一樣都要過 `safe_storage_path` 驗證。

## Plugin 與呈現

- 新增獨立 plugin `sync`（不是塞進 `storage`，因為核心邏輯是跟 `system` plugin 的
  client/server/domain 角色綁在一起，職責不一樣）。
- 沒有手動觸發指令。唯一的指令是 `sync status`——列出每個同步對象上次同步的時間、結
  果（成功/失敗）、搬了幾個檔案、刪了幾個、產生幾個衝突副本。
- `panel_text()` 顯示 `sync status` 的內容，用一般純文字 panel（不需要客製化的瀏覽
  UI，不用碰 `frontend.html`）。
- 每次同步的搬檔/刪除/衝突事件都寫進現有的 activities log。

## 錯誤處理

- 對象連不上——這次跳過、記錯誤到 activities log，不影響其他對象、不讓背景執行緒掛
  掉，下一輪繼續重試。
- 同步跑到一半失敗——只有真的成功的路徑才更新 baseline，失敗的路徑保留原本 baseline，
  下一輪會被當成「還沒同步」重新處理。
- 不驗證傳輸後的內容正確性（不做傳完再算一次 hash 比對）——沿用 `files` plugin「相信
  底層傳輸」的既有做法。

## 測試

- **分類演算法**（核心，純函式，不牽涉網路/檔案系統 I/O）：針對每一種分類情況（新增、
  單向修改、單向刪除、雙方改成一樣、真衝突、雙方各自新建同名檔案、無 baseline 時保守
  當作衝突）各寫測試，這是整個功能正確性最關鍵的部分，要測到完整。
- **baseline 讀寫**：JSON 序列化、先寫暫存檔再 rename、讀不到/壞掉要能安全降級，用暫
  存目錄測試，比照 `storage.rs` 現有的測試慣例。
- 網路/HTTP/MQTT 傳輸的部分不另外寫測試，跟這個專案現有的 web handler 慣例一致。

## 不做的事

- 不做版本歷史/保留舊版本（獨立子項目）。
- 不做多個具名共用資料夾的細粒度同步設定（獨立子項目「共享資料夾/權限」的範圍）。
- 不做手動同步指令——完全自動、背景輪詢驅動。
- 不另外設計配對設定機制——完全複用 `system` plugin 既有的 client/server/domain 角色。
- 不做傳輸後的內容完整性再驗證。
- 不限制/篩選跨 domain 同步對象——只要在 `global` registry 看得到（也就是共用同一把
  加密金鑰）就會自動同步。
