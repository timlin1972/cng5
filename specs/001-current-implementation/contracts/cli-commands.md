# Contract: Shell 指令文法（CLI／TUI／web `/api/exec` 共用）

## 模式狀態機

```
Root ──plugin enter <name>──▶ 外掛層 ──panel（僅 GUI）──▶ 面板設定層
  ▲__________________________~ / ...（任一層跳回 root）___________|
外掛層/面板層 ──..──▶ 上一層
```

指令支援前綴縮寫（例如 `p e wol` 等同 `plugin enter wol`）。

## Root 層指令

| 指令 | 說明 |
|------|------|
| `plugin show` | 列出目前有哪些外掛 |
| `plugin enter <name>` | 進入指定外掛 |
| `mode cli` / `mode gui` | 切換 CLI／GUI 前景畫面 |
| `shell` | 借用目前終端機開一個真正的 host shell，`exit` 返回 |
| `upgrade` | 觸發自我更新（見 research.md §6），非阻塞、背景執行 |
| `history` | 列出之前執行過的指令 |
| `!<n>` | 重新執行 history 第 n 筆指令 |
| `exit` / `quit` | 結束程式 |
| `help` | 目前層級可用指令的一行式清單 |
| `manual` | 目前層級較完整的說明與範例 |
| `?`（按鍵，非 Enter 送出） | 顯示上下文說明 |

## 外掛層通用指令

每個外掛在 `plugin enter <name>` 後額外提供 `help`/`manual`（同上）與（僅 GUI）
`panel` 子層；面板子層指令：`rect x y w h` / `show` / `hidden` / `activate`。

## 外掛專屬指令（節錄，各外掛完整清單見對應 `manual_text()`）

| 外掛 | 指令 |
|------|------|
| `wol` | `add <name> <mac>` / `remove <name>` / `wakeon <name\|mac>` / `status` |
| `weather` | `show` / `add <city>` / `remove <city>` |
| `notepad` | `list`（編輯僅限 GUI 面板內互動） |
| `music` | `download <url>` / `list` |
| `gitrepo` | `add <dir>` / `remove <dir>` / `clear` / `list` / `scan` |
| `activities` | `list [n]` / `status` / `limit <n>` / `clear` |
| `device` | `list` / `status` |
| `system` | `status` / `version` / `mode standalone\|server\|client` / `server <ip>` |
| `global` | `domain <name>` / `bridge <id>` / `status` / `list` / `clear` |
| `remote` | `connect <id>` / `connect <domain>/<id>` / `status` |
| `remote-output` | `show <panel-name>` |
| `files` | `copy <folder> to\|from <id>` / `copy <folder> to\|from <domain>/<id>` / `status` |
| `qr` | （無指令，僅面板；PgUp/PgDn 於 GUI 切換顯示目標） |
| `output` | （無指令，僅面板：即時捲動指令輸出紀錄） |

## Remote 連線 passthrough 語意

`remote` 外掛下 `connect <id>` 或 `connect <domain>/<id>` 成功後，Shell 提示字元
變為 `<id>::<遠端提示字元>`，此後每一行輸入原封不動轉送到遠端機器執行（如同直接
在該機器上操作）；`shell` 在此連線狀態下開啟的是遠端機器的真實 host shell（透過
WebSocket/PTY，逐位元組傳輸）；`upgrade` 在此連線狀態下觸發的是**遠端機器**的
更新流程，且更新指令送出後本地連線自動中斷。`~`/`...` 從遠端連線狀態跳回本機
root 會中斷此連線。
