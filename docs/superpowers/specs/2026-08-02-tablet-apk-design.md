# tablet APK 設計

## 目的

把已完成的 `/tablet` 觸控 UI 包成一個可以直接安裝在實體 Android 平板上的
App：開機/點圖示就全螢幕顯示 `/tablet` 頁面，不需要手動開瀏覽器、打網址、
再切成全螢幕。

## 範圍

- 新增一個獨立的 Android Gradle 專案（`android/` 目錄），跟現有 Rust
  後端（`src/`）、桌面版/平板版網頁（`src/web/`）完全零觸碰、互不相依。
  這個 App 純粹是一個指向既有 `/tablet` 網址的 WebView 外殼，不重新實作
  任何頁面內容或後端邏輯。
- 不做：APK 簽章發布、開機自動啟動、螢幕常亮、美術圖示、TWA/PWA 安裝、
  Cordova/Capacitor 這類額外框架。這些都是可能的後續需求，這次不做
  （YAGNI）。

## 技術選型

**原生 WebView 外殼**（Kotlin + Android SDK，單一 Activity）。

考慮過的其他方案：
- **Trusted Web Activity（Bubblewrap）**：需要頁面是 HTTPS 並設定 Digital
  Asset Links 網域驗證。目前 `/tablet` 是區網 `http://<ip>:9759/tablet`，
  不符合，排除。
- **Cordova/Capacitor**：多一整層 Node/npm 建置工具鏈才能包出一個「顯示
  一個網址」的殼，對這個需求是過度工程，排除。

## 架構

新增 `android/` 目錄，內含一個最小 Gradle/Kotlin 專案：

- **套件名稱**：`com.moxa.cng5.tablet`
- **App 名稱**：`cng5 tablet`
- **minSdk 24 / targetSdk 34 / compileSdk 34**
- **圖示**：先用 Android 預設圖示，不做美術。

### `MainActivity`（唯一畫面）

用一個 `FrameLayout` 疊兩層，靠顯示/隱藏切換（不開第二個 Activity，避免
額外的生命週期/回退堆疊要處理）：

1. **`SettingsView`**：一個 `EditText`（輸入完整網址，例如
   `http://100.100.1.61:9759/tablet`）+ 一個「儲存並顯示」按鈕。按下後把
   網址存進 `SharedPreferences`，切到 `WebView` 層。
2. **`WebView` 層**：全螢幕 immersive（隱藏狀態列/導覽列），
   `loadUrl(savedUrl)`。

**啟動流程：**
- 讀 `SharedPreferences` 裡的網址。
- 沒存過網址 → 顯示 `SettingsView`。
- 已存過網址 → 隱藏 `SettingsView`、顯示 `WebView`、`loadUrl`、進入
  immersive 全螢幕模式。

**回設定的方式：** 螢幕四個角落各有一塊透明的觸控區（約 80dp 見方，蓋在
`WebView` 之上，不吃掉其餘區域的觸控事件），長按 2 秒觸發切回
`SettingsView`（`WebView` 不銷毀，只是隱藏，网址欄位預先帶入目前存的網址
方便修改）。

**載入失敗處理：** `WebView.onReceivedError` 觸發時，隱藏 `WebView`、顯示
一段純文字錯誤訊息（例如「連不到 <url>，3 秒後自動重試」），啟動一個
每 3 秒的 timer 呼叫 `loadUrl` 重試，直到成功（`onPageFinished` 觸發後
停止 timer、隱藏錯誤訊息、顯示 `WebView`）。不需要手動重試按鈕——平板
是常駐顯示裝置，自動重試即可。

### `AndroidManifest.xml`

- 權限：只要 `<uses-permission android:name="android.permission.INTERNET" />`。
- `<application android:usesCleartextTraffic="true">`：Android 9+
  預設封鎖明文 HTTP，區網用 IP 打 `/tablet` 一定要開這個。
- `MainActivity` 設 `android:configChanges="orientation|screenSize"`，避免
  平板轉向時整個 Activity 重建、`WebView` 重新載入、丟失使用者當下操作
  到一半的狀態（例如 player 分頁播到一半）。

## 資料流

```
啟動 App
  → 讀 SharedPreferences["server_url"]
  → 空值？
      是 → 顯示 SettingsView，等使用者輸入網址並按「儲存並顯示」
      否 → 顯示 WebView，loadUrl(url)，進 immersive 全螢幕
  → WebView 載入失敗？
      是 → 顯示錯誤訊息，每 3 秒自動重試 loadUrl
      否（onPageFinished）→ 正常顯示
  → 使用者在任一角落長按 2 秒
      → 隱藏 WebView，顯示 SettingsView（網址欄位帶入目前值）
```

沒有其他資料流——App 本身不呼叫任何 API、不解析任何 `/tablet` 頁面內容，
純粹是一層網址容器。

## 錯誤處理

- 網址格式錯誤（例如缺 `http://` 前綴）：`SettingsView` 存檔前檢查是否以
  `http://` 或 `https://` 開頭，不是的話直接補上 `http://` 前綴再存檔，
  不彈錯誤視窗（平板操作情境下，跳錯誤視窗不如直接容錯處理）。
- 載入失敗：見上方「載入失敗處理」，自動重試，不需要使用者介入。
- 沒有網路權限以外的例外情境（例如伺服器回 4xx/5xx）：`WebView` 預設行為
  即可（顯示伺服器回傳的內容），不特別攔截。

## 測試計畫

這台開發機沒有實體 Android 平板或模擬器可以互動操作，測試分兩層：

**開發機這邊可驗證的：**
- `./gradlew assembleDebug` 編譯成功，產出 `app-debug.apk`。
- 用 `aapt dump badging app-debug.apk`（或 `apkanalyzer manifest print`）
  確認套件名稱、`minSdk`/`targetSdk`、`INTERNET` 權限、
  `usesCleartextTraffic` 都符合設計。

**需要使用者在實體平板上確認的（開發機做不到）：**
- 把 `app-debug.apk` 傳到平板、開啟「安裝未知來源」、實際安裝。
- 第一次啟動顯示 `SettingsView`，輸入網址存檔後正確全螢幕顯示 `/tablet`。
- 四個角落長按 2 秒能正確切回 `SettingsView`，且欄位帶入目前已存的網址。
- 關掉 Wi-Fi/斷開伺服器，確認顯示錯誤訊息並持續自動重試；恢復連線後能
  自動接上正常顯示，不用手動重開 App。
- 平板轉向（如果該平板支援）時 `WebView` 內容不會重新整理、不會丟失
  操作狀態。

## 開放性問題 / 後續可能需要但這次不做

- 開機自動啟動、螢幕常亮：使用者這輪明確選擇不需要，若之後需要常駐
  展示用途可以再加。
- 簽章發布/上架 Google Play：這次只做 debug APK 側載，不涉及簽章金鑰
  管理。
- App 圖示美術：先用預設圖示。
