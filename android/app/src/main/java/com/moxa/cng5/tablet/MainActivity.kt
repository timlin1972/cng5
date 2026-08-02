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

    private enum class Mode { SETTINGS, WEB }
    private var mode = Mode.SETTINGS

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
        webView.settings.mediaPlaybackRequiresUserGesture = false
        webView.webViewClient = object : WebViewClient() {
            override fun onPageFinished(view: WebView?, url: String?) {
                if (mode != Mode.WEB) return
                retryHandler.removeCallbacks(retryRunnable)
                errorText.visibility = View.GONE
                webView.visibility = View.VISIBLE
            }

            override fun onReceivedError(
                view: WebView?,
                request: android.webkit.WebResourceRequest?,
                error: android.webkit.WebResourceError?
            ) {
                if (mode != Mode.WEB) return
                if (request?.isForMainFrame != true) return
                errorText.text = "連不到 ${request?.url}，3 秒後自動重試"
                errorText.visibility = View.VISIBLE
                webView.visibility = View.GONE
                retryHandler.removeCallbacks(retryRunnable)
                retryHandler.postDelayed(retryRunnable, 3000)
            }
        }

        saveButton.setOnClickListener {
            if (urlInput.text.toString().isBlank()) {
                return@setOnClickListener
            }
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

        setupCornerLongPress()
    }

    private fun setupCornerLongPress() {
        val cornerIds = listOf(R.id.corner_tl, R.id.corner_tr, R.id.corner_bl, R.id.corner_br)
        val longPressRunnable = Runnable {
            if (mode == Mode.SETTINGS) return@Runnable
            showSettings()
        }
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

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus && webView.visibility == View.VISIBLE) {
            enterImmersiveMode()
        }
    }

    private fun showSettings() {
        mode = Mode.SETTINGS
        retryHandler.removeCallbacks(retryRunnable)
        webView.stopLoading()
        urlInput.setText(prefs.get() ?: "")
        settingsView.visibility = View.VISIBLE
        webView.visibility = View.GONE
        errorText.visibility = View.GONE
        enterNormalMode()
    }

    private fun showWebViewFor(url: String) {
        mode = Mode.WEB
        currentUrl = url
        settingsView.visibility = View.GONE
        webView.visibility = View.VISIBLE
        errorText.visibility = View.GONE
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

    override fun onBackPressed() {
        // kiosk display — back button never exits or navigates
    }

    override fun onDestroy() {
        retryHandler.removeCallbacksAndMessages(null)
        webView.stopLoading()
        webView.destroy()
        super.onDestroy()
    }
}
