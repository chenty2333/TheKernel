//! User-originated page-fault dispatch.
//!
//! Ordinary population remains serialized by the single `AddrSpace` mutex.
//! A registered MISSING fault instead publishes one bounded broker waiter
//! under that mutex, drops it, waits interruptibly, then retries the original
//! userspace instruction. Intermediate delegated states never escape through
//! `PageFaultResult`.

use alloc::{sync::Arc, vec::Vec};
use core::ffi::{c_ulong, c_void};

use axerrno::AxError;
use axhal::{paging::MappingFlags, trap::PageFaultFlags};
use axsync::Mutex;
use memory_addr::{MemoryAddr, VirtAddr};
use tk_linux_mm::{FaultAccess, FaultDisposition, FaultFailure as DelegatedFaultFailure};

use super::{
    AddrSpace, PageFaultFailure, PageFaultKind, PageFaultResult, SharedMmapFaultPlan,
    repair_local_spurious_fault,
};
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

        // A file-cache eviction write-protects aliases under the mm lock. A
        // fault must neither repopulate during that interval nor attempt
        // cache replacement while owning the mutex: reclaim prepares aliases
        // in every mapped address space. Keep both retries wholly internal,
        // releasing the lock and validating the VMA again afterwards.
        'retry: loop {
            loop {
                let retry = {
                    let locked = aspace.lock();
                    locked.file_eviction_retry_for_range(vaddr.align_down_4k(), 4096)
                };
                let Some(retry) = retry else {
                    break;
                };
                if retry.wait().is_err() {
                    // Returning to the trap boundary preserves normal pending
                    // signal/exit processing; userspace will re-execute or take
                    // that signal rather than observing a fabricated bus fault.
                    return PageFaultResult::Handled;
                }
            }

            let (admitted, delegated_fault_kind) = {
                let mut locked = aspace.lock();
                // Close the probe-to-handler gap above.  A retirement can publish
                // after the first lock was released but before this lock is
                // acquired; do not let the fault path install or upgrade an alias
                // in that interval.  Returning Handled after the lock-external
                // completion wait makes the CPU retry the original instruction,
                // which re-enters the normal VMA validation path.
                if let Some(retry) =
                    locked.file_eviction_retry_for_range(vaddr.align_down_4k(), 4096)
                {
                    drop(locked);
                    let _ = retry.wait();
                    return PageFaultResult::Handled;
                }
                match locked.admit_uffd_missing_fault(vaddr, fault_access(access_flags)) {
                    // A userfaultfd resolver supplies or zero-fills the page in
                    // task context; the kernel did not perform backing I/O for
                    // this fault, so its successful completion is minor.
                    Ok(Some(admitted)) => (admitted, Some(PageFaultKind::Minor)),
                    Ok(None) => {
                        match locked.prepare_shared_mmap_fault(vaddr, access_flags) {
                            Ok(Some(plan)) => {
                                drop(locked);
                                return complete_shared_mmap_fault(aspace.clone(), plan);
                            }
                            Ok(None) => {}
                            Err(error) => return admission_failure(error),
                        }
                        let fault_candidate = locked.fault_needs_accounting(vaddr, access_flags);
                        let read_before = axtask::current_may_uninit().and_then(|current| {
                            current
                                .try_as_thread()
                                .map(|thread| thread.backing_read_bytes())
                        });
                        let result =
                            locked.handle_page_fault_result(vaddr, access_flags, Some(user_sp));
                        if result == PageFaultResult::Retry {
                            let cache = locked.file_cache_reclaim_for_fault(vaddr);
                            drop(locked);
                            let Some(cache) = cache else {
                                return PageFaultResult::Failed(
                                    PageFaultFailure::InternalInconsistency,
                                );
                            };
                            match cache.reclaim_one() {
                                Ok(true) => continue 'retry,
                                Err(error) if error.canonicalize() == AxError::ResourceBusy => {
                                    // No mm/cache lock is held. Let the current
                                    // cache owner progress, then return through
                                    // normal signal/exit handling before the
                                    // instruction faults and revalidates again.
                                    axtask::yield_now();
                                    return PageFaultResult::Handled;
                                }
                                // The cache remained full but has no evictable
                                // page. This is memory pressure, not a fabricated
                                // backing-I/O failure.
                                Ok(false) => {
                                    return PageFaultResult::Failed(PageFaultFailure::OutOfMemory);
                                }
                                Err(_) => {
                                    return PageFaultResult::Failed(
                                        PageFaultFailure::BackingUnavailable,
                                    );
                                }
                            }
                        }
                        if result == PageFaultResult::Handled
                            && fault_candidate
                            && let (Some(current), Some(read_before)) =
                                (axtask::current_may_uninit(), read_before)
                            && let Some(thread) = current.try_as_thread()
                        {
                            thread.account_resolved_page_fault(read_before);
                        }
                        return result;
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
                            return PageFaultResult::Failed(
                                PageFaultFailure::InternalInconsistency,
                            );
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
            if matches!(
                disposition,
                FaultDisposition::Supply | FaultDisposition::ZeroFill
            ) {
                // The resolver may have run on another CPU. Repair this waiter's
                // current CPU before retrying the original userspace instruction;
                // each coalesced waiter owns its own local maintenance.
                repair_local_spurious_fault(vaddr);
                // The resolver may have populated the target address space from a
                // different task. Publish the shared mm peak at the completion
                // edge while the waiter still owns the target image.
                let locked = aspace.lock();
                locked.merge_resident_highwater(locked.resident_user_bytes() as u64 / 1024);
            }
            let result = disposition_result(disposition);
            if matches!(
                disposition,
                FaultDisposition::Supply | FaultDisposition::ZeroFill
            ) && let Some(current) = axtask::current_may_uninit()
                && let Some(thread) = current.try_as_thread()
                && let Some(kind) = delegated_fault_kind
            {
                match kind {
                    PageFaultKind::Minor => thread.account_minor_fault(),
                    PageFaultKind::Major => thread.account_major_fault(),
                }
            }
            return result;
        }
    }
}

fn complete_shared_mmap_fault(
    aspace: Arc<Mutex<AddrSpace>>,
    mut plan: SharedMmapFaultPlan,
) -> PageFaultResult {
    let page_ref;
    if plan.needs_fault {
        let mut vmf = shared_mmap_vm_fault(&mut plan);
        let result = unsafe { plan.ops.call_fault(&mut vmf) };
        if let Some(failure) = shared_mmap_callback_failure(result) {
            let page = unsafe { tk_linux_kapi::mm::vm_fault_page(&vmf) };
            if !page.is_null() {
                unsafe { tk_linux_kapi::shmem::put_page(page) };
            }
            return failure;
        }
        let page = unsafe { tk_linux_kapi::mm::vm_fault_page(&vmf) };
        if !page.is_null() {
            page_ref = match plan.ops.retain_fault_page(plan.page_index, page, plan.paddr) {
                Ok(page) => Some(page),
                Err(_) => return PageFaultResult::Failed(PageFaultFailure::BackingUnavailable),
            };
        } else if result & tk_linux_kapi::mm::VM_FAULT_NOPAGE != 0
            && result & tk_linux_kapi::mm::VM_FAULT_LOCKED == 0
        {
            let page = unsafe { tk_linux_kapi::shmem::pin_physical_frame(plan.paddr) };
            if page.is_null() {
                return PageFaultResult::Failed(PageFaultFailure::BackingUnavailable);
            }
            page_ref = match plan.ops.retain_fault_page(plan.page_index, page, plan.paddr) {
                Ok(page) => Some(page),
                Err(_) => return PageFaultResult::Failed(PageFaultFailure::BackingUnavailable),
            };
        } else {
            return PageFaultResult::Failed(PageFaultFailure::BackingUnavailable);
        }
        if result & tk_linux_kapi::mm::VM_FAULT_LOCKED != 0 {
            let Some(page) = page_ref.as_ref() else {
                return PageFaultResult::Failed(PageFaultFailure::BackingUnavailable);
            };
            unsafe { tk_linux_kapi::shmem::unlock_page(page.page()) };
        }
    } else {
        let cached_page = plan.ops.cached_page(plan.page_index).or_else(|| {
            let page = unsafe { tk_linux_kapi::shmem::pin_physical_frame(plan.paddr) };
            if page.is_null() {
                None
            } else {
                plan.ops
                    .retain_fault_page(plan.page_index, page, plan.paddr)
                    .ok()
            }
        });
        if cached_page.is_none() {
            return PageFaultResult::Failed(PageFaultFailure::BackingUnavailable);
        }
        page_ref = cached_page;
    }

    if plan.needs_mkwrite {
        let Some(page) = page_ref.as_ref() else {
            return PageFaultResult::Failed(PageFaultFailure::BackingUnavailable);
        };
        let mut vmf = shared_mmap_vm_fault(&mut plan);
        unsafe { tk_linux_kapi::mm::vm_fault_set_page(&mut vmf, page.page()) };
        let result = unsafe {
            if plan.ops.has_page_mkwrite() {
                plan.ops.call_page_mkwrite(&mut vmf)
            } else {
                plan.ops.call_pfn_mkwrite(&mut vmf)
            }
        };
        if let Some(failure) = shared_mmap_callback_failure(result) {
            if result & tk_linux_kapi::mm::VM_FAULT_LOCKED != 0 {
                unsafe { tk_linux_kapi::shmem::unlock_page(page.page()) };
            }
            return failure;
        }
        let callback_page = unsafe { tk_linux_kapi::mm::vm_fault_page(&vmf) };
        if !callback_page.is_null() && callback_page != page.page() {
            unsafe { tk_linux_kapi::shmem::put_page(callback_page) };
            if result & tk_linux_kapi::mm::VM_FAULT_LOCKED != 0 {
                unsafe { tk_linux_kapi::shmem::unlock_page(page.page()) };
            }
            return PageFaultResult::Failed(PageFaultFailure::BackingUnavailable);
        }
        unsafe { tk_linux_kapi::shmem::set_page_dirty_c(page.page()) };
        if result & tk_linux_kapi::mm::VM_FAULT_LOCKED != 0 {
            unsafe { tk_linux_kapi::shmem::unlock_page(page.page()) };
        }
    }

    aspace.lock().commit_shared_mmap_fault(&plan)
}

fn shared_mmap_vm_fault(plan: &mut SharedMmapFaultPlan) -> tk_linux_kapi::mm::VmFault {
    let mut vmf = tk_linux_kapi::mm::VmFault::empty();
    vmf.vma = &mut plan.vma;
    vmf.pgoff = plan.pgoff as c_ulong;
    vmf.address = plan.page.as_usize() as c_ulong;
    vmf.real_address = vmf.address;
    if plan.write {
        vmf.flags = tk_linux_kapi::mm::FAULT_FLAG_WRITE;
    }
    vmf
}

fn shared_mmap_callback_failure(result: u32) -> Option<PageFaultResult> {
    if result & tk_linux_kapi::mm::VM_FAULT_OOM != 0 {
        return Some(PageFaultResult::Failed(PageFaultFailure::OutOfMemory));
    }
    if result & tk_linux_kapi::mm::VM_FAULT_SIGBUS != 0 {
        return Some(PageFaultResult::Failed(PageFaultFailure::BackingUnavailable));
    }
    let known = tk_linux_kapi::mm::VM_FAULT_NOPAGE
        | tk_linux_kapi::mm::VM_FAULT_LOCKED
        | tk_linux_kapi::mm::VM_FAULT_FALLBACK;
    (result & !known != 0)
        .then_some(PageFaultResult::Failed(PageFaultFailure::BackingUnavailable))
}

unsafe extern "C" fn core_vmf_insert_pfn(
    vma: *mut tk_linux_kapi::mm::VmAreaStruct,
    address: c_ulong,
    pfn: c_ulong,
) -> u32 {
    if vma.is_null() {
        return tk_linux_kapi::mm::VM_FAULT_SIGBUS;
    }
    let Some(aspace) = axtask::current_may_uninit()
        .and_then(|current| current.try_as_thread().map(|thread| thread.proc_data.aspace()))
    else {
        return tk_linux_kapi::mm::VM_FAULT_SIGBUS;
    };
    let Ok(pfn) = usize::try_from(pfn) else {
        return tk_linux_kapi::mm::VM_FAULT_SIGBUS;
    };
    let mut locked = aspace.lock();
    locked.insert_shared_mmap_pfn(unsafe { &*vma }, address as usize, pfn)
}

unsafe extern "C" fn core_mapping_wrprotect_range(
    mapping: *mut c_void,
    pgoff: c_ulong,
    pfn: c_ulong,
    nr_pages: c_ulong,
) -> i32 {
    if mapping.is_null() || nr_pages == 0 {
        return -tk_linux_kapi::config::EINVAL;
    }
    let (Ok(pgoff), Ok(pfn), Ok(nr_pages)) = (
        usize::try_from(pgoff),
        usize::try_from(pfn),
        usize::try_from(nr_pages),
    ) else {
        return -tk_linux_kapi::config::EOVERFLOW;
    };
    if pgoff.checked_add(nr_pages).is_none() || pfn.checked_add(nr_pages).is_none() {
        return -tk_linux_kapi::config::EOVERFLOW;
    }

    let snapshots = match super::aspace::snapshot_all_aliases() {
        Ok(snapshots) => snapshots,
        Err(error) => return shared_mmap_errno(error),
    };
    let mut keys = Vec::new();
    for snapshot in snapshots {
        let Some(aspace) = snapshot.revalidate() else {
            continue;
        };
        if aspace.lock().has_shared_mmap_mapping(mapping as usize) {
            let key = snapshot.key();
            if !keys.contains(&key) {
                if keys.try_reserve(1).is_err() {
                    return -tk_linux_kapi::config::ENOMEM;
                }
                keys.push(key);
            }
        }
    }
    keys.sort_unstable();

    let mut mutations = Vec::new();
    let mut aliases = Vec::new();
    if mutations.try_reserve_exact(keys.len()).is_err()
        || aliases.try_reserve(keys.len()).is_err()
    {
        return -tk_linux_kapi::config::ENOMEM;
    }
    for key in keys {
        let (mutation, backing_aliases) = match super::aspace::reserve_alias_mutation(key) {
            Ok(reservation) => reservation,
            Err(error) => return shared_mmap_errno(error),
        };
        mutations.push(mutation);
        if aliases
            .try_reserve(backing_aliases.len())
            .is_err()
        {
            return -tk_linux_kapi::config::ENOMEM;
        }
        aliases.extend(backing_aliases);
    }

    let mut participants = Vec::new();
    if participants.try_reserve(aliases.len()).is_err() {
        return -tk_linux_kapi::config::ENOMEM;
    }
    for alias in aliases {
        if let Some(aspace) = alias.revalidate() {
            participants.push((alias.address_space_id(), aspace));
        }
    }
    participants.sort_unstable_by_key(|(id, _)| *id);
    participants.dedup_by(|(_, lhs), (_, rhs)| Arc::ptr_eq(lhs, rhs));
    let mut guards = Vec::new();
    if guards.try_reserve_exact(participants.len()).is_err() {
        return -tk_linux_kapi::config::ENOMEM;
    }
    for (_, participant) in &participants {
        guards.push(participant.lock());
    }
    let mut protect_plans = Vec::new();
    if protect_plans.try_reserve_exact(guards.len()).is_err() {
        return -tk_linux_kapi::config::ENOMEM;
    }
    for guard in &mut guards {
        match guard.prepare_shared_mmap_wrprotect(
            mapping as usize,
            pgoff,
            pfn,
            nr_pages,
        ) {
            Ok(plan) => protect_plans.push(plan),
            Err(error) => return shared_mmap_errno(error),
        }
    }
    for (guard, plan) in guards.iter_mut().zip(protect_plans) {
        guard.commit_shared_mmap_wrprotect(plan);
    }
    0
}

fn shared_mmap_errno(error: AxError) -> i32 {
    -match error.canonicalize() {
        AxError::NoMemory => tk_linux_kapi::config::ENOMEM,
        AxError::InvalidInput | AxError::OutOfRange => tk_linux_kapi::config::EINVAL,
        AxError::BadAddress => tk_linux_kapi::config::EFAULT,
        AxError::ResourceBusy | AxError::WouldBlock => tk_linux_kapi::config::EBUSY,
        _ => tk_linux_kapi::config::EIO,
    }
}

/// Installs the real SharedPages providers before a VMA carrying LinuxKPI VM
/// operations can be published. Each KAPI installer accepts only an
/// identical repeat installation.
pub(crate) fn install_shared_mmap_providers() -> Result<(), &'static str> {
    tk_linux_kapi::mm_native::install_mapping_wrprotect_handler(core_mapping_wrprotect_range)?;
    tk_linux_kapi::mm_native::install_vmf_insert_pfn_handler(core_vmf_insert_pfn)
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
    use alloc::sync::Arc;
    use core::cell::UnsafeCell;
    use memory_addr::PAGE_SIZE_4K;
    use tk_linux_kapi::{
        mm::{VmAreaStruct, VmFault, VmOperationsStruct, VM_MAYWRITE, VM_READ, VM_SHARED, VM_WRITE},
        types::File,
    };

    static TEST_MAPPING_COOKIE: u8 = 0;

    struct TestVmFileOwner(UnsafeCell<File>);

    // SAFETY: the test never mutates the file record after creating its VMA.
    unsafe impl Send for TestVmFileOwner {}
    unsafe impl Sync for TestVmFileOwner {}

    unsafe extern "C" fn test_vm_fault(_fault: *mut VmFault) -> u32 {
        tk_linux_kapi::mm::VM_FAULT_SIGBUS
    }

    unsafe extern "C" fn test_page_mkwrite(_fault: *mut VmFault) -> u32 {
        tk_linux_kapi::mm::VM_FAULT_SIGBUS
    }

    unsafe extern "C" fn test_vm_close(_vma: *mut VmAreaStruct) {}

    static TEST_VM_OPS: VmOperationsStruct = VmOperationsStruct {
        open: None,
        close: Some(test_vm_close),
        mapped: None,
        may_split: None,
        mremap: None,
        mprotect: None,
        fault: Some(test_vm_fault),
        huge_fault: None,
        map_pages: None,
        pagesize: None,
        page_mkwrite: Some(test_page_mkwrite),
        pfn_mkwrite: None,
        access: None,
        name: None,
        set_policy: None,
        get_policy: None,
        uffd_ops: core::ptr::null(),
    };

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
    fn shared_mmap_callback_results_fail_closed() {
        use tk_linux_kapi::mm::{VM_FAULT_FALLBACK, VM_FAULT_LOCKED, VM_FAULT_NOPAGE};

        assert_eq!(
            shared_mmap_callback_failure(tk_linux_kapi::mm::VM_FAULT_OOM),
            Some(PageFaultResult::Failed(PageFaultFailure::OutOfMemory)),
        );
        assert_eq!(
            shared_mmap_callback_failure(tk_linux_kapi::mm::VM_FAULT_SIGBUS),
            Some(PageFaultResult::Failed(PageFaultFailure::BackingUnavailable)),
        );
        assert_eq!(
            shared_mmap_callback_failure(VM_FAULT_NOPAGE | VM_FAULT_LOCKED | VM_FAULT_FALLBACK),
            None,
        );
        assert!(shared_mmap_callback_failure(1 << 31).is_some());
    }

    #[test]
    fn shared_mmap_rearm_writeprotects_every_shared_pages_alias() {
        let mapping_cookie = core::ptr::addr_of!(TEST_MAPPING_COOKIE).cast_mut().cast();
        let owner = Arc::new(TestVmFileOwner(UnsafeCell::new(File {
            _before_f_mapping: [0; 16],
            f_mapping: mapping_cookie,
            private_data: core::ptr::null_mut(),
            f_inode: core::ptr::null_mut(),
            f_flags: 0,
            _tail: [0; 132],
        })));
        let mut template = VmAreaStruct::empty();
        template.vm_start = 0x1000;
        template.vm_end = 0x1000 + PAGE_SIZE_4K as c_ulong;
        template.vm_ops = &TEST_VM_OPS;
        template.vm_pgoff = 0;
        template.vm_file = owner.0.get();
        template.vm_flags = VM_SHARED | VM_MAYWRITE | VM_READ | VM_WRITE;
        // SAFETY: the owner pins the File record and the static operation
        // table through deferred close; this test does not call either fault
        // callback.
        let ops = unsafe {
            crate::file::SharedMmapOps::from_vma(
                Arc::new(crate::mm::SharedPages::new_fixed(
                    PAGE_SIZE_4K,
                    axhal::paging::PageSize::Size4K,
                )
                .unwrap()),
                &template,
                owner.clone(),
            )
        }
        .unwrap();
        let pages = ops.pages().clone();
        let start = VirtAddr::from(0x1000);
        let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
        let first = Arc::new(Mutex::new(
            AddrSpace::new_empty(start, PAGE_SIZE_4K * 4).unwrap(),
        ));
        let second = Arc::new(Mutex::new(
            AddrSpace::new_empty(start, PAGE_SIZE_4K * 4).unwrap(),
        ));
        for aspace in [&first, &second] {
            let mut locked = aspace.lock();
            let backend = crate::mm::Backend::new_shared(start, pages.clone())
                .with_shared_mmap_ops(ops.clone())
                .unwrap();
            locked.map(start, PAGE_SIZE_4K, flags, false, backend).unwrap();
            locked.sync_shared_alias_bindings(aspace).unwrap();
            locked.populate_area(start, PAGE_SIZE_4K, MappingFlags::READ).unwrap();
            assert!(
                !locked
                    .page_table()
                    .query(start)
                    .unwrap()
                    .1
                    .contains(MappingFlags::WRITE)
            );
        }
        let paddr = pages.physical_page(0).unwrap();
        for aspace in [&first, &second] {
            aspace
                .lock()
                .page_table_mut()
                .cursor()
                .remap(start, paddr, flags)
                .unwrap();
        }

        // A mismatched frame is rejected before any alias permission changes.
        assert_eq!(
            unsafe {
                core_mapping_wrprotect_range(
                    mapping_cookie,
                    0,
                    (paddr.as_usize() / PAGE_SIZE_4K + 1) as c_ulong,
                    1,
                )
            },
            -tk_linux_kapi::config::EFAULT,
        );
        for aspace in [&first, &second] {
            assert!(
                aspace
                    .lock()
                    .page_table()
                    .query(start)
                    .unwrap()
                    .1
                    .contains(MappingFlags::WRITE)
            );
        }

        assert_eq!(
            unsafe {
                core_mapping_wrprotect_range(
                    mapping_cookie,
                    0,
                    (paddr.as_usize() / PAGE_SIZE_4K) as c_ulong,
                    1,
                )
            },
            0,
        );
        for aspace in [&first, &second] {
            assert!(
                !aspace
                    .lock()
                    .page_table()
                    .query(start)
                    .unwrap()
                    .1
                    .contains(MappingFlags::WRITE)
            );
        }
        for aspace in [&first, &second] {
            aspace
                .lock()
                .page_table_mut()
                .cursor()
                .remap(
                    start,
                    paddr,
                    MappingFlags::USER | MappingFlags::READ | MappingFlags::SHADOW_STACK,
                )
                .unwrap();
        }
        assert_eq!(
            unsafe {
                core_mapping_wrprotect_range(
                    mapping_cookie,
                    0,
                    (paddr.as_usize() / PAGE_SIZE_4K) as c_ulong,
                    1,
                )
            },
            0,
        );
        for aspace in [&first, &second] {
            assert!(
                !aspace
                    .lock()
                    .page_table()
                    .query(start)
                    .unwrap()
                    .1
                    .contains(MappingFlags::SHADOW_STACK)
            );
        }
        drop(first);
        drop(second);
        drop(ops);
        crate::mm::drain_deferred_mapping_finalizers(16);
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
