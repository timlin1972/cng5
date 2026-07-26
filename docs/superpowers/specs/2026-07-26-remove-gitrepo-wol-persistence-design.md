# 移除 gitrepo/wol plugin 的目錄式設定持久化

日期：2026-07-26

## 目的

`gitrepo` plugin（監控目錄清單）跟 `wol` plugin（命名裝置清單）目前會把設定存進
`gitrepo/watched.txt`、`wol/devices.txt`，啟動時讀回來，這樣重開程式不用重新
`add`。但使用者實際上是把這些 `add` 指令直接寫進 `script-local.cli`（每次啟動
都會執行，見 `main.rs`），所以這個目錄式持久化機制變得多餘，拿掉它。

## 範圍

- `src/plugins/gitrepo.rs`：移除 `GITREPO_DIR`/`WATCHED_FILE` 常數、
  `watched_path()`/`load_watched()`/`save_watched()`；`new()` 的 `watched`
  改成空的 `Vec::new()`；`add`/`remove`/`clear` 拿掉存檔呼叫；`MANUAL_TEXT`
  刪掉「監控目錄清單會存檔，重開程式不用重新 add」這句話。
- `src/plugins/wol.rs`：移除 `WOL_DIR`/`DEVICES_FILE` 常數、
  `devices_path()`/`load_devices()`/`save_devices()`；`new()` 的 `devices`
  改成空的 `HashMap::new()`；`add`/`remove` 拿掉存檔呼叫。`MANUAL_TEXT` 本來
  就沒提到持久化，不用改。
- 刪除磁碟上現有的 `gitrepo/`、`wol/` 整個目錄（含 `watched.txt`/
  `devices.txt`）——使用者已確認這兩份資料不需要保留。

## 不做的事

- 不動 `music`/`notepad` plugin 的資料夾——那些放的是實際內容檔案（音樂檔、
  筆記檔），不是設定清單，跟這次要拿掉的機制是不同的東西。
- 不動 `.gitignore` 裡 `/gitrepo`、`/wol` 這兩行——留著沒有壞處。
- 不新增其他功能（例如把 add 過的東西輸出成 script-local.cli 可以貼的格式）。

## 測試

- 這兩個檔案本來就沒有既有的 `#[cfg(test)]` 測試模組，這次也不新增——純粹是
  移除一段 I/O 邏輯，改動範圍小且直接，靠 `cargo build`/`cargo test`（確認其他
  測試沒被影響）加上手動操作 `add`/`remove`/`list` 確認行為正確即可。
