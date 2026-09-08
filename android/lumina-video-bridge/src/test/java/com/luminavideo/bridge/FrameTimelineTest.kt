package com.luminavideo.bridge

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class FrameTimelineTest {
    @Test
    fun seekDiscardsOldImagesAndKeepsInFlightSubmissionGeneration() {
        val timeline = FrameTimeline(2)
        timeline.record(100, 1_000)
        timeline.record(200, 2_000)
        val inFlight = timeline.take(1_000)
        timeline.reset(1)
        assertNull(timeline.take(2_000))
        assertEquals(0L, inFlight?.generation)
        timeline.record(5_000, 3_000)
        assertEquals(FrameTimeline.Stamp(5_000, 1), timeline.take(3_000))
        assertNull(timeline.take(3_000))
    }

    @Test
    fun overloadedTimelineDropsTheOldestReleaseTime() {
        val timeline = FrameTimeline(2)
        timeline.record(100, 1_000)
        timeline.record(200, 2_000)
        timeline.record(300, 3_000)
        assertNull(timeline.take(1_000))
        assertEquals(200L, timeline.take(2_000)?.presentationTimeUs)
        assertEquals(300L, timeline.take(3_000)?.presentationTimeUs)
    }
}
