use alloc::sync::Arc;
use core::{future::poll_fn, task::Poll};

use axerrno::{AxError, AxResult};
use axfs::FS_CONTEXT;
use axhal::uspace::UserContext;
use axtask::{
    AxTaskExt, SchedClass, current, future::block_on, reclaim_exited_tasks_if_many,
    sched_state,
    spawn_task_with_sched, yield_now,
};
use bitflags::bitflags;
use kspin::SpinNoIrq;
use linux_raw_sys::general::*;
use starry_process::Pid;
use starry_signal::Signo;
use starry_vm::VmMutPtr;

#[cfg(target_arch = "loongarch64")]
use crate::task::copy_current_user_fpu_state_to;
use crate::{
    file::{FD_TABLE, FileLike, PidFd, close_file_like},
    mm::copy_from_kernel,
    syscall::inherit_proc_shm,
    task::{AsThread, ProcessData, Thread, add_task_to_table, new_user_task, vm_write_in_aspace},
};

bitflags! {
    /// Options for use with [`sys_clone`] and [`sys_clone3`].
    #[derive(Debug, Clone, Copy, Default)]
    pub struct CloneFlags: u64 {
        /// The calling process and the child process run in the same memory space.
        const VM = CLONE_VM as u64;
        /// The caller and the child process share the same filesystem information.
        const FS = CLONE_FS as u64;
        /// The calling process and the child process share the same file descriptor table.
        const FILES = CLONE_FILES as u64;
        /// The calling process and the child process share the same table of signal handlers.
        const SIGHAND = CLONE_SIGHAND as u64;
        /// Sets pidfd to the child process's PID file descriptor.
        const PIDFD = CLONE_PIDFD as u64;
        /// If the calling process is being traced, then trace the child also.
        const PTRACE = CLONE_PTRACE as u64;
        /// The execution of the calling process is suspended until the child releases
        /// its virtual memory resources via a call to execve(2) or _exit(2) (as with vfork(2)).
        const VFORK = CLONE_VFORK as u64;
        /// The parent of the new child (as returned by getppid(2)) will be the same
        /// as that of the calling process.
        const PARENT = CLONE_PARENT as u64;
        /// The child is placed in the same thread group as the calling process.
        const THREAD = CLONE_THREAD as u64;
        /// The cloned child is started in a new mount namespace.
        const NEWNS = CLONE_NEWNS as u64;
        /// The child and the calling process share a single list of System V
        /// semaphore adjustment values.
        const SYSVSEM = CLONE_SYSVSEM as u64;
        /// The TLS (Thread Local Storage) descriptor is set to tls.
        const SETTLS = CLONE_SETTLS as u64;
        /// Store the child thread ID in the parent's memory.
        const PARENT_SETTID = CLONE_PARENT_SETTID as u64;
        /// Clear (zero) the child thread ID in child memory when the child exits,
        /// and do a wakeup on the futex at that address.
        const CHILD_CLEARTID = CLONE_CHILD_CLEARTID as u64;
        /// A tracing process cannot force `CLONE_PTRACE` on this child process.
        const UNTRACED = CLONE_UNTRACED as u64;
        /// Store the child thread ID in the child's memory.
        const CHILD_SETTID = CLONE_CHILD_SETTID as u64;
        /// Create the process in a new cgroup namespace.
        const NEWCGROUP = CLONE_NEWCGROUP as u64;
        /// Create the process in a new UTS namespace.
        const NEWUTS = CLONE_NEWUTS as u64;
        /// Create the process in a new IPC namespace.
        const NEWIPC = CLONE_NEWIPC as u64;
        /// Create the process in a new user namespace.
        const NEWUSER = CLONE_NEWUSER as u64;
        /// Create the process in a new PID namespace.
        const NEWPID = CLONE_NEWPID as u64;
        /// Create the process in a new network namespace.
        const NEWNET = CLONE_NEWNET as u64;
        /// The new process shares an I/O context with the calling process.
        const IO = CLONE_IO as u64;
        /// Clear signal handlers on clone (since Linux 5.5).
        const CLEAR_SIGHAND = 0x100000000u64;
        /// Clone into specific cgroup (since Linux 5.7).
        const INTO_CGROUP = 0x200000000u64;
        /// (Deprecated) Causes the parent not to receive a signal when the child terminated.
        const DETACHED = CLONE_DETACHED as u64;
    }
}

/// Unified arguments for clone/clone3/fork/vfork.
#[derive(Debug, Clone, Copy, Default)]
pub struct CloneArgs {
    pub flags: CloneFlags,
    pub exit_signal: u64,
    pub stack: usize,
    pub tls: usize,
    pub parent_tid: usize,
    pub child_tid: usize,
    pub pidfd: usize,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum CloneApi {
    Clone,
    Clone3,
}

impl CloneArgs {
    fn wait_for_vfork(proc_data: &ProcessData) {
        block_on(poll_fn(|cx| {
            if !proc_data.vfork_in_progress() {
                return Poll::Ready(());
            }
            proc_data.vfork_event.register(cx.waker());
            if !proc_data.vfork_in_progress() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }));
    }

    pub(super) fn validate_for(&self, api: CloneApi) -> AxResult<()> {
        let Self { flags, .. } = self;

        if flags.contains(CloneFlags::THREAD)
            && !flags.contains(CloneFlags::VM | CloneFlags::SIGHAND)
        {
            return Err(AxError::InvalidInput);
        }
        if flags.contains(CloneFlags::SIGHAND) && !flags.contains(CloneFlags::VM) {
            return Err(AxError::InvalidInput);
        }
        if flags.contains(CloneFlags::VFORK | CloneFlags::THREAD) {
            return Err(AxError::InvalidInput);
        }
        if flags.contains(CloneFlags::DETACHED) {
            match api {
                CloneApi::Clone if !flags.contains(CloneFlags::PIDFD) => {}
                CloneApi::Clone | CloneApi::Clone3 => return Err(AxError::InvalidInput),
            }
        }
        if flags.contains(CloneFlags::PIDFD | CloneFlags::THREAD) {
            return Err(AxError::InvalidInput);
        }
        if flags.contains(CloneFlags::THREAD | CloneFlags::NEWNET) {
            return Err(AxError::InvalidInput);
        }

        if flags.contains(CloneFlags::FS | CloneFlags::NEWNS) {
            return Err(AxError::InvalidInput);
        }

        if flags.contains(CloneFlags::NEWUTS)
            && !current()
                .as_thread()
                .proc_data
                .has_effective_capability(CAP_SYS_ADMIN)
        {
            return Err(AxError::OperationNotPermitted);
        }

        if flags.contains(CloneFlags::INTO_CGROUP) {
            warn!("sys_clone3: CLONE_INTO_CGROUP not supported");
            return Err(AxError::OperationNotSupported);
        }

        Ok(())
    }

    pub(super) fn do_clone(self, uctx: &UserContext, api: CloneApi) -> AxResult<isize> {
        self.validate_for(api)?;

        let Self {
            flags,
            exit_signal,
            stack,
            tls,
            parent_tid,
            child_tid,
            pidfd,
        } = self;

        debug!(
            "do_clone <= flags: {:?}, exit_signal: {}, stack: {:#x}, tls: {:#x}",
            flags, exit_signal, stack, tls
        );
        let exit_signal = if exit_signal > 0 {
            Some(Signo::from_repr(exit_signal as u8).ok_or(AxError::InvalidInput)?)
        } else {
            None
        };

        let mut new_uctx = *uctx;
        if stack != 0 {
            new_uctx.set_sp(stack);
        }
        if flags.contains(CloneFlags::SETTLS) {
            new_uctx.set_tls(tls);
        }
        new_uctx.set_retval(0);

        let curr = current();
        let old_proc_data = &curr.as_thread().proc_data;
        if old_proc_data.exec_in_progress() {
            return Err(AxError::Interrupted);
        }

        // Thread-heavy user workloads such as libcbench can accumulate a large
        // batch of already-joined dead tasks on the local CPU between clone
        // bursts. Reclaim them before allocating and queueing the next child so
        // post-join fork/create phases do not inherit the previous burst's
        // stack and task-structure pressure.
        // Only reclaim when enough exited tasks have accumulated to avoid
        // reclaim-churn (one create, one reclaim, one create, ...).
        reclaim_exited_tasks_if_many(16);

        let mut child_sched_state = sched_state(&curr);
        if !flags.contains(CloneFlags::THREAD) && child_sched_state.reset_on_fork {
            child_sched_state.reset_on_fork = false;
            match child_sched_state.class {
                SchedClass::Fifo | SchedClass::RoundRobin => {
                    child_sched_state.class = SchedClass::Normal;
                    child_sched_state.nice = 0;
                    child_sched_state.rt_priority = 0;
                }
                SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => {
                    if child_sched_state.nice < 0 {
                        child_sched_state.nice = 0;
                    }
                    child_sched_state.rt_priority = 0;
                }
            }
        }

        let mut new_task = new_user_task(&curr.name(), new_uctx);
        #[cfg(target_arch = "loongarch64")]
        {
            copy_current_user_fpu_state_to(new_task.ctx_mut());
        }

        let tid = new_task.id().as_u64() as Pid;
        if flags.contains(CloneFlags::PARENT_SETTID) && parent_tid != 0 {
            (parent_tid as *mut Pid).vm_write(tid).ok();
        }

        let new_proc_data = if flags.contains(CloneFlags::THREAD) {
            new_task
                .ctx_mut()
                .set_page_table_root(old_proc_data.aspace().lock().page_table_root());
            old_proc_data.clone()
        } else {
            let proc = if flags.contains(CloneFlags::PARENT) {
                old_proc_data.proc.parent().ok_or(AxError::InvalidInput)?
            } else {
                old_proc_data.proc.clone()
            }
            .fork(tid, exit_signal.map(|signo| signo as u8));

            let aspace = if flags.contains(CloneFlags::VM) {
                old_proc_data.aspace()
            } else {
                let parent_aspace = old_proc_data.aspace();
                let aspace = {
                    let mut parent_guard = parent_aspace.lock();
                    parent_guard.try_clone()?
                };
                copy_from_kernel(&mut aspace.lock())?;
                aspace
            };
            new_task
                .ctx_mut()
                .set_page_table_root(aspace.lock().page_table_root());

            let signal_actions = if flags.contains(CloneFlags::SIGHAND) {
                old_proc_data.signal.actions.clone()
            } else if flags.contains(CloneFlags::CLEAR_SIGHAND) {
                Arc::new(SpinNoIrq::new(Default::default()))
            } else {
                Arc::new(SpinNoIrq::new(old_proc_data.signal.actions.lock().clone()))
            };

            let net_ns = if flags.contains(CloneFlags::NEWNET) {
                axnet::NetStack::new_loopback_only()
            } else {
                old_proc_data.net_ns.clone()
            };

            #[cfg(target_arch = "loongarch64")]
            let (child_exe_path, child_cmdline) = {
                // On LoongArch, process-shared exec metadata can be transiently
                // inconsistent during early shell-heavy bootstrap. Seed the child
                // from the scheduler-visible task name; execve installs the final
                // path/cmdline as soon as the child replaces its image.
                let fallback_name = curr.name();
                (fallback_name.clone(), Arc::new(alloc::vec![fallback_name]))
            };
            #[cfg(not(target_arch = "loongarch64"))]
            let (child_exe_path, child_cmdline) = (
                old_proc_data.exe_path.read().clone(),
                old_proc_data.cmdline.read().clone(),
            );

            let proc_data = ProcessData::new(
                proc,
                child_exe_path,
                child_cmdline,
                aspace,
                signal_actions,
                exit_signal,
                net_ns,
            );
            proc_data.set_umask(old_proc_data.umask());
            proc_data.set_credentials(old_proc_data.credentials());
            proc_data.set_capability_state(old_proc_data.capability_state());
            proc_data.set_supplementary_groups(old_proc_data.supplementary_groups());
            proc_data.set_heap_top(old_proc_data.get_heap_top());

            {
                let mut scope = proc_data.scope.write();
                if flags.contains(CloneFlags::FILES) {
                    FD_TABLE.scope_mut(&mut scope).clone_from(&FD_TABLE);
                } else {
                    FD_TABLE
                        .scope_mut(&mut scope)
                        .write()
                        .clone_from(&FD_TABLE.read());
                }

                if flags.contains(CloneFlags::FS) {
                    FS_CONTEXT.scope_mut(&mut scope).clone_from(&FS_CONTEXT);
                } else {
                    FS_CONTEXT
                        .scope_mut(&mut scope)
                        .lock()
                        .clone_from(&FS_CONTEXT.lock());
                }
            }

            proc_data
        };

        if flags.contains(CloneFlags::THREAD) {
            if !old_proc_data.try_add_thread(tid) {
                return Err(AxError::Interrupted);
            }
        } else {
            new_proc_data.proc.add_thread(tid);
            inherit_proc_shm(old_proc_data.proc.pid(), new_proc_data.proc.pid());
        }

        let rollback_clone_setup = || {
            if flags.contains(CloneFlags::THREAD) {
                old_proc_data.proc.remove_thread(tid);
            } else {
                new_proc_data.proc.abort_fork();
            }
        };
        let thr = Thread::new(tid, new_proc_data.clone());
        let child_aspace = new_proc_data.aspace();
        if flags.contains(CloneFlags::CHILD_SETTID) && child_tid != 0 {
            if let Err(err) = vm_write_in_aspace(&child_aspace, child_tid as *mut Pid, tid) {
                rollback_clone_setup();
                return Err(err);
            }
        }
        if flags.contains(CloneFlags::CHILD_CLEARTID) {
            thr.set_clear_child_tid(child_tid);
        }
        if flags.contains(CloneFlags::PIDFD) && pidfd != 0 {
            let pidfd_obj = if flags.contains(CloneFlags::THREAD) {
                PidFd::new_thread(&thr)
            } else {
                PidFd::new_process(&new_proc_data)
            };
            let fd = match pidfd_obj.add_to_fd_table(true) {
                Ok(fd) => fd,
                Err(err) => {
                    rollback_clone_setup();
                    return Err(err);
                }
            };
            if let Err(err) = (pidfd as *mut i32).vm_write(fd) {
                let _ = close_file_like(fd);
                rollback_clone_setup();
                return Err(err.into());
            }
        }
        *new_task.task_ext_mut() = Some(AxTaskExt::from_impl(thr));

        if flags.contains(CloneFlags::VFORK) {
            new_proc_data.begin_vfork(curr.id().as_u64() as Pid);
        }

        // Freshly forked user tasks need to run once promptly so they can
        // establish their first blocking point before the parent starts
        // /proc-based synchronization loops. Seeding them behind the parent's
        // fair vruntime leaves large fork storms stuck in runnable state long
        // enough for LTP children to hit their own futex timeouts first.
        let task = spawn_task_with_sched(new_task, child_sched_state);
        add_task_to_table(&task);
        // Give the freshly cloned task a chance to enter its first blocking
        // syscall before the parent spins in /proc-based synchronization
        // loops. Large LTP fork storms otherwise leave children runnable long
        // enough to trip their own timeouts on the single-CPU contest shape.
        yield_now();

        if flags.contains(CloneFlags::VFORK) {
            Self::wait_for_vfork(&new_proc_data);
        }

        Ok(tid as _)
    }
}

pub fn sys_clone(
    uctx: &UserContext,
    flags: u32,
    stack: usize,
    parent_tid: usize,
    #[cfg(any(target_arch = "x86_64", target_arch = "loongarch64"))] child_tid: usize,
    tls: usize,
    #[cfg(not(any(target_arch = "x86_64", target_arch = "loongarch64")))] child_tid: usize,
) -> AxResult<isize> {
    const FLAG_MASK: u32 = 0xff;
    let clone_flags = CloneFlags::from_bits_truncate((flags & !FLAG_MASK) as u64);
    let exit_signal = (flags & FLAG_MASK) as u64;

    if clone_flags.contains(CloneFlags::PIDFD | CloneFlags::PARENT_SETTID) {
        return Err(AxError::InvalidInput);
    }

    let args = CloneArgs {
        flags: clone_flags,
        exit_signal,
        stack,
        tls,
        parent_tid,
        child_tid,
        // In sys_clone, parent_tid is reused for pidfd when CLONE_PIDFD is set
        pidfd: if clone_flags.contains(CloneFlags::PIDFD) {
            parent_tid
        } else {
            0
        },
    };

    args.do_clone(uctx, CloneApi::Clone)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_fork(uctx: &UserContext) -> AxResult<isize> {
    sys_clone(uctx, SIGCHLD, 0, 0, 0, 0)
}

#[cfg(test)]
mod tests {
    use axerrno::AxError;
    use linux_raw_sys::general::{CLONE_DETACHED, CLONE_PIDFD};

    use super::{CloneApi, CloneArgs, CloneFlags};

    #[test]
    fn clone_validate_allows_detached_without_pidfd() {
        let args = CloneArgs {
            flags: CloneFlags::from_bits_retain(CLONE_DETACHED as u64),
            ..Default::default()
        };
        assert_eq!(args.validate_for(CloneApi::Clone), Ok(()));
    }

    #[test]
    fn clone_validate_rejects_detached_with_pidfd() {
        let args = CloneArgs {
            flags: CloneFlags::from_bits_retain((CLONE_DETACHED | CLONE_PIDFD) as u64),
            ..Default::default()
        };
        assert_eq!(
            args.validate_for(CloneApi::Clone),
            Err(AxError::InvalidInput)
        );
    }
}
