//! The AddrSpace type, its teardown and anonymous-reclaim outcomes.

use super::*;

pub struct AddrSpace {
    pub(super) va_range: VirtAddrRange,
    pub(super) address_space_id: AddressSpaceId,
    pub(super) hardware_asid: HardwareAddressSpaceId,
    /// Historical peak of resident user pages for this memory image.
    ///
    /// The mark belongs to the mm rather than a particular task: CLONE_VM
    /// owners and remote memory operations all observe the same Linux
    /// process-wide high-water mark.
    pub(super) maxrss_kb: AtomicU64,
    /// Shared mm-level OOM-reaper ownership.  This is part of the address
    /// space rather than ProcessData because separate CLONE_VM process groups
    /// must never concurrently retire the same PTE/backing ownership.
    pub(super) oom_reap_state: AtomicU8,
    /// PR_SET_THP_DISABLE is an mm property and therefore naturally shared by
    /// every CLONE_VM owner. Ordinary fork copies it; exec explicitly carries
    /// it into the replacement image.
    pub(super) thp_disable_mode: ThpDisableMode,
    /// Monotonic PTE invalidation generation and bounded per-CPU residency.
    /// The state is shared with scheduler hooks so they can publish residency
    /// without taking the address-space mutex.
    pub(super) tlb: Arc<TlbState>,
    pub(super) topology_mapping_id: MappingId,
    pub(super) topology_generation: MappingGeneration,
    pub(super) areas: MemorySet<Backend>,
    pub(super) mapping_identities: MappingIdentityIndex,
    pub(super) growdown_starts: BTreeSet<VirtAddr>,
    /// MADV_GUARD_INSTALL overlays anonymous VMAs with faulting guard ranges
    /// without splitting their ownership/lineage.  PTEs are discarded at
    /// installation and page-fault admission consults this sidecar first.
    pub(super) madvise_guard_ranges: BTreeMap<VirtAddr, VirtAddr>,
    /// Software representation of MADV_HWPOISON pages.  The real hardware
    /// error path and this administrative injection share SIGBUS-class fault
    /// behavior through `BackingUnavailable` below.
    pub(super) madvise_hwpoison_ranges: BTreeMap<VirtAddr, VirtAddr>,
    /// Pages accepted by MADV_FREE.  A generation is assigned at admission
    /// and the leaf is write-protected; a later write fault consumes exactly
    /// that generation before restoring write access.
    pub(super) madvise_free_pages: BTreeMap<VirtAddr, LazyFreePage>,
    pub(super) next_madvise_free_generation: u64,
    pub(super) wipe_on_fork_ranges: BTreeMap<VirtAddr, VirtAddr>,
    pub(super) dontfork_ranges: BTreeMap<VirtAddr, VirtAddr>,
    /// VM_DONTDUMP policy installed by MADV_DONTDUMP. A sidecar preserves
    /// partial-VMA byte ranges without inventing a hardware PTE flag.
    pub(super) dontdump_ranges: Vec<(VirtAddr, VirtAddr)>,
    pub(super) locked_ranges: BTreeMap<VirtAddr, VirtAddr>,
    /// File-cache eviction fences published before aliases are write
    /// protected.  The address-space mutex protects this table together with
    /// the VMA/PTE topology, so an alias-publishing mutation can never race a
    /// prepared cache-page retirement unnoticed.
    pub(super) file_eviction_fences: Vec<FileEvictionFenceKey>,
    pub(super) next_file_eviction_fence_generation: u64,
    /// CET default shadow stacks are owned by Linux tasks, but the ownership
    /// lives in the mm that owns the VMA.  In particular, CLONE_VM peers may
    /// have distinct ProcessData objects, so keeping this in ProcessData (or
    /// walking a thread group) loses owners belonging to another sharer.
    ///
    /// This is deliberately a fallibly-grown vector rather than a global
    /// side table: it is protected by the same address-space mutex as the
    /// VMAs it names, and a task id occurs at most once in one mm.
    pub(super) cet_default_shadow_stacks: Vec<CetDefaultShadowStackOwner>,
    /// One weak reverse-map lease per live shared-memory backing referenced by
    /// this mm.  Keeping leases on the address space, rather than individual
    /// VMAs, makes split/merge/unmap lifecycle handling explicit and avoids a
    /// backend-to-mm ownership cycle.
    pub(super) alias_bindings: BTreeMap<SharedBackingKey, AliasLease>,

    /// Non-present anonymous leaves.  The hardware table deliberately has no
    /// encoding for them; this owner-side registry is the authoritative
    /// software PTE and keeps swap entries out of physical-address APIs.
    pub(super) swapped: BTreeMap<VirtAddr, crate::mm::SwapPte>,
    pub(super) user_io_pins: Box<UserIoPinRegistry>,
    pub(super) active_long_term_cow_pins: Vec<ActiveLongTermCowPin>,
    pub(in crate::mm) uffd: Option<Box<super::super::userfaultfd::UffdAddressSpaceState>>,
    pub(super) lock_future_mappings: bool,
    pub(super) lock_future_on_fault: bool,
    pub(super) pt: PageTable,
}

impl fmt::Debug for AddrSpace {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("AddrSpace")
            .field("va_range", &self.va_range)
            .field("page_table_root", &self.pt.root_paddr())
            .field("hardware_asid", &self.hardware_asid.asid())
            .field("hardware_asid_generation", &self.hardware_asid.generation())
            .field("areas", &self.areas)
            .finish()
    }
}

impl Drop for AddrSpace {
    fn drop(&mut self) {
        debug_assert_eq!(self.user_io_pins.progress().total(), 0);
        debug_assert!(self.active_long_term_cow_pins.is_empty());
        if let Err(err) = self.clear_areas_with_tlb_grace() {
            warn!("AddrSpace::drop: failed to unmap all areas: {err:?}");
        }
        let _ = self.user_io_pins.begin_teardown();
        let _ = self.user_io_pins.finish_teardown();
    }
}

/// Result of one anonymous-leaf reclaim attempt.
pub(super) enum AnonymousReclaim {
    /// The leaf was written to swap and its frame retired.
    Reclaimed,
    /// The leaf belonged to a `MAP_DROPPABLE` mapping: its frame was retired
    /// and the bytes were dropped without being written anywhere.
    Dropped,
    /// The leaf is not exclusively owned, not 4 KiB, not resident, or could
    /// not be paged out; it stays resident.
    NotEligible,
    /// No swap area is active, so no leaf can be paged out.
    NoSwapArea,
}
