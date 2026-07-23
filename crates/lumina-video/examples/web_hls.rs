#![cfg_attr(target_family = "wasm", no_main)]

#[cfg(target_family = "wasm")]
mod web {
    use gpui::{
        div, prelude::*, px, rgb, size, App, Bounds, Context, Window, WindowBounds, WindowOptions,
    };
    use lumina_video::GpuiWebVideoPlayer;

    const DEFAULT_HLS_URL: &str = "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8";

    pub struct WebHlsExample {
        player: Result<GpuiWebVideoPlayer, String>,
    }

    impl WebHlsExample {
        fn new() -> Self {
            let url = web_sys::window()
                .and_then(|window| window.location().search().ok())
                .and_then(|query| {
                    let value = query.strip_prefix("?url=")?;
                    js_sys::decode_uri_component(value).ok().map(String::from)
                })
                .unwrap_or_else(|| DEFAULT_HLS_URL.to_owned());
            let player = GpuiWebVideoPlayer::new(&url)
                .map_err(|error| error.to_string())
                .and_then(|mut player| {
                    player
                        .player_mut()
                        .play()
                        .map(|()| player)
                        .map_err(|error| error.to_string())
                });
            Self { player }
        }
    }

    impl Render for WebHlsExample {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if let Ok(player) = &mut self.player {
                if let Err(error) = player.update(window) {
                    self.player = Err(error.to_string());
                    cx.notify();
                }
            }

            let content = match &self.player {
                Ok(player) => player.surface_element(),
                Err(error) => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(0xff8080))
                    .child(error.clone())
                    .into_any_element(),
            };

            div().size_full().bg(rgb(0x0d1117)).child(content).child(
                div()
                    .absolute()
                    .left_4()
                    .bottom_4()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(0x161b22))
                    .text_color(rgb(0xe6edf3))
                    .child("HLS by default; use ?url=<MP4-or-HLS-URL> for direct playback"),
            )
        }
    }

    pub fn run() {
        gpui_platform::application().run(|cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(960.0), px(540.0)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| cx.new(|_| WebHlsExample::new()),
            )
            .expect("open GPUI web video window");
            cx.activate(true);
        });
    }
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    eprintln!("web_hls is a WASM/WebGPU example; build it for wasm32-unknown-unknown");
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    gpui_platform::web_init();
    web::run();
}
