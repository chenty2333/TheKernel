//! Linux `sync_file` for the DRM core: a sync_file descriptor snapshots one
//! fence, and `SYNC_IOC_MERGE` joins two of them (`sync_file_ioctl_merge()`).
//! Syncobj export/import (`drm_syncobj_export_sync_file` and
//! `sync_file_get_fence()` on the DRM side) reuses these helpers.

use alloc::{borrow::Cow, sync::Arc};
use core::task::Context;

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};

use crate::{
    drm::fence::Fence,
    file::{FileLike, IoctlContext, Kstat},
};


const SYNC_IOC_MAGIC: u32 = b'>' as u32;

#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct SyncMergeData {
    name: [u8; 32],
    fd2: i32,
    fence: i32,
    flags: u32,
    pad: u32,
}

const SYNC_IOC_MERGE: u32 =
    (3 << 30) | ((core::mem::size_of::<SyncMergeData>() as u32) << 16) | (SYNC_IOC_MAGIC << 8) | 3;
const _: () = {
    assert!(core::mem::size_of::<SyncMergeData>() == 48);
    assert!(SYNC_IOC_MERGE == 0xc030_3e03);
};

fn validate_merge_data(request: &SyncMergeData) -> AxResult<()> {
    if request.flags != 0 || request.pad != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn finish_merge_ioctl(
    memory: &crate::mm::UserMemoryCapability,
    arg: usize,
    mut request: SyncMergeData,
    output: crate::file::PreparedFdPublication,
) -> AxResult<usize> {
    request.fence = output.fd();
    // Linux bounds the user-visible name before returning sync_merge_data.
    request.name[request.name.len() - 1] = 0;
    memory
        .write_value(arg as *mut SyncMergeData, request)
        .map_err(crate::mm::map_usercopy_error)?;
    // Linux keeps the fd reserved until after copy_to_user, so a failed
    // result copy neither leaks a visible descriptor nor exposes a partial
    // sync_file. Dropping `output` rolls the exact reservation back.
    output.commit();
    Ok(0)
}


pub struct SyncFile {
    fence: Arc<Fence>,
}

impl SyncFile {
    pub fn new(fence: Arc<Fence>) -> Arc<Self> {
        Arc::new(Self { fence })
    }
    pub(crate) fn try_new(fence: Arc<Fence>) -> AxResult<Arc<Self>> {
        Arc::try_new(Self { fence }).map_err(|_| AxError::NoMemory)
    }
    pub fn fence(&self) -> Arc<Fence> {
        self.fence.clone()
    }
}
impl FileLike for SyncFile {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(crate::file::anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:[sync_file]",
        )))
    }
    fn set_nonblocking(&self, _: bool) -> AxResult<()> {
        Ok(())
    }
    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        if cmd != SYNC_IOC_MERGE {
            return Err(AxError::NotATty);
        }

        // `sync_file_ioctl_merge()` reserves a CLOEXEC fd before importing
        // its userspace request. Keep the slot invisible until after the
        // merged-fd number has been copied back successfully.
        let slot = reserve_fd(context, true)?;
        let request = context
            .user_memory()
            .read_value::<SyncMergeData>(arg as *const SyncMergeData)
            .map_err(crate::mm::map_usercopy_error)?;
        validate_merge_data(&request)?;

        // Linux sync_file_fdget() returns NULL for both an invalid descriptor
        // and a descriptor of another file type; the merge ioctl reports
        // -ENOENT in either case.
        let other = super::import(context, request.fd2).map_err(|_| AxError::NotFound)?;
        let merged = Fence::join(&[self.fence.clone(), other])?;
        let output = publish(merged, slot)?;
        finish_merge_ioctl(context.user_memory(), arg, request, output)
    }
}
impl Pollable for SyncFile {
    fn poll(&self) -> IoEvents {
        self.fence.poll_events()
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        self.fence.register_events(context, events)
    }
}

pub(crate) fn export(
    fence: Arc<Fence>,
    context: &crate::file::IoctlContext,
    cloexec: bool,
) -> AxResult<i32> {
    let slot = reserve_fd(context, cloexec)?;
    Ok(publish(fence, slot)?.commit())
}

pub(crate) fn reserve_fd(
    context: &IoctlContext,
    cloexec: bool,
) -> AxResult<crate::file::ReservedFd> {
    let limit = context.caller_process().rlim.read()[linux_raw_sys::general::RLIMIT_NOFILE]
        .current
        .min(crate::task::AX_FILE_LIMIT as u64) as usize;
    crate::file::reserve_fd_in(context.files().clone(), limit, cloexec)
}

pub(crate) fn publish(
    fence: Arc<Fence>,
    slot: crate::file::ReservedFd,
) -> AxResult<crate::file::PreparedFdPublication> {
    let description = crate::file::FileDescription::new(SyncFile::new(fence))?;
    slot.prepare_publication(description)
}
pub(crate) fn import(context: &crate::file::IoctlContext, fd: i32) -> AxResult<Arc<Fence>> {
    context
        .get_file_like(fd)?
        .downcast::<SyncFile>()
        .map(|f| f.fence())
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, task::Wake};
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Waker},
    };

    use axhal::paging::{MappingFlags, PageSize};
    use axsync::Mutex;
    use memory_addr::{PAGE_SIZE_4K, VirtAddr};

    use super::*;

    fn sync_file_test_memory() -> crate::mm::UserMemoryCapability {
        let mut address_space =
            crate::mm::AddrSpace::new_empty(VirtAddr::from(0x1000), PAGE_SIZE_4K * 2).unwrap();
        address_space
            .map(
                VirtAddr::from(0x1000),
                PAGE_SIZE_4K,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                false,
                crate::mm::Backend::new_alloc(VirtAddr::from(0x1000), PageSize::Size4K),
            )
            .unwrap();
        crate::mm::UserMemoryCapability::new(Arc::new(Mutex::new(address_space)))
    }

    fn prepared_test_sync_file(
        table: Arc<crate::file::FdTable>,
    ) -> crate::file::PreparedFdPublication {
        let slot = crate::file::reserve_fd_in(table, 32, true).unwrap();
        let description =
            crate::file::FileDescription::new(SyncFile::new(Fence::new(false))).unwrap();
        slot.prepare_publication(description).unwrap()
    }

    #[test]
    fn sync_file_merge_ioctl_matches_linux_abi_and_validates_reserved_fields() {
        assert_eq!(SYNC_IOC_MERGE, 0xc030_3e03);
        assert_eq!(core::mem::size_of::<SyncMergeData>(), 48);
        assert_eq!(validate_merge_data(&SyncMergeData::default()), Ok(()));
        assert_eq!(
            validate_merge_data(&SyncMergeData {
                flags: 1,
                ..SyncMergeData::default()
            }),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            validate_merge_data(&SyncMergeData {
                pad: 1,
                ..SyncMergeData::default()
            }),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn sync_file_merge_copyout_fault_cancels_unpublished_cloexec_fd() {
        let table = Arc::new(crate::file::FdTable::new().unwrap());
        let output = prepared_test_sync_file(table.clone());
        let fd = output.fd();
        assert!(table.get_description(fd).is_err());

        // 0x2000 lies in the address-space range but has no mapped page.
        let error = finish_merge_ioctl(
            &sync_file_test_memory(),
            0x2000,
            SyncMergeData::default(),
            output,
        )
        .unwrap_err();
        assert_eq!(error, AxError::BadAddress);
        assert!(table.get_description(fd).is_err());
        let replacement = crate::file::reserve_fd_in(table.clone(), 32, true).unwrap();
        assert_eq!(replacement.fd(), fd);
    }

    #[test]
    fn sync_file_merge_copyout_publishes_cloexec_result_only_after_success() {
        let table = Arc::new(crate::file::FdTable::new().unwrap());
        let memory = sync_file_test_memory();
        let output = prepared_test_sync_file(table.clone());
        let fd = output.fd();
        assert_eq!(
            finish_merge_ioctl(
                &memory,
                0x1000,
                SyncMergeData {
                    name: [b'X'; 32],
                    ..SyncMergeData::default()
                },
                output,
            ),
            Ok(0)
        );
        let result = memory
            .read_value::<SyncMergeData>(0x1000 as *const SyncMergeData)
            .unwrap();
        assert_eq!(result.fence, fd);
        assert_eq!(result.name[31], 0);
        assert!(table.get_cloexec(fd).unwrap());
        assert!(
            table
                .get_description(fd)
                .unwrap()
                .inner
                .downcast_ref::<SyncFile>()
                .is_some()
        );
    }

    #[test]
    fn sync_file_merge_fence_waits_for_every_source_and_propagates_failure() {
        let first = Fence::new(false);
        let second = Fence::new(false);
        let merged = Fence::join(&[first.clone(), second.clone()]).unwrap();
        first.signal();
        assert!(!merged.is_signaled());
        second.signal_error();
        assert!(merged.is_signaled());
        assert!(merged.is_failed());
    }

    #[test]
    fn sync_file_poll_tracks_deferred_completion_error_and_shared_fence_identity() {
        let pending = Fence::new(false);
        let file = SyncFile::new(pending.clone());
        let duplicate = file.clone();
        assert!(file.poll().is_empty());
        assert!(Arc::ptr_eq(&file.fence(), &duplicate.fence()));

        // A sync_file snapshots one fence: terminal completion becomes
        // pollable only after that producer fence signals.
        pending.signal();
        assert!(file.poll().contains(IoEvents::READABLE));
        assert!(!file.poll().contains(IoEvents::ERROR));

        let failed = Fence::new(false);
        let error_file = SyncFile::new(failed.clone());
        assert!(error_file.poll().is_empty());
        failed.signal_error();
        assert!(error_file.poll().contains(IoEvents::ERROR));
        assert!(!error_file.poll().contains(IoEvents::READABLE));
    }


    struct CountingWake(AtomicUsize);

    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn sync_file_poll_registration_wakes_when_its_fence_signals() {
        let fence = Fence::new(false);
        let sync_file = SyncFile::new(fence.clone());
        assert_eq!(sync_file.poll(), IoEvents::empty());

        let wake = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(wake.clone());
        let mut context = Context::from_waker(&waker);
        let _registration = sync_file
            .register(&mut context, IoEvents::READABLE)
            .unwrap();

        fence.signal();
        assert_eq!(wake.0.load(Ordering::SeqCst), 1);
        assert_eq!(sync_file.poll(), IoEvents::READABLE);
    }

}
