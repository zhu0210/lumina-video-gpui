package com.luminavideo.bridge

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class LiveWindowRecoveryTest {
    @Test
    fun retryIsBoundedEvenIfReadyBrieflyReturns() {
        val recovery = LiveWindowRecovery()
        assertTrue(recovery.start(1_000))
        assertFalse(recovery.start(2_000))
        assertTrue(recovery.finish())
        assertFalse(recovery.start(30_999))
        assertTrue(recovery.start(31_000))
    }

    @Test
    fun readinessCompletesTheAttemptButStalledRecoveryExpires() {
        val recovery = LiveWindowRecovery()
        assertFalse(recovery.hasTimedOut(100_000))
        assertTrue(recovery.start(100_000))
        assertFalse(recovery.hasTimedOut(109_999))
        assertTrue(recovery.hasTimedOut(110_000))
        assertTrue(recovery.finish())
        assertFalse(recovery.hasTimedOut(120_000))
        assertFalse(recovery.finish())
    }

    @Test
    fun newSourceResetsDeadlineAndRetryBudget() {
        val recovery = LiveWindowRecovery()
        assertTrue(recovery.start(1_000))
        recovery.reset()
        assertFalse(recovery.hasTimedOut(11_000))
        assertTrue(recovery.start(2_000))
    }
}
