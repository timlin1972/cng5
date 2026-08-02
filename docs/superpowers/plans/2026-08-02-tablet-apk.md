# tablet APK Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增一個獨立的 Android Gradle 專案（`android/`），把已完成的 `/tablet` 觸控 UI 包成一個可以側載安裝的 debug APK：設定畫面存伺服器網址、全螢幕 immersive 顯示、角落長按回設定、載入失敗自動重試。

**Architecture:** 單一 `MainActivity`（純 `android.app.Activity`，不用 AppCompat）疊兩層：`settings_view`（`EditText` + 存檔按鈕）跟 `WebView`，用顯示/隱藏切換，不開第二個 Activity。四個螢幕角落各放一個 80dp 透明 `FrameLayout` 偵測「按住 2 秒」手勢用來切回設定畫面。`WebViewClient.onReceivedError` 觸發時顯示錯誤文字並每 3 秒自動重試 `loadUrl`。跟現有 Rust 後端（`src/`）、既有網頁（`src/web/`）完全零觸碰。

**Tech Stack:** Kotlin + Android SDK（Gradle Kotlin DSL），AGP 8.7.2、Kotlin 2.0.21、Gradle（版本由 `services.gradle.org/versions/current` 動態取得）、JDK 17（Eclipse Temurin，portable tarball，不裝進系統）、Android cmdline-tools + platform-tools + build-tools;34.0.0（同樣 portable，不用 root/sudo）。

## Global Constraints

- 套件名稱固定 `com.moxa.cng5.tablet`，App 名稱固定 `cng5 tablet`。
- `minSdk 24` / `targetSdk 34` / `compileSdk 34`。
- App 圖示用系統內建的 `@android:drawable/sym_def_app_icon`，不做美術、不產生自訂 mipmap 資源。
- Manifest 只要 `android.permission.INTERNET` 一個權限，`<application>` 要有 `android:usesCleartextTraffic="true"`（區網 IP 走 HTTP，Android 9+ 預設會擋）。
- 不做：APK 簽章發布/上架、開機自動啟動、螢幕常亮、Cordova/Capacitor/TWA 這類額外框架或工具鏈。
- 角落長按判定固定 2000ms（不是系統預設的 long-click ~500ms，需要自己用 `Handler.postDelayed` 實作）。
- 載入失敗後固定每 3000ms 自動重試一次 `loadUrl`，不需要使用者手動按重試鈕。
- 這整個子專案跟現有 Rust 後端（`src/`）、既有網頁（`src/web/frontend.html`、`src/web/tablet.html`）完全零觸碰，不修改任何既有檔案（只新增 `android/` 目錄跟根目錄 `.gitignore` 追加幾行）。
- 這台開發機沒有實體 Android 平板或模擬器，所有「編譯成功」等級的驗證由本計畫的 `./gradlew` 指令完成；實際觸控/顯示行為需要使用者在真實平板上確認（Task 6 有完整清單）。

---

### Task 1: Android 建置工具鏈（JDK 17 + SDK + Gradle wrapper，portable，不用 sudo）

**Files:**
- Create: `android/toolchain-env.sh`
- Create: `android/gradlew`、`android/gradlew.bat`、`android/gradle/wrapper/gradle-wrapper.properties`、`android/gradle/wrapper/gradle-wrapper.jar`（由 `gradle wrapper` 任務產生）
- Modify: `.gitignore`（追加 Android 相關忽略規則）

**Interfaces:**
- Produces（後面所有 Task 都會用到）：`android/toolchain-env.sh`，`source` 之後會匯出 `JAVA_HOME`、`ANDROID_HOME`、把兩者的 `bin` 目錄加進 `PATH`。之後每個 Task 執行 `./gradlew` 前都要先 `source android/toolchain-env.sh`。

- [ ] **Step 1: 取得目前 Gradle 版本與下載網址**

```bash
python3 -c "
import json, urllib.request
data = json.load(urllib.request.urlopen('https://services.gradle.org/versions/current'))
print(data['version'])
print(data['downloadUrl'])
" > /tmp/gradle_current.txt
cat /tmp/gradle_current.txt
```

Expected: 兩行輸出，第一行是版本號（例如 `8.10.2`），第二行是
`https://services.gradle.org/distributions/gradle-<版本>-bin.zip` 這樣的網址。

- [ ] **Step 2: 下載並解壓縮 Gradle（只當 bootstrap 用，之後都改用 `./gradlew`）**

```bash
GRADLE_VERSION=$(sed -n '1p' /tmp/gradle_current.txt)
GRADLE_URL=$(sed -n '2p' /tmp/gradle_current.txt)
mkdir -p /home/moxa/.android-toolchain
curl -L -o /tmp/gradle-bin.zip "$GRADLE_URL"
unzip -q -o /tmp/gradle-bin.zip -d /home/moxa/.android-toolchain/gradle-bootstrap
ls /home/moxa/.android-toolchain/gradle-bootstrap
```

Expected: 看到一個 `gradle-<版本>` 目錄，裡面有 `bin/gradle`。

- [ ] **Step 3: 下載並解壓縮 portable JDK 17（Eclipse Temurin，不裝進系統、不用 sudo）**

```bash
curl -L -o /tmp/jdk17.tar.gz \
  "https://api.adoptium.net/v3/binary/latest/17/ga/linux/x64/jdk/hotspot/normal/eclipse"
tar xzf /tmp/jdk17.tar.gz -C /home/moxa/.android-toolchain
ls -d /home/moxa/.android-toolchain/jdk-17*
```

Expected: 看到一個 `jdk-17.x.x+xx` 目錄。

- [ ] **Step 4: 找到目前的 Android command line tools 下載網址並下載解壓縮**

用 WebFetch 工具查 `https://developer.android.com/studio` 頁面上「Command
line tools only」區塊 Linux 版本的 zip 下載連結（網址格式是
`https://dl.google.com/android/repository/commandlinetools-linux-<數字版本>_latest.zip`），
把查到的網址填進 `CMDLINE_TOOLS_URL`。如果 WebFetch 查不到或頁面結構
變了，先試已知可用的版本當備援：
`https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip`。

```bash
CMDLINE_TOOLS_URL="（填入 WebFetch 查到的網址，或上面那個備援網址）"
curl -L -o /tmp/cmdline-tools.zip "$CMDLINE_TOOLS_URL"
rm -rf /tmp/cmdline-tools-extract && mkdir -p /tmp/cmdline-tools-extract
unzip -q /tmp/cmdline-tools.zip -d /tmp/cmdline-tools-extract
mkdir -p /home/moxa/.android-toolchain/android-sdk/cmdline-tools
mv /tmp/cmdline-tools-extract/cmdline-tools \
   /home/moxa/.android-toolchain/android-sdk/cmdline-tools/latest
ls /home/moxa/.android-toolchain/android-sdk/cmdline-tools/latest/bin
```

Expected: 看到 `sdkmanager`、`avdmanager` 等執行檔。

- [ ] **Step 5: 寫 `android/toolchain-env.sh`**

```bash
mkdir -p /home/moxa/cng5/android
```

寫入 `/home/moxa/cng5/android/toolchain-env.sh`：

```bash
#!/usr/bin/env bash
# Portable Android 建置工具鏈（JDK 17 + SDK cmdline-tools），裝在
# ~/.android-toolchain，不動系統既有的 Java 8，也不需要 sudo。
export JAVA_HOME
JAVA_HOME=$(ls -d /home/moxa/.android-toolchain/jdk-17*/ | head -1)
export ANDROID_HOME=/home/moxa/.android-toolchain/android-sdk
export PATH="$JAVA_HOME/bin:$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$PATH"
```

- [ ] **Step 6: 安裝 SDK 套件（platform-tools/platform 34/build-tools 34.0.0）**

```bash
source /home/moxa/cng5/android/toolchain-env.sh
yes | sdkmanager --licenses
sdkmanager "platform-tools" "platforms;android-34" "build-tools;34.0.0"
sdkmanager --list_installed
```

Expected: `--list_installed` 列出 `platform-tools`、`platforms;android-34`、
`build-tools;34.0.0` 三項。

- [ ] **Step 7: 產生 Gradle wrapper（在 `android/` 目錄裡）**

```bash
GRADLE_VERSION=$(sed -n '1p' /tmp/gradle_current.txt)
source /home/moxa/cng5/android/toolchain-env.sh
cd /home/moxa/cng5/android
/home/moxa/.android-toolchain/gradle-bootstrap/gradle-$GRADLE_VERSION/bin/gradle \
  wrapper --gradle-version "$GRADLE_VERSION" --distribution-type bin
ls gradlew gradlew.bat gradle/wrapper
```

Expected: `gradlew`、`gradlew.bat`、`gradle/wrapper/gradle-wrapper.properties`、
`gradle/wrapper/gradle-wrapper.jar` 都存在。

- [ ] **Step 8: 驗證 wrapper 可用**

```bash
source /home/moxa/cng5/android/toolchain-env.sh
cd /home/moxa/cng5/android
./gradlew --version
```

Expected: 輸出裡 `Gradle` 版本跟剛剛取得的版本一致，`JVM` 那行顯示
`17.x.x`（不是系統原本的 Java 8）。

- [ ] **Step 9: `.gitignore` 追加 Android 相關規則**

在 `/home/moxa/cng5/.gitignore` 檔尾追加：

```
# Android（tablet APK 子專案）
/android/local.properties
/android/.gradle
/android/build
/android/*/build
/android/*.iml
/android/.idea
```

- [ ] **Step 10: Commit**

```bash
cd /home/moxa/cng5
git add android/toolchain-env.sh android/gradlew android/gradlew.bat \
  android/gradle/wrapper/gradle-wrapper.properties \
  android/gradle/wrapper/gradle-wrapper.jar .gitignore
git commit -m "$(cat <<'EOF'
新增 android/：Gradle wrapper + portable 建置工具鏈設定腳本

JDK 17／Android SDK cmdline-tools 都裝在 ~/.android-toolchain（不動系統
既有 Java 8，不用 sudo），toolchain-env.sh 匯出 JAVA_HOME/ANDROID_HOME/
PATH，之後每次跑 ./gradlew 前都要先 source 這個腳本。
EOF
)"
```

---

### Task 2: 專案骨架（可編譯出空白 APK）

**Files:**
- Create: `android/settings.gradle.kts`
- Create: `android/build.gradle.kts`
- Create: `android/gradle.properties`
- Create: `android/app/build.gradle.kts`
- Create: `android/app/src/main/AndroidManifest.xml`
- Create: `android/app/src/main/res/layout/activity_main.xml`
- Create: `android/app/src/main/java/com/moxa/cng5/tablet/MainActivity.kt`

**Interfaces:**
- Consumes: Task 1 的 `android/toolchain-env.sh`（`source` 後才有 `JAVA_HOME`/`ANDROID_HOME`/`./gradlew` 可用的環境）。
- Produces（後面 Task 都會用到，view id 定義在此，之後不再變動）：
  - Layout `R.layout.activity_main`，內含 view id：`settings_view`（容器）、`url_input`（`EditText`）、`save_button`（`Button`）、`webview`（`WebView`）、`error_text`（`TextView`）、`corner_tl`/`corner_tr`/`corner_bl`/`corner_br`（四個 `FrameLayout`）。
  - `MainActivity : Activity()`，`onCreate` 目前只有 `setContentView(R.layout.activity_main)`。

- [ ] **Step 1: `android/settings.gradle.kts`**

```kotlin
pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}
dependencyResolutionManagement {
    repositories {
        google()
        mavenCentral()
    }
}
rootProject.name = "cng5-tablet"
include(":app")
```

- [ ] **Step 2: `android/build.gradle.kts`**

```kotlin
plugins {
    id("com.android.application") version "8.7.2" apply false
    id("org.jetbrains.kotlin.android") version "2.0.21" apply false
}
```

- [ ] **Step 3: `android/gradle.properties`**

```
org.gradle.jvmargs=-Xmx2048m
```

- [ ] **Step 4: `android/app/build.gradle.kts`**

```kotlin
plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.moxa.cng5.tablet"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.moxa.cng5.tablet"
        minSdk = 24
        targetSdk = 34
        versionCode = 1
        versionName = "1.0"
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }
}

dependencies {
    testImplementation("junit:junit:4.13.2")
}
```

- [ ] **Step 5: `android/app/src/main/AndroidManifest.xml`**

```xml
<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android">

    <uses-permission android:name="android.permission.INTERNET" />

    <application
        android:label="cng5 tablet"
        android:icon="@android:drawable/sym_def_app_icon"
        android:usesCleartextTraffic="true"
        android:theme="@android:style/Theme.Material.Light.NoActionBar">
        <activity
            android:name=".MainActivity"
            android:exported="true"
            android:configChanges="orientation|screenSize">
            <intent-filter>
                <action android:name="android.intent.action.MAIN" />
                <category android:name="android.intent.category.LAUNCHER" />
            </intent-filter>
        </activity>
    </application>
</manifest>
```

- [ ] **Step 6: `android/app/src/main/res/layout/activity_main.xml`**

```xml
<?xml version="1.0" encoding="utf-8"?>
<FrameLayout xmlns:android="http://schemas.android.com/apk/res/android"
    android:id="@+id/root"
    android:layout_width="match_parent"
    android:layout_height="match_parent"
    android:background="#111318">

    <LinearLayout
        android:id="@+id/settings_view"
        android:layout_width="match_parent"
        android:layout_height="match_parent"
        android:orientation="vertical"
        android:gravity="center"
        android:padding="32dp"
        android:visibility="visible">

        <EditText
            android:id="@+id/url_input"
            android:layout_width="match_parent"
            android:layout_height="wrap_content"
            android:hint="http://192.168.1.10:9759/tablet"
            android:inputType="textUri" />

        <Button
            android:id="@+id/save_button"
            android:layout_width="wrap_content"
            android:layout_height="wrap_content"
            android:layout_marginTop="16dp"
            android:text="儲存並顯示" />
    </LinearLayout>

    <WebView
        android:id="@+id/webview"
        android:layout_width="match_parent"
        android:layout_height="match_parent"
        android:visibility="gone" />

    <TextView
        android:id="@+id/error_text"
        android:layout_width="match_parent"
        android:layout_height="match_parent"
        android:gravity="center"
        android:textColor="#FFFFFF"
        android:textSize="16sp"
        android:visibility="gone" />

    <FrameLayout
        android:id="@+id/corner_tl"
        android:layout_width="80dp"
        android:layout_height="80dp"
        android:layout_gravity="top|start" />

    <FrameLayout
        android:id="@+id/corner_tr"
        android:layout_width="80dp"
        android:layout_height="80dp"
        android:layout_gravity="top|end" />

    <FrameLayout
        android:id="@+id/corner_bl"
        android:layout_width="80dp"
        android:layout_height="80dp"
        android:layout_gravity="bottom|start" />

    <FrameLayout
        android:id="@+id/corner_br"
        android:layout_width="80dp"
        android:layout_height="80dp"
        android:layout_gravity="bottom|end" />
</FrameLayout>
```

- [ ] **Step 7: `android/app/src/main/java/com/moxa/cng5/tablet/MainActivity.kt`**

```kotlin
package com.moxa.cng5.tablet

import android.app.Activity
import android.os.Bundle

class MainActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
    }
}
```

- [ ] **Step 8: 編譯驗證**

```bash
source /home/moxa/cng5/android/toolchain-env.sh
cd /home/moxa/cng5/android
./gradlew assembleDebug
ls app/build/outputs/apk/debug/app-debug.apk
```

Expected: `BUILD SUCCESSFUL`，`app-debug.apk` 存在。

- [ ] **Step 9: Commit**

```bash
cd /home/moxa/cng5
git add android/settings.gradle.kts android/build.gradle.kts \
  android/gradle.properties android/app/build.gradle.kts \
  android/app/src/main/AndroidManifest.xml \
  android/app/src/main/res/layout/activity_main.xml \
  android/app/src/main/java/com/moxa/cng5/tablet/MainActivity.kt
git commit -m "$(cat <<'EOF'
android/：專案骨架，可編譯出空白 APK

套件名稱 com.moxa.cng5.tablet，minSdk 24/targetSdk 34/compileSdk 34，
圖示用系統內建 sym_def_app_icon 不做美術。layout 先把後面幾個 task
會用到的 view（settings_view/url_input/save_button/webview/error_text/
四個角落 corner_*）都定義好，MainActivity 目前只是空殼。
EOF
)"
```

---

### Task 3: 網址正規化（TDD）＋ SharedPreferences 儲存 ＋ 設定畫面接上顯示切換

**Files:**
- Create: `android/app/src/main/java/com/moxa/cng5/tablet/UrlNormalizer.kt`
- Create: `android/app/src/test/java/com/moxa/cng5/tablet/UrlNormalizerTest.kt`
- Create: `android/app/src/main/java/com/moxa/cng5/tablet/UrlPrefs.kt`
- Modify: `android/app/src/main/java/com/moxa/cng5/tablet/MainActivity.kt`

**Interfaces:**
- Consumes: Task 2 的 layout view id（`settings_view`/`url_input`/`save_button`/`webview`）。
- Produces（Task 4 會用到）：
  - `UrlNormalizer.normalize(input: String): String`
  - `UrlPrefs(context: Context)`，方法 `get(): String?`、`save(url: String)`
  - `MainActivity` 私有方法 `showSettings()`、`showWebViewFor(url: String)`（Task 4 會擴充 `showWebViewFor` 的內容，但方法簽章不變）。

- [ ] **Step 1: 寫失敗的測試 `UrlNormalizerTest.kt`**

```kotlin
package com.moxa.cng5.tablet

import org.junit.Assert.assertEquals
import org.junit.Test

class UrlNormalizerTest {
    @Test
    fun addsHttpPrefixWhenMissing() {
        assertEquals(
            "http://100.100.1.61:9759/tablet",
            UrlNormalizer.normalize("100.100.1.61:9759/tablet")
        )
    }

    @Test
    fun leavesHttpUrlUnchanged() {
        assertEquals("http://foo/tablet", UrlNormalizer.normalize("http://foo/tablet"))
    }

    @Test
    fun leavesHttpsUrlUnchanged() {
        assertEquals("https://foo/tablet", UrlNormalizer.normalize("https://foo/tablet"))
    }

    @Test
    fun trimsWhitespace() {
        assertEquals("http://foo", UrlNormalizer.normalize("  foo  "))
    }
}
```

- [ ] **Step 2: 跑測試確認失敗**

```bash
source /home/moxa/cng5/android/toolchain-env.sh
cd /home/moxa/cng5/android
./gradlew testDebugUnitTest --tests "com.moxa.cng5.tablet.UrlNormalizerTest"
```

Expected: `BUILD FAILED`，錯誤訊息包含 `unresolved reference: UrlNormalizer`。

- [ ] **Step 3: 實作 `UrlNormalizer.kt`**

```kotlin
package com.moxa.cng5.tablet

object UrlNormalizer {
    fun normalize(input: String): String {
        val trimmed = input.trim()
        return if (trimmed.startsWith("http://") || trimmed.startsWith("https://")) {
            trimmed
        } else {
            "http://$trimmed"
        }
    }
}
```

- [ ] **Step 4: 跑測試確認通過**

```bash
source /home/moxa/cng5/android/toolchain-env.sh
cd /home/moxa/cng5/android
./gradlew testDebugUnitTest --tests "com.moxa.cng5.tablet.UrlNormalizerTest"
```

Expected: `BUILD SUCCESSFUL`，4 個測試都 PASS。

- [ ] **Step 5: 實作 `UrlPrefs.kt`**

```kotlin
package com.moxa.cng5.tablet

import android.content.Context

class UrlPrefs(context: Context) {
    private val prefs = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)

    fun get(): String? = prefs.getString(KEY_URL, null)

    fun save(url: String) {
        prefs.edit().putString(KEY_URL, url).apply()
    }

    companion object {
        private const val PREFS_NAME = "tablet_prefs"
        private const val KEY_URL = "server_url"
    }
}
```

- [ ] **Step 6: 修改 `MainActivity.kt`，接上設定畫面／WebView 顯示切換**

```kotlin
package com.moxa.cng5.tablet

import android.app.Activity
import android.os.Bundle
import android.view.View
import android.webkit.WebView
import android.widget.Button
import android.widget.EditText

class MainActivity : Activity() {
    private lateinit var prefs: UrlPrefs
    private lateinit var settingsView: View
    private lateinit var webView: WebView
    private lateinit var urlInput: EditText

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        prefs = UrlPrefs(this)
        settingsView = findViewById(R.id.settings_view)
        webView = findViewById(R.id.webview)
        urlInput = findViewById(R.id.url_input)
        val saveButton = findViewById<Button>(R.id.save_button)

        saveButton.setOnClickListener {
            val url = UrlNormalizer.normalize(urlInput.text.toString())
            prefs.save(url)
            showWebViewFor(url)
        }

        val savedUrl = prefs.get()
        if (savedUrl != null) {
            urlInput.setText(savedUrl)
            showWebViewFor(savedUrl)
        } else {
            showSettings()
        }
    }

    private fun showSettings() {
        settingsView.visibility = View.VISIBLE
        webView.visibility = View.GONE
    }

    private fun showWebViewFor(url: String) {
        settingsView.visibility = View.GONE
        webView.visibility = View.VISIBLE
    }
}
```

- [ ] **Step 7: 編譯驗證**

```bash
source /home/moxa/cng5/android/toolchain-env.sh
cd /home/moxa/cng5/android
./gradlew assembleDebug
```

Expected: `BUILD SUCCESSFUL`（`WebView` 目前還沒接 `loadUrl`，這是 Task 4
的範圍——這一步只確認編譯跟顯示切換的邏輯沒問題）。

- [ ] **Step 8: Commit**

```bash
cd /home/moxa/cng5
git add android/app/src/main/java/com/moxa/cng5/tablet/UrlNormalizer.kt \
  android/app/src/test/java/com/moxa/cng5/tablet/UrlNormalizerTest.kt \
  android/app/src/main/java/com/moxa/cng5/tablet/UrlPrefs.kt \
  android/app/src/main/java/com/moxa/cng5/tablet/MainActivity.kt
git commit -m "$(cat <<'EOF'
android：網址正規化 + SharedPreferences 儲存 + 設定畫面顯示切換

UrlNormalizer 補上缺的 http:// 前綴（TDD 補測試）；UrlPrefs 包一層
SharedPreferences 存/讀伺服器網址；MainActivity 啟動時依有沒有存過網址
決定顯示設定畫面還是 WebView 容器（WebView 實際載入內容留給下一個
task）。
EOF
)"
```

---

### Task 4: WebView 載入、全螢幕 immersive、失敗自動重試

**Files:**
- Modify: `android/app/src/main/java/com/moxa/cng5/tablet/MainActivity.kt`

**Interfaces:**
- Consumes: Task 3 的 `showSettings()`/`showWebViewFor(url: String)`、`UrlPrefs`、layout 的 `error_text`。
- Produces: 無下游任務依賴這個 task 的產物（Task 5 只會呼叫既有的 `showSettings()`）。

- [ ] **Step 1: 把 `MainActivity.kt` 改成下面這樣（新增 WebView 設定/WebViewClient/immersive 模式/自動重試）**

```kotlin
package com.moxa.cng5.tablet

import android.app.Activity
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.View
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.Button
import android.widget.EditText
import android.widget.TextView

class MainActivity : Activity() {
    private lateinit var prefs: UrlPrefs
    private lateinit var settingsView: View
    private lateinit var webView: WebView
    private lateinit var errorText: TextView
    private lateinit var urlInput: EditText
    private val retryHandler = Handler(Looper.getMainLooper())
    private var currentUrl: String? = null
    private val retryRunnable = Runnable { currentUrl?.let { webView.loadUrl(it) } }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        prefs = UrlPrefs(this)
        settingsView = findViewById(R.id.settings_view)
        webView = findViewById(R.id.webview)
        errorText = findViewById(R.id.error_text)
        urlInput = findViewById(R.id.url_input)
        val saveButton = findViewById<Button>(R.id.save_button)

        webView.settings.javaScriptEnabled = true
        webView.webViewClient = object : WebViewClient() {
            override fun onPageFinished(view: WebView?, url: String?) {
                retryHandler.removeCallbacks(retryRunnable)
                errorText.visibility = View.GONE
                webView.visibility = View.VISIBLE
            }

            override fun onReceivedError(
                view: WebView?,
                errorCode: Int,
                description: String?,
                failingUrl: String?
            ) {
                errorText.text = "連不到 $failingUrl，3 秒後自動重試"
                errorText.visibility = View.VISIBLE
                webView.visibility = View.GONE
                retryHandler.removeCallbacks(retryRunnable)
                retryHandler.postDelayed(retryRunnable, 3000)
            }
        }

        saveButton.setOnClickListener {
            val url = UrlNormalizer.normalize(urlInput.text.toString())
            prefs.save(url)
            showWebViewFor(url)
        }

        val savedUrl = prefs.get()
        if (savedUrl != null) {
            urlInput.setText(savedUrl)
            showWebViewFor(savedUrl)
        } else {
            showSettings()
        }
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus && webView.visibility == View.VISIBLE) {
            enterImmersiveMode()
        }
    }

    private fun showSettings() {
        retryHandler.removeCallbacks(retryRunnable)
        settingsView.visibility = View.VISIBLE
        webView.visibility = View.GONE
        errorText.visibility = View.GONE
        enterNormalMode()
    }

    private fun showWebViewFor(url: String) {
        currentUrl = url
        settingsView.visibility = View.GONE
        webView.visibility = View.VISIBLE
        webView.loadUrl(url)
        enterImmersiveMode()
    }

    private fun enterImmersiveMode() {
        @Suppress("DEPRECATION")
        window.decorView.systemUiVisibility = (
            View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                or View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                or View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_FULLSCREEN
                or View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
            )
    }

    private fun enterNormalMode() {
        @Suppress("DEPRECATION")
        window.decorView.systemUiVisibility = View.SYSTEM_UI_FLAG_LAYOUT_STABLE
    }
}
```

- [ ] **Step 2: 編譯驗證**

```bash
source /home/moxa/cng5/android/toolchain-env.sh
cd /home/moxa/cng5/android
./gradlew assembleDebug
```

Expected: `BUILD SUCCESSFUL`。實際載入/全螢幕/自動重試的行為屬於「需要
實體平板確認」的項目，列在 Task 6。

- [ ] **Step 3: Commit**

```bash
cd /home/moxa/cng5
git add android/app/src/main/java/com/moxa/cng5/tablet/MainActivity.kt
git commit -m "$(cat <<'EOF'
android：WebView 載入 + 全螢幕 immersive + 載入失敗自動重試

啟用 JavaScript，WebViewClient.onPageFinished 顯示正常畫面，
onReceivedError 顯示錯誤文字並每 3 秒自動重試 loadUrl。onWindowFocusChanged
重新套用 immersive flag，避免系統手勢喚出的狀態列一直留著不收回去。
EOF
)"
```

---

### Task 5: 四角落長按 2 秒回設定畫面

**Files:**
- Modify: `android/app/src/main/java/com/moxa/cng5/tablet/MainActivity.kt`

**Interfaces:**
- Consumes: Task 4 的 `showSettings()`、layout 的 `corner_tl`/`corner_tr`/`corner_bl`/`corner_br`。
- Produces: 無下游任務依賴這個 task 的產物。

- [ ] **Step 1: 修改 `showSettings()`，回設定畫面時把目前網址帶回輸入框**

把：

```kotlin
    private fun showSettings() {
        retryHandler.removeCallbacks(retryRunnable)
        settingsView.visibility = View.VISIBLE
        webView.visibility = View.GONE
        errorText.visibility = View.GONE
        enterNormalMode()
    }
```

改成：

```kotlin
    private fun showSettings() {
        retryHandler.removeCallbacks(retryRunnable)
        urlInput.setText(prefs.get() ?: "")
        settingsView.visibility = View.VISIBLE
        webView.visibility = View.GONE
        errorText.visibility = View.GONE
        enterNormalMode()
    }
```

- [ ] **Step 2: 在 `onCreate` 結尾呼叫新增的角落長按設定，並新增對應方法**

把 `onCreate` 結尾（`if (savedUrl != null) { ... } else { showSettings() }`
那段之後）：

```kotlin
        val savedUrl = prefs.get()
        if (savedUrl != null) {
            urlInput.setText(savedUrl)
            showWebViewFor(savedUrl)
        } else {
            showSettings()
        }
    }
```

改成：

```kotlin
        val savedUrl = prefs.get()
        if (savedUrl != null) {
            urlInput.setText(savedUrl)
            showWebViewFor(savedUrl)
        } else {
            showSettings()
        }

        setupCornerLongPress()
    }

    private fun setupCornerLongPress() {
        val cornerIds = listOf(R.id.corner_tl, R.id.corner_tr, R.id.corner_bl, R.id.corner_br)
        val longPressRunnable = Runnable { showSettings() }
        for (id in cornerIds) {
            findViewById<View>(id).setOnTouchListener { _, event ->
                when (event.action) {
                    android.view.MotionEvent.ACTION_DOWN -> {
                        retryHandler.postDelayed(longPressRunnable, 2000)
                        true
                    }
                    android.view.MotionEvent.ACTION_UP,
                    android.view.MotionEvent.ACTION_CANCEL -> {
                        retryHandler.removeCallbacks(longPressRunnable)
                        true
                    }
                    else -> false
                }
            }
        }
    }
```

（沿用既有的 `retryHandler` 做長按計時，不用再多宣告一個 `Handler`。）

- [ ] **Step 3: 編譯驗證**

```bash
source /home/moxa/cng5/android/toolchain-env.sh
cd /home/moxa/cng5/android
./gradlew assembleDebug
```

Expected: `BUILD SUCCESSFUL`。實際「按住 2 秒才觸發、放開就取消」的行為
需要實體平板確認，列在 Task 6。

- [ ] **Step 4: Commit**

```bash
cd /home/moxa/cng5
git add android/app/src/main/java/com/moxa/cng5/tablet/MainActivity.kt
git commit -m "$(cat <<'EOF'
android：四角落長按 2 秒回設定畫面

四個角落各鋪一個透明 80dp 觸控區，用 Handler.postDelayed(2000ms) 判定
長按（不是系統預設的 long-click ~500ms），放開/取消就清掉計時器。回到
設定畫面時把目前已存的網址帶回輸入框方便修改。
EOF
)"
```

---

### Task 6: 最終建置、APK 內容確認、實機測試說明

**Files:**
- Create: `android/README.md`

**Interfaces:**
- Consumes: Task 1~5 的完整成果（無新程式介面）。

- [ ] **Step 1: 最終編譯**

```bash
source /home/moxa/cng5/android/toolchain-env.sh
cd /home/moxa/cng5/android
./gradlew assembleDebug
```

Expected: `BUILD SUCCESSFUL`。

- [ ] **Step 2: 用 aapt 檢查 APK 內容**

```bash
source /home/moxa/cng5/android/toolchain-env.sh
"$ANDROID_HOME/build-tools/34.0.0/aapt" dump badging \
  /home/moxa/cng5/android/app/build/outputs/apk/debug/app-debug.apk
```

Expected: 輸出裡看得到：
- `package: name='com.moxa.cng5.tablet'`
- `sdkVersion:'24'`、`targetSdkVersion:'34'`
- `uses-permission: name='android.permission.INTERNET'`
- `application-label:'cng5 tablet'`

- [ ] **Step 3: 寫 `android/README.md`**

```markdown
# cng5 tablet APK

把 `/tablet` 觸控 UI 包成一個可以側載安裝的 Android App（WebView 外殼），
設計細節見 `docs/superpowers/specs/2026-08-02-tablet-apk-design.md`。

## 編譯

第一次使用前，先跑過 `docs/superpowers/plans/2026-08-02-tablet-apk.md`
的 Task 1（安裝 portable JDK 17 / Android SDK / 產生 Gradle wrapper）。
之後每次編譯：

\`\`\`bash
source android/toolchain-env.sh
cd android
./gradlew assembleDebug
\`\`\`

APK 產出在 `android/app/build/outputs/apk/debug/app-debug.apk`。

## 安裝到平板

方法一（USB 接電腦，平板開「USB 偵錯」）：

\`\`\`bash
source android/toolchain-env.sh
adb install -r android/app/build/outputs/apk/debug/app-debug.apk
\`\`\`

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
```

- [ ] **Step 4: Commit**

```bash
cd /home/moxa/cng5
git add android/README.md
git commit -m "$(cat <<'EOF'
android：補上 README（編譯/安裝步驟 + 實機手動驗收清單）

開發機沒有實體平板/模擬器，這份清單交給使用者在真實裝置上跑一次。
EOF
)"
```

---

### Task 7: 端對端確認（純驗證，無新程式碼）

**Files:** 無新增/修改檔案。

- [ ] **Step 1: 全部單元測試 + 編譯一次跑完**

```bash
source /home/moxa/cng5/android/toolchain-env.sh
cd /home/moxa/cng5/android
./gradlew testDebugUnitTest assembleDebug
```

Expected: 全部成功，`UrlNormalizerTest` 4 個測試都 PASS，`app-debug.apk`
產出。

- [ ] **Step 2: 確認既有 Rust 專案完全沒受影響**

```bash
cd /home/moxa/cng5
cargo build && cargo test
git status --porcelain -- src/
```

Expected: `cargo build`/`cargo test` 成功（134 passed，跟之前一致），
`git status` 對 `src/` 沒有任何異動。

- [ ] **Step 3: 把 `android/README.md` 的手動驗收清單交給使用者**

這個 task 不需要 commit——把 Task 6 產出的 `android/README.md` 手動驗收
清單交給使用者，請他們把 `app-debug.apk` 側載到實體平板上跑一次。
