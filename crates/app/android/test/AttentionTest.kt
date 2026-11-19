package io.v0l.agentbench

fun check(cond: Boolean, msg: String) {
    if (!cond) throw AssertionError(msg)
}

fun d(id: String, status: String? = null, title: String? = null, project: String? = null, removed: Boolean = false) =
    Delta(id, removed, status, title, project)

fun main() {
    val first = AttentionState(null)
    val e1 = first.apply(listOf(d("a", "running", "chat a", "demo")), true)
    check(e1.isEmpty(), "first snapshot is silent")

    val s = AttentionState(first.rows)
    val e2 = s.apply(listOf(d("a", status = "waiting")), false)
    check(e2 == listOf<Event>(Event.Alert("a", "needs", "chat a", "demo")), "waiting alerts with the remembered title: $e2")
    val e3 = s.apply(listOf(d("a", status = "running")), false)
    check(e3 == listOf<Event>(Event.Cancel("a")), "leaving waiting takes the alert down: $e3")
    val e4 = s.apply(listOf(d("a", status = "idle")), false)
    check(e4 == listOf<Event>(Event.Alert("a", "finished", "chat a", "demo")), "finish: $e4")
    val e5 = s.apply(listOf(d("a", title = "renamed")), false)
    check(e5.isEmpty() && s.rows["a"]!!.title == "renamed", "a rename alone is silent")
    val e6 = s.apply(listOf(d("a", removed = true)), false)
    check(e6.isEmpty() && s.rows.isEmpty(), "removal")

    val r = AttentionState(mapOf("x" to Row("waiting", "t", "p")))
    val e7 = r.apply(listOf(d("y", "idle", "new", "p")), true)
    check(e7 == listOf<Event>(Event.Cancel("x")) && r.rows.keys == setOf("y"), "reset replaces state: $e7")

    val p = Pacing(3000, 1000)
    check(p.delayBeforeNext(10_000) == 0L, "first poll goes at once")
    p.started(10_000)
    check(p.delayBeforeNext(10_100) == 2_900L, "a fast return waits out the gap")
    check(p.delayBeforeNext(14_000) == 0L, "a slow poll goes at once")
    println("ok")
}
