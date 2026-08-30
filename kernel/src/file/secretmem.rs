use alloc::{borrow::Cow, sync::Arc};
use core::{
    sync::atomic::{AtomicU64, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use linux_raw_sys::general::S_IFREG;

use crate::{
    file::{
        FileLike, FileMmapProtection, FileMmapRequest, FixedSharedMmapRegion, Kstat,
        PreparedFileMmap, anon_inode_stat,
    },
    mm::SharedPages,
};

/// Anonymous secret-memory descriptor. Data is reachable only through its
/// shared, non-executable VMA; byte-stream I/O is deliberately unsupported.
pub(crate) struct SecretMemFile {
    pages: Mutex<Option<Arc<SharedPages>>>,
    size: AtomicU64,
}

impl SecretMemFile {
    pub(crate) const fn new() -> Self {
        Self {
            pages: Mutex::new(None),
            size: AtomicU64::new(0),
        }
    }

    pub(crate) fn truncate(&self, size: u64) -> AxResult<()> {
        let size: usize = size.try_into().map_err(|_| AxError::NoMemory)?;
        let mut current = self.pages.lock();
        // secretmem has a fixed size once it becomes nonempty; in particular
        // ftruncate(fd, 0) must not silently discard a live secret object.
        if current.is_some() {
            return Err(AxError::InvalidInput);
        }
        if size == 0 {
            return Ok(());
        }
        // i_size is byte-granular; backing metadata is sparse, so truncation
        // only records the logical size and never reserves one entry/page.
        let pages = Arc::try_new(SharedPages::new_secret_fixed(size)?)
            .map_err(|_| AxError::NoMemory)?;
        *current = Some(pages);
        self.size.store(size as u64, Ordering::Release);
        Ok(())
    }

    /// Validates the immutable-after-first-size rule without allocating or
    /// mutating.  ftruncate uses this before RLIMIT_FSIZE so a second secret
    /// truncate retains Linux's EINVAL priority.
    pub(crate) fn check_truncate(&self) -> AxResult<()> {
        if self.pages.lock().is_some() {
            Err(AxError::InvalidInput)
        } else {
            Ok(())
        }
    }

    pub(crate) fn size(&self) -> u64 {
        self.size.load(Ordering::Acquire)
    }
}

impl Pollable for SecretMemFile {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }
    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PreparedPollRegistration::try_new(0)?.commit()
    }
}

impl FileLike for SecretMemFile {
    fn stat(&self) -> AxResult<Kstat> {
        let mut stat = anon_inode_stat();
        stat.mode = S_IFREG | 0o600;
        stat.size = self.size.load(Ordering::Acquire);
        stat.blocks = 0;
        Ok(stat)
    }
    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok(Cow::Borrowed("/secretmem"))
    }
    fn prepare_mmap(&self, request: FileMmapRequest) -> AxResult<Option<PreparedFileMmap>> {
        let pages = self.pages.lock().clone().ok_or(AxError::InvalidInput)?;
        FixedSharedMmapRegion::try_new(
            0,
            pages,
            FileMmapProtection::READ | FileMmapProtection::WRITE,
        )?
        .prepare(request)
    }
    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        Ok(())
    }
}
