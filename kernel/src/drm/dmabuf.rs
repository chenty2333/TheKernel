//! Generic PRIME dma-buf OFDs.  The OFD owns an `Arc<GemObject>`, so closing
//! either the exporting handle or dma-buf fd cannot invalidate mappings or a
//! scanout which already retained the object.

use alloc::{borrow::Cow, sync::Arc};
use core::task::Context;

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};

use super::{fence::Fence, gem::GemObject, syncobj};
use crate::file::{FileLike, IoctlContext, Kstat};

const DMA_BUF_SYNC_READ: u32 = 1;
const DMA_BUF_SYNC_WRITE: u32 = 2;
const DMA_BUF_SYNC_RW: u32 = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE;

#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct SyncFileIoctl {
    flags: u32,
    fd: i32,
}

const fn dma_buf_ioctl<T>(nr: u32, direction: u32) -> u32 {
    (direction << 30) | ((core::mem::size_of::<T>() as u32) << 16) | ((b'b' as u32) << 8) | nr
}

const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: u32 = dma_buf_ioctl::<SyncFileIoctl>(2, 3);
const DMA_BUF_IOCTL_IMPORT_SYNC_FILE: u32 = dma_buf_ioctl::<SyncFileIoctl>(3, 1);
const _: () = {
    assert!(core::mem::size_of::<SyncFileIoctl>() == 8);
    assert!(DMA_BUF_IOCTL_EXPORT_SYNC_FILE == 0xc008_6202);
    assert!(DMA_BUF_IOCTL_IMPORT_SYNC_FILE == 0x4008_6203);
};

fn validate_sync_flags(flags: u32) -> AxResult<()> {
    if flags == 0 || flags & !DMA_BUF_SYNC_RW != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

/// The current DRM reservation is exclusive and covers every admitted GPU
/// user, so both READ and WRITE exports conservatively snapshot its latest
/// fence. A reservation without users is equivalent to an already-signaled
/// sync_file.
fn export_sync_fence(object: &GemObject, flags: u32) -> AxResult<Arc<Fence>> {
    validate_sync_flags(flags)?;
    Ok(object
        .reservation
        .predecessor()
        .unwrap_or_else(|| Fence::new(true)))
}

/// This kernel has one exclusive reservation edge rather than Linux's
/// read/write fence sets. Retain imported READ and WRITE fences on that edge;
/// this may serialize extra readers, but can never let a later writer pass an
/// imported fence.
fn import_sync_fence(object: &GemObject, flags: u32, fence: Arc<Fence>) -> AxResult<()> {
    validate_sync_flags(flags)?;
    object.reservation.append(fence)
}

pub struct DmaBufFile {
    object: Arc<GemObject>,
}
impl DmaBufFile {
    pub(crate) fn new(object: Arc<GemObject>) -> Arc<Self> {
        Arc::new(Self { object })
    }
    pub(crate) fn object(&self) -> Arc<GemObject> {
        self.object.clone()
    }
}
impl FileLike for DmaBufFile {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(crate::file::anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:[dmabuf]",
        )))
    }
    fn set_nonblocking(&self, _: bool) -> AxResult<()> {
        Ok(())
    }
    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        if !matches!(
            cmd,
            DMA_BUF_IOCTL_EXPORT_SYNC_FILE | DMA_BUF_IOCTL_IMPORT_SYNC_FILE
        ) {
            return Err(AxError::NotATty);
        }
        let mut request = context
            .user_memory()
            .read_value::<SyncFileIoctl>(arg as *const SyncFileIoctl)
            .map_err(crate::mm::map_usercopy_error)?;
        if cmd == DMA_BUF_IOCTL_EXPORT_SYNC_FILE {
            let fence = export_sync_fence(&self.object, request.flags)?;
            request.fd = syncobj::export(fence, context, true)?;
            if let Err(error) = context
                .user_memory()
                .write_value(arg as *mut SyncFileIoctl, request)
                .map_err(crate::mm::map_usercopy_error)
            {
                // Do not leak a descriptor if copying the result back to
                // the caller faults. Close through the syscall's captured
                // file table, never through ambient current-task lookup.
                if let Ok(file) = context.files().close(request.fd) {
                    drop(file);
                }
                return Err(error);
            }
        } else {
            validate_sync_flags(request.flags)?;
            // dma_buf_import_sync_file() reports -EINVAL when
            // sync_file_get_fence() cannot resolve a sync_file, including an
            // invalid descriptor and a descriptor for another file type.
            let fence = syncobj::import(context, request.fd).map_err(|_| AxError::InvalidInput)?;
            import_sync_fence(&self.object, request.flags, fence)?;
        }
        Ok(0)
    }
}
impl Pollable for DmaBufFile {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }
    fn register<'a>(
        &'a self,
        _: &mut Context<'_>,
        _: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        axpoll::PreparedPollRegistration::try_new(0)?.commit()
    }
}

pub(crate) fn export(
    object: Arc<GemObject>,
    context: &crate::file::IoctlContext,
    cloexec: bool,
) -> AxResult<i32> {
    context.add_file_like(DmaBufFile::new(object), cloexec)
}
pub(crate) fn import(context: &crate::file::IoctlContext, fd: i32) -> AxResult<Arc<GemObject>> {
    let file = context.get_file_like(fd)?;
    file.downcast::<DmaBufFile>().map(|buf| buf.object())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::GemBacking;

    struct Backing;
    impl GemBacking for Backing {
        fn shared_pages(&self) -> crate::drm::DrmResult<Arc<crate::mm::SharedPages>> {
            Err(crate::drm::DrmError::Unsupported)
        }
    }

    fn object() -> GemObject {
        GemObject::new(Arc::new(Backing), 4096, 4096)
    }

    #[test]
    fn dma_buf_sync_file_uapi_flags_and_commands_match_linux() {
        assert_eq!(validate_sync_flags(DMA_BUF_SYNC_READ), Ok(()));
        assert_eq!(validate_sync_flags(DMA_BUF_SYNC_WRITE), Ok(()));
        assert_eq!(validate_sync_flags(DMA_BUF_SYNC_RW), Ok(()));
        assert_eq!(validate_sync_flags(0), Err(AxError::InvalidInput));
        assert_eq!(validate_sync_flags(4), Err(AxError::InvalidInput));
    }

    #[test]
    fn dma_buf_sync_file_snapshots_and_imports_reservation_dependencies() {
        let object = object();
        let empty = export_sync_fence(&object, DMA_BUF_SYNC_READ).unwrap();
        assert!(empty.is_signaled());

        let prior = Fence::new(false);
        object.reservation.publish(prior.clone());
        assert!(Arc::ptr_eq(
            &export_sync_fence(&object, DMA_BUF_SYNC_WRITE).unwrap(),
            &prior
        ));

        let imported = Fence::new(false);
        import_sync_fence(&object, DMA_BUF_SYNC_READ, imported.clone()).unwrap();
        let chained = export_sync_fence(&object, DMA_BUF_SYNC_RW).unwrap();
        assert_eq!(
            chained.wait(Some(core::time::Duration::ZERO)),
            Err(AxError::WouldBlock)
        );
        prior.signal();
        assert_eq!(
            chained.wait(Some(core::time::Duration::ZERO)),
            Err(AxError::WouldBlock)
        );
        imported.signal_error();
        assert_eq!(
            chained.wait(Some(core::time::Duration::ZERO)),
            Err(AxError::Io)
        );
    }
}
