//! lumina-video GPUI Demo Application
//!
//! Demonstrates hardware-accelerated video playback in GPUI using
//! `GpuiVideoPlayer`. Shows NV12 GPU-side YUV→RGB conversion via
//! GPUI's built-in `surface()` element, plus interactive controls.
//!
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, px, relative, rgb, rgba, size, App, Bounds, FontWeight, KeyDownEvent, MouseButton,
    SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use lumina_video::GpuiVideoPlayer;

const SAMPLE_VIDEOS: &[(&str, &str)] = &[
    (
        "Big Buck Bunny (MP4)",
        "https://download.blender.org/peach/bigbuckbunny_movies/BigBuckBunny_320x180.mp4",
    ),
    (
        "Sintel (MP4)",
        "https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/Sintel.mp4",
    ),
    (
        "Elephant's Dream (MP4)",
        "https://archive.org/download/ElephantsDream/ed_hd.mp4",
    ),
    (
        "Tears of Steel (MOV)",
        "https://download.blender.org/demo/movies/ToS/tears_of_steel_720p.mov",
    ),
    (
        "Sample MKV",
        "https://www.learningcontainer.com/wp-content/uploads/2020/05/sample-mkv-file.mkv",
    ),
];

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("lumina_video=debug".parse().unwrap()),
        )
        .init();

    // Platform-specific application creation.
    // Mirrors what gpui_platform::application() does internally, but using
    // the platform crates directly so we don't need gpui_platform as a dep.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    let app = gpui::Application::with_platform(gpui_linux::current_platform(false));
    #[cfg(target_os = "macos")]
    let app =
        gpui::Application::with_platform(std::rc::Rc::new(gpui_macos::MacPlatform::new(false)));
    #[cfg(target_os = "windows")]
    let app = gpui::Application::with_platform(std::rc::Rc::new(
        gpui_windows::WindowsPlatform::new(false).expect("Failed to initialise Windows platform"),
    ));
    #[cfg(not(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "macos",
        target_os = "windows"
    )))]
    compile_error!(
        "unsupported platform — lumina-video GPUI demo requires Linux, macOS, or Windows"
    );

    app.run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.0), px(700.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("lumina-video GPUI Demo")),
                    appears_transparent: false,
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|_| DemoApp::new()),
        )
        .unwrap();
        cx.activate(true);
    });
}

struct DemoApp {
    player: Option<GpuiVideoPlayer>,
    selected_sample: usize,
    status: String,
}

impl DemoApp {
    fn new() -> Self {
        let mut s = Self {
            player: None,
            selected_sample: 0,
            status: "Select a sample and press Load, or press Enter".into(),
        };
        // Auto-load test video if LUMINA_TEST_VIDEO env var is set
        if let Ok(test_url) = std::env::var("LUMINA_TEST_VIDEO") {
            s.load_video(&test_url);
        }
        s
    }

    fn load_video(&mut self, url: &str) {
        tracing::info!("Loading: {url}");
        self.status = format!("Loading: {url}...");

        let player = GpuiVideoPlayer::new(url.to_string())
            .with_autoplay(true)
            .with_controls(true)
            .with_looping(false);
        self.player = Some(player);
    }
}

impl Render for DemoApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Update video player each frame
        if let Some(ref mut player) = self.player {
            player.update(window, cx);
        }

        let sel_name = SAMPLE_VIDEOS[self.selected_sample].0;
        let sel_url = SAMPLE_VIDEOS[self.selected_sample].1;

        // Gather player state for rendering
        let (is_ready, is_playing, is_ended, is_error) = match &self.player {
            Some(p) => (p.is_ready(), p.is_playing(), p.is_ended(), p.is_error()),
            None => (false, false, false, false),
        };

        // Controls bar state
        let show_controls = self.player.as_ref().is_some_and(|p| p.is_ready());
        let play_icon = if is_ended {
            "↺"
        } else if is_playing {
            "⏸"
        } else {
            "▶"
        };
        let is_muted = self.player.as_ref().is_some_and(|p| p.is_muted());
        let mute_icon = if is_muted { "🔇" } else { "🔊" };
        let position = self
            .player
            .as_ref()
            .map_or(Duration::ZERO, |p| p.position());
        let duration = self.player.as_ref().and_then(|p| p.duration());
        let seek_progress = self.player.as_ref().map_or(0.0f32, |p| p.seek_progress());
        let buffering = self.player.as_ref().map_or(100, |p| p.buffering_percent());

        // Sidebar info
        let state_text = if let Some(ref p) = self.player {
            if p.is_playing() {
                "▶ Playing"
            } else if p.is_ended() {
                "⏹ Ended"
            } else if p.is_error() {
                "✕ Error"
            } else if p.is_ready() {
                "⏸ Ready"
            } else {
                "⏳ Loading..."
            }
        } else {
            "No video"
        };
        let pos_text = format_time(position);
        let dur_text = duration.map(format_time).unwrap_or_else(|| "live".into());
        let res_text = self
            .player
            .as_ref()
            .and_then(|p| p.metadata())
            .map(|m| format!("{}×{}", m.width, m.height))
            .unwrap_or_else(|| "—".into());
        let fps_text = self
            .player
            .as_ref()
            .and_then(|p| p.frame_rate())
            .map(|f| format!("{f:.1} fps"))
            .unwrap_or_else(|| "—".into());

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x0d1117))
            .text_color(rgb(0xe6edf3))
            .on_key_down(
                cx.listener(|this: &mut DemoApp, event: &KeyDownEvent, _w, cx| {
                    let key = event.keystroke.key.as_str();
                    match key {
                        "left" => {
                            if let Some(ref mut p) = this.player {
                                let pos = p.position();
                                p.seek(pos.saturating_sub(Duration::from_secs(5)));
                                this.status = format!("Seek ← {:.0}s", p.position().as_secs());
                                cx.notify();
                            }
                        }
                        "right" => {
                            if let Some(ref mut p) = this.player {
                                let pos = p.position();
                                let dur = p.duration().unwrap_or(Duration::MAX);
                                p.seek((pos + Duration::from_secs(5)).min(dur));
                                this.status = format!("Seek → {:.0}s", p.position().as_secs());
                                cx.notify();
                            }
                        }
                        "up" => {
                            if this.selected_sample > 0 {
                                this.selected_sample -= 1;
                                cx.notify();
                            }
                        }
                        "down" => {
                            if this.selected_sample + 1 < SAMPLE_VIDEOS.len() {
                                this.selected_sample += 1;
                                cx.notify();
                            }
                        }
                        "enter" | "return" => {
                            let url = SAMPLE_VIDEOS[this.selected_sample].1.to_string();
                            this.load_video(&url);
                            cx.notify();
                        }
                        "space" => {
                            if let Some(ref mut p) = this.player {
                                p.toggle_playback();
                                cx.notify();
                            }
                        }
                        "m" => {
                            if let Some(ref mut p) = this.player {
                                p.toggle_mute();
                                cx.notify();
                            }
                        }
                        _ => {}
                    }
                }),
            )
            // === Top toolbar ===
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .bg(rgb(0x161b22))
                    .border_b_1()
                    .border_color(rgb(0x30363d))
                    // ◀ Prev
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .rounded_sm()
                            .hover(|d| d.bg(rgb(0x21262d)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e, _w, cx| {
                                    if this.selected_sample > 0 {
                                        this.selected_sample -= 1;
                                        cx.notify();
                                    }
                                }),
                            )
                            .child("◀"),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(sel_name.to_string()),
                    )
                    // ▶ Next
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .rounded_sm()
                            .hover(|d| d.bg(rgb(0x21262d)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e, _w, cx| {
                                    if this.selected_sample + 1 < SAMPLE_VIDEOS.len() {
                                        this.selected_sample += 1;
                                        cx.notify();
                                    }
                                }),
                            )
                            .child("▶"),
                    )
                    // URL
                    .child(
                        div()
                            .flex_1()
                            .h(px(30.0))
                            .bg(rgb(0x0d1117))
                            .border_1()
                            .border_color(rgb(0x30363d))
                            .rounded_sm()
                            .px_2()
                            .flex()
                            .items_center()
                            .text_sm()
                            .text_color(rgb(0x8b949e))
                            .overflow_hidden()
                            .child(sel_url.to_string()),
                    )
                    // Load
                    .child(
                        div()
                            .px_4()
                            .py(px(6.0))
                            .bg(rgb(0x238636))
                            .rounded_md()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .cursor_pointer()
                            .hover(|d| d.bg(rgb(0x2ea043)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _e, _w, cx| {
                                    let url = SAMPLE_VIDEOS[this.selected_sample].1.to_string();
                                    this.load_video(&url);
                                    cx.notify();
                                }),
                            )
                            .child("Load"),
                    ),
            )
            // === Main area ===
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    // Video + controls
                    .child(
                        div()
                            .flex_1()
                            .relative()
                            .bg(rgb(0x000000))
                            // Video frame / placeholder / overlays
                            .child({
                                if is_ready {
                                    // Show the video surface
                                    let player_ref = self.player.as_ref().unwrap();
                                    player_ref.surface_element().into_any_element()
                                } else if is_error {
                                    let player_ref = self.player.as_ref().unwrap();
                                    player_ref.error_overlay().into_any_element()
                                } else if self.player.is_some() {
                                    // Still loading
                                    div()
                                        .absolute()
                                        .size_full()
                                        .bg(rgb(0x000000))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            div()
                                                .flex()
                                                .flex_col()
                                                .items_center()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .text_xl()
                                                        .text_color(rgb(0xcccccc))
                                                        .child("⏳"),
                                                )
                                                .child(
                                                    div()
                                                        .text_sm()
                                                        .text_color(rgb(0x999999))
                                                        .child("Loading..."),
                                                ),
                                        )
                                        .into_any_element()
                                } else {
                                    // No player
                                    div()
                                        .size_full()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            div()
                                                .text_color(rgb(0x484f58))
                                                .text_lg()
                                                .child("No video loaded"),
                                        )
                                        .into_any_element()
                                }
                            })
                            // Buffering overlay
                            .child({
                                if is_playing && buffering < 100 {
                                    div()
                                        .absolute()
                                        .size_full()
                                        .bg(rgba(0x00000088))
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .justify_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_lg()
                                                .text_color(rgb(0x64b4ff))
                                                .child(format!("{buffering}%")),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(0xcccccc))
                                                .child("Buffering..."),
                                        )
                                        .into_any_element()
                                } else {
                                    div().into_any_element()
                                }
                            })
                            // Subtitle overlay
                            .child({
                                if let Some(ref p) = self.player {
                                    if let Some(sub_el) = p.subtitle_overlay() {
                                        sub_el.into_any_element()
                                    } else {
                                        div().into_any_element()
                                    }
                                } else {
                                    div().into_any_element()
                                }
                            })
                            // Controls bar
                            .child(if show_controls {
                                div()
                                    .absolute()
                                    .bottom_0()
                                    .left_0()
                                    .right_0()
                                    .h(px(40.0))
                                    .bg(rgba(0x000000cc))
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .px_2()
                                    .gap_2()
                                    // Play/pause
                                    .child(
                                        div()
                                            .px_1()
                                            .cursor_pointer()
                                            .text_color(rgb(0xffffff))
                                            .hover(|d| d.bg(rgba(0xffffff22)))
                                            .rounded_sm()
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(|this, _e, _w, cx| {
                                                    if let Some(ref mut p) = this.player {
                                                        p.toggle_playback();
                                                        cx.notify();
                                                    }
                                                }),
                                            )
                                            .child(play_icon.to_string()),
                                    )
                                    // Time
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(0xcccccc))
                                            .child(pos_text.clone()),
                                    )
                                    // Seek bar (visual only)
                                    .child(
                                        div().flex_1().h(px(20.0)).flex().items_center().child(
                                            div()
                                                .w_full()
                                                .h(px(4.0))
                                                .bg(rgba(0xffffff33))
                                                .rounded_full()
                                                .child(
                                                    div()
                                                        .h_full()
                                                        .bg(rgb(0x3b82f6))
                                                        .rounded_full()
                                                        .w(relative(seek_progress)),
                                                ),
                                        ),
                                    )
                                    // Duration
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(0xcccccc))
                                            .child(dur_text.clone()),
                                    )
                                    // Mute
                                    .child(
                                        div()
                                            .px_1()
                                            .cursor_pointer()
                                            .text_color(rgb(0xffffff))
                                            .hover(|d| d.bg(rgba(0xffffff22)))
                                            .rounded_sm()
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(|this, _e, _w, cx| {
                                                    if let Some(ref mut p) = this.player {
                                                        p.toggle_mute();
                                                        cx.notify();
                                                    }
                                                }),
                                            )
                                            .child(mute_icon.to_string()),
                                    )
                                    .into_any_element()
                            } else {
                                div().into_any_element()
                            })
                            // Click to play/pause
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e, _w, cx| {
                                    if let Some(ref mut p) = this.player {
                                        p.toggle_playback();
                                        cx.notify();
                                    }
                                }),
                            ),
                    )
                    // Sidebar
                    .child(
                        div()
                            .w(px(220.0))
                            .bg(rgb(0x161b22))
                            .border_l_1()
                            .border_color(rgb(0x30363d))
                            .flex()
                            .flex_col()
                            .p_3()
                            .gap_1()
                            .child(section("SAMPLE VIDEOS"))
                            .children({
                                let sel = self.selected_sample;
                                let mut items: Vec<gpui::AnyElement> = Vec::new();
                                for (i, (name, _)) in SAMPLE_VIDEOS.iter().enumerate() {
                                    let selected = i == sel;
                                    items.push(
                                        div()
                                            .px_2()
                                            .py_1()
                                            .rounded_sm()
                                            .text_sm()
                                            .cursor_pointer()
                                            .when(selected, |d| d.bg(rgb(0x1f6feb)))
                                            .hover(
                                                |d| if selected { d } else { d.bg(rgb(0x21262d)) },
                                            )
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, _e, _w, cx| {
                                                    this.selected_sample = i;
                                                    cx.notify();
                                                }),
                                            )
                                            .child(*name)
                                            .into_any_element(),
                                    );
                                }
                                items
                            })
                            .child(div().h(px(16.0)))
                            .child(section("VIDEO INFO"))
                            .child(info_row("Status", state_text))
                            .child(info_row("Position", &pos_text))
                            .child(info_row("Duration", &dur_text))
                            .child(info_row("Resolution", &res_text))
                            .child(info_row("Frame Rate", &fps_text))
                            .child(div().h(px(12.0)))
                            .child(section("KEYBOARD"))
                            .child(kb("← →", "Seek ±5s"))
                            .child(kb("Space", "Play/Pause"))
                            .child(kb("M", "Mute"))
                            .child(kb("↑ ↓", "Prev/Next"))
                            .child(kb("Enter", "Load")),
                    ),
            )
            // === Status bar ===
            .child(
                div()
                    .h(px(24.0))
                    .flex()
                    .items_center()
                    .px_2()
                    .bg(rgb(0x161b22))
                    .border_t_1()
                    .border_color(rgb(0x30363d))
                    .text_xs()
                    .text_color(rgb(0x8b949e))
                    .child(self.status.clone()),
            )
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn section(title: &str) -> impl IntoElement {
    div()
        .font_weight(FontWeight::BOLD)
        .text_sm()
        .mb_2()
        .text_color(rgb(0x8b949e))
        .child(title.to_string())
}

fn info_row(label: &str, value: &str) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .justify_between()
        .child(
            div()
                .text_xs()
                .text_color(rgb(0x8b949e))
                .child(label.to_string()),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(0xe6edf3))
                .child(value.to_string()),
        )
}

fn kb(key: &str, desc: &str) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .gap_2()
        .child(
            div()
                .text_xs()
                .text_color(rgb(0x58a6ff))
                .w(px(48.0))
                .child(key.to_string()),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(0x8b949e))
                .child(desc.to_string()),
        )
}

fn format_time(d: Duration) -> String {
    let secs = d.as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}
