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
