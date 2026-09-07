//! MoQ track subscription and object receipt handling.
//!
//! This module handles subscribing to MoQ tracks and receiving media objects
//! (groups and frames) from the subscribed tracks.

use super::catalog::{AudioTrackInfo, MoqCatalog, VideoTrackInfo};
use super::error::MoqError;

use bytes::Bytes;
use moq_net::{group::Consumer as GroupConsumer, track::Subscriber as TrackConsumer};

/// State of a track subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionState {
    /// Not subscribed
    Inactive,
    /// Subscription pending
    Subscribing,
    /// Actively receiving data
    Active,
    /// Subscription closed/ended
    Closed,
    /// Subscription failed
    Failed,
}

/// A received media frame from a MoQ track.
#[derive(Debug, Clone)]
pub struct MoqFrame {
    /// Group sequence number
    pub group_sequence: u64,
    /// Frame index within the group
    pub frame_index: usize,
    /// Frame data
    pub data: Bytes,
    /// Timestamp in milliseconds (if available from timing info)
    pub timestamp_ms: Option<u64>,
}

/// Subscriber for a single MoQ track.
pub struct MoqTrackSubscriber {
    /// Track info (video or audio)
    track_info: TrackInfo,
    /// Track consumer for receiving groups
    consumer: Option<TrackConsumer>,
    /// Current group being read
    current_group: Option<GroupConsumer>,
    /// Current group sequence
    current_group_seq: u64,
    /// Current frame index within group
    current_frame_idx: usize,
    /// Subscription state
    state: SubscriptionState,
    /// Total frames received
    frames_received: u64,
    /// Total bytes received
    bytes_received: u64,
}

/// Track info wrapper for either video or audio.
#[derive(Debug, Clone)]
pub enum TrackInfo {
    Video(VideoTrackInfo),
    Audio(AudioTrackInfo),
}

impl TrackInfo {
    pub fn name(&self) -> &str {
        match self {
            TrackInfo::Video(v) => &v.name,
            TrackInfo::Audio(a) => &a.name,
        }
    }

    pub fn is_video(&self) -> bool {
        matches!(self, TrackInfo::Video(_))
    }

    pub fn is_audio(&self) -> bool {
        matches!(self, TrackInfo::Audio(_))
    }
}

impl MoqTrackSubscriber {
    /// Creates a new subscriber for a video track.
    pub fn for_video(info: VideoTrackInfo) -> Self {
        Self {
            track_info: TrackInfo::Video(info),
            consumer: None,
            current_group: None,
            current_group_seq: 0,
            current_frame_idx: 0,
            state: SubscriptionState::Inactive,
            frames_received: 0,
            bytes_received: 0,
        }
    }

    /// Creates a new subscriber for an audio track.
    pub fn for_audio(info: AudioTrackInfo) -> Self {
        Self {
            track_info: TrackInfo::Audio(info),
            consumer: None,
            current_group: None,
            current_group_seq: 0,
            current_frame_idx: 0,
            state: SubscriptionState::Inactive,
            frames_received: 0,
            bytes_received: 0,
        }
    }

    /// Returns the track info.
    pub fn track_info(&self) -> &TrackInfo {
        &self.track_info
    }

    /// Returns the subscription state.
    pub fn state(&self) -> SubscriptionState {
        self.state
    }

    /// Returns true if the subscription is active.
    pub fn is_active(&self) -> bool {
        self.state == SubscriptionState::Active
    }

    /// Returns the number of frames received.
    pub fn frames_received(&self) -> u64 {
        self.frames_received
    }

    /// Returns the number of bytes received.
    pub fn bytes_received(&self) -> u64 {
        self.bytes_received
    }

    /// Sets the track consumer from a MoQ session.
    ///
    /// Call this after obtaining a TrackConsumer from the broadcast.
    pub fn set_consumer(&mut self, consumer: TrackConsumer) {
        self.consumer = Some(consumer);
        self.state = SubscriptionState::Active;
    }

    /// Reads the next frame from the subscribed track.
    ///
    /// Returns `None` if the track has ended or there's no data available yet.
    pub async fn next_frame(&mut self) -> Result<Option<MoqFrame>, MoqError> {
        let consumer = match &mut self.consumer {
            Some(c) => c,
            None => {
                return Err(MoqError::SubscriptionError(
                    "Track not subscribed".to_string(),
                ))
            }
        };

        // If we have a current group, try to read the next frame
        if let Some(ref mut group) = self.current_group {
            match group.read_frame().await {
                Ok(Some(data)) => {
                    let frame = MoqFrame {
                        group_sequence: self.current_group_seq,
                        frame_index: self.current_frame_idx,
                        data: data.payload,
                        timestamp_ms: None, // Would need timing info from the stream
                    };
                    self.current_frame_idx += 1;
                    self.frames_received += 1;
                    self.bytes_received += frame.data.len() as u64;
                    return Ok(Some(frame));
                }
                Ok(None) => {
                    // Group exhausted, need new group
                    self.current_group = None;
                }
                Err(e) => {
                    return Err(MoqError::ObjectError(format!("Failed to read frame: {e}")));
                }
            }
        }

        // Try to get the next group
        match consumer.next_group().await {
            Ok(Some(group)) => {
                self.current_group_seq = group.sequence;
                self.current_frame_idx = 0;
                self.current_group = Some(group);

                // Try to read first frame from new group
                if let Some(ref mut group) = self.current_group {
                    match group.read_frame().await {
                        Ok(Some(data)) => {
                            let frame = MoqFrame {
                                group_sequence: self.current_group_seq,
                                frame_index: self.current_frame_idx,
                                data: data.payload,
                                timestamp_ms: None,
                            };
                            self.current_frame_idx += 1;
                            self.frames_received += 1;
                            self.bytes_received += frame.data.len() as u64;
                            return Ok(Some(frame));
                        }
                        Ok(None) => {
                            self.current_group = None;
                            return Ok(None);
                        }
                        Err(e) => {
                            return Err(MoqError::ObjectError(format!(
                                "Failed to read first frame: {e}"
                            )));
                        }
                    }
                }
                Ok(None)
            }
            Ok(None) => {
                // Track closed
                self.state = SubscriptionState::Closed;
                Ok(None)
            }
            Err(e) => Err(MoqError::ObjectError(format!("Failed to get group: {e}"))),
        }
    }

    /// Marks the subscription as failed.
    pub fn set_failed(&mut self) {
        self.state = SubscriptionState::Failed;
        self.consumer = None;
        self.current_group = None;
    }

    /// Closes the subscription.
    pub fn close(&mut self) {
        self.state = SubscriptionState::Closed;
        self.consumer = None;
        self.current_group = None;
    }
}

/// Manager for multiple track subscriptions.
pub struct MoqSubscriptionManager {
    /// Video track subscriber
    video: Option<MoqTrackSubscriber>,
    /// Audio track subscriber
    audio: Option<MoqTrackSubscriber>,
    /// Catalog (if available)
    catalog: Option<MoqCatalog>,
}

impl MoqSubscriptionManager {
    /// Creates a new subscription manager.
    pub fn new() -> Self {
        Self {
            video: None,
            audio: None,
            catalog: None,
        }
    }

    /// Sets the catalog.
    pub fn set_catalog(&mut self, catalog: MoqCatalog) {
        self.catalog = Some(catalog);
    }

    /// Returns the catalog.
    pub fn catalog(&self) -> Option<&MoqCatalog> {
        self.catalog.as_ref()
    }

    /// Creates subscribers from the catalog's primary tracks.
    pub fn subscribe_primary_tracks(&mut self) -> Result<(), MoqError> {
        let catalog = self
            .catalog
            .as_ref()
            .ok_or_else(|| MoqError::CatalogError("No catalog available".to_string()))?;

        if let Some(video_info) = catalog.primary_video() {
            self.video = Some(MoqTrackSubscriber::for_video(video_info.clone()));
        }

        if let Some(audio_info) = catalog.primary_audio() {
            self.audio = Some(MoqTrackSubscriber::for_audio(audio_info.clone()));
        }

        Ok(())
    }

    /// Returns a mutable reference to the video subscriber.
    pub fn video_subscriber(&mut self) -> Option<&mut MoqTrackSubscriber> {
        self.video.as_mut()
    }

    /// Returns a mutable reference to the audio subscriber.
    pub fn audio_subscriber(&mut self) -> Option<&mut MoqTrackSubscriber> {
        self.audio.as_mut()
    }

    /// Returns true if video is being subscribed.
    pub fn has_video(&self) -> bool {
        self.video.is_some()
    }

    /// Returns true if audio is being subscribed.
    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    /// Returns the video track info.
    pub fn video_info(&self) -> Option<&VideoTrackInfo> {
        self.video.as_ref().and_then(|s| match &s.track_info {
            TrackInfo::Video(v) => Some(v),
            _ => None,
        })
    }

    /// Returns the audio track info.
    pub fn audio_info(&self) -> Option<&AudioTrackInfo> {
        self.audio.as_ref().and_then(|s| match &s.track_info {
            TrackInfo::Audio(a) => Some(a),
            _ => None,
        })
    }
}

impl Default for MoqSubscriptionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moq::catalog::VideoCodec;

    #[test]
    fn test_track_info() {
        let video_info = VideoTrackInfo {
            name: "video".to_string(),
            codec: VideoCodec::H264,
            width: 1920,
            height: 1080,
            frame_rate: 30.0,
            bitrate: None,
            init_data: None,
        };

        let track = TrackInfo::Video(video_info);
        assert_eq!(track.name(), "video");
        assert!(track.is_video());
        assert!(!track.is_audio());
    }

    #[test]
    fn test_subscriber_initial_state() {
        let video_info = VideoTrackInfo {
            name: "video".to_string(),
            codec: VideoCodec::H264,
            width: 1920,
            height: 1080,
            frame_rate: 30.0,
            bitrate: None,
            init_data: None,
        };

        let subscriber = MoqTrackSubscriber::for_video(video_info);
        assert_eq!(subscriber.state(), SubscriptionState::Inactive);
        assert!(!subscriber.is_active());
        assert_eq!(subscriber.frames_received(), 0);
        assert_eq!(subscriber.bytes_received(), 0);
    }

    #[test]
    fn test_subscription_manager() {
        let manager = MoqSubscriptionManager::new();
        assert!(!manager.has_video());
        assert!(!manager.has_audio());
        assert!(manager.catalog().is_none());
    }
}

/// Application-level subscription settings for an encoded media rendition.
#[derive(Clone)]
pub(super) struct MediaTrack {
    pub name: String,
    pub priority: u8,
}

/// A decoded Legacy envelope; the first object in each group is independently decodable.
pub(super) struct LegacyFrame {
    pub timestamp: std::time::Duration,
    pub payload: Bytes,
    pub keyframe: bool,
}

/// Reads every frame in a media group, abandoning stalled groups within the latency budget.
/// The network crate's track.read_frame() only returns one frame per group (catalog semantics).
pub(super) struct LegacyConsumer {
    broadcast: moq_net::broadcast::Consumer,
    track: MediaTrack,
    latency: std::time::Duration,
    subscriber: Option<TrackConsumer>,
    group: Option<GroupConsumer>,
    first: bool,
}

impl LegacyConsumer {
    pub fn new(
        broadcast: moq_net::broadcast::Consumer,
        track: MediaTrack,
        latency: std::time::Duration,
    ) -> Self {
        Self {
            broadcast,
            track,
            latency,
            subscriber: None,
            group: None,
            first: true,
        }
    }

    pub async fn read(
        &mut self,
    ) -> Result<Option<LegacyFrame>, Box<dyn std::error::Error + Send + Sync>> {
        if self.subscriber.is_none() {
            let subscription = moq_net::track::Subscription::default()
                .with_priority(self.track.priority)
                .with_latency_max(self.latency);
            self.subscriber = Some(
                self.broadcast
                    .track(&self.track.name)?
                    .subscribe(subscription)
                    .await?,
            );
        }
        let subscriber = self
            .subscriber
            .as_mut()
            .ok_or("Media subscription missing")?;
        loop {
            if self.group.is_none() {
                self.group = subscriber.next_group().await?;
                self.first = true;
                if self.group.is_none() {
                    return Ok(None);
                }
            }
            let group = self.group.as_mut().ok_or("Media group missing")?;
            let result = tokio::time::timeout(
                self.latency.max(std::time::Duration::from_millis(1)),
                group.read_frame(),
            )
            .await;
            match result {
                Ok(Ok(Some(frame))) => {
                    let frame = hang::container::Frame::decode(frame.payload)?;
                    let timestamp = std::time::Duration::from_micros(
                        frame.timestamp.convert(moq_net::Timescale::MICRO)?.value(),
                    );
                    let keyframe = std::mem::replace(&mut self.first, false);
                    return Ok(Some(LegacyFrame {
                        timestamp,
                        payload: frame.payload,
                        keyframe,
                    }));
                }
                Ok(Ok(None)) | Err(_) => {
                    self.group = None;
                }
                Ok(Err(
                    moq_net::Error::Old
                    | moq_net::Error::Lagged
                    | moq_net::Error::Evicted
                    | moq_net::Error::Dropped
                    | moq_net::Error::Timeout,
                )) => {
                    // A lost/expired group does not close its track. Resume at the
                    // next group boundary (a keyframe), never at a damaged delta.
                    self.group = None;
                }
                Ok(Err(error)) => return Err(error.into()),
            }
        }
    }
}

#[cfg(test)]
mod legacy_tests {
    use super::*;
    use std::time::Duration;

    fn frame(payload: &'static [u8], micros: u64) -> hang::container::Frame {
        hang::container::Frame {
            timestamp: moq_net::Timestamp::from_micros(micros).unwrap(),
            payload: Bytes::from_static(payload),
        }
    }

    #[tokio::test]
    async fn legacy_reads_every_frame_and_marks_group_start() {
        let mut broadcast = moq_net::broadcast::Producer::new(Default::default());
        let mut track = broadcast
            .create_track("video", hang::container::track_info())
            .unwrap();
        let mut group = track.append_group().unwrap();
        frame(b"key", 10).write_to(&mut group).unwrap();
        frame(b"delta", 20).write_to(&mut group).unwrap();
        group.finish().unwrap();
        let mut reader = LegacyConsumer::new(
            broadcast.consume(),
            MediaTrack {
                name: "video".into(),
                priority: 100,
            },
            Duration::from_millis(20),
        );
        let first = reader.read().await.unwrap().unwrap();
        let second = reader.read().await.unwrap().unwrap();
        assert!(first.keyframe);
        assert!(!second.keyframe);
        assert_eq!(first.payload, b"key"[..]);
        assert_eq!(second.payload, b"delta"[..]);
        assert_eq!(second.timestamp, Duration::from_micros(20));
    }

    #[tokio::test]
    async fn legacy_recovers_after_group_loss_but_preserves_fatal_errors() {
        for error in [
            moq_net::Error::Old,
            moq_net::Error::Lagged,
            moq_net::Error::Evicted,
            moq_net::Error::Dropped,
            moq_net::Error::Timeout,
            moq_net::Error::Transport("connection lost".into()),
            moq_net::Error::Unauthorized,
        ] {
            let fatal = matches!(
                error,
                moq_net::Error::Transport(_) | moq_net::Error::Unauthorized
            );
            let mut broadcast = moq_net::broadcast::Producer::new(Default::default());
            let mut track = broadcast
                .create_track("video", hang::container::track_info())
                .unwrap();
            let mut lost = track.append_group().unwrap();
            frame(b"old", 10).write_to(&mut lost).unwrap();
            let mut reader = LegacyConsumer::new(
                broadcast.consume(),
                MediaTrack {
                    name: "video".into(),
                    priority: 100,
                },
                Duration::from_secs(1),
            );
            assert_eq!(reader.read().await.unwrap().unwrap().payload, b"old"[..]);
            lost.abort(error.clone()).unwrap();
            let mut fresh = track.append_group().unwrap();
            frame(b"fresh-key", 30).write_to(&mut fresh).unwrap();
            frame(b"fresh-delta", 40).write_to(&mut fresh).unwrap();
            fresh.finish().unwrap();
            let next = tokio::time::timeout(Duration::from_secs(1), reader.read())
                .await
                .unwrap();
            if fatal {
                assert_eq!(next.err().unwrap().to_string(), error.to_string());
            } else {
                let next = next.unwrap().unwrap();
                assert_eq!(next.payload, b"fresh-key"[..]);
                assert_eq!(next.timestamp, Duration::from_micros(30));
                assert!(next.keyframe);
                let delta = reader.read().await.unwrap().unwrap();
                assert_eq!(delta.payload, b"fresh-delta"[..]);
                assert!(!delta.keyframe);
            }
        }
    }

    #[tokio::test]
    async fn legacy_skips_stalled_group_within_latency_budget() {
        let mut broadcast = moq_net::broadcast::Producer::new(Default::default());
        let mut track = broadcast
            .create_track("video", hang::container::track_info())
            .unwrap();
        let mut stalled = track.append_group().unwrap();
        frame(b"old", 10).write_to(&mut stalled).unwrap();
        let mut reader = LegacyConsumer::new(
            broadcast.consume(),
            MediaTrack {
                name: "video".into(),
                priority: 100,
            },
            Duration::from_millis(20),
        );
        assert!(reader.read().await.unwrap().unwrap().keyframe);
        let mut fresh = track.append_group().unwrap();
        frame(b"fresh", 20).write_to(&mut fresh).unwrap();
        fresh.finish().unwrap();
        let next = tokio::time::timeout(Duration::from_secs(1), reader.read())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(next.payload, b"fresh"[..]);
        assert!(next.keyframe);
    }
}
