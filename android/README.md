# cng5 tablet APK

把 `/tablet` 觸控 UI 包成一個可以側載安裝的 Android App（WebView 外殼），
設計細節見 `docs/superpowers/specs/2026-08-02-tablet-apk-design.md`。

## 編譯

第一次使用前，先跑過 `docs/superpowers/plans/2026-08-02-tablet-apk.md`
的 Task 1（安裝 portable JDK 17 / Android SDK / 產生 Gradle wrapper）。
之後每次編譯：

```bash
source android/toolchain-env.sh
cd android
./gradlew assembleDebug
```

APK 產出在 `android/app/build/outputs/apk/debug/app-debug.apk`。

## 安裝到平板

方法一（USB 接電腦，平板開「USB 偵錯」）：

```bash
source android/toolchain-env.sh
adb install -r android/app/build/outputs/apk/debug/app-debug.apk
```

方法二（不用電腦）：把 `app-debug.apk` 傳到平板（例如用雲端硬碟/USB
隨身碟），平板上開啟「允許安裝未知來源 App」，直接點檔案安裝。

## 首次使用

1. 開啟 App，會看到設定畫面（一個網址輸入框）。
2. 輸入 cng5 伺服器的 `/tablet` 網址，例如
   `http://100.100.1.61:9759/tablet`（沒打 `http://` 前綴會自動補上）。
3. 按「儲存並顯示」，之後每次開 App 都會直接全螢幕顯示這個網址。
4. 之後要換網址：在螢幕任一角落按住不放 2 秒，會切回設定畫面（輸入框
   會預先帶入目前存的網址）。

## 手動驗收清單（需要實體平板，開發機做不到）

- [ ] 第一次啟動顯示設定畫面，輸入網址存檔後正確全螢幕顯示 `/tablet`。
- [ ] 四個角落長按 2 秒都能正確切回設定畫面，且欄位帶入目前已存的網址；
      按住不到 2 秒放開則不會觸發。
- [ ] 關掉 Wi-Fi 或停掉伺服器，確認顯示錯誤訊息並持續每 3 秒自動重試；
      恢復連線後能自動接上正常顯示，不用手動重開 App。
- [ ] 平板轉向（如果該平板支援）時 WebView 內容不會重新整理、不會丟失
      操作到一半的狀態（例如 player 分頁播到一半）。
- [ ] 讓 `/tablet` 頁面裡某個次要資源（例如圖片）暫時載入失敗，確認整頁
      不會被誤判成連線失敗而跳出錯誤畫面（如果真的跳出來且沒有自己恢復，
      這是已知的風險點，回報給開發者）。
- [ ] 觀察斷線重試時，畫面會不會偶爾卡在空白/錯誤網頁而沒有真的重試
      （如果發生，這是已知的風險點，回報給開發者）。
