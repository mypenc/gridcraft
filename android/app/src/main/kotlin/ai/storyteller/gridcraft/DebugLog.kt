package ai.storyteller.gridcraft

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.graphics.Typeface
import android.os.Build
import android.util.Log
import android.util.TypedValue
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AlertDialog
import java.io.File
import kotlin.concurrent.thread

/**
 * On-device diagnostics for when there is no computer to run `adb logcat`.
 *
 * - Help ▸ Show Debug Log... in the app's menu opens a dialog with this app's own logcat output
 *   (Rust startup/GPU messages, panics) plus device info, with Copy and Share.
 * - The dialog also opens by itself after a Java crash, or when the previous run ended without a
 *   clean exit (e.g. a native crash), since the app may have closed before you could open the menu.
 */
object DebugLog {
    private const val MAX_LOG_CHARS = 80_000
    private var dialog: AlertDialog? = null

    fun install(activity: Activity) {
        val crashFile = File(activity.filesDir, "last_crash.txt")
        val activeFlag = File(activity.filesDir, "session_active")
        val diedLastTime = activeFlag.exists()
        runCatching { activeFlag.writeText("1") }

        val previous = Thread.getDefaultUncaughtExceptionHandler()
        Thread.setDefaultUncaughtExceptionHandler { t, e ->
            runCatching { crashFile.writeText("Thread: ${t.name}\n${Log.getStackTraceString(e)}") }
            previous?.uncaughtException(t, e)
        }

        if (crashFile.exists() || diedLastTime) {
            activity.window.decorView.postDelayed({ show(activity) }, 250)
        }
    }

    /** Called from onDestroy when the activity is really finishing: the next launch won't auto-open the log. */
    fun markClean(activity: Activity) {
        runCatching { File(activity.filesDir, "session_active").delete() }
    }

    fun show(activity: Activity) {
        thread {
            val report = buildReport(activity)
            activity.runOnUiThread { present(activity, report) }
        }
    }

    private fun buildReport(activity: Activity): String {
        val crashFile = File(activity.filesDir, "last_crash.txt")
        val sb = StringBuilder()
        sb.append("GridCraft debug report\n")
        sb.append("Device: ${Build.MANUFACTURER} ${Build.MODEL}, Android ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT})\n")
        sb.append("ABIs: ${Build.SUPPORTED_ABIS.joinToString()}\n\n")
        if (crashFile.exists()) {
            sb.append("--- Last Java crash ---\n")
            sb.append(runCatching { crashFile.readText() }.getOrDefault("(unreadable)"))
            sb.append("\n\n")
            runCatching { crashFile.delete() }
        }
        sb.append(exitReasons(activity))
        if (Build.VERSION.SDK_INT >= 30) {
            runCatching {
                val am = activity.getSystemService(Context.ACTIVITY_SERVICE) as android.app.ActivityManager
                am.getHistoricalProcessExitReasons(activity.packageName, 0, 3).forEach {
                    sb.append("--- Last exit: reason ${it.reason}, status ${it.status}, ${it.description} ---\n")
                }
                sb.append("\n")
            }
        }
        var logcat = readLogcat()
        if (logcat.length > MAX_LOG_CHARS) logcat = "...(trimmed)\n" + logcat.substring(logcat.length - MAX_LOG_CHARS)
        sb.append("--- logcat (this app) ---\n").append(logcat)
        return sb.toString()
    }

    /** Android's own record of how the last runs ended (crash, native crash, killed…). */
    private fun exitReasons(activity: Activity): String {
        if (Build.VERSION.SDK_INT < 30) return ""
        return runCatching {
            val am = activity.getSystemService(Context.ACTIVITY_SERVICE) as android.app.ActivityManager
            val sb = StringBuilder("--- how the last runs ended ---\n")
            for (e in am.getHistoricalProcessExitReasons(activity.packageName, 0, 3)) {
                sb.append("reason=${e.reason} status=${e.status} ${e.description ?: ""}\n")
                // A native crash carries its tombstone (signal, backtrace).
                if (e.reason == android.app.ApplicationExitInfo.REASON_CRASH_NATIVE) {
                    runCatching { e.traceInputStream?.bufferedReader()?.readText()?.take(6000) }.getOrNull()?.let { sb.append(it).append('\n') }
                }
            }
            sb.append('\n').toString()
        }.getOrDefault("")
    }

    /** An app may read its own log lines without any permission. */
    private fun readLogcat(): String = try {
        val process = ProcessBuilder("logcat", "-d", "-v", "time", "-t", "2500").redirectErrorStream(true).start()
        process.inputStream.bufferedReader().readText()
    } catch (e: Exception) {
        "logcat unavailable: $e"
    }

    private fun present(activity: Activity, report: String) {
        if (activity.isFinishing || activity.isDestroyed) return
        dialog?.dismiss()
        val pad = (12 * activity.resources.displayMetrics.density).toInt()
        val body = TextView(activity).apply {
            text = report
            setTextIsSelectable(true)
            typeface = Typeface.MONOSPACE
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 10f)
            setPadding(pad, pad, pad, pad)
        }
        val scroll = ScrollView(activity).apply { addView(body) }
        scroll.post { scroll.fullScroll(ScrollView.FOCUS_DOWN) }
        dialog = AlertDialog.Builder(activity)
            .setTitle("GridCraft log")
            .setView(scroll)
            .setPositiveButton("Copy all") { _, _ -> copy(activity, report) }
            .setNeutralButton("Share") { _, _ -> share(activity, report) }
            .setNegativeButton("Close", null)
            .show()
    }

    private fun copy(context: Context, text: String) {
        val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        clipboard.setPrimaryClip(ClipData.newPlainText("GridCraft log", text))
        Toast.makeText(context, "Copied", Toast.LENGTH_SHORT).show()
    }

    private fun share(context: Context, text: String) {
        val send = Intent(Intent.ACTION_SEND).apply {
            type = "text/plain"
            putExtra(Intent.EXTRA_TEXT, text)
        }
        context.startActivity(Intent.createChooser(send, "Share log"))
    }
}
