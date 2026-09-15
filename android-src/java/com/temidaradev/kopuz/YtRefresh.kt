package com.temidaradev.kopuz

import android.annotation.SuppressLint
import android.app.Activity
import android.content.Context
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.webkit.CookieManager
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.FrameLayout

/**
 * Headless YouTube cookie keepalive for Android.
 *
 * The problem: the in-app sign-in ([YtLogin]) captures the cookie jar ONCE. YouTube
 * then rotates the session cookies (`__Secure-*PSIDTS`, `SIDCC`, …) every few
 * minutes, and a static captured jar goes stale within hours — the app starts
 * getting "sign-in prompt returned, cookies expired" and the user has to re-add
 * the server. But the sign-in itself is still valid: Android's [CookieManager] is
 * process-global and persistent, so the account stays logged in (which is why
 * re-adding "just works" without re-entering a password).
 *
 * The fix: keep an offscreen, signed-in `music.youtube.com` WebView alive and
 * reload it periodically. Loading the real page is exactly what makes YouTube
 * rotate the cookies — like a browser tab left open. After each load we read the
 * freshly-rotated jar out of the CookieManager and hand it to Rust
 * ([nativeOnRefreshedCookies]), which persists it as the active session. No user
 * interaction, ever.
 *
 * Offscreen mechanics mirror [PotMinter]: an unattached Android WebView throttles
 * its JS and may never finish loading, so the WebView is attached to the Activity
 * content view at 1x1 px (never visible) and `resumeTimers()` un-freezes it before
 * a reload (Android suspends background WebView JS).
 */
object YtRefresh {
    private const val TAG = "YtRefresh"

    // Implemented in Rust (player::systemint::android). The freshly-rotated cookie
    // jar for music.youtube.com; Rust persists it as the active YT session.
    @JvmStatic external fun nativeOnRefreshedCookies(cookies: String)

    // Same desktop UA the sign-in uses, so YouTube serves the full web session
    // rather than an app/webview-flavoured one.
    private const val DESKTOP_UA =
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 " +
            "(KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36"
    private const val URL = "https://music.youtube.com/"
    // Re-pull well inside YouTube's rotation window (it tears idle sessions down
    // around the ten-minute mark; the rotating cookies change every few minutes).
    private const val INTERVAL_MS = 12L * 60L * 1000L

    @Volatile private var webView: WebView? = null
    private val main = Handler(Looper.getMainLooper())

    /** Start (or, if already running, immediately re-pull). Idempotent. */
    @JvmStatic
    fun start(context: Context) {
        main.post {
            if (webView != null) {
                reloadNow()
            } else {
                setup(context)
            }
        }
    }

    @SuppressLint("SetJavaScriptEnabled")
    private fun setup(context: Context) {
        if (webView != null) return
        val web = WebView(context)
        web.settings.apply {
            javaScriptEnabled = true
            domStorageEnabled = true
            userAgentString = DESKTOP_UA
        }
        val cm = CookieManager.getInstance()
        cm.setAcceptCookie(true)
        cm.setAcceptThirdPartyCookies(web, true)

        web.webViewClient = object : WebViewClient() {
            override fun onPageFinished(view: WebView, url: String) {
                cm.flush()
                val jar = cm.getCookie("https://music.youtube.com") ?: ""
                // Only report a genuinely signed-in jar. `__Secure-3PSID` is the
                // youtube.com session cookie and `__Secure-3PAPISID` is what the
                // backend hashes for SAPISIDHASH auth — the same signed-in check
                // YtLogin uses. A logged-out page (session finally died) is
                // ignored so we never clobber a good session with an empty one.
                val signedIn = url.contains("music.youtube.com") &&
                    jar.contains("__Secure-3PSID") &&
                    jar.contains("__Secure-3PAPISID")
                if (signedIn) {
                    Log.i(TAG, "re-pulled rotated cookies (${jar.length} chars)")
                    nativeOnRefreshedCookies(jar)
                } else {
                    Log.w(TAG, "refresh load not signed in — keeping current session")
                }
            }
        }

        // Attach invisibly so the renderer stays live (see PotMinter).
        val activity = context as? Activity
        if (activity != null) {
            activity.addContentView(web, FrameLayout.LayoutParams(1, 1))
        } else {
            Log.w(TAG, "context is not an Activity — WebView left detached (may stall)")
        }

        webView = web
        web.loadUrl(URL)
        scheduleNext()
        Log.i(TAG, "cookie keepalive webview created")
    }

    private fun reloadNow() {
        val web = webView ?: return
        try {
            web.onResume()
            web.resumeTimers()
        } catch (e: Exception) {
            Log.w(TAG, "resume before reload failed: ${e.message}")
        }
        web.loadUrl(URL)
    }

    private fun scheduleNext() {
        main.postDelayed(
            {
                reloadNow()
                scheduleNext()
            },
            INTERVAL_MS,
        )
    }
}
