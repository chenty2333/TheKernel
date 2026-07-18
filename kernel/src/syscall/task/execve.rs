use alloc::{string::String, sync::Arc, vec::Vec};
use core::{
    ffi::c_char,
    mem::{self, size_of},
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::NodeType;
use axhal::uspace::UserContext;
use axtask::current;
use linux_raw_sys::general::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW, CAP_SYS_PTRACE};
use memory_addr::PAGE_SIZE_4K;
use starry_process::Pid;
use starry_signal::{SignalAction, SignalDisposition, Signo};
use starry_vm::{VmError, vm_load_until_nul};

#[cfg(target_arch = "loongarch64")]
use crate::task::reset_current_user_fpu_state;
use crate::{
    config::USER_HEAP_BASE,
    file::{
        FD_TABLE, ResolveAtResult, fanotify, permission::VfsSecurityContext,
        replace_process_fd_table, resolve_at_with_security,
    },
    mm::{
        ExecImageAccess, copy_from_kernel, finish_prepared_user_app, new_user_aspace_empty,
        prepare_user_app_at, vm_load_string,
    },
    readiness::block_on_poll_set,
    task::{
        AsThread, Cred, ExecCommitRuntime, ExecCredentialInput, ExecImageIdentity,
        ExecImageReadability, ExecMountPrivilege, ExecTraceState, FileCapabilities,
        ProcessAccessState, ProcessData, PtraceRelationshipSnapshot, Thread, UserNamespace,
        check_signals, commit_exec_identity_handoff, fail_closed_exit, get_task,
        has_pending_fatal_signal, linux_pid_from_task_id, map_exec_dumpability,
        notify_ptrace_attach_stop, ns_capable, prepare_task_alias_admission, process_error,
        release_exec_action_then_complete, set_current_user_page_table_root,
    },
};

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

fn reset_exec_signal_state(thr: &Thread) {
    let mut actions = thr.proc_data.signal.actions.lock();
    for raw in 1..=64u8 {
        let Some(signo) = Signo::from_repr(raw) else {
            continue;
        };
        if matches!(actions[signo].disposition, SignalDisposition::Handler(_)) {
            actions[signo] = SignalAction::default();
        }
    }
    drop(actions);
    thr.signal.set_stack(Default::default());
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

fn map_exec_vm_error(err: VmError) -> AxError {
    match err {
        VmError::TooLong => exec_arg_too_big(),
        _ => err.into(),
    }
}

fn vm_load_exec_string(ptr: *const c_char) -> AxResult<String> {
    #[allow(clippy::unnecessary_cast)]
    let bytes = vm_load_until_nul(ptr as *const u8).map_err(map_exec_vm_error)?;
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

    fn push_str(&mut self, value: &str) -> AxResult {
        let string_bytes = value.len().checked_add(1).ok_or_else(exec_arg_too_big)?;
        if string_bytes > EXEC_MAX_ARG_STRLEN {
            return Err(exec_arg_too_big());
        }

        let entry_bytes = string_bytes
            .checked_add(size_of::<usize>())
            .ok_or_else(exec_arg_too_big)?;
        self.bytes = self
            .bytes
            .checked_add(entry_bytes)
            .ok_or_else(exec_arg_too_big)?;
        if self.bytes > self.limit {
            return Err(exec_arg_too_big());
        }
        Ok(())
    }
}

fn load_exec_string_vec(
    ptr: *const *const c_char,
    sizer: &mut ExecArgSizer,
) -> AxResult<Vec<String>> {
    let ptrs = vm_load_until_nul(ptr).map_err(map_exec_vm_error)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(ptrs.len())
        .map_err(|_| AxError::NoMemory)?;
    for ptr in ptrs {
        let value = vm_load_exec_string(ptr)?;
        sizer.push_str(&value)?;
        values.push(value);
    }
    Ok(values)
}

fn load_exec_args_env(
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> AxResult<(Vec<String>, Vec<String>)> {
    let mut sizer = ExecArgSizer::new()?;
    let args = if argv.is_null() {
        Vec::new()
    } else {
        load_exec_string_vec(argv, &mut sizer)?
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
        load_exec_string_vec(envp, &mut sizer)?
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
    let mut new_aspace = new_user_aspace_empty()?;
    copy_from_kernel(&mut new_aspace)?;
    let mut prepared_app = prepare_user_app_at(
        &mut new_aspace,
        loc.clone(),
        abs_path.as_str(),
        &args,
        &envs,
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
    )?;
    let entry_point = loaded.entry_point;
    let user_stack_base = loaded.stack_pointer;
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
    let new_root = new_aspace.lock().page_table_root();
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
    sibling_tids.retain(|&tid| tid != curr_tid);
    interrupt_exec_siblings(&sibling_tids);
    if let Err(err) = wait_for_exec_group(proc_data, thr, uctx, curr_tid, &sibling_tids) {
        return Err(err);
    }
    if let Some(private) = private_fd_table {
        let previous = thr.with_mut_scope(|scope| replace_process_fd_table(scope, private));
        drop(previous);
    }
    // The token owns the exact selected table plus full-capacity detach and
    // cleanup storage. Commit covers flags/descriptors added after preparation
    // and has no recoverable branch or runtime invariant panic.
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
    // become visible as one composite transition. No fallible commit remains.
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
    drop(lifecycle);
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
    set_current_user_page_table_root(new_root);
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

    proc_data.set_heap_top(USER_HEAP_BASE + crate::config::USER_HEAP_SIZE);

    #[cfg(target_arch = "loongarch64")]
    reset_current_user_fpu_state();

    reset_exec_signal_state(thr);

    // Clear clear_child_tid after exec since the original address is no longer valid.
    curr.as_thread().set_clear_child_tid(0);
    curr.as_thread().set_robust_list_head(0);

    uctx.set_ip(entry_point.as_usize());
    uctx.set_sp(user_stack_base.as_usize());
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

pub fn sys_execve(
    uctx: &mut UserContext,
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> AxResult<isize> {
    let path = vm_load_string(path)?;
    let (args, envs) = load_exec_args_env(argv, envp)?;

    debug!("sys_execve <= path: {path:?}, args: {args:?}, envs: {envs:?}");

    // Freeze one immutable actor snapshot before path resolution; that same
    // Arc supplies DAC, component hooks, and credential derivation throughout.
    let security = VfsSecurityContext::new(current().as_thread().current_cred());
    let loc = resolve_at_with_security(AT_FDCWD, Some(&path), 0, &security)?
        .into_file()
        .ok_or(AxError::InvalidInput)?;
    do_execve(uctx, loc, args, envs, &security)
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicU32, Ordering};

    use axerrno::AxError;
    use linux_raw_sys::general::CAP_SYS_PTRACE;

    use super::{
        classify_exec_trace_state, exact_exec_thread_snapshot, exec_file_capabilities,
        exec_mm_owner_user_ns, files_preparation_covers_thread_snapshot,
    };
    use crate::{
        mm::ExecImageAccess,
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
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |old| {
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
}

pub fn sys_execveat(
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

    let path = vm_load_string(path)?;
    let (args, envs) = load_exec_args_env(argv, envp)?;
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
