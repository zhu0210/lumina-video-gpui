//! GStreamer-based video decoder for Linux.
//!
//! This module provides hardware-accelerated video decoding using GStreamer,
//! which handles codec edge cases (frame_num gaps, broken streams) robustly.
//!
//! GStreamer automatically selects the best decoder (VA-API, software fallback)
//! and handles all the complexity of H.264/VP8/VP9/AV1 decoding.
//!
//! Audio is played directly by GStreamer via autoaudiosink, with volume control
//! exposed through the GStreamer volume element.
//!
//! ## Zero-Copy DMABuf Support
//!
//! When the `zero-copy` feature is enabled, this decoder can expose DMABuf file
//! descriptors from VA-API decoded frames. This allows GPU-to-GPU transfers without
//! copying data through the CPU.
//!
//! ## DRM Modifier Support (GStreamer 1.24+)
//!
//! With GStreamer 1.24+, the `va` plugin exposes DRM modifiers in caps via the
//! `drm-format` field (e.g., `NV12:0x0100000000000002` for Intel X-tile).
//! This module parses the modifier to ensure correct Vulkan import of tiled buffers.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use lumina_video_core::session::AudioTrack;

use crate::video::{
    CpuFrame, DecodedFrame, HwAccelType, PixelFormat, Plane, VideoDecoderBackend, VideoError,
    VideoFrame, VideoMetadata,
};

use crate::video::{DmaBufPlane, LinuxGpuSurface};

/// Selects the sink used by a GStreamer audio branch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GstAudioSinkMode {
    /// Use the platform's normal audio sink.
    #[default]
    Auto,
    /// Use a synchronized headless sink for deterministic harnesses.
    Fake,
}

/// Shared audio state for GStreamer audio control.
/// This is used to control volume/mute from the UI thread.
#[derive(Clone)]
pub struct GstAudioHandle {
    inner: Arc<GstAudioHandleInner>,
}

/// Result of an in-session audio stream selection attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioTrackSelectionResult {
    Selected(AudioTrack),
    Failed {
        requested_id: String,
        prior_restored_id: Option<String>,
        reason: String,
    },
}

struct GstAudioHandleInner {
    /// Volume element for control (None if no audio)
    volume_element: Option<gst::Element>,
    /// Whether audio is available
    has_audio: AtomicBool,
    /// Number of buffers observed on the connected audio branch.
    audio_buffers_seen: AtomicU64,
    /// Whether audio is muted
    muted: AtomicBool,
    /// Volume level (0.0 - 1.0)
    volume: std::sync::atomic::AtomicU32, // stored as volume * 100
}

impl GstAudioHandle {
    fn new(volume_element: Option<gst::Element>) -> Self {
        // Start with has_audio=false; set to true when audio pad connects
        Self {
            inner: Arc::new(GstAudioHandleInner {
                volume_element,
                has_audio: AtomicBool::new(false),
                audio_buffers_seen: AtomicU64::new(0),
                muted: AtomicBool::new(false),
                volume: std::sync::atomic::AtomicU32::new(100), // 100%
            }),
        }
    }

    /// Called when an audio pad successfully connects.
    fn set_audio_connected(&self) {
        self.inner.has_audio.store(true, Ordering::Relaxed);
    }

    /// Returns whether audio is available.
    pub fn has_audio(&self) -> bool {
        self.inner.has_audio.load(Ordering::Relaxed)
    }

    /// Returns the number of buffers observed on the audio branch.
    pub fn audio_buffers_seen(&self) -> u64 {
        self.inner.audio_buffers_seen.load(Ordering::Relaxed)
    }

    fn record_audio_buffer(&self) {
        self.inner
            .audio_buffers_seen
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Returns whether audio is muted.
    pub fn is_muted(&self) -> bool {
        self.inner.muted.load(Ordering::Relaxed)
    }

    /// Sets the mute state.
    pub fn set_muted(&self, muted: bool) {
        self.inner.muted.store(muted, Ordering::Relaxed);
        self.apply_volume();
    }

    /// Toggles mute state.
    pub fn toggle_mute(&self) {
        // Use fetch_xor for atomic toggle to avoid TOCTOU race condition
        self.inner.muted.fetch_xor(true, Ordering::Relaxed);
        self.apply_volume();
    }

    /// Returns the current volume (0-100).
    pub fn volume(&self) -> u32 {
        self.inner.volume.load(Ordering::Relaxed)
    }

    /// Sets the volume (0-100).
    pub fn set_volume(&self, volume: u32) {
        self.inner.volume.store(volume.min(100), Ordering::Relaxed);
        self.apply_volume();
    }

    /// Applies the current volume/mute state to the GStreamer element.
    fn apply_volume(&self) {
        if let Some(ref vol_elem) = self.inner.volume_element {
            let effective_volume = if self.inner.muted.load(Ordering::Relaxed) {
                0.0
            } else {
                self.inner.volume.load(Ordering::Relaxed) as f64 / 100.0
            };
            vol_elem.set_property("volume", effective_volume);
        }
    }
}

/// Buffering thresholds for hysteresis to prevent rapid pause/resume oscillation.
/// - Low threshold: pause only when buffer drops critically low
/// - High threshold: resume only when buffer is sufficiently full
///
/// The gap between thresholds prevents rapid state changes on marginal connections.
const BUFFER_LOW_THRESHOLD: i32 = 10; // Pause when buffer drops below this %
const BUFFER_HIGH_THRESHOLD: i32 = 100; // Resume when buffer reaches this %
const LIFECYCLE_POLL: Duration = Duration::from_millis(50);

/// Default bound for one seek/resync or decoder teardown operation.
pub const DEFAULT_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(2);

/// Cancellation shared by a session and its GStreamer worker.
///
/// This is deliberately separate from the command mailbox: dropping or
/// replacing a session must still wake a worker that is opening a pipeline or
/// waiting for a sample even when the command queue is full.
#[derive(Clone, Debug)]
pub struct GstLifecycleControl {
    cancelled: Arc<AtomicBool>,
    stop_requested: Arc<AtomicBool>,
    deadline: Arc<std::sync::Mutex<Option<Instant>>>,
}

impl GstLifecycleControl {
    /// Creates an active lifecycle control.
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            stop_requested: Arc::new(AtomicBool::new(false)),
            deadline: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Requests cancellation and records the one absolute cleanup deadline.
    pub fn cancel(&self, timeout: Duration) {
        let now = Instant::now();
        let requested_deadline = now.checked_add(timeout).unwrap_or(now);
        if let Ok(mut shared_deadline) = self.deadline.lock() {
            *shared_deadline = Some(match *shared_deadline {
                Some(existing) if existing <= requested_deadline => existing,
                _ => requested_deadline,
            });
        }
        self.cancelled.store(true, Ordering::Release);
    }

    /// Requests worker-side Stop while also bypassing the command FIFO.
    pub fn request_stop(&self, timeout: Duration) {
        self.stop_requested.store(true, Ordering::Release);
        self.cancel(timeout);
    }

    /// Returns whether the owning session has been dropped or replaced.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Returns whether cancellation was requested by an explicit Stop command.
    pub fn is_stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::Acquire)
    }

    /// Returns the absolute deadline established by [`Self::cancel`].
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline.lock().ok().and_then(|deadline| *deadline)
    }
}

impl Default for GstLifecycleControl {
    fn default() -> Self {
        Self::new()
    }
}

/// GStreamer-based video decoder for Linux.
///
/// Uses a GStreamer pipeline:
/// - Video: `uridecodebin3 ! videoconvert ! video/x-raw,format=NV12 ! appsink`
/// - Audio: `uridecodebin3 ! audioconvert ! audioresample ! volume ! autoaudiosink`
///
/// This handles:
/// - HTTP/HTTPS streaming
/// - All common codecs (H.264, VP8, VP9, AV1)
/// - Hardware acceleration via VA-API (automatic)
/// - Edge cases that break other decoders
/// - Audio playback with volume control
pub struct GStreamerDecoder {
    pipeline: gst::Pipeline,
    appsink: gst_app::AppSink,
    /// Keep the #7 session on the owned system-memory path.
    system_memory_only: bool,
    metadata: VideoMetadata,
    position: Duration,
    eof: bool,
    /// True if we just seeked and are waiting for first frame
    seeking: bool,
    /// Target position of the last seek (for stale frame detection)
    seek_target: Option<Duration>,
    /// True if the last seek was backward (target < position at seek time)
    last_seek_backward: bool,
    /// Deadline shared by the seek and first-frame resync phase.
    seek_deadline: Option<Instant>,
    /// Absolute deadline for the current worker operation, retained after a
    /// failed seek so teardown cannot start a fresh timeout window.
    active_operation_deadline: Option<Instant>,
    /// Cached preroll sample for first decode_next() call
    preroll_sample: Option<gst::Sample>,
    /// Buffering percentage (0-100), 100 means fully buffered
    buffering_percent: i32,
    /// True once we've reached 100% buffering at least once (for rebuffer detection)
    was_fully_buffered: bool,
    /// True if the user explicitly paused (prevents buffering auto-resume)
    user_paused: bool,
    /// Queued error from bus messages during seek (returned on next decode_next)
    pending_error: Option<VideoError>,
    /// Audio control handle
    audio_handle: GstAudioHandle,
    /// Discoverable audio tracks from the latest StreamCollection.
    audio_tracks: Vec<AudioTrack>,
    /// Every video stream id from the latest StreamCollection. GStreamer
    /// requires these ids to accompany an audio id in SELECT_STREAMS.
    video_stream_ids: Vec<String>,
    /// Video ids confirmed by the latest StreamsSelected message.
    selected_video_stream_ids: Vec<String>,
    /// Audio id confirmed by the latest StreamsSelected message.
    selected_audio_stream_id: Option<String>,
    /// Set when a new StreamCollection changes the public track metadata.
    audio_tracks_changed: bool,
    lifecycle_timeout: Duration,
    lifecycle_control: GstLifecycleControl,
    cleaned_up: bool,
}

impl GStreamerDecoder {
    fn stream_audio_track(stream: &gst::Stream) -> Option<AudioTrack> {
        let id = stream.stream_id()?.to_string();
        let tags = stream.tags();
        let language = tags
            .as_ref()
            .and_then(|tags| tags.get::<gst::tags::LanguageCode>())
            .map(|value| value.get().to_string());
        let title = tags
            .as_ref()
            .and_then(|tags| tags.get::<gst::tags::Title>())
            .map(|value| value.get().to_string());
        let codec = tags
            .as_ref()
            .and_then(|tags| tags.get::<gst::tags::AudioCodec>())
            .map(|value| value.get().to_string())
            .or_else(|| stream.caps().and_then(Self::caps_audio_codec))
            .unwrap_or_else(|| "unknown".into());

        Some(AudioTrack {
            id,
            language,
            title,
            codec,
        })
    }

    fn caps_audio_codec(caps: gst::Caps) -> Option<String> {
        let structure = caps.structure(0)?;
        let name = structure.name();
        match name {
            "audio/mpeg" => match structure.get::<i32>("mpegversion").ok() {
                Some(4) => Some("AAC".into()),
                _ => Some(name.to_string()),
            },
            "audio/x-opus" => Some("Opus".into()),
            "audio/x-vorbis" => Some("Vorbis".into()),
            _ => Some(name.to_string()),
        }
    }

    fn collection_metadata(collection: &gst::StreamCollection) -> (Vec<AudioTrack>, Vec<String>) {
        let mut audio_tracks = Vec::new();
        let mut video_stream_ids = Vec::new();
        for stream in collection {
            let stream_type = stream.stream_type();
            if stream_type.contains(gst::StreamType::AUDIO) {
                if let Some(track) = Self::stream_audio_track(&stream) {
                    audio_tracks.push(track);
                }
            }
            if stream_type.contains(gst::StreamType::VIDEO) {
                if let Some(id) = stream.stream_id() {
                    video_stream_ids.push(id.to_string());
                }
            }
        }
        (audio_tracks, video_stream_ids)
    }

    fn selected_audio_id(message: &gst::message::StreamsSelected) -> Option<String> {
        message.streams().find_map(|stream| {
            if stream.stream_type().contains(gst::StreamType::AUDIO) {
                stream.stream_id().map(|id| id.to_string())
            } else {
                None
            }
        })
    }

    fn capture_stream_collection(&mut self, collection: &gst::StreamCollection) {
        let (audio_tracks, video_stream_ids) = Self::collection_metadata(collection);
        if self.audio_tracks != audio_tracks || self.video_stream_ids != video_stream_ids {
            self.audio_tracks = audio_tracks;
            self.video_stream_ids = video_stream_ids;
            self.audio_tracks_changed = true;
        }
    }

    fn capture_selected_streams(&mut self, message: &gst::message::StreamsSelected) {
        self.selected_audio_stream_id = Self::selected_audio_id(message);
        self.selected_video_stream_ids = message
            .streams()
            .filter_map(|stream| {
                if stream.stream_type().contains(gst::StreamType::VIDEO) {
                    stream.stream_id().map(|id| id.to_string())
                } else {
                    None
                }
            })
            .collect();
    }

    fn earliest_deadline(first: Option<Instant>, second: Option<Instant>) -> Option<Instant> {
        match (first, second) {
            (Some(first), Some(second)) => Some(if first <= second { first } else { second }),
            (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
            (None, None) => None,
        }
    }

    fn cleanup_pipeline(pipeline: &gst::Pipeline, deadline: Instant) {
        let _ = pipeline.set_state(gst::State::Null);
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        if !remaining.is_zero() {
            let nanos = remaining.as_nanos().min(u64::MAX as u128) as u64;
            let _ = pipeline.state(gst::ClockTime::from_nseconds(nanos));
        }
    }

    /// Creates a new GStreamer decoder for the given URL.
    pub fn new(url: &str) -> Result<Self, VideoError> {
        Self::new_with_memory_policy_and_audio_sink_and_timeout(
            url,
            false,
            GstAudioSinkMode::Auto,
            DEFAULT_LIFECYCLE_TIMEOUT,
            GstLifecycleControl::new(),
        )
    }

    /// Creates a decoder that rejects DMABuf output and returns owned CPU frames.
    ///
    /// The session adapter uses this path because its renderer contract is
    /// `SystemMemoryUpload`; the one GStreamer buffer-to-CPU extraction is the
    /// ownership hand-off and no second PTS wait or frame copy is introduced.
    pub fn new_system_memory(url: &str) -> Result<Self, VideoError> {
        Self::new_with_memory_policy_and_audio_sink_and_timeout(
            url,
            true,
            GstAudioSinkMode::Auto,
            DEFAULT_LIFECYCLE_TIMEOUT,
            GstLifecycleControl::new(),
        )
    }

    /// Creates a system-memory decoder with an explicit audio sink policy.
    pub fn new_system_memory_with_audio_sink(
        url: &str,
        audio_sink: GstAudioSinkMode,
    ) -> Result<Self, VideoError> {
        Self::new_with_memory_policy_and_audio_sink_and_timeout(
            url,
            true,
            audio_sink,
            DEFAULT_LIFECYCLE_TIMEOUT,
            GstLifecycleControl::new(),
        )
    }

    /// Creates a system-memory decoder using a worker-owned cancellation
    /// signal. The worker uses this constructor so opening and teardown obey
    /// the same session lifecycle deadline.
    pub fn new_system_memory_with_audio_sink_and_timeout_and_control(
        url: &str,
        audio_sink: GstAudioSinkMode,
        lifecycle_timeout: Duration,
        lifecycle_control: GstLifecycleControl,
    ) -> Result<Self, VideoError> {
        Self::new_with_memory_policy_and_audio_sink_and_timeout(
            url,
            true,
            audio_sink,
            lifecycle_timeout,
            lifecycle_control,
        )
    }

    /// Returns the latest discoverable audio tracks.
    pub fn audio_tracks(&self) -> &[AudioTrack] {
        &self.audio_tracks
    }

    /// Returns the audio stream id confirmed by GStreamer.
    pub fn selected_audio_track_id(&self) -> Option<&str> {
        self.selected_audio_stream_id.as_deref()
    }

    /// Takes a track update observed after initialization.
    pub fn take_audio_tracks_update(&mut self) -> Option<Vec<AudioTrack>> {
        if !self.audio_tracks_changed {
            return None;
        }
        self.audio_tracks_changed = false;
        Some(self.audio_tracks.clone())
    }

    fn new_with_memory_policy_and_audio_sink_and_timeout(
        url: &str,
        system_memory_only: bool,
        audio_sink: GstAudioSinkMode,
        lifecycle_timeout: Duration,
        lifecycle_control: GstLifecycleControl,
    ) -> Result<Self, VideoError> {
        let init_now = Instant::now();
        let init_deadline = init_now.checked_add(lifecycle_timeout).unwrap_or(init_now);
        if lifecycle_control.is_cancelled() {
            return Err(VideoError::DecoderInit("lifecycle cancelled".into()));
        }

        // Initialize vendored runtime environment before GStreamer init
        #[cfg(feature = "vendored-runtime")]
        {
            let runtime = crate::vendored_runtime::VendoredRuntime::new();
            if !runtime.init() {
                tracing::warn!("vendored-runtime: vendor directory not found; falling back to system libraries");
            }
        }

        // Initialize GStreamer (safe to call multiple times)
        gst::init().map_err(|e| VideoError::DecoderInit(format!("GStreamer init failed: {e}")))?;

        // Build the pipeline
        let pipeline = gst::Pipeline::new();

        // Source element - handles HTTP, HTTPS, file://
        let source = gst::ElementFactory::make("uridecodebin3")
            .property("uri", url)
            .build()
            .map_err(|e| VideoError::DecoderInit(format!("Failed to create uridecodebin3: {e}")))?;

        // === Video elements ===
        let videoconvert = gst::ElementFactory::make("videoconvert")
            .build()
            .map_err(|e| VideoError::DecoderInit(format!("Failed to create videoconvert: {e}")))?;

        // App sink to pull video frames - constrained buffering for better seek behavior
        let appsink = gst_app::AppSink::builder()
            .caps(
                &gst_video::VideoCapsBuilder::new()
                    .format(gst_video::VideoFormat::Nv12)
                    .build(),
            )
            .max_buffers(1)
            .drop(true)
            .build();

        // === Audio elements ===
        let audioconvert = gst::ElementFactory::make("audioconvert")
            .build()
            .map_err(|e| VideoError::DecoderInit(format!("Failed to create audioconvert: {e}")))?;

        let audioresample = gst::ElementFactory::make("audioresample")
            .build()
            .map_err(|e| VideoError::DecoderInit(format!("Failed to create audioresample: {e}")))?;

        let volume = gst::ElementFactory::make("volume")
            .property("volume", 1.0f64)
            .build()
            .map_err(|e| VideoError::DecoderInit(format!("Failed to create volume: {e}")))?;
        let audio_probe_pad = volume.static_pad("sink");

        let audiosink = match audio_sink {
            GstAudioSinkMode::Auto => {
                gst::ElementFactory::make("autoaudiosink")
                    .build()
                    .map_err(|e| {
                        VideoError::DecoderInit(format!("Failed to create autoaudiosink: {e}"))
                    })?
            }
            GstAudioSinkMode::Fake => gst::ElementFactory::make("fakesink")
                .property("sync", true)
                .build()
                .map_err(|e| {
                    VideoError::DecoderInit(format!("Failed to create synchronized fakesink: {e}"))
                })?,
        };

        // Add all elements to pipeline
        pipeline
            .add_many([
                &source,
                &videoconvert,
                appsink.upcast_ref(),
                &audioconvert,
                &audioresample,
                &volume,
                &audiosink,
            ])
            .map_err(|e| VideoError::DecoderInit(format!("Failed to add elements: {e}")))?;

        // Link video chain: videoconvert -> appsink
        videoconvert
            .link(&appsink)
            .map_err(|e| VideoError::DecoderInit(format!("Failed to link video elements: {e}")))?;

        // Link audio chain: audioconvert -> audioresample -> volume -> audiosink
        gst::Element::link_many([&audioconvert, &audioresample, &volume, &audiosink])
            .map_err(|e| VideoError::DecoderInit(format!("Failed to link audio elements: {e}")))?;

        // Create audio handle with volume element (has_audio starts false until pad connects)
        let audio_handle = GstAudioHandle::new(Some(volume));

        if let Some(audio_pad) = audio_probe_pad {
            let audio_handle = audio_handle.clone();
            let _ = audio_pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
                audio_handle.record_audio_buffer();
                gst::PadProbeReturn::Ok
            });
        }

        // Handle dynamic pad creation from uridecodebin3. Its stream
        // selection can remove and add pads in-session; the removed pad is
        // unlinked by GStreamer before the replacement arrives.
        let videoconvert_weak = videoconvert.downgrade();
        let audioconvert_weak = audioconvert.downgrade();
        let audio_handle_clone = audio_handle.clone();
        source.connect_pad_added(move |_src, src_pad| {
            let caps = src_pad
                .current_caps()
                .unwrap_or_else(|| src_pad.query_caps(None));
            let Some(structure) = caps.structure(0) else {
                return;
            };
            let name = structure.name();

            if name.starts_with("video/") {
                if let Some(videoconvert) = videoconvert_weak.upgrade() {
                    let Some(sink_pad) = videoconvert.static_pad("sink") else {
                        tracing::warn!("videoconvert element has no sink pad");
                        return;
                    };
                    if !sink_pad.is_linked() {
                        if let Err(e) = src_pad.link(&sink_pad) {
                            tracing::warn!("Failed to link video pad: {:?}", e);
                        } else {
                            tracing::info!("Linked video pad: {}", name);
                        }
                    }
                }
            } else if name.starts_with("audio/") {
                if let Some(audioconvert) = audioconvert_weak.upgrade() {
                    let Some(sink_pad) = audioconvert.static_pad("sink") else {
                        tracing::warn!("audioconvert element has no sink pad");
                        return;
                    };
                    if !sink_pad.is_linked() {
                        if let Err(e) = src_pad.link(&sink_pad) {
                            tracing::warn!("Failed to link audio pad: {:?}", e);
                        } else {
                            tracing::info!("Linked audio pad: {}", name);
                            audio_handle_clone.set_audio_connected();
                        }
                    }
                }
            }
        });

        // Set pipeline to Paused to get metadata without starting playback
        // (Playing state would autoplay the video)
        pipeline
            .set_state(gst::State::Paused)
            .map_err(|e| VideoError::DecoderInit(format!("Failed to start pipeline: {e:?}")))?;

        // Wait for pipeline to reach paused state (preroll) or error
        let Some(bus) = pipeline.bus() else {
            Self::cleanup_pipeline(&pipeline, init_deadline);
            return Err(VideoError::DecoderInit("Pipeline has no bus".to_string()));
        };
        let mut width = 0u32;
        let mut height = 0u32;
        let mut duration = None;
        let mut initial_audio_tracks = Vec::new();
        let mut initial_video_stream_ids = Vec::new();
        let mut initial_selected_audio_stream_id = None;
        let mut initial_selected_video_stream_ids = Vec::new();

        // Track buffering during init (in case 100% is reached before decode loop starts)
        let mut init_buffering_percent = 0i32;

        // Wait for async state change and get metadata. Small polls keep
        // cancellation observable while a network source is opening.
        loop {
            if lifecycle_control.is_cancelled() {
                Self::cleanup_pipeline(
                    &pipeline,
                    lifecycle_control.deadline().unwrap_or(init_deadline),
                );
                return Err(VideoError::DecoderInit("lifecycle cancelled".into()));
            }
            let remaining = init_deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                Self::cleanup_pipeline(&pipeline, init_deadline);
                return Err(VideoError::DecoderInit(
                    "pipeline initialization timed out".into(),
                ));
            }
            let timeout = remaining.min(LIFECYCLE_POLL);
            let Some(msg) = bus.timed_pop(Self::clock_time(timeout)) else {
                continue;
            };
            match msg.view() {
                gst::MessageView::AsyncDone(_) => {
                    // Query duration
                    if let Some(dur) = pipeline.query_duration::<gst::ClockTime>() {
                        duration = Some(Duration::from_nanos(dur.nseconds()));
                    }
                    break;
                }
                gst::MessageView::StreamCollection(collection) => {
                    let (audio_tracks, video_stream_ids) =
                        Self::collection_metadata(&collection.stream_collection());
                    initial_audio_tracks = audio_tracks;
                    initial_video_stream_ids = video_stream_ids;
                }
                gst::MessageView::StreamsSelected(selected) => {
                    initial_selected_audio_stream_id = Self::selected_audio_id(selected);
                    initial_selected_video_stream_ids = selected
                        .streams()
                        .filter_map(|stream| {
                            if stream.stream_type().contains(gst::StreamType::VIDEO) {
                                stream.stream_id().map(|id| id.to_string())
                            } else {
                                None
                            }
                        })
                        .collect();
                }
                gst::MessageView::Error(err) => {
                    // Clean up pipeline before returning error
                    Self::cleanup_pipeline(&pipeline, init_deadline);
                    return Err(VideoError::DecoderInit(format!(
                        "Pipeline error: {} ({:?})",
                        err.error(),
                        err.debug()
                    )));
                }
                gst::MessageView::StateChanged(state) => {
                    if state
                        .src()
                        .map(|s| s == pipeline.upcast_ref::<gst::Object>())
                        .unwrap_or(false)
                    {
                        tracing::debug!(
                            "Pipeline state: {:?} -> {:?}",
                            state.old(),
                            state.current()
                        );
                    }
                }
                gst::MessageView::Buffering(buffering) => {
                    // Track buffering during init - important for fast streams
                    // that reach 100% before decode loop starts
                    init_buffering_percent = buffering.percent();
                    tracing::debug!("Init buffering: {}%", init_buffering_percent);
                }
                _ => {}
            }
        }

        // StreamCollection and StreamsSelected can be posted immediately
        // after AsyncDone. Capture any already-queued metadata before the
        // first public session snapshot is built.
        while let Some(msg) = bus.pop() {
            match msg.view() {
                gst::MessageView::StreamCollection(collection) => {
                    let (audio_tracks, video_stream_ids) =
                        Self::collection_metadata(&collection.stream_collection());
                    initial_audio_tracks = audio_tracks;
                    initial_video_stream_ids = video_stream_ids;
                }
                gst::MessageView::StreamsSelected(selected) => {
                    initial_selected_audio_stream_id = Self::selected_audio_id(selected);
                    initial_selected_video_stream_ids = selected
                        .streams()
                        .filter_map(|stream| {
                            if stream.stream_type().contains(gst::StreamType::VIDEO) {
                                stream.stream_id().map(|id| id.to_string())
                            } else {
                                None
                            }
                        })
                        .collect();
                }
                _ => {}
            }
        }
        // Get video dimensions and frame rate from appsink caps
        let mut frame_rate = 30.0f32; // Default fallback
        if let Some(caps) = appsink.sink_pads().first().and_then(|p| p.current_caps()) {
            if let Some(s) = caps.structure(0) {
                width = s.get::<i32>("width").unwrap_or(0) as u32;
                height = s.get::<i32>("height").unwrap_or(0) as u32;
                // Extract frame rate from caps (stored as fraction)
                if let Ok(fps) = s.get::<gst::Fraction>("framerate") {
                    if fps.denom() != 0 {
                        frame_rate = fps.numer() as f32 / fps.denom() as f32;
                        tracing::debug!("Detected frame rate: {:.2} fps", frame_rate);
                    }
                }
            }
        }

        // Try to pull preroll sample - this gives us dimensions AND the first
        // frame. Small polls keep cancellation observable for slow streams.
        let preroll_sample = loop {
            if lifecycle_control.is_cancelled() {
                Self::cleanup_pipeline(
                    &pipeline,
                    lifecycle_control.deadline().unwrap_or(init_deadline),
                );
                return Err(VideoError::DecoderInit("lifecycle cancelled".into()));
            }
            let remaining = init_deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                Self::cleanup_pipeline(&pipeline, init_deadline);
                return Err(VideoError::DecoderInit("pipeline preroll timed out".into()));
            }
            if let Some(sample) =
                appsink.try_pull_preroll(Self::clock_time(remaining.min(LIFECYCLE_POLL)))
            {
                break Some(sample);
            }
        };

        // If we couldn't get dimensions/framerate from caps, try from preroll sample
        if width == 0 || height == 0 || frame_rate == 30.0 {
            if let Some(ref sample) = preroll_sample {
                if let Some(caps) = sample.caps() {
                    if let Some(s) = caps.structure(0) {
                        if width == 0 {
                            width = s.get::<i32>("width").unwrap_or(0) as u32;
                        }
                        if height == 0 {
                            height = s.get::<i32>("height").unwrap_or(0) as u32;
                        }
                        // Try to get frame rate from preroll sample caps
                        if frame_rate == 30.0 {
                            if let Ok(fps) = s.get::<gst::Fraction>("framerate") {
                                if fps.denom() != 0 {
                                    frame_rate = fps.numer() as f32 / fps.denom() as f32;
                                    tracing::debug!(
                                        "Detected frame rate from preroll: {:.2} fps",
                                        frame_rate
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        if width == 0 || height == 0 {
            // Clean up pipeline before returning error
            Self::cleanup_pipeline(&pipeline, init_deadline);
            return Err(VideoError::DecoderInit(
                "Could not determine video dimensions".to_string(),
            ));
        }

        tracing::info!(
            "GStreamer decoder initialized: {}x{}, duration: {:?}, audio: {}",
            width,
            height,
            duration,
            audio_handle.has_audio()
        );

        let metadata = VideoMetadata {
            width,
            height,
            duration,
            frame_rate, // Extracted from caps, defaults to 30fps if not found
            codec: "unknown".to_string(), // GStreamer handles codec internally
            pixel_aspect_ratio: 1.0,
            start_time: None, // GStreamer handles sync internally
        };

        // For network streams, use buffering tracked during init (may have reached 100% already)
        // For local files, assume 100%
        let initial_buffering = if url.starts_with("http://") || url.starts_with("https://") {
            // Use the buffering percentage observed during init
            // This handles fast streams that buffer completely during preroll
            init_buffering_percent
        } else {
            100 // Local files are immediately available
        };

        Ok(Self {
            pipeline,
            appsink,
            system_memory_only,
            metadata,
            position: Duration::ZERO,
            eof: false,
            seeking: false,
            seek_target: None,
            last_seek_backward: false,
            seek_deadline: None,
            active_operation_deadline: None,
            preroll_sample,
            buffering_percent: initial_buffering,
            was_fully_buffered: initial_buffering >= 100,
            user_paused: false,
            pending_error: None,
            audio_handle,
            audio_tracks: initial_audio_tracks,
            video_stream_ids: initial_video_stream_ids,
            selected_video_stream_ids: initial_selected_video_stream_ids,
            selected_audio_stream_id: initial_selected_audio_stream_id,
            audio_tracks_changed: false,
            lifecycle_timeout,
            lifecycle_control,
            cleaned_up: false,
        })
    }

    /// Returns the audio handle for volume/mute control.
    pub fn audio_handle(&self) -> &GstAudioHandle {
        &self.audio_handle
    }

    /// Stops the pipeline with the configured worker-side deadline.
    pub fn shutdown(&mut self) {
        let deadline = Self::earliest_deadline(
            self.lifecycle_control.deadline(),
            self.active_operation_deadline,
        )
        .unwrap_or_else(|| self.deadline_for());
        self.cleanup_with_deadline(deadline);
    }

    /// Stops the pipeline without extending an already-started teardown
    /// deadline.
    pub fn shutdown_with_deadline(&mut self, deadline: Option<Instant>) {
        let deadline = Self::earliest_deadline(
            self.lifecycle_control.deadline(),
            Self::earliest_deadline(deadline, self.active_operation_deadline),
        )
        .unwrap_or_else(|| self.deadline_for());
        self.cleanup_with_deadline(deadline);
    }

    /// Returns the absolute deadline of the operation currently being
    /// completed by the worker, if any.
    pub fn active_operation_deadline(&self) -> Option<Instant> {
        self.active_operation_deadline
    }

    /// Starts one bounded worker operation and returns its absolute deadline.
    pub fn begin_operation_deadline(&mut self) -> Instant {
        let deadline = self.deadline_for();
        self.active_operation_deadline = Some(deadline);
        deadline
    }

    /// Sets the playback intent used when a seek has to produce a preroll
    /// frame. A paused session must remain paused after resync, including a
    /// seek issued after natural EOS.
    pub fn set_paused_intent(&mut self, paused: bool) {
        self.user_paused = paused;
    }

    /// Converts a GStreamer sample to our VideoFrame format.
    ///
    /// When the `zero-copy` feature is enabled, this will attempt to extract
    /// a DMABuf file descriptor from the sample first. If DMABuf is not available
    /// (e.g., software decoder, or unsupported allocator), it falls back to
    /// CPU memory copy.
    fn sample_to_frame(&self, sample: gst::Sample) -> Result<VideoFrame, VideoError> {
        let buffer = sample
            .buffer()
            .ok_or_else(|| VideoError::DecodeFailed("Sample has no buffer".to_string()))?;

        let caps = sample
            .caps()
            .ok_or_else(|| VideoError::DecodeFailed("Sample has no caps".to_string()))?;

        let video_info = gst_video::VideoInfo::from_caps(caps)
            .map_err(|e| VideoError::DecodeFailed(format!("Invalid video caps: {e}")))?;

        let pts = buffer
            .pts()
            .map(|t| Duration::from_nanos(t.nseconds()))
            .unwrap_or(self.position);

        let width = video_info.width();
        let height = video_info.height();

        // The #7 session explicitly negotiates system-memory upload. Do not
        // let a hardware allocator cross that seam as an implicit DMABuf path.
        if !self.system_memory_only {
            // Try zero-copy DMABuf path first (always enabled on Linux)
            if let Some(frame) =
                self.try_dmabuf_frame(buffer, &video_info, pts, width, height, sample.clone())?
            {
                return Ok(frame);
            }
            // Fall through to CPU path if DMABuf not available
        }

        // CPU copy path (fallback)
        self.sample_to_cpu_frame(buffer, &video_info, pts, width, height)
    }

    /// Parses the DRM modifier from GStreamer 1.24+ `drm-format` caps field.
    ///
    /// The `drm-format` field contains a string like `NV12:0x0100000000000002` where:
    /// - `NV12` is the DRM fourcc format
    /// - `0x0100000000000002` is the DRM modifier (e.g., Intel X-tile)
    ///
    /// Returns the modifier if found and parseable, or `None` if:
    /// - Caps don't have `drm-format` field (GStreamer < 1.24)
    /// - The format is LINEAR (no modifier suffix)
    /// - Parsing fails
    fn parse_drm_modifier_from_caps(sample: &gst::Sample) -> Option<u64> {
        let caps = sample.caps()?;
        let structure = caps.structure(0)?;

        // Try to get the drm-format field (GStreamer 1.24+ with va plugin)
        let drm_format: String = structure.get("drm-format").ok()?;

        // Parse format like "NV12:0x0100000000000002"
        // If no colon, it's just the format without modifier (assume LINEAR)
        let modifier_str = drm_format.split(':').nth(1)?;

        // Parse the hex modifier value
        let modifier = if modifier_str.starts_with("0x") || modifier_str.starts_with("0X") {
            u64::from_str_radix(&modifier_str[2..], 16).ok()?
        } else {
            modifier_str.parse::<u64>().ok()?
        };

        tracing::debug!(
            "Parsed DRM modifier from caps: drm-format='{}' -> modifier=0x{:016x}",
            drm_format,
            modifier
        );

        Some(modifier)
    }

    /// Extracts DMABuf file descriptors and per-plane metadata from a GStreamer buffer.
    ///
    /// Returns `Ok(Some(frame))` if DMABuf extraction succeeded,
    /// `Ok(None)` if the memory is not a DMABuf (fall back to CPU),
    /// `Err` if there was an error during extraction.
    ///
    /// # Multi-Plane Support (lumina-video-s0e)
    ///
    /// This function extracts per-plane metadata for multi-plane formats (NV12, YUV420p).
    /// GStreamer can provide planes in two configurations:
    /// 1. **Single FD with offsets**: All planes share one fd, distinguished by offset
    /// 2. **Multiple FDs**: Each plane has its own fd (offset is typically 0)
    ///
    /// Both cases are handled by checking `buffer.n_memory()` and extracting the
    /// appropriate fd/offset/stride for each plane from `video_info`.
    ///
    /// Multi-plane YUV formats are parsed and returned as [`DmaBufFrame`] with
    /// [`PixelFormat::Nv12`] or [`PixelFormat::Yuv420p`]. The zero-copy import path
    /// in [`zero_copy::linux`] handles these via `VkImageDrmFormatModifierExplicitCreateInfoEXT`.
    /// If zero-copy import fails (e.g., driver doesn't support the modifier), the
    /// CPU fallback path in [`DmaBufFrame`] is used automatically.
    fn try_dmabuf_frame(
        &self,
        buffer: &gst::BufferRef,
        video_info: &gst_video::VideoInfo,
        pts: Duration,
        width: u32,
        height: u32,
        sample: gst::Sample,
    ) -> Result<Option<VideoFrame>, VideoError> {
        use std::os::fd::RawFd;

        let format = video_info.format();
        let num_planes = video_info.n_planes() as usize;

        // Map GStreamer format to our PixelFormat
        let pixel_format = match format {
            gst_video::VideoFormat::Bgra | gst_video::VideoFormat::Bgrx => PixelFormat::Bgra,
            gst_video::VideoFormat::Rgba | gst_video::VideoFormat::Rgbx => PixelFormat::Rgba,
            gst_video::VideoFormat::Nv12 => PixelFormat::Nv12,
            gst_video::VideoFormat::I420 => PixelFormat::Yuv420p,
            _ => {
                tracing::debug!(
                    "Linux zero-copy: unsupported format {:?}, using CPU path",
                    format
                );
                return Ok(None);
            }
        };

        // Check if the first memory block is a DMABuf
        let Some(memory) = buffer.memory(0) else {
            return Ok(None);
        };

        // Check if this is DMABuf memory
        if !memory.is_memory_type::<gstreamer_allocators::DmaBufMemory>() {
            tracing::trace!("Buffer memory is not DMABuf, using CPU copy path");
            return Ok(None);
        }

        // Determine if we have multiple FDs (one per plane) or single FD with offsets
        let n_memory = buffer.n_memory();
        let multi_fd = n_memory >= num_planes && num_planes > 1;

        // Detect single-FD multi-plane layouts (common with VA-API).
        // These require special Vulkan import using VkImageDrmFormatModifierExplicitCreateInfoEXT
        // with a pPlaneLayouts array specifying each plane's offset and stride.
        let is_single_fd = num_planes > 1 && !multi_fd;

        // Extract per-plane metadata
        let mut planes: Vec<DmaBufPlane> = Vec::with_capacity(num_planes);

        // For single-FD layouts, dup the primary FD once and share it across all planes.
        // This avoids leaking N-1 FDs per frame (lumina-video-dvh).
        // The import code only uses primary_fd() for single-FD layouts.
        let mut primary_dup_fd: RawFd = -1;

        for plane_idx in 0..num_planes {
            // Get the memory block for this plane
            // If multi_fd: each plane has its own GstMemory
            // If single_fd: all planes share the first GstMemory, differentiated by offset
            let mem_idx = if multi_fd { plane_idx as u32 } else { 0 };
            let Some(plane_memory) = buffer.memory(mem_idx as usize) else {
                tracing::warn!(
                    "DMABuf plane {} has no memory block (expected at index {})",
                    plane_idx,
                    mem_idx
                );
                return Ok(None);
            };

            // Verify it's DMABuf memory
            if !plane_memory.is_memory_type::<gstreamer_allocators::DmaBufMemory>() {
                tracing::warn!("DMABuf plane {} memory is not DMABuf type", plane_idx);
                return Ok(None);
            }

            // Downcast to DmaBufMemory to access fd() method
            let dmabuf_memory = plane_memory
                .downcast_memory_ref::<gstreamer_allocators::DmaBufMemory>()
                .ok_or_else(|| {
                    VideoError::DecodeFailed("Failed to downcast to DmaBufMemory".to_string())
                })?;

            // Extract the file descriptor
            let gst_fd: RawFd = dmabuf_memory.fd();
            if gst_fd < 0 {
                tracing::warn!("DMABuf plane {} has invalid fd: {}", plane_idx, gst_fd);
                return Ok(None);
            }

            // SAFETY: dup() the FD so Vulkan gets its own copy to take ownership of.
            // This avoids double-close: GStreamer closes its FD when GstMemory drops,
            // Vulkan closes the dup'd FD when vkFreeMemory is called.
            //
            // For single-FD layouts: only dup once (first plane), reuse for others.
            // The import code only uses primary_fd(), so other planes just need
            // offset/stride metadata - their fd field is set to the shared dup'd fd
            // but won't be used directly.
            let fd: RawFd = if is_single_fd {
                if plane_idx == 0 {
                    // First plane: dup and save for reuse
                    // SAFETY: `gst_fd` is a live GStreamer DMABuf descriptor;
                    // `dup` creates the owned descriptor passed to Vulkan.
                    let dup_fd = unsafe { libc::dup(gst_fd) };
                    if dup_fd < 0 {
                        tracing::warn!(
                            "Failed to dup DMABuf fd {} for plane {}: {}",
                            gst_fd,
                            plane_idx,
                            std::io::Error::last_os_error()
                        );
                        return Ok(None);
                    }
                    primary_dup_fd = dup_fd;
                    dup_fd
                } else {
                    // Subsequent planes in single-FD: reuse the already dup'd fd
                    // This fd value is stored but not used directly - only offset/stride matter
                    primary_dup_fd
                }
            } else {
                // Multi-FD: each plane gets its own dup'd fd
                // SAFETY: `gst_fd` is a live GStreamer DMABuf descriptor;
                // `dup` creates the owned descriptor passed to Vulkan.
                let dup_fd = unsafe { libc::dup(gst_fd) };
                if dup_fd < 0 {
                    tracing::warn!(
                        "Failed to dup DMABuf fd {} for plane {}: {}",
                        gst_fd,
                        plane_idx,
                        std::io::Error::last_os_error()
                    );
                    // Close any already-dup'd fds before returning
                    for plane in &planes {
                        // SAFETY: Each plane fd was duplicated and remains
                        // owned by this error path.
                        unsafe { libc::close(plane.fd) };
                    }
                    return Ok(None);
                }
                dup_fd
            };

            // Get stride and offset from VideoInfo (use .get() to avoid panic on malformed caps)
            // Helper to close FDs on error - for single-FD we only have one unique FD to close
            let close_fds_on_error =
                |planes: &[DmaBufPlane], current_fd: RawFd, is_single: bool| {
                    if is_single {
                        // Single-FD: all planes share the same fd, close once
                        if current_fd >= 0 {
                            // SAFETY: `current_fd` is the duplicated descriptor
                            // owned by this cleanup path.
                            unsafe { libc::close(current_fd) };
                        }
                    } else {
                        // Multi-FD: close all unique plane fds plus current
                        for plane in planes {
                            // SAFETY: Each plane fd is a duplicated descriptor
                            // still owned by this cleanup path.
                            unsafe { libc::close(plane.fd) };
                        }
                        if current_fd >= 0 {
                            // SAFETY: `current_fd` is the duplicated descriptor
                            // owned by this cleanup path.
                            unsafe { libc::close(current_fd) };
                        }
                    }
                };

            let Some(&stride_i32) = video_info.stride().get(plane_idx) else {
                tracing::warn!(
                    "DMABuf plane {} missing stride entry in VideoInfo",
                    plane_idx
                );
                close_fds_on_error(&planes, fd, is_single_fd);
                return Ok(None);
            };
            let Some(&offset_usize) = video_info.offset().get(plane_idx) else {
                tracing::warn!(
                    "DMABuf plane {} missing offset entry in VideoInfo",
                    plane_idx
                );
                close_fds_on_error(&planes, fd, is_single_fd);
                return Ok(None);
            };
            if stride_i32 < 0 {
                tracing::warn!(
                    "DMABuf plane {} has negative stride {}",
                    plane_idx,
                    stride_i32
                );
                close_fds_on_error(&planes, fd, is_single_fd);
                return Ok(None);
            }
            let stride = stride_i32 as u32;
            let offset = offset_usize as u64;

            // Calculate plane size (approximate - may not account for padding)
            let plane_size = plane_memory.size() as u64;

            planes.push(DmaBufPlane {
                fd,
                offset,
                stride,
                size: plane_size,
            });

            tracing::debug!(
                "Extracted DMABuf plane {}: fd={}, offset={}, stride={}, size={}",
                plane_idx,
                fd,
                offset,
                stride,
                plane_size
            );
        }

        // Get DRM format modifier from caps (GStreamer 1.24+ with va plugin)
        // The va plugin exposes the actual modifier in drm-format caps field.
        // Example: "NV12:0x0100000000000002" = Intel X-tile
        // Fall back to LINEAR (0) if not available (older GStreamer or vaapi plugin)
        let modifier: u64 = Self::parse_drm_modifier_from_caps(&sample).unwrap_or_else(|| {
            tracing::debug!(
                "No DRM modifier in caps (GStreamer < 1.24 or legacy vaapi plugin), assuming LINEAR"
            );
            0 // DRM_FORMAT_MOD_LINEAR
        });

        tracing::debug!(
            "Extracted DMABuf with {} planes: {}x{} {:?}, modifier=0x{:x}, multi_fd={}",
            planes.len(),
            width,
            height,
            pixel_format,
            modifier,
            multi_fd
        );

        // Extract CPU fallback data in case zero-copy import fails at render time.
        // This ensures graceful degradation rather than dropping frames.
        let cpu_fallback = self.extract_cpu_fallback(buffer, video_info, width, height);

        // Create the LinuxGpuSurface
        // The sample is kept alive by wrapping it in an Arc.
        // The FDs passed here are dup'd copies - Vulkan takes ownership and will close them.
        let sample_owner: Arc<dyn std::any::Any + Send + Sync> = Arc::new(sample);

        if is_single_fd {
            tracing::debug!(
                "Single-FD multi-plane DMABuf detected: {} planes share fd={}",
                planes.len(),
                planes.first().map(|p| p.fd).unwrap_or(-1)
            );
        }

        // SAFETY: We've verified all fds are valid (they're dup'd copies we own)
        let surface = unsafe {
            LinuxGpuSurface::new(
                planes,
                width,
                height,
                pixel_format,
                modifier,
                is_single_fd,
                cpu_fallback,
                sample_owner,
            )
        };

        Ok(Some(VideoFrame::new(pts, DecodedFrame::Linux(surface))))
    }

    /// Converts a GStreamer buffer to a CPU frame (fallback path).
    fn sample_to_cpu_frame(
        &self,
        buffer: &gst::BufferRef,
        video_info: &gst_video::VideoInfo,
        pts: Duration,
        width: u32,
        height: u32,
    ) -> Result<VideoFrame, VideoError> {
        // Map the buffer for reading
        let map = buffer
            .map_readable()
            .map_err(|e| VideoError::DecodeFailed(format!("Failed to map buffer: {e}")))?;

        let data = map.as_slice();
        let format = video_info.format();

        // Determine pixel format and extract planes accordingly
        let (pixel_format, planes) =
            match format {
                gst_video::VideoFormat::Nv12 => {
                    // NV12: Y plane followed by interleaved UV plane (2 planes)
                    let strides = video_info.stride();
                    let offsets = video_info.offset();
                    let y_stride = *strides.first().ok_or_else(|| {
                        VideoError::DecodeFailed("NV12: missing Y stride".to_string())
                    })? as usize;
                    let uv_stride = *strides.get(1).ok_or_else(|| {
                        VideoError::DecodeFailed("NV12: missing UV stride".to_string())
                    })? as usize;
                    let y_offset = *offsets.first().ok_or_else(|| {
                        VideoError::DecodeFailed("NV12: missing Y offset".to_string())
                    })?;
                    let uv_offset = *offsets.get(1).ok_or_else(|| {
                        VideoError::DecodeFailed("NV12: missing UV offset".to_string())
                    })?;

                    let y_size = y_stride * height as usize;
                    let uv_size = uv_stride * (height as usize).div_ceil(2);

                    // Extract Y plane
                    let y_data = if y_offset + y_size <= data.len() {
                        data[y_offset..y_offset + y_size].to_vec()
                    } else {
                        return Err(VideoError::DecodeFailed(
                            "Y plane out of bounds".to_string(),
                        ));
                    };

                    // Extract UV plane
                    let uv_data = if uv_offset + uv_size <= data.len() {
                        data[uv_offset..uv_offset + uv_size].to_vec()
                    } else {
                        return Err(VideoError::DecodeFailed(
                            "UV plane out of bounds".to_string(),
                        ));
                    };

                    let y_plane = Plane {
                        data: y_data,
                        stride: y_stride,
                    };

                    let uv_plane = Plane {
                        data: uv_data,
                        stride: uv_stride,
                    };

                    (PixelFormat::Nv12, vec![y_plane, uv_plane])
                }
                gst_video::VideoFormat::I420 => {
                    // I420/YUV420p: Y, U, V as separate planes (3 planes)
                    let strides = video_info.stride();
                    let offsets = video_info.offset();
                    let y_stride = *strides.first().ok_or_else(|| {
                        VideoError::DecodeFailed("I420: missing Y stride".to_string())
                    })? as usize;
                    let u_stride = *strides.get(1).ok_or_else(|| {
                        VideoError::DecodeFailed("I420: missing U stride".to_string())
                    })? as usize;
                    let v_stride = *strides.get(2).ok_or_else(|| {
                        VideoError::DecodeFailed("I420: missing V stride".to_string())
                    })? as usize;
                    let y_offset = *offsets.first().ok_or_else(|| {
                        VideoError::DecodeFailed("I420: missing Y offset".to_string())
                    })?;
                    let u_offset = *offsets.get(1).ok_or_else(|| {
                        VideoError::DecodeFailed("I420: missing U offset".to_string())
                    })?;
                    let v_offset = *offsets.get(2).ok_or_else(|| {
                        VideoError::DecodeFailed("I420: missing V offset".to_string())
                    })?;

                    let y_size = y_stride * height as usize;
                    // U and V planes are quarter size (half width, half height)
                    let uv_height = (height as usize).div_ceil(2);
                    let u_size = u_stride * uv_height;
                    let v_size = v_stride * uv_height;

                    // Extract Y plane
                    let y_data = if y_offset + y_size <= data.len() {
                        data[y_offset..y_offset + y_size].to_vec()
                    } else {
                        return Err(VideoError::DecodeFailed(
                            "Y plane out of bounds".to_string(),
                        ));
                    };

                    // Extract U plane
                    let u_data = if u_offset + u_size <= data.len() {
                        data[u_offset..u_offset + u_size].to_vec()
                    } else {
                        return Err(VideoError::DecodeFailed(
                            "U plane out of bounds".to_string(),
                        ));
                    };

                    // Extract V plane
                    let v_data = if v_offset + v_size <= data.len() {
                        data[v_offset..v_offset + v_size].to_vec()
                    } else {
                        return Err(VideoError::DecodeFailed(
                            "V plane out of bounds".to_string(),
                        ));
                    };

                    let y_plane = Plane {
                        data: y_data,
                        stride: y_stride,
                    };

                    let u_plane = Plane {
                        data: u_data,
                        stride: u_stride,
                    };

                    let v_plane = Plane {
                        data: v_data,
                        stride: v_stride,
                    };

                    (PixelFormat::Yuv420p, vec![y_plane, u_plane, v_plane])
                }
                _ => {
                    return Err(VideoError::DecodeFailed(format!(
                        "Unsupported pixel format for CPU path: {format:?}"
                    )));
                }
            };

        let cpu_frame = CpuFrame::new(pixel_format, width, height, planes);

        Ok(VideoFrame::new(pts, DecodedFrame::Cpu(cpu_frame)))
    }

    /// Extracts CPU frame data from a GStreamer buffer for zero-copy fallback.
    ///
    /// This is called during DMABuf frame extraction to provide fallback data
    /// in case zero-copy import fails at render time. Returns `None` if extraction
    /// fails (e.g., buffer mapping error), in which case the frame may be dropped.
    ///
    /// Supports both NV12 (2 planes: Y, UV interleaved) and I420/YUV420p (3 planes: Y, U, V).
    fn extract_cpu_fallback(
        &self,
        buffer: &gst::BufferRef,
        video_info: &gst_video::VideoInfo,
        width: u32,
        height: u32,
    ) -> Option<CpuFrame> {
        // Map the buffer for reading
        let map = match buffer.map_readable() {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("Failed to map buffer for CPU fallback: {e}");
                return None;
            }
        };

        let data = map.as_slice();
        let format = video_info.format();

        // Determine pixel format and extract planes accordingly
        match format {
            gst_video::VideoFormat::Nv12 => {
                // NV12: Y plane followed by interleaved UV plane (2 planes)
                let strides = video_info.stride();
                let offsets = video_info.offset();
                let y_stride = (*strides.first()?) as usize;
                let uv_stride = (*strides.get(1)?) as usize;
                let y_offset = *offsets.first()?;
                let uv_offset = *offsets.get(1)?;

                let y_size = y_stride * height as usize;
                let uv_size = uv_stride * (height as usize).div_ceil(2);

                // Extract Y plane
                let y_data = if y_offset + y_size <= data.len() {
                    data[y_offset..y_offset + y_size].to_vec()
                } else {
                    tracing::debug!("Y plane out of bounds for CPU fallback");
                    return None;
                };

                // Extract UV plane
                let uv_data = if uv_offset + uv_size <= data.len() {
                    data[uv_offset..uv_offset + uv_size].to_vec()
                } else {
                    tracing::debug!("UV plane out of bounds for CPU fallback");
                    return None;
                };

                let y_plane = Plane {
                    data: y_data,
                    stride: y_stride,
                };

                let uv_plane = Plane {
                    data: uv_data,
                    stride: uv_stride,
                };

                Some(CpuFrame::new(
                    PixelFormat::Nv12,
                    width,
                    height,
                    vec![y_plane, uv_plane],
                ))
            }
            gst_video::VideoFormat::I420 => {
                // I420/YUV420p: Y, U, V as separate planes (3 planes)
                let strides = video_info.stride();
                let offsets = video_info.offset();
                let y_stride = (*strides.first()?) as usize;
                let u_stride = (*strides.get(1)?) as usize;
                let v_stride = (*strides.get(2)?) as usize;
                let y_offset = *offsets.first()?;
                let u_offset = *offsets.get(1)?;
                let v_offset = *offsets.get(2)?;

                let y_size = y_stride * height as usize;
                // U and V planes are quarter size (half width, half height)
                let uv_height = (height as usize).div_ceil(2);
                let u_size = u_stride * uv_height;
                let v_size = v_stride * uv_height;

                // Extract Y plane
                let y_data = if y_offset + y_size <= data.len() {
                    data[y_offset..y_offset + y_size].to_vec()
                } else {
                    tracing::debug!("Y plane out of bounds for CPU fallback");
                    return None;
                };

                // Extract U plane
                let u_data = if u_offset + u_size <= data.len() {
                    data[u_offset..u_offset + u_size].to_vec()
                } else {
                    tracing::debug!("U plane out of bounds for CPU fallback");
                    return None;
                };

                // Extract V plane
                let v_data = if v_offset + v_size <= data.len() {
                    data[v_offset..v_offset + v_size].to_vec()
                } else {
                    tracing::debug!("V plane out of bounds for CPU fallback");
                    return None;
                };

                let y_plane = Plane {
                    data: y_data,
                    stride: y_stride,
                };

                let u_plane = Plane {
                    data: u_data,
                    stride: u_stride,
                };

                let v_plane = Plane {
                    data: v_data,
                    stride: v_stride,
                };

                Some(CpuFrame::new(
                    PixelFormat::Yuv420p,
                    width,
                    height,
                    vec![y_plane, u_plane, v_plane],
                ))
            }
            _ => {
                tracing::debug!("Unsupported pixel format for CPU fallback: {:?}", format);
                None
            }
        }
    }

    fn deadline_for(&self) -> Instant {
        let now = Instant::now();
        now.checked_add(self.lifecycle_timeout).unwrap_or(now)
    }

    fn remaining(deadline: Instant) -> Duration {
        deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO)
    }

    fn clock_time(timeout: Duration) -> gst::ClockTime {
        let nanos = timeout.as_nanos().min(u64::MAX as u128) as u64;
        gst::ClockTime::from_nseconds(nanos)
    }

    fn send_audio_selection(&self, audio_id: Option<&str>) -> bool {
        let mut stream_ids = if self.video_stream_ids.is_empty() {
            self.selected_video_stream_ids.clone()
        } else {
            self.video_stream_ids.clone()
        };
        if let Some(audio_id) = audio_id {
            stream_ids.push(audio_id.to_string());
        }
        self.pipeline.send_event(gst::event::SelectStreams::new(
            stream_ids.iter().map(String::as_str),
        ))
    }

    fn wait_for_audio_selection(
        &mut self,
        requested_id: Option<&str>,
        deadline: Instant,
    ) -> Result<(), String> {
        let Some(bus) = self.pipeline.bus() else {
            return Err("pipeline has no bus".into());
        };
        loop {
            let remaining = Self::remaining(deadline);
            if remaining.is_zero() {
                return Err("audio stream selection timed out".into());
            }
            let Some(message) = bus.timed_pop_filtered(
                Self::clock_time(remaining.min(LIFECYCLE_POLL)),
                &[
                    gst::MessageType::StreamCollection,
                    gst::MessageType::StreamsSelected,
                    gst::MessageType::Error,
                ],
            ) else {
                continue;
            };
            match message.view() {
                gst::MessageView::StreamCollection(collection) => {
                    self.capture_stream_collection(&collection.stream_collection());
                }
                gst::MessageView::StreamsSelected(selected) => {
                    self.capture_selected_streams(selected);
                    let selected_id = self.selected_audio_stream_id.as_deref();
                    let confirmed = match requested_id {
                        Some(requested_id) => selected_id == Some(requested_id),
                        None => selected_id.is_none(),
                    };
                    if confirmed {
                        return Ok(());
                    }
                    return Err(format!(
                        "GStreamer selected {:?} instead of {:?}",
                        selected_id, requested_id
                    ));
                }
                gst::MessageView::Error(error) => {
                    return Err(format!(
                        "pipeline rejected audio stream selection: {} ({:?})",
                        error.error(),
                        error.debug()
                    ));
                }
                _ => {}
            }
        }
    }

    /// Selects one audio stream without rebuilding the pipeline or player.
    ///
    /// The selection confirmation and best-effort rollback use one absolute
    /// operation deadline. A failed rollback deliberately reports that the
    /// current selection is unknown rather than guessing.
    pub fn select_audio_track(&mut self, requested_id: &str) -> AudioTrackSelectionResult {
        let requested_id = requested_id.to_string();
        let deadline = self.begin_operation_deadline();
        let prior_id = self.selected_audio_stream_id.clone();
        let requested_exists = self
            .audio_tracks
            .iter()
            .any(|track| track.id == requested_id);

        if !requested_exists {
            self.active_operation_deadline = None;
            return AudioTrackSelectionResult::Failed {
                requested_id,
                prior_restored_id: prior_id,
                reason: "requested audio stream id is not in the latest StreamCollection; prior audio selection unchanged".into(),
            };
        }

        let selection_sent = self.send_audio_selection(Some(requested_id.as_str()));
        let primary = if selection_sent {
            self.wait_for_audio_selection(Some(requested_id.as_str()), deadline)
        } else {
            Err("pipeline rejected SELECT_STREAMS".into())
        };

        if primary.is_ok() {
            if let Some(track) = self
                .audio_tracks
                .iter()
                .find(|track| track.id == requested_id)
                .cloned()
            {
                self.active_operation_deadline = None;
                return AudioTrackSelectionResult::Selected(track);
            }
        }

        let primary_reason = primary
            .err()
            .unwrap_or_else(|| "audio stream selection failed".into());
        let rollback_confirmed = if !Self::remaining(deadline).is_zero()
            && self.send_audio_selection(prior_id.as_deref())
        {
            self.wait_for_audio_selection(prior_id.as_deref(), deadline)
                .is_ok()
        } else {
            false
        };
        self.active_operation_deadline = None;

        if rollback_confirmed {
            AudioTrackSelectionResult::Failed {
                requested_id,
                prior_restored_id: prior_id,
                reason: format!("{primary_reason}; prior audio selection restored"),
            }
        } else {
            AudioTrackSelectionResult::Failed {
                requested_id,
                prior_restored_id: None,
                reason: format!(
                    "{primary_reason}; rollback failed or timed out; current selection unknown"
                ),
            }
        }
    }

    fn cleanup_with_deadline(&mut self, deadline: Instant) {
        if self.cleaned_up {
            return;
        }
        self.cleaned_up = true;
        Self::cleanup_pipeline(&self.pipeline, deadline);
    }

    /// Internal seek implementation (may be retried on transient errors).
    fn seek_internal(&mut self, position: Duration, deadline: Instant) -> Result<(), VideoError> {
        if self.lifecycle_control.is_cancelled() || Self::remaining(deadline).is_zero() {
            return Err(VideoError::SeekFailed("Seek timed out".into()));
        }
        let position_ns = position.as_nanos() as u64;

        // Mark that we're seeking - decode_next will skip bus polling
        self.seeking = true;
        self.seek_target = Some(position);
        // Record seek direction BEFORE updating position (for stale frame detection)
        self.last_seek_backward = position < self.position;

        // Choose seek flags based on direction:
        // - Forward: KEY_UNIT for fast keyframe-based seeking
        // - Backward: ACCURATE for reliable frame-accurate seeking
        //   (KEY_UNIT + SNAP_BEFORE caused video freeze, see notedeck-vid-w4r)
        let flags = if self.last_seek_backward {
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE
        } else {
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT
        };

        if let Err(e) = self
            .pipeline
            .seek_simple(flags, gst::ClockTime::from_nseconds(position_ns))
        {
            // Clear seeking state on error to avoid getting stuck
            self.seeking = false;
            self.seek_target = None;
            return Err(VideoError::SeekFailed(format!("Seek failed: {e:?}")));
        }

        // Wait for seek completion using short filtered polls. This prevents
        // a state wait from hiding cancellation for the whole operation.
        if let Some(bus) = self.pipeline.bus() {
            loop {
                if self.lifecycle_control.is_cancelled() {
                    self.seeking = false;
                    self.seek_target = None;
                    return Err(VideoError::SeekFailed("Seek cancelled".into()));
                }
                let timeout = Self::remaining(deadline);
                if timeout.is_zero() {
                    self.seeking = false;
                    self.seek_target = None;
                    return Err(VideoError::SeekFailed("Seek completion timed out".into()));
                }
                let msg = bus.timed_pop_filtered(
                    Self::clock_time(timeout.min(LIFECYCLE_POLL)),
                    &[gst::MessageType::AsyncDone, gst::MessageType::Error],
                );
                let Some(msg) = msg else {
                    continue;
                };
                match msg.view() {
                    gst::MessageView::AsyncDone(_) => {
                        let direction = if position < self.position {
                            "backward"
                        } else {
                            "forward"
                        };
                        tracing::debug!(
                            "Seek {} completed: {:?} -> {:?}",
                            direction,
                            self.position,
                            position
                        );
                        break;
                    }
                    gst::MessageView::Error(err) => {
                        self.seeking = false;
                        self.seek_target = None;
                        return Err(VideoError::SeekFailed(format!(
                            "Seek error: {} ({:?})",
                            err.error(),
                            err.debug()
                        )));
                    }
                    _ => {}
                }
            }
        }

        self.position = position;
        self.eof = false;
        // Assume rebuffering will be needed after seek (HTTP streams)
        self.buffering_percent = 0;
        // Reset so we don't pause during post-seek buffering
        self.was_fully_buffered = false;

        Ok(())
    }

    /// Pulls and stores the first post-seek frame before the seek deadline is
    /// released. Paused seeks use preroll directly, so a later Play cannot
    /// discover that resync expired while the decoder was idle.
    fn pull_seek_sample(&mut self, deadline: Instant) -> Result<(), VideoError> {
        let restore_paused = self.user_paused;
        if restore_paused {
            // A FLUSH seek while already paused does not reliably enqueue an
            // appsink sample on every GStreamer source. Let the pipeline
            // produce one, then restore the user's paused state below.
            let _ = self.pipeline.set_state(gst::State::Playing);
            let remaining = Self::remaining(deadline);
            if !remaining.is_zero() {
                let _ = self
                    .pipeline
                    .state(Self::clock_time(remaining.min(LIFECYCLE_POLL)));
            }
        }

        let mut discarded = 0_u32;
        while discarded <= 5 {
            if self.lifecycle_control.is_cancelled() {
                self.seeking = false;
                self.seek_target = None;
                self.restore_paused_after_seek(deadline);
                return Err(VideoError::SeekFailed("Seek cancelled".into()));
            }
            let remaining = Self::remaining(deadline);
            if remaining.is_zero() {
                self.seeking = false;
                self.seek_target = None;
                self.restore_paused_after_seek(deadline);
                return Err(VideoError::SeekFailed("Seek preroll timed out".into()));
            }
            let timeout = Self::clock_time(remaining.min(LIFECYCLE_POLL));
            let sample = self.appsink.try_pull_sample(timeout);
            let sample = if sample.is_some() {
                sample
            } else {
                let remaining = Self::remaining(deadline);
                if remaining.is_zero() {
                    None
                } else {
                    self.appsink
                        .try_pull_preroll(Self::clock_time(remaining.min(LIFECYCLE_POLL)))
                }
            };
            let Some(sample) = sample else {
                continue;
            };
            let frame = match self.sample_to_frame(sample.clone()) {
                Ok(frame) => frame,
                Err(error) => {
                    self.restore_paused_after_seek(deadline);
                    return Err(error);
                }
            };
            if self.is_stale_frame(frame.pts, discarded, 5) {
                discarded = discarded.saturating_add(1);
                continue;
            }
            self.preroll_sample = Some(sample);
            self.seek_deadline = None;
            self.active_operation_deadline = None;
            self.restore_paused_after_seek(deadline);
            return Ok(());
        }

        self.seeking = false;
        self.seek_target = None;
        self.restore_paused_after_seek(deadline);
        Err(VideoError::SeekFailed(
            "seek produced only stale frames".into(),
        ))
    }

    fn restore_paused_after_seek(&self, deadline: Instant) {
        if !self.user_paused {
            return;
        }
        let _ = self.pipeline.set_state(gst::State::Paused);
        let remaining = Self::remaining(deadline);
        if !remaining.is_zero() {
            let _ = self
                .pipeline
                .state(Self::clock_time(remaining.min(LIFECYCLE_POLL)));
        }
    }

    /// Processes a bus message during decode_next.
    /// Returns Some(result) if decode_next should return early, None to continue.
    fn process_bus_message(
        &mut self,
        msg: &gst::Message,
    ) -> Option<Result<Option<VideoFrame>, VideoError>> {
        match msg.view() {
            gst::MessageView::Error(err) => {
                let error = VideoError::DecodeFailed(format!("Pipeline error: {}", err.error()));
                if self.seeking {
                    // Queue error to return on next decode_next() call
                    // Don't silently drop real pipeline failures during seek
                    self.pending_error = Some(error);
                    return None;
                }
                return Some(Err(error));
            }
            gst::MessageView::Eos(_) if !self.seeking => {
                self.eof = true;
                return Some(Ok(None));
            }
            gst::MessageView::Buffering(buffering) => {
                self.handle_buffering_message(buffering.percent());
            }
            gst::MessageView::StreamCollection(collection) => {
                self.capture_stream_collection(&collection.stream_collection());
            }
            gst::MessageView::StreamsSelected(selected) => {
                self.capture_selected_streams(selected);
            }
            _ => {}
        }
        None
    }

    /// Handles buffering percentage changes with hysteresis.
    fn handle_buffering_message(&mut self, percent: i32) {
        if percent == self.buffering_percent {
            return;
        }

        tracing::debug!("Buffering: {}%", percent);
        self.buffering_percent = percent;

        // Resume when buffer is full, but only if user hasn't explicitly paused
        if percent >= BUFFER_HIGH_THRESHOLD {
            self.was_fully_buffered = true;
            if !self.user_paused {
                let _ = self.pipeline.set_state(gst::State::Playing);
            }
            return;
        }

        // Pause only on rebuffer (after we've been at 100% once) when critically low
        if self.was_fully_buffered && percent < BUFFER_LOW_THRESHOLD {
            tracing::info!("Buffer critically low ({}%), pausing to refill", percent);
            let _ = self.pipeline.set_state(gst::State::Paused);
        }
    }

    /// Checks if a frame should be discarded as stale during seeking.
    /// Returns true if the frame is stale and should be skipped.
    fn is_stale_frame(&self, frame_pts: Duration, discarded: u32, max_stale: u32) -> bool {
        if !self.seeking || discarded >= max_stale {
            return false;
        }

        let Some(target) = self.seek_target else {
            return false;
        };

        // For backward seeks: discard frames far AFTER the target
        let too_far_after = frame_pts > target + Duration::from_secs(2);

        // For forward seeks: discard frames BEFORE the target
        let too_far_before =
            !self.last_seek_backward && frame_pts + Duration::from_millis(100) < target;

        if too_far_after || too_far_before {
            tracing::debug!(
                "Discarding stale frame at {:?} (seek target {:?}, {})",
                frame_pts,
                target,
                if too_far_before { "before" } else { "after" }
            );
            return true;
        }

        false
    }

    /// Handles the None case when pulling a sample from appsink.
    fn handle_no_sample(&mut self) {
        if self.seeking {
            tracing::debug!(
                "No frame after seek: eos={}, position={:?}",
                self.appsink.is_eos(),
                self.position
            );
        }

        if self.appsink.is_eos() {
            self.eof = true;
            self.seeking = false;
            self.seek_target = None;
            self.seek_deadline = None;
        }
    }
}

impl Drop for GStreamerDecoder {
    fn drop(&mut self) {
        // Drop may run after the worker has already lost its session. State
        // waits belong to explicit worker cleanup; decoder Drop is strictly
        // fire-and-forget so it cannot extend UI/session teardown.
        if !self.cleaned_up {
            let _ = self.pipeline.set_state(gst::State::Null);
            self.cleaned_up = true;
        }
    }
}

// Safety: GStreamerDecoder can be sent between threads because:
// - gst::Pipeline, gst::Element, gst_app::AppSink, and gst::Sample all implement Send
//   in gstreamer-rs (GStreamer objects are reference-counted and thread-safe)
// - All other fields (Duration, bool, i32, etc.) are Send
// - GstAudioHandle uses Arc for thread-safe sharing
// The compiler should derive Send automatically, but we verify it with a static assert:
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<gst::Pipeline>();
    assert_send::<gst_app::AppSink>();
    assert_send::<gst::Sample>();
    assert_send::<GstAudioHandle>();
};

impl VideoDecoderBackend for GStreamerDecoder {
    fn open(url: &str) -> Result<Self, VideoError>
    where
        Self: Sized,
    {
        Self::new(url)
    }

    fn decode_next(&mut self) -> Result<Option<VideoFrame>, VideoError> {
        if self.lifecycle_control.is_cancelled() {
            return Err(VideoError::Generic("lifecycle cancelled".into()));
        }

        // Return any queued error from seek (errors during seek are queued, not dropped)
        if let Some(error) = self.pending_error.take() {
            // Clear seek state so the decoder doesn't use stale flags on next call
            self.seeking = false;
            self.seek_target = None;
            self.seek_deadline = None;
            return Err(error);
        }

        if self.eof {
            return Ok(None);
        }

        // Return cached preroll sample on first call (consumed during init for dimensions)
        if let Some(sample) = self.preroll_sample.take() {
            let frame = self.sample_to_frame(sample)?;
            tracing::debug!("Returning cached preroll frame at {:?}", frame.pts);
            self.position = frame.pts;
            self.seeking = false;
            self.seek_target = None;
            self.seek_deadline = None;
            self.active_operation_deadline = None;
            return Ok(Some(frame));
        }

        // Poll bus for messages - errors during seek are queued (not dropped) and returned above
        // EOS during seek is skipped; seek() handles AsyncDone
        if let Some(bus) = self.pipeline.bus() {
            while let Some(msg) = bus.pop() {
                if let Some(result) = self.process_bus_message(&msg) {
                    return result;
                }
            }
        }

        // Use longer timeout when buffering or after seek
        let timeout_ms = if self.seeking || self.buffering_percent < 100 {
            1000
        } else {
            100
        };

        // When seeking, we may need to discard stale frames
        let max_stale_frames: u32 = if self.seeking { 5 } else { 0 };
        let mut discarded: u32 = 0;

        loop {
            if self.lifecycle_control.is_cancelled() {
                return Err(VideoError::Generic("lifecycle cancelled".into()));
            }
            let timeout = if let Some(deadline) = self.seek_deadline {
                let remaining = Self::remaining(deadline);
                if remaining.is_zero() {
                    self.seeking = false;
                    self.seek_target = None;
                    self.seek_deadline = None;
                    return Err(VideoError::SeekFailed("Seek timed out".into()));
                }
                remaining
                    .min(Duration::from_millis(timeout_ms as u64))
                    .min(LIFECYCLE_POLL)
            } else {
                Duration::from_millis(timeout_ms as u64).min(LIFECYCLE_POLL)
            };
            if timeout.is_zero() {
                return Err(VideoError::Generic("lifecycle cancelled".into()));
            }
            let Some(sample) = self.appsink.try_pull_sample(Self::clock_time(timeout)) else {
                if self
                    .seek_deadline
                    .is_some_and(|deadline| Self::remaining(deadline).is_zero())
                {
                    self.seeking = false;
                    self.seek_target = None;
                    self.seek_deadline = None;
                    return Err(VideoError::SeekFailed("Seek timed out".into()));
                }
                self.handle_no_sample();
                return Ok(None);
            };

            let frame = self.sample_to_frame(sample)?;

            // Check for stale frames after seek
            if self.is_stale_frame(frame.pts, discarded, max_stale_frames) {
                discarded += 1;
                continue;
            }

            if self.seeking {
                tracing::debug!(
                    "First frame after seek at {:?} (expected ~{:?})",
                    frame.pts,
                    self.position
                );
            }

            self.position = frame.pts;
            self.seeking = false;
            self.seek_target = None;
            self.seek_deadline = None;
            self.active_operation_deadline = None;
            return Ok(Some(frame));
        }
    }

    fn seek(&mut self, position: Duration) -> Result<(), VideoError> {
        // Retry seek up to 3 times for transient HTTP errors
        const MAX_RETRIES: u32 = 3;
        let deadline = self.deadline_for();
        self.seek_deadline = Some(deadline);
        self.active_operation_deadline = Some(deadline);
        let mut last_error = None;

        for attempt in 0..=MAX_RETRIES {
            if Self::remaining(deadline).is_zero() {
                break;
            }
            match self.seek_internal(position, deadline) {
                Ok(()) => match self.pull_seek_sample(deadline) {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        last_error = Some(error);
                        break;
                    }
                },
                Err(e) => {
                    if attempt < MAX_RETRIES && !Self::remaining(deadline).is_zero() {
                        tracing::warn!("Seek attempt {} failed, retrying: {}", attempt + 1, e);
                        // Capture user pause state before toggling pipeline states
                        let was_paused = self.user_paused;
                        // Reset pipeline state before retry - helps recover from HTTP errors
                        let _ = self.pipeline.set_state(gst::State::Paused);
                        let paused_wait = Self::remaining(deadline).min(LIFECYCLE_POLL);
                        if paused_wait.is_zero() {
                            last_error = Some(e);
                            break;
                        }
                        let _ = self.pipeline.state(Self::clock_time(paused_wait));
                        let _ = self.pipeline.set_state(gst::State::Playing);
                        let playing_wait = Self::remaining(deadline).min(LIFECYCLE_POLL);
                        if playing_wait.is_zero() {
                            last_error = Some(e);
                            break;
                        }
                        let _ = self.pipeline.state(Self::clock_time(playing_wait));
                        // Restore paused state if user had paused before seek
                        if was_paused {
                            let _ = self.pipeline.set_state(gst::State::Paused);
                            let paused_wait = Self::remaining(deadline).min(LIFECYCLE_POLL);
                            if !paused_wait.is_zero() {
                                let _ = self.pipeline.state(Self::clock_time(paused_wait));
                            }
                        }
                        // Longer delay for HTTP reconnection
                        let delay = Self::remaining(deadline).min(LIFECYCLE_POLL);
                        if !delay.is_zero() && !self.lifecycle_control.is_cancelled() {
                            std::thread::sleep(delay);
                        }
                    }
                    last_error = Some(e);
                }
            }
        }

        self.seek_deadline = None;
        Err(last_error.unwrap_or_else(|| VideoError::SeekFailed("Seek timed out".into())))
    }

    fn metadata(&self) -> &VideoMetadata {
        &self.metadata
    }

    fn pause(&mut self) -> Result<(), VideoError> {
        self.user_paused = true;
        self.pipeline
            .set_state(gst::State::Paused)
            .map_err(|e| VideoError::Generic(format!("Pause failed: {e:?}")))?;
        Ok(())
    }

    fn resume(&mut self) -> Result<(), VideoError> {
        self.user_paused = false;
        tracing::debug!("GStreamer: resuming pipeline to Playing state");
        self.pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| VideoError::Generic(format!("Resume failed: {e:?}")))?;
        Ok(())
    }

    fn set_muted(&mut self, muted: bool) -> Result<(), VideoError> {
        self.audio_handle.set_muted(muted);
        Ok(())
    }

    fn set_volume(&mut self, volume: f32) -> Result<(), VideoError> {
        // Convert 0.0-1.0 to 0-100
        self.audio_handle.set_volume((volume * 100.0) as u32);
        Ok(())
    }

    fn is_eof(&self) -> bool {
        self.eof
    }

    fn buffering_percent(&self) -> i32 {
        self.buffering_percent
    }

    /// GStreamer handles audio internally - no separate FFmpeg audio thread needed.
    fn handles_audio_internally(&self) -> bool {
        true
    }

    fn hw_accel_type(&self) -> HwAccelType {
        // GStreamer handles HW accel internally via uridecodebin3 auto-selection.
        // We can't know at runtime which decoder (VA-API, software, etc.) is in use.
        HwAccelType::None
    }
}

#[cfg(test)]
mod tests {
    use super::{GStreamerDecoder, GstLifecycleControl};
    use std::time::{Duration, Instant};

    #[test]
    fn lifecycle_cancellation_shares_one_absolute_deadline() {
        let control = GstLifecycleControl::new();
        let worker_control = control.clone();
        let before = Instant::now();

        control.cancel(Duration::from_millis(100));

        assert!(worker_control.is_cancelled());
        let Some(deadline) = worker_control.deadline() else {
            panic!("cancellation must record a cleanup deadline");
        };
        assert!(deadline >= before);
        assert!(deadline <= before + Duration::from_secs(1));
    }

    #[test]
    fn cleanup_deadline_uses_the_earlier_lifecycle_or_operation_deadline() {
        let now = Instant::now();
        let lifecycle = now + Duration::from_millis(10);
        let operation = now + Duration::from_millis(100);
        assert_eq!(
            GStreamerDecoder::earliest_deadline(Some(lifecycle), Some(operation)),
            Some(lifecycle)
        );
        assert_eq!(
            GStreamerDecoder::earliest_deadline(Some(operation), Some(lifecycle)),
            Some(lifecycle)
        );
    }

    #[test]
    fn stream_collection_metadata_uses_raw_ids_and_audio_tags() {
        if gst::init().is_err() {
            return;
        }
        let mut tags = gst::TagList::new();
        let Some(tags_ref) = tags.get_mut() else {
            return;
        };
        tags_ref.add::<gst::tags::LanguageCode>(&"eng", gst::TagMergeMode::Append);
        tags_ref.add::<gst::tags::Title>(&"English", gst::TagMergeMode::Append);
        tags_ref.add::<gst::tags::AudioCodec>(&"AAC", gst::TagMergeMode::Append);

        let video = gst::Stream::new(
            Some("video-raw-id"),
            None,
            gst::StreamType::VIDEO,
            gst::StreamFlags::empty(),
        );
        let audio = gst::Stream::new(
            Some("audio-raw-id"),
            None,
            gst::StreamType::AUDIO,
            gst::StreamFlags::empty(),
        );
        audio.set_tags(Some(&tags));
        let collection = gst::StreamCollection::builder(None)
            .streams([video, audio])
            .build();

        let (tracks, video_ids) = GStreamerDecoder::collection_metadata(&collection);
        assert_eq!(video_ids, ["video-raw-id"]);
        assert_eq!(tracks.len(), 1);
        assert_eq!(
            tracks.first().map(|track| track.id.as_str()),
            Some("audio-raw-id")
        );
        assert_eq!(
            tracks.first().and_then(|track| track.language.as_deref()),
            Some("eng")
        );
        assert_eq!(
            tracks.first().and_then(|track| track.title.as_deref()),
            Some("English")
        );
        assert_eq!(
            tracks.first().map(|track| track.codec.as_str()),
            Some("AAC")
        );
    }
}
