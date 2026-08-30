use alloc::{borrow::Cow, sync::Arc};
use core::{
    sync::atomic::{AtomicU64, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, Pollable};
use axsync::{Mutex, MutexGuard};
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

/// Serializes a secretmem `ftruncate` from its immutable-size admission
/// through publication.  The guard deliberately spans the caller's resource
/// limit check: otherwise two callers can both observe a zero size, after
/// which a zero-length truncate may incorrectly succeed after another caller
/// has fixed the object's size.
pub(crate) struct SecretMemTruncateGuard<'a> {
    secret: &'a SecretMemFile,
    pages: MutexGuard<'a, Option<Arc<SharedPages>>>,
}

impl SecretMemFile {
    pub(crate) const fn new() -> Self {
        Self {
            pages: Mutex::new(None),
            size: AtomicU64::new(0),
        }
    }

    /// Acquires the immutable-size admission for an `ftruncate` operation.
    ///
    /// Callers must retain the returned guard until after every check that can
    /// reject the mutation, and then use [`SecretMemTruncateGuard::truncate`]
    /// to publish the first nonzero size.
    pub(crate) fn begin_truncate(&self) -> AxResult<SecretMemTruncateGuard<'_>> {
        let pages = self.pages.lock();
        if self.size.load(Ordering::Acquire) != 0 {
            return Err(AxError::InvalidInput);
        }
        Ok(SecretMemTruncateGuard {
            secret: self,
            pages,
        })
    }

    pub(crate) fn size(&self) -> u64 {
        self.size.load(Ordering::Acquire)
    }
}

impl SecretMemTruncateGuard<'_> {
    pub(crate) fn truncate(mut self, size: u64) -> AxResult<()> {
        let size: usize = size.try_into().map_err(|_| AxError::NoMemory)?;
        // secretmem has a fixed size once it becomes nonempty; in particular
        // ftruncate(fd, 0) must not silently discard a live secret object.
        // `begin_truncate` checked this under the same mutex; retain the
        // assertion here so future callers cannot separate admission from
        // mutation accidentally.
        if self.secret.size.load(Ordering::Acquire) != 0 {
            return Err(AxError::InvalidInput);
        }
        if size == 0 {
            return Ok(());
        }
        // i_size is byte-granular; backing metadata is sparse, so truncation
        // only records the logical size and never reserves one entry/page.
        if let Some(pages) = self.pages.as_ref() {
            pages.set_secret_size_once(size)?;
        } else {
            let pages = Arc::try_new(SharedPages::new_secret_fixed(size)?)
                .map_err(|_| AxError::NoMemory)?;
            *self.pages = Some(pages);
        }
        self.secret.size.store(size as u64, Ordering::Release);
        Ok(())
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
        let pages = {
            let mut pages = self.pages.lock();
            match pages.as_ref() {
                Some(pages) => pages.clone(),
                None => {
                    let backing = Arc::try_new(SharedPages::new_secret_fixed(0)?)
                        .map_err(|_| AxError::NoMemory)?;
                    *pages = Some(backing.clone());
                    backing
                }
            }
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::FileMmapSharing;
    use memory_addr::PAGE_SIZE_4K;

    fn shared_request() -> FileMmapRequest {
        FileMmapRequest::try_new(
            0,
            PAGE_SIZE_4K,
            PAGE_SIZE_4K,
            FileMmapProtection::READ | FileMmapProtection::WRITE,
            FileMmapSharing::Shared,
        )
        .unwrap()
    }

    #[test]
    fn zero_length_secret_mmap_reuses_backing_for_first_truncate() {
        let secret = SecretMemFile::new();
        let mapped = secret.prepare_mmap(shared_request()).unwrap().unwrap().into_pages();
        assert!(mapped.is_secret());
        assert_eq!(mapped.total_bytes(), 0);

        secret
            .begin_truncate()
            .unwrap()
            .truncate(PAGE_SIZE_4K as u64)
            .unwrap();
        let remapped = secret.prepare_mmap(shared_request()).unwrap().unwrap().into_pages();
        assert!(Arc::ptr_eq(&mapped, &remapped));
        assert_eq!(mapped.total_bytes(), PAGE_SIZE_4K);

        mapped.write_bytes(0, &[0]).unwrap();
    }

    #[test]
    fn first_nonzero_size_freezes_later_zero_resize() {
        let secret = SecretMemFile::new();
        secret
            .begin_truncate()
            .unwrap()
            .truncate(PAGE_SIZE_4K as u64)
            .unwrap();

        // `begin_truncate` is the serialized admission used by ftruncate;
        // after a concurrent first-size publisher releases it, a queued zero
        // resize observes the published size and must fail.
        assert_eq!(secret.begin_truncate().err(), Some(AxError::InvalidInput));
        assert_eq!(secret.size(), PAGE_SIZE_4K as u64);
    }
}
