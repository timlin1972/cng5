# Quickstart：驗證現有實作可運作

本指南驗證 spec.md 五個使用者情境（P1~P5）在既有實作中確實可運作。不重複
data-model.md/contracts/ 已有的細節，只列出跑起來要做什麼、預期看到什麼。

## 前置需求

- 已安裝與 `Cargo.toml`（edition 2024）相容的 Rust 工具鏈與 `cargo`
- 專案根目錄可寫入（會產生/讀取 `remote-key`、`notepad/`、`music/` 等本機檔案）
- 若要驗證跨網域情境（P1）：至少兩台能各自對外連上 `broker.emqx.io:1883` 的機器

## 建置與執行既有自動化測試

```bash
cd /home/moxa/cng5
cargo build
cargo test        # 驗證 19 個既有單元測試：crypto/activity/sysinfo/shell
```

`cargo test` 直接涵蓋跨網域協定最關鍵、最難用手動方式驗證的部分：加解密往返、
錯誤金鑰、密文竄改、原封重放拒絕、時間戳窗口邊界（`crypto.rs` 6 個測試）。

## 情境 2（P2）：單一狀態、多介面同時操作

1. 準備一份最小 `script.cli`：
   ```
   mode gui
   plugin enter output
   panel
   rect 0 0 50 100
   show
   ~
   ```
2. `cargo run` 啟動（背景會自動起 web 伺服器於 port 9759）。
3. 瀏覽器開啟 `http://localhost:9759/`，透過網頁的執行指令框呼叫 `plugin enter
   wol` 之類的指令。
4. 回到終端機 TUI，確認同一個外掛的狀態（例如剛剛在瀏覽器新增的 WOL 目標）已
   出現在終端機畫面。
   **預期**：兩種介面看到的是同一份資料，沒有任一邊落後或遺失。

## 情境 3（P3）：群組裝置上線狀態

1. 機器 A：`plugin enter system` → `mode server` → `~`
2. 機器 B：`plugin enter system` → `server <機器A的內網位址>` → `mode client` → `~`
3. 等待至少 10~20 秒（兩個回報週期）。
4. 兩台機器上都執行 `plugin enter device` → `list`。
   **預期**：雙方都能看到彼此的 id/ip/os/version/uptime。

## 情境 1（P1）：跨網域安全遠端操控

1. 在兩台**彼此網路不可達**的機器（分別視為 domain X、domain Y 的伺服器）上：
   - 各自把相同內容的 32-byte 對稱金鑰以 64 個 hex 字元寫入專案根目錄的
     `remote-key` 檔案（兩邊必須逐字元相同）。
   - 各自執行 `plugin enter global` → `bridge <相同的橋接代號>` → `domain X`
     （或 `Y`）→ `~`。
2. 在 domain X 機器上：`plugin enter remote` → `connect Y/<domain-Y機器的id>`。
   **預期**：提示字元變為 `<id>::...`，輸入指令能在遠端機器（domain Y）上執行
   並看到（透過 `remote-output` 面板或 `show output`）其輸出。
3. 手動製造一次重放：可直接執行 `cargo test -p cng5 replayed_message_rejected`
   （已涵蓋此案例，不需要真的攔截網路封包重放）。
   **預期**：第二次打開同一包加密訊息回傳錯誤。

## 情境 4（P4）：個人化小工具（節錄天氣與 WOL）

```
plugin enter weather
add Taipei
show
~
plugin enter wol
add mydesktop AA:BB:CC:DD:EE:FF
wakeon mydesktop
~
```

**預期**：`weather show` 顯示台北目前與未來數天天氣；`wakeon` 對已登記的 MAC
位址送出喚醒封包（實際是否開機取決於目標硬體是否支援並啟用 WOL）。

## 情境 5（P5）：自我更新

```
upgrade
```

**預期**：終端機立刻可繼續輸入其他指令（非阻塞）；背景完成 `git fetch --all`
→ `git reset --hard origin/main` → `cargo build`；若建置失敗，印出錯誤訊息且
服務繼續以目前版本運作；若建置成功，服務在短暫切換後以新版本繼續運作，過程中
`/api/version` 可用來確認版本是否已更新。

## 已知限制（驗證時勿誤判為 bug）

- `remote-key` 不存在或格式錯誤時，任何跨網域功能都會直接回錯，這是預期行為
  （見 research.md §3），不是需要自動修復的錯誤。
- 檔案複製僅支援 `music`/`notepad` 兩個資料夾，對其他資料夾名稱送出 `copy` 會
  被拒絕，這是白名單機制的預期行為。
