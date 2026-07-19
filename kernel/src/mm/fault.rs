//! User-originated page-fault dispatch.
//!
//! Ordinary population remains serialized by the single `AddrSpace` mutex.
//! A registered MISSING fault instead publishes one bounded broker waiter
//! under that mutex, drops it, waits interruptibly, then retries the original
//! userspace instruction. Intermediate delegated states never escape through
//! `PageFaultResult`.

use alloc::sync::Arc;

use axerrno::AxError;
use axhal::{paging::MappingFlags, trap::PageFaultFlags};
use axsync::Mutex;
use memory_addr::VirtAddr;
use thekernel_linux_mm::{FaultAccess, FaultDisposition, FaultFailure as DelegatedFaultFailure};

use super::{AddrSpace, PageFaultFailure, PageFaultResult};
use crate::{
    readiness::block_on_poll_set_interruptible_if,
    task::{AsThread, has_pending_syscall_signal, linux_pid_from_task_id},
};

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

        let admitted = {
            let mut locked = aspace.lock();
            match locked.admit_uffd_missing_fault(vaddr, fault_access(access_flags)) {
                Ok(Some(admitted)) => admitted,
                Ok(None) => {
                    return locked.handle_page_fault_result(vaddr, access_flags, Some(user_sp));
                }
                Err(error) => return admission_failure(error),
            }
        };

        // New-request readiness is a lock-external hint. Exact coalescing
        // carries no duplicate handler wake.
        let wait = admitted.publish();
        let waited = block_on_poll_set_interruptible_if(
            wait.completion(),
            || aspace.lock().take_uffd_fault_completion(&wait),
            fault_wait_should_interrupt,
        );
        let disposition = match waited {
            Ok(disposition) => disposition,
            Err(wait_error) => {
                let cancelled = {
                    let mut locked = aspace.lock();
                    locked.cancel_uffd_fault_wait(&wait)
                };
                let cancelled = match cancelled {
                    Ok(cancelled) => cancelled,
                    Err(_) => {
                        return PageFaultResult::Failed(PageFaultFailure::InternalInconsistency);
                    }
                };
                match cancelled.finish() {
                    Some(disposition) => {
                        // The terminal result became visible after the wait
                        // helper's final recheck but before cancellation.
                        // Completion wins; preserve the consumed task
                        // interrupt for the next user-return boundary.
                        if wait_error == AxError::Interrupted {
                            axtask::current().interrupt();
                        }
                        disposition
                    }
                    None if wait_error == AxError::Interrupted => {
                        // The pending signal/exit/exec transition won and this
                        // waiter no longer owns broker resources. Returning to
                        // the trap boundary lets normal task policy run.
                        return PageFaultResult::Handled;
                    }
                    None => return wait_failure(wait_error),
                }
            }
        };
        disposition_result(disposition)
    }
}

fn fault_access(access_flags: PageFaultFlags) -> FaultAccess {
    if access_flags.contains(MappingFlags::WRITE) {
        FaultAccess::Write
    } else if access_flags.contains(MappingFlags::EXECUTE) {
        FaultAccess::Execute
    } else {
        FaultAccess::Read
    }
}

fn fault_wait_should_interrupt() -> bool {
    let current = axtask::current();
    let thread = current.as_thread();
    if thread.pending_exit() || has_pending_syscall_signal(thread) {
        return true;
    }
    linux_pid_from_task_id(current.id().as_u64())
        .map(|tid| thread.proc_data.should_exit_for_exec(tid))
        .unwrap_or(true)
}

fn admission_failure(error: AxError) -> PageFaultResult {
    let failure = match error.canonicalize() {
        AxError::NoMemory => PageFaultFailure::OutOfMemory,
        AxError::OperationNotPermitted | AxError::BadAddress => PageFaultFailure::AccessDenied,
        _ => PageFaultFailure::InternalInconsistency,
    };
    PageFaultResult::Failed(failure)
}

fn wait_failure(error: AxError) -> PageFaultResult {
    let failure = match error.canonicalize() {
        AxError::NoMemory => PageFaultFailure::OutOfMemory,
        AxError::BadState | AxError::OutOfRange => PageFaultFailure::InternalInconsistency,
        _ => PageFaultFailure::BackingUnavailable,
    };
    PageFaultResult::Failed(failure)
}

fn disposition_result(disposition: FaultDisposition) -> PageFaultResult {
    match disposition {
        FaultDisposition::Supply
        | FaultDisposition::ZeroFill
        | FaultDisposition::Cancelled
        | FaultDisposition::HandlerDetached
        | FaultDisposition::Failure(DelegatedFaultFailure::Retry) => PageFaultResult::Handled,
        FaultDisposition::Failure(DelegatedFaultFailure::Segmentation) => {
            PageFaultResult::Failed(PageFaultFailure::AccessDenied)
        }
        FaultDisposition::Failure(DelegatedFaultFailure::Bus)
        | FaultDisposition::Failure(DelegatedFaultFailure::Io) => {
            PageFaultResult::Failed(PageFaultFailure::BackingUnavailable)
        }
        FaultDisposition::Failure(DelegatedFaultFailure::OutOfMemory) => {
            PageFaultResult::Failed(PageFaultFailure::OutOfMemory)
        }
        FaultDisposition::Continue | FaultDisposition::WriteProtect => {
            PageFaultResult::Failed(PageFaultFailure::InternalInconsistency)
        }
        _ => PageFaultResult::Failed(PageFaultFailure::InternalInconsistency),
    }
}

/// Handles a page fault raised while executing in userspace.
///
/// The owned handle retains the address space while a delegated waiter sleeps;
/// kernel-originated usercopy faults deliberately remain on the non-sleeping
/// exception-fixup path.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fault_access_prefers_write_then_execute_then_read() {
        assert_eq!(fault_access(MappingFlags::READ), FaultAccess::Read);
        assert_eq!(fault_access(MappingFlags::EXECUTE), FaultAccess::Execute);
        assert_eq!(
            fault_access(MappingFlags::READ | MappingFlags::EXECUTE | MappingFlags::WRITE),
            FaultAccess::Write
        );
    }

    #[test]
    fn delegated_terminal_dispositions_map_only_to_public_fault_terminals() {
        for disposition in [
            FaultDisposition::Supply,
            FaultDisposition::ZeroFill,
            FaultDisposition::Cancelled,
            FaultDisposition::HandlerDetached,
            FaultDisposition::Failure(DelegatedFaultFailure::Retry),
        ] {
            assert_eq!(disposition_result(disposition), PageFaultResult::Handled);
        }
        assert_eq!(
            disposition_result(FaultDisposition::Failure(
                DelegatedFaultFailure::Segmentation
            )),
            PageFaultResult::Failed(PageFaultFailure::AccessDenied)
        );
        for failure in [DelegatedFaultFailure::Bus, DelegatedFaultFailure::Io] {
            assert_eq!(
                disposition_result(FaultDisposition::Failure(failure)),
                PageFaultResult::Failed(PageFaultFailure::BackingUnavailable)
            );
        }
        assert_eq!(
            disposition_result(FaultDisposition::Failure(
                DelegatedFaultFailure::OutOfMemory
            )),
            PageFaultResult::Failed(PageFaultFailure::OutOfMemory)
        );
        for unsupported in [FaultDisposition::Continue, FaultDisposition::WriteProtect] {
            assert_eq!(
                disposition_result(unsupported),
                PageFaultResult::Failed(PageFaultFailure::InternalInconsistency)
            );
        }
    }

    #[test]
    fn admission_and_wait_resource_failures_fail_closed_by_class() {
        assert_eq!(
            admission_failure(AxError::NoMemory),
            PageFaultResult::Failed(PageFaultFailure::OutOfMemory)
        );
        assert_eq!(
            admission_failure(AxError::OperationNotPermitted),
            PageFaultResult::Failed(PageFaultFailure::AccessDenied)
        );
        assert_eq!(
            wait_failure(AxError::BadState),
            PageFaultResult::Failed(PageFaultFailure::InternalInconsistency)
        );
        assert_eq!(
            wait_failure(AxError::Io),
            PageFaultResult::Failed(PageFaultFailure::BackingUnavailable)
        );
    }
}
