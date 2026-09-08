package com.luminavideo.bridge

/** Bounded correspondence between codec release times and media PTS across seeks. */
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
    fun take(releaseTimeNs: Long): Stamp? = frames.remove(releaseTimeNs)
}
