use alloc::{string::String, sync::Arc, vec::Vec};
use core::{
    ffi::c_char,
    mem::{self, MaybeUninit, size_of},
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::NodeType;
use axhal::{
    paging::{MappingFlags, PageSize},
    uspace::UserContext,
};
use axtask::current;
use linux_raw_sys::general::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW, CAP_SYS_PTRACE};
use memory_addr::{PAGE_SIZE_4K, VirtAddr};
use thekernel_linux_process_adapter::Pid;
use thekernel_linux_signal::Signo;
use thekernel_linux_usercopy::{UserCopyError, UserMemory, UserMemoryContext, vm_load_until_nul};

use crate::{
    file::{
        FD_TABLE, ResolveAtResult, fanotify, permission::VfsSecurityContext,
        replace_process_fd_table, resolve_at_with_security,
    },
    keyring::{self, KeyTaskOwner},
    mm::{
        Backend, ExecImageAccess, ExecLayout, copy_from_kernel, finish_prepared_user_app,
        new_user_aspace_empty, new_user_aspace_with_page_zero, preflight_user_app_at,
    },
    readiness::block_on_poll_set,
    task::{
        AsThread, Cred, ExecCommitRuntime, ExecCredentialInput, ExecImageIdentity,
        ExecImageReadability, ExecMountPrivilege, ExecTraceState, FileCapabilities,
        ProcessAccessState, ProcessData, PtraceRelationshipSnapshot, Thread, UserNamespace,
        check_signals, commit_exec_identity_handoff, fail_closed_exit, get_task,
        has_pending_fatal_signal, linux_pid_from_task_id, map_exec_dumpability,
        notify_ptrace_attach_stop, ns_capable, prepare_task_alias_admission, process_error,
        release_exec_action_then_complete, reset_current_task_extended_state,
        set_current_user_address_space,
    },
};

const PER_CLEAR_ON_SETID: u32 = 0x0074_0000;
const MMAP_PAGE_ZERO: u32 = 0x0010_0000;

fn effective_exec_personality(personality: u32, secure_exec: bool) -> u32 {
    if secure_exec {
        personality & !PER_CLEAR_ON_SETID
    } else {
        personality
    }
}

fn install_mmap_page_zero(aspace: &mut crate::mm::AddrSpace) -> AxResult {
    let start = VirtAddr::from_usize(0);
    aspace.map(
        start,
        PAGE_SIZE_4K,
        MappingFlags::READ | MappingFlags::EXECUTE | MappingFlags::USER,
        false,
        Backend::new_alloc(start, PageSize::Size4K),
    )?;
    // Seal rejects mprotect, munmap, fixed replacement, and mremap for the
    // lifetime of this exec image.
    aspace.seal(start, PAGE_SIZE_4K)
}

fn interrupt_exec_siblings(sibling_tids: &[Pid]) {
    for &tid in sibling_tids {
        if let Ok(task) = get_task(tid) {
            task.interrupt();
        }
    }
}

fn files_preparation_covers_thread_snapshot(has_private_table: bool, threads: &[Pid]) -> bool {
    has_private_table || threads.len() <= 1
}

fn exec_file_capabilities(
    nosuid: bool,
    read: impl FnOnce() -> AxResult<Option<FileCapabilities>>,
) -> AxResult<Option<FileCapabilities>> {
    if nosuid { Ok(None) } else { read() }
}

fn exec_mm_owner_user_ns(
    proposed_user_ns: &Arc<UserNamespace>,
    image_access: ExecImageAccess,
) -> Arc<UserNamespace> {
    if !image_access.executable_unreadable() {
        return proposed_user_ns.clone();
    }

    // Linux would_dump() raises mm->user_ns to an ancestor able to dominate
    // the unreadable inode. Filesystems here have neither superblock user_ns
    // nor idmapped mounts, so their conservative owner is the initial user ns.
    let mut owner = proposed_user_ns.clone();
    while let Some(parent) = owner.parent() {
        owner = parent;
    }
    owner
}

fn exact_exec_thread_snapshot<I>(snapshot: &[Pid], current_count: usize, current: I) -> bool
where
    I: Iterator<Item = Pid>,
{
    current_count == snapshot.len() && current.eq(snapshot.iter().copied())
}

/// Installs the architectural entry state for a new process image.
///
/// An exec must not expose syscall arguments, TLS, or any other register state
/// from the old image.  This is especially observable on x86_64, where the ELF
/// entry ABI reserves RDX for the dynamic linker's finalizer.  Leaving the
/// execve envp argument there makes a static libc register that user-stack
/// pointer as `rtld_fini` and jump to it when `main` returns.
fn install_exec_user_context(uctx: &mut UserContext, entry: usize, stack: VirtAddr) {
    *uctx = UserContext::new(entry, stack, 0);
}

/// Owns the process-wide exec/attach/thread-admission gate.
///
/// The gate is claimed only after the loader has frozen the old-image inputs,
/// but before ptrace state or executable privilege metadata is sampled. Every
/// fallible preparation below that point therefore releases it automatically,
/// while a successful exec keeps it through the complete image publication.
#[must_use = "dropping the admission guard releases the exec gate"]
struct ExecAdmission<'a> {
    proc_data: &'a ProcessData,
    owner: Pid,
    armed: bool,
}

impl<'a> ExecAdmission<'a> {
    fn try_begin(proc_data: &'a ProcessData, owner: Pid) -> Option<Self> {
        proc_data.begin_exec(owner).then_some(Self {
            proc_data,
            owner,
            armed: true,
        })
    }

    fn release(mut self) {
        self.proc_data.end_exec(self.owner);
        self.armed = false;
    }
}

impl Drop for ExecAdmission<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.proc_data.end_exec(self.owner);
        }
    }
}

fn classify_exec_trace_state(
    ptracer_credential: Option<&Cred>,
    target_user_ns: &Arc<UserNamespace>,
) -> ExecTraceState {
    // Linux asks whether the relationship's frozen ptracer_cred is capable in
    // the proposed exec credential's user namespace. Exec derivation preserves
    // the actor's exact namespace Arc, so callers pass actor.user_ns() here;
    // the executable/MM owner namespace is deliberately unrelated.
    match ptracer_credential {
        Some(ptracer) if !ns_capable(ptracer, target_user_ns, CAP_SYS_PTRACE) => {
            ExecTraceState::SuppressingPrivilege
        }
        Some(_) | None => ExecTraceState::NotSuppressingPrivilege,
    }
}

fn exec_trace_state(
    relationship: Option<&PtraceRelationshipSnapshot>,
    target_user_ns: &Arc<UserNamespace>,
) -> ExecTraceState {
    classify_exec_trace_state(
        relationship.map(|relationship| relationship.ptracer_cred().as_ref()),
        target_user_ns,
    )
}

fn exec_ptrace_relationship_is_stable(
    prepared: Option<&PtraceRelationshipSnapshot>,
    current: Option<&PtraceRelationshipSnapshot>,
) -> bool {
    match (prepared, current) {
        (None, None) | (Some(_), None) => true,
        (Some(prepared), Some(current)) => {
            prepared.session() == current.session()
                && prepared.origin() == current.origin()
                && Arc::ptr_eq(prepared.ptracer_cred(), current.ptracer_cred())
        }
        (None, Some(_)) => false,
    }
}

fn wait_for_exec_group(
    proc_data: &ProcessData,
    thr: &Thread,
    uctx: &mut UserContext,
    curr_tid: Pid,
    sibling_tids: &[Pid],
) -> AxResult<()> {
    while proc_data.is_exec_owner(curr_tid) && !proc_data.exec_ready(curr_tid) {
        match block_on_poll_set(&proc_data.exec_event, || {
            if !proc_data.is_exec_owner(curr_tid) || proc_data.exec_ready(curr_tid) {
                Ok(())
            } else {
                Err(AxError::WouldBlock)
            }
        }) {
            Ok(()) => {}
            Err(_) => {
                interrupt_exec_siblings(sibling_tids);
                // Fatal default-action signals must still win immediately. Other
                // pending signals stay queued and are resolved against the new
                // image once exec commits.
                if has_pending_fatal_signal(thr) {
                    while check_signals(thr, uctx, None) {}
                }
                if thr.pending_exit() || !proc_data.is_exec_owner(curr_tid) {
                    return Err(AxError::Interrupted);
                }
            }
        }
    }

    if proc_data.is_exec_owner(curr_tid) && proc_data.exec_ready(curr_tid) {
        Ok(())
    } else {
        Err(AxError::Interrupted)
    }
}

const SUPPORTED_EXECVEAT_FLAGS: u32 = AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW;
const EXEC_MAX_ARG_STRLEN: usize = 32 * PAGE_SIZE_4K;
const EXEC_ARG_MAX: usize = 2 * 1024 * 1024;
const EXEC_STACK_SAFETY_MARGIN: usize = 4 * PAGE_SIZE_4K;

fn exec_arg_limit() -> usize {
    EXEC_ARG_MAX.min(crate::config::USER_STACK_SIZE.saturating_sub(EXEC_STACK_SAFETY_MARGIN))
}

fn exec_arg_too_big() -> AxError {
    LinuxError::E2BIG.into()
}

fn try_copy_string(value: &str) -> AxResult<String> {
    let mut copy = String::new();
    copy.try_reserve_exact(value.len())
        .map_err(|_| AxError::NoMemory)?;
    copy.push_str(value);
    Ok(copy)
}

fn map_exec_vm_error(err: UserCopyError) -> AxError {
    match err {
        UserCopyError::TooLong => exec_arg_too_big(),
        UserCopyError::BadAddress | UserCopyError::AccessDenied => AxError::BadAddress,
        UserCopyError::NoMemory => AxError::NoMemory,
        _ => AxError::BadAddress,
    }
}

fn vm_load_exec_string<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const c_char,
) -> AxResult<String> {
    let bytes = vm_load_until_nul(memory, ptr.cast::<u8>()).map_err(map_exec_vm_error)?;
    String::from_utf8(bytes).map_err(|_| AxError::IllegalBytes)
}

struct ExecArgSizer {
    bytes: usize,
    limit: usize,
}

impl ExecArgSizer {
    fn new() -> AxResult<Self> {
        let bytes = 3usize
            .checked_mul(size_of::<usize>())
            .ok_or_else(exec_arg_too_big)?;
        let this = Self {
            bytes,
            limit: exec_arg_limit(),
        };
        if this.bytes > this.limit {
            Err(exec_arg_too_big())
        } else {
            Ok(this)
        }
    }

    fn push_pointer_slot(&mut self) -> AxResult {
        self.bytes = self
            .bytes
            .checked_add(size_of::<usize>())
            .ok_or_else(exec_arg_too_big)?;
        if self.bytes > self.limit {
            return Err(exec_arg_too_big());
        }
        Ok(())
    }

    fn push_string_bytes(&mut self, value: &str) -> AxResult {
        let string_bytes = value.len().checked_add(1).ok_or_else(exec_arg_too_big)?;
        if string_bytes > EXEC_MAX_ARG_STRLEN {
            return Err(exec_arg_too_big());
        }

        self.bytes = self
            .bytes
            .checked_add(string_bytes)
            .ok_or_else(exec_arg_too_big)?;
        if self.bytes > self.limit {
            return Err(exec_arg_too_big());
        }
        Ok(())
    }

    fn push_str(&mut self, value: &str) -> AxResult {
        self.push_pointer_slot()?;
        self.push_string_bytes(value)
    }
}

const EXEC_POINTER_ARRAY_CHUNK: usize = 256;

fn exec_pointer_array_address(base: usize, index: usize) -> AxResult<usize> {
    let offset = index
        .checked_mul(size_of::<usize>())
        .ok_or(AxError::BadAddress)?;
    base.checked_add(offset).ok_or(AxError::BadAddress)
}

fn exec_pointer_array_chunk_len(address: usize, remaining: usize) -> usize {
    let page_offset = address % PAGE_SIZE_4K;
    let page_remaining = PAGE_SIZE_4K - page_offset;
    // An unaligned pointer at the end of a page can cross into the next page.
    // Read that one complete slot so the provider can report EFAULT rather
    // than silently truncating the typed value.
    let page_slots = (page_remaining / size_of::<usize>()).max(1);
    remaining.min(EXEC_POINTER_ARRAY_CHUNK).min(page_slots)
}

fn snapshot_exec_pointer_array<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const *const c_char,
    sizer: &mut ExecArgSizer,
) -> AxResult<Vec<*const c_char>> {
    // Each non-null pointer consumes one stack pointer slot. The initial
    // sizer budget already accounts for argc and the two array sentinels, so
    // permit one final read for the terminating null without charging it.
    let pointer_slots = sizer
        .limit
        .saturating_sub(sizer.bytes)
        .checked_div(size_of::<usize>())
        .ok_or_else(exec_arg_too_big)?;
    let max_elements = pointer_slots.checked_add(1).ok_or_else(exec_arg_too_big)?;

    let mut values = Vec::new();
    let mut index = 0usize;
    while index < max_elements {
        let address = exec_pointer_array_address(ptr as usize, index)?;
        let chunk_len = exec_pointer_array_chunk_len(address, max_elements - index);
        let mut chunk = [MaybeUninit::<usize>::uninit(); EXEC_POINTER_ARRAY_CHUNK];
        memory
            .read_slice(address as *const usize, &mut chunk[..chunk_len])
            .map_err(map_exec_vm_error)?;

        let non_null = chunk[..chunk_len]
            .iter()
            .position(|raw| {
                // SAFETY: read_slice initialized every element in this chunk.
                unsafe { raw.assume_init() == 0 }
            })
            .unwrap_or(chunk_len);
        for _ in 0..non_null {
            sizer.push_pointer_slot()?;
        }
        if non_null != 0 {
            values
                .try_reserve_exact(non_null)
                .map_err(|_| AxError::NoMemory)?;
            for raw in &chunk[..non_null] {
                // SAFETY: read_slice initialized every element in this chunk.
                values.push(unsafe { raw.assume_init() as *const c_char });
            }
        }
        if non_null != chunk_len {
            return Ok(values);
        }

        index = index.checked_add(chunk_len).ok_or(AxError::BadAddress)?;
    }

    Err(exec_arg_too_big())
}

fn load_exec_string_vec<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const *const c_char,
    sizer: &mut ExecArgSizer,
) -> AxResult<Vec<String>> {
    let ptrs = snapshot_exec_pointer_array(memory, ptr, sizer)?;
    let mut values = Vec::new();
    if !ptrs.is_empty() {
        values
            .try_reserve_exact(ptrs.len())
            .map_err(|_| AxError::NoMemory)?;
    }
    for ptr in ptrs {
        let value = vm_load_exec_string(memory, ptr)?;
        sizer.push_string_bytes(&value)?;
        values.push(value);
    }
    Ok(values)
}

fn load_exec_args_env<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> AxResult<(Vec<String>, Vec<String>)> {
    let mut sizer = ExecArgSizer::new()?;
    let args = if argv.is_null() {
        Vec::new()
    } else {
        load_exec_string_vec(memory, argv, &mut sizer)?
    };
    let args = if args.is_empty() {
        sizer.push_str("")?;
        let mut args = Vec::new();
        args.try_reserve_exact(1).map_err(|_| AxError::NoMemory)?;
        args.push(String::new());
        args
    } else {
        args
    };

    let envs = if envp.is_null() {
        Vec::new()
    } else {
        load_exec_string_vec(memory, envp, &mut sizer)?
    };

    Ok((args, envs))
}

fn do_execve(
    uctx: &mut UserContext,
    loc: axfs_ng_vfs::Location,
    args: Vec<String>,
    envs: Vec<String>,
    security: &VfsSecurityContext,
) -> AxResult<isize> {
    let actor = security.actor_arc();
    let credentials = security.credentials();
    let curr = current();
    let curr_tid = linux_pid_from_task_id(curr.id().as_u64())?;
    fanotify::permission_check(
        &loc,
        &loc,
        fanotify::FAN_OPEN_EXEC_PERM,
        loc.is_dir(),
        false,
    )?;
    fanotify::permission_check(&loc, &loc, fanotify::FAN_OPEN_PERM, loc.is_dir(), false)?;

    let abs_path = {
        let absolute_path = loc.absolute_path()?;
        try_copy_string(absolute_path.as_str())
    }?;
    let task_name = try_copy_string(loc.name())?;

    let thr = curr.as_thread();
    let proc_data = &thr.proc_data;
    // Keep the old registration/events intact through every fallible image
    // preparation step. The reservation clears them only after the new image
    // has crossed its irreversible publication boundary.
    let rseq_exec = thr.prepare_rseq_exec()?;
    // Resolve the terminal interpreter and derive its privilege effects before
    // selecting any personality-controlled address-space behavior.  The
    // preflight holds the exact credential lease; mapping consumes its cached
    // ELF entries later, so this does not perform a second executable load.
    let mut prepared_app = preflight_user_app_at(
        loc.clone(),
        abs_path.as_str(),
        &args,
        credentials,
        actor,
        security.filesystem_owner_user_ns(),
    )?;

    // Linearize against PTRACE_ATTACH/PTRACE_SEIZE and thread publication
    // before sampling either the trace relationship or terminal privilege
    // metadata. If attach published first, this exec observes that exact
    // relationship. If this gate published first, a later attach is rejected
    // until the new image is completely visible.
    let exec_admission =
        ExecAdmission::try_begin(proc_data, curr_tid).ok_or(AxError::Interrupted)?;
    let exec_ptrace_relationship = proc_data.ptrace_relationship_snapshot();

    // Only the terminal ELF (the shebang interpreter when the initial object
    // is a script) supplies set-ID and file-capability privilege. PT_INTERP is
    // part of the readability/content chain but is never a credential source.
    let source_security = prepared_app.take_credential_source_security()?;
    let source_mode = source_security.mode();
    let final_exe_path = {
        let path = prepared_app.credential_source.absolute_path()?;
        try_copy_string(path.as_str())?
    };
    let file_owner = source_security.owner();
    let nosuid = crate::mounts::is_nosuid(&prepared_app.credential_source)?;
    let file_capabilities = exec_file_capabilities(nosuid, || {
        crate::file::xattr_provider::security_capabilities_for_exec(&prepared_app.credential_source)
    })?;
    let input = ExecCredentialInput::new(
        source_mode,
        file_owner,
        if nosuid {
            ExecMountPrivilege::NoSuid
        } else {
            ExecMountPrivilege::Honor
        },
        exec_trace_state(exec_ptrace_relationship.as_ref(), actor.user_ns()),
        if prepared_app.image_access.executable_unreadable() {
            ExecImageReadability::Unreadable
        } else {
            ExecImageReadability::Readable
        },
        file_capabilities,
    );
    let credential_lease = prepared_app.take_credential_lease()?;
    let prepared_exec_cred = thr.prepare_exec_credential(actor, input, source_security)?;
    let effects = prepared_exec_cred.effects();
    let clear_personality_on_setid = effects.clear_personality_on_setid();
    let effective_personality =
        effective_exec_personality(thr.personality(), clear_personality_on_setid);
    let mmap_page_zero = effective_personality & MMAP_PAGE_ZERO != 0;
    let layout = if effective_personality & crate::syscall::sys::ADDR_NO_RANDOMIZE != 0 {
        ExecLayout::fixed()
    } else {
        ExecLayout::randomized()
    };
    let mut new_aspace = if mmap_page_zero {
        new_user_aspace_with_page_zero()?
    } else {
        new_user_aspace_empty()?
    };
    copy_from_kernel(&mut new_aspace)?;
    let mm_owner_user_ns = exec_mm_owner_user_ns(
        prepared_exec_cred.proposed_user_ns(),
        prepared_app.image_access,
    );
    let new_access_state = ProcessAccessState::try_new(
        map_exec_dumpability(effects.dumpability()),
        mm_owner_user_ns,
    )?;

    // The proposed identity, not the old current credential, owns all five
    // identity/security auxv entries installed into the new stack.
    let loaded = finish_prepared_user_app(
        &mut new_aspace,
        abs_path.as_str(),
        &envs,
        prepared_app,
        effects.aux_identity(),
        layout,
    )?;
    let entry_point = loaded.entry_point;
    let user_stack_base = loaded.stack_pointer;
    // Page zero is a personality VMA, not loader input.  Add it after the
    // preflight-backed image mapping; secure exec selected the cleared
    // effective personality above.
    if mmap_page_zero {
        install_mmap_page_zero(&mut new_aspace)?;
    }
    fanotify::notify(
        &loc,
        &loc,
        fanotify::FAN_OPEN | fanotify::FAN_OPEN_EXEC,
        loc.is_dir(),
        false,
    );

    // Everything needed by the new image is owned before de-threading or any
    // irreversible close/address-space publication. In particular, neither
    // the address-space handle nor the procfs cmdline may allocate after the
    // CLOEXEC commit point.
    let new_aspace = Arc::try_new(axsync::Mutex::new(new_aspace)).map_err(|_| AxError::NoMemory)?;
    let new_token = new_aspace.lock().address_space_token();
    let new_cmdline = Arc::try_new(loaded.arguments).map_err(|_| AxError::NoMemory)?;
    let new_task_alias = (!thr.is_thread_group_leader()).then(|| curr.clone());
    let task_alias_admission = new_task_alias
        .as_ref()
        .map(|task| prepare_task_alias_admission(proc_data.proc.pid(), task))
        .transpose()?;

    // This fallible registry snapshot is prepared while the exec gate excludes
    // new threads. The allocation-free ordered TID recheck still detects a
    // sibling exit before de-threading without resampling a different set.
    let mut sibling_tids = proc_data.proc.try_threads().map_err(process_error)?;

    // Take the private files snapshot before killing sibling threads. A
    // failure can then cancel exec without leaving the old image unexpectedly
    // de-threaded. Multi-thread exec always needs a snapshot because this
    // kernel represents all sibling files pointers with one process-scope
    // Arc; a single-thread caller needs one only when CLONE_FILES shares the
    // table with another process.
    let has_siblings = sibling_tids.len() > 1;
    let private_fd_table = if has_siblings || Arc::strong_count(&*FD_TABLE) > 1 {
        Some(Arc::try_new(FD_TABLE.fork_copy()?).map_err(|_| AxError::NoMemory)?)
    } else {
        None
    };
    let cloexec = match private_fd_table.as_ref() {
        Some(table) => table.prepare_cloexec(),
        None => FD_TABLE.prepare_cloexec(),
    };
    let cloexec = cloexec?;

    if !exact_exec_thread_snapshot(
        &sibling_tids,
        proc_data.proc.thread_count(),
        proc_data.proc.thread_ids(),
    ) || !files_preparation_covers_thread_snapshot(private_fd_table.is_some(), &sibling_tids)
    {
        return Err(AxError::Interrupted);
    }
    // A fresh relationship cannot publish after ExecAdmission has linearized.
    // Detach is harmless (and may leave a conservative suppression decision),
    // but a different exact session/credential is an internal invariant
    // failure rather than a retry-shaped EINTR.
    let current_ptrace_relationship = proc_data.ptrace_relationship_snapshot();
    if !exec_ptrace_relationship_is_stable(
        exec_ptrace_relationship.as_ref(),
        current_ptrace_relationship.as_ref(),
    ) {
        return Err(AxError::OperationNotPermitted);
    }
    // Keep the ABI crate's privilege-sensitive check as a second fail-closed
    // assertion over the exact relationship classification.
    if prepared_exec_cred.revalidation().is_stale(exec_trace_state(
        current_ptrace_relationship.as_ref(),
        actor.user_ns(),
    )) {
        return Err(AxError::OperationNotPermitted);
    }
    // Reserve the private sighand owner before interrupting or waiting for any
    // sibling. Its commit re-snapshots the fixed action table under the source
    // owner gate, so peer updates which linearize while siblings drain are
    // retained without allowing an allocation failure after teardown.
    let prepared_signal_unshare = proc_data
        .signal
        .try_prepare_exec_unshare()
        .map_err(|_| AxError::NoMemory)?;
    sibling_tids.retain(|&tid| tid != curr_tid);
    interrupt_exec_siblings(&sibling_tids);
    wait_for_exec_group(proc_data, thr, uctx, curr_tid, &sibling_tids)?;
    // No-failure commit: the manager retains its old queues/registrations,
    // while the owner swap resets only caught dispositions and leaves
    // sighand-sharing peers on the old owner.
    prepared_signal_unshare.commit();
    if let Some(private) = private_fd_table {
        let previous = thr.with_mut_scope(|scope| replace_process_fd_table(scope, private));
        drop(previous);
    }
    // The selected table owns full-capacity detach and cleanup storage. Commit
    // covers flags/descriptors added after preparation and has no recoverable
    // branch or runtime invariant panic.
    cloexec.commit();
    crate::file::inotify::wait_current_close_notifications();

    crate::syscall::cleanup_process_aio(proc_data.proc.pid());

    // POSIX timers created by timer_create(2) belong to the old process
    // image and do not survive a successful exec.  We are already beyond the
    // recoverable exec boundary here, so detach the complete table under its
    // owner lock and retire timer/alarm leases only after releasing that lock.
    // Timerfd objects deliberately remain in the file table unless CLOEXEC
    // selected them above; setitimer state is also preserved separately.
    let retired_posix_timers = {
        let mut timers = proc_data.posix_timers.lock();
        mem::take(&mut *timers)
    };
    drop(retired_posix_timers);

    // This is the Linux bprm_committing_creds analogue. Sibling teardown,
    // CLOEXEC, and AIO cleanup have crossed the point of no return; the hook is
    // infallible and the resulting typestate is the only value accepted by
    // composite credential/image publication.
    let exec_runtime = ExecCommitRuntime::new(
        proc_data.proc.pid(),
        curr_tid,
        proc_data.proc.pid(),
        ExecImageIdentity::from_arc(&new_aspace),
        new_access_state.owner_user_ns().clone(),
    );
    let committing_exec_cred = prepared_exec_cred.begin_commit(credential_lease, exec_runtime);

    // A non-leader exec adopts the thread-group ID. The visible TID, reserved
    // alias, credential/group-leader binding, address space, and access owner
    // become visible as one composite transition. No fallible commit remains;
    // the signal-owner commit above deliberately precedes this image handoff.
    // Process lifecycle serialization also keeps a signal operation from
    // publishing through the old thread-pid identity after this handoff.
    // Security post-commit notification remains below, after the guard drops.
    let lifecycle = proc_data.lock_process_lifecycle();
    let exec_commit = commit_exec_identity_handoff(
        task_alias_admission,
        proc_data,
        curr_tid,
        thr,
        committing_exec_cred,
        new_aspace,
        new_access_state,
    );
    if clear_personality_on_setid {
        thr.clear_personality_flags(PER_CLEAR_ON_SETID);
    }
    drop(lifecycle);
    keyring::exec_committed(KeyTaskOwner::new(thr.kernel_tid(), proc_data.proc.pid()))
        .unwrap_or_else(|error| fail_closed_exit(error));
    // Image, group-leader, and task-alias publication locks are now absent.
    // Notify before taking the ptrace-action lock, while the returned
    // retirement continues to own the old credential and old image through
    // the hardware page-table-root switch below.
    let mut exec_retirement = exec_commit.complete_post_commit();
    // Freeze the relationship which observed the exec commit while the exec
    // gate still excludes a fresh attach. The action gate keeps that exact
    // relationship stable through stop publication; a later attachment must
    // not inherit this already-committed exec event.
    let exec_ptrace_action = proc_data.lock_ptrace_actions();
    let exec_ptrace_session = proc_data.ptrace_active_session();
    let executable_key = exec_retirement
        .finish_executable_lease()
        .unwrap_or_else(|error| fail_closed_exit(error));
    set_current_user_address_space(new_token);
    proc_data.replace_executable(executable_key);

    drop(curr.replace_name(task_name));

    let old_exe_path = {
        let mut exe_path_guard = proc_data.exe_path.write();
        mem::replace(&mut *exe_path_guard, final_exe_path)
    };
    drop(old_exe_path);
    let old_cmdline = {
        let mut cmdline_guard = proc_data.cmdline.write();
        mem::replace(&mut *cmdline_guard, new_cmdline)
    };
    drop(old_cmdline);

    proc_data.set_heap_layout(layout.heap_base());

    thr.signal.set_stack(Default::default());

    // Clear clear_child_tid after exec since the original address is no longer valid.
    curr.as_thread().set_clear_child_tid(0);
    curr.as_thread().set_robust_list_head(0);

    install_exec_user_context(uctx, entry_point.as_usize(), user_stack_base);
    reset_current_task_extended_state();
    let _ = rseq_exec.commit();
    if let Some(session) = exec_ptrace_session
        && proc_data.ptrace_stop(session, Signo::SIGTRAP as u8)
    {
        notify_ptrace_attach_stop(proc_data);
    }
    // The old page tables and exact credential/security owners stay alive while
    // the saved task context and hardware root both name the new image. Release
    // the ptrace action gate before the full-image callback, but retain those
    // owners through the exec/vfork publication gates below.
    let completed_exec_retirement = release_exec_action_then_complete(exec_ptrace_action, || {
        exec_retirement.complete_exec_committed()
    });
    // Keep CLONE_THREAD publication and the vfork parent gated until the
    // complete new image and its late committed notification are visible.
    exec_admission.release();
    proc_data.release_vfork();
    drop(completed_exec_retirement);
    Ok(0)
}

pub fn sys_execve<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    uctx: &mut UserContext,
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> AxResult<isize> {
    let path = vm_load_exec_string(memory, path)?;
    let (args, envs) = load_exec_args_env(memory, argv, envp)?;

    debug!("sys_execve <= path: {path:?}, args: {args:?}, envs: {envs:?}");

    // Freeze one immutable actor snapshot before path resolution; that same
    // Arc supplies DAC, component hooks, and credential derivation throughout.
    let security = VfsSecurityContext::new(current().as_thread().current_cred());
    let loc = resolve_at_with_security(AT_FDCWD, Some(&path), 0, &security)?
        .into_file()
        .ok_or(AxError::InvalidInput)?;
    do_execve(uctx, loc, args, envs, &security)
}

pub fn sys_execveat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    uctx: &mut UserContext,
    dirfd: i32,
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
    flags: i32,
) -> AxResult<isize> {
    if flags < 0 || (flags as u32) & !SUPPORTED_EXECVEAT_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }

    let path = vm_load_exec_string(memory, path)?;
    let (args, envs) = load_exec_args_env(memory, argv, envp)?;
    debug!(
        "sys_execveat <= dirfd: {dirfd}, path: {path:?}, args: {args:?}, envs: {envs:?}, flags: \
         {flags:#x}"
    );

    // Use one pre-exec view for the initial path and every interpreter lookup.
    let security = VfsSecurityContext::new(current().as_thread().current_cred());
    let resolved = if path.is_empty() {
        if (flags as u32) & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        resolve_at_with_security(dirfd, None, flags as u32, &security)?
    } else {
        resolve_at_with_security(dirfd, Some(path.as_str()), flags as u32, &security)?
    };

    let loc = match resolved {
        ResolveAtResult::File(loc) => loc,
        ResolveAtResult::Other(_) => return Err(AxError::InvalidInput),
    };
    if (flags as u32) & AT_SYMLINK_NOFOLLOW != 0 && loc.node_type() == NodeType::Symlink {
        return Err(axerrno::LinuxError::ELOOP.into());
    }

    do_execve(uctx, loc, args, envs, &security)
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec, vec::Vec};
    use core::{
        ffi::c_char,
        mem::{MaybeUninit, size_of},
        sync::atomic::{AtomicU32, Ordering},
    };

    use axerrno::AxError;
    use linux_raw_sys::general::CAP_SYS_PTRACE;
    use thekernel_linux_usercopy::{UserCopyError, UserMemory, UserMemoryContext, VmResult};

    use super::{
        ExecArgSizer, MappingFlags, PAGE_SIZE_4K, UserContext, VirtAddr, classify_exec_trace_state,
        effective_exec_personality, exact_exec_thread_snapshot, exec_arg_limit,
        exec_file_capabilities, exec_mm_owner_user_ns, files_preparation_covers_thread_snapshot,
        install_exec_user_context, install_mmap_page_zero, load_exec_string_vec,
    };
    use crate::{
        mm::{ExecImageAccess, new_user_aspace_with_page_zero},
        syscall::sys::PER_CLEAR_ON_SETID,
        task::{
            Cred, CredentialSlot, ExecTraceState, Kgid, Kuid, UserNamespace,
            release_exec_action_then_complete,
        },
    };

    fn without_effective_ptrace(credential: Arc<Cred>) -> Arc<Cred> {
        let slot = CredentialSlot::new(credential);
        slot.replace_capabilities_for_test(&[CAP_SYS_PTRACE], &[])
            .unwrap()
    }

    #[test]
    fn exec_replaces_old_image_registers_and_tls() {
        let old_stack = VirtAddr::from_usize(0x8000);
        let mut context = UserContext::new(0x1111, old_stack, 7);
        context.set_arg2(0x7fff_ffff_fe30);
        context.set_tls(0xfeed_face);

        let new_stack = VirtAddr::from_usize(0x20_0000);
        install_exec_user_context(&mut context, 0x40_1000, new_stack);

        assert_eq!(context.ip(), 0x40_1000);
        assert_eq!(context.sp(), new_stack.as_usize());
        assert_eq!(context.arg0(), 0);
        assert_eq!(context.arg2(), 0);
        assert_eq!(context.tls(), 0);
    }

    #[test]
    fn mmap_page_zero_is_read_execute_and_sealed() {
        let mut aspace = new_user_aspace_with_page_zero().unwrap();
        install_mmap_page_zero(&mut aspace).unwrap();

        let area = aspace.find_area(VirtAddr::from_usize(0)).unwrap();
        assert!(
            area.flags()
                .contains(MappingFlags::READ | MappingFlags::EXECUTE)
        );
        assert!(!area.flags().contains(MappingFlags::WRITE));
        assert!(area.backend().is_sealed());
        assert!(matches!(
            aspace.unmap(VirtAddr::from_usize(0), PAGE_SIZE_4K),
            Err(AxError::OperationNotPermitted)
        ));
    }

    #[test]
    fn secure_exec_uses_personality_after_all_setid_clears() {
        let personality = PER_CLEAR_ON_SETID | 0x8000_0000;
        assert_eq!(effective_exec_personality(personality, true), 0x8000_0000);
        assert_eq!(effective_exec_personality(personality, false), personality);
    }

    #[test]
    fn unreadable_exec_chain_uses_initial_mm_owner_namespace() {
        let initial = UserNamespace::try_new_root().unwrap();
        let child = initial
            .try_fork(
                Kuid::from_raw(1000).unwrap(),
                Kgid::from_raw(1000).unwrap(),
                false,
            )
            .unwrap();
        let readable = exec_mm_owner_user_ns(&child, ExecImageAccess::for_test(true));
        assert!(Arc::ptr_eq(&readable, &child));
        let unreadable = exec_mm_owner_user_ns(&child, ExecImageAccess::for_test(false));
        assert!(Arc::ptr_eq(&unreadable, &initial));
    }

    #[test]
    fn exec_thread_snapshot_rejects_same_count_tid_aba() {
        assert!(exact_exec_thread_snapshot(&[7, 8], 2, [7, 8].into_iter()));
        assert!(!exact_exec_thread_snapshot(&[7, 8], 2, [7, 9].into_iter()));
    }

    #[test]
    fn nosuid_skips_even_malformed_file_capability_payload() {
        assert_eq!(
            exec_file_capabilities(true, || Err(AxError::InvalidInput)),
            Ok(None)
        );
        assert_eq!(
            exec_file_capabilities(false, || Err(AxError::InvalidInput)),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn exec_files_preparation_rejects_a_clone_race() {
        assert!(files_preparation_covers_thread_snapshot(false, &[7]));
        assert!(!files_preparation_covers_thread_snapshot(false, &[7, 8]));
        assert!(files_preparation_covers_thread_snapshot(true, &[7, 8]));
    }

    #[test]
    fn exec_ptrace_privilege_uses_effective_attach_credential() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let capable = Cred::try_root(namespace.clone()).unwrap();
        assert_eq!(
            classify_exec_trace_state(Some(&capable), &namespace),
            ExecTraceState::NotSuppressingPrivilege
        );

        // Keeping CAP_SYS_PTRACE only in the permitted set is insufficient:
        // Linux ptracer_capable() uses the frozen credential's effective set.
        let permitted_only = without_effective_ptrace(capable);
        assert_eq!(
            classify_exec_trace_state(Some(&permitted_only), &namespace),
            ExecTraceState::SuppressingPrivilege
        );

        let tracer_slot = CredentialSlot::new(permitted_only.clone());
        let attach_time = tracer_slot.current();
        let current = tracer_slot
            .replace_capabilities_for_test(&[CAP_SYS_PTRACE], &[CAP_SYS_PTRACE])
            .unwrap();
        assert_eq!(
            classify_exec_trace_state(Some(&attach_time), &namespace),
            ExecTraceState::SuppressingPrivilege
        );
        assert_eq!(
            classify_exec_trace_state(Some(&current), &namespace),
            ExecTraceState::NotSuppressingPrivilege
        );
        assert_eq!(
            classify_exec_trace_state(None, &namespace),
            ExecTraceState::NotSuppressingPrivilege
        );
    }

    #[test]
    fn exec_ptrace_capability_follows_target_namespace_direction() {
        let initial = UserNamespace::try_new_root().unwrap();
        let child = initial
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let sibling = initial
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let initial_ptracer = Cred::try_root(initial.clone()).unwrap();
        let child_ptracer = Cred::try_with_user_namespace(&initial_ptracer, child.clone()).unwrap();
        let sibling_ptracer = Cred::try_with_user_namespace(&initial_ptracer, sibling).unwrap();

        assert_eq!(
            classify_exec_trace_state(Some(&initial_ptracer), &child),
            ExecTraceState::NotSuppressingPrivilege
        );
        assert_eq!(
            classify_exec_trace_state(Some(&child_ptracer), &initial),
            ExecTraceState::SuppressingPrivilege
        );
        assert_eq!(
            classify_exec_trace_state(Some(&sibling_ptracer), &child),
            ExecTraceState::SuppressingPrivilege
        );
    }

    #[test]
    fn exec_action_gate_is_released_before_retired_owners() {
        struct DropTrace<'a> {
            trace: &'a AtomicU32,
            value: u32,
        }

        impl Drop for DropTrace<'_> {
            fn drop(&mut self) {
                self.trace
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |old| {
                        Some(old * 10 + self.value)
                    })
                    .unwrap();
            }
        }

        let trace = AtomicU32::new(0);
        let retirement = release_exec_action_then_complete(
            DropTrace {
                trace: &trace,
                value: 1,
            },
            || {
                assert_eq!(trace.load(Ordering::SeqCst), 1);
                DropTrace {
                    trace: &trace,
                    value: 2,
                }
            },
        );
        assert_eq!(trace.load(Ordering::SeqCst), 1);
        drop(retirement);
        assert_eq!(trace.load(Ordering::SeqCst), 12);
    }

    struct TestMemory {
        bytes: Vec<u8>,
    }

    unsafe impl UserMemory for TestMemory {
        fn read(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult {
            let end = start
                .checked_add(dst.len())
                .ok_or(UserCopyError::BadAddress)?;
            let src = self
                .bytes
                .get(start..end)
                .ok_or(UserCopyError::BadAddress)?;
            for (slot, byte) in dst.iter_mut().zip(src) {
                slot.write(*byte);
            }
            Ok(())
        }

        fn write(&mut self, _start: usize, _src: &[u8]) -> VmResult {
            Err(UserCopyError::BadAddress)
        }
    }

    struct PageBoundedTestMemory {
        bytes: Vec<u8>,
        blocked_page: usize,
    }

    unsafe impl UserMemory for PageBoundedTestMemory {
        fn read(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult {
            let end = start
                .checked_add(dst.len())
                .ok_or(UserCopyError::BadAddress)?;
            if !dst.is_empty()
                && (start / PAGE_SIZE_4K == self.blocked_page
                    || (end - 1) / PAGE_SIZE_4K == self.blocked_page)
            {
                return Err(UserCopyError::BadAddress);
            }
            let src = self
                .bytes
                .get(start..end)
                .ok_or(UserCopyError::BadAddress)?;
            for (slot, byte) in dst.iter_mut().zip(src) {
                slot.write(*byte);
            }
            Ok(())
        }

        fn write(&mut self, _start: usize, _src: &[u8]) -> VmResult {
            Err(UserCopyError::BadAddress)
        }
    }

    fn put_usize(bytes: &mut [u8], offset: usize, value: usize) {
        bytes[offset..offset + size_of::<usize>()].copy_from_slice(&value.to_ne_bytes());
    }

    #[test]
    fn exec_pointer_array_allows_more_than_generic_scan_limit() {
        let count = 17_000usize;
        let base = 0x100usize;
        let array_bytes = (count + 1) * size_of::<usize>();
        let string = base + array_bytes;
        let mut bytes = vec![0; string + super::EXEC_POINTER_ARRAY_CHUNK * size_of::<usize>()];
        for index in 0..count {
            put_usize(&mut bytes, base + index * size_of::<usize>(), string);
        }
        put_usize(&mut bytes, base + count * size_of::<usize>(), 0);

        let mut provider = TestMemory { bytes };
        let mut memory = UserMemoryContext::new(&mut provider);
        let mut sizer = ExecArgSizer::new().unwrap();
        let values =
            load_exec_string_vec(&mut memory, base as *const *const c_char, &mut sizer).unwrap();

        assert_eq!(values.len(), count);
        assert!(sizer.bytes < exec_arg_limit());
    }

    #[test]
    fn exec_pointer_array_nul_at_page_tail_does_not_read_next_page() {
        let base = PAGE_SIZE_4K + size_of::<usize>();
        let count = PAGE_SIZE_4K / size_of::<usize>() - 2;
        let string = 3 * PAGE_SIZE_4K;
        let mut bytes = vec![0; string + PAGE_SIZE_4K];
        for index in 0..count {
            put_usize(&mut bytes, base + index * size_of::<usize>(), string);
        }
        put_usize(&mut bytes, base + count * size_of::<usize>(), 0);

        let mut provider = PageBoundedTestMemory {
            bytes,
            blocked_page: 2,
        };
        let mut memory = UserMemoryContext::new(&mut provider);
        let mut sizer = ExecArgSizer::new().unwrap();
        let values =
            load_exec_string_vec(&mut memory, base as *const *const c_char, &mut sizer).unwrap();

        assert_eq!(values.len(), count);
    }

    #[test]
    fn exec_empty_pointer_array_does_not_reserve_chunk() {
        let base = 0x100usize;
        let mut bytes = vec![0; base + PAGE_SIZE_4K];
        put_usize(&mut bytes, base, 0);

        let mut provider = TestMemory { bytes };
        let mut memory = UserMemoryContext::new(&mut provider);
        let mut sizer = ExecArgSizer::new().unwrap();
        let values =
            load_exec_string_vec(&mut memory, base as *const *const c_char, &mut sizer).unwrap();

        assert!(values.is_empty());
        assert_eq!(values.capacity(), 0);
    }

    #[test]
    fn exec_pointer_array_without_nul_is_e2big() {
        let base = 0x100usize;
        let mut bytes = vec![0; base + 2 * size_of::<usize>()];
        put_usize(&mut bytes, base, 1);
        put_usize(&mut bytes, base + size_of::<usize>(), 1);

        let mut provider = TestMemory { bytes };
        let mut memory = UserMemoryContext::new(&mut provider);
        let mut sizer = ExecArgSizer {
            bytes: exec_arg_limit() - size_of::<usize>(),
            limit: exec_arg_limit(),
        };
        let result = load_exec_string_vec(&mut memory, base as *const *const c_char, &mut sizer);

        assert_eq!(result, Err(super::exec_arg_too_big()));
    }

    #[test]
    fn exec_pointer_array_address_overflow_is_efault() {
        let mut provider = TestMemory { bytes: vec![0] };
        let mut memory = UserMemoryContext::new(&mut provider);
        let mut sizer = ExecArgSizer::new().unwrap();
        let result =
            load_exec_string_vec(&mut memory, usize::MAX as *const *const c_char, &mut sizer);

        assert_eq!(result, Err(AxError::BadAddress));
    }

    #[test]
    fn exec_pointer_and_string_budget_is_e2big() {
        let base = 0x100usize;
        let string = base + 2 * size_of::<usize>();
        let mut bytes = vec![0; string + 32];
        put_usize(&mut bytes, base, string);
        put_usize(&mut bytes, base + size_of::<usize>(), 0);
        bytes[string] = b'x';

        let mut provider = TestMemory { bytes };
        let mut memory = UserMemoryContext::new(&mut provider);
        let limit = exec_arg_limit();
        let mut sizer = ExecArgSizer {
            bytes: limit - size_of::<usize>() - 1,
            limit,
        };
        let result = load_exec_string_vec(&mut memory, base as *const *const c_char, &mut sizer);

        assert_eq!(result, Err(super::exec_arg_too_big()));
    }
}
