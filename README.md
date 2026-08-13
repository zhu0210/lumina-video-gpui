# lumina-video

*It just works.*

[![CI](https://github.com/lumina-video/lumina-video/actions/workflows/ci.yml/badge.svg)](https://github.com/lumina-video/lumina-video/actions/workflows/ci.yml)

> **Experimental:** Linux and macOS GPUI playback plus the iOS FFI are under active development.

Lumina Video provides hardware-accelerated playback and native-frame delivery for embedded applications. `lumina-video-gpui` is the sole Rust UI/player entry point; the lower-level `core`, `native-frame`, `gst`, and `wgpu` crates are implementation boundaries.

## Rust entry point

```toml
[dependencies]
lumina-video-gpui = { git = "https://github.com/lumina-video/lumina-video" }
```

```rust
use lumina_video_gpui::GpuiVideoPlayer;

let player = GpuiVideoPlayer::new("https://example.com/video.mp4")
    .with_autoplay(true);
```

The player is rendered through GPUI's `surface()` element. Native decoders and the wgpu frame-import boundary remain behind the GPUI crate.

## Running the demo

```bash
cargo run --package lumina-video-demo
```

On Windows, enable the opt-in native decoder:

```bash
cargo run --package lumina-video-demo --features windows-native-video
```

## Linux

Install GStreamer development libraries and runtime plugins, then run the demo:

```bash
sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav \
  gstreamer1.0-vaapi
cargo run --package lumina-video-demo
```

For a bundled GStreamer runtime on Ubuntu 24.04+:

```toml
[dependencies]
lumina-video-gpui = {
    git = "https://github.com/lumina-video/lumina-video",
    features = ["vendored-runtime"],
}
```

The Nix development shell and pre-built Linux packages are also available:

```bash
nix develop github:lumina-video/lumina-video
nix run github:lumina-video/lumina-video
```

See [GitHub Releases](https://github.com/lumina-video/lumina-video/releases) for `.deb`, `.rpm`, and Flatpak artifacts.

## macOS

VideoToolbox handles native playback. Install FFmpeg for MKV/WebM support, then run:

```bash
./scripts/setup-macos-ffmpeg.sh
export SDKROOT=$(xcrun --sdk macosx --show-sdk-path)
cargo run --package lumina-video-demo
```

## iOS

The iOS FFI and Swift package remain separate from the GPUI entry point. They use the native-frame player and IOSurface/Metal delivery:

```bash
./ios/build-ios.sh sim
./ios/build-ios.sh device
```

See [docs/IOS.md](docs/IOS.md) and [docs/ios-ffi-contract.md](docs/ios-ffi-contract.md) for the Swift integration and test harness.

## Features

| Feature | Purpose |
|---------|---------|
| `moq` | Media over QUIC live streaming (experimental) |
| `vendored-runtime` | Bundle GStreamer libraries on Linux |
| `windows-native-video` | Enable Windows Media Foundation/DXVA2 playback |

Enable features on `lumina-video-gpui`; the demo forwards the same supported flags.

## Architecture

```text
lumina-video-demo → lumina-video-gpui
                         ├─ lumina-video-core
                         ├─ lumina-video-native-frame
                         ├─ lumina-video-wgpu
                         └─ lumina-video-gst (Linux)
```

Frames use native memory where the platform and renderer support it. On unsupported paths, the explicit frame tier falls back to system-memory upload without changing the GPUI entry point.

## License

MIT OR Apache-2.0.
