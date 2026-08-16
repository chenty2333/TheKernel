//! User address space management and user-space memory access.

mod access;
mod asid;
mod aspace;
#[cfg(feature = "mm-lock-diagnostics")]
mod diagnostics;
mod fault;
mod io;
mod loader;
mod pressure;
mod remap;
mod stats;
mod tlb;
mod usercopy;
mod userfaultfd;

#[cfg(feature = "asid-switch-diagnostics")]
pub use axhal::context::asid_switch_diagnostics_snapshot;
#[cfg(all(feature = "asid-switch-diagnostics", feature = "test-io-control"))]
pub use axhal::context::{reset_asid_switch_diagnostics, set_asid_switch_diagnostics_enabled};

#[cfg(feature = "mm-lock-diagnostics")]
// This is the MM-local control surface for the proc/runner integration slice.
#[allow(unused_imports)]
pub use self::diagnostics::{
    MM_LOCK_HISTOGRAM_BUCKETS, MmLockDiagnosticsResetError, MmLockDiagnosticsSetError,
    MmLockDiagnosticsSnapshot, MmLockStage, MmLockStageSnapshot, mm_lock_diagnostics_enabled,
    mm_lock_diagnostics_snapshot, reset_mm_lock_diagnostics, set_mm_lock_diagnostics_enabled,
};
pub use self::{
    access::*, asid::AddressSpaceToken, aspace::*, io::*, loader::*, pressure::*, stats::*,
};
pub(crate) use self::{
    asid::init as init_hardware_asids,
    fault::handle_user_page_fault,
    pressure::init_memory_pressure,
    remap::remap_user_mapping,
    tlb::{
        init as init_tlb_shootdown, repair_local_spurious_fault, retire_after_tlb_grace,
        synchronize_icache, synchronize_tlb, synchronize_tlb_and_icache,
        synchronize_tlb_for_addr_space,
    },
    usercopy::{
        AddressSpaceUserMemory, UserMemoryCapability, map_usercopy_error, with_user_memory,
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
