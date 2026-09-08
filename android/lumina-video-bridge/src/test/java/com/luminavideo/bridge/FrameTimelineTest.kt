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
        val inFlight = timeline.take(1_000, 0)
        timeline.reset(1)
        assertNull(timeline.take(2_000, 0))
        assertEquals(0L, inFlight?.generation)
        timeline.record(5_000, 3_000)
        assertEquals(FrameTimeline.Stamp(5_000, 1), timeline.take(3_000, 0))
        assertNull(timeline.take(3_000, 0))
    }

    @Test
    fun overloadedTimelineDropsTheOldestReleaseTime() {
        val timeline = FrameTimeline(2)
        timeline.record(100, 1_000)
        timeline.record(200, 2_000)
        timeline.record(300, 3_000)
        assertNull(timeline.take(1_000, 0))
        assertEquals(200L, timeline.take(2_000, 0)?.presentationTimeUs)
        assertEquals(300L, timeline.take(3_000, 0)?.presentationTimeUs)
    }

    @Test
    fun liveFramesFollowTheCurrentWindowOriginInsteadOfThePeriodOrigin() {
        val timeline = FrameTimeline(2)
        timeline.record(12_320_000, 1_000)
        assertEquals(320_000L, timeline.take(1_000, -12_000_000)?.presentationTimeUs)

        // The manifest advances after the metadata callback but before Image delivery.
        timeline.record(13_320_000, 2_000)
        assertEquals(320_000L, timeline.take(2_000, -13_000_000)?.presentationTimeUs)
    }

    @Test
    fun seekAndPeriodTransitionDiscardUnmatchedImagesAndUseTheNewOrigin() {
        val timeline = FrameTimeline(2)
        timeline.record(12_320_000, 1_000)
        timeline.reset(7)
        assertNull(timeline.take(1_000, -12_000_000))
        timeline.record(14_000_000, 2_000)
        assertEquals(FrameTimeline.Stamp(2_000_000, 7), timeline.take(2_000, -12_000_000))

        timeline.record(15_000_000, 3_000)
        timeline.reset(timeline.generation())
        assertNull(timeline.take(3_000, 20_000_000))
        timeline.record(500_000, 4_000)
        assertEquals(FrameTimeline.Stamp(20_500_000, 7), timeline.take(4_000, 20_000_000))
    }

    @Test
    fun framesOutsideTheWindowOrNativeTimestampRangeAreDropped() {
        val timeline = FrameTimeline(2)
        timeline.record(1_000_000, 1_000)
        assertNull(timeline.take(1_000, -2_000_000))
        timeline.record(Long.MAX_VALUE, 2_000)
        assertNull(timeline.take(2_000, 1))
        timeline.record(Long.MAX_VALUE / 1000 + 1, 3_000)
        assertNull(timeline.take(3_000, 0))
    }
}
