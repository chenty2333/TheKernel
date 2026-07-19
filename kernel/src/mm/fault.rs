//! User-originated page-fault dispatch.
//!
//! This module is the owned boundary between the trap/task layer and address-
//! space fault policy.  Today every user fault is still resolved synchronously
//! while holding the single `AddrSpace` mutex.  Keeping the owned address-space
//! handle here allows a future delegated fault session to drop that mutex while
//! it waits and to re-lock for validation, without exposing an intermediate
//! state through `PageFaultResult`.

use alloc::sync::Arc;

use axhal::trap::PageFaultFlags;
use axsync::Mutex;
use memory_addr::VirtAddr;

use super::{AddrSpace, PageFaultResult};

/// Owned context for one user-originated page fault.
///
/// Kernel-originated user-copy faults deliberately do not use this path: they
/// may run with IRQs masked and must remain non-sleeping.
struct FaultSession {
    aspace: Arc<Mutex<AddrSpace>>,
    vaddr: VirtAddr,
    access_flags: PageFaultFlags,
    user_sp: VirtAddr,
}

impl FaultSession {
    fn run(self) -> PageFaultResult {
        let Self {
            aspace,
            vaddr,
            access_flags,
            user_sp,
        } = self;

        // The dormant session boundary does not change current locking or
        // population semantics: ordinary faults still execute under one
        // AddrSpace critical section.
        aspace
            .lock()
            .handle_page_fault_result(vaddr, access_flags, Some(user_sp))
    }
}

/// Handles a page fault raised while executing in userspace.
///
/// The owned handle is intentional.  A future delegated session may retain
/// only owned request identity across a lock-external wait, then re-lock and
/// validate before returning this terminal result.
pub(crate) fn handle_user_page_fault(
    aspace: Arc<Mutex<AddrSpace>>,
    vaddr: VirtAddr,
    access_flags: PageFaultFlags,
    user_sp: VirtAddr,
) -> PageFaultResult {
    FaultSession {
        aspace,
        vaddr,
        access_flags,
        user_sp,
    }
    .run()
}
