//! User address space management and user-space memory access.

mod access;
mod aspace;
#[cfg(feature = "mm-lock-diagnostics")]
mod diagnostics;
mod fault;
mod io;
mod loader;
mod remap;
mod stats;
mod tlb;
mod userfaultfd;

#[cfg(feature = "mm-lock-diagnostics")]
// This is the MM-local control surface for the proc/runner integration slice.
#[allow(unused_imports)]
pub use self::diagnostics::{
    MM_LOCK_HISTOGRAM_BUCKETS, MmLockDiagnosticsResetError, MmLockDiagnosticsSetError,
    MmLockDiagnosticsSnapshot, MmLockStage, MmLockStageSnapshot, mm_lock_diagnostics_enabled,
    mm_lock_diagnostics_snapshot, reset_mm_lock_diagnostics, set_mm_lock_diagnostics_enabled,
};
pub use self::{access::*, aspace::*, io::*, loader::*, stats::*};
pub(crate) use self::{
    fault::handle_user_page_fault,
    remap::remap_user_mapping,
    tlb::{
        init as init_tlb_shootdown, repair_local_spurious_fault, retire_after_tlb_grace,
        synchronize_icache, synchronize_tlb, synchronize_tlb_and_icache,
    },
    userfaultfd::*,
};

/// Acquires an existing MM lock, adding timing only in an explicit
/// `mm-lock-diagnostics` build. The default expansion remains the raw lock
/// operation, with no clock reads or counter updates.
macro_rules! lock_mm_diagnosed {
    ($handle:expr, $stage:ident) => {{
        #[cfg(feature = "mm-lock-diagnostics")]
        {
            let stage = $crate::mm::diagnostics::MmLockStage::$stage;
            let sample = $crate::mm::diagnostics::begin_mm_lock(stage);
            let guard = ($handle).lock();
            $crate::mm::diagnostics::finish_mm_lock(guard, sample)
        }
        #[cfg(not(feature = "mm-lock-diagnostics"))]
        {
            ($handle).lock()
        }
    }};
}

pub(crate) use lock_mm_diagnosed;

pub(crate) fn checked_align_up(value: usize, align: usize) -> Option<usize> {
    if !align.is_power_of_two() {
        return None;
    }
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}

pub(crate) fn checked_align_up_4k(value: usize) -> Option<usize> {
    checked_align_up(value, memory_addr::PAGE_SIZE_4K)
}
