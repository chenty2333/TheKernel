use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::{ffi::c_char, future::poll_fn, mem::size_of, task::Poll};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::FS_CONTEXT;
use axfs_ng_vfs::NodeType;
use axhal::uspace::UserContext;
use axtask::{
    current,
    future::{block_on, interruptible},
};
use linux_raw_sys::general::{AT_EMPTY_PATH, AT_SYMLINK_NOFOLLOW};
use memory_addr::PAGE_SIZE_4K;
use starry_process::Pid;
use starry_signal::{SignalAction, SignalDisposition, Signo};
use starry_vm::{VmError, vm_load_until_nul};

#[cfg(target_arch = "loongarch64")]
use crate::task::reset_current_user_fpu_state;
use crate::{
    config::USER_HEAP_BASE,
    file::{FD_TABLE, ResolveAtResult, executable, resolve_at},
    mm::{copy_from_kernel, load_user_app, new_user_aspace_empty, vm_load_string},
    task::{
        AsThread, ProcessData, Thread, add_task_alias, check_signals, get_task,
        has_pending_fatal_signal, set_current_user_page_table_root,
    },
};

fn interrupt_exec_siblings(sibling_tids: &[Pid]) {
    for &tid in sibling_tids {
        if let Ok(task) = get_task(tid) {
            task.interrupt();
        }
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
    let mut values = Vec::with_capacity(ptrs.len());
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
        vec![String::new()]
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
) -> AxResult<isize> {
    let abs_path = loc.absolute_path()?.to_string();
    let task_name = loc.name().to_string();

    let mut new_aspace = new_user_aspace_empty()?;
    copy_from_kernel(&mut new_aspace)?;
    let (entry_point, user_stack_base) =
        load_user_app(&mut new_aspace, Some(abs_path.as_str()), &args, &envs)?;
    let executable_key = executable::acquire(&loc);

    let curr = current();
    let thr = curr.as_thread();
    let proc_data = &thr.proc_data;
    let curr_tid = curr.id().as_u64() as Pid;

    let mut exec_started = false;
    if proc_data.proc.threads().len() > 1 {
        if !proc_data.begin_exec(curr_tid) {
            executable::release(executable_key);
            return Err(AxError::Interrupted);
        }
        exec_started = true;
        let sibling_tids = proc_data
            .proc
            .threads()
            .into_iter()
            .filter(|&tid| tid != curr_tid)
            .collect::<Vec<_>>();
        interrupt_exec_siblings(&sibling_tids);
        if let Err(err) = wait_for_exec_group(proc_data, thr, uctx, curr_tid, &sibling_tids) {
            proc_data.end_exec(curr_tid);
            executable::release(executable_key);
            return Err(err);
        }
    }

    let new_aspace = Arc::new(axsync::Mutex::new(new_aspace));
    let new_root = new_aspace.lock().page_table_root();
    let old_aspace = proc_data.replace_aspace(new_aspace);
    set_current_user_page_table_root(new_root);
    drop(old_aspace);
    curr.as_thread().set_tid(proc_data.proc.pid());
    if curr_tid != proc_data.proc.pid() {
        let curr_task = curr.clone();
        add_task_alias(proc_data.proc.pid(), &curr_task);
    }
    if exec_started {
        proc_data.end_exec(curr_tid);
    }
    proc_data.replace_executable(executable_key);

    curr.set_name(&task_name);

    #[cfg(target_arch = "loongarch64")]
    {
        let mut exe_path_guard = proc_data.exe_path.write();
        let old_exe_path = core::mem::replace(&mut *exe_path_guard, abs_path);
        core::mem::forget(old_exe_path);
        drop(exe_path_guard);

        let mut cmdline_guard = proc_data.cmdline.write();
        let old_cmdline = core::mem::replace(&mut *cmdline_guard, Arc::new(args));
        core::mem::forget(old_cmdline);
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        *proc_data.exe_path.write() = abs_path;
        *proc_data.cmdline.write() = Arc::new(args);
    }

    proc_data.set_heap_top(USER_HEAP_BASE + crate::config::USER_HEAP_SIZE);

    #[cfg(target_arch = "loongarch64")]
    reset_current_user_fpu_state();

    reset_exec_signal_state(thr);

    // Clear set_child_tid after exec since the original address is no longer valid
    curr.as_thread().set_clear_child_tid(0);
    curr.as_thread().set_robust_list_head(0);

    // Close CLOEXEC file descriptors
    let mut fd_table = FD_TABLE.write();
    let cloexec_fds = fd_table
        .ids()
        .filter(|it| fd_table.get(*it).unwrap().cloexec)
        .collect::<Vec<_>>();
    for fd in cloexec_fds {
        fd_table.remove(fd);
    }
    drop(fd_table);

    proc_data.release_vfork();

    uctx.set_ip(entry_point.as_usize());
    uctx.set_sp(user_stack_base.as_usize());
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

    let loc = FS_CONTEXT.lock().resolve(&path)?;
    do_execve(uctx, loc, args, envs)
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

    let resolved = if path.is_empty() {
        if (flags as u32) & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        resolve_at(dirfd, None, flags as u32)?
    } else {
        resolve_at(dirfd, Some(path.as_str()), flags as u32)?
    };

    let loc = match resolved {
        ResolveAtResult::File(loc) => loc,
        ResolveAtResult::Other(_) => return Err(AxError::InvalidInput),
    };
    if (flags as u32) & AT_SYMLINK_NOFOLLOW != 0 && loc.node_type() == NodeType::Symlink {
        return Err(axerrno::LinuxError::ELOOP.into());
    }

    do_execve(uctx, loc, args, envs)
}
