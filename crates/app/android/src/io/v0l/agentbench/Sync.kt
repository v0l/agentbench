package io.v0l.agentbench

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

/** One workspace to watch. */
data class Target(val name: String, val url: String, val token: String)

/** What the app tells the background work: which servers, with what tokens, and whether to watch. */
class Config(ctx: Context, private val target: Target? = null, private val several: Boolean = false) {
    @Suppress("DEPRECATION")
    private val prefs = ctx.applicationContext.getSharedPreferences("agentbench-sync", Context.MODE_MULTI_PROCESS)
    private val seen = ctx.applicationContext.getSharedPreferences(
        if (target == null) "agentbench-watch" else "agentbench-watch-${target.url.hashCode()}",
        Context.MODE_PRIVATE,
    )
    val on get() = prefs.getBoolean("on", false)
    val url get() = (target?.url ?: prefs.getString("url", "").orEmpty()).trim().trimEnd('/')
    val token get() = (target?.token ?: prefs.getString("token", "").orEmpty()).trim()
    val ready get() = on && url.isNotEmpty() && token.isNotEmpty()
    val alertManaged get() = prefs.getBoolean("alert_managed", false)

    /** The workspace name to put in a notification, only when more than one is watched. */
    val label get() = if (several) target?.name.orEmpty() else ""

    /** Every workspace the app asked to watch; an older app only sets one url and token. */
    fun targets(): List<Target> {
        val list = runCatching { JSONArray(prefs.getString("servers", "").orEmpty()) }.getOrNull()
        val out = ArrayList<Target>()
        if (list != null) for (i in 0 until list.length()) {
            val o = list.getJSONObject(i)
            val t = Target(o.optString("name"), o.optString("url").trim().trimEnd('/'), o.optString("token").trim())
            if (t.url.isNotEmpty() && t.token.isNotEmpty()) out.add(t)
        }
        if (out.isEmpty() && url.isNotEmpty() && token.isNotEmpty()) out.add(Target("", url, token))
        return out
    }

    init {
        val server = url
        if (seen.getString("server", null) != server) {
            seen.edit().clear().putString("server", server).commit()
        }
    }

    /** The last change the server told us about, so a restart carries on from there. */
    var cursor: Long
        get() = seen.getLong("cursor", 0)
        set(v) = seen.edit().putLong("cursor", v).apply()

    /** Each chat's status when we last looked, as JSON, to see what changed. */
    var known: String
        get() = seen.getString("known", "").orEmpty()
        set(v) = seen.edit().putString("known", v).apply()

    var epoch: Long
        get() = seen.getLong("epoch", 0)
        set(v) = seen.edit().putLong("epoch", v).apply()

    var feedCursor: Long
        get() = seen.getLong("feed_cursor", 0)
        set(v) = seen.edit().putLong("feed_cursor", v).apply()

    var legacy: Boolean
        get() = seen.getBoolean("legacy", false)
        set(v) = seen.edit().putBoolean("legacy", v).apply()
}

object Net {
    private val open = java.util.concurrent.CopyOnWriteArraySet<HttpURLConnection>()
    private val lock = Object()

    fun drop() {
        android.util.Log.i("agentbench", "watch: network changed, reconnecting")
        open.forEach { it.disconnect() }
        synchronized(lock) { lock.notifyAll() }
    }

    fun sleep(ms: Long) {
        synchronized(lock) { lock.wait(ms) }
    }

    /** GET a path on the server and return the JSON, or null when it could not be reached. */
    fun get(cfg: Config, path: String, timeoutMs: Int): JSONObject? = try {
        val c = URL(cfg.url + path).openConnection() as HttpURLConnection
        c.connectTimeout = 10_000
        c.readTimeout = timeoutMs
        c.setRequestProperty("Authorization", "Bearer ${cfg.token}")
        open.add(c)
        try {
            if (c.responseCode == 200) JSONObject(c.inputStream.bufferedReader().readText()) else { c.errorStream?.close(); null }
        } finally {
            open.remove(c)
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
                description = "Shown while agentbench keeps an eye on your agents. Hide it here."
            }
        )
    }

    /** Whether the app's window is on screen, when there is no need to notify. */
    private fun visible(ctx: Context): Boolean {
        val am = ctx.getSystemService(ActivityManager::class.java)
        return am.runningAppProcesses.orEmpty().any {
            it.processName == ctx.packageName &&
                it.importance <= ActivityManager.RunningAppProcessInfo.IMPORTANCE_FOREGROUND
        }
    }

    private fun load(cfg: Config): Map<String, Row>? {
        val o = runCatching { JSONObject(cfg.known) }.getOrNull() ?: return null
        if (!o.has("v2")) return null
        val out = HashMap<String, Row>()
        for (id in o.keys()) {
            if (id == "v2") continue
            val a = o.optJSONArray(id) ?: continue
            out[id] = Row(a.optString(0), a.optString(1), a.optString(2), a.optBoolean(3, false))
        }
        return out
    }

    private fun save(cfg: Config, rows: Map<String, Row>) {
        val o = JSONObject().put("v2", 1)
        for ((id, r) in rows) o.put(id, JSONArray().put(r.status).put(r.title).put(r.project).put(r.managed))
        cfg.known = o.toString()
    }

    /**
     * One round of the watch: ask the server for status changes only and apply them. The first
     * call, or a cursor the server no longer has, brings one snapshot of ids and statuses; every
     * later call brings only what changed. Returns false when the server could not be reached.
     */
    fun sync(ctx: Context, cfg: Config, waitSecs: Int): Boolean {
        if (cfg.legacy) return legacy(ctx, cfg)
        val r = Net.get(
            cfg,
            "/api/changes?kinds=attention&since=${cfg.feedCursor}&epoch=${cfg.epoch}&wait=$waitSecs",
            waitSecs * 1000 + 5_000,
        ) ?: return false
        if (!r.has("epoch")) {
            cfg.legacy = true
            return legacy(ctx, cfg)
        }
        val list = r.optJSONArray("changes") ?: JSONArray()
        val deltas = ArrayList<Delta>()
        for (i in 0 until list.length()) {
            val d = list.getJSONObject(i)
            deltas.add(
                Delta(
                    d.getString("id"),
                    d.optBoolean("removed", false),
                    if (d.has("status")) d.getString("status") else null,
                    if (d.has("title")) d.getString("title") else null,
                    if (d.has("project")) d.getString("project") else null,
                    if (d.has("managed")) d.getBoolean("managed") else null,
                )
            )
        }
        val state = AttentionState(load(cfg), cfg.alertManaged)
        val reset = r.optBoolean("reset", false)
        val events = state.apply(deltas, reset)
        val show = events.isNotEmpty() && !visible(ctx)
        val nm = ctx.getSystemService(NotificationManager::class.java)
        for (e in events) when (e) {
            is Event.Cancel -> nm.cancel(e.id.hashCode())
            is Event.Alert -> if (show) when (e.kind) {
                "needs" -> notify(ctx, ATTENTION, e.id.hashCode(), heading(cfg, e.project, "needs you"), e.title, e.id)
                "failed" -> notify(ctx, ATTENTION, e.id.hashCode(), heading(cfg, e.project, "failed"), e.title, e.id)
                else -> notify(ctx, DONE, e.id.hashCode(), heading(cfg, e.project, "finished"), e.title, e.id)
            }
        }
        if (deltas.isNotEmpty() || reset) save(cfg, state.rows)
        cfg.epoch = r.optLong("epoch", 0)
        cfg.feedCursor = r.optLong("cursor", 0)
        return true
    }

    fun heading(cfg: Config, project: String, what: String): String =
        if (cfg.label.isEmpty()) "$project $what" else "[${cfg.label}] $project $what"

    private fun legacy(ctx: Context, cfg: Config): Boolean {
        val ok = check(ctx, cfg) != null
        if (ok) Net.sleep(30_000)
        return ok
    }

    /** Legacy path for a server without the attention feed: the full overview. */
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
            if (was != status) android.util.Log.i("agentbench", "$id: $was -> $status (alerting: $show)")
            if (!show || was == status) continue
            val project = projects[s.optLong("project_id")].orEmpty()
            val title = s.optString("title").ifBlank { "chat" }
            when {
                status == "waiting" -> notify(ctx, ATTENTION, nid, heading(cfg, project, "needs you"), title, id)
                status == "failed" -> notify(ctx, ATTENTION, nid, heading(cfg, project, "failed"), title, id)
                status == "idle" && was == "running" -> notify(ctx, DONE, nid, heading(cfg, project, "finished"), title, id)
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

    private fun notify(ctx: Context, channel: String, id: Int, title: String, text: String, session: String) {
        // The time makes each tap a new link, so opening the same chat twice still counts.
        val open = Intent(ctx, MainActivity::class.java).apply {
            data = Uri.parse("agentbench://open?session=$session&t=${System.currentTimeMillis()}")
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
