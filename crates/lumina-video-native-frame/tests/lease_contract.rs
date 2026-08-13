use std::time::Duration;

use lumina_video_native_frame::video::PixelFormat;
use lumina_video_native_frame::{
    AcquireSync, CpuMemory, CpuPlane, FrameExtent, NativeFrameDescriptor, NativeFrameLease,
    NativeMemory,
};

#[test]
fn cpu_lease_owns_format_planes_and_timing() -> Result<(), Box<dyn std::error::Error>> {
    let lease = NativeFrameLease::new(
        NativeFrameDescriptor {
            frame_id: 4,
            stream_generation: 2,
            pts: Duration::from_millis(40),
            duration: Some(Duration::from_millis(33)),
            extent: FrameExtent::new(2, 1),
            format: PixelFormat::Rgba,
        },
        NativeMemory::Cpu(CpuMemory::new(vec![CpuPlane::new(vec![1, 2, 3, 4], 8)])),
        AcquireSync::None,
    )?;

    assert_eq!(lease.descriptor.frame_id, 4);
    assert_eq!(lease.descriptor.stream_generation, 2);
    assert_eq!(lease.descriptor.extent, FrameExtent::new(2, 1));
    assert!(matches!(lease.memory, NativeMemory::Cpu(_)));
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn dmabuf_planes_can_share_one_owned_memory_object() -> Result<(), Box<dyn std::error::Error>> {
    use std::fs::File;
    use std::os::fd::{FromRawFd, IntoRawFd};

    use lumina_video_native_frame::{DmaBufMemory, DmaBufObject, DmaBufPlane};

    let object = DmaBufObject {
        // SAFETY: the raw descriptor is transferred exactly once into OwnedFd.
        fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(File::open("/dev/null")?.into_raw_fd()) },
        size: 16,
    };
    let memory = DmaBufMemory::new(
        vec![object],
        vec![
            DmaBufPlane {
                object: 0,
                offset: 0,
                stride: 4,
                size: 4,
            },
            DmaBufPlane {
                object: 0,
                offset: 4,
                stride: 4,
                size: 4,
            },
        ],
        0,
        0,
    )?;

    let first = memory.planes.first().ok_or("missing first plane")?;
    let second = memory.planes.get(1).ok_or("missing second plane")?;
    assert_eq!(memory.objects.len(), 1);
    assert_eq!(memory.planes.len(), 2);
    assert_eq!(first.object, second.object);
    Ok(())
}
