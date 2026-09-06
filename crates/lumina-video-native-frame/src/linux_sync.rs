//! Linux explicit producer-fence snapshots for owned DMABuf frames.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::{AcquireSync, DmaBufMemory};

const DMA_BUF_BASE: u32 = b'b' as u32;
const SYNC_IOC_MAGIC: u32 = b'>' as u32;
const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_READ: u32 = 2;
const IOC_WRITE: u32 = 1;
const DMA_BUF_SYNC_READ: u32 = 1;

#[repr(C)]
#[derive(Debug, Default)]
struct DmaBufExportSyncFile {
    flags: u32,
    fd: i32,
}

#[repr(C)]
#[derive(Debug)]
struct SyncMergeData {
    name: [u8; 32],
    fd2: i32,
    fence: i32,
    flags: u32,
    pad: u32,
}

const fn ioc(direction: u32, kind: u32, number: u32, size: u32) -> libc::c_ulong {
    ((direction << IOC_DIRSHIFT)
        | (kind << IOC_TYPESHIFT)
        | (number << IOC_NRSHIFT)
        | (size << IOC_SIZESHIFT)) as libc::c_ulong
}

const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: libc::c_ulong = ioc(
    IOC_READ | IOC_WRITE,
    DMA_BUF_BASE,
    2,
    std::mem::size_of::<DmaBufExportSyncFile>() as u32,
);
const SYNC_IOC_MERGE: libc::c_ulong = ioc(
    IOC_READ | IOC_WRITE,
    SYNC_IOC_MAGIC,
    3,
    std::mem::size_of::<SyncMergeData>() as u32,
);

/// The narrow injected syscall surface used by fence-fold tests.
pub(crate) trait SyncFileOps {
    fn export(&mut self, dmabuf_fd: RawFd) -> Result<OwnedFd, String>;
    fn merge(&mut self, left: OwnedFd, right: OwnedFd) -> Result<OwnedFd, String>;
}

struct LinuxSyncFileOps;

impl SyncFileOps for LinuxSyncFileOps {
    fn export(&mut self, dmabuf_fd: RawFd) -> Result<OwnedFd, String> {
        let mut data = DmaBufExportSyncFile {
            flags: DMA_BUF_SYNC_READ,
            fd: -1,
        };
        // SAFETY: `data` is the exact repr(C) Linux UAPI payload for
        // DMA_BUF_IOCTL_EXPORT_SYNC_FILE, and `dmabuf_fd` is borrowed from a
        // live OwnedFd for this synchronous ioctl only.
        let result = unsafe { libc::ioctl(dmabuf_fd, DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &mut data) };
        if result < 0 {
            close_returned_fd(&mut data.fd);
            return Err(std::io::Error::last_os_error().to_string());
        }
        if data.fd < 0 {
            return Err("DMA_BUF_IOCTL_EXPORT_SYNC_FILE returned an invalid fd".into());
        }
        // SAFETY: The kernel returned a new sync-file descriptor owned by the
        // caller; OwnedFd closes it exactly once.
        Ok(unsafe { OwnedFd::from_raw_fd(data.fd) })
    }

    fn merge(&mut self, left: OwnedFd, right: OwnedFd) -> Result<OwnedFd, String> {
        let mut data = SyncMergeData {
            name: [0; 32],
            fd2: right.as_raw_fd(),
            fence: -1,
            flags: 0,
            pad: 0,
        };
        // SAFETY: `data` is the exact repr(C) Linux UAPI payload for
        // SYNC_IOC_MERGE, and both descriptors remain owned and live through
        // this synchronous ioctl.
        let result = unsafe { libc::ioctl(left.as_raw_fd(), SYNC_IOC_MERGE, &mut data) };
        if result < 0 {
            close_returned_fd(&mut data.fence);
            return Err(std::io::Error::last_os_error().to_string());
        }
        if data.fence < 0 {
            return Err("SYNC_IOC_MERGE returned an invalid fd".into());
        }
        drop(left);
        drop(right);
        // SAFETY: The kernel returned a new sync-file descriptor owned by the
        // caller; OwnedFd closes it exactly once.
        Ok(unsafe { OwnedFd::from_raw_fd(data.fence) })
    }
}

fn close_returned_fd(fd: &mut i32) {
    if *fd < 0 {
        return;
    }
    // SAFETY: A non-negative fd in an ioctl output slot is an owned descriptor
    // returned by the kernel and must be closed on the error path.
    unsafe {
        libc::close(*fd);
    }
    *fd = -1;
}

/// Exports one read fence per unique DMABuf object and folds them pairwise.
/// No waiting, polling, or heap-backed intermediate fence list is used.
pub(crate) fn fold_exported_sync_files<O: SyncFileOps>(
    memory: &DmaBufMemory,
    ops: &mut O,
) -> Result<Option<OwnedFd>, String> {
    let mut merged = None;
    for (index, object) in memory.objects.iter().enumerate() {
        let duplicate = memory.objects.get(..index).is_some_and(|prior| {
            prior
                .iter()
                .any(|entry| entry.fd.as_raw_fd() == object.fd.as_raw_fd())
        });
        if duplicate {
            continue;
        }
        let exported = ops.export(object.fd.as_raw_fd())?;
        merged = Some(match merged {
            Some(previous) => ops.merge(previous, exported)?,
            None => exported,
        });
    }
    Ok(merged)
}

/// Takes a producer-fence snapshot for a DMABuf frame.
pub fn export_acquire_sync(memory: &DmaBufMemory) -> Result<AcquireSync, String> {
    let mut ops = LinuxSyncFileOps;
    Ok(match fold_exported_sync_files(memory, &mut ops)? {
        Some(fd) => AcquireSync::SyncFile(fd),
        None => AcquireSync::None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DmaBufMemoryPlane, DmaBufObject};
    use std::fs::File;

    #[test]
    fn linux_uapi_payloads_and_ioctl_numbers_match_kernel_abi() {
        assert_eq!(std::mem::size_of::<DmaBufExportSyncFile>(), 8);
        assert_eq!(std::mem::size_of::<SyncMergeData>(), 48);
        assert_eq!(DMA_BUF_SYNC_READ, 1);
        assert_eq!(DMA_BUF_IOCTL_EXPORT_SYNC_FILE, 0xc008_6202);
        assert_eq!(SYNC_IOC_MERGE, 0xc030_3e03);
    }

    struct FakeOps {
        exports: usize,
        merges: usize,
        fail_export_at: Option<usize>,
        fail_merge: bool,
        exported_fds: Vec<RawFd>,
        merged_inputs: Vec<(RawFd, RawFd)>,
    }

    impl SyncFileOps for FakeOps {
        fn export(&mut self, _dmabuf_fd: RawFd) -> Result<OwnedFd, String> {
            self.exports += 1;
            if self.fail_export_at == Some(self.exports) {
                return Err("export failed".into());
            }
            let fd = OwnedFd::from(File::open("/dev/null").map_err(|e| e.to_string())?);
            self.exported_fds.push(fd.as_raw_fd());
            Ok(fd)
        }

        fn merge(&mut self, left: OwnedFd, right: OwnedFd) -> Result<OwnedFd, String> {
            self.merges += 1;
            self.merged_inputs
                .push((left.as_raw_fd(), right.as_raw_fd()));
            if self.fail_merge {
                drop(left);
                drop(right);
                return Err("merge failed".into());
            }
            drop(left);
            drop(right);
            Ok(OwnedFd::from(
                File::open("/dev/null").map_err(|e| e.to_string())?,
            ))
        }
    }

    fn fd_is_closed(fd: RawFd) -> bool {
        // SAFETY: `fd` is only inspected; fcntl does not take ownership or
        // mutate the descriptor, and the test expects EBADF after its owner
        // has been dropped.
        unsafe { libc::fcntl(fd, libc::F_GETFD) == -1 }
    }

    fn memory() -> Result<DmaBufMemory, Box<dyn std::error::Error>> {
        Ok(DmaBufMemory::new(
            vec![
                DmaBufObject {
                    fd: OwnedFd::from(File::open("/dev/null")?),
                    size: Some(1),
                },
                DmaBufObject {
                    fd: OwnedFd::from(File::open("/dev/null")?),
                    size: Some(1),
                },
            ],
            vec![
                DmaBufMemoryPlane {
                    object: 0,
                    offset: 0,
                    size: Some(1),
                },
                DmaBufMemoryPlane {
                    object: 1,
                    offset: 0,
                    size: Some(1),
                },
            ],
            vec![],
            None,
            Some(0),
        )?)
    }

    #[test]
    fn exports_once_per_unique_object_and_folds_pairwise() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut ops = FakeOps {
            exports: 0,
            merges: 0,
            fail_export_at: None,
            fail_merge: false,
            exported_fds: Vec::new(),
            merged_inputs: Vec::new(),
        };
        let result = fold_exported_sync_files(&memory()?, &mut ops)?;
        assert!(result.is_some());
        assert_eq!(ops.exports, 2);
        assert_eq!(ops.merges, 1);
        Ok(())
    }

    #[test]
    fn export_error_drops_previous_fence() -> Result<(), Box<dyn std::error::Error>> {
        let mut ops = FakeOps {
            exports: 0,
            merges: 0,
            fail_export_at: Some(2),
            fail_merge: false,
            exported_fds: Vec::new(),
            merged_inputs: Vec::new(),
        };
        assert!(fold_exported_sync_files(&memory()?, &mut ops).is_err());
        assert_eq!(ops.exports, 2);
        assert_eq!(ops.exported_fds.len(), 1);
        assert!(ops.exported_fds.iter().copied().all(fd_is_closed));
        Ok(())
    }

    #[test]
    fn merge_error_is_nonblocking_and_cleans_inputs() -> Result<(), Box<dyn std::error::Error>> {
        let mut ops = FakeOps {
            exports: 0,
            merges: 0,
            fail_export_at: None,
            fail_merge: true,
            exported_fds: Vec::new(),
            merged_inputs: Vec::new(),
        };
        assert!(fold_exported_sync_files(&memory()?, &mut ops).is_err());
        assert_eq!(ops.exports, 2);
        assert_eq!(ops.merges, 1);
        assert_eq!(ops.merged_inputs.len(), 1);
        let Some(&(left, right)) = ops.merged_inputs.first() else {
            return Err("merge inputs were not recorded".into());
        };
        assert!(fd_is_closed(left));
        assert!(fd_is_closed(right));
        Ok(())
    }
}
