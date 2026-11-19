package io.v0l.agentbench

data class Row(val status: String, val title: String, val project: String)

sealed class Event {
    data class Cancel(val id: String) : Event()
    data class Alert(val id: String, val kind: String, val title: String, val project: String) : Event()
}

class Delta(
    val id: String,
    val removed: Boolean,
    val status: String?,
    val title: String?,
    val project: String?,
)

/** What the phone knows about each chat: status, title and project only, never any content. */
class AttentionState(start: Map<String, Row>?) {
    val rows = HashMap<String, Row>(start ?: emptyMap())
    private var seeded = start != null

    fun apply(deltas: List<Delta>, reset: Boolean): List<Event> {
        val events = ArrayList<Event>()
        val before = HashMap(rows)
        if (reset) rows.clear()
        for (d in deltas) {
            if (d.removed) {
                rows.remove(d.id)
                continue
            }
            val old = rows[d.id]
            rows[d.id] = Row(
                d.status ?: old?.status ?: "idle",
                d.title ?: old?.title ?: "",
                d.project ?: old?.project ?: "",
            )
        }
        val touched = if (reset) rows.keys + before.keys else deltas.map { it.id }.toSet()
        for (id in touched) {
            val was = before[id]?.status
            val now = rows[id]
            if (now == null) {
                if (was == "waiting" || was == "failed") events.add(Event.Cancel(id))
                continue
            }
            if (was == now.status) continue
            if (was == "waiting" || was == "failed") events.add(Event.Cancel(id))
            if (!seeded) continue
            val title = now.title.ifBlank { "chat" }
            when {
                now.status == "waiting" -> events.add(Event.Alert(id, "needs", title, now.project))
                now.status == "failed" -> events.add(Event.Alert(id, "failed", title, now.project))
                now.status == "idle" && was == "running" -> events.add(Event.Alert(id, "finished", title, now.project))
            }
        }
        seeded = true
        return events
    }
}

/** How soon the watch loop may ask again: never faster than `minGapMs`, and a burst is held back `settleMs`. */
class Pacing(private val minGapMs: Long = 3_000, private val settleMs: Long = 1_000) {
    private var lastStart = 0L

    fun delayBeforeNext(now: Long): Long {
        if (lastStart == 0L) return 0
        return maxOf(0L, lastStart + minGapMs - now)
    }

    fun started(now: Long) {
        lastStart = now
    }

    fun settle(): Long = settleMs
}
