//! Framework-neutral wgpu upload and native-frame import for Lumina video.
#![cfg_attr(target_family = "wasm", allow(clippy::arc_with_non_send_sync))]

#[cfg(target_os = "android")]
pub use lumina_video_core::android_video;
pub use lumina_video_core::video;

pub mod frame_to_texture;
pub mod zero_copy;

use std::sync::Arc;
use std::time::Duration;

/// Wgpu resources required by upload and import paths.
pub trait VideoWgpuContext {
    /// Returns the renderer device.
    fn device(&self) -> &wgpu::Device;

    /// Returns the renderer queue.
    fn queue(&self) -> &wgpu::Queue;

    /// Returns adapter information used for platform-device matching.
    fn adapter_info(&self) -> wgpu::AdapterInfo;
}

/// Path realized for a GPU video frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealizedVideoPath {
    /// Producer memory is sampled directly.
    ZeroCopy,
    /// A GPU-side copy was required.
    GpuCopy,
    /// CPU planes were uploaded to wgpu textures.
    CpuUpload,
    /// No supported path was available.
    Unsupported,
}

/// Texture payload of an imported or uploaded frame.
#[derive(Clone)]
pub enum GpuVideoFrameTextures {
    /// RGBA or BGRA texture.
    Rgba(Arc<wgpu::Texture>),
    /// NV12 luma and interleaved chroma textures.
    Nv12 {
        /// R8Unorm luma texture.
        y: Arc<wgpu::Texture>,
        /// Rg8Unorm chroma texture.
        cb_cr: Arc<wgpu::Texture>,
    },
}

/// Framework-neutral GPU video frame.
#[derive(Clone)]
pub struct GpuVideoFrame {
    /// GPU textures.
    pub textures: GpuVideoFrameTextures,
    /// Native width.
    pub width: u32,
    /// Native height.
    pub height: u32,
    /// Presentation timestamp.
    pub timestamp: Duration,
    /// Seek/discontinuity generation.
    pub generation: u64,
    /// Color metadata propagated from the decoder.
    pub color: lumina_video_core::video::VideoColorMetadata,
    /// Realized upload/import path.
    pub path: RealizedVideoPath,
    /// Retained producer ownership needed by imported textures.
    pub producer: Option<Arc<dyn std::any::Any + Send + Sync>>,
}
