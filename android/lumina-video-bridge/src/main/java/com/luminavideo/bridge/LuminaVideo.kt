/**
 * Static initialization entry point for lumina-video on Android.
 *
 * Call [LuminaVideo.init] once in your Activity's `onCreate()` to enable
 * native-frame Android playback from Rust via `AndroidVideoDecoder::new()`.
 *
 * # Usage
 *
 * ```kotlin
 * class MyActivity : GameActivity() {
 *     override fun onCreate(savedInstanceState: Bundle?) {
 *         LuminaVideo.init(this)
 *         super.onCreate(savedInstanceState)
 *     }
 * }
 * ```
 *
 * # How It Works
 *
 * When Rust's `AndroidVideoDecoder::new()` runs (on a background thread), it calls
 * `LuminaVideo.createPlayer(nativeHandle)` via JNI. This method:
 * 1. Creates a dedicated HandlerThread with a Looper
 * 2. Creates ExoPlayer on that thread (ExoPlayer requires a Looper)
 * 3. Sets up ImageReader for HardwareBuffer extraction
 * 4. Blocks the calling thread with CountDownLatch until ready
 * 5. Returns the configured ExoPlayerBridge
 *
 * This eliminates the split-brain architecture where Kotlin-side ExoPlayer and
 * Rust-side AndroidVideoDecoder were independent, making `play()` a no-op.
 */
package com.luminavideo.bridge

import android.app.Activity
import android.content.Context
import android.util.Log
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.LifecycleOwner
import androidx.media3.exoplayer.ExoPlayer
import java.util.concurrent.atomic.AtomicBoolean

object LuminaVideo {
    private const val TAG = "LuminaVideo"

    private val initialized = AtomicBoolean(false)
    private var appContext: Context? = null
    private var customBuilder: ExoPlayer.Builder? = null
    private var generation = 0L

    /** All bridges created via [createPlayer], tracked for lifecycle cleanup. */
    private val activeBridges = mutableListOf<ExoPlayerBridge>()

    /**
     * Initializes lumina-video for self-contained Android video playback.
     *
     * Must be called once from your Activity's `onCreate()` before any
     * `AndroidVideoDecoder::new()` calls from Rust.
     *
     * @param activity The host Activity. Only `applicationContext` is retained (no leak).
     * @param builder Optional custom ExoPlayer.Builder for advanced configuration
     *                (e.g., custom MediaSource factories, bandwidth meters).
     *                If null, a default builder is used.
     */
    @JvmStatic
    @JvmOverloads
    fun init(activity: Activity, builder: ExoPlayer.Builder? = null) {
        val activityGeneration = synchronized(activeBridges) {
            if (!initialized.compareAndSet(false, true)) {
                Log.w(TAG, "LuminaVideo.init() already called, ignoring")
                return
            }
            appContext = activity.applicationContext
            customBuilder = builder
            generation++
            generation
        }

        // Register lifecycle observer for cleanup on Activity destroy.
        // On destroy, reset initialized so a recreated Activity can re-register.
        if (activity is LifecycleOwner) {
            activity.lifecycle.addObserver(LifecycleEventObserver { _, event ->
                if (event == Lifecycle.Event.ON_DESTROY) {
                    shutdown(activityGeneration)
                }
            })
        } else {
            Log.i(TAG, "NativeActivity hosts must call LuminaVideo.shutdown() from onDestroy()")
        }
    }

    /** NativeActivity hosts call this from onDestroy(); LifecycleOwner hosts call it automatically. */
    @JvmStatic
    fun shutdown() = shutdown(null)

    private fun shutdown(expectedGeneration: Long?) {
        val bridges = synchronized(activeBridges) {
            if (expectedGeneration != null && generation != expectedGeneration) return
            val snapshot = activeBridges.toList()
            activeBridges.clear()
            appContext = null
            customBuilder = null
            initialized.set(false)
            generation++
            snapshot
        }
        bridges.forEach { it.release() }
    }

    internal fun onBridgeReleased(bridge: ExoPlayerBridge) {
        synchronized(activeBridges) { activeBridges.remove(bridge) }
    }

    /**
     * Creates an ExoPlayer instance and returns a configured bridge.
     *
     * Called from Rust JNI (`android_video.rs`) on a background thread.
     * Creates a dedicated HandlerThread for ExoPlayer, blocks until ready.
     *
     * @param nativeHandle Native pointer for Rust-side SharedState (for JNI callbacks)
     * @return Configured ExoPlayerBridge, or null if init() wasn't called or creation failed
     */
    @JvmStatic
    fun createPlayer(nativeHandle: Long): ExoPlayerBridge? {
        val (ctx, builder, playerGeneration) = synchronized(activeBridges) {
            Triple(appContext, customBuilder, generation)
        }
        if (ctx == null) {
            Log.e(TAG, "createPlayer() called before init()")
            return null
        }
        val bridge = ExoPlayerBridge(nativeHandle)
        if (!bridge.initializeWithPlayer(ctx, builder)) return null
        val accepted = synchronized(activeBridges) {
            if (initialized.get() && generation == playerGeneration) {
                activeBridges.add(bridge)
                true
            } else false
        }
        if (!accepted) {
            bridge.release()
            return null
        }

        Log.i(TAG, "createPlayer() succeeded, nativeHandle=$nativeHandle")
        return bridge
    }
}
