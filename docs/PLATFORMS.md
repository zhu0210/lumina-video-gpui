# Platform support and validation

The target-specific Cargo manifests and implementations define current support.
A successful build does not establish hardware playback coverage. Container parsing,
codec availability, hardware decoding and native-memory import are separate capabilities.

## Implemented paths

| Platform | Media path | Frame delivery | Validation boundary |
|---|---|---|---|
| Linux | GStreamer media session; bundled GStreamer 1.28.6 and FFmpeg 9 runtime available | DMA-BUF import into Vulkan/wgpu; explicit fallback if import is unavailable | Local Intel GPU playback, session regressions, and standalone package tests in a clean Ubuntu container |
| macOS | AVFoundation/VideoToolbox; FFmpeg fallback for containers AVFoundation cannot open | IOSurface native import; FFmpeg fallback may deliver CPU frames | Intel and Apple Silicon CI builds/tests; bundled application builds |
| iOS | AVFoundation/VideoToolbox with native audio | IOSurface retained through Metal GPU completion | Device and simulator builds; simulator playback, pause, seek, EOS and lifetime tests through the Swift/Metal harness |
| Android | ExoPlayer/MediaCodec; native MoQ decoder where enabled | AHardwareBuffer with producer fence and GPU YUV conversion; explicit fallback on unsupported devices | Native cross-checks, bridge unit tests and APK builds; physical-device playback remains a separate check |
| Windows | Media Foundation plus the maintained audio output | Native D3D11/DX12 import into wgpu | Windows compilation and regression tests; actual MF/WASAPI sound output and GPU playback still require device validation |
| Web | HTMLVideoElement, native HLS or hls.js; browser MoQ bridge | Browser GPU copy into WebGPU | Wasm builds, bridge regressions and browser checks; codec support depends on the browser |

Native-memory delivery is the normal path. CPU pixel upload is the final fallback,
with its cause reported; it is never counted as zero-copy. Hardware decoding alone
does not prove that frame delivery avoids CPU copies.

The iOS harness tests the Swift bridge and Metal renderer, not the complete GPUI
renderer. GPUI is checked for both iOS targets, but physical iOS/Android playback and
GPUI rendering need device validation. No device model compatibility list is claimed.

## Formats and runtime requirements

- **Linux:** the packaged runtime includes its selected demuxers and decoders. Plugin
  availability, driver capabilities, pixel format and DRM modifier support determine
  the usable route. See [runtime packaging](GSTREAMER-RUNTIME.md) for the lock file,
  contents and deployment baseline. The current standalone Linux build targets
  Ubuntu 24.04/glibc 2.39; it is not an Ubuntu 22.04 binary.
- **macOS:** AVFoundation handles supported native containers. MKV/WebM fallback uses
  the bundled FFmpeg build. Ship the packaged application, including its FFmpeg
  libraries, rather than copying the executable alone.
- **iOS:** the current AVFoundation path does not provide general MKV support. A
  GStreamer migration is still a proposal, not an implemented feature. See the
  [iOS integration guide](IOS.md) for the generated XCFramework and Swift package.
- **Android:** the maintained bridge has `minSdk = 26`. Its ExoPlayer native-memory
  route currently requires API 33 producer fences; older versions use the explicitly
  reported final fallback. Format support depends on ExoPlayer and the installed
  MediaCodec implementations. See [Android integration](ANDROID.md).
- **Windows:** available Media Foundation decoders and OS components determine
  format coverage. Bundling FFmpeg for Linux/macOS does not add a Windows FFmpeg
  fallback. Do not assume a codec extension is installed or that all MKV streams work.
- **Web:** browser codecs and media APIs determine playback coverage. The GPU copy
  path is distinct from native texture aliasing.

An MKV demuxer does not imply hardware support for every codec that MKV can contain.
Use actual media fixtures and frame-delivery diagnostics to establish coverage.

## Audio and packaging

The current Linux session uses GStreamer audio-sink selection (`autoaudiosink` by
default). Legacy environment switches for forcing ALSA/PulseAudio/PipeWire are not
part of this session's contract. Headless fixture tests explicitly select a fake sink;
that does not validate audible output.

Linux releases can bundle the runtime using the `vendored-runtime` feature. macOS
release packaging includes the pinned FFmpeg libraries. Android bundles the Java
bridge, while iOS packages the Rust static libraries and headers as an XCFramework.
The Windows backend still relies on platform media and audio services.

See the [cross-platform CI](../.github/workflows/ci.yml),
[iOS application tests](../.github/workflows/ios.yml), and
[standalone runtime tests](../.github/workflows/package-gstreamer.yml) for the checks
that are actually run. Passing them does not replace physical-device acceptance.
