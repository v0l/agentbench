package io.v0l.foreman

import android.app.ActivityManager
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import org.json.JSONArray
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL

/** What the app tells the background work: which server, with what token, and whether to watch. */
class Config(ctx: Context) {
    private val prefs = ctx.applicationContext.getSharedPreferences("foreman-sync", Context.MODE_PRIVATE)
    val on get() = prefs.getBoolean("on", false)
    val url get() = prefs.getString("url", "").orEmpty().trim().trimEnd('/')
    val token get() = prefs.getString("token", "").orEmpty().trim()
    val ready get() = on && url.isNotEmpty() && token.isNotEmpty()

    /** The last change the server told us about, so a restart carries on from there. */
    var cursor: Long
        get() = prefs.getLong("cursor", 0)
        set(v) = prefs.edit().putLong("cursor", v).apply()

    /** Each chat's status when we last looked, as JSON, to see what changed. */
    var known: String
        get() = prefs.getString("known", "").orEmpty()
        set(v) = prefs.edit().putString("known", v).apply()
}

object Net {
    /** GET a path on the server and return the JSON, or null when it could not be reached. */
    fun get(cfg: Config, path: String, timeoutMs: Int): JSONObject? = try {
        val c = URL(cfg.url + path).openConnection() as HttpURLConnection
        c.connectTimeout = 10_000
        c.readTimeout = timeoutMs
        c.setRequestProperty("Authorization", "Bearer ${cfg.token}")
        try {
            if (c.responseCode == 200) JSONObject(c.inputStream.bufferedReader().readText()) else null
        } finally {
            c.disconnect()
        }
    } catch (e: Exception) {
        null
    }
}

/** Works out what happened to the chats since last time and says so in a notification. */
object Watcher {
    const val ATTENTION = "attention"
    const val DONE = "done"
    const val SERVICE = "service"

    fun channels(ctx: Context) {
        val nm = ctx.getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(ATTENTION, "Needs you", NotificationManager.IMPORTANCE_HIGH).apply {
                description = "An agent is waiting for an answer, or a chat failed"
            }
        )
        nm.createNotificationChannel(
            NotificationChannel(DONE, "Finished", NotificationManager.IMPORTANCE_DEFAULT).apply {
                description = "An agent finished its work"
            }
        )
        nm.createNotificationChannel(
            NotificationChannel(SERVICE, "Watching", NotificationManager.IMPORTANCE_MIN).apply {
                description = "Shown while foreman keeps an eye on your agents. Hide it here."
            }
        )
    }

    /** Whether the app's window is on screen, when there is no need to notify. */
    private fun visible(ctx: Context): Boolean {
        val am = ctx.getSystemService(ActivityManager::class.java)
        val me = android.os.Process.myPid()
        return am.runningAppProcesses.orEmpty().any {
            it.pid == me && it.importance <= ActivityManager.RunningAppProcessInfo.IMPORTANCE_FOREGROUND
        }
    }

    /** Compare the server's chats with what we knew and notify. Returns the server's cursor. */
    fun check(ctx: Context, cfg: Config): Long? {
        val o = Net.get(cfg, "/api/overview", 20_000) ?: return null
        val sessions = o.optJSONArray("sessions") ?: JSONArray()
        val projects = HashMap<Long, String>()
        o.optJSONArray("projects")?.let { for (i in 0 until it.length()) it.getJSONObject(i).let { p -> projects[p.getLong("id")] = p.getString("name") } }
        val before = runCatching { JSONObject(cfg.known) }.getOrNull()
        val now = JSONObject()
        val show = before != null && !visible(ctx)
        val nm = ctx.getSystemService(NotificationManager::class.java)
        for (i in 0 until sessions.length()) {
            val s = sessions.getJSONObject(i)
            val id = s.getString("id")
            val status = s.getString("status")
            now.put(id, status)
            val was = before?.optString(id, "")
            val nid = id.hashCode()
            // Take down an alert once the chat has moved on from needing you.
            if (was != status && (was == "waiting" || was == "failed")) nm.cancel(nid)
            if (was != status) android.util.Log.i("foreman", "$id: $was -> $status (alerting: $show)")
            if (!show || was == status) continue
            val project = projects[s.optLong("project_id")].orEmpty()
            val title = s.optString("title").ifBlank { "chat" }
            when {
                status == "waiting" -> notify(ctx, ATTENTION, nid, "$project needs you", title)
                status == "failed" -> notify(ctx, ATTENTION, nid, "$project failed", title)
                status == "idle" && was == "running" -> notify(ctx, DONE, nid, "$project finished", title)
            }
        }
        cfg.known = now.toString()
        return o.optLong("cursor", 0)
    }

    /** The status bar icon, found by name: there is no generated R class without Gradle. */
    fun icon(ctx: Context): android.graphics.drawable.Icon =
        android.graphics.drawable.Icon.createWithResource(
            ctx, ctx.resources.getIdentifier("ic_stat", "drawable", ctx.packageName)
        )

    private fun notify(ctx: Context, channel: String, id: Int, title: String, text: String) {
        val open = ctx.packageManager.getLaunchIntentForPackage(ctx.packageName)?.apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP
        }
        val tap = PendingIntent.getActivity(ctx, id, open, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        val n = Notification.Builder(ctx, channel)
            .setSmallIcon(icon(ctx))
            .setContentTitle(title)
            .setContentText(text)
            .setContentIntent(tap)
            .setAutoCancel(true)
            .build()
        ctx.getSystemService(NotificationManager::class.java).notify(id, n)
    }
}
