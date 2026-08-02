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
