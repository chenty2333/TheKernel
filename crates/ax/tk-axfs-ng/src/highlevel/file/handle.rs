//! The File open description and its std-style I/O trait impls.

use super::*;

/// Provides `std::fs::File`-like interface.
pub struct File {
    pub(super) inner: FileBackend,
    pub(super) open_handle: Option<Arc<dyn FileNodeOps>>,
    /// Mutable open-file-description append status. Access admission remains
    /// in `flags`; toggling append must never manufacture write authority.
    pub(super) append: AtomicBool,
    /// One-shot O_TRUNC capability installed only by the deferred-open path.
    /// It belongs to the OFD, so duplicated descriptors share one commit.
    pub(super) pending_open_truncate: AtomicBool,
    pub(super) flags: FileFlags,
    /// Serializes operations which observe and later commit the current
    /// position without requiring the position spin lock to remain held while
    /// an external transfer consumer runs.
    pub(super) position_transaction: Mutex<()>,
    pub(super) position: Option<Mutex<u64>>,
    pub(super) readahead: AtomicU8,
    #[cfg(feature = "times")]
    pub(super) access_flags: AtomicU8,
}

impl Read for &File {
    fn read(&mut self, buf: &mut [u8]) -> axio::Result<usize> {
        (*self).read(buf)
    }
}

impl Write for &File {
    fn write(&mut self, buf: &[u8]) -> axio::Result<usize> {
        (*self).write(buf)
    }

    fn flush(&mut self) -> axio::Result {
        (*self).flush()
    }
}

impl Seek for &File {
    fn seek(&mut self, pos: SeekFrom) -> axio::Result<u64> {
        self.access(FileFlags::empty())?;
        let _transaction = self.position_transaction.lock();

        if let Some(guard) = self.position.as_ref() {
            let mut guard = guard.lock();
            let new_pos = match pos {
                SeekFrom::Start(pos) => pos,
                SeekFrom::End(off) => {
                    let size = self.access(FileFlags::empty())?.location().len()?;
                    size.checked_add_signed(off).ok_or(VfsError::InvalidInput)?
                }
                SeekFrom::Current(off) => guard
                    .checked_add_signed(off)
                    .ok_or(VfsError::InvalidInput)?,
            };
            *guard = new_pos;
            Ok(new_pos)
        } else {
            Ok(0)
        }
    }
}

impl Pollable for File {
    fn poll(&self) -> IoEvents {
        self.open_handle
            .as_ref()
            .map_or_else(|| self.inner.location().poll(), |handle| handle.poll())
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        match self.open_handle.as_ref() {
            Some(handle) => handle.register(context, events),
            None => self.inner.location().register(context, events),
        }
    }
}

impl Drop for File {
    fn drop(&mut self) {
        #[cfg(feature = "times")]
        self.flush_times();
        // A close cannot return a provider failure to the completed syscall;
        // retain any failed projection for a later fsync/writeback instead of
        // revising successful I/O here.
        let _ = self.location().flush_metadata_time_overlay();
        if let Some(handle) = &self.open_handle {
            // Close is best-effort at this layer: the provider records an
            // asynchronous release error on its connection/writeback state.
            // Destruction cannot return it to a completed userspace syscall.
            let _ = handle.release_handle();
        }
    }
}
