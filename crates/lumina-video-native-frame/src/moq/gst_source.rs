//! Encoded MoQ tracks feeding the existing GStreamer media session.
//! GStreamer owns both audio and video presentation; transport never decodes pixels.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;

use super::{worker, MoqUrl};
use crate::moq_decoder::{MoqDecoder, MoqDecoderConfig, MoqSharedState};
use crate::video::VideoError;

type Error = Box<dyn std::error::Error + Send + Sync>;
const MAX_QUEUED_BUFFERS: u64 = 4;
const READ_TIMEOUT: Duration = Duration::from_secs(8);

/// Lives with the GStreamer pipeline; dropping it cancels transport without
/// joining a Tokio runtime on the render thread.
pub(crate) struct GstMoqSource {
    pub(crate) element: gst::Element,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Drop for GstMoqSource {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl GstMoqSource {
    pub(crate) fn new(url: &str, deadline: Instant) -> Result<Self, VideoError> {
        let moq_url =
            MoqUrl::parse(url).map_err(|error| VideoError::OpenFailed(error.to_string()))?;
        let mut config = MoqDecoderConfig::default();
        config.apply_localhost_tls_bypass(&moq_url);
        let (source, decoder) = decode_bin()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("moq-gst-source")
            .build()
            .map_err(|error| VideoError::OpenFailed(error.to_string()))?;
        // Install the nonblocking shutdown guard before any fallible setup.
        // A rejected catalog/codec must not join transport tasks on its caller.
        let result = Self {
            element: source.clone().upcast(),
            runtime: Some(runtime),
        };
        let runtime = result
            .runtime
            .as_ref()
            .ok_or_else(|| VideoError::DecoderInit("MoQ runtime missing".into()))?;
        let opened = runtime.block_on(async {
            tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), async {
                let (url, path) = worker::build_connect_url(&moq_url);
                let url = url::Url::parse(&url)?;
                let (mut origin, _, session) =
                    worker::connect_to_relay(&url, &config, "GStreamer").await?;
                let broadcast =
                    worker::discover_broadcast(&mut origin, path, &moq_url, "GStreamer").await?;
                let shared = Arc::new(MoqSharedState::new());
                let catalog =
                    worker::fetch_and_validate_catalog(&broadcast, &shared, &config, "GStreamer")
                        .await?;
                Ok::<_, Error>((catalog, broadcast, session, origin))
            })
            .await
        });
        let (catalog, broadcast, session, origin) = match opened {
            Ok(Ok(opened)) => opened,
            result => {
                return Err(VideoError::OpenFailed(match result {
                    Ok(Err(error)) => error.to_string(),
                    _ => "MoQ connection/catalog initialization timed out".into(),
                }));
            }
        };

        let video_config = catalog
            .catalog
            .video
            .renditions
            .get(&catalog.video_track_name)
            .ok_or_else(|| {
                VideoError::DecoderInit("MoQ catalog lost selected video track".into())
            })?;
        let (caps, parser) = video_caps(video_config)?;
        let video = add_track(&source, &decoder, "video", &caps, parser, false)?;
        let mut tracks = vec![(catalog.video_track_name.clone(), video, true)];
        if let Some((name, config)) =
            crate::moq_audio::select_preferred_audio_rendition(&catalog.catalog)
        {
            if !matches!(config.container, hang::catalog::Container::Legacy) {
                return Err(VideoError::UnsupportedFormat(
                    "MoQ audio requires Legacy encoded frames".into(),
                ));
            }
            let (caps, parser) = audio_caps(config)?;
            let audio = add_track(&source, &decoder, "audio", &caps, parser, true)?;
            tracks.push((name.to_owned(), audio, false));
        }
        let timeline = Arc::new(OnceLock::new());
        let error_source = source.clone();
        runtime.spawn(async move {
            // Retain connection and announcement subscription while either track runs.
            let _connection = (session, origin);
            let mut tasks = tokio::task::JoinSet::new();
            for (name, appsrc, video) in tracks {
                let consumer = hang::container::OrderedConsumer::new(
                    broadcast.subscribe_track(&moq_lite::Track {
                        name,
                        priority: if video { 100 } else { 50 },
                    }),
                    catalog.max_latency,
                );
                tasks.spawn(forward_track(
                    consumer,
                    appsrc,
                    video,
                    Arc::clone(&timeline),
                ));
            }
            while let Some(result) = tasks.join_next().await {
                let error = match result {
                    Ok(Ok(())) => continue,
                    Ok(Err(error)) => error.to_string(),
                    Err(error) => error.to_string(),
                };
                gst::element_error!(
                    error_source,
                    gst::ResourceError::Read,
                    ("MoQ track failed: {error}")
                );
                tasks.abort_all();
                break;
            }
        });
        Ok(result)
    }
}

fn decode_bin() -> Result<(gst::Bin, gst::Element), VideoError> {
    let source = gst::Bin::new();
    let decoder = gst::ElementFactory::make("decodebin3")
        .build()
        .map_err(|error| VideoError::DecoderInit(error.to_string()))?;
    source
        .add(&decoder)
        .map_err(|error| VideoError::DecoderInit(error.to_string()))?;
    let weak_source = source.downgrade();
    decoder.connect_pad_added(move |_, pad| {
        if pad.direction() != gst::PadDirection::Src {
            return;
        }
        let Some(source) = weak_source.upgrade() else {
            return;
        };
        let result = gst::GhostPad::with_target(pad).and_then(|ghost| {
            ghost.set_active(true)?;
            source.add_pad(&ghost)
        });
        if let Err(error) = result {
            gst::element_error!(source, gst::CoreError::Pad, ("MoQ output pad: {error}"));
        }
    });
    let weak_source = source.downgrade();
    decoder.connect_pad_removed(move |_, pad| {
        if pad.direction() != gst::PadDirection::Src {
            return;
        }
        if let Some(source) = weak_source.upgrade() {
            if let Some(ghost) = source.static_pad(&pad.name()) {
                let _ = source.remove_pad(&ghost);
            }
        }
    });

    Ok((source, decoder))
}

fn add_track(
    source: &gst::Bin,
    decoder: &gst::Element,
    name: &str,
    caps: &gst::Caps,
    parser_name: &str,
    additional: bool,
) -> Result<gst_app::AppSrc, VideoError> {
    let appsrc = gst_app::AppSrc::builder()
        .name(format!("moq_{name}"))
        .caps(caps)
        .stream_type(gst_app::AppStreamType::Stream)
        .format(gst::Format::Time)
        .is_live(true)
        .block(false)
        .max_buffers(MAX_QUEUED_BUFFERS)
        .max_bytes(4 * 1024 * 1024)
        .leaky_type(gst_app::AppLeakyType::Downstream)
        .build();
    let parser = gst::ElementFactory::make(parser_name)
        .build()
        .map_err(|error| VideoError::DecoderInit(format!("MoQ {parser_name}: {error}")))?;
    source
        .add_many([appsrc.upcast_ref(), &parser])
        .and_then(|_| appsrc.link(&parser))
        .map_err(|error| VideoError::DecoderInit(error.to_string()))?;
    let sink = if additional {
        decoder.request_pad_simple("sink_%u")
    } else {
        decoder.static_pad("sink")
    }
    .ok_or_else(|| VideoError::DecoderInit("decodebin3 has no encoded input pad".into()))?;
    parser
        .static_pad("src")
        .ok_or_else(|| VideoError::DecoderInit("MoQ parser has no output".into()))?
        .link(&sink)
        .map_err(|error| {
            VideoError::DecoderInit(format!(
                "MoQ {name} parser to {} (peer {:?}): {error}",
                sink.name(),
                sink.peer().map(|pad| pad.path_string())
            ))
        })?;
    Ok(appsrc)
}

fn video_caps(
    config: &hang::catalog::VideoConfig,
) -> Result<(gst::Caps, &'static str), VideoError> {
    use hang::catalog::VideoCodec;
    let description = config.description.as_ref();
    let (media, parser, format) = match config.codec {
        VideoCodec::H264(_) => {
            if let Some(description) = description {
                MoqDecoder::parse_avcc_box(description)?;
            }
            (
                "video/x-h264",
                "h264parse",
                Some(if description.is_some() {
                    "avc"
                } else {
                    "byte-stream"
                }),
            )
        }
        VideoCodec::H265(_) => (
            "video/x-h265",
            "h265parse",
            Some(if description.is_some() {
                "hvc1"
            } else {
                "byte-stream"
            }),
        ),
        VideoCodec::VP9(_) => ("video/x-vp9", "vp9parse", None),
        VideoCodec::AV1(_) => ("video/x-av1", "av1parse", Some("obu-stream")),
        _ => {
            return Err(VideoError::UnsupportedFormat(format!(
                "MoQ codec {}",
                config.codec
            )))
        }
    };
    let mut caps = gst::Caps::builder(media).field(
        "alignment",
        if media == "video/x-av1" { "tu" } else { "au" },
    );
    if let Some(format) = format {
        caps = caps.field("stream-format", format);
    }
    if let Some(description) = description {
        caps = caps.field("codec_data", gst::Buffer::from_slice(description.clone()));
    }
    Ok((caps.build(), parser))
}

fn audio_caps(
    config: &hang::catalog::AudioConfig,
) -> Result<(gst::Caps, &'static str), VideoError> {
    use hang::catalog::AudioCodec;
    match config.codec {
        AudioCodec::AAC(_) => {
            let description = config
                .description
                .as_ref()
                .filter(|description| description.len() >= 2)
                .ok_or_else(|| {
                    VideoError::UnsupportedFormat("MoQ AAC requires AudioSpecificConfig".into())
                })?;
            Ok((
                gst::Caps::builder("audio/mpeg")
                    .field("mpegversion", 4i32)
                    .field("stream-format", "raw")
                    .field("codec_data", gst::Buffer::from_slice(description.clone()))
                    .build(),
                "aacparse",
            ))
        }
        AudioCodec::Opus if (1..=2).contains(&config.channel_count) => Ok((
            gst::Caps::builder("audio/x-opus")
                .field("channel-mapping-family", 0i32)
                .field("channels", config.channel_count as i32)
                .field("rate", 48000i32)
                .build(),
            "opusparse",
        )),
        _ => Err(VideoError::UnsupportedFormat(format!(
            "MoQ audio codec {}",
            config.codec
        ))),
    }
}

/// Shared, write-once publisher/running-time origin keeps audio and video on
/// the same GStreamer clock without a per-frame mutex or wall-clock conversion.
type Timeline = OnceLock<(u64, u64, u64)>;

fn running_pts(timestamp: u64, origin: u64, base: u64, paused: u64) -> Option<u64> {
    base.checked_add(timestamp.saturating_sub(origin))?
        .checked_sub(origin.saturating_sub(timestamp))?
        .checked_sub(paused)
}

async fn forward_track(
    mut consumer: hang::container::OrderedConsumer,
    appsrc: gst_app::AppSrc,
    video: bool,
    timeline: Arc<Timeline>,
) -> Result<(), Error> {
    let mut scratch = bytes::BytesMut::new();
    let mut waiting_for_keyframe = video;
    let mut wait_started = Instant::now();
    loop {
        // This dedicated task never retries a consumer after cancellation.
        let frame = tokio::time::timeout(READ_TIMEOUT, consumer.read()).await??;
        let Some(frame) = frame else {
            appsrc.end_of_stream()?;
            return Ok(());
        };
        // Keep consuming the live edge while paused, but never leave future
        // timestamps queued against a clock whose running time is suspended.
        if appsrc.current_state() != gst::State::Playing {
            waiting_for_keyframe = video;
            wait_started = Instant::now();
            continue;
        }
        if video && appsrc.current_level_buffers() >= MAX_QUEUED_BUFFERS {
            if !waiting_for_keyframe {
                wait_started = Instant::now();
            }
            waiting_for_keyframe = true;
        }
        if waiting_for_keyframe && !frame.keyframe {
            if wait_started.elapsed() >= READ_TIMEOUT {
                return Err("MoQ keyframe recovery timed out".into());
            }
            continue;
        }
        let discontinuity = waiting_for_keyframe;
        waiting_for_keyframe = false;
        let timestamp = u64::try_from(frame.timestamp.as_micros())?;
        let current_base = appsrc.base_time().map_or(0, |time| time.useconds());
        let &(origin, base, initial_base) = timeline.get_or_init(|| {
            (
                timestamp,
                appsrc
                    .current_running_time()
                    .map_or(0, |time| time.useconds())
                    .saturating_add(100_000),
                current_base,
            )
        });
        let pts = running_pts(
            timestamp,
            origin,
            base,
            current_base.saturating_sub(initial_base),
        )
        .ok_or("MoQ publisher timestamp moved before the session origin")?;
        let mut buffer =
            gst::Buffer::from_slice(worker::assemble_payload(&frame.payload, &mut scratch));
        let buffer_ref = buffer
            .get_mut()
            .ok_or("MoQ encoded buffer is not writable")?;
        buffer_ref.set_pts(gst::ClockTime::from_useconds(pts));
        if video && !frame.keyframe {
            buffer_ref.set_flags(gst::BufferFlags::DELTA_UNIT);
        }
        if discontinuity {
            buffer_ref.set_flags(gst::BufferFlags::DISCONT);
        }
        // Encoded payload ownership transfers without copying decoded pixels.
        appsrc.push_buffer(buffer)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_timeline_preserves_av_offsets_and_skips_paused_time() {
        let publisher = 7_200_000_000;
        assert_eq!(running_pts(publisher, publisher, 100_000, 0), Some(100_000));
        assert_eq!(
            running_pts(publisher - 20_000, publisher, 100_000, 0),
            Some(80_000)
        );
        assert_eq!(
            running_pts(publisher + 5_040_000, publisher, 100_000, 5_000_000),
            Some(140_000)
        );
        assert_eq!(running_pts(0, publisher, 100_000, 0), None);
        assert_eq!(running_pts(u64::MAX, 0, 100_000, 0), None);
    }

    #[test]
    #[ignore = "requires fixtures/generate.sh and GStreamer H264/AAC decoder plugins"]
    fn encoded_av_tracks_decode_through_the_moq_source_bin() -> Result<(), Error> {
        gst::init()?;
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/generated/h264-aac.mp4");
        if !fixture.is_file() {
            return Err("run fixtures/generate.sh first".into());
        }
        let input = gst::parse::launch(&format!(
            "filesrc location=\"{}\" ! qtdemux name=d d.video_0 ! queue ! appsink name=video sync=false d.audio_0 ! queue ! appsink name=audio sync=false", fixture.display()
        ))?.downcast::<gst::Pipeline>().map_err(|_| "fixture is not a pipeline")?;
        struct Stop(gst::Pipeline);
        impl Drop for Stop {
            fn drop(&mut self) {
                let _ = self.0.set_state(gst::State::Null);
            }
        }
        let _input_stop = Stop(input.clone());
        input.set_state(gst::State::Playing)?;
        let take_samples = |name: &str| -> Result<Vec<gst::Sample>, Error> {
            let sink = input
                .by_name(name)
                .ok_or("fixture sink missing")?
                .downcast::<gst_app::AppSink>()
                .map_err(|_| "fixture sink is not appsink")?;
            (0..12)
                .map(|_| {
                    sink.try_pull_sample(gst::ClockTime::from_seconds(3))
                        .ok_or_else(|| "fixture encoded sample missing".into())
                })
                .collect()
        };
        let video_samples = take_samples("video")?;
        let audio_samples = take_samples("audio")?;
        let description = |samples: &[gst::Sample]| -> Result<bytes::Bytes, Error> {
            let caps = samples
                .first()
                .and_then(|sample| sample.caps())
                .ok_or("encoded caps missing")?;
            let buffer = caps
                .structure(0)
                .ok_or("encoded caps empty")?
                .get::<gst::Buffer>("codec_data")?;
            let map = buffer.map_readable()?;
            Ok(bytes::Bytes::copy_from_slice(map.as_slice()))
        };
        let video_config = hang::catalog::VideoConfig {
            codec: "avc1.64001f".parse()?,
            description: Some(description(&video_samples)?),
            coded_width: Some(320),
            coded_height: Some(180),
            display_ratio_width: None,
            display_ratio_height: None,
            bitrate: None,
            framerate: Some(30.0),
            optimize_for_latency: Some(true),
            container: hang::catalog::Container::Legacy,
            jitter: None,
        };
        let audio_config = hang::catalog::AudioConfig {
            codec: "mp4a.40.2".parse()?,
            sample_rate: 48000,
            channel_count: 2,
            description: Some(description(&audio_samples)?),
            bitrate: None,
            container: hang::catalog::Container::Legacy,
            jitter: None,
        };
        let (bin, decoder) = decode_bin()?;
        let (caps, parser) = video_caps(&video_config)?;
        let video = add_track(&bin, &decoder, "video", &caps, parser, false)?;
        let (caps, parser) = audio_caps(&audio_config)?;
        let audio = add_track(&bin, &decoder, "audio", &caps, parser, true)?;
        let output = gst::Pipeline::new();
        let _output_stop = Stop(output.clone());
        let video_sink = gst_app::AppSink::builder().sync(false).build();
        let audio_sink = gst_app::AppSink::builder().sync(false).build();
        output.add_many([
            bin.upcast_ref::<gst::Element>(),
            video_sink.upcast_ref(),
            audio_sink.upcast_ref(),
        ])?;
        let weak_video = video_sink.downgrade();
        let weak_audio = audio_sink.downgrade();
        bin.connect_pad_added(move |_, pad| {
            let sink = if pad.name().starts_with("video") {
                weak_video.upgrade()
            } else {
                weak_audio.upgrade()
            };
            if let Some(sink) = sink {
                if let Some(input) = sink.static_pad("sink") {
                    let _ = pad.link(&input);
                }
            }
        });
        output.set_state(gst::State::Playing)?;
        // Keep the test's tiny batch from deliberately exercising the live drop policy.
        video.set_max_buffers(0);
        audio.set_max_buffers(0);
        for (appsrc, samples) in [(&video, video_samples), (&audio, audio_samples)] {
            for sample in samples {
                appsrc.push_buffer(
                    sample
                        .buffer()
                        .ok_or("encoded sample has no buffer")?
                        .to_owned(),
                )?;
            }
            appsrc.end_of_stream()?;
        }
        let decoded_video = video_sink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .ok_or("MoQ source produced no decoded video")?;
        let decoded_audio = audio_sink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .ok_or("MoQ source produced no decoded audio")?;
        let caps = decoded_video.caps().ok_or("decoded caps missing")?;
        let info = gstreamer_video::VideoInfo::from_caps(caps)?;
        assert_eq!((info.width(), info.height()), (320, 180));
        let pixels = decoded_video
            .buffer()
            .ok_or("decoded video buffer missing")?
            .map_readable()?;
        assert!(
            pixels
                .as_slice()
                .windows(2)
                .any(|pair| pair.first() != pair.get(1)),
            "decoded output is a fabricated solid frame"
        );
        assert!(decoded_audio
            .buffer()
            .is_some_and(|buffer| buffer.size() > 0));
        Ok(())
    }
}
