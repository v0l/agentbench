package io.v0l.agentbench

import android.app.Notification
import android.app.Service
import android.app.job.JobInfo
import android.app.job.JobParameters
import android.app.job.JobScheduler
import android.app.job.JobService
import android.content.BroadcastReceiver
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.IBinder

/**
 * Holds one long poll open to the server (it answers when something changes, or after 25 seconds)
 * so a chat that needs you shows up at once. That costs one small request every 25 seconds.
 */
class WatchService : Service() {
    @Volatile private var running = false

    override fun onBind(i: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        Watcher.channels(this)
        val n = Notification.Builder(this, Watcher.SERVICE)
            .setSmallIcon(Watcher.icon(this))
            .setContentTitle("Watching your agents")
            .setOngoing(true)
            .build()
        startForeground(1, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        schedule(this)
        if (!running) {
            running = true
            Thread(::loop, "agentbench-watch").start()
        }
        return START_STICKY
    }

    override fun onDestroy() {
        running = false
    }

    private fun loop() {
        var failures = 0
        while (running) {
            val cfg = Config(this)
            if (!cfg.ready) {
                stopSelf()
                return
            }
            if (cfg.cursor == 0L) {
                cfg.cursor = Watcher.check(this, cfg) ?: 0
            }
            val r = Net.get(cfg, "/api/changes?since=${cfg.cursor}&wait=25", 40_000)
            if (r == null) {
                // Back off while the server or the network is away: 5s, 10s, 20s ... one minute.
                failures = minOf(failures + 1, 4)
                Thread.sleep(5_000L shl (failures - 1))
                continue
            }
            failures = 0
            val cursor = r.optLong("cursor", cfg.cursor)
            val changes = r.optJSONArray("changes")
            if (changes != null && changes.length() > 0) {
                var sessions = false
                for (i in 0 until changes.length()) if (changes.getJSONObject(i).optString("kind") == "session") sessions = true
                if (sessions) Watcher.check(this, cfg)
            }
            cfg.cursor = cursor
        }
    }

    companion object {
        /** Every 15 minutes, the least Android allows, for when the service is not allowed to run. */
        fun schedule(ctx: Context) {
            try {
                val js = ctx.getSystemService(JobScheduler::class.java)
                if (js.getPendingJob(2) != null) return
                js.schedule(
                    JobInfo.Builder(2, ComponentName(ctx, SyncJob::class.java))
                        .setRequiredNetworkType(JobInfo.NETWORK_TYPE_ANY)
                        .setPeriodic(15 * 60 * 1000L)
                        .setPersisted(true)
                        .build()
                )
            } catch (e: Exception) {
                android.util.Log.w("agentbench", "could not schedule the periodic sync", e)
            }
        }
    }
}

/** The fallback: look once, now and then, even when the service has been stopped by the system. */
class SyncJob : JobService() {
    override fun onStartJob(p: JobParameters): Boolean {
        Thread {
            val cfg = Config(this)
            if (cfg.ready) {
                Watcher.channels(this)
                Watcher.check(this, cfg)
            }
            jobFinished(p, false)
        }.start()
        return true
    }

    override fun onStopJob(p: JobParameters) = true
}

/** After a restart of the phone, start watching again. */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(ctx: Context, i: Intent) {
        if (!Config(ctx).ready) return
        WatchService.schedule(ctx)
        try {
            ctx.startForegroundService(Intent(ctx, WatchService::class.java))
        } catch (e: Exception) {
            // Not allowed to start from here: the job above still looks every 15 minutes.
        }
    }
}
