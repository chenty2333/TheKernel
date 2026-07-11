use alloc::{string::String, sync::Arc, vec::Vec};
use core::{
    ffi::c_char,
    future::poll_fn,
    mem::{self, size_of},
    task::Poll,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::NodeType;
use axhal::uspace::UserContext;
use axtask::{
    current,
    future::{block_on, interruptible},
};
use linux_raw_sys::general::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW};
use memory_addr::PAGE_SIZE_4K;
use starry_process::Pid;
use starry_signal::{SignalAction, SignalDisposition, Signo};
use starry_vm::{VmError, vm_load_until_nul};

#[cfg(target_arch = "loongarch64")]
use crate::task::reset_current_user_fpu_state;
use crate::{
    config::USER_HEAP_BASE,
    file::{
        FD_TABLE, ResolveAtResult, executable, fanotify, replace_process_fd_table,
        resolve_at_with_credentials,
    },
    mm::{copy_from_kernel, load_user_app_at, new_user_aspace_empty, vm_load_string},
    task::{
        AsThread, DacCredentialView, ProcessData, Thread, check_signals, get_task,
        has_pending_fatal_signal, notify_ptrace_attach_stop, prepare_task_alias_admission,
        set_current_user_page_table_root,
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
        match block_on(interruptible(poll_fn(|cx| {
            if !proc_data.is_exec_owner(curr_tid) || proc_data.exec_ready(curr_tid) {
                Poll::Ready(())
            } else {
                proc_data.exec_event.register(cx.waker());
                if !proc_data.is_exec_owner(curr_tid) || proc_data.exec_ready(curr_tid) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        }))) {
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

fn exec_or_release<T>(
    executable_key: Option<executable::ExecutableKey>,
    result: AxResult<T>,
) -> AxResult<T> {
    match result {
        Ok(value) => Ok(value),
        Err(err) => {
            executable::release(executable_key);
            Err(err)
        }
    }
}

fn do_execve(
    uctx: &mut UserContext,
    loc: axfs_ng_vfs::Location,
    args: Vec<String>,
    envs: Vec<String>,
    credentials: &DacCredentialView,
) -> AxResult<isize> {
    executable::check_not_write_open(&loc)?;
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
    let executable_key = executable::acquire_if_not_write_open(&loc)?;

    let mut new_aspace = exec_or_release(executable_key, new_user_aspace_empty())?;
    exec_or_release(executable_key, copy_from_kernel(&mut new_aspace))?;
    let (entry_point, user_stack_base) = exec_or_release(
        executable_key,
        load_user_app_at(
            &mut new_aspace,
            loc.clone(),
            abs_path.as_str(),
            &args,
            &envs,
            credentials,
        ),
    )?;
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
    let new_aspace = exec_or_release(
        executable_key,
        Arc::try_new(axsync::Mutex::new(new_aspace)).map_err(|_| AxError::NoMemory),
    )?;
    let new_root = new_aspace.lock().page_table_root();
    let new_cmdline = exec_or_release(
        executable_key,
        Arc::try_new(args).map_err(|_| AxError::NoMemory),
    )?;

    let curr = current();
    let thr = curr.as_thread();
    let proc_data = &thr.proc_data;
    let curr_tid = curr.id().as_u64() as Pid;
    let new_task_alias = (curr_tid != proc_data.proc.pid()).then(|| curr.clone());
    let task_alias_admission = match new_task_alias
        .as_ref()
        .map(|task| prepare_task_alias_admission(proc_data.proc.pid(), task))
        .transpose()
    {
        Ok(admission) => admission,
        Err(err) => {
            executable::release(executable_key);
            return Err(err);
        }
    };

    // Take the private files snapshot before killing sibling threads. A
    // failure can then cancel exec without leaving the old image unexpectedly
    // de-threaded. Multi-thread exec always needs a snapshot because this
    // kernel represents all sibling files pointers with one process-scope
    // Arc; a single-thread caller needs one only when CLONE_FILES shares the
    // table with another process.
    let has_siblings = proc_data.proc.thread_count() > 1;
    let private_fd_table = if has_siblings || Arc::strong_count(&*FD_TABLE) > 1 {
        match FD_TABLE.fork_copy() {
            Ok(table) => match Arc::try_new(table) {
                Ok(table) => Some(table),
                Err(_) => {
                    executable::release(executable_key);
                    return Err(AxError::NoMemory);
                }
            },
            Err(err) => {
                executable::release(executable_key);
                return Err(err);
            }
        }
    } else {
        None
    };
    let cloexec_batch = match private_fd_table.as_ref() {
        Some(table) => table.prepare_cloexec_batch(),
        None => FD_TABLE.prepare_cloexec_batch(),
    };
    let cloexec_batch = match cloexec_batch {
        Ok(batch) => batch,
        Err(err) => {
            executable::release(executable_key);
            return Err(err);
        }
    };

    // Gate every exec, including an apparently single-threaded one. Otherwise
    // CLONE_THREAD can publish a sibling between the preflight count and the
    // irreversible commit. The gate freezes thread-group growth; the snapshot
    // below then validates that the files preparation made before the gate is
    // still sufficient. If clone won the race, abort before interrupting any
    // sibling and let userspace retry instead of committing with a shared
    // process-scope files pointer.
    if !proc_data.begin_exec(curr_tid) {
        executable::release(executable_key);
        return Err(AxError::Interrupted);
    }
    let mut sibling_tids = match proc_data.proc.try_threads() {
        Ok(threads) => threads,
        Err(_) => {
            proc_data.end_exec(curr_tid);
            executable::release(executable_key);
            return Err(AxError::NoMemory);
        }
    };
    if !files_preparation_covers_thread_snapshot(private_fd_table.is_some(), &sibling_tids) {
        proc_data.end_exec(curr_tid);
        executable::release(executable_key);
        return Err(AxError::Interrupted);
    }
    sibling_tids.retain(|&tid| tid != curr_tid);
    interrupt_exec_siblings(&sibling_tids);
    if let Err(err) = wait_for_exec_group(proc_data, thr, uctx, curr_tid, &sibling_tids) {
        proc_data.end_exec(curr_tid);
        executable::release(executable_key);
        return Err(err);
    }
    // Credential allocation and invariant checks must finish before the first
    // irreversible exec action. The prepared value remains invisible until
    // the address-space commit below.
    let prepared_exec_cred = match thr.prepare_clear_keep_caps_on_exec() {
        Ok(prepared) => prepared,
        Err(err) => {
            proc_data.end_exec(curr_tid);
            executable::release(executable_key);
            return Err(err);
        }
    };

    // A non-leader exec adopts the thread-group ID. Its lookup bucket was
    // admitted before de-threading, so this publication cannot fail before
    // the first irreversible close. `get_visible_task()` will not expose the
    // alias until `set_tid()` below changes the task's visible ID.
    if let Some(admission) = task_alias_admission {
        admission.commit();
    }

    if let Some(private) = private_fd_table {
        let previous = thr.with_mut_scope(|scope| replace_process_fd_table(scope, private));
        drop(previous);
    }
    // The buffer was reserved before de-threading, and the selected table is
    // either private or owned by this sole thread. Detaching all CLOEXEC slots
    // is therefore allocation-free and cannot fail at the exec commit point.
    drop(FD_TABLE.close_cloexec(cloexec_batch));
    crate::file::inotify::wait_current_close_notifications();

    crate::syscall::cleanup_process_aio(proc_data.proc.pid());
    let old_aspace = proc_data.replace_aspace(new_aspace);
    set_current_user_page_table_root(new_root);
    drop(old_aspace);
    if let Some(prepared) = prepared_exec_cred {
        prepared.commit();
    }
    curr.as_thread().set_tid(proc_data.proc.pid());
    proc_data.replace_executable(executable_key);

    drop(curr.replace_name(task_name));

    let old_exe_path = {
        let mut exe_path_guard = proc_data.exe_path.write();
        mem::replace(&mut *exe_path_guard, abs_path)
    };
    drop(old_exe_path);
    let old_cmdline = {
        let mut cmdline_guard = proc_data.cmdline.write();
        mem::replace(&mut *cmdline_guard, new_cmdline)
    };
    drop(old_cmdline);

    proc_data.set_heap_top(USER_HEAP_BASE + crate::config::USER_HEAP_SIZE);
    proc_data.clear_mempolicy_ranges();

    #[cfg(target_arch = "loongarch64")]
    reset_current_user_fpu_state();

    reset_exec_signal_state(thr);

    // Clear set_child_tid after exec since the original address is no longer valid
    curr.as_thread().set_clear_child_tid(0);
    curr.as_thread().set_robust_list_head(0);

    // Keep CLONE_THREAD publication gated until every process-wide field of
    // the new image has been installed. Releasing the gate at the address-space
    // swap would let a new sibling observe a half-committed exec image.
    proc_data.end_exec(curr_tid);
    proc_data.release_vfork();

    uctx.set_ip(entry_point.as_usize());
    uctx.set_sp(user_stack_base.as_usize());
    if proc_data.ptrace_tracer().is_some() && proc_data.ptrace_stop(Signo::SIGTRAP as u8) {
        notify_ptrace_attach_stop(proc_data);
    }
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

    // Credential locks precede FS_CONTEXT for the whole pathname operation.
    let credentials = current().as_thread().fs_dac_credentials();
    let loc = resolve_at_with_credentials(AT_FDCWD, Some(&path), 0, &credentials)?
        .into_file()
        .ok_or(AxError::InvalidInput)?;
    do_execve(uctx, loc, args, envs, &credentials)
}

#[cfg(test)]
mod tests {
    use super::files_preparation_covers_thread_snapshot;

    #[test]
    fn exec_files_preparation_rejects_a_clone_race() {
        assert!(files_preparation_covers_thread_snapshot(false, &[7]));
        assert!(!files_preparation_covers_thread_snapshot(false, &[7, 8]));
        assert!(files_preparation_covers_thread_snapshot(true, &[7, 8]));
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
    let credentials = current().as_thread().fs_dac_credentials();
    let resolved = if path.is_empty() {
        if (flags as u32) & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        resolve_at_with_credentials(dirfd, None, flags as u32, &credentials)?
    } else {
        resolve_at_with_credentials(dirfd, Some(path.as_str()), flags as u32, &credentials)?
    };

    let loc = match resolved {
        ResolveAtResult::File(loc) => loc,
        ResolveAtResult::Other(_) => return Err(AxError::InvalidInput),
    };
    if (flags as u32) & AT_SYMLINK_NOFOLLOW != 0 && loc.node_type() == NodeType::Symlink {
        return Err(axerrno::LinuxError::ELOOP.into());
    }

    do_execve(uctx, loc, args, envs, &credentials)
}
