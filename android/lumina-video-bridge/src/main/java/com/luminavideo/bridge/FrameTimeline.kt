package com.luminavideo.bridge

/** Bounded correspondence between codec release times and period PTS across seeks. */
internal class FrameTimeline(private val capacity: Int) {
    data class Stamp(val presentationTimeUs: Long, val generation: Long)

    private val frames = LinkedHashMap<Long, Stamp>()
    private var generation = 0L

    @Synchronized
    fun generation(): Long = generation

    @Synchronized
    fun reset(nextGeneration: Long) {
        generation = nextGeneration
        frames.clear()
    }

    @Synchronized
    fun record(presentationTimeUs: Long, releaseTimeNs: Long) {
        if (frames.size >= capacity) {
            val oldest = frames.keys.iterator()
            if (oldest.hasNext()) {
                oldest.next()
                oldest.remove()
            }
        }
        frames[releaseTimeNs] = Stamp(presentationTimeUs, generation)
    }

    @Synchronized
    fun take(releaseTimeNs: Long, periodPositionInWindowUs: Long): Stamp? {
        val stamp = frames.remove(releaseTimeNs) ?: return null
        // Media3's frame listener reports period time; currentPosition reports
        // window time. Resolve at consumption because a live window can move
        // while an Image is pending, without changing the period or generation.
        val windowTimeUs = try {
            Math.addExact(stamp.presentationTimeUs, periodPositionInWindowUs)
        } catch (_: ArithmeticException) {
            return null
        }
        // Frames before a sliding window, or outside the native nanosecond
        // timestamp range, cannot be submitted to the presentation queue.
        if (windowTimeUs < 0 || windowTimeUs > Long.MAX_VALUE / 1000L) return null
        return stamp.copy(presentationTimeUs = windowTimeUs)
    }
}
