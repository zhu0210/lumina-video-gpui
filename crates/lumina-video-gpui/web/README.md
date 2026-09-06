# Browser video

`GpuiWebVideoPlayer` keeps decoding, timing, and audio in `HTMLVideoElement`.
HLS uses the browser on Safari and bundled HLS.js elsewhere. WebGPU copies
browser frames directly to GPUI's texture; no CPU pixel readback is used.
This is GPU conversion/copy, not native external-memory aliasing.

From this directory:

```sh
npm ci
npm run build
RUSTUP_TOOLCHAIN=nightly trunk serve
```

Open http://127.0.0.1:8080/. The default source is HLS; `?url=<encoded-url>`
selects an MP4 or HLS source. The server includes the isolation headers required
by GPUI. A browser with WebGPU is required. Playback begins muted according to
browser autoplay rules; applications can unmute in response to a user gesture.

Nightly is required by GPUI's `wasm_thread` dependency. Install its
`wasm32-unknown-unknown` target if missing. To check without serving:

```sh
cargo +nightly check -p lumina-video-gpui --target wasm32-unknown-unknown --bin web_hls
npm test
```

`WebMoqSession` preserves the browser MoQ transport, WebCodecs video,
AudioWorklet audio, catalog discovery, and track-selection APIs. Use its
`connect`, `start_playback`, `poll_state`, and `copy_to_wgpu_texture` methods
with the GPUI renderer's device/queue. Its transport requires the bundled
`MoqNet` global and `moq-audio-worklet.js`; the example HTML includes both.
The pinned `@moq/net` 0.3.4 package supplies the current transport API.
Subscriptions preserve priority, and frame payloads use its structured frame API.
The HLS example does not claim a live MoQ relay acceptance test.

The JS callback regression verifies EOS/replay registration and cancellation
before Rust closure destruction. Native `VideoFrame`/HTMLVideoElement resources
remain browser-owned and should be closed through the provided session/player
lifecycle APIs.
