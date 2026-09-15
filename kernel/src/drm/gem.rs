use alloc::sync::Arc;

use crate::mm::SharedPages;

/// Driver-owned storage for a GEM object.  The DRM core never maps or copies it.
pub trait GemBacking: Send + Sync {
    /// The fixed pages retained by a VMA after its originating GEM handle closes.
    fn shared_pages(&self) -> super::DrmResult<Arc<SharedPages>>;
    /// Host VirtIO resource identity, when this backing is directly owned by
    /// a render/blob or scanout resource.  PRIME aliases retain the same
    /// backing Arc, so this identity follows imports instead of being tied to
    /// a per-file GEM handle.
    fn host_resource(&self) -> Option<HostResource> {
        None
    }
}

/// Typed resource identity determines whether scanout needs a CPU upload,
/// a native virgl texture, or an explicit blob layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostResourceKind {
    Scanout2d,
    Render3d,
    Blob,
}

/// Typed host ownership carried by the backing itself.  A GEM handle is only
/// a per-file name, so this is deliberately attached to the shared backing
/// and survives PRIME import/export unchanged.
#[derive(Clone, Copy)]
pub enum HostResource {
    Scanout2d {
        resource: u32,
    },
    Render3d {
        resource: u32,
        meta: super::render::RenderResource,
    },
    Blob {
        resource: u32,
        mem: super::render::BlobMem,
        flags: u32,
        size: u64,
        mapped: bool,
    },
}

impl HostResource {
    pub const fn id(self) -> u32 {
        match self {
            Self::Scanout2d { resource }
            | Self::Render3d { resource, .. }
            | Self::Blob { resource, .. } => resource,
        }
    }
    pub const fn kind(self) -> HostResourceKind {
        match self {
            Self::Scanout2d { .. } => HostResourceKind::Scanout2d,
            Self::Render3d { .. } => HostResourceKind::Render3d,
            Self::Blob { .. } => HostResourceKind::Blob,
        }
    }
}

/// Per-OFD object name, never valid in another [`crate::drm::DrmFile`].
pub type GemHandle = u32;
/// Per-device mmap token.  The devfs adapter turns this into a byte offset.
pub type MmapOffset = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DumbRequest {
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
}

#[derive(Clone)]
pub struct DumbBuffer {
    pub handle: GemHandle,
    pub pitch: u32,
    pub size: u64,
    pub mmap_offset: MmapOffset,
}

pub(crate) struct GemObject {
    pub(crate) backing: Arc<dyn GemBacking>,
    pub(crate) size: u64,
    pub(crate) mmap_offset: MmapOffset,
    pub(crate) reservation: super::fence::Reservation,
    /// Nonzero only for a legacy virgl resource owned by this GEM object.
    pub(crate) render_resource: Option<u32>,
    pub(crate) render_meta: Option<super::render::RenderResource>,
    pub(crate) render_blob_mem: Option<u32>,
}

impl GemObject {
    pub(crate) fn new(backing: Arc<dyn GemBacking>, size: u64, mmap_offset: MmapOffset) -> Self {
        Self {
            backing,
            size,
            mmap_offset,
            reservation: super::fence::Reservation::new(),
            render_resource: None,
            render_meta: None,
            render_blob_mem: None,
        }
    }

    pub(crate) fn render(
        backing: Arc<dyn GemBacking>,
        size: u64,
        mmap_offset: MmapOffset,
        resource: u32,
        meta: super::render::RenderResource,
        blob_mem: Option<u32>,
    ) -> Self {
        Self {
            backing,
            size,
            mmap_offset,
            reservation: super::fence::Reservation::new(),
            render_resource: Some(resource),
            render_meta: Some(meta),
            render_blob_mem: blob_mem,
        }
    }
}

// Pinned guest render backing is not reclaimable. Keep an admission budget
// separate from handle statistics: exported buffers and VMAs outlive handles.
use core::sync::atomic::{AtomicUsize, Ordering};
static PINNED_GEM_BYTES: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct GemMemoryCharge {
    owner: Arc<AtomicUsize>,
    bytes: usize,
}
impl GemMemoryCharge {
    pub(crate) fn reserve(owner: Arc<AtomicUsize>, bytes: usize) -> super::DrmResult<Arc<Self>> {
        let total = axhal::mem::total_ram_size();
        let global_limit = (total / 4).min(512 * 1024 * 1024);
        let file_limit = (total / 8).min(256 * 1024 * 1024);
        Self::reserve_with_limits(owner, bytes, global_limit, file_limit)
    }
    fn reserve_with_limits(
        owner: Arc<AtomicUsize>,
        bytes: usize,
        global_limit: usize,
        file_limit: usize,
    ) -> super::DrmResult<Arc<Self>> {
        owner
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| *next <= file_limit)
            })
            .map_err(|_| super::DrmError::NoMemory)?;
        if PINNED_GEM_BYTES
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| *next <= global_limit)
            })
            .is_err()
        {
            owner.fetch_sub(bytes, Ordering::AcqRel);
            return Err(super::DrmError::NoMemory);
        }
        Arc::try_new(Self { owner, bytes }).map_err(|_| super::DrmError::NoMemory)
    }
}
impl Drop for GemMemoryCharge {
    fn drop(&mut self) {
        PINNED_GEM_BYTES.fetch_sub(self.bytes, Ordering::AcqRel);
        self.owner.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod memory_charge_tests {
    use super::*;
    #[test]
    fn global_admission_failure_refunds_the_file_charge() {
        let owner = Arc::new(AtomicUsize::new(0));
        assert!(GemMemoryCharge::reserve_with_limits(owner.clone(), 4096, 0, 4096).is_err());
        assert_eq!(owner.load(Ordering::Acquire), 0);
    }

    #[test]
    fn page_alias_retains_charge_after_the_originating_owner_closes() {
        let _context = crate::test_support::scheduler_test_context();
        let owner = Arc::new(AtomicUsize::new(0));
        let charge =
            GemMemoryCharge::reserve_with_limits(owner.clone(), 4096, usize::MAX, 4096).unwrap();
        let pages =
            Arc::new(SharedPages::new_fixed(4096, axhal::paging::PageSize::Size4K).unwrap());
        pages.retain_allocation_owner(charge).unwrap();
        let mapping = pages.clone();
        drop(pages);
        assert_eq!(owner.load(Ordering::Acquire), 4096);
        drop(mapping);
        assert_eq!(owner.load(Ordering::Acquire), 0);
    }

    #[test]
    fn charges_refund_only_after_the_last_reference() {
        let owner = Arc::new(AtomicUsize::new(0));
        let charge =
            GemMemoryCharge::reserve_with_limits(owner.clone(), 4096, usize::MAX, 4096).unwrap();
        assert!(GemMemoryCharge::reserve_with_limits(owner.clone(), 1, usize::MAX, 4096).is_err());
        let retained = charge.clone();
        drop(charge);
        assert_eq!(owner.load(Ordering::Acquire), 4096);
        drop(retained);
        assert_eq!(owner.load(Ordering::Acquire), 0);
    }
}
