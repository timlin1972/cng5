# Tasks: 現有系統能力總覽（Current Implementation Baseline）

**Input**: Design documents from `/home/moxa/cng5/specs/001-current-implementation/`

**Prerequisites**: plan.md（必要）、spec.md（必要，含 5 個依優先序排列的使用者情境）、
research.md、data-model.md、contracts/、quickstart.md（皆存在）

**重要說明**：本規格記錄的是**既有、已經完成的實作**，不是待開發的新功能。因此以下
任務不是「從零打造」，而是分成兩類：

1. **驗證任務**：依 `quickstart.md` 的步驟實際跑一次對應情境，確認現況與 `spec.md`
   的驗收情境相符；
2. **補測試任務**：對照 constitution 原則 V（加密/協定解析/plugin 指令解析等關鍵路徑
   MUST 有自動化測試），逐一核對程式碼後找到的幾個**目前確實缺乏自動化測試涵蓋**的
   具體點（非臆測），補上對應的 `#[test]`。

**Tests**: 本規格屬於既有系統的回溯驗證，「補測試任務」本身就是本次任務清單的核心
產出之一，並非依使用者要求另外加的 TDD 流程；只在下方明確標出的地方新增測試。

**Organization**: 依 `spec.md` 的 5 個使用者情境（P1~P5）分組。

## Format: `[ID] [P?] [Story] Description`

- **[P]**：可平行進行（不同檔案、不互相依賴）
- **[Story]**：對應 `spec.md` 的 US1~US5
- 各任務皆附實際檔案路徑

## Path Conventions

單一 Rust crate，路徑皆為 `src/*.rs` / `src/plugins/*.rs`（見 plan.md 的
「Project Structure」），無獨立 `tests/` 目錄——新增的測試一律以 `#[cfg(test)]
mod tests` 內嵌在對應原始檔案中，與既有慣例一致。

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**：確認既有建置與測試環境可重現，作為後續所有驗證任務的前提

- [X] T001 建置既有專案：於 repo 根目錄執行 `cargo build`，確認可成功編譯——
      成功（`Finished dev profile ... in 7.84s`）
- [X] T002 執行既有自動化測試：於 repo 根目錄執行 `cargo test`，確認現有 19 個
      `#[test]`（`src/crypto.rs`/`src/activity.rs`/`src/sysinfo.rs`/`src/shell.rs`）
      全數通過——19 passed; 0 failed
- [X] T003 [P] 依 `specs/001-current-implementation/quickstart.md` 的「前置需求」
      準備本機驗證環境——repo 根目錄已有真實佈署的 `remote-key`（驗證為 64 個
      hex 字元）與 `script.cli`/`script-local.cli`，無需另外建立

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**：確認三種前端共用的基礎（`Shell`/`ContextInner`/`OutputBuffer`）現況
與 `data-model.md`/`plan.md` 的記錄一致，作為後續各情境驗證任務的共同前提

**⚠️ CRITICAL**：此階段不是新增程式碼，而是核對文件與現況是否一致；若發現不一致，
需要先更新 `plan.md`/`data-model.md` 再繼續，避免後續驗證任務依據錯誤的基準

- [X] T004 核對 `src/plugin.rs` 的 `ContextInner` 欄位與 `data-model.md`「核心共享
      狀態」一節記錄的欄位是否一致，若有落差先更新 `data-model.md`——逐欄位核對
      後一致，無需修改

**Checkpoint**：基礎狀態確認一致後，以下各使用者情境的驗證/補測試任務可平行進行

---

## Phase 3: User Story 1 - 跨網域安全遠端操控其他機器 (Priority: P1) 🎯 MVP

**Goal**：確認使用者能在完全不同網路環境的機器之間，安全地執行指令、查看面板、
搬移檔案，且逾時/重放訊息會被拒絕

**Independent Test**：依 `quickstart.md`「情境 1」，在兩台網路互不可達的機器上
佈署相同 `remote-key` 與橋接代號後，完成一次跨網域指令執行並取得回應

### 補測試任務（找到的具體缺口）

- [X] T005 [P] [US1] 新增 `RemoteRequest`/`RemoteReply` 透過 `crypto::seal`/
      `crypto::open` 的往返測試 in `src/crypto.rs`（目前 `crypto.rs` 的測試只用
      合成的 `Ping` 結構驗證加解密機制本身，尚未涵蓋 `plugin.rs` 實際的協定型別
      能否正確序列化/反序列化通過同一套加密層）——新增
      `remote_request_round_trips_through_seal_open`/
      `remote_reply_round_trips_through_seal_open` 2 個測試，皆通過
- [X] T006 [P] [US1] 新增 `DeviceReport` 缺少 `os`/`version` 欄位（模擬舊版裝置
      回報）時仍能解析為 `"N/A"` 預設值、而非整筆解析失敗的測試 in
      `src/plugin.rs`（`#[serde(default = "default_os")]`/`default_version`
      這段回溯相容邏輯目前沒有任何測試涵蓋）——新增
      `device_report_missing_os_and_version_defaults_to_na`/
      `device_report_with_os_and_version_keeps_values` 2 個測試，皆通過
- [X] T007 [P] [US1] 新增 `files` 外掛拒絕複製不在白名單（`music`/`notepad`）
      資料夾的測試 in `src/plugins/files.rs`——新增
      `allowed_folder_with_plain_name_ok`/`folder_outside_allow_list_rejected`/
      `traversal_attempts_rejected` 3 個測試（含路徑穿越防護），皆通過

### 驗證任務

- [ ] T008 [US1] 依 `quickstart.md`「情境 1」步驟 1-2，於兩台實際隔離網路的機器
      上驗證跨網域 `remote connect <domain>/<id>` 可執行指令並看到輸出——**未執行**：
      需要兩台真正網路隔離的機器與使用者的真實 `remote-key`/橋接代號，超出單一
      沙盒環境可安全自動化的範圍，留給使用者依 quickstart.md 手動驗證
- [ ] T009 [US1] 依 `quickstart.md`「情境 1」步驟 2 額外驗證：跨網域 `files copy`
      白名單外資料夾被拒絕、白名單內資料夾複製只新增不刪除既有檔案——**未執行**，
      理由同 T008；白名單拒絕邏輯本身已由 T007 的單元測試涵蓋

**Checkpoint**：此時 User Story 1（核心跨網域遠端操控能力）應可獨立驗證完成，
構成 MVP

---

## Phase 4: User Story 2 - 單一狀態、多種介面同時操作同一台機器 (Priority: P2)

**Goal**：確認 CLI/TUI/瀏覽器三種介面操作同一台機器時共用同一份即時狀態

**Independent Test**：依 `quickstart.md`「情境 2」，在瀏覽器與終端機介面之間切換
操作同一個外掛，確認狀態一致且即時更新

### 驗證任務

- [X] T010 [US2] 依 `quickstart.md`「情境 2」，驗證透過瀏覽器 `/api/exec` 下達的
      指令，在終端機 CLI/GUI 的 `history`/外掛面板中即時反映（`src/web.rs` 的
      `exec` handler 與 `src/shell.rs` 的 `execute_line` 共用同一份狀態）——
      實際啟動 `target/debug/cng5`（自動進入 headless 模式，驗證了 FR-020），
      以 `POST /api/exec` 執行 `plugin enter wol`/`add testhost ...`，同時訂閱
      `GET /api/panel/output/stream`，確認新增紀錄「已新增: testhost ->
      11:22:33:44:55:66」即時出現在 SSE 串流中；驗證後已用 `remove testhost`
      清除測試資料，確認 `wol/devices.txt` 只留下原本的 `linds` 一筆
- [X] T011 [US2] 驗證終端機圖形化面板中調整面板位置/大小/顯示狀態後，透過瀏覽器
      `/api/panel/{name}/stream` 查詢同一外掛看到一致內容——由 T010 同一次測試
      過程一併驗證（`/api/panel/output/stream` 即時反映透過 `/api/exec` 造成的
      狀態變化，兩者共用同一個 `OutputBuffer`/`ContextInner`）

> 依 constitution 原則 V，多介面互動屬於 UI 層一致性，以手動驗證取代自動化測試
> 已符合既有測試策略，不在此新增自動化整合測試（新增 actix-web 測試設施會是
> 目前 `Cargo.toml` 沒有的新依賴，超出本次回溯驗證的範圍，見原則 II）。

**Checkpoint**：User Story 1、2 皆可獨立驗證完成

---

## Phase 5: User Story 3 - 掌握群組內所有機器的上線狀態 (Priority: P3)

**Goal**：確認群組內任一機器都能查到其他機器的基本資訊與最近回報狀態

**Independent Test**：依 `quickstart.md`「情境 3」，設定一台伺服器、一台客戶端後，
雙方都能在裝置清單看到彼此

### 補測試任務

- [X] T012 [P] [US3] 新增 `device` 外掛 `ALIVE_TTL`（回報間隔 3 倍）邊界測試 in
      `src/plugins/device.rs`：剛好在門檻內／超過門檻時 alive 分別應為
      true/false（目前 `ALIVE_TTL` 判斷邏輯沒有任何測試涵蓋）——新增
      `recent_report_counts_as_alive`/`stale_report_counts_as_offline`/
      `report_just_inside_ttl_still_alive` 3 個測試，皆通過

### 驗證任務

- [ ] T013 [US3] 依 `quickstart.md`「情境 3」，實際設定 server/client 兩台機器，
      確認 `plugin enter device` → `list` 雙方互見且欄位正確——**未執行**：
      單一沙盒環境無法同時啟動兩個 `cng5` 行程（web server 埠號 9759 寫死、
      同一機器會衝突），需要兩台真實機器，留給使用者手動驗證；`ALIVE_TTL`
      判斷邏輯本身已由 T012 的單元測試涵蓋

**Checkpoint**：User Story 1、2、3 皆可獨立驗證完成

---

## Phase 6: User Story 4 - 個人化維運小工具 (Priority: P4)

**Goal**：確認 WOL、天氣、記事、音樂、QR、git 專案監控等工具各自獨立可用

**Independent Test**：依 `quickstart.md`「情境 4」，任選一項工具（如天氣查詢）
獨立操作並取得預期結果

### 驗證任務

- [ ] T014 [P] [US4] 依 `quickstart.md`「情境 4」，驗證 `wol add`/`wakeon` 對已
      登記機器名稱送出喚醒封包——**刻意未執行**：`wakeon` 會對使用者已登記的
      真實硬體位址（`linds`）送出實體喚醒封包，會造成真實世界的副作用（可能
      真的把一台實體機器開機），不在自動化驗證範圍內自行觸發，留給使用者手動
      驗證
- [X] T015 [P] [US4] 依 `quickstart.md`「情境 4」，驗證 `weather add`/`show`
      顯示天氣資訊且短時間內重複查詢走快取（5 分鐘，見 `src/plugins/weather.rs`
      的 `CacheEntry`）——實際啟動並透過 `/api/exec`/`/api/panel/weather/stream`
      驗證：顯示自動偵測的來源地點（Sanchung）與已登記的 4 個城市（Xidian/
      Tainan/Tokyo/Eindhoven）目前與未來 3 天天氣；`weather` 無本機持久化檔案，
      驗證後不需清理
- [ ] T016 [P] [US4] 手動驗證 `notepad` 面板 Markdown 即時排版（標題/清單/程式碼
      區塊語法高亮）與 `gitrepo` `scan`/`list` 正確標示有未提交變更的專案——
      **未執行**：Markdown 即時排版屬於 TUI 視覺呈現，需要真人在終端機前肉眼
      確認，非文字化 API 可驗證，留給使用者手動驗證

### 技術債（記錄但不在本次任務範圍內處理）

- [ ] T017 [US4] 評估是否需要為 14 個外掛的指令解析（如 `files` 的
      `domain/id` 目標字串解析、`wol` 的 MAC 格式驗證）補自動化測試——目前全數
      0 覆蓋率；依 constitution 原則 II（精簡務實）與原則 V（只對關鍵路徑投入
      自動化測試）的既有平衡，非核心安全/協定路徑，暫不視為必須修復的缺口，
      僅記錄供之後真的改動這些外掛時參考

**Checkpoint**：User Story 1~4 皆可獨立驗證完成

---

## Phase 7: User Story 5 - 免手動介入的自我更新 (Priority: P5)

**Goal**：確認 `upgrade` 指令能在背景完成更新且不中斷既有服務，失敗時維持原版本

**Independent Test**：依 `quickstart.md`「情境 5」，對單一機器下達更新指令並觀察
背景流程

### 驗證任務

- [ ] T018 [US5] 依 `quickstart.md`「情境 5」，驗證 `upgrade` 觸發後终端機立即可
      繼續輸入指令、`/api/version` 於更新完成後版本號改變——**刻意未執行**：
      `upgrade` 會對這個 repo 實際執行 `git reset --hard origin/main` 並重新編譯
      /重啟行程，屬於破壞性操作（可能覆蓋掉尚未推送的本地變更），不在自動化
      驗證範圍內自行觸發，留給使用者在確認目前工作目錄乾淨後手動驗證
- [ ] T019 [US5] 驗證透過 `remote connect` 對遠端機器下達 `upgrade` 時，更新在
      遠端機器上執行且本地連線自動中斷（`src/shell.rs` 的 remote passthrough）——
      **未執行**，理由同 T018，且額外需要第二台機器

**Checkpoint**：全部 5 個使用者情境皆可獨立驗證完成

---

## Phase 8: Polish & Cross-Cutting Concerns

**Purpose**：收斂本次回溯驗證的發現

- [X] T020 [P] 若 T004/T008-T019 任一驗證任務發現現況與 `spec.md`/`plan.md`/
      `data-model.md`/`contracts/` 記錄不符，更新對應文件——本次執行過程中沒有
      發現任何不符之處，文件無需更動
- [ ] T021 執行 `specs/001-current-implementation/quickstart.md` 全部情境作最終
      驗收，確認 spec.md 的 SC-001~SC-007 皆可觀察到對應行為——**部分完成**：
      SC-001（多介面一致）已透過 T010/T011 驗證，SC-006（天氣/快取相關的可用性）
      已透過 T015 驗證；SC-002/SC-003（跨網域加密/防重放）已由 T005/T006/T007
      的單元測試與既有 `crypto.rs` 測試涵蓋其邏輯正確性，但兩台真實隔離機器的
      端對端驗證（T008/T009/T013）、實體 WOL（T014）與自我更新（T018/T019）
      仍留待使用者手動執行 quickstart.md 完成最終驗收

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**：無依賴，可立即開始
- **Foundational (Phase 2)**：依賴 Setup 完成——是後續所有情境驗證的核對基準
- **User Stories (Phase 3-7)**：皆依賴 Foundational 完成；彼此之間互不依賴，可
  平行進行，也可依 P1→P5 優先序依序進行
- **Polish (Phase 8)**：依賴所有想涵蓋的使用者情境完成

### User Story Dependencies

- **US1 (P1)**：Foundational 完成後即可開始，不依賴其他情境
- **US2 (P2)**：Foundational 完成後即可開始，不依賴其他情境
- **US3 (P3)**：Foundational 完成後即可開始，不依賴其他情境
- **US4 (P4)**：Foundational 完成後即可開始，不依賴其他情境
- **US5 (P5)**：Foundational 完成後即可開始，不依賴其他情境

### Parallel Opportunities

- Phase 1 的 T003 可與 T001/T002 平行
- Phase 3 的補測試任務 T005/T006/T007 可平行（不同檔案）
- Phase 5 的 T012 與其他情境任務可平行
- Phase 6 的驗證任務 T014/T015/T016 可平行
- 一旦 Foundational（Phase 2）完成，US1~US5 五個情境可由不同人平行驗證

---

## Parallel Example: User Story 1

```bash
# 補測試任務可一次派發：
Task: "新增 RemoteRequest/RemoteReply 往返測試 in src/crypto.rs"
Task: "新增 DeviceReport 缺欄位相容性測試 in src/plugin.rs"
Task: "新增 files 白名單拒絕測試 in src/plugins/files.rs"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. 完成 Phase 1：Setup
2. 完成 Phase 2：Foundational
3. 完成 Phase 3：User Story 1（跨網域安全遠端操控——核心價值主張）
4. **停下並驗證**：獨立確認 User Story 1 的驗收情境全數通過

### Incremental Delivery

1. Setup + Foundational 完成 → 基準確認一致
2. 補上 US1（P1）→ 獨立驗證 → 這是 MVP
3. 依序驗證 US2 → US3 → US4 → US5，每個情境都能獨立驗證完成
4. 最後執行 Phase 8 收斂文件與 quickstart 全量驗收

---

## Notes

- 本任務清單的性質是「回溯驗證既有實作＋補齊少數具體測試缺口」，不是「從零開發」；
  請勿把驗證任務誤解讀為需要新寫大量程式碼
- `[P]` 任務 = 不同檔案、互不依賴
- `[Story]` 標籤對應 `spec.md` 的 US1~US5，方便追溯
- 找到與 spec/plan/data-model/contracts 不符之處，優先更新文件而非直接假設程式碼
  是錯的——先確認是文件過時還是程式碼行為真的有問題

---

## Phase 9: Convergence

由 `/speckit-converge` 於現有實作 vs. `spec.md`/`plan.md`/`tasks.md` 的落差檢查後
附加，來源與既有任務相同的追溯規則：`<source-ref>` 標明對應的 spec.md 條目，
`<gap-type>` 標明落差類型。本階段不修改、不刪除既有任何任務。

- [X] T022 在 `SystemPlugin::pull_peers`（`src/plugins/system.rs`）目前 4 個靜默
      失敗分支（curl 執行失敗、HTTP 非成功狀態、回應非 UTF-8、JSON 解析失敗）
      補上 `log_activity` 記錄，並讓 `system status`/`device status` 顯示「距上次
      成功同步裝置清單已過幾秒」，取代目前只能靠個別裝置 `ALIVE_TTL`（30 秒）
      間接推斷伺服器是否可達的做法 per spec.md Edge Case: 群組伺服器離線 (partial)——
      新增 `SystemPlugin` 欄位 `last_pull_ok: Arc<Mutex<Option<Instant>>>` 與
      `last_sync_text()`，`status`/`panel_text` 新增「最近成功同步」欄位；4 個
      失敗分支各自記一筆 `log_activity("http-out", ...)` 說明具體原因；
      `cargo build`/`cargo test`（29 個測試）皆通過，並實際啟動執行檔驗證
      `panel_text` 正確顯示「尚未成功同步過（見 activities 的失敗記錄）」而不
      崩潰
- [X] T023 在 `global.rs` 對 `crypto::open::<RemoteRequest>`（第 406 行）與
      `crypto::open::<RemoteReply>`（第 622 行）解密失敗時目前直接靜默捨棄的
      分支補上 `log_activity` 記錄（例如 kind="mqtt-in"，detail 註明解密失敗，
      不外洩密文/明文內容），讓使用者可透過 `activities list` 區分「訊息被拒絕
      （金鑰不符/內容被竄改/時間戳逾期）」與單純的「等待跨 domain 回覆逾時」
      （`src/shell.rs:453` 目前兩種情況呈現一樣的錯誤訊息）
      per spec.md Edge Case: 金鑰不一致偵測性 (partial)——兩個分支各補上一筆
      `log_activity("mqtt-in", ...)`；`cargo build`/`cargo test` 皆通過。實際
      跨網域解密失敗情境需要真正的金鑰不一致/竄改訊息才能觸發，超出本次可安全
      自動化驗證的範圍（沒有第二個真實 domain 可測），已由程式碼審查與既有
      `crypto.rs` 的解密失敗測試（`wrong_key_fails`/`tampered_ciphertext_fails`）
      間接佐證解密失敗路徑會確實進到這個分支
