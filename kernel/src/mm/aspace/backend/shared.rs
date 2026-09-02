use alloc::{sync::Arc, vec::Vec};
use core::{
    any::Any,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use axerrno::{AxError, AxResult};
use axhal::paging::{MappingFlags, PageSize, PageTable, PageTableCursor, PagingError};
use axsync::Mutex;
use hashbrown::HashMap;
use memory_addr::{PAGE_SIZE_4K, PhysAddr, VirtAddr, VirtAddrRange};

use super::{
    super::SharedBackingKey, AddrSpace, Backend, BackendOps, BackendRetirement, FutexBackingId,
    FutexBackingIdentity, FutexWordOffset, MappingStatus, PopulateOutcome, SharedFutexKey,
    alloc_frame, dealloc_frame, divide_page, page_table_flags, pages_in, preflight_sparse_leaves,
    preflight_sparse_unmap,
};
use crate::{
    file::{DeferredFileLease, FileHandle, FileLike, FileMmapProtection, PreparedFileMmap},
    mm::{FileLikeMappingLease, FileMappingSharing, secret::SecretFrame},
};

static FIXED_SHARED_MAPPING_ID: AtomicU64 = AtomicU64::new(1);
// Resident anonymous/SysV shared pages. File-backed mappings deliberately do
// not use this path: their cache pages are not anonymous shmem pages.
static SHMEM_RESIDENT_PAGES: AtomicUsize = AtomicUsize::new(0);

const FOLIO_4K_PAGES: usize = PageSize::Size2M as usize / PageSize::Size4K as usize;

/// An owned 2 MiB allocation represented by 512 indexed 4 KiB entries.
///
/// `SharedPageStorage::pages` remains the authoritative indexed lookup table:
/// callers never need to know whether an entry was originally allocated as a
/// base page or as part of a folio.  This side table exists only to give the
/// allocator the correct lifetime and deallocation granularity.
#[derive(Clone)]
struct SharedFolio {
    start_index: usize,
    paddr: PhysAddr,
    /// The exact former base frames stay owned until the folio is demoted.
    /// Keeping them is what makes a failed cross-mm publication reversible:
    /// rolling the PTEs back never revives already-freed physical memory.
    old_pages: Vec<PhysAddr>,
}

/// Allocation-only half of a shmem 4 KiB -> 2 MiB promotion.
///
/// The MADV_COLLAPSE alias transaction creates this before revoking WRITE
/// from any CPU translation.  Publishing it later only copies already-owned
/// frames and moves pre-reserved metadata into `SharedPageStorage`, so that
/// phase cannot fail due to allocation.  Dropping an uncommitted value returns
/// exactly the temporary frames it owns.
pub struct PreparedSharedFolioPromotion {
    start_index: usize,
    folio: Option<PhysAddr>,
    missing_pages: Vec<(usize, PhysAddr)>,
    old_pages: Vec<PhysAddr>,
}

impl PreparedSharedFolioPromotion {
    pub fn folio(&self) -> PhysAddr {
        self.folio
            .expect("prepared shared folio lost its allocation")
    }
}

impl Drop for PreparedSharedFolioPromotion {
    fn drop(&mut self) {
        if let Some(folio) = self.folio.take() {
            dealloc_frame(folio, PageSize::Size2M);
        }
        for (_, frame) in self.missing_pages.drain(..) {
            dealloc_frame(frame, PageSize::Size4K);
        }
    }
}

struct SharedPageStorage {
    // `None` is a sparse anonymous-shmem hole.  Keeping logical slots in the
    // vector preserves stable futex/backing offsets while allowing
    // MADV_REMOVE to return resident frames to the allocator.  A later fault
    // materializes a fresh zeroed frame under the same mutex.
    pages: Vec<Option<PhysAddr>>,
    folios: Vec<SharedFolio>,
}

pub fn shmem_resident_pages() -> usize {
    SHMEM_RESIDENT_PAGES.load(Ordering::Acquire)
}

/// Converts resident backing frames to Linux `NR_SHMEM` base-page units.
///
/// The backing can use a huge page, but sysinfo's `sharedram` is expressed in
/// bytes from a count of 4 KiB base pages. Keeping the charge in that unit
/// makes allocation, growth, and final drop independent of huge-page
/// promotion or demotion.
fn shmem_base_pages(resident_frames: usize, page_size: PageSize) -> usize {
    resident_frames.saturating_mul(page_size as usize / PAGE_SIZE_4K)
}

fn charge_shmem_pages(charge: &AtomicUsize, resident_frames: usize, page_size: PageSize) {
    charge.fetch_add(
        shmem_base_pages(resident_frames, page_size),
        Ordering::Release,
    );
}

fn uncharge_shmem_pages(charge: &AtomicUsize, resident_frames: usize, page_size: PageSize) {
    let pages = shmem_base_pages(resident_frames, page_size);
    let _ = charge.try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
        n.checked_sub(pages)
    });
}

pub struct SharedPages {
    backing_key: SharedBackingKey,
    phys_pages: Mutex<SharedPageStorage>,
    // Secret objects keep their frames in a separate owner.  The ordinary
    // `phys_pages` vector is deliberately empty for these objects so no
    // helper can accidentally obtain a direct-map alias.
    secret_frames: Option<Mutex<HashMap<usize, SecretFrame>>>,
    secret_size: Option<AtomicUsize>,
    // Growing a zero-length secret backing publishes its page count before
    // its logical EOF. Faults therefore either see the old EOF and SIGBUS or
    // the fully admitted new range.
    secret_growth: Option<Mutex<()>>,
    // `futex_id` is queried while an IRQ-safe futex queue gate is held. Keep
    // the published length separate from `phys_pages`: taking that mutex in
    // the gate would be a blocking operation. The backing only grows, so a
    // stale (smaller) snapshot can cause a retry but can never expose a word
    // beyond the live allocation.
    published_len: AtomicUsize,
    /// Mutable file-visible EOF for fixed file-backed shared objects.  Frame
    /// ownership may outlive truncate because live VMAs retain it, but faults
    /// beyond this boundary must still become SIGBUS.
    logical_eof: AtomicUsize,
    pub size: PageSize,
    fixed: bool,
    // A fixed direct view snapshots direct-map pointers for IRQ use. Holding
    // this count prevents 4 KiB <-> 2 MiB backing replacement.
    direct_view_pins: AtomicUsize,
    resident_charge: Option<&'static AtomicUsize>,
    /// Device-owned pages have no allocator ownership in this kernel.  The
    /// lease pins the PCI aperture/resource mapping until the last VMA/GEM
    /// reference goes away; final drop must never return these frames to RAM.
    external_lease: Option<Arc<ExternalPageLease>>,
    /// Immutable cache/type attributes supplied by the external owner.
    /// They are merged into every PTE publication and cannot be removed by
    /// mprotect or a later remap.
    external_mapping_flags: MappingFlags,
}

/// Lifetime anchor for an externally owned exact-4K physical-page vector.
///
/// The opaque owner is normally the VirtGPU MAP_BLOB resource/aperture lease.
/// Keeping it behind an Arc makes VMA split/fork fragments and exported GEM
/// handles all participate in the same final-release boundary without ever
/// teaching the page allocator that PCI memory is ordinary RAM.
pub struct ExternalPageLease {
    owner: Arc<dyn Any + Send + Sync>,
    transport_owner: Option<Arc<dyn Any + Send + Sync>>,
    live: AtomicBool,
    /// Serializes the reset revoke transition against a populate operation.
    /// A mapper holds this while it validates liveness and publishes PTEs;
    /// reset flips `live` under the same gate before walking reverse maps.
    gate: Mutex<()>,
}

impl ExternalPageLease {
    pub fn new(owner: Arc<dyn Any + Send + Sync>) -> Self {
        Self {
            owner,
            transport_owner: None,
            live: AtomicBool::new(true),
            gate: Mutex::new(()),
        }
    }

    /// In addition to the MAP_BLOB owner, retain the device transport state.
    /// This is required for remove while a userspace VMA still exists: PCI
    /// BAR teardown cannot race an external PTE that has not reached its
    /// final drop.
    pub fn new_with_transport(
        owner: Arc<dyn Any + Send + Sync>,
        transport_owner: Arc<dyn Any + Send + Sync>,
    ) -> Self {
        Self {
            owner,
            transport_owner: Some(transport_owner),
            live: AtomicBool::new(true),
            gate: Mutex::new(()),
        }
    }

    pub fn owner(&self) -> &Arc<dyn Any + Send + Sync> {
        &self.owner
    }

    /// Reset/remove permanently revokes CPU mapping access before the BAR or
    /// resource owner may be torn down. Existing VMA fragments retain this
    /// object, but future faults fail rather than publishing stale PCI PTEs.
    pub fn mark_dead(&self) {
        let _gate = self.gate.lock();
        self.live.store(false, Ordering::Release);
    }

    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }

    fn mapping_gate(&self) -> AxResult<axsync::MutexGuard<'_, ()>> {
        let gate = self.gate.lock();
        if self.is_live() {
            Ok(gate)
        } else {
            Err(AxError::Io)
        }
    }
}
impl SharedPages {
    pub(crate) const fn is_secret(&self) -> bool {
        self.secret_frames.is_some()
    }
    /// Allocates a fixed, 4K secret backing.  Kernel copies use the secret
    /// window; VMA population consumes only the physical frame number.
    pub(crate) fn new_secret_fixed(size: usize) -> AxResult<Self> {
        let count = size.div_ceil(PAGE_SIZE_4K);
        Ok(Self {
            backing_key: SharedBackingKey::allocate()?,
            phys_pages: Mutex::new(SharedPageStorage {
                pages: Vec::new(),
                folios: Vec::new(),
            }),
            secret_frames: Some(Mutex::new(HashMap::new())),
            secret_size: Some(AtomicUsize::new(size)),
            secret_growth: Some(Mutex::new(())),
            published_len: AtomicUsize::new(count),
            logical_eof: AtomicUsize::new(size),
            size: PageSize::Size4K,
            fixed: true,
            direct_view_pins: AtomicUsize::new(0),
            resident_charge: None,
            external_lease: None,
            external_mapping_flags: MappingFlags::empty(),
        })
    }
    pub fn new(size: usize, page_size: PageSize) -> AxResult<Self> {
        Self::new_with_growth(size, page_size, true)
    }

    /// Allocates an exact shared backing which can never grow after
    /// publication. All frames and vector capacity are acquired by this call.
    pub fn new_fixed(size: usize, page_size: PageSize) -> AxResult<Self> {
        Self::new_with_growth(size, page_size, false)
    }

    /// Allocates SysV shared pages and charges only frames that were actually
    /// obtained.  The charge lives with the backing Arc through its final drop.
    pub fn new_sysv_charged(size: usize, page_size: PageSize) -> AxResult<Self> {
        // SysV segments have a fixed shm_segsz; unlike anonymous shared
        // mappings they cannot grow through a later range extension.
        Self::new_with_growth_charged(size, page_size, false, Some(&SHMEM_RESIDENT_PAGES))
    }

    /// Constructs a growable anonymous MAP_SHARED backing accounted as shmem.
    pub fn new_shmem(size: usize, page_size: PageSize) -> AxResult<Self> {
        Self::new_with_growth_charged(size, page_size, true, Some(&SHMEM_RESIDENT_PAGES))
    }

    fn new_with_growth(size: usize, page_size: PageSize, growable: bool) -> AxResult<Self> {
        Self::new_with_growth_charged(size, page_size, growable, None)
    }

    fn new_with_growth_charged(
        size: usize,
        page_size: PageSize,
        growable: bool,
        resident_charge: Option<&'static AtomicUsize>,
    ) -> AxResult<Self> {
        if !page_size.is_aligned(size) {
            return Err(AxError::InvalidInput);
        }
        // Reserve identity before acquiring frames so key exhaustion leaves no
        // allocation or resident charge to unwind.
        let backing_key = SharedBackingKey::allocate()?;
        let num_pages = size / page_size as usize;
        let mut phys_pages = Vec::new();
        phys_pages
            .try_reserve_exact(num_pages)
            .map_err(|_| AxError::NoMemory)?;
        for _ in 0..num_pages {
            match alloc_frame(true, page_size) {
                Ok(frame) => phys_pages.push(frame),
                Err(err) => {
                    for frame in phys_pages {
                        dealloc_frame(frame, page_size);
                    }
                    return Err(err);
                }
            }
        }
        if let Some(charge) = resident_charge {
            charge_shmem_pages(charge, num_pages, page_size);
        }
        Ok(Self {
            backing_key,
            phys_pages: Mutex::new(SharedPageStorage {
                pages: phys_pages.into_iter().map(Some).collect(),
                folios: Vec::new(),
            }),
            secret_frames: None,
            secret_size: None,
            secret_growth: None,
            published_len: AtomicUsize::new(num_pages),
            logical_eof: AtomicUsize::new(size),
            size: page_size,
            fixed: !growable,
            direct_view_pins: AtomicUsize::new(0),
            resident_charge,
            external_lease: None,
            external_mapping_flags: MappingFlags::empty(),
        })
    }

    /// Builds an immutable vector of device-owned 4 KiB pages.  The caller
    /// has already validated the aperture range and retains the transport
    /// mapping through `lease`; this constructor deliberately performs no RAM
    /// allocation, charge, direct-map view, folio conversion, or reclaim.
    pub fn new_external_4k(
        pages: Vec<PhysAddr>,
        lease: Arc<ExternalPageLease>,
        mapping_flags: MappingFlags,
    ) -> AxResult<Self> {
        if pages.is_empty()
            || pages
                .iter()
                .any(|page| !PageSize::Size4K.is_aligned(page.as_usize()))
            || !mapping_flags.contains(MappingFlags::DEVICE | MappingFlags::UNCACHED)
        {
            return Err(AxError::InvalidInput);
        }
        let page_count = pages.len();
        Ok(Self {
            backing_key: SharedBackingKey::allocate()?,
            phys_pages: Mutex::new(SharedPageStorage {
                pages: pages.into_iter().map(Some).collect(),
                folios: Vec::new(),
            }),
            secret_frames: None,
            secret_size: None,
            secret_growth: None,
            published_len: AtomicUsize::new(page_count),
            logical_eof: AtomicUsize::new(page_count * PAGE_SIZE_4K),
            size: PageSize::Size4K,
            fixed: true,
            direct_view_pins: AtomicUsize::new(0),
            resident_charge: None,
            external_lease: Some(lease),
            external_mapping_flags: MappingFlags::DEVICE | MappingFlags::UNCACHED,
        })
    }

    pub const fn is_external(&self) -> bool {
        self.external_lease.is_some()
    }

    /// Whether this object is the growable anonymous-shmem backing used by
    /// anonymous MAP_SHARED.  Fixed control/device/SysV objects and
    /// unaccounted kernel shared buffers must never be punched through
    /// MADV_REMOVE.
    pub(crate) const fn supports_madv_remove(&self) -> bool {
        self.resident_charge.is_some()
            && !self.fixed
            && self.secret_frames.is_none()
            && self.external_lease.is_none()
            && matches!(self.size, PageSize::Size4K)
    }

    pub(crate) fn external_live(&self) -> bool {
        self.external_lease
            .as_ref()
            .is_none_or(|lease| lease.is_live())
    }

    fn with_external_mapping<T>(&self, op: impl FnOnce() -> AxResult<T>) -> AxResult<T> {
        if let Some(lease) = &self.external_lease {
            let _gate = lease.mapping_gate()?;
            op()
        } else {
            op()
        }
    }

    fn mapping_flags(&self) -> MappingFlags {
        self.external_mapping_flags
    }

    /// Stable reverse-map identity for this backing's complete lifetime.
    pub(crate) const fn backing_key(&self) -> SharedBackingKey {
        self.backing_key
    }

    pub const fn page_size(&self) -> PageSize {
        self.size
    }

    pub const fn is_fixed(&self) -> bool {
        self.fixed
    }

    pub fn len(&self) -> usize {
        self.secret_size.as_ref().map_or_else(
            || self.phys_pages.lock().pages.len(),
            |size| size.load(Ordering::Acquire).div_ceil(PAGE_SIZE_4K),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.phys_pages.lock().pages.is_empty()
    }

    /// Publishes the first nonzero logical size for a zero-length secret
    /// backing.  Existing mappings retain this Arc and begin faulting pages
    /// only after this publication.
    pub(crate) fn set_secret_size_once(&self, size: usize) -> AxResult {
        if size == 0 {
            return Ok(());
        }
        let secret_size = self.secret_size.as_ref().ok_or(AxError::InvalidInput)?;
        let _growth = self
            .secret_growth
            .as_ref()
            .expect("secret growth gate")
            .lock();
        if secret_size.load(Ordering::Acquire) != 0 {
            return Err(AxError::InvalidInput);
        }
        secret_size.store(size, Ordering::Release);
        Ok(())
    }

    pub fn ensure_len(&self, len: usize) -> AxResult {
        if self.secret_frames.is_some() {
            return (len <= self.len())
                .then_some(())
                .ok_or(AxError::InvalidInput);
        }
        let current_len = self.phys_pages.lock().pages.len();
        if current_len >= len {
            return Ok(());
        }
        if self.fixed {
            return Err(AxError::InvalidInput);
        }

        let mut new_pages = Vec::new();
        new_pages
            .try_reserve_exact(len - current_len)
            .map_err(|_| AxError::NoMemory)?;
        for _ in current_len..len {
            match alloc_frame(true, self.size) {
                Ok(frame) => new_pages.push(frame),
                Err(err) => {
                    for frame in new_pages {
                        dealloc_frame(frame, self.size);
                    }
                    return Err(err);
                }
            }
        }

        let mut pages = self.phys_pages.lock();
        if pages.pages.len() >= len {
            drop(pages);
            for frame in new_pages {
                dealloc_frame(frame, self.size);
            }
            return Ok(());
        }
        let needed = len - pages.pages.len();
        if pages.pages.try_reserve_exact(needed).is_err() {
            drop(pages);
            for frame in new_pages {
                dealloc_frame(frame, self.size);
            }
            return Err(AxError::NoMemory);
        }
        let unused = new_pages.split_off(needed);
        pages.pages.extend(new_pages.into_iter().map(Some));
        if let Some(charge) = self.resident_charge {
            charge_shmem_pages(charge, needed, self.size);
        }
        let published_len = pages.pages.len();
        drop(pages);
        self.published_len.store(published_len, Ordering::Release);
        let bytes = published_len.saturating_mul(self.size as usize);
        let _ = self
            .logical_eof
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                (old < bytes).then_some(bytes)
            });
        for frame in unused {
            dealloc_frame(frame, self.size);
        }
        Ok(())
    }

    /// Returns the resident frame for one logical slot, materializing a
    /// zero-filled page after a sparse-shmem punch.  Allocation and resident
    /// charging are serialized with hole creation by `phys_pages`.
    fn materialize_page_locked(
        &self,
        pages: &mut SharedPageStorage,
        index: usize,
    ) -> AxResult<PhysAddr> {
        let slot = pages.pages.get_mut(index).ok_or(AxError::InvalidInput)?;
        if let Some(frame) = *slot {
            return Ok(frame);
        }
        let frame = alloc_frame(true, self.size)?;
        *slot = Some(frame);
        if let Some(charge) = self.resident_charge {
            charge_shmem_pages(charge, 1, self.size);
        }
        Ok(frame)
    }

    pub fn total_bytes(&self) -> usize {
        self.len() * self.size as usize
    }

    pub(crate) fn set_logical_eof(&self, eof: usize) -> AxResult<()> {
        if eof > self.total_bytes() {
            return Err(AxError::InvalidInput);
        }
        self.logical_eof.store(eof, Ordering::Release);
        Ok(())
    }

    /// The range that may fault and be copied for a secret object. Like
    /// Linux's secretmem page-cache backing, a non-page-aligned i_size owns
    /// the complete final page; bytes in its tail begin zeroed and remain
    /// addressable. The following page faults as SIGBUS.
    fn secret_accessible_bytes(&self) -> Option<usize> {
        self.secret_size.as_ref().and_then(|size| {
            size.load(Ordering::Acquire)
                .checked_add(PAGE_SIZE_4K - 1)
                .map(|end| end & !(PAGE_SIZE_4K - 1))
        })
    }

    fn total_bytes_snapshot(&self) -> usize {
        self.secret_size.as_ref().map_or_else(
            || self.published_len.load(Ordering::Acquire) * self.size as usize,
            |size| size.load(Ordering::Acquire),
        )
    }

    pub fn read_bytes(&self, offset: usize, mut buf: &mut [u8]) -> AxResult {
        if offset.checked_add(buf.len()).ok_or(AxError::InvalidInput)?
            > self
                .secret_accessible_bytes()
                .unwrap_or_else(|| self.total_bytes())
        {
            return Err(AxError::InvalidInput);
        }

        let page_bytes = self.size as usize;
        if let Some(frames) = &self.secret_frames {
            let mut pages = frames.lock();
            let mut page_index = offset / page_bytes;
            let mut page_offset = offset % page_bytes;
            while !buf.is_empty() {
                let chunk_len = (page_bytes - page_offset).min(buf.len());
                secret_page(&mut pages, page_index)?.copy_to(&mut buf[..chunk_len], page_offset)?;
                buf = &mut buf[chunk_len..];
                page_index += 1;
                page_offset = 0;
            }
            return Ok(());
        }
        let mut pages = self.phys_pages.lock();
        let mut page_index = offset / page_bytes;
        let mut page_offset = offset % page_bytes;

        while !buf.is_empty() {
            let phys = self.materialize_page_locked(&mut pages, page_index)?;
            let src = axhal::mem::phys_to_virt(phys).as_usize() + page_offset;
            let chunk_len = (page_bytes - page_offset).min(buf.len());
            unsafe {
                core::ptr::copy_nonoverlapping(src as *const u8, buf.as_mut_ptr(), chunk_len);
            }
            let (_, rest) = buf.split_at_mut(chunk_len);
            buf = rest;
            page_index += 1;
            page_offset = 0;
        }

        Ok(())
    }

    /// Copies from an already-materialized secret backing without allocating
    /// a frame.  IRQ-safe nofault users need this distinction: a missing
    /// secret page remains a retry, not an implicit population.
    pub(crate) fn read_secret_bytes_resident(&self, offset: usize, mut buf: &mut [u8]) -> AxResult {
        let frames = self.secret_frames.as_ref().ok_or(AxError::InvalidInput)?;
        if offset.checked_add(buf.len()).ok_or(AxError::InvalidInput)?
            > self
                .secret_accessible_bytes()
                .unwrap_or_else(|| self.total_bytes())
        {
            return Err(AxError::InvalidInput);
        }
        let pages = frames.lock();
        let mut page_index = offset / PAGE_SIZE_4K;
        let mut page_offset = offset % PAGE_SIZE_4K;
        while !buf.is_empty() {
            let chunk_len = (PAGE_SIZE_4K - page_offset).min(buf.len());
            pages
                .get(&page_index)
                .ok_or(AxError::BadAddress)?
                .copy_to(&mut buf[..chunk_len], page_offset)?;
            buf = &mut buf[chunk_len..];
            page_index += 1;
            page_offset = 0;
        }
        Ok(())
    }

    pub fn write_bytes(&self, offset: usize, mut buf: &[u8]) -> AxResult {
        if offset.checked_add(buf.len()).ok_or(AxError::InvalidInput)?
            > self
                .secret_accessible_bytes()
                .unwrap_or_else(|| self.total_bytes())
        {
            return Err(AxError::InvalidInput);
        }

        let page_bytes = self.size as usize;
        if let Some(frames) = &self.secret_frames {
            let mut pages = frames.lock();
            let mut page_index = offset / page_bytes;
            let mut page_offset = offset % page_bytes;
            while !buf.is_empty() {
                let chunk_len = (page_bytes - page_offset).min(buf.len());
                secret_page(&mut pages, page_index)?.copy_from(&buf[..chunk_len], page_offset)?;
                buf = &buf[chunk_len..];
                page_index += 1;
                page_offset = 0;
            }
            return Ok(());
        }
        let mut pages = self.phys_pages.lock();
        let mut page_index = offset / page_bytes;
        let mut page_offset = offset % page_bytes;

        while !buf.is_empty() {
            let phys = self.materialize_page_locked(&mut pages, page_index)?;
            let dst = axhal::mem::phys_to_virt(phys).as_usize() + page_offset;
            let chunk_len = (page_bytes - page_offset).min(buf.len());
            unsafe {
                core::ptr::copy_nonoverlapping(buf.as_ptr(), dst as *mut u8, chunk_len);
            }
            buf = &buf[chunk_len..];
            page_index += 1;
            page_offset = 0;
        }

        Ok(())
    }

    /// Writes an already-materialized secret backing without allocating a
    /// frame; see [`Self::read_secret_bytes_resident`].
    pub(crate) fn write_secret_bytes_resident(&self, offset: usize, mut buf: &[u8]) -> AxResult {
        let frames = self.secret_frames.as_ref().ok_or(AxError::InvalidInput)?;
        if offset.checked_add(buf.len()).ok_or(AxError::InvalidInput)?
            > self
                .secret_accessible_bytes()
                .unwrap_or_else(|| self.total_bytes())
        {
            return Err(AxError::InvalidInput);
        }
        let pages = frames.lock();
        let mut page_index = offset / PAGE_SIZE_4K;
        let mut page_offset = offset % PAGE_SIZE_4K;
        while !buf.is_empty() {
            let chunk_len = (PAGE_SIZE_4K - page_offset).min(buf.len());
            pages
                .get(&page_index)
                .ok_or(AxError::BadAddress)?
                .copy_from(&buf[..chunk_len], page_offset)?;
            buf = &buf[chunk_len..];
            page_index += 1;
            page_offset = 0;
        }
        Ok(())
    }

    fn with_pages_range<T>(
        &self,
        start_index: usize,
        count: usize,
        use_pages: impl FnOnce(&[PhysAddr]) -> AxResult<T>,
    ) -> AxResult<T> {
        if let Some(frames) = &self.secret_frames {
            let mut frames = frames.lock();
            let end = start_index
                .checked_add(count)
                .ok_or(AxError::InvalidInput)?;
            if end > self.len() {
                return Err(AxError::NoMemory);
            }
            let mut pages = Vec::new();
            pages
                .try_reserve_exact(count)
                .map_err(|_| AxError::NoMemory)?;
            for index in start_index..end {
                pages.push(secret_page(&mut frames, index)?.physical());
            }
            return use_pages(&pages);
        }
        let mut storage = self.phys_pages.lock();
        let end = start_index
            .checked_add(count)
            .ok_or(AxError::InvalidInput)?;
        if end > storage.pages.len() {
            return Err(AxError::NoMemory);
        }
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(count)
            .map_err(|_| AxError::NoMemory)?;
        for index in start_index..end {
            pages.push(self.materialize_page_locked(&mut storage, index)?);
        }
        use_pages(&pages)
    }

    /// Returns the physical address for a 4 KiB indexed backing entry.
    ///
    /// A promoted folio still exposes all 512 entries through this interface,
    /// as consecutive physical addresses derived from its 2 MiB base.
    pub fn paddr_at(&self, index: usize) -> AxResult<PhysAddr> {
        if let Some(frames) = &self.secret_frames {
            let mut frames = frames.lock();
            if index >= self.len() {
                return Err(AxError::InvalidInput);
            }
            return Ok(secret_page(&mut frames, index)?.physical());
        }
        let mut pages = self.phys_pages.lock();
        self.materialize_page_locked(&mut pages, index)
    }

    /// Commits a sparse hole after every alias PTE in this range has been
    /// detached and globally invalidated by the syscall transaction.
    ///
    /// Fully covered frames are returned to the allocator and uncharged;
    /// partial boundary pages retain their frame and are zeroed.  Logical
    /// length (`published_len`) deliberately stays unchanged: a hole is still
    /// part of the shmem object and future faults materialize a fresh zeroed
    /// page at the same backing offset.
    pub(crate) fn remove_range(&self, offset: usize, length: usize) -> AxResult<()> {
        if !self.supports_madv_remove() {
            return Err(AxError::OperationNotSupported);
        }
        let end = offset.checked_add(length).ok_or(AxError::InvalidInput)?;
        if end > self.total_bytes() {
            return Err(AxError::NoMemory);
        }
        if length == 0 {
            return Ok(());
        }

        let page_size = self.size as usize;
        let first = offset / page_size;
        let last = end.div_ceil(page_size);
        let mut storage = self.phys_pages.lock();
        if last > storage.pages.len() {
            return Err(AxError::NoMemory);
        }
        if self.direct_view_pins.load(Ordering::Acquire) != 0
            || storage.folios.iter().any(|folio| {
                let folio_end = folio.start_index + FOLIO_4K_PAGES;
                first < folio_end && folio.start_index < last
            })
        {
            // The outer transaction demotes all promoted aliases before PTE
            // invalidation. Reaching this branch means its stable snapshot was
            // violated and no frame may be freed.
            return Err(AxError::ResourceBusy);
        }
        let mut released = 0usize;
        for index in first..last {
            let page_start = index * page_size;
            let covered_start = offset.max(page_start);
            let covered_end = end.min(page_start + page_size);
            if covered_start == page_start && covered_end == page_start + page_size {
                if let Some(frame) = storage.pages[index].take() {
                    dealloc_frame(frame, self.size);
                    released += 1;
                }
                continue;
            }
            let Some(frame) = storage.pages[index] else {
                continue;
            };
            let zero_start = covered_start - page_start;
            let zero_end = covered_end - page_start;
            unsafe {
                core::ptr::write_bytes(
                    axhal::mem::phys_to_virt(frame).as_mut_ptr().add(zero_start),
                    0,
                    zero_end - zero_start,
                );
            }
        }
        if let Some(charge) = self.resident_charge {
            uncharge_shmem_pages(charge, released, self.size);
        }
        Ok(())
    }

    fn physical_page(&self, index: usize) -> AxResult<PhysAddr> {
        self.paddr_at(index)
    }

    /// Snapshots the source 4 KiB frames retained by a promoted folio.
    ///
    /// Callers use this before changing any alias PTE so all allocation can
    /// fail before publication.  The returned frames stay owned by the folio
    /// until [`Self::demote_4k_folio`] commits the ownership transition.
    pub fn demote_4k_folio_frames(&self, start_index: usize) -> AxResult<Vec<PhysAddr>> {
        if self.is_external() {
            return Err(AxError::OperationNotSupported);
        }
        if self.size != PageSize::Size4K || !start_index.is_multiple_of(FOLIO_4K_PAGES) {
            return Err(AxError::InvalidInput);
        }
        let storage = self.phys_pages.lock();
        if self.direct_view_pins.load(Ordering::Acquire) != 0 {
            return Err(AxError::ResourceBusy);
        }
        let folio = storage
            .folios
            .iter()
            .find(|folio| folio.start_index == start_index)
            .ok_or(AxError::InvalidInput)?;
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(FOLIO_4K_PAGES)
            .map_err(|_| AxError::NoMemory)?;
        frames.extend_from_slice(&folio.old_pages);
        Ok(frames)
    }

    pub fn has_4k_folio(&self, start_index: usize) -> bool {
        self.size == PageSize::Size4K
            && self
                .phys_pages
                .lock()
                .folios
                .iter()
                .any(|folio| folio.start_index == start_index)
    }

    /// Reserves every allocation required by a future 4 KiB -> 2 MiB folio
    /// publication.  The returned object owns temporary pages until commit.
    pub fn prepare_4k_folio_promotion(
        &self,
        start_index: usize,
    ) -> AxResult<PreparedSharedFolioPromotion> {
        if self.is_external() {
            return Err(AxError::OperationNotSupported);
        }
        if self.size != PageSize::Size4K || !start_index.is_multiple_of(FOLIO_4K_PAGES) {
            return Err(AxError::InvalidInput);
        }
        let end = start_index
            .checked_add(FOLIO_4K_PAGES)
            .ok_or(AxError::InvalidInput)?;
        let mut storage = self.phys_pages.lock();
        if self.direct_view_pins.load(Ordering::Acquire) != 0 {
            return Err(AxError::ResourceBusy);
        }
        if end > storage.pages.len()
            || storage.folios.iter().any(|folio| {
                let folio_end = folio.start_index + FOLIO_4K_PAGES;
                start_index < folio_end && folio.start_index < end
            })
        {
            return Err(AxError::InvalidInput);
        }
        storage
            .folios
            .try_reserve_exact(1)
            .map_err(|_| AxError::NoMemory)?;
        let mut old_pages = Vec::new();
        old_pages
            .try_reserve_exact(FOLIO_4K_PAGES)
            .map_err(|_| AxError::NoMemory)?;
        let missing_count = storage.pages[start_index..end]
            .iter()
            .filter(|page| page.is_none())
            .count();
        let mut missing_pages = Vec::new();
        missing_pages
            .try_reserve_exact(missing_count)
            .map_err(|_| AxError::NoMemory)?;
        for index in start_index..end {
            if storage.pages[index].is_some() {
                continue;
            }
            match alloc_frame(true, self.size) {
                Ok(frame) => missing_pages.push((index, frame)),
                Err(error) => {
                    for (_, frame) in missing_pages {
                        dealloc_frame(frame, self.size);
                    }
                    return Err(error);
                }
            }
        }

        let folio = match alloc_frame(false, PageSize::Size2M) {
            Ok(folio) => folio,
            Err(error) => {
                for (_, frame) in missing_pages {
                    dealloc_frame(frame, self.size);
                }
                return Err(error);
            }
        };
        let mut missing = missing_pages.iter().copied().peekable();
        for index in start_index..end {
            let source = match storage.pages[index] {
                Some(frame) => frame,
                None => {
                    let (missing_index, frame) = missing
                        .next()
                        .expect("preallocated sparse folio source disappeared");
                    debug_assert_eq!(missing_index, index);
                    frame
                }
            };
            old_pages.push(source);
        }
        Ok(PreparedSharedFolioPromotion {
            start_index,
            folio: Some(folio),
            missing_pages,
            old_pages,
        })
    }

    /// Copies and publishes a fully prepared folio after the caller has
    /// revoked writable aliases and completed its first TLB grace period.
    /// All fallible allocation and metadata growth happened in preparation;
    /// invariant failures here are kernel bugs rather than recoverable
    /// user-visible outcomes.
    pub fn commit_4k_folio_promotion(
        &self,
        mut prepared: PreparedSharedFolioPromotion,
    ) -> PhysAddr {
        let folio = prepared
            .folio
            .take()
            .expect("prepared shared folio lost its allocation");
        let end = prepared.start_index + FOLIO_4K_PAGES;
        let mut storage = self.phys_pages.lock();
        assert!(
            end <= storage.pages.len(),
            "prepared shmem folio escaped its backing"
        );
        assert!(
            !storage.folios.iter().any(|candidate| {
                let candidate_end = candidate.start_index + FOLIO_4K_PAGES;
                prepared.start_index < candidate_end && candidate.start_index < end
            }),
            "prepared shmem folio raced another promotion"
        );
        assert!(
            storage.folios.len() < storage.folios.capacity(),
            "prepared shmem folio lost its metadata reservation"
        );
        assert_eq!(prepared.old_pages.len(), FOLIO_4K_PAGES);
        for (offset, &source) in prepared.old_pages.iter().enumerate() {
            let destination = axhal::mem::phys_to_virt(PhysAddr::from(
                folio.as_usize() + offset * PageSize::Size4K as usize,
            ));
            unsafe {
                core::ptr::copy_nonoverlapping(
                    axhal::mem::phys_to_virt(source).as_ptr(),
                    destination.as_mut_ptr(),
                    PageSize::Size4K as usize,
                );
            }
        }
        for (offset, entry) in storage.pages[prepared.start_index..end]
            .iter_mut()
            .enumerate()
        {
            *entry = Some(PhysAddr::from(
                folio.as_usize() + offset * PageSize::Size4K as usize,
            ));
        }
        storage.folios.push(SharedFolio {
            start_index: prepared.start_index,
            paddr: folio,
            old_pages: core::mem::take(&mut prepared.old_pages),
        });
        if let Some(charge) = self.resident_charge {
            charge_shmem_pages(charge, prepared.missing_pages.len(), self.size);
        }
        // These frames are now represented by the folio's old-pages list or
        // indexed backing slots, so Drop must not release them.
        prepared.missing_pages.clear();
        folio
    }

    /// Legacy one-shot helper for callers which do not need a cross-mm PTE
    /// transaction.  MADV_COLLAPSE uses the split prepare/commit API above.
    pub fn promote_4k_folio(&self, start_index: usize) -> AxResult<PhysAddr> {
        let prepared = self.prepare_4k_folio_promotion(start_index)?;
        Ok(self.commit_4k_folio_promotion(prepared))
    }

    /// Transactionally restore the base-page backing for a promoted folio.
    ///
    /// This is the inverse ownership transition.  The exact source 4 KiB
    /// frames are retained by the promoted folio, so no allocation is needed
    /// and a transaction can restore the original backing identity exactly.
    pub fn demote_4k_folio(&self, start_index: usize) -> AxResult {
        if self.is_external() {
            return Err(AxError::OperationNotSupported);
        }
        if self.size != PageSize::Size4K || !start_index.is_multiple_of(FOLIO_4K_PAGES) {
            return Err(AxError::InvalidInput);
        }
        let end = start_index
            .checked_add(FOLIO_4K_PAGES)
            .ok_or(AxError::InvalidInput)?;
        let mut storage = self.phys_pages.lock();
        if self.direct_view_pins.load(Ordering::Acquire) != 0 {
            return Err(AxError::ResourceBusy);
        }
        let folio_index = storage
            .folios
            .iter()
            .position(|folio| folio.start_index == start_index)
            .ok_or(AxError::InvalidInput)?;
        if end > storage.pages.len() {
            return Err(AxError::BadState);
        }
        let folio = &storage.folios[folio_index];
        if folio.old_pages.len() != FOLIO_4K_PAGES {
            return Err(AxError::BadState);
        }
        for (offset, &destination) in folio.old_pages.iter().enumerate() {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    axhal::mem::phys_to_virt(PhysAddr::from(
                        folio.paddr.as_usize() + offset * PageSize::Size4K as usize,
                    ))
                    .as_ptr(),
                    axhal::mem::phys_to_virt(destination).as_mut_ptr(),
                    PageSize::Size4K as usize,
                );
            }
        }
        let folio = storage.folios.remove(folio_index);
        for (slot, frame) in storage.pages[start_index..end]
            .iter_mut()
            .zip(folio.old_pages.iter().copied())
        {
            *slot = Some(frame);
        }
        dealloc_frame(folio.paddr, PageSize::Size2M);
        Ok(())
    }

    /// Captures a direct-map view suitable for IRQ use. Construction may
    /// allocate and take the backing mutex; the returned operations do neither.
    pub(crate) fn fixed_view(self: &Arc<Self>) -> AxResult<SharedFixedView> {
        if !self.fixed
            || self.size != PageSize::Size4K
            || self.secret_frames.is_some()
            || self.is_external()
        {
            return Err(AxError::OperationNotSupported);
        }
        let storage = self.phys_pages.lock();
        let total_bytes = storage
            .pages
            .len()
            .checked_mul(PAGE_SIZE_4K)
            .ok_or(AxError::InvalidInput)?;
        let mut page_bases = Vec::new();
        page_bases
            .try_reserve_exact(storage.pages.len())
            .map_err(|_| AxError::NoMemory)?;
        for page in &storage.pages {
            let page = (*page).ok_or(AxError::BadState)?;
            page_bases.push(
                NonNull::new(axhal::mem::phys_to_virt(page).as_usize() as *mut u8)
                    .ok_or(AxError::BadState)?,
            );
        }
        self.direct_view_pins
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .map_err(|_| AxError::ResourceBusy)?;
        let inner = match Arc::try_new(FixedSharedViewInner {
            pages: self.clone(),
            page_bases,
            total_bytes,
        }) {
            Ok(inner) => inner,
            Err(_) => {
                self.direct_view_pins.fetch_sub(1, Ordering::Release);
                return Err(AxError::NoMemory);
            }
        };
        drop(storage);
        Ok(SharedFixedView { inner })
    }

    /// Returns an aligned, bounds-checked atomic view into a fixed backing.
    /// The handle exposes only Acquire loads and Release stores.
    pub fn atomic_u32(self: &Arc<Self>, offset: usize) -> AxResult<SharedAtomicU32> {
        self.fixed_view()?.atomic_u32(offset)
    }
}

/// A lifecycle-fixed direct view of a 4 KiB shared backing. Clones share one
/// pin. Its final drop must run in task context: releasing the retained
/// backing Arc may reclaim frames under its mutex.
#[derive(Clone)]
pub struct SharedFixedView {
    inner: Arc<FixedSharedViewInner>,
}

struct FixedSharedViewInner {
    pages: Arc<SharedPages>,
    page_bases: Vec<NonNull<u8>>,
    total_bytes: usize,
}

impl Drop for FixedSharedViewInner {
    fn drop(&mut self) {
        self.pages.direct_view_pins.fetch_sub(1, Ordering::Release);
    }
}

unsafe impl Send for SharedFixedView {}
unsafe impl Sync for SharedFixedView {}

impl SharedFixedView {
    pub(crate) fn len(&self) -> usize {
        self.inner.total_bytes
    }

    /// Writes a complete ring record, wrapping at `base + size`.
    ///
    /// # Safety
    ///
    /// The caller must serialize producers and prove this record does not
    /// overlap bytes concurrently read by a consumer or written by another
    /// producer. The ring head/tail protocol must establish that ownership.
    /// All bounds are checked before copying, so an error writes no bytes.
    pub(crate) unsafe fn write_wrapped(
        &self,
        base: usize,
        size: usize,
        offset: usize,
        bytes: &[u8],
    ) -> AxResult {
        validate_wrapped_write(self.len(), base, size, offset, bytes.len())?;
        let first = bytes.len().min(size - offset);
        self.copy_at(base + offset, &bytes[..first]);
        self.copy_at(base, &bytes[first..]);
        Ok(())
    }

    pub(crate) fn atomic_u32(&self, offset: usize) -> AxResult<SharedAtomicU32> {
        Ok(SharedAtomicU32 {
            address: self.atomic_address::<AtomicU32>(offset)?,
            view: self.clone(),
        })
    }

    pub(crate) fn atomic_u64(&self, offset: usize) -> AxResult<SharedAtomicU64> {
        Ok(SharedAtomicU64 {
            address: self.atomic_address::<AtomicU64>(offset)?,
            view: self.clone(),
        })
    }

    fn atomic_address<T>(&self, offset: usize) -> AxResult<NonNull<T>> {
        validate_atomic_offset::<T>(self.len(), PAGE_SIZE_4K, offset)?;
        let page = offset / PAGE_SIZE_4K;
        let in_page = offset % PAGE_SIZE_4K;
        let address = (self.inner.page_bases[page].as_ptr() as usize)
            .checked_add(in_page)
            .ok_or(AxError::InvalidInput)?;
        NonNull::new(address as *mut T).ok_or(AxError::BadState)
    }

    fn copy_at(&self, mut offset: usize, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let page = offset / PAGE_SIZE_4K;
            let in_page = offset % PAGE_SIZE_4K;
            let count = bytes.len().min(PAGE_SIZE_4K - in_page);
            // SAFETY: write_wrapped validated the full range against this
            // pinned view, and its safety contract excludes concurrent bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    self.inner.page_bases[page].as_ptr().add(in_page),
                    count,
                );
            }
            offset += count;
            bytes = &bytes[count..];
        }
    }
}

fn secret_page(pages: &mut HashMap<usize, SecretFrame>, index: usize) -> AxResult<&SecretFrame> {
    if !pages.contains_key(&index) {
        let frame = SecretFrame::allocate()?;
        pages.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        pages.insert(index, frame);
    }
    Ok(pages.get(&index).expect("secret frame inserted"))
}

fn validate_wrapped_write(
    total_bytes: usize,
    base: usize,
    size: usize,
    offset: usize,
    bytes_len: usize,
) -> AxResult {
    if size == 0
        || !size.is_power_of_two()
        || offset >= size
        || bytes_len > size
        || base.checked_add(size).is_none_or(|end| end > total_bytes)
        || base.checked_add(offset).is_none()
    {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn validate_atomic_offset<T>(total_bytes: usize, page_size: usize, offset: usize) -> AxResult {
    if page_size < core::mem::size_of::<T>()
        || !offset.is_multiple_of(core::mem::align_of::<T>())
        || offset
            .checked_add(core::mem::size_of::<T>())
            .is_none_or(|end| end > total_bytes)
        || offset % page_size > page_size - core::mem::size_of::<T>()
    {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

/// A lifetime-pinned atomic word stored in a fixed shared-page backing.
///
/// This handle, and [`SharedFixedView`], must be dropped from task context:
/// their final Arc drop can release the backing and take its frame mutex.
pub struct SharedAtomicU32 {
    address: NonNull<AtomicU32>,
    view: SharedFixedView,
}

// The target is naturally aligned, points into immutable backing storage, and
// is accessed exclusively through AtomicU32 operations.
unsafe impl Send for SharedAtomicU32 {}
unsafe impl Sync for SharedAtomicU32 {}

impl Clone for SharedAtomicU32 {
    fn clone(&self) -> Self {
        Self {
            address: self.address,
            view: self.view.clone(),
        }
    }
}

impl SharedAtomicU32 {
    pub fn load_acquire(&self) -> u32 {
        // SAFETY: construction validated alignment and the view pins frames.
        atomic_load_acquire(self.address)
    }

    pub fn store_release(&self, value: u32) {
        // SAFETY: see load_acquire; AtomicU32 supplies the shared mutation law.
        atomic_store_release(self.address, value);
    }
}

/// A lifetime-pinned 64-bit metadata word for shared producer/consumer rings.
/// Its final drop has the same task-context requirement as [`SharedAtomicU32`].
pub struct SharedAtomicU64 {
    address: NonNull<AtomicU64>,
    view: SharedFixedView,
}

unsafe impl Send for SharedAtomicU64 {}
unsafe impl Sync for SharedAtomicU64 {}

impl Clone for SharedAtomicU64 {
    fn clone(&self) -> Self {
        Self {
            address: self.address,
            view: self.view.clone(),
        }
    }
}

impl SharedAtomicU64 {
    pub fn load_acquire(&self) -> u64 {
        unsafe { self.address.as_ref() }.load(Ordering::Acquire)
    }

    pub fn store_release(&self, value: u64) {
        unsafe { self.address.as_ref() }.store(value, Ordering::Release);
    }
}

fn atomic_load_acquire(address: NonNull<AtomicU32>) -> u32 {
    // SAFETY: callers supply a live, naturally aligned AtomicU32.
    unsafe { address.as_ref() }.load(Ordering::Acquire)
}

fn atomic_store_release(address: NonNull<AtomicU32>, value: u32) {
    // SAFETY: callers supply a live, naturally aligned AtomicU32.
    unsafe { address.as_ref() }.store(value, Ordering::Release);
}

impl Drop for SharedPages {
    fn drop(&mut self) {
        // Dropping SecretFrame performs window zeroing, restores the direct
        // alias only after the wipe, and finally returns the frame.
        if self.secret_frames.is_some() || self.is_external() {
            return;
        }
        let storage = self.phys_pages.lock();
        let mut resident = 0usize;
        for (index, frame) in storage.pages.iter().enumerate() {
            if !storage.folios.iter().any(|folio| {
                (folio.start_index..folio.start_index + FOLIO_4K_PAGES).contains(&index)
            }) && let Some(frame) = *frame
            {
                dealloc_frame(frame, self.size);
                resident += 1;
            }
        }
        for folio in &storage.folios {
            dealloc_frame(folio.paddr, PageSize::Size2M);
            for &frame in &folio.old_pages {
                dealloc_frame(frame, PageSize::Size4K);
            }
            resident += FOLIO_4K_PAGES;
        }
        if let Some(charge) = self.resident_charge {
            uncharge_shmem_pages(charge, resident, self.size);
        }
    }
}

// FIXME: This implementation does not allow map or unmap partial ranges.
#[derive(Clone)]
pub struct SharedBackend {
    start: VirtAddr,
    page_offset: usize,
    pages: Arc<SharedPages>,
    may_protect: MappingFlags,
    map_id: SharedMapId,
    status: MappingStatus,
}

#[derive(Clone)]
enum SharedMapId {
    Dynamic(Arc<()>),
    Fixed(u64),
}

impl SharedMapId {
    fn same_mapping(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Dynamic(lhs), Self::Dynamic(rhs)) => Arc::ptr_eq(lhs, rhs),
            (Self::Fixed(lhs), Self::Fixed(rhs)) => lhs == rhs,
            (Self::Dynamic(_), Self::Fixed(_)) | (Self::Fixed(_), Self::Dynamic(_)) => false,
        }
    }
}

impl SharedBackend {
    pub(super) fn preflight_map(&self, range: VirtAddrRange, flags: MappingFlags) -> AxResult {
        if !self.pages.external_live() {
            return Err(AxError::Io);
        }
        pages_in(range, self.pages.size)?;
        self.check_protect_flags(flags)
    }

    pub(crate) fn is_secret(&self) -> bool {
        self.pages.is_secret()
    }
    pub(crate) fn faults_with_sigbus(&self, vaddr: VirtAddr) -> bool {
        self.backing_offset(vaddr.as_usize()).is_none_or(|offset| {
            offset
                >= if self.pages.is_secret() {
                    self.pages.secret_accessible_bytes().expect("secret size")
                } else {
                    self.pages.logical_eof.load(Ordering::Acquire)
                }
        })
    }
    /// Clone this shared backing at a different page cursor while retaining
    /// its physical-object identity.  `remap_file_pages` uses this only while
    /// an AddrSpace replacement transaction owns the old VMA, so futex keys
    /// continue to name the same `SharedPages` object at the rebased offset.
    pub(crate) fn clone_rebased(&self, start: VirtAddr, page_offset: usize) -> AxResult<Self> {
        let byte_offset = page_offset
            .checked_mul(self.pages.size as usize)
            .ok_or(AxError::InvalidInput)?;
        if byte_offset >= self.pages.total_bytes() {
            return Err(AxError::InvalidInput);
        }
        Ok(Self {
            start,
            page_offset,
            pages: self.pages.clone(),
            may_protect: self.may_protect,
            map_id: self.map_id.clone(),
            status: self.status.clone(),
        })
    }

    pub(super) const fn mapping_status(&self) -> &MappingStatus {
        &self.status
    }

    pub(super) fn mapping_status_mut(&mut self) -> &mut MappingStatus {
        &mut self.status
    }

    pub fn pages(&self) -> &Arc<SharedPages> {
        &self.pages
    }

    pub(crate) fn backing_offset(&self, address: usize) -> Option<usize> {
        let relative = address.checked_sub(self.start.as_usize())?;
        self.page_offset
            .checked_mul(self.pages.size as usize)?
            .checked_add(relative)
    }

    pub(crate) fn futex_key(&self, address: usize) -> Option<SharedFutexKey> {
        let offset = self.backing_offset(address)?;
        let end = offset.checked_add(size_of::<u32>())?;
        (end <= self.pages.total_bytes()).then(|| {
            SharedFutexKey::new(
                FutexBackingIdentity::Shared(self.pages.clone()),
                FutexWordOffset::new(offset),
            )
        })
    }

    pub(crate) fn futex_id(&self, address: usize) -> Option<(FutexBackingId, FutexWordOffset)> {
        let offset = self.backing_offset(address)?;
        let end = offset.checked_add(size_of::<u32>())?;
        (end <= self.pages.total_bytes_snapshot()).then(|| {
            (
                FutexBackingId::shared(Arc::as_ptr(&self.pages) as usize),
                FutexWordOffset::new(offset),
            )
        })
    }

    pub(crate) fn compatible_with(&self, other: &Self) -> bool {
        self.map_id.same_mapping(&other.map_id)
            && self.start == other.start
            && self.page_offset == other.page_offset
            && Arc::ptr_eq(&self.pages, &other.pages)
    }

    fn clone_for_range_with_id(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
        map_id: SharedMapId,
    ) -> AxResult<Self> {
        let (start, backing_advance) =
            super::relocate_affine_origin(self.start, old_start, new_start)?;
        let backing_pages = divide_page(backing_advance, self.pages.size)?;
        let page_offset = self
            .page_offset
            .checked_add(backing_pages)
            .ok_or(AxError::InvalidInput)?;
        Ok(Self {
            start,
            page_offset,
            pages: self.pages.clone(),
            may_protect: self.may_protect,
            map_id,
            status: self.status.relocated(old_start, new_start)?,
        })
    }

    pub(crate) fn clone_for_range(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
    ) -> AxResult<Self> {
        self.clone_for_range_with_id(old_start, new_start, self.map_id.clone())
    }

    pub(crate) fn duplicate_mapping(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
    ) -> AxResult<Self> {
        // mremap(old_size = 0) duplicates a MAP_SHARED secret VMA.  It must
        // retain the same secret object but gets an independent map identity;
        // ordinary fixed control mappings remain non-duplicable.
        if self.pages.is_fixed() && !self.pages.is_secret() {
            return Err(AxError::OperationNotSupported);
        }
        let map_id = Arc::try_new(()).map_err(|_| AxError::NoMemory)?;
        self.clone_for_range_with_id(old_start, new_start, SharedMapId::Dynamic(map_id))
    }

    pub(crate) fn ensure_range_covered(&self, start: VirtAddr, size: usize) -> AxResult {
        // Growing a secret VMA never grows its immutable file backing.  New
        // pages are valid mappings but fault as SIGBUS once their backing
        // offset reaches i_size.
        if self.pages.is_secret() {
            return Ok(());
        }
        let offset = start
            .as_usize()
            .checked_sub(self.start.as_usize())
            .ok_or(AxError::InvalidInput)?;
        let start_index = self
            .page_offset
            .checked_add(divide_page(offset, self.pages.size)?)
            .ok_or(AxError::InvalidInput)?;
        let count = divide_page(size, self.pages.size)?;
        self.pages.ensure_len(
            start_index
                .checked_add(count)
                .ok_or(AxError::InvalidInput)?,
        )
    }

    pub(crate) fn check_protect_flags(&self, flags: MappingFlags) -> AxResult {
        let requested = flags & access_flags();
        if !self.may_protect.contains(requested) {
            return Err(AxError::PermissionDenied);
        }
        Ok(())
    }
}

impl BackendOps for SharedBackend {
    fn page_size(&self) -> PageSize {
        self.pages.size
    }

    fn map(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        _pt: &mut PageTableCursor,
    ) -> AxResult {
        debug!("Shared::map: {range:?} {flags:?}");
        self.preflight_map(range, flags)?;
        Ok(())
    }

    fn preflight_protect(
        &self,
        range: VirtAddrRange,
        new_flags: MappingFlags,
        pt: &PageTable,
    ) -> AxResult {
        self.check_protect_flags(new_flags)?;
        preflight_sparse_unmap(range, self.pages.size, pt)
    }

    fn unmap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult<BackendRetirement> {
        debug!("Shared::unmap: {range:?}");
        // `drain_present_leaves` handles both original folio-sized leaves and
        // the P1 children published by a prepared pkey demotion.
        let _ = pt.drain_present_leaves(range.start, range.size())?;
        Ok(BackendRetirement::empty())
    }

    fn preflight_unmap(&self, range: VirtAddrRange, pt: &PageTable) -> AxResult {
        preflight_sparse_leaves(range, pt)
    }

    fn populate(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        access_flags: MappingFlags,
        pt: &mut PageTableCursor,
    ) -> PopulateOutcome {
        let result = self.pages.with_external_mapping(|| {
            let offset = range
                .start
                .as_usize()
                .checked_sub(self.start.as_usize())
                .ok_or(AxError::InvalidInput)?;
            let start_index = self
                .page_offset
                .checked_add(divide_page(offset, self.pages.size)?)
                .ok_or(AxError::InvalidInput)?;
            let count = divide_page(range.size(), self.pages.size)?;
            let mut needs_tlb_sync = false;
            let result = {
                let mut populated = 0;
                for (index, vaddr) in
                    (start_index..start_index + count).zip(pages_in(range, self.pages.size)?)
                {
                    let paddr = self.pages.physical_page(index)?;
                    match pt.query(vaddr) {
                        Ok((mapped_paddr, page_flags, page_size)) => {
                            if page_size != self.pages.size || mapped_paddr != paddr {
                                return Err(AxError::BadAddress);
                            }
                            if access_flags.contains(MappingFlags::WRITE)
                                && !page_flags.contains(MappingFlags::WRITE)
                            {
                                pt.remap(
                                    vaddr,
                                    paddr,
                                    page_table_flags(flags | self.pages.mapping_flags()),
                                )?;
                                needs_tlb_sync = true;
                                populated += 1;
                            } else if page_flags.contains(access_flags) {
                                populated += 1;
                            }
                        }
                        Err(PagingError::NotMapped) => {
                            pt.map(
                                vaddr,
                                paddr,
                                self.pages.size,
                                page_table_flags(flags | self.pages.mapping_flags()),
                            )?;
                            populated += 1;
                        }
                        Err(_) => return Err(AxError::BadAddress),
                    }
                }
                Ok(populated)
            };
            if needs_tlb_sync {
                pt.flush();
                drop(crate::mm::synchronize_tlb());
            }
            result
        });
        PopulateOutcome::immediate(result)
    }

    fn clone_map(
        &self,
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _old_pt: &mut PageTableCursor,
        _new_pt: &mut PageTableCursor,
        _new_aspace: &Arc<Mutex<AddrSpace>>,
        _active_long_term_cow_frames: &[PhysAddr],
        _share_shadow_stack: bool,
    ) -> AxResult<Backend> {
        Ok(Backend::Shared(self.clone()))
    }
}

impl Backend {
    pub(crate) fn shared_pages(&self) -> Option<&Arc<SharedPages>> {
        match self {
            Self::Shared(shared) => Some(shared.pages()),
            Self::Linear(_) | Self::Cow(_) | Self::File(_) => None,
        }
    }

    /// Stable reverse-map identity for an anonymous/shared-memory backing.
    /// File mappings have their own cache identity and are deliberately not
    /// folded into this shmem alias registry.
    pub(crate) fn shared_backing_key(&self) -> Option<super::super::SharedBackingKey> {
        match self {
            Self::Shared(shared) => Some(shared.pages.backing_key()),
            Self::Linear(_) | Self::Cow(_) | Self::File(_) => None,
        }
    }

    pub fn new_shared(start: VirtAddr, pages: Arc<SharedPages>) -> Self {
        Self::new_shared_with_may_protect(start, pages, access_flags())
    }

    pub fn try_new_shared(start: VirtAddr, pages: Arc<SharedPages>) -> AxResult<Self> {
        Self::try_new_shared_with_may_protect(start, pages, access_flags())
    }

    pub fn try_new_shared_with_may_protect(
        start: VirtAddr,
        pages: Arc<SharedPages>,
        may_protect: MappingFlags,
    ) -> AxResult<Self> {
        let map_id = Arc::try_new(()).map_err(|_| AxError::NoMemory)?;
        Ok(Self::Shared(SharedBackend {
            start,
            page_offset: 0,
            pages,
            may_protect: may_protect & access_flags(),
            map_id: SharedMapId::Dynamic(map_id),
            status: MappingStatus::default(),
        }))
    }

    pub fn new_shared_with_may_protect(
        start: VirtAddr,
        pages: Arc<SharedPages>,
        may_protect: MappingFlags,
    ) -> Self {
        Self::Shared(SharedBackend {
            start,
            page_offset: 0,
            pages,
            may_protect: may_protect & access_flags(),
            map_id: SharedMapId::Dynamic(Arc::new(())),
            status: MappingStatus::default(),
        })
    }
}

/// Every fallible resource needed to bind a fixed object mapping.
///
/// Construction runs before the address-space lock. `into_backend` only moves
/// prevalidated ownership into the VMA backend and performs no allocation.
pub(crate) struct PreparedFixedSharedMapping {
    pages: Arc<SharedPages>,
    owner: Option<DeferredFileLease>,
    ofd_key: u64,
    object_offset: u64,
    page_offset: usize,
    initial_flags: MappingFlags,
    may_protect: MappingFlags,
    map_id: u64,
}

impl PreparedFixedSharedMapping {
    pub(crate) fn try_new(
        handle: FileHandle<dyn FileLike>,
        plan: PreparedFileMmap,
    ) -> AxResult<Self> {
        let request = plan.request();
        let region_offset = plan.region_offset();
        let pages = plan.pages().clone();
        if !pages.is_fixed()
            || request.offset() < region_offset
            || request
                .offset()
                .checked_sub(region_offset)
                .is_none_or(|relative| {
                    relative
                        .checked_add(request.length() as u64)
                        .is_none_or(|end| end > pages.total_bytes() as u64 && !pages.is_secret())
                })
            || request.page_size() != pages.page_size() as usize
        {
            return Err(AxError::InvalidInput);
        }

        let map_id = FIXED_SHARED_MAPPING_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| AxError::TooManyOpenFiles)?;
        let ofd_key = handle.open_file_description_key();
        let owner = if plan.retains_description() {
            let retained: Arc<dyn Any + Send + Sync> = pages.clone();
            Some(DeferredFileLease::try_new(handle, retained)?)
        } else {
            None
        };
        let may_protect = mapping_flags(plan.may_protect());
        Ok(Self {
            pages: plan.into_pages(),
            owner,
            ofd_key,
            object_offset: request.offset(),
            page_offset: ((request.offset() - region_offset) / pages.page_size() as u64) as usize,
            initial_flags: mapping_flags(request.protection()),
            may_protect,
            map_id,
        })
    }

    pub(crate) fn shared_backing_key(&self) -> SharedBackingKey {
        self.pages.backing_key()
    }

    pub(crate) fn into_backend(self, start: VirtAddr) -> Backend {
        let Self {
            pages,
            owner,
            ofd_key,
            object_offset,
            page_offset,
            initial_flags,
            may_protect,
            map_id,
        } = self;
        let mapping = match owner {
            Some(owner) => FileLikeMappingLease::new(
                owner,
                ofd_key,
                start,
                object_offset,
                initial_flags,
                may_protect,
                FileMappingSharing::Shared,
            ),
            None => FileLikeMappingLease::new_detached(
                map_id as usize,
                ofd_key,
                start,
                object_offset,
                initial_flags,
                may_protect,
                FileMappingSharing::Shared,
            ),
        };
        Backend::Shared(SharedBackend {
            start,
            page_offset,
            pages,
            may_protect: may_protect & access_flags(),
            map_id: SharedMapId::Fixed(map_id),
            status: MappingStatus::default(),
        })
        .with_file_like_mapping(mapping)
    }
}

fn mapping_flags(protection: FileMmapProtection) -> MappingFlags {
    let mut flags = MappingFlags::USER;
    if protection.contains(FileMmapProtection::READ) {
        flags |= MappingFlags::READ;
    }
    if protection.contains(FileMmapProtection::WRITE) {
        flags |= MappingFlags::WRITE;
    }
    if protection.contains(FileMmapProtection::EXECUTE) {
        flags |= MappingFlags::EXECUTE;
    }
    flags
}

fn access_flags() -> MappingFlags {
    MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shmem_charge_is_in_base_page_equivalents_for_all_backing_sizes() {
        assert_eq!(shmem_base_pages(1, PageSize::Size4K), 1);
        assert_eq!(shmem_base_pages(1, PageSize::Size2M), 512);
        assert_eq!(shmem_base_pages(1, PageSize::Size1G), 262_144);

        // The same resident byte range has one NR_SHMEM charge regardless of
        // whether its backing is represented as base pages or huge pages.
        assert_eq!(
            shmem_base_pages(512, PageSize::Size4K),
            shmem_base_pages(1, PageSize::Size2M)
        );
        assert_eq!(
            shmem_base_pages(512, PageSize::Size2M),
            shmem_base_pages(1, PageSize::Size1G)
        );
    }

    #[test]
    fn shmem_huge_backing_growth_and_drop_are_symmetric() {
        let _context = crate::test_support::scheduler_test_context();
        let baseline = shmem_resident_pages();
        let pages = SharedPages::new_shmem(PageSize::Size2M as usize, PageSize::Size2M).unwrap();
        assert_eq!(shmem_resident_pages(), baseline + 512);

        pages.ensure_len(2).unwrap();
        assert_eq!(shmem_resident_pages(), baseline + 1024);
        drop(pages);
        assert_eq!(shmem_resident_pages(), baseline);
    }

    #[test]
    fn shared_atomic_offsets_are_aligned_bounded_and_page_local() {
        assert!(validate_atomic_offset::<AtomicU32>(0x2000, 0x1000, 0).is_ok());
        assert!(validate_atomic_offset::<AtomicU32>(0x2000, 0x1000, 0xffc).is_ok());
        assert_eq!(
            validate_atomic_offset::<AtomicU32>(0x2000, 0x1000, 1),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            validate_atomic_offset::<AtomicU32>(0x2000, 0x1000, 0x2000),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            validate_atomic_offset::<AtomicU32>(3, 3, 0),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            validate_atomic_offset::<AtomicU64>(0x2000, 0x1000, 0xffc),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn shared_atomic_handle_uses_release_store_and_acquire_load() {
        let atomic = AtomicU32::new(7);
        let address = NonNull::from(&atomic);
        assert_eq!(atomic_load_acquire(address), 7);
        atomic_store_release(address, 29);
        assert_eq!(atomic.load(Ordering::Acquire), 29);
    }

    #[test]
    fn fixed_view_wraps_across_page_and_ring_end() {
        let _context = crate::test_support::scheduler_test_context();
        let pages = Arc::new(SharedPages::new_fixed(PAGE_SIZE_4K * 2, PageSize::Size4K).unwrap());
        let view = pages.fixed_view().unwrap();
        let base = PAGE_SIZE_4K - 4;
        // SAFETY: this test is the only producer and has no consumer.
        unsafe { view.write_wrapped(base, 8, 6, &[0xa1, 0xb2, 0xc3, 0xd4]) }.unwrap();
        let mut bytes = [0_u8; 8];
        pages.read_bytes(base, &mut bytes).unwrap();
        assert_eq!(bytes, [0xc3, 0xd4, 0, 0, 0, 0, 0xa1, 0xb2]);

        // SAFETY: this test is the only producer and has no consumer.
        unsafe { view.write_wrapped(base, 8, 0, &[1, 2, 3, 4, 5, 6, 7, 8]) }.unwrap();
        pages.read_bytes(base, &mut bytes).unwrap();
        assert_eq!(bytes, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn fixed_view_rejects_invalid_write_before_copying() {
        let _context = crate::test_support::scheduler_test_context();
        let pages = Arc::new(SharedPages::new_fixed(PAGE_SIZE_4K, PageSize::Size4K).unwrap());
        let view = pages.fixed_view().unwrap();
        assert_eq!(
            unsafe { view.write_wrapped(PAGE_SIZE_4K - 4, 8, 0, &[1]) },
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            unsafe { view.write_wrapped(0, 3, 0, &[1]) },
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            unsafe { view.write_wrapped(0, 8, 8, &[1]) },
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            unsafe { view.write_wrapped(0, 8, 0, &[1; 9]) },
            Err(AxError::InvalidInput)
        );
        let mut bytes = [0_u8; 8];
        pages.read_bytes(0, &mut bytes).unwrap();
        assert_eq!(bytes, [0; 8]);
    }

    #[test]
    fn fixed_view_atomics_pin_until_all_clones_drop() {
        let _context = crate::test_support::scheduler_test_context();
        let pages = Arc::new(
            SharedPages::new_fixed(PAGE_SIZE_4K * FOLIO_4K_PAGES, PageSize::Size4K).unwrap(),
        );
        let view = pages.fixed_view().unwrap();
        let clone = view.clone();
        let word = view.atomic_u64(8).unwrap();
        word.store_release(0x0123_4567_89ab_cdef);
        assert_eq!(word.load_acquire(), 0x0123_4567_89ab_cdef);
        assert!(matches!(
            view.atomic_u64(PAGE_SIZE_4K - 4),
            Err(AxError::InvalidInput)
        ));
        assert_eq!(pages.promote_4k_folio(0), Err(AxError::ResourceBusy));
        drop(word);
        drop(view);
        assert_eq!(pages.promote_4k_folio(0), Err(AxError::ResourceBusy));
        drop(clone);
        pages.promote_4k_folio(0).unwrap();
        let demote_view = pages.fixed_view().unwrap();
        assert_eq!(pages.demote_4k_folio(0), Err(AxError::ResourceBusy));
        drop(demote_view);
        pages.demote_4k_folio(0).unwrap();
    }

    #[test]
    fn secret_backing_is_sparse_and_faults_past_logical_eof() {
        let pages = Arc::new(SharedPages::new_secret_fixed(PAGE_SIZE_4K + 1).unwrap());
        assert_eq!(pages.len(), 2);
        assert!(pages.secret_frames.as_ref().unwrap().lock().is_empty());
        let backend = SharedBackend {
            start: VirtAddr::from(0x4000),
            page_offset: 0,
            pages,
            may_protect: access_flags(),
            map_id: SharedMapId::Fixed(1),
            status: MappingStatus::default(),
        };
        assert!(!backend.faults_with_sigbus(VirtAddr::from(0x4000)));
        assert!(!backend.faults_with_sigbus(VirtAddr::from(0x5000)));
        assert!(backend.faults_with_sigbus(VirtAddr::from(0x6000)));
    }

    #[test]
    fn zero_length_secret_backing_faults_every_mapping_access() {
        let pages = Arc::new(SharedPages::new_secret_fixed(0).unwrap());
        let backend = SharedBackend {
            start: VirtAddr::from(0x4000),
            page_offset: 0,
            pages,
            may_protect: access_flags(),
            map_id: SharedMapId::Fixed(1),
            status: MappingStatus::default(),
        };
        assert!(backend.faults_with_sigbus(VirtAddr::from(0x4000)));
        assert!(backend.faults_with_sigbus(VirtAddr::from(0x5000)));
    }

    #[test]
    fn secret_partial_last_page_is_accessible_but_following_page_sigbuses() {
        let _context = crate::test_support::scheduler_test_context();
        let pages = Arc::new(SharedPages::new_secret_fixed(PAGE_SIZE_4K + 1).unwrap());
        let backend = SharedBackend {
            start: VirtAddr::from(0x4000),
            page_offset: 0,
            pages: pages.clone(),
            may_protect: access_flags(),
            map_id: SharedMapId::Fixed(1),
            status: MappingStatus::default(),
        };
        let mut tail = [0xff_u8; 1];
        pages.read_bytes(PAGE_SIZE_4K + 0xffe, &mut tail).unwrap();
        assert_eq!(tail, [0]);
        pages.write_bytes(PAGE_SIZE_4K + 0xffe, &[0xa5]).unwrap();
        pages.read_bytes(PAGE_SIZE_4K + 0xffe, &mut tail).unwrap();
        assert_eq!(tail, [0xa5]);
        assert!(!backend.faults_with_sigbus(VirtAddr::from(0x5ffe)));
        assert!(backend.faults_with_sigbus(VirtAddr::from(0x6000)));
    }

    #[test]
    fn resident_secret_copy_never_populates_a_missing_frame() {
        let _context = crate::test_support::scheduler_test_context();
        let pages = SharedPages::new_secret_fixed(PAGE_SIZE_4K).unwrap();
        let mut byte = [0_u8; 1];
        assert_eq!(
            pages.read_secret_bytes_resident(0, &mut byte),
            Err(AxError::BadAddress)
        );
        pages.write_bytes(0, &[0x5a]).unwrap();
        pages.read_secret_bytes_resident(0, &mut byte).unwrap();
        assert_eq!(byte, [0x5a]);
    }

    #[test]
    fn secret_fixed_mapping_can_grow_and_duplicate_without_backing_growth() {
        let pages = Arc::new(SharedPages::new_secret_fixed(PAGE_SIZE_4K).unwrap());
        let backend = SharedBackend {
            start: VirtAddr::from(0x4000),
            page_offset: 0,
            pages: pages.clone(),
            may_protect: access_flags(),
            map_id: SharedMapId::Fixed(1),
            status: MappingStatus::default(),
        };
        backend
            .ensure_range_covered(VirtAddr::from(0x4000), PAGE_SIZE_4K * 2)
            .unwrap();
        assert_eq!(pages.len(), 1);
        assert!(backend.faults_with_sigbus(VirtAddr::from(0x5000)));

        let duplicate = backend
            .duplicate_mapping(VirtAddr::from(0x4000), VirtAddr::from(0x8000))
            .unwrap();
        assert!(Arc::ptr_eq(backend.pages(), duplicate.pages()));
        assert_eq!(duplicate.futex_id(0x8000), backend.futex_id(0x4000));
    }

    #[test]
    fn futex_id_uses_published_length_without_taking_pages_lock() {
        let _context = crate::test_support::scheduler_test_context();
        let pages = Arc::new(SharedPages::new(0, PageSize::Size4K).unwrap());
        let backend = SharedBackend {
            start: VirtAddr::from(0x4000),
            page_offset: 0,
            pages: pages.clone(),
            may_protect: access_flags(),
            map_id: SharedMapId::Dynamic(Arc::new(())),
            status: MappingStatus::default(),
        };
        // Holding the vector mutex proves that the gate-safe identity query
        // does not fall back to `total_bytes()` and block on the same lock.
        let pages_guard = pages.phys_pages.lock();
        assert_eq!(backend.futex_id(0x4000), None);
        drop(pages_guard);
        // SharedPages teardown needs task context in host tests; this test is
        // only about the nonblocking identity query.
        core::mem::forget(backend);
        core::mem::forget(pages);
    }

    #[test]
    fn low_address_shared_suffix_preserves_page_and_futex_cursors() {
        let origin = VirtAddr::from(0x4000);
        let source = VirtAddr::from(0x8000);
        let destination = VirtAddr::from(0x1000);
        let pages = Arc::new(SharedPages::new(0, PageSize::Size4K).unwrap());
        let map_id = SharedMapId::Dynamic(Arc::new(()));
        let backend = SharedBackend {
            start: origin,
            page_offset: 0,
            pages: pages.clone(),
            may_protect: access_flags(),
            map_id: map_id.clone(),
            status: MappingStatus::default(),
        };
        // Keep one zero-page owner alive: dropping the final kernel mutex has
        // no current task in host tests.
        core::mem::forget(pages);

        let first = backend
            .clone_for_range_with_id(source, destination, map_id.clone())
            .unwrap();
        let second_fragment = backend
            .clone_for_range_with_id(source, destination, map_id)
            .unwrap();

        assert_eq!(first.start, destination);
        assert_eq!(first.page_offset, 4);
        assert_eq!(first.backing_offset(destination.as_usize()), Some(0x4000));
        assert_eq!(
            first.backing_offset((destination + 0x1000).as_usize()),
            Some(0x5000)
        );
        assert!(first.compatible_with(&second_fragment));
    }
}
