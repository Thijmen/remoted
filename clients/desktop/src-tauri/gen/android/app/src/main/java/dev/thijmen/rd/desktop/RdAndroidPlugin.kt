package dev.thijmen.rd.desktop

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.provider.Settings
import android.view.WindowManager
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.FileProvider
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin
import java.io.File

/**
 * What the Rust side needs from Android: hide the system bars while a session is on screen, keep
 * the screen awake there, open links in the browser, and hand a downloaded APK to the installer
 * (docs/android.md). Registered as `rd-android` from `src/android.rs`.
 */
@TauriPlugin
class RdAndroidPlugin(private val host: Activity) : Plugin(host) {
  private var immersive = false
  private var keepAwake = false

  /** Page -> app: the viewer is on a session page (or left it). Safe to call often. */
  @Command
  fun setChrome(invoke: Invoke) {
    val args = invoke.getArgs()
    immersive = args.optBoolean("immersive", false)
    keepAwake = args.optBoolean("keepAwake", false)
    host.runOnUiThread { applyChrome() }
    invoke.resolve()
  }

  /** A page asked for a link outside the app: hand it to the browser, never load it here. */
  @Command
  fun openUrl(invoke: Invoke) {
    val url = invoke.getArgs().getString("url", "")
    if (url.isNullOrEmpty()) {
      invoke.reject("no URL")
      return
    }
    try {
      host.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)))
      invoke.resolve()
    } catch (e: Exception) {
      invoke.reject("cannot open the browser", e)
    }
  }

  /**
   * Installs the APK the updater downloaded. Android checks that it is signed with the same key
   * as the running app and asks the user to confirm; when "install unknown apps" is off for rd,
   * the settings page for it opens instead and the user can try again.
   */
  @Command
  fun installApk(invoke: Invoke) {
    val path = invoke.getArgs().getString("path", "")
    val file = if (path.isNullOrEmpty()) null else File(path)
    if (file == null || !file.isFile) {
      invoke.reject("the downloaded APK is gone; check for the update again")
      return
    }
    if (!host.packageManager.canRequestPackageInstalls()) {
      try {
        host.startActivity(
          Intent(
            Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
            Uri.parse("package:${host.packageName}")
          )
        )
      } catch (_: Exception) {
        // No settings page for it: the installer still asks when it starts.
      }
      invoke.reject("allow rd to install unknown apps, then tap Update again")
      return
    }
    try {
      val uri = FileProvider.getUriForFile(host, "${host.packageName}.fileprovider", file)
      val intent = Intent(Intent.ACTION_VIEW).apply {
        setDataAndType(uri, "application/vnd.android.package-archive")
        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
      }
      host.startActivity(intent)
      invoke.resolve()
    } catch (e: Exception) {
      invoke.reject("cannot start the installer", e)
    }
  }

  // The activity is recreated on some configuration changes and the system may show the bars
  // again after a pause: re-apply the last state (it is not persisted, and starts as "off").
  override fun onResume(activity: AppCompatActivity) {
    applyChrome()
  }

  private fun applyChrome() {
    val window = host.window
    if (keepAwake) {
      window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    } else {
      window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    }
    val controller = WindowCompat.getInsetsController(window, window.decorView)
    if (immersive) {
      controller.systemBarsBehavior =
        WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
      controller.hide(WindowInsetsCompat.Type.systemBars())
    } else {
      controller.show(WindowInsetsCompat.Type.systemBars())
    }
  }
}
