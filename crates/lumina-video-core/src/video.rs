//! Framework-neutral video state and metadata semantics.
//!
//! Native frame storage, decoder backends, and GPU surfaces live in
//! `lumina-video-native-frame`.  This module contains only values that are
//! meaningful to a media session independent of its platform implementation.

use std::sync::Arc;
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use parking_lot::Mutex;
#[cfg(target_arch = "wasm32")]
use std::sync::Mutex;

/// Represents the current state of video playback.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoState {
    Loading,
    Ready,
    Playing { position: Duration },
    Paused { position: Duration },
    Buffering { position: Duration },
    Error(VideoError),
    Ended,
}

impl VideoState {
    pub fn position(&self) -> Option<Duration> {
        match self {
            Self::Playing { position }
            | Self::Paused { position }
            | Self::Buffering { position } => Some(*position),
            Self::Loading | Self::Ready | Self::Error(_) | Self::Ended => None,
        }
    }

    pub fn is_playing(&self) -> bool {
        matches!(self, Self::Playing { .. })
    }

    pub fn can_play(&self) -> bool {
        matches!(self, Self::Ready | Self::Paused { .. } | Self::Ended)
    }
}

/// Errors that can occur during video playback.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoError {
    OpenFailed(String),
    DecoderInit(String),
    DecodeFailed(String),
    SeekFailed(String),
    UnsupportedFormat(String),
    Network(String),
    Tls(String),
    Generic(String),
}

impl std::fmt::Display for VideoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenFailed(message) => write!(f, "Failed to open video: {message}"),
            Self::DecoderInit(message) => write!(f, "Decoder initialization failed: {message}"),
            Self::DecodeFailed(message) => write!(f, "Frame decode failed: {message}"),
            Self::SeekFailed(message) => write!(f, "Seek failed: {message}"),
            Self::UnsupportedFormat(message) => write!(f, "Unsupported format: {message}"),
            Self::Network(message) => write!(f, "Network error: {message}"),
            Self::Tls(message) => write!(f, "TLS error: {message}"),
            Self::Generic(message) => write!(f, "Video error: {message}"),
        }
    }
}

impl std::error::Error for VideoError {}

/// Metadata for one video stream.
#[derive(Debug, Clone)]
pub struct VideoMetadata {
    pub width: u32,
    pub height: u32,
    pub duration: Option<Duration>,
    pub frame_rate: f32,
    pub codec: String,
    pub pixel_aspect_ratio: f32,
    pub start_time: Option<Duration>,
}

impl VideoMetadata {
    pub fn aspect_ratio(&self) -> f32 {
        if self.height == 0 {
            return 1.0;
        }
        (self.width as f32 / self.height as f32) * self.pixel_aspect_ratio
    }

    pub fn frame_duration(&self) -> Duration {
        if self.frame_rate <= 0.0 || !self.frame_rate.is_finite() {
            return Duration::from_millis(33);
        }
        Duration::from_secs_f64(1.0 / self.frame_rate as f64)
    }
}

/// Shared state handle for callers that need to observe a player.
#[derive(Clone)]
pub struct VideoPlayerHandle {
    inner: Arc<Mutex<VideoPlayerInner>>,
}

struct VideoPlayerInner {
    state: VideoState,
    metadata: Option<VideoMetadata>,
}

impl VideoPlayerHandle {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VideoPlayerInner {
                state: VideoState::Loading,
                metadata: None,
            })),
        }
    }

    pub fn state(&self) -> VideoState {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.inner.lock().state.clone()
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.inner
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .state
                .clone()
        }
    }

    pub fn set_state(&self, state: VideoState) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.inner.lock().state = state;
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.inner
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .state = state;
        }
    }

    pub fn metadata(&self) -> Option<VideoMetadata> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.inner.lock().metadata.clone()
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.inner
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .metadata
                .clone()
        }
    }

    pub fn set_metadata(&self, metadata: VideoMetadata) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.inner.lock().metadata = Some(metadata);
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.inner
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .metadata = Some(metadata);
        }
    }
}

impl Default for VideoPlayerHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_semantics_remain_stable() {
        let playing = VideoState::Playing {
            position: Duration::from_secs(10),
        };
        assert_eq!(playing.position(), Some(Duration::from_secs(10)));
    }

    #[test]
    fn metadata_aspect_ratio_is_unchanged() {
        let metadata = VideoMetadata {
            width: 1920,
            height: 1080,
            duration: Some(Duration::from_secs(120)),
            frame_rate: 30.0,
            codec: "h264".to_string(),
            pixel_aspect_ratio: 1.0,
            start_time: None,
        };
        assert!((metadata.aspect_ratio() - 1.777).abs() < 0.01);
    }
}
