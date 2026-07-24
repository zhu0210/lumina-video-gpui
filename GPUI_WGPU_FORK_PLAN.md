# GPUI wgpu Fork Cleanup and Optimization Plan

Status updated: 2026-07-24

This file tracks the managed-fork roadmap and the implementation state shared
by the Zed, Lumina, and gpui-mobile forks.

Implementation commits:

- Zed fork `6051b7f967` — wgpu 30 synchronization, coherent context API,
  validated surface sources, ordered rendering, clipping, and color matrices.
- Zed fork `5e25352f27` — renderer-adapter DRM render-node resolution.
- Zed fork `f1a47d59d1` — shared dynamic surface parameters and bounded
  weak-owner texture-view/bind-group caches.
- Zed fork `3e6755081f` — removed the public GPUI macOS wgpu frame-import API.
- Zed fork `611b585675` — validate every supported YCbCr matrix/range pair
  against independent reference equations.
- Zed fork `161bcba7c8` — apply sRGB, BT.709, and BT.2020 transfer metadata in
  the NV12 shader with reference and uniform-layout tests.
- Lumina fork `324e1e9` — single-poll scheduler integration.
- Lumina fork `ce1f157` — `lumina-video-wgpu` split, frame metadata, DMABuf
  ownership, and recurring stall-log removal.
- Lumina fork `0607a9b` — automatic GPUI renderer/decoder GPU alignment.
- Lumina fork `39b2eda` — scheduled `GpuVideoFrame` metadata/lifetime
  propagation and state-aware repaint requests.
- Lumina fork `6ba09f5` — preserve browser-native video/HLS decoding while replacing
  the egui renderer bridge with a GPUI/WebGPU surface integration.
- Lumina fork `26367fc` — scope VA-API, NVIDIA, and software decoder selection and
  render-node configuration to individual Linux GStreamer pipelines.
- Lumina fork `eb42656` — validate DMABuf plane layouts, keep import descriptors
  RAII-owned, and reject unsafe shared-allocation or unsynchronized imports.
- Lumina fork `a2941cb` — remove remaining active egui code and the retired Android
  demo crate while preserving browser HLS/MoQ and native mobile decoder bridges.
- Lumina fork `9243c3a` — expose explicit macOS/Windows interop capabilities, validate
  macOS BGRA-only import, and stop advertising unsynchronized Windows handles.
- Lumina fork `3c96833` — inject a deterministic scheduler clock, initialize the
  playback clock correctly, drop late frames internally, and prevent held-frame
  underrun accounting.
- Lumina fork `0bd37a5` — skip read-only GStreamer device properties; its
  temporary Intel/AMD fallback was superseded after runtime diagnosis.
- Lumina fork `b78cf4d` — start browser video-frame callbacks and add a compact
  GPUI WebGPU example for HLS and direct video URLs.
- Lumina fork `d59f6fd` — restore shared-allocation NV12 DMABuf zero-copy using
  independently owned descriptor imports with explicit per-plane layouts.
- Lumina fork `b12def7` — count realized wgpu import/upload paths and expose
  zero-copy, CPU-upload, and import-failure counters through the GPUI player.
- Lumina fork `ee9cdc9` — enforce bounded live drop-oldest and VOD producer
  backpressure policies with deterministic tests.
- Lumina fork `6274a51` — publish GStreamer's pipeline position to the
  scheduler and use the decoder/audio clock as Linux playback master.
- Lumina fork `a3e1bbb` — keep normal future-frame holds out of reject recovery,
  report clock fallback once per transition, and admit the one-frame EOS tail.
- Lumina fork `6e8f0ea` — make the GPUI demo progress track clickable and
  draggable with a visible seek thumb and bounded target conversion.
- Lumina fork `3348cc2` — validate single- and multi-plane DMABuf layouts before
  HAL access and cover malformed metadata plus owned-descriptor cleanup.
- Lumina fork `7f03282` — remove the disabled raw-FD single-allocation importer
  so all Linux multi-plane imports use the validated RAII path.

## Completed

### Frame scheduling: GPUI poll semantics

- Added `FramePollResult` with `NewFrame`, `Hold`, `Buffering`, and
  `EndOfStream`.
- Added a monotonic scheduler presentation generation so consumers can
  distinguish a newly selected frame from a retained frame without inferring
  from queue length.
- Changed `GpuiVideoPlayer` to poll exactly once per update.
- Upload/import now runs only for `NewFrame`; held frames retain their existing
  GPU textures.
- Removed the 16-poll draining loop, duplicate scheduler warning block, and
  related warning timestamp.
- Kept `poll_frame() -> Option<VideoFrame>` temporarily as a compatibility
  adapter for non-GPUI and FFI consumers.
- Added deterministic regression tests for:
  - new-frame versus hold behavior;
  - preservation of a queued future frame;
  - buffering with an empty non-EOS queue;
  - end-of-stream with an empty EOS queue.

Verification completed:

- `cargo test -p lumina-video-core --lib` — 53 passed after the crate split and
  generation/DMABuf changes.
- `cargo test -p lumina-video --lib` — passed.
- `cargo check -p lumina-video` — passed.
- `cargo check -p lumina-video-core -p lumina-video-wgpu -p lumina-video` —
  passed.
- `cargo test -p gpui_wgpu --lib -- --test-threads=1` — 27 passed, including
  surface color, transfer, and clipping coverage. Serial execution avoids the
  headless environment's earlier parallel atlas/font-test instability.
- Focused BT.601/709/2020 full/limited-range reference tests — passed.
- `git diff --check` — passed.
- `cargo check -p lumina-video --target wasm32-unknown-unknown` — passed with
  browser video, HLS.js, and MoQ support enabled.
- `cargo clippy -p lumina-video --target wasm32-unknown-unknown -- -D warnings`
  — passed.
- Native `cargo check` passed for `lumina-video`, `lumina-video-core`, and
  `lumina-video-wgpu` after the web integration changes.

### Web/WASM and HLS support

- Browser-native `HTMLVideoElement` playback and HLS.js quality/buffer controls
  remain supported on `wasm32`.
- Added `GpuiWebVideoPlayer`, which uploads decoded browser frames into a
  texture owned by GPUI's wgpu device and exposes it through GPUI's validated
  RGBA surface source.
- The browser remains responsible for decode, adaptive HLS streaming, timing,
  and audio; the GPUI integration requests animation frames only while loading
  or playing.
- `GpuiWebVideoPlayer` now starts the browser's recurring
  `requestVideoFrameCallback` loop and refreshes playback state and metadata
  before deciding whether GPUI needs another animation frame.
- Preserved the WebTransport/MoQ decoder path.
- Removed the old egui web demo application. A compact GPUI web/HLS example is
  now provided as `web_hls`; it defaults to a public HLS test stream and accepts
  direct MP4/HLS URLs through `?url=`.
- `RUSTC_BOOTSTRAP=1 cargo check -p lumina-video --example web_hls --target
  wasm32-unknown-unknown` and the matching clippy command with `-D warnings`
  pass. GPUI's current `wasm_thread` dependency requires its nightly feature
  when targeting WASM.
- A full `cargo build` and `wasm-bindgen` packaging pass also succeeds. On the
  current test device, HLS.js reaches `readyState=4`, playback time advances,
  and WebGPU initializes, but the GPUI canvas remains black. The official GPUI
  web example also does not render on this device, so browser presentation
  debugging is deliberately deferred rather than weakening or removing
  WASM/HLS support.

## Partially Complete

### wgpu 30 baseline

- Both adjacent forks declare wgpu 30 and the Zed synchronization is committed.
- Linux checks pass against the adjacent checkout.
- Cross-platform build-matrix verification and publishing/pinning the Zed
  commit remain outstanding. A remote git pin cannot be made reproducible until
  `3e6755081f` is pushed to the managed Zed remote.

### Scheduler API migration

- GPUI uses the explicit poll result.
- Other consumers still use the compatibility `poll_frame()` adapter.
- Seek/discontinuity generation is attached to emitted `VideoFrame` values and
  carried by the new `GpuVideoFrame` representation.
- The scheduler uses an injectable monotonic clock. Deterministic tests cover
  VOD timestamp holds, one-poll late-frame dropping, seek generation,
  pause/resume position, buffering/EOS, and false-underrun prevention.
- Fixed first-frame clock initialization: the waiting flag was previously
  cleared before `on_frame_received`, preventing the clock from starting.
- Recurring underrun/decode/network stall warnings were removed; metrics remain
  counter-based.

### GPUI wgpu backend API

- Added private-field `WgpuContextHandle` accessors and coherent
  `WgpuContextDescriptor { instance, adapter, device, queue }` injection.
- Removed device-only adapter guessing.
- New window surfaces validate the injected adapter and return a typed
  compatibility error.
- Adapter info, identity placeholders, and import capability reporting are
  exposed. Linux DRM-node resolution and Windows LUID population remain.
- Tuple conversions were replaced by validated RGBA/BGRA/NV12 source types
  carrying alpha and color metadata.
- A deprecated `GpuContextHandle` alias remains temporarily for source
  compatibility.
- Remove wgpu IOSurface import from GPUI.

### Renderer optimization and correctness

- Mixed RGBA/NV12 scene order is now preserved.
- Scissors are clamped for negative and partially offscreen masks, and empty
  intersections are skipped.
- NV12 conversion selects BT.601/709/2020, full/limited range, and
  sRGB/BT.709/BT.2020 transfer behavior from frame metadata.
- Per-frame surface diagnostics were removed from the touched paths.
- RGBA and NV12 use one aligned dynamic parameter buffer.
- Texture views live with validated submitted sources; bind groups use bounded
  weak-owner renderer caches, and draw-list capacity is retained across frames.
- Remove remaining unconditional/per-frame diagnostics.
- Add allocator-instrumented proof for held-frame repaint. The implementation
  no longer creates buffers, views, bind groups, or a new draw vector on a
  warmed unchanged frame, but this still needs measurement coverage.
- BT.601, BT.709, and BT.2020 now have full- and limited-range coverage against
  independent reference equations, including neutral black/white endpoints.
- Complete and retain compact RGBA, NV12, and offscreen-3D examples.

### Lumina crate boundaries

- Created `lumina-video-wgpu` and removed wgpu dependencies from
  `lumina-video-core`.
- Moved CPU texture upload and platform import modules into the new crate.
- Added the framework-neutral `VideoWgpuContext` trait, `GpuVideoFrame`,
  retained producer ownership, color/sync metadata, and realized-path
  reporting. The GPUI path now consumes `GpuVideoFrame`; legacy texture helpers
  remain internal to the uploader and can be collapsed later.
- Restrict `lumina-video` to GPUI UI/player integration.
- The old egui desktop player/rendering modules and web demo were removed in
  `6ba09f5`; dormant adapter code is now physically removed from
  `web_video.rs`.
- The retired egui Android Rust application crate is removed. Android decoder,
  MediaCodec, AHardwareBuffer, JNI, and Java bridge code remain for future GPUI
  mobile integration.
- Active crate code, manifests, packaging summaries, and primary contributor
  documentation now use GPUI/framework-independent terminology. Historical
  investigation documents and the old Android application project still need
  archival or removal.
- Keep browser/WASM support, including native browser playback, HLS.js controls,
  MoQ, and GPUI/WebGPU texture presentation.
- Replace adjacent-checkout path dependencies with the pushed managed-fork
  commit and developer-local Cargo patches.
- Unused LiveKit and notify workspace patches, one dead cached-upload helper,
  and the disabled duplicate raw-FD DMABuf importer were removed; further
  dead/eager upload auditing remains.

### Remaining scheduler behavior

- Native-integrated decoder paths now publish `current_time()` through the
  decode thread and use that position as the scheduler master clock. Linux
  GStreamer queries the pipeline position; the deterministic wall clock remains
  the declared fallback when a decoder cannot expose a native position.
- Bounded VOD producer backpressure and live drop-oldest behavior are
  implemented and covered. Live evictions are exposed as a cumulative counter.
- Normal timestamp holds no longer open reject windows, trigger burst recovery,
  or emit warnings. Missing-audio fallback is a one-shot transition warning,
  and a short final-frame/audio-clock difference at EOS is handled directly.
  Continue auditing unrelated decoder/network transition logs.
- Held frames no longer enter buffering or increment underrun/stall metrics.
- VOD uses a tight timestamp tolerance; live adaptive tolerance remains tied to
  explicit frame-rate pacing.
- Playback requests animation frames while loading/playing; the unconditional
  demo timer was removed, and paused/buffering states do not spin.
- The GPUI demo exposes a visible seek thumb. Clicking the track seeks
  immediately; dragging previews the target and commits it on release. Live or
  zero-duration streams remain non-seekable, and target clamping is unit-tested.

### Linux interop

- Decoder GPU identity is derived from the GPUI wgpu context, and Linux resolves
  its DRM render node by matching wgpu PCI vendor/device identity in sysfs.
- GStreamer decoder choice is now installed through each `uridecodebin`'s
  `autoplug-select` callback. VA-API, NVIDIA, and software-only policies do not
  mutate registry ranks, and render-node properties are applied to elements
  within that pipeline without setting `GST_VA_DRM_DEVICE`.
- Unit coverage verifies that each decoder policy admits the intended hardware
  family and software fallback.
- DMABuf planes now use shared `OwnedFd` RAII ownership; the wgpu boundary
  explicitly duplicates descriptors and retains `OwnedFd` ownership until
  Vulkan successfully consumes each descriptor.
- Core and importer validation now cover dimensions, plane counts, strides,
  offsets, computed sizes, allocation bounds where reported, modifier presence,
  and consistency of single-FD metadata.
- Disjoint multi-FD and shared-allocation planes are importable as separate
  sampled textures. Shared planes use independently owned duplicated
  descriptors and retain their explicit DRM offsets, strides, sizes, and
  modifier through Vulkan image creation.
- Intel VA-API shared-allocation NV12 was runtime-verified with the Tile4
  modifier: both planes imported zero-copy, GPUI uploaded the first frame, and
  no CPU fallback or import failure occurred. This restores the working
  pre-refactor behavior while retaining the newer RAII and layout validation.
- `VideoImportStats` now counts zero-copy only after wgpu returns usable
  textures. Decoder-produced DMABufs, realized zero-copy imports, CPU uploads,
  and failed imports are no longer conflated; the GPUI demo displays the
  realized counters.
- Only verified implicit synchronization is currently accepted. Missing or
  explicit sync-file metadata is rejected without blocking; Vulkan sync-file
  semaphore import remains outstanding.
- Add Intel, AMD, NVIDIA, hybrid-GPU, modesetting, and software fallbacks.
- A Linux Intel/`prime-run` smoke test now reaches `First video frame uploaded
  to GPU` through successful shared-DMABuf imports; it also verifies that
  read-only `GstVaPostProc.device-path` is ignored instead of panicking and
  that GStreamer's advancing position becomes the playback master clock.
  Repeated normal frame-hold warnings are absent in the current run. AMD,
  NVIDIA, hybrid-GPU, and modesetting smoke coverage remains.
- A repeat smoke attempt from the restricted agent environment cannot open a
  Wayland compositor (`NoCompositor`); the successful zero-copy evidence above
  is from the user's real desktop session and remains the authoritative runtime
  result.

### Android GPUI mobile integration

- `gpui-mobile` now uses the managed GPUI wgpu fork and Lumina's GPUI player
  instead of the retired native platform-view video player.
- GPUI exposes its Android wgpu context and enables the Vulkan extensions
  required for AHardwareBuffer import when supported by the selected adapter.
- ExoPlayer/Media3 supports H.264/AV1 MP4, WebM, and HLS sources. HLS uses a
  monotonic live timeline and reports an unknown duration instead of exposing
  the current segment duration as the whole stream.
- Android decoded frames retain Java `Image` ownership only until the bounded
  Vulkan conversion fence completes. ImageReader, JNI global references,
  HardwareBuffer references, and fence descriptors are released on success,
  drop, import failure, player switch, and shutdown paths.
- Decode, JNI, scheduler, and Vulkan queues are bounded. Superseded live frames
  are dropped instead of consuming every ImageReader slot.
- ImageReader surface replacement keeps the previous reader alive through the
  MediaCodec handoff, avoiding abandoned-buffer and released-codec races.
- Player initialization, autoplay, repeated source switching, handler-thread
  shutdown, and host-native-library loading are coordinated by the bridge.
- Android frame timestamps are anchored to ExoPlayer's native audio position.
  HLS discontinuities remain monotonic; VOD drift larger than roughly one
  frame is re-anchored rather than entering recurring multi-second catch-up
  windows.
- GPU video compositing restores the full-target scissor before later GPUI
  batches, so text and controls painted after a video remain visible.
- The Android demo uses working Lorem Video H.264, AV1, WebM, and live HLS
  sources and a GPUI-native video surface.
- A release Rust library and debug APK build pass for arm64-v8a/API 26. Device
  testing confirms smooth sustained playback after the timestamp and lifetime
  fixes; broader vendor/API coverage remains outstanding.

### macOS and Windows follow-up

- `MacOsInteropCapabilities` reports BGRA IOSurface import only when the wgpu
  device exposes Metal. NV12 remains explicitly false, and non-BGRA surfaces
  are rejected before import.
- `WindowsInteropCapabilities` reports CPU upload as the only supported path.
  Shared-handle opening is no longer advertised as available without adapter
  LUID matching and shared-fence synchronization.
- BGRA IOSurface import retains the decoded frame/producer through
  `GpuVideoFrame`.
- Implement NV12 IOSurface plane import and map VideoToolbox attachments.
- Match Windows D3D11/D3D12 adapters by LUID and implement a synchronized
  bounded shared-texture ring.
- Keep typed CPU-upload fallback until platform zero-copy tests pass.
- Preserve iOS decoder glue for future IOSurface integration without restoring
  egui applications.

### Acceptance coverage

- Complete native/wgpu platform build matrices.
- Add GPUI context, surface validation, z-order, clipping, device-loss, resize,
  and offscreen-compositing tests.
- DMABuf import tests cover zero dimensions, unsupported formats, invalid
  modifiers, plane counts, odd NV12 geometry, short strides/sizes,
  out-of-allocation ranges, offset overflow, and owned-FD cleanup after
  validation failure. GPU/HAL partial-allocation failure injection remains.
- Matrix, range, and transfer-function rendering now have reference coverage.
  Wider-gamut primary conversion and HDR tone mapping are outside the current
  SDR surface contract and remain future capability work.
- Add platform smoke and sustained-playback tests.
- Add measurements for copies, blocking, allocations, bounded memory, drops,
  fallback paths, CPU/GPU cost, and A/V drift.
  Realized zero-copy/CPU-upload/import-failure counters are complete; the
  remaining CPU/GPU cost, allocation, memory, blocking, and A/V drift
  measurements are outstanding.

## Not Yet Implemented

- Allocator-instrumented verification of the renderer hot path.
- Cross-driver validation of the restored shared-allocation separate-plane
  Vulkan import, plus explicit sync-file semaphore import. A native
  multi-planar VkImage remains an optional follow-up if a driver cannot expose
  plane-compatible single-channel images.
- NV12 IOSurface plane import and VideoToolbox color-attachment mapping. GPUI's
  duplicate wgpu importer is removed and Lumina's BGRA importer is
  capability-gated.
- Windows LUID matching, shared-texture ring, and GPU synchronization.
- Archive/remove historical egui investigation material and the old Android
  application project; active Rust crates, examples, exports, and dependencies
  are already removed. The pinned managed-fork dependency policy and full
  cross-platform build/performance matrix remain.
- Replace adjacent-checkout Zed and Lumina paths in gpui-mobile with pinned
  managed-fork revisions once the current coordinated fork commits are
  published. Adjacent paths remain the supported developer layout for this
  integration checkpoint.
- Complete Android smoke coverage across Qualcomm, Mali, and older API levels,
  including sustained HLS, rapid source switching, background/restore, and
  explicit CPU-upload fallback where AHardwareBuffer import is unavailable.
- Complete GPUI browser presentation/runtime verification on a device where
  the official GPUI web example renders. HLS decode and time advancement are
  confirmed on the current device, but the canvas is black; direct-video and
  cross-browser presentation remain outstanding. WASM/HLS support is retained.
