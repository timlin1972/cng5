<!--
Sync Impact Report
Version change: [TEMPLATE] → 1.0.0 (initial concrete ratification)
Modified principles: n/a (first fill-in from template placeholders)
Added sections:
  - Core Principles I-V (安全優先 / 精簡務實 / Rust 慣例與錯誤處理 / CLI-TUI 優先 / 關鍵路徑測試)
  - Additional Constraints (跨 domain remote + MQTT 通訊安全規範)
  - Development Workflow (單人維護的務實流程)
  - Governance
Removed sections: none
Templates requiring updates:
  - .specify/templates/plan-template.md ✅ generic "Constitution Check" gate, no hardcoded principle names — no edit needed
  - .specify/templates/spec-template.md ✅ no constitution-specific references — no edit needed
  - .specify/templates/tasks-template.md ✅ no constitution-specific references — no edit needed
  - .claude/skills/speckit-*/SKILL.md ✅ reviewed, generic references only — no edit needed
Follow-up TODOs:
  - TODO(RATIFICATION_DATE): inferred from first git commit (2026-07-16); confirm if the project's
    actual practices predate the repository.
-->

# cng5 Constitution

cng5（Center Next Generation 5）是一個單人維護的 Rust 專案，整合 CLI/TUI 操作介面、
跨 domain 遠端控制與 MQTT 訊息傳輸。以下原則反映目前程式碼中已經在遵循的實務準則，
供未來的功能規劃（`/speckit-plan`、`/speckit-tasks`）與程式碼審查依循。

## Core Principles

### I. 安全優先，加密與防重放不可退讓 (NON-NEGOTIABLE)
所有跨 domain 或透過 MQTT 傳輸的 payload MUST 使用 AEAD 加密（目前為
ChaCha20-Poly1305）進行全 payload 加密；訊息 MUST 附帶時間戳並在 ±30 秒視窗外
的訊息 MUST 被拒絕，以防止重放攻擊。金鑰 MUST NOT 寫死在程式碼或提交進版控管，
只能以檔案方式手動佈署並限制存取權限。任何新增的遠端/MQTT 通道在合併前 MUST
說明其加密與防重放機制，若無法達成 MUST 在文件中明確記錄風險與理由。
**理由**：cng5 的核心價值是安全的跨網域遠端控制；一旦通訊層被竄改或重放，後果
是遠端主機被任意操控，此風險無法透過事後修補彌補，必須在設計期就排除。

### II. 精簡務實，拒絕過度設計 (YAGNI)
新功能 MUST 從解決當下的具體需求出發，不得為了假設中的未來需求預先建立抽象層、
設定系統或外掛框架。三行重複的邏輯優於一個只用一次的抽象。修 bug 時只修 bug，
不順手重構無關程式碼；一次性操作不需要為它建立可重用的 helper。
**理由**：專案由單人維護，過度抽象會拉長之後回頭理解程式碼的時間，得不償失。

### III. Rust 慣例與明確的錯誤處理
可能失敗的操作 MUST 回傳 `Result`（透過 `anyhow` 或明確的錯誤型別），一般執行
路徑上 MUST NOT 使用 `unwrap()` / `expect()`，除非是啟動階段的組態載入等「失敗
即代表程式無法繼續執行」的情境，且 SHOULD 附上簡短理由註解。`unsafe` 區塊 MUST
附上解釋為何安全的註解才能合併。
**理由**：遠端控制工具若因未處理的 panic 而中斷，使用者往往無法即時介入修復，
明確的錯誤處理是維持可用性的最低要求。

### IV. CLI/TUI 優先，核心邏輯與介面層解耦
每個功能 MUST 能透過 CLI（`script.cli` / plugin 指令）驅動，不得只能透過 TUI
互動才能觸發。核心邏輯（協定解析、加密、plugin 執行）SHOULD 獨立於 `ratatui`、
`actix-web`、`crossterm` 等介面框架之外，方便日後替換介面或撰寫測試。
**理由**：文字化的輸入輸出讓行為可被腳本化、可被記錄，也讓核心邏輯不必依賴
UI 執行環境即可驗證。

### V. 針對關鍵路徑務實測試
加密／解密、協定訊息解析（含防重放判斷）與 plugin 指令解析等邏輯 MUST 有自動
化測試涵蓋正常與邊界情況（例如時間戳超出 ±30 秒視窗、payload 被竄改）。TUI/
GUI 互動與一次性腳本則可用手動執行驗證取代自動化測試，但 MUST 在合併前實際
執行過一次確認行為符合預期。
**理由**：安全與協定相關的邏輯出錯代價高且難以在事後用人工檢查發現，值得投入
自動化測試；介面互動則測試投資報酬率低，手動驗證已足夠。

## Additional Constraints

- 跨 domain remote 與 MQTT 相關設計細節（金鑰佈署方式、payload 格式、防重放
  視窗）以 `crypto.rs` 與相關 plugin 程式碼為準；任何調整 MUST 同步更新專案
  記憶（memory）中對應的設計紀錄，避免文件與實作不同步。
- 依賴套件的新增 SHOULD 有明確用途（見 `Cargo.toml`），避免引入僅為了單一小
  功能就整合的大型框架。

## Development Workflow

- 本專案由單一維護者（Tim HC Lin）開發，不強制要求第三方 code review，但每次
  合併前 MUST 自行確認變更符合本憲章的原則，特別是原則 I（安全）與原則 V
  （關鍵路徑測試）。
- 涉及加密、協定或防重放邏輯的變更 MUST 在提交前跑過對應的自動化測試
  （`cargo test`）。
- 涉及 TUI/CLI 行為的變更 SHOULD 實際啟動程式驗證一次黃金路徑與已知的邊界情況。

## Governance

本憲章的效力高於其他非正式的開發習慣；若程式碼與本憲章衝突，以本憲章為準，
並應盡快修正程式碼或以修訂憲章的方式更新原則。

**修訂程序**：直接修改本檔案（`.specify/memory/constitution.md`），在同一次
修訂中更新版本號與 `Last Amended` 日期，並在 commit message 中說明修訂原因。

**版本規則（語意化版本）**：
- MAJOR：移除或重新定義既有原則，造成不相容的治理變動。
- MINOR：新增原則或章節，或對既有原則做出實質擴充。
- PATCH：文字澄清、措辭調整、typo 修正等非語意變動。

**合規檢查**：使用 `/speckit-plan` 產出的計畫 MUST 通過 `Constitution Check`
章節的檢核；若必須違反某項原則，MUST 在計畫的 Complexity Tracking 中說明理由
與替代方案為何不可行。

**Version**: 1.0.0 | **Ratified**: 2026-07-16 | **Last Amended**: 2026-07-24
