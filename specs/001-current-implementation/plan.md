# Implementation Plan: 現有系統能力總覽（Current Implementation Baseline）

**Branch**: `001-current-implementation` | **Date**: 2026-07-24 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/home/moxa/cng5/specs/001-current-implementation/spec.md`

**Note**: 此規格記錄的是**已經完成的既有實作**，不是一項待開發的新功能。因此本計畫
的角色不是「決定要怎麼蓋」，而是把現有程式碼中已經做出的技術決策、資料模型與對外
介面，整理成可供未來新功能規劃（下一次 `/speckit-specify` 針對具體變更）比對基準
的文件。Phase 0/1 產物因此是「回溯記錄既有決策」而非「研究未知選項」。

## Summary

cng5 是一個以單一 Rust 執行檔提供的個人跨機器維運工具：一份共用的 Shell/外掛狀態
同時透過終端機文字介面、終端機圖形化面板、以及內嵌的瀏覽器網頁介面操作；機器之間
可以組成「群組」互相回報狀態，群組之間又能透過一個共同的公開 MQTT broker 橋接，
讓使用者能以 AEAD 加密、具時間戳防重放保護的訊息，安全地跨越完全不相通的網路環境
（domain）執行指令、查看面板、搬移檔案。其餘的天氣、記事、音樂、Wake-on-LAN、
QR 分享、git 專案監控、自我更新等，都是建立在這個共用狀態與外掛架構之上的個別
工具。技術做法：Rust 單一 crate、內嵌 actix-web 伺服器、ratatui TUI、rumqttc
MQTT client、chacha20poly1305 AEAD，全部資料以本機檔案或行程記憶體保存，不使用
資料庫。

## Technical Context

**Language/Version**: Rust, edition 2024（`Cargo.toml`），單一二進位執行檔 `cng5`

**Primary Dependencies**: `actix-web`/`actix-files`/`actix-ws`（內嵌網頁伺服器、
靜態檔案、WebSocket）、`ratatui`/`crossterm`（終端機圖形化面板）、`rustyline`
（CLI 行編輯與歷史）、`portable-pty`（真實 host shell / 遠端 shell passthrough）、
`rumqttc`（跨網域橋接用 MQTT client）、`chacha20poly1305` + `data-encoding`（跨網域
訊息 AEAD 加密）、`tungstenite`（CLI 端連向遠端 shell 的 WebSocket client）、`qrcode`
（連線 QR Code）、`id3`（音樂檔案 ID3 標籤/封面）、`serde`/`serde_json`（所有跨行程/
跨機器訊息格式）、`tokio`（web 伺服器與背景輪詢的非同步執行環境）、`anyhow`、
`shell-words`、`unicode-width`、`async-stream`

**Storage**: 純本機檔案／行程記憶體，無資料庫。共用金鑰 `remote-key`（純文字、64
hex 字元，手動佈署且不進版控）、啟動腳本 `script.cli`（進版控的共用預設）與
`script-local.cli`（不進版控的機器別覆寫）、`notepad/*.md` 筆記、`music/*.mp3` 音樂
檔、`gitrepo` 監控清單持久化到磁碟；裝置清單、跨網域關聯表、活動流水帳（上限
1000 筆，見 `activity.rs`）僅存於行程記憶體，重啟即清空。

**Testing**: `cargo test`；目前 19 個 `#[test]`，分布在 `crypto.rs`（AEAD 加解密
往返、錯誤金鑰、密文竄改、重放訊息、時間戳邊界共 7 個）、`activity.rs`（流水帳
順序/上限/清空共 5 個）、`sysinfo.rs`、`shell.rs`。TUI/GUI 互動與一次性腳本則以
手動啟動驗證取代自動化測試（見 constitution 原則 V）。

**Target Platform**: 跨平台（Linux 為主要開發/部署平台；`sysinfo.rs` 另外以
`#[cfg(windows)]`/`#[cfg(not(windows))]` 提供 Windows 與 macOS/Linux 各自的系統
資訊 FFI 呼叫，如 `GetTickCount64`/`GetComputerNameW` 對應 `/proc/uptime`/
`gethostname`/`localtime_r`/`kill`）

**Project Type**: 單一 Rust crate，同時扮演 CLI 工具、TUI 應用程式、與內嵌 Web
服務三種角色（不對應範本的任一種標準專案類型，見下方「Project Structure」的實際
佈局）

**Performance Goals**: 非高流量服務，設計規模是個人與少量自有機器；已知的既有節奏
常數：裝置回報週期 10 秒（`system` client → server）、天氣查詢快取 5 分鐘、活動
流水帳上限 1000 筆（超過自動丟棄最舊）。無需額外訂定新的效能目標。

**Constraints**: 跨網域訊息透過公開 MQTT broker（`broker.emqx.io`）中繼，實測其
單則訊息原始 payload 超過約 11000 bytes 會被靜默丟棄（無錯誤回報），因此跨網域
檔案傳輸切成 `FILE_CHUNK_SIZE = 4 KiB` 一個 chunk、檔案清單分頁預算
`FILE_LIST_PAGE_BUDGET = 4 KiB`；所有跨網域訊息時間戳容許窗口 `±30` 秒
（`REPLAY_WINDOW_SECS`）。

**Scale/Scope**: 單一使用者、少量（個位數到十位數量級）受信任機器；未設計多租戶
或多使用者權限隔離。

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

逐項比對 `.specify/memory/constitution.md`（v1.0.0）的既有實作證據：

| 原則 | 狀態 | 證據 |
|------|------|------|
| I. 安全優先，加密與防重放不可退讓 | ✅ PASS | `crypto.rs`：ChaCha20-Poly1305 全 payload 加密、`Envelope{ts}` + `±30s` 時間窗、已解密 nonce 記錄表防止原封重放；金鑰以 `remote-key` 純文字檔手動佈署且已加入 `.gitignore`；7 個對應單元測試涵蓋正常/錯誤金鑰/竄改/重放/時間戳邊界。 |
| II. 精簡務實，拒絕過度設計 | ✅ PASS | 例：`crypto.rs` 手寫 8 行 `decode_hex` 而非為了讀一個 key 檔案引入 `hex` crate；儲存全部用本機檔案，未引入資料庫；未見為假設中的未來需求預先建立的抽象層。 |
| III. Rust 慣例與明確的錯誤處理 | ⚠ 觀察但不算違規 | 專案中有 170 處 `unwrap()`/`expect()`，多數是 `Mutex::lock().unwrap()`（鎖中毒視為不可恢復的程式錯誤，屬合理例外，非一般執行路徑的可恢復失敗）；`unsafe` 僅出現在 `sysinfo.rs` 對應 Windows/POSIX 系統呼叫（`GetTickCount64`/`gethostname`/`localtime_r`/`kill` 等），為取得跨平台系統資訊所必要，非隨意使用。此為既有事實記錄，不需要 Complexity Tracking 佐證。 |
| IV. CLI/TUI 優先，核心邏輯與介面層解耦 | ✅ PASS | 每個外掛透過 `Plugin::dispatch(cmd, args, out)` 以文字指令驅動；GUI 只是額外呼叫 `panel_text()` 呈現，網頁 `/api/exec` 呼叫的是同一個 `execute_line`——三個介面共用同一份指令直譯邏輯，沒有只能透過 TUI 觸發的功能。 |
| V. 針對關鍵路徑務實測試 | ✅ PASS | 加密/防重放（`crypto.rs`）與活動流水帳（`activity.rs`）等關鍵路徑有自動化測試；TUI 互動與一次性腳本以手動驗證取代，符合原則允許的例外範圍。 |

沒有需要在 Complexity Tracking 中說明的原則違反項目。

## Project Structure

### Documentation (this feature)

```text
specs/001-current-implementation/
├── plan.md              # 本檔案
├── research.md          # Phase 0：既有關鍵技術決策回溯記錄
├── data-model.md         # Phase 1：既有資料模型
├── quickstart.md         # Phase 1：驗證既有系統可運作的操作指南
├── contracts/            # Phase 1：對外介面（Web API／CLI 指令／跨網域協定）
│   ├── web-api.md
│   ├── cli-commands.md
│   └── mqtt-protocol.md
└── checklists/
    └── requirements.md   # /speckit-specify 產出的規格品質檢查清單
```

### Source Code (repository root)

實際既有佈局（單一 crate，非範本的任一種標準選項，依現況記錄）：

```text
src/
├── main.rs              # 進入點：CLI 參數（含隱藏的 --respawn-after）、外掛註冊、
│                         # 啟動 web 伺服器背景執行緒、執行 script.cli/script-local.cli、
│                         # 依 mode 進入 CLI/GUI/headless 前景迴圈
├── shell.rs              # 指令直譯器（CLI/GUI/web /api/exec 共用）、Mode/PanelState、
│                         # remote connect passthrough、upgrade 自我更新流程
├── gui.rs                # ratatui TUI：面板版面、Markdown/語法高亮 notepad 編輯器
├── web.rs                # actix-web 伺服器：REST/SSE/WebSocket 端點（見 contracts/web-api.md）
├── web/frontend.html     # 內嵌單頁瀏覽器前端（含 xterm.js 遠端 shell 終端機）
├── plugin.rs             # Plugin trait、共用 ContextInner、跨網域訊息型別
│                         # （DeviceReport/RemoteRequest/RemoteReply/…，見 data-model.md）
├── plugins/               # 14 個外掛：activities/device/files/gitrepo/global/music/
│                         # notepad/output/qr/remote/remote_output/system/weather/wol
├── crypto.rs              # ChaCha20-Poly1305 AEAD 加解密 + 時間戳/nonce 防重放
├── activity.rs            # 上限筆數的網路活動流水帳
├── sysinfo.rs             # 跨平台系統資訊（含 Windows/POSIX FFI）
└── output.rs              # 共用輸出緩衝區（CLI/GUI/web SSE 共用同一份指令輸出紀錄）
```

沒有獨立的 `tests/` 目錄——單元測試以 `#[cfg(test)] mod tests` 內嵌在需要它們的
原始檔案中（`crypto.rs`/`activity.rs`/`sysinfo.rs`/`shell.rs`），符合 constitution
原則 V「只針對關鍵路徑投入自動化測試」的務實範圍，而非為每個檔案都建立對應測試
檔案的慣例。

**Structure Decision**：維持現況的單一 crate、模組化（`src/*.rs` + `src/plugins/*.rs`）
佈局；不拆分成多個 crate 或 workspace——目前規模（單一使用者、5231 行核心程式碼
+ 14 個外掛）不足以證明拆分的額外複雜度合理（呼應 constitution 原則 II）。

## Complexity Tracking

*Constitution Check 沒有需要說明的違反項目，本節故意留空。*

## Post-Design Constitution Re-check

完成 Phase 1（`data-model.md`、`contracts/`、`quickstart.md`）後重新比對：所有
設計產物都只是把既有程式碼的既有行為寫下來，沒有引入任何新的技術決策或抽象，
因此上方 Constitution Check 表格的五項結論維持不變，無新增違規。
