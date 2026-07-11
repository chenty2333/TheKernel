use alloc::sync::Arc;
use core::{future::poll_fn, task::Poll};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::FS_CONTEXT;
use axhal::uspace::UserContext;
use axtask::{
    AxTaskExt, SchedClass, current, future::block_on, prepare_task_with_sched_from,
    publish_prepared_task, reclaim_exited_tasks, sched_state, yield_now,
};
use bitflags::bitflags;
use kspin::SpinNoIrq;
use linux_raw_sys::general::*;
use starry_process::{Pid, ProcessError};
use starry_signal::Signo;
use starry_vm::VmMutPtr;

#[cfg(target_arch = "loongarch64")]
use crate::task::copy_current_user_fpu_state_to;
use crate::{
    file::{FD_TABLE, FdTable, FileDescription, PidFd, reserve_fd, try_new_process_scope},
    mm::copy_from_kernel,
    pseudofs::cgroup,
    syscall::inherit_proc_shm,
    task::{
        AsThread, ProcessData, Thread, prepare_task_table_admission, try_new_user_task, try_tasks,
        vm_write_in_aspace,
    },
};

fn should_yield_after_clone(flags: CloneFlags) -> bool {
    flags.intersects(
        CloneFlags::THREAD
            | CloneFlags::VM
            | CloneFlags::VFORK
            | CloneFlags::PARENT_SETTID
            | CloneFlags::CHILD_SETTID
            | CloneFlags::CHILD_CLEARTID,
    )
}

/// Rolls back every process/thread registry publication made before a clone
/// becomes runnable. Process and thread membership have their own unpublished
/// admission tokens; this guard owns external cgroup/SHM side effects only.
struct CloneRollback {
    tid: Pid,
    inherited_shm: bool,
    charged_cgroup: bool,
    armed: bool,
}

impl CloneRollback {
    fn new(tid: Pid) -> Self {
        Self {
            tid,
            inherited_shm: false,
            charged_cgroup: false,
            armed: true,
        }
    }

    fn note_shm_inheritance(&mut self) {
        self.inherited_shm = true;
    }

    fn note_cgroup_charge(&mut self) {
        self.charged_cgroup = true;
    }

    fn commit(&mut self) {
        self.armed = false;
    }
}

impl Drop for CloneRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if self.charged_cgroup {
            cgroup::detach_process(self.tid);
        }
        if self.inherited_shm {
            crate::syscall::clear_proc_shm(self.tid);
        }
    }
}

fn map_process_error(error: ProcessError) -> AxError {
    match error {
        ProcessError::NoMemory | ProcessError::Capacity => AxError::NoMemory,
        ProcessError::AlreadyExists => AxError::AlreadyExists,
    }
}

bitflags! {
    /// Options for use with [`sys_clone`] and [`sys_clone3`].
    #[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
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

fn check_rlimit_nproc(proc_data: &ProcessData) -> AxResult<()> {
    if proc_data.uid() == 0
        || proc_data.has_effective_capability(CAP_SYS_RESOURCE)
        || proc_data.has_effective_capability(CAP_SYS_ADMIN)
    {
        return Ok(());
    }

    let limit = proc_data.rlim.read()[RLIMIT_NPROC].current;
    if limit == RLIM_INFINITY as i64 as u64 {
        return Ok(());
    }

    let uid = proc_data.uid();
    let count = try_tasks()?
        .into_iter()
        .filter(|task| {
            let thread = task.as_thread();
            !thread.pending_exit() && thread.proc_data.uid() == uid
        })
        .count() as u64;

    if count >= limit {
        return Err(AxError::from(LinuxError::EAGAIN));
    }

    Ok(())
}

/// Unified arguments for clone/clone3/fork/vfork.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct CloneArgs {
    pub flags: CloneFlags,
    pub exit_signal: u64,
    pub stack: usize,
    pub tls: usize,
    pub parent_tid: usize,
    pub child_tid: usize,
    pub pidfd: usize,
    pub cgroup_fd: Option<i32>,
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

        if flags.intersects(
            CloneFlags::NEWNS
                | CloneFlags::NEWIPC
                | CloneFlags::IO
                | CloneFlags::SYSVSEM
                | CloneFlags::PTRACE
                | CloneFlags::UNTRACED,
        ) {
            return Err(AxError::OperationNotSupported);
        }

        if flags.contains(CloneFlags::THREAD)
            && !flags.contains(CloneFlags::VM | CloneFlags::SIGHAND)
        {
            return Err(AxError::InvalidInput);
        }
        // `ProcessData::scope` is currently shared by every thread in a
        // thread group, so this kernel cannot yet represent Linux threads
        // that unshare either `files_struct` or `fs_struct`.  Reject that
        // shape honestly until those pointers move to a task-level resource
        // context; silently sharing them would let one thread mutate every
        // sibling's descriptor table or cwd/root state.
        if flags.contains(CloneFlags::THREAD) && !flags.contains(CloneFlags::FILES | CloneFlags::FS)
        {
            return Err(AxError::OperationNotSupported);
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

        if flags.contains(CloneFlags::NEWUTS)
            && !current()
                .as_thread()
                .proc_data
                .has_effective_capability(CAP_SYS_ADMIN)
        {
            return Err(AxError::OperationNotPermitted);
        }

        if flags.contains(CloneFlags::INTO_CGROUP) && api != CloneApi::Clone3 {
            return Err(AxError::InvalidInput);
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
            cgroup_fd,
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

        // Long fork/exit workloads can leave already-reaped tasks queued on
        // the local CPU. Free them before allocating another child so later
        // fork bursts do not inherit stale task-stack and address-space
        // pressure.
        reclaim_exited_tasks();
        check_rlimit_nproc(old_proc_data)?;

        let mut child_sched_state = sched_state(&curr);
        if !flags.contains(CloneFlags::THREAD) && child_sched_state.reset_on_fork {
            child_sched_state.reset_on_fork = false;
            match child_sched_state.class {
                SchedClass::Fifo | SchedClass::RoundRobin => {
                    child_sched_state.class = SchedClass::Normal;
                    child_sched_state.nice = 0;
                    child_sched_state.rt_priority = 0;
                    child_sched_state.dl_runtime = 0;
                    child_sched_state.dl_deadline = 0;
                    child_sched_state.dl_period = 0;
                }
                SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => {
                    if child_sched_state.nice < 0 {
                        child_sched_state.nice = 0;
                    }
                    child_sched_state.rt_priority = 0;
                    child_sched_state.dl_runtime = 0;
                    child_sched_state.dl_deadline = 0;
                    child_sched_state.dl_period = 0;
                }
                SchedClass::Deadline => {
                    child_sched_state.class = SchedClass::Normal;
                    child_sched_state.nice = 0;
                    child_sched_state.rt_priority = 0;
                    child_sched_state.dl_runtime = 0;
                    child_sched_state.dl_deadline = 0;
                    child_sched_state.dl_period = 0;
                }
            }
        }

        let task_name = curr.try_name().map_err(|_| AxError::NoMemory)?;
        let mut new_task = try_new_user_task(task_name, new_uctx)?;
        #[cfg(target_arch = "loongarch64")]
        {
            copy_current_user_fpu_state_to(new_task.ctx_mut());
        }

        let tid = new_task.id().as_u64() as Pid;

        let (new_proc_data, process_admission, thread_admission) = if flags
            .contains(CloneFlags::THREAD)
        {
            new_task
                .ctx_mut()
                .set_page_table_root(old_proc_data.aspace().lock().page_table_root());
            let proc_data = old_proc_data.clone();
            let thread_admission = proc_data.prepare_thread(tid)?;
            (proc_data, None, thread_admission)
        } else {
            let parent = if flags.contains(CloneFlags::PARENT) {
                old_proc_data.proc.parent().ok_or(AxError::InvalidInput)?
            } else {
                old_proc_data.proc.clone()
            };
            let process_admission = parent
                .prepare_fork(tid, exit_signal.map(|signo| signo as u8))
                .map_err(map_process_error)?;
            let proc = process_admission.process().clone();

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
                Arc::try_new(SpinNoIrq::new(Default::default())).map_err(|_| AxError::NoMemory)?
            } else {
                let actions = old_proc_data.signal.actions.lock().clone();
                Arc::try_new(SpinNoIrq::new(actions)).map_err(|_| AxError::NoMemory)?
            };

            let net_ns = if flags.contains(CloneFlags::NEWNET) {
                axnet::NetStack::try_new_loopback_only()?
            } else {
                old_proc_data.net_ns.clone()
            };
            let cgroup_ns = if flags.contains(CloneFlags::NEWCGROUP) {
                old_proc_data.cgroup_ns().try_fork()?
            } else {
                old_proc_data.cgroup_ns()
            };
            let pid_ns = if flags.contains(CloneFlags::NEWPID) {
                old_proc_data.pid_ns().try_fork(tid)?
            } else {
                old_proc_data.pid_ns()
            };
            let user_ns = if flags.contains(CloneFlags::NEWUSER) {
                old_proc_data.user_ns().try_fork(old_proc_data.euid())?
            } else {
                old_proc_data.user_ns()
            };
            let uts_ns = if flags.contains(CloneFlags::NEWUTS) {
                old_proc_data.uts_ns().try_fork()?
            } else {
                old_proc_data.uts_ns()
            };
            let time_ns = old_proc_data.time_ns_for_children();

            #[cfg(target_arch = "loongarch64")]
            let (child_exe_path, child_cmdline) = {
                // On LoongArch, process-shared exec metadata can be transiently
                // inconsistent during early shell-heavy bootstrap. Seed the child
                // from the scheduler-visible task name; execve installs the final
                // path/cmdline as soon as the child replaces its image.
                let fallback_name = curr.try_name().map_err(|_| AxError::NoMemory)?;
                let mut child_exe_path = alloc::string::String::new();
                child_exe_path
                    .try_reserve_exact(fallback_name.len())
                    .map_err(|_| AxError::NoMemory)?;
                child_exe_path.push_str(&fallback_name);
                let mut child_cmdline = alloc::vec::Vec::new();
                child_cmdline
                    .try_reserve_exact(1)
                    .map_err(|_| AxError::NoMemory)?;
                child_cmdline.push(fallback_name);
                let child_cmdline = Arc::try_new(child_cmdline).map_err(|_| AxError::NoMemory)?;
                (child_exe_path, child_cmdline)
            };
            #[cfg(not(target_arch = "loongarch64"))]
            let (child_exe_path, child_cmdline) = (
                old_proc_data.try_exe_path()?,
                old_proc_data.cmdline.read().clone(),
            );

            // Construct the resources that replace scope-local defaults before
            // taking the child scope lock. The lock section below performs
            // only pointer swaps; displaced defaults are dropped afterwards.
            let child_fd_table = if flags.contains(CloneFlags::FILES) {
                FD_TABLE.clone()
            } else {
                Arc::try_new(FD_TABLE.fork_copy()?).map_err(|_| AxError::NoMemory)?
            };
            let child_fs_context = if flags.contains(CloneFlags::FS) {
                FS_CONTEXT.clone()
            } else {
                let cloned = FS_CONTEXT.lock().clone();
                Arc::try_new(axsync::Mutex::new(cloned)).map_err(|_| AxError::NoMemory)?
            };
            let scope = try_new_process_scope(child_fd_table, child_fs_context)?;
            let exit_fd_table = Arc::try_new(FdTable::new()?).map_err(|_| AxError::NoMemory)?;

            let proc_data = ProcessData::try_new(
                proc,
                child_exe_path,
                old_proc_data.retain_executable()?,
                child_cmdline,
                aspace,
                scope,
                exit_fd_table,
                signal_actions,
                exit_signal,
                net_ns,
                cgroup_ns,
                pid_ns,
                user_ns,
                uts_ns,
                time_ns,
            )?;
            proc_data.set_umask(old_proc_data.umask());
            proc_data.set_credentials(old_proc_data.credentials());
            proc_data.set_capability_state(old_proc_data.capability_state());
            proc_data.set_supplementary_groups(old_proc_data.try_supplementary_groups()?);
            proc_data.set_heap_top(old_proc_data.get_heap_top());
            proc_data.try_inherit_mempolicy_from(old_proc_data)?;
            proc_data.inherit_timerslack_from(old_proc_data);
            if old_proc_data.no_new_privs() {
                proc_data.set_no_new_privs();
            }

            let thread_admission = proc_data.prepare_thread(tid)?;
            (proc_data, Some(process_admission), thread_admission)
        };
        let mut rollback = CloneRollback::new(tid);
        let (thr, signal_registration) = Thread::try_new(tid, new_proc_data.clone())?;
        let child_aspace = new_proc_data.aspace();
        if flags.contains(CloneFlags::CHILD_CLEARTID) {
            thr.set_clear_child_tid(child_tid);
        }
        let mut pending_pidfd = if flags.contains(CloneFlags::PIDFD) {
            let reservation = reserve_fd(true)?;
            let pidfd_obj = if flags.contains(CloneFlags::THREAD) {
                PidFd::new_thread(&thr)
            } else {
                PidFd::new_process(&new_proc_data)
            };
            let pidfd_file: Arc<dyn crate::file::FileLike> =
                Arc::try_new(pidfd_obj).map_err(|_| AxError::NoMemory)?;
            let description = FileDescription::new(pidfd_file)?;
            Some((reservation, description))
        } else {
            None
        };
        if !flags.contains(CloneFlags::THREAD) {
            let charge_result = if let Some(fd) = cgroup_fd {
                cgroup::try_charge_fork_into(fd, tid)
            } else {
                cgroup::try_charge_fork(old_proc_data.proc.pid(), tid)
            };
            charge_result?;
            rollback.note_cgroup_charge();
        }

        if !flags.contains(CloneFlags::THREAD) {
            inherit_proc_shm(old_proc_data.proc.pid(), new_proc_data.proc.pid())?;
            rollback.note_shm_inheritance();
        }

        // The task extension must exist before the pidfd becomes visible. If
        // publication fails, dropping `new_task` and the armed rollback token
        // tears down the complete unpublished child.
        *new_task.task_ext_mut() = Some(AxTaskExt::from_impl(thr));

        // Fallibly allocate/configure the scheduler object and reserve every
        // task/process/group/session lookup bucket before copying the pidfd
        // number or publishing it into a possibly shared files_struct.
        let task = prepare_task_with_sched_from(new_task, child_sched_state, &curr)?;
        let task_table_admission = prepare_task_table_admission(&task)?;

        if let Some((reservation, _)) = pending_pidfd.as_ref() {
            let fd = reservation.fd();
            (pidfd as *mut i32).vm_write(fd)?;
        }

        if flags.contains(CloneFlags::VFORK) {
            new_proc_data.begin_vfork(curr.id().as_u64() as Pid);
        }

        if let Some((reservation, description)) = pending_pidfd.take() {
            reservation.publish(description)?;
        }

        // From this point onward every operation is an allocation-free,
        // infallible publication step. Disarm before lookup/runqueue commit,
        // where another CPU could otherwise observe the child concurrently
        // with rollback.
        signal_registration.commit();
        thread_admission.commit();
        if let Some(process_admission) = process_admission {
            process_admission.commit();
        }
        rollback.commit();
        task_table_admission.commit();

        // Linux performs both TID stores only after child construction has
        // succeeded, and neither copy fault cancels an otherwise successful
        // clone. Keep them after every fallible admission so a failed clone
        // cannot leave a TID for a child that was never published. The child
        // is not runnable until `publish_prepared_task()` below.
        if flags.contains(CloneFlags::PARENT_SETTID) {
            let _ = (parent_tid as *mut Pid).vm_write(tid);
        }
        if flags.contains(CloneFlags::CHILD_SETTID) && child_tid != 0 {
            let _ = vm_write_in_aspace(&child_aspace, child_tid as *mut Pid, tid);
        }
        publish_prepared_task(task);
        // Thread/vfork-style clones often rely on immediate child progress for
        // futex or parent/child tid handshakes. Plain fork children are seeded
        // behind the parent in CFS, so the parent can finish post-fork setup
        // before a newly-created child runs unbounded user code.
        if should_yield_after_clone(flags) {
            yield_now();
        }

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
    let clone_flags =
        CloneFlags::from_bits((flags & !FLAG_MASK) as u64).ok_or(AxError::InvalidInput)?;
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
        cgroup_fd: None,
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

    #[test]
    fn clone_validate_rejects_thread_with_process_scoped_files() {
        let args = CloneArgs {
            flags: CloneFlags::THREAD | CloneFlags::VM | CloneFlags::SIGHAND | CloneFlags::FS,
            ..Default::default()
        };
        assert_eq!(
            args.validate_for(CloneApi::Clone),
            Err(AxError::OperationNotSupported)
        );
    }

    #[test]
    fn clone_validate_accepts_representable_thread_resources() {
        let args = CloneArgs {
            flags: CloneFlags::THREAD
                | CloneFlags::VM
                | CloneFlags::SIGHAND
                | CloneFlags::FILES
                | CloneFlags::FS,
            ..Default::default()
        };
        assert_eq!(args.validate_for(CloneApi::Clone), Ok(()));
    }
}
