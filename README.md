# Lumina Video for GPUI

Lumina Video is a hardware-accelerated video stack for GPUI applications. The
managed fork is organized into three layers:

- `lumina-video-core` owns decoding, clocks, bounded queues, frame scheduling,
  color metadata, synchronization metadata, and native frame handles.
- `lumina-video-wgpu` owns CPU upload and capability-gated native-frame import
  into wgpu textures.
- `lumina-video` owns GPUI players, controls, repaint scheduling, and GPUI
  surface conversion.

The repositories target wgpu 30. Native GPUI rendering is supported on Linux,
macOS, and Windows. Browser builds preserve native `HTMLVideoElement` playback,
HLS.js adaptive streaming and quality controls, and WebTransport/MoQ decoding.

## Development

This checkout expects the managed Zed/GPUI fork in an adjacent `zed` directory
while fork commits are under active development:

```text
gpui-port/
├── lumina-video-gpui/
└── zed/
```

Build and test the native crates:

```sh
cargo check -p lumina-video-core -p lumina-video-wgpu -p lumina-video
cargo test -p lumina-video-core --lib
cargo test -p lumina-video-wgpu --lib
```

Verify browser support:

```sh
rustup target add wasm32-unknown-unknown
cargo check -p lumina-video --target wasm32-unknown-unknown
cargo clippy -p lumina-video --target wasm32-unknown-unknown -- -D warnings
```

Run the GPUI desktop example:

```sh
cargo run -p lumina-video-demo --features gpui-wgpu
```

## Platform paths

- Linux prefers decoder/renderer GPU alignment and disjoint DMABuf import,
  falling back by capability when layouts or synchronization cannot be proven
  safe.
- macOS keeps native window rendering available while IOSurface video import is
  capability-gated in `lumina-video-wgpu`.
- Windows keeps CPU upload as the supported fallback until D3D11/D3D12 adapter
  matching and shared-fence import are validated.
- iOS and Android decoder bridges and native frame handles remain available for
  future GPUI mobile integration.

See [GPUI_WGPU_FORK_PLAN.md](GPUI_WGPU_FORK_PLAN.md) in managed development
checkouts for the implementation and verification status. That roadmap is
intentionally untracked until it becomes repository policy.
