//! Membarrier and TLB-shootdown state of an address space.

use super::*;

/// Registration and generation state for private expedited membarriers.
///
/// The registration bits are process-image state: ordinary fork copies them,
/// while exec starts with a fresh address space and therefore a fresh state.
/// The generation and acknowledgements are deliberately separate from the
/// address-space TLB generation; a barrier must never be mistaken for a page
/// table shootdown acknowledgement.
pub(crate) struct MembarrierState {
    pub(super) registrations: AtomicU32,
    pub(super) generation: AtomicU64,
    pub(super) ack_generations: [AtomicU64; axconfig::plat::MAX_CPU_NUM],
}

impl MembarrierState {
    /// `MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED`. Linux's `membarrier_state`
    /// also carries non-ready companion bits (`*_READY`, and a
    /// `MEMBARRIER_STATE_GLOBAL_EXPEDITED` mirror per runqueue); this kernel
    /// keeps one ready bit per registration command because the registration
    /// only has to be reported back by `MEMBARRIER_CMD_GET_REGISTRATIONS`.
    pub(super) const REGISTER_GLOBAL_EXPEDITED: u32 = 1 << 2;
    pub(super) const REGISTER_PRIVATE: u32 = 1 << 4;
    pub(super) const REGISTER_SYNC_CORE: u32 = 1 << 6;

    pub(crate) const fn new() -> Self {
        Self::with_registrations(0)
    }

    pub(super) const fn with_registrations(registrations: u32) -> Self {
        Self {
            registrations: AtomicU32::new(registrations),
            generation: AtomicU64::new(0),
            ack_generations: [const { AtomicU64::new(0) }; axconfig::plat::MAX_CPU_NUM],
        }
    }

    pub(crate) fn fork_clone(&self) -> Self {
        Self::with_registrations(self.registrations.load(Ordering::Acquire))
    }

    pub(crate) fn register_global(&self) {
        self.registrations
            .fetch_or(Self::REGISTER_GLOBAL_EXPEDITED, Ordering::AcqRel);
    }

    pub(crate) fn register_private(&self) {
        self.registrations
            .fetch_or(Self::REGISTER_PRIVATE, Ordering::AcqRel);
    }

    pub(crate) fn register_sync_core(&self) {
        // Linux keeps the ordinary and sync-core private expedited
        // registrations independent: registering sync-core alone must not
        // authorize MEMBARRIER_CMD_PRIVATE_EXPEDITED.
        self.registrations
            .fetch_or(Self::REGISTER_SYNC_CORE, Ordering::AcqRel);
    }

    /// The `MEMBARRIER_CMD_REGISTER_*` bits, which are exactly what
    /// `MEMBARRIER_CMD_GET_REGISTRATIONS` reports (Linux's
    /// `membarrier_get_registrations()` maps each ready state to its
    /// registration command bit).
    pub(crate) fn registrations(&self) -> u32 {
        self.registrations.load(Ordering::Acquire)
            & (Self::REGISTER_GLOBAL_EXPEDITED | Self::REGISTER_PRIVATE | Self::REGISTER_SYNC_CORE)
    }

    pub(crate) fn private_registered(&self) -> bool {
        self.registrations() & Self::REGISTER_PRIVATE != 0
    }

    pub(crate) fn sync_core_registered(&self) -> bool {
        self.registrations() & Self::REGISTER_SYNC_CORE != 0
    }

    /// Completes a barrier generation for a CPU that is entering this image
    /// after an issuer took its resident snapshot. Entry hooks use the same
    /// generation as the IPI path, and conservatively execute the x86
    /// serializing primitive even when the original command was the cheaper
    /// ordinary private barrier. This closes the admission/snapshot race
    /// without taking a scheduler or address-space lock.
    pub(crate) fn synchronize_entering_cpu(&self, cpu: usize) {
        let generation = self.generation.load(Ordering::SeqCst);
        if generation == 0 || self.acknowledged(cpu, generation) {
            return;
        }
        fence(Ordering::SeqCst);
        #[cfg(target_arch = "x86_64")]
        {
            let _ = core::arch::x86_64::__cpuid(0);
        }
        fence(Ordering::SeqCst);
        self.acknowledge(cpu, generation);
    }

    pub(crate) fn next_generation(&self) -> AxResult<u64> {
        self.generation
            .try_update(Ordering::SeqCst, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map(|previous| previous + 1)
            .map_err(|_| AxError::from(axerrno::LinuxError::EOVERFLOW))
    }

    pub(crate) fn acknowledged(&self, cpu: usize, generation: u64) -> bool {
        assert!(
            cpu < axconfig::plat::MAX_CPU_NUM,
            "membarrier CPU index exceeds fixed capacity"
        );
        self.ack_generations[cpu].load(Ordering::Acquire) >= generation
    }

    pub(crate) fn acknowledge(&self, cpu: usize, generation: u64) {
        assert!(
            cpu < axconfig::plat::MAX_CPU_NUM,
            "membarrier CPU index exceeds fixed capacity"
        );
        let _ = self.ack_generations[cpu].try_update(
            Ordering::Release,
            Ordering::Acquire,
            |previous| Some(previous.max(generation)),
        );
    }
}

/// The virtual memory address space.
pub(crate) struct TlbState {
    pub(super) generation: AtomicU64,
    pub(super) resident_cpus: [AtomicBool; axconfig::plat::MAX_CPU_NUM],
    pub(super) seen_generations: [AtomicU64; axconfig::plat::MAX_CPU_NUM],
    pub(super) membarrier: MembarrierState,
    pub(super) ldt: SpinNoIrq<Option<Arc<Ldt>>>,
}

impl TlbState {
    pub(super) const fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            resident_cpus: [const { AtomicBool::new(false) }; axconfig::plat::MAX_CPU_NUM],
            seen_generations: [const { AtomicU64::new(0) }; axconfig::plat::MAX_CPU_NUM],
            membarrier: MembarrierState::new(),
            ldt: SpinNoIrq::new(None),
        }
    }

    pub(super) fn fork_clone(&self) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            generation: AtomicU64::new(0),
            resident_cpus: [const { AtomicBool::new(false) }; axconfig::plat::MAX_CPU_NUM],
            seen_generations: [const { AtomicU64::new(0) }; axconfig::plat::MAX_CPU_NUM],
            membarrier: self.membarrier.fork_clone(),
            ldt: SpinNoIrq::new(None),
        })
        .map_err(|_| AxError::NoMemory)
    }

    /// Publishes membership before sampling the generation. The returned
    /// generation is acknowledged only after the caller has completed the
    /// local flush, so a writer cannot mistake an entering CPU for one that
    /// already repaired its translations.
    pub(super) fn admit_cpu(&self, cpu: usize) -> Option<u64> {
        assert!(
            cpu < axconfig::plat::MAX_CPU_NUM,
            "address-space TLB CPU index exceeds fixed capacity"
        );
        self.resident_cpus[cpu].store(true, Ordering::SeqCst);
        let generation = self.generation.load(Ordering::SeqCst);
        let seen = self.seen_generations[cpu].load(Ordering::SeqCst);
        (seen < generation).then_some(generation)
    }

    pub(crate) fn enter_current(&self) {
        let _guard = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        if let Some(generation) = self.admit_cpu(cpu) {
            axhal::asm::flush_tlb(None);
            self.seen_generations[cpu].store(generation, Ordering::SeqCst);
        }
        self.membarrier.synchronize_entering_cpu(cpu);
        self.reload_current_ldt();
    }

    /// Reloads the current CPU's descriptor. Callers keep IRQs/preemption
    /// disabled so the per-CPU GDT cannot be concurrently changed.
    pub(crate) fn reload_current_ldt(&self) {
        let ldt = self.ldt.lock();
        let (base, len) = ldt.as_ref().map_or((core::ptr::null(), 0), |table| {
            (table.bytes().as_ptr(), table.bytes().len())
        });
        // SAFETY: callers keep IRQs and preemption disabled, the table is held by this address
        // space's `Arc<Ldt>` under the lock, and a replaced table is dropped only after the TLB
        // grace completes, so it describes a valid LDT for as long as any CPU may use it.
        unsafe { axhal::asm::load_user_ldt(base, len) };
    }

    pub(super) fn replace_ldt(&self, new: Option<Arc<Ldt>>) -> Option<Arc<Ldt>> {
        core::mem::replace(&mut *self.ldt.lock(), new)
    }

    pub(super) fn snapshot_ldt(&self) -> Option<Arc<Ldt>> {
        self.ldt.lock().clone()
    }

    pub(crate) fn membarrier_state(&self) -> &MembarrierState {
        &self.membarrier
    }

    pub(crate) fn membarrier_resident_on(&self, cpu: usize) -> bool {
        assert!(
            cpu < axconfig::plat::MAX_CPU_NUM,
            "membarrier CPU index exceeds fixed capacity"
        );
        self.resident_cpus[cpu].load(Ordering::SeqCst)
    }

    pub(super) fn synchronize_after_mutation(&self) -> impl Drop {
        super::super::synchronize_tlb_for_addr_space(
            &self.generation,
            &self.resident_cpus,
            &self.seen_generations,
        )
    }
}

#[derive(Clone, Copy)]
pub(super) struct LazyFreePage {
    pub(super) generation: u64,
    pub(super) paddr: PhysAddr,
    pub(super) restore_flags: MappingFlags,
}

/// Detached policy state for a prepared fixed replacement.  All fallible
/// cloning and any B-tree growth needed by an incoming locked map finishes
/// before old PTE/VMA withdrawal; publication is a plain field swap.
pub(super) struct PreparedFixedReplacementSidecars {
    pub(super) growdown_starts: BTreeSet<VirtAddr>,
    pub(super) madvise_guard_ranges: BTreeMap<VirtAddr, VirtAddr>,
    pub(super) madvise_hwpoison_ranges: BTreeMap<VirtAddr, VirtAddr>,
    pub(super) madvise_free_pages: BTreeMap<VirtAddr, LazyFreePage>,
    pub(super) wipe_on_fork_ranges: BTreeMap<VirtAddr, VirtAddr>,
    pub(super) dontfork_ranges: BTreeMap<VirtAddr, VirtAddr>,
    pub(super) dontdump_ranges: Vec<(VirtAddr, VirtAddr)>,
    pub(super) locked_ranges: BTreeMap<VirtAddr, VirtAddr>,
}
