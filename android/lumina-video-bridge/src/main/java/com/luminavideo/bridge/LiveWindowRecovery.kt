package com.luminavideo.bridge

/** One live-edge recovery at a time, with a deadline and a cooldown after each attempt. */
internal class LiveWindowRecovery {
    private var lastAttemptMs: Long? = null
    private var deadlineMs: Long? = null

    fun start(nowMs: Long): Boolean {
        if (deadlineMs != null || lastAttemptMs?.let { nowMs - it < 30_000 } == true) return false
        lastAttemptMs = nowMs
        deadlineMs = nowMs + 10_000
        return true
    }

    fun hasTimedOut(nowMs: Long): Boolean = deadlineMs?.let { nowMs >= it } == true

    fun finish(): Boolean {
        val wasRecovering = deadlineMs != null
        deadlineMs = null
        return wasRecovering
    }

    fun reset() {
        lastAttemptMs = null
        deadlineMs = null
    }
}
