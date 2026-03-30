use alloc::{string::ToString, sync::Arc, vec::Vec};
use core::{ffi::c_char, future::poll_fn, task::Poll};

use axerrno::{AxError, AxResult};
use axfs::FS_CONTEXT;
use axhal::uspace::UserContext;
use axtask::{
    current,
    future::{block_on, interruptible},
};
use starry_process::Pid;
use starry_signal::{SignalAction, SignalDisposition, Signo};
use starry_vm::vm_load_until_nul;

use crate::{
    config::USER_HEAP_BASE,
    file::FD_TABLE,
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

pub fn sys_execve(
    uctx: &mut UserContext,
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> AxResult<isize> {
    let path = vm_load_string(path)?;

    let args = if argv.is_null() {
        // Handle NULL argv (treat as empty array)
        Vec::new()
    } else {
        vm_load_until_nul(argv)?
            .into_iter()
            .map(vm_load_string)
            .collect::<Result<Vec<_>, _>>()?
    };

    let envs = if envp.is_null() {
        // Handle NULL envp (treat as empty array)
        Vec::new()
    } else {
        vm_load_until_nul(envp)?
            .into_iter()
            .map(vm_load_string)
            .collect::<Result<Vec<_>, _>>()?
    };

    debug!("sys_execve <= path: {path:?}, args: {args:?}, envs: {envs:?}");

    let curr = current();
    let thr = curr.as_thread();
    let proc_data = &thr.proc_data;
    let curr_tid = curr.id().as_u64() as Pid;

    let loc = FS_CONTEXT.lock().resolve(&path)?;
    let abs_path = loc.absolute_path()?.to_string();
    let task_name = loc.name().to_string();

    let mut new_aspace = new_user_aspace_empty()?;
    copy_from_kernel(&mut new_aspace)?;
    let (entry_point, user_stack_base) =
        load_user_app(&mut new_aspace, Some(path.as_str()), &args, &envs)?;

    let mut exec_started = false;
    if proc_data.proc.threads().len() > 1 {
        if !proc_data.begin_exec(curr_tid) {
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
            return Err(err);
        }
    }

    let new_root = new_aspace.page_table_root();
    let old_aspace = {
        let mut aspace = proc_data.aspace.lock();
        core::mem::replace(&mut *aspace, new_aspace)
    };
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

    curr.set_name(&task_name);

    *proc_data.exe_path.write() = abs_path;
    *proc_data.cmdline.write() = Arc::new(args);

    proc_data.set_heap_top(USER_HEAP_BASE);

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

    uctx.set_ip(entry_point.as_usize());
    uctx.set_sp(user_stack_base.as_usize());
    Ok(0)
}
