use std::time::Duration;

use lumina_video_native_frame::video::PixelFormat;
use lumina_video_native_frame::{
    AcquireSync, ChromaHorizontal, ChromaVertical, ColorMatrix, ColorMetadata, ColorPrimaries,
    ColorRange, ColorTransfer, CpuMemory, CpuPlane, FrameExtent, NativeFrameDescriptor,
    NativeFrameLease, NativeMemory,
};

#[test]
fn cpu_lease_owns_format_planes_and_timing() -> Result<(), Box<dyn std::error::Error>> {
    let color = ColorMetadata {
        matrix: ColorMatrix::Bt709,
        primaries: ColorPrimaries::Bt709,
        transfer: ColorTransfer::Bt709,
        range: ColorRange::Limited,
        chroma_horizontal: ChromaHorizontal::Cosited,
        chroma_vertical: ChromaVertical::Centered,
    };
    let lease = NativeFrameLease::new(
        NativeFrameDescriptor {
            frame_id: 4,
            stream_generation: 2,
            pts: Duration::from_millis(40),
            duration: Some(Duration::from_millis(33)),
            extent: FrameExtent::new(2, 1),
            format: PixelFormat::Rgba,
            color,
            color_transform: None,
        },
        NativeMemory::Cpu(CpuMemory::new(vec![CpuPlane::new(vec![1, 2, 3, 4], 8)])),
        AcquireSync::None,
    )?;

    assert_eq!(lease.descriptor.frame_id, 4);
    assert_eq!(lease.descriptor.stream_generation, 2);
    assert_eq!(lease.descriptor.extent, FrameExtent::new(2, 1));
    assert_eq!(lease.descriptor.color, color);
    assert!(matches!(lease.memory, NativeMemory::Cpu(_)));
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn dmabuf_format_planes_can_share_one_memory_view() -> Result<(), Box<dyn std::error::Error>> {
    use std::fs::File;
    use std::os::fd::{AsRawFd, OwnedFd};

    use lumina_video_native_frame::{
        DmaBufFormatPlane, DmaBufMemory, DmaBufMemoryPlane, DmaBufObject,
    };

    let object = DmaBufObject {
        fd: OwnedFd::from(File::open("/dev/null")?),
        size: Some(16),
    };
    let memory = DmaBufMemory::new(
        vec![object],
        vec![DmaBufMemoryPlane {
            object: 0,
            offset: 0,
            size: Some(8),
        }],
        vec![
            DmaBufFormatPlane {
                memory_plane: 0,
                offset: 4,
                stride: 4,
                size: Some(4),
            },
            DmaBufFormatPlane {
                memory_plane: 0,
                offset: 0,
                stride: 4,
                size: Some(4),
            },
        ],
        Some(0x3231564e),
        Some(0),
    )?;

    let first = memory
        .format_planes
        .first()
        .ok_or("missing first format plane")?;
    let second = memory
        .format_planes
        .get(1)
        .ok_or("missing second format plane")?;
    assert_eq!(memory.objects.len(), 1);
    assert_eq!(memory.memory_planes.len(), 1);
    assert_eq!(memory.format_planes.len(), 2);
    assert_eq!(first.memory_plane, second.memory_plane);
    let object = memory.objects.first().ok_or("missing object")?;
    assert_eq!(object.size, Some(16));
    assert!(object.fd.as_raw_fd() >= 0);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn dmabuf_views_can_share_fd_or_use_disjoint_objects() -> Result<(), Box<dyn std::error::Error>> {
    use std::fs::File;
    use std::os::fd::{AsRawFd, OwnedFd};

    use lumina_video_native_frame::{
        DmaBufFormatPlane, DmaBufMemory, DmaBufMemoryPlane, DmaBufObject,
    };

    let file = File::open("/dev/null")?;
    let fd = file.as_raw_fd();
    let shared = DmaBufObject {
        fd: OwnedFd::from(file),
        size: Some(32),
    };
    let other = DmaBufObject {
        fd: OwnedFd::from(File::open("/dev/null")?),
        size: Some(16),
    };
    let other_fd = other.fd.as_raw_fd();
    let memory = DmaBufMemory::new(
        vec![shared, other],
        vec![
            DmaBufMemoryPlane {
                object: 0,
                offset: 0,
                size: Some(8),
            },
            DmaBufMemoryPlane {
                object: 1,
                offset: 0,
                size: Some(8),
            },
            DmaBufMemoryPlane {
                object: 0,
                offset: 8,
                size: Some(8),
            },
        ],
        vec![
            DmaBufFormatPlane {
                memory_plane: 0,
                offset: 0,
                stride: 4,
                size: Some(4),
            },
            DmaBufFormatPlane {
                memory_plane: 1,
                offset: 0,
                stride: 4,
                size: Some(4),
            },
        ],
        Some(0x3231564e),
        Some(0),
    )?;
    let lease = NativeFrameLease::new(
        NativeFrameDescriptor {
            frame_id: 5,
            stream_generation: 1,
            pts: Duration::ZERO,
            duration: None,
            extent: FrameExtent::new(2, 1),
            format: PixelFormat::Nv12,
            color: lumina_video_native_frame::ColorMetadata::default(),
            color_transform: None,
        },
        NativeMemory::DmaBuf(memory),
        AcquireSync::None,
    )?;
    let NativeMemory::DmaBuf(memory) = lease.memory else {
        return Err("expected DMABuf memory".into());
    };

    assert_eq!(memory.objects.len(), 2);
    assert_eq!(memory.memory_planes.len(), 3);
    assert_eq!(memory.format_planes.len(), 2);
    let shared_object = memory.objects.first().ok_or("missing shared object")?;
    let other_object = memory.objects.get(1).ok_or("missing other object")?;
    assert_eq!(shared_object.fd.as_raw_fd(), fd);
    assert_eq!(other_object.fd.as_raw_fd(), other_fd);
    assert_ne!(shared_object.fd.as_raw_fd(), other_object.fd.as_raw_fd());
    let first_memory = memory
        .memory_planes
        .first()
        .ok_or("missing first memory plane")?;
    let second_memory = memory
        .memory_planes
        .get(1)
        .ok_or("missing second memory plane")?;
    assert_eq!(first_memory.object, 0);
    assert_eq!(second_memory.object, 1);
    let shared_view = memory
        .memory_planes
        .get(2)
        .ok_or("missing shared memory view")?;
    assert_eq!(shared_view.object, 0);
    assert_eq!(shared_view.offset, 8);
    let first_format = memory
        .format_planes
        .first()
        .ok_or("missing first format plane")?;
    let second_format = memory
        .format_planes
        .get(1)
        .ok_or("missing second format plane")?;
    assert_eq!(first_format.memory_plane, 0);
    assert_eq!(second_format.memory_plane, 1);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn invalid_memory_and_format_plane_references_are_rejected(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::fs::File;
    use std::os::fd::OwnedFd;

    use lumina_video_native_frame::{
        DmaBufFormatPlane, DmaBufMemory, DmaBufMemoryPlane, DmaBufObject, NativeFrameError,
    };

    let object = || -> Result<DmaBufObject, std::io::Error> {
        let file = File::open("/dev/null")?;
        Ok(DmaBufObject {
            fd: OwnedFd::from(file),
            size: Some(8),
        })
    };
    let memory_error = DmaBufMemory::new(
        vec![object()?],
        vec![DmaBufMemoryPlane {
            object: 1,
            offset: 0,
            size: Some(1),
        }],
        Vec::new(),
        Some(0),
        Some(0),
    );
    assert!(matches!(
        memory_error,
        Err(NativeFrameError::MemoryPlaneObject { .. })
    ));

    let format_error = DmaBufMemory::new(
        vec![object()?],
        vec![DmaBufMemoryPlane {
            object: 0,
            offset: 0,
            size: Some(1),
        }],
        vec![DmaBufFormatPlane {
            memory_plane: 1,
            offset: 0,
            stride: 1,
            size: Some(1),
        }],
        Some(0),
        Some(0),
    );
    assert!(matches!(
        format_error,
        Err(NativeFrameError::FormatPlaneMemory { .. })
    ));

    let memory_bounds = DmaBufMemory::new(
        vec![object()?],
        vec![DmaBufMemoryPlane {
            object: 0,
            offset: 7,
            size: Some(2),
        }],
        Vec::new(),
        Some(0),
        Some(0),
    );
    assert!(matches!(
        memory_bounds,
        Err(NativeFrameError::MemoryPlaneOutOfBounds { .. })
    ));

    let format_bounds = DmaBufMemory::new(
        vec![object()?],
        vec![DmaBufMemoryPlane {
            object: 0,
            offset: 0,
            size: Some(2),
        }],
        vec![DmaBufFormatPlane {
            memory_plane: 0,
            offset: 1,
            stride: 1,
            size: Some(2),
        }],
        Some(0),
        Some(0),
    );
    assert!(matches!(
        format_bounds,
        Err(NativeFrameError::FormatPlaneOutOfBounds { .. })
    ));
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn dmabuf_lease_move_arc_clone_and_final_fd_cleanup() -> Result<(), Box<dyn std::error::Error>> {
    use std::fs::File;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::sync::Arc;

    use lumina_video_native_frame::{
        DmaBufFormatPlane, DmaBufMemory, DmaBufMemoryPlane, DmaBufObject,
    };

    let file = File::open("/dev/null")?;
    let raw_fd = file.as_raw_fd();
    let memory = DmaBufMemory::new(
        vec![DmaBufObject {
            fd: OwnedFd::from(file),
            size: Some(4),
        }],
        vec![DmaBufMemoryPlane {
            object: 0,
            offset: 0,
            size: Some(4),
        }],
        vec![DmaBufFormatPlane {
            memory_plane: 0,
            offset: 0,
            stride: 4,
            size: Some(4),
        }],
        Some(0x34325241),
        Some(0),
    )?;
    let lease = NativeFrameLease::new(
        NativeFrameDescriptor {
            frame_id: 9,
            stream_generation: 1,
            pts: Duration::ZERO,
            duration: None,
            extent: FrameExtent::new(1, 1),
            format: PixelFormat::Rgba,
            color: lumina_video_native_frame::ColorMetadata::default(),
            color_transform: None,
        },
        NativeMemory::DmaBuf(memory),
        AcquireSync::None,
    )?;

    fn move_lease(lease: NativeFrameLease) -> Arc<NativeFrameLease> {
        Arc::new(lease)
    }

    let owner = move_lease(lease);
    let clone = Arc::clone(&owner);
    drop(owner);
    // SAFETY: raw_fd was borrowed from the still-live OwnedFd held by clone.
    assert!(unsafe { libc::fcntl(raw_fd, libc::F_GETFD) } >= 0);
    drop(clone);
    // SAFETY: querying a descriptor after its owner drops is the intentional
    // EBADF cleanup assertion; no Rust reference is formed from raw_fd.
    assert_eq!(unsafe { libc::fcntl(raw_fd, libc::F_GETFD) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EBADF)
    );
    Ok(())
}
