//! The CachedFile handle, its per-open user data and teardown.

use super::*;

/// A file handle with an LRU page cache for buffered I/O.
pub struct CachedFile {
    pub(super) inner: Location,
    pub(super) shared: Arc<CachedFileShared>,
    pub(super) in_memory: bool,
}

impl Clone for CachedFile {
    fn clone(&self) -> Self {
        self.shared.open_handles.fetch_add(1, Ordering::AcqRel);
        Self {
            inner: self.inner.clone(),
            shared: self.shared.clone(),
            in_memory: self.in_memory,
        }
    }
}

pub(super) struct FileUserData {
    pub(super) registry_key: CachedFileRegistryKey,
    pub(super) identity_lease: Arc<CachedFileIdentityLease>,
    pub(super) shared: Weak<CachedFileShared>,
    pub(super) retained: Option<Arc<CachedFileShared>>,
    pub(super) writeback_anchor: Option<WritebackAnchor>,
    pub(super) retained_pages: usize,
    pub(super) retained_epoch: u64,
    pub(super) mountpoint: Weak<Mountpoint>,
    pub(super) entry: WeakDirEntry,
}

impl FileUserData {
    pub(super) fn new_identity(location: &Location) -> Self {
        let object = NEXT_CACHED_FILE_IDENTITY
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .expect("cached file identity generation exhausted");
        let identity_lease = Arc::new(CachedFileIdentityLease {
            object,
            discarded_unlinked: AtomicBool::new(false),
        });
        let object_key = location.object_key();
        let registry_key = CachedFileIdentity {
            device: object_key.filesystem,
            inode: object_key.object,
            object: identity_lease.object(),
        };
        Self {
            registry_key,
            identity_lease,
            shared: Weak::new(),
            retained: None,
            writeback_anchor: None,
            retained_pages: 0,
            retained_epoch: 0,
            mountpoint: Arc::downgrade(location.mountpoint()),
            entry: location.entry().downgrade(),
        }
    }

    pub(super) fn new(location: &Location, shared: &Arc<CachedFileShared>) -> Self {
        Self {
            registry_key: shared.registry_key,
            identity_lease: shared.identity_lease.clone(),
            shared: Arc::downgrade(shared),
            retained: None,
            writeback_anchor: None,
            retained_pages: 0,
            retained_epoch: 0,
            mountpoint: Arc::downgrade(location.mountpoint()),
            entry: location.entry().downgrade(),
        }
    }

    pub(super) fn identity(&self) -> CachedFileRegistryKey {
        self.registry_key
    }

    pub fn shared(&self) -> Option<Arc<CachedFileShared>> {
        self.retained.clone().or_else(|| self.shared.upgrade())
    }

    pub(super) fn references_shared(&self, shared: &Arc<CachedFileShared>) -> bool {
        self.retained.as_ref().map_or_else(
            || core::ptr::eq(self.shared.as_ptr(), Arc::as_ptr(shared)),
            |retained| Arc::ptr_eq(retained, shared),
        )
    }

    pub(super) fn has_live_shared(&self) -> bool {
        self.retained.is_some() || self.shared.strong_count() != 0
    }

    pub fn writeback_anchor(&self) -> Option<WritebackAnchor> {
        if let Some(anchor) = &self.writeback_anchor {
            return Some(anchor.clone());
        }
        Some(
            self.mountpoint
                .upgrade()?
                .writeback_anchor(self.entry.upgrade()?),
        )
    }

    pub(super) fn update_location(&mut self, location: &Location) {
        self.mountpoint = Arc::downgrade(location.mountpoint());
        self.entry = location.entry().downgrade();
    }

    pub(super) fn retain_closed(
        &mut self,
        location: &Location,
        shared: &Arc<CachedFileShared>,
        pages: usize,
    ) -> Option<Arc<CachedFileShared>> {
        self.update_location(location);
        let old_pages = self.retained_pages;
        if pages > old_pages {
            CLOSED_FILE_CACHE_RETAINED_PAGES.fetch_add(pages - old_pages, Ordering::AcqRel);
        } else if old_pages > pages {
            CLOSED_FILE_CACHE_RETAINED_PAGES.fetch_sub(old_pages - pages, Ordering::AcqRel);
        }
        let retired = self.retained.replace(shared.clone());
        self.retained_pages = pages;
        self.retained_epoch = CLOSED_FILE_CACHE_RETAIN_EPOCH.fetch_add(1, Ordering::Relaxed) + 1;
        retired
    }

    pub(super) fn release_retained(&mut self) -> Option<Arc<CachedFileShared>> {
        let retained = self.retained.take()?;
        let pages = self.retained_pages;
        self.retained_pages = 0;
        self.retained_epoch = 0;
        if pages != 0 {
            CLOSED_FILE_CACHE_RETAINED_PAGES.fetch_sub(pages, Ordering::AcqRel);
        }
        record_cached_file_counter(&CLOSED_FILE_CACHE_RETAIN_RELEASES, 1);
        Some(retained)
    }
}

impl Drop for FileUserData {
    fn drop(&mut self) {
        let _ = self.release_retained();
    }
}

impl Drop for CachedFile {
    fn drop(&mut self) {
        let open_handles = self.shared.open_handles.fetch_sub(1, Ordering::AcqRel);
        if open_handles == 0 {
            warn!("\x012CachedFile dropped with no open handle reference");
            return;
        }
        if open_handles > 1 {
            return;
        }
        if self.shared.unlinked.load(Ordering::Acquire) {
            // The last open handle is not necessarily the last physical
            // owner. Keep registry/cache ownership deferred while an exact
            // direct/effect range lease is live; that lease's Drop performs
            // the final synchronous cleanup.
            request_unlinked_cached_file_cleanup(&self.shared);
            return;
        }
        if try_retain_closed_cached_file(&self.inner, &self.shared) {
            return;
        }
        let file = match self.inner.entry().as_file() {
            Ok(file) => file,
            Err(err) => {
                debug!("Failed to access file for cache drop: {err:?}");
                return;
            }
        };
        if let Err(err) = self.drain_cache(file) {
            // `close(2)` is not required to persist data to the device. Keep
            // the explicit flush path on `fsync`/`fdatasync`, and only make
            // dirty cached pages visible to the inode here.
            ratelimit::warn_ratelimited!("Failed to drain cached file pages on drop: {err:?}");
        }
    }
}
