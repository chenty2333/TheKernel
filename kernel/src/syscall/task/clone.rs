use alloc::sync::Arc;

use axerrno::{AxError, AxResult, LinuxError};
use axfs::FS_CONTEXT;
use axhal::uspace::UserContext;
use axtask::{
    AxTaskExt, SchedClass, current, prepare_task_with_sched_from, publish_prepared_task,
    reclaim_exited_tasks, reserve_prepared_task, sched_state, yield_now,
};
use bitflags::bitflags;
use kspin::SpinNoIrq;
use linux_raw_sys::general::*;
use starry_process::{Pid, ProcessError};
use starry_signal::{SignalInfo, Signo};
use starry_vm::VmMutPtr;

#[cfg(target_arch = "loongarch64")]
use crate::task::copy_current_user_fpu_state_to;
use crate::{
    file::{FD_TABLE, FdTable, FileDescription, PidFd, reserve_fd, try_new_process_scope},
    mm::copy_from_kernel,
    pseudofs::cgroup,
    readiness::block_on_poll_set_uninterruptible,
    syscall::prepare_proc_shm_inheritance,
    task::{
        AsThread, Cred, CredentialSlot, Dumpability, InitialProcessThreadAdmission,
        NetworkNamespace, PendingCredentialPublication, PendingThreadPublication,
        ProcessAccessState, ProcessData, ProcessThreadAdmission, TaskParentChoice, Thread,
        get_process_data, linux_pid_from_task_id, lock_task_parent_publication,
        prepare_task_table_admission, process_domain, send_signal_thread_inner, try_new_user_task,
        try_tasks,
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

fn clone_namespace_owner(
    flags: CloneFlags,
    parent_cred: &Cred,
    child_cred: &Cred,
) -> AxResult<Arc<crate::task::UserNamespace>> {
    let owner = child_cred.user_ns().clone();
    if flags.intersects(
        CloneFlags::NEWCGROUP | CloneFlags::NEWUTS | CloneFlags::NEWPID | CloneFlags::NEWNET,
    ) && !crate::task::security::prepared_credential_namespace_capable(
        parent_cred,
        child_cred,
        &owner,
        CAP_SYS_ADMIN,
    ) {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(owner)
}

fn clone_process_access_state(
    flags: CloneFlags,
    parent_dumpability: Dumpability,
    parent: Arc<ProcessAccessState>,
) -> AxResult<Arc<ProcessAccessState>> {
    if flags.contains(CloneFlags::VM) {
        Ok(parent)
    } else {
        ProcessAccessState::try_new(parent_dumpability, parent.owner_user_ns().clone())
    }
}

enum CloneThreadPublication {
    Live(ProcessThreadAdmission),
    Initial(InitialProcessThreadAdmission),
}

impl CloneThreadPublication {
    const fn is_initial(&self) -> bool {
        matches!(self, Self::Initial(_))
    }

    fn commit(self) -> PendingThreadPublication {
        match self {
            Self::Live(admission) => admission.commit(),
            Self::Initial(admission) => {
                let (process, completion) = admission.commit();
                drop(process);
                completion
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CloneCredentialPublicationKind {
    Fork,
    UserNamespace,
}

const fn clone_credential_publication_kind(
    flags: CloneFlags,
) -> Option<CloneCredentialPublicationKind> {
    if flags.contains(CloneFlags::THREAD) {
        None
    } else if flags.contains(CloneFlags::NEWUSER) {
        Some(CloneCredentialPublicationKind::UserNamespace)
    } else {
        Some(CloneCredentialPublicationKind::Fork)
    }
}

/// Preserves the existing process-lifecycle protection through secondary
/// publication, group-exit handoff, and TID stores, then releases it before an
/// infallible security callback and runqueue publication.
fn release_clone_lifecycle_then<P>(process_lifecycle_lock: P, notify: impl FnOnce()) {
    drop(process_lifecycle_lock);
    notify();
}

fn map_process_error(error: ProcessError) -> AxError {
    match error {
        ProcessError::NoMemory | ProcessError::Capacity => AxError::NoMemory,
        ProcessError::AlreadyExists => AxError::AlreadyExists,
        ProcessError::NotPublished | ProcessError::NotLive | ProcessError::NotInitialized => {
            AxError::NoSuchProcess
        }
        ProcessError::WrongDomain => AxError::BadState,
        _ => AxError::BadState,
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

fn check_rlimit_nproc(thread: &Thread) -> AxResult<()> {
    let proc_data = &thread.proc_data;
    let cred = thread.current_cred();
    let uid = cred.ids().ruid;
    if cred.is_initial_root_ruid()
        || cred.has_effective_capability(CAP_SYS_RESOURCE)
        || cred.has_effective_capability(CAP_SYS_ADMIN)
    {
        return Ok(());
    }

    let limit = proc_data.rlim.read()[RLIMIT_NPROC].current;
    if limit == RLIM_INFINITY as i64 as u64 {
        return Ok(());
    }

    let count = try_tasks()?
        .into_iter()
        .filter(|task| {
            let thread = task.as_thread();
            !thread.pending_exit() && thread.current_cred().ids().ruid == uid
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
    fn wait_for_vfork(proc_data: &ProcessData) -> AxResult<()> {
        block_on_poll_set_uninterruptible(&proc_data.vfork_event, || {
            if !proc_data.vfork_in_progress() {
                Ok(())
            } else {
                Err(AxError::WouldBlock)
            }
        })
    }

    pub(super) fn validate_for(&self, api: CloneApi) -> AxResult<()> {
        let Self { flags, .. } = self;

        if flags.intersects(
            CloneFlags::NEWNS
                | CloneFlags::NEWIPC
                | CloneFlags::IO
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
        // NPTL thread creation includes CLONE_SYSVSEM. Threads already share
        // one ProcessData, and SEM_UNDO operations remain explicitly
        // unsupported, so no representable undo state can diverge. A
        // non-thread clone would require a separately shared undo-list object.
        if flags.contains(CloneFlags::SYSVSEM) && !flags.contains(CloneFlags::THREAD) {
            return Err(AxError::OperationNotSupported);
        }
        if flags.contains(CloneFlags::SIGHAND) && !flags.contains(CloneFlags::VM) {
            return Err(AxError::InvalidInput);
        }
        // Linux forbids creating a user namespace inside an existing thread
        // group or while sharing fs_struct with the parent. Either shape would
        // make one thread group's credentials span user namespaces or let the
        // new namespace retain a parent-owned root/cwd security context.
        if flags.contains(CloneFlags::NEWUSER)
            && flags.intersects(CloneFlags::THREAD | CloneFlags::PARENT | CloneFlags::FS)
        {
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
        if flags.contains(CloneFlags::THREAD)
            && flags.intersects(
                CloneFlags::NEWCGROUP
                    | CloneFlags::NEWUTS
                    | CloneFlags::NEWPID
                    | CloneFlags::NEWNET,
            )
        {
            return Err(AxError::InvalidInput);
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
        let calling_thread = curr.as_thread();
        let calling_tid = linux_pid_from_task_id(curr.id().as_u64())?;
        let old_proc_data = &calling_thread.proc_data;
        let credential_publication_kind = clone_credential_publication_kind(flags);
        // Every branch derives from one immutable calling-task snapshot.
        // Threads share its exact composite identity; a new process gets a
        // separately prepared module-state clone in its own outer credential.
        let (parent_cred, parent_dumpability, parent_aspace, parent_access_state) =
            old_proc_data.fork_image_credential_snapshot(calling_thread);
        let child_cred = if flags.contains(CloneFlags::NEWUSER) {
            // Match Linux current_chrooted(): creating a user namespace from
            // a restricted filesystem root must not create authority which
            // can be used to escape that root in later namespace slices.
            if !FS_CONTEXT.lock().root_dir().is_root() {
                return Err(AxError::OperationNotPermitted);
            }
            let ids = parent_cred.ids();
            let user_ns = parent_cred.user_ns().try_fork(
                ids.euid,
                ids.egid,
                parent_cred.has_effective_capability_in_own_user_ns(CAP_SETFCAP),
            )?;
            Cred::try_prepare_with_user_namespace(&parent_cred, user_ns)?
        } else if flags.contains(CloneFlags::THREAD) {
            parent_cred.clone()
        } else {
            Cred::try_prepare_clone_for_fork(&parent_cred)?
        };
        let namespace_owner = clone_namespace_owner(flags, &parent_cred, &child_cred)?;
        if old_proc_data.exec_in_progress() {
            return Err(AxError::Interrupted);
        }

        // Long fork/exit workloads can leave already-reaped tasks queued on
        // the local CPU. Free them before allocating another child so later
        // fork bursts do not inherit stale task-stack and address-space
        // pressure.
        reclaim_exited_tasks();
        check_rlimit_nproc(calling_thread)?;

        let mut child_sched_state = sched_state(&curr);
        let parent_reset_on_fork = calling_thread.sched_reset_on_fork();
        let child_reset_on_fork = flags.contains(CloneFlags::THREAD) && parent_reset_on_fork;
        if !flags.contains(CloneFlags::THREAD) && parent_reset_on_fork {
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

        let task_name = curr.try_name().map_err(|_| AxError::NoMemory)?;
        let mut new_task = try_new_user_task(task_name, new_uctx)?;
        #[cfg(target_arch = "loongarch64")]
        {
            copy_current_user_fpu_state_to(new_task.ctx_mut());
        }

        let tid = linux_pid_from_task_id(new_task.id().as_u64())?;
        let child_credential = CredentialSlot::try_new(child_cred.clone())?;

        let fork_parent_data = if flags.contains(CloneFlags::THREAD) {
            None
        } else if flags.contains(CloneFlags::PARENT) {
            let parent = old_proc_data.proc.parent().ok_or(AxError::InvalidInput)?;
            Some(get_process_data(parent.pid())?)
        } else {
            Some(old_proc_data.clone())
        };
        // CLONE_THREAD has no new process parent, but its reserved core
        // membership must still exclude the current process's final exit until
        // exact-parent, core, and TASK_TABLE publication complete. This keeps
        // the common path out of the core's defensive Busy result for an
        // unpublished membership.
        let fork_lifecycle = if flags.contains(CloneFlags::THREAD) {
            Some(old_proc_data.lock_process_lifecycle())
        } else {
            fork_parent_data
                .as_ref()
                .map(|parent| parent.lock_process_lifecycle())
        };

        let (new_proc_data, thread_publication) = if flags.contains(CloneFlags::THREAD) {
            new_task
                .ctx_mut()
                .set_page_table_root(parent_aspace.lock().page_table_root());
            let proc_data = old_proc_data.clone();
            let thread_admission = proc_data.prepare_thread(tid)?;
            (proc_data, CloneThreadPublication::Live(thread_admission))
        } else {
            let parent = fork_parent_data.as_ref().ok_or(AxError::BadState)?;
            let prepared_zombie_snapshot = ProcessData::try_prepare_zombie_snapshot()?;
            let process_admission = process_domain()?
                .prepare_fork(&parent.proc, tid, exit_signal.map(|signo| signo as u8))
                .map_err(map_process_error)?;
            let process_admission = process_admission
                .prepare_initial_thread(tid)
                .map_err(map_process_error)?;
            let proc = process_admission.process().clone();

            let aspace = if flags.contains(CloneFlags::VM) {
                parent_aspace
            } else {
                let aspace = {
                    let mut parent_guard = parent_aspace.lock();
                    parent_guard.try_clone()?
                };
                copy_from_kernel(&mut aspace.lock())?;
                aspace
            };
            let access_state =
                clone_process_access_state(flags, parent_dumpability, parent_access_state)?;
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
                NetworkNamespace::try_new_loopback_only(namespace_owner.clone())?
            } else {
                old_proc_data.net_ns.clone()
            };
            let cgroup_ns = if flags.contains(CloneFlags::NEWCGROUP) {
                old_proc_data
                    .cgroup_ns()
                    .try_fork(namespace_owner.clone())?
            } else {
                old_proc_data.cgroup_ns()
            };
            let pid_ns = if flags.contains(CloneFlags::NEWPID) {
                old_proc_data
                    .pid_ns()
                    .try_fork(tid, namespace_owner.clone())?
            } else {
                old_proc_data.pid_ns()
            };
            let uts_ns = if flags.contains(CloneFlags::NEWUTS) {
                old_proc_data.uts_ns().try_fork(namespace_owner)?
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
                prepared_zombie_snapshot,
                child_credential.clone(),
                child_exe_path,
                old_proc_data.retain_executable()?,
                child_cmdline,
                aspace,
                access_state,
                scope,
                exit_fd_table,
                signal_actions,
                exit_signal,
                net_ns,
                cgroup_ns,
                pid_ns,
                uts_ns,
                time_ns,
            )?;
            proc_data.set_umask(old_proc_data.umask());
            *proc_data.rlim.write() = old_proc_data.rlim.read().clone();
            proc_data.set_heap_top(old_proc_data.get_heap_top());
            proc_data.try_inherit_mempolicy_from(old_proc_data)?;
            proc_data.inherit_timerslack_from(old_proc_data);
            let thread_admission = proc_data.prepare_initial_thread(process_admission)?;
            (proc_data, CloneThreadPublication::Initial(thread_admission))
        };
        let (thr, signal_registration) =
            Thread::try_new(tid, new_proc_data.clone(), child_credential)?;
        if thread_publication.is_initial() {
            new_proc_data.bind_initial_group_leader_signal(tid, thr.signal.clone())?;
        }
        let task_parent_choice = if flags.intersects(CloneFlags::PARENT | CloneFlags::THREAD) {
            TaskParentChoice::Inherit(calling_thread.task_parent_node().clone())
        } else {
            TaskParentChoice::Caller(calling_thread.task_parent_node().clone())
        };
        thr.set_sched_reset_on_fork(child_reset_on_fork);
        if flags.contains(CloneFlags::CHILD_SETTID) {
            thr.set_child_tid_address(child_tid);
        }
        if flags.contains(CloneFlags::CHILD_CLEARTID) {
            thr.set_clear_child_tid(child_tid);
        }
        let mut pending_pidfd = if flags.contains(CloneFlags::PIDFD) {
            let reservation = reserve_fd(true)?;
            let pidfd_obj = if flags.contains(CloneFlags::THREAD) {
                PidFd::new_thread_unbound(&thr)
            } else {
                PidFd::new_process(&new_proc_data)
            };
            let pidfd_obj = Arc::try_new(pidfd_obj).map_err(|_| AxError::NoMemory)?;
            let thread_pidfd = flags
                .contains(CloneFlags::THREAD)
                .then(|| pidfd_obj.clone());
            let pidfd_file: Arc<dyn crate::file::FileLike> = pidfd_obj;
            let description = FileDescription::new(pidfd_file)?;
            Some((reservation.prepare_publication(description)?, thread_pidfd))
        } else {
            None
        };
        let cgroup_admission = if !flags.contains(CloneFlags::THREAD) {
            Some(if let Some(fd) = cgroup_fd {
                cgroup::prepare_fork_charge_into(fd, tid)?
            } else {
                cgroup::prepare_fork_charge(old_proc_data.proc.pid(), tid)?
            })
        } else {
            None
        };

        let shm_admission = (!flags.contains(CloneFlags::THREAD))
            .then(|| {
                prepare_proc_shm_inheritance(old_proc_data.proc.pid(), new_proc_data.proc.pid())
            })
            .transpose()?;

        // The task extension must exist before any process identity becomes
        // visible. Every admission token below rolls itself back on failure.
        *new_task.task_ext_mut() = Some(AxTaskExt::from_impl(thr));

        // Fallibly allocate/configure the scheduler object and reserve every
        // task/process/group/session lookup bucket before copying the pidfd
        // number or publishing it into a possibly shared files_struct.
        let task = prepare_task_with_sched_from(new_task, child_sched_state, &curr)?;
        if let Some((_, Some(pidfd))) = pending_pidfd.as_ref() {
            pidfd.bind_thread_task(&task)?;
        }
        let task_publication =
            reserve_prepared_task(task.clone()).map_err(|error| error.into_ax_error())?;
        let task_table_admission = prepare_task_table_admission(&task)?;
        let credential_publication = match credential_publication_kind {
            None => None,
            Some(CloneCredentialPublicationKind::Fork) => Some(
                PendingCredentialPublication::try_fork(&parent_cred, &child_cred, task.clone())?,
            ),
            Some(CloneCredentialPublicationKind::UserNamespace) => {
                Some(PendingCredentialPublication::try_user_namespace(
                    &parent_cred,
                    &child_cred,
                    task.clone(),
                )?)
            }
        };

        if let Some((publication, _)) = pending_pidfd.as_ref() {
            let fd = publication.fd();
            (pidfd as *mut i32).vm_write(fd)?;
        }

        if flags.contains(CloneFlags::VFORK) {
            new_proc_data.begin_vfork(calling_tid);
        }

        if let Some(publication) = credential_publication.as_ref() {
            publication.activate();
        }

        // Exact real-parent publication linearizes against parent-task exit.
        // The node, hard-limit charge, and intrusive links were all reserved
        // while the child was private, so this step is allocation-free and
        // infallible even for a root/no-parent inheritance relation. This is
        // deliberately after the last fallible admission and before the first
        // global identity publication.
        let task_parent_publication = lock_task_parent_publication();
        task.as_thread()
            .publish_task_parent(&task_parent_publication, task_parent_choice);

        // From this point onward every operation is an allocation-free,
        // infallible publication step. Publish the exact signal endpoint and
        // core process/thread identity with TASK_TABLE before any fd or
        // secondary global index.
        signal_registration
            .commit()
            .expect("private child signal registration was cancelled before publication");
        let thread_completion =
            task_table_admission.commit_with_publication(|| thread_publication.commit());
        drop(task_parent_publication);

        // TASK_TABLE is the primary runtime lookup. Cgroup and SysV SHM hidden
        // entries become visible only after it, so their readers can never
        // observe an unpublished child PID. PIDFD follows the same ordering:
        // once visible through a shared files_struct, signal/core/task lookup is
        // already complete. The prepared task remains off-runqueue throughout.
        if let Some(admission) = cgroup_admission {
            admission.commit();
        }
        if let Some(admission) = shm_admission {
            admission.commit();
        }
        if let Some((publication, _)) = pending_pidfd.take() {
            publication.commit();
        }

        // Exact handoff with group_exit: if its first scan ran before this TID
        // entered TASK_TABLE, the permanent gate is sampled after TASK_TABLE
        // publication and SIGKILL is queued directly to this prepared Thread
        // before the task can enter the runqueue. If group_exit linearizes after
        // this sample, its scan can already resolve this exact task-table entry.
        if thread_completion.must_terminate_for_group_exit() {
            send_signal_thread_inner(
                &task,
                task.as_thread(),
                SignalInfo::new_kernel(Signo::SIGKILL),
            );
        }

        // Linux performs PARENT_SETTID only after child construction succeeds,
        // and a copy fault does not cancel an otherwise successful clone. The
        // CHILD_SETTID address was attached to the private Thread above; the
        // child consumes it before first entering user mode, after publication.
        if flags.contains(CloneFlags::PARENT_SETTID) {
            let _ = (parent_tid as *mut Pid).vm_write(tid);
        }
        release_clone_lifecycle_then(fork_lifecycle, || {
            if let Some(publication) = credential_publication {
                publication.notify();
            }
        });
        let published_task = publish_prepared_task(task_publication);
        debug_assert!(Arc::ptr_eq(&published_task, &task));
        drop(published_task);
        thread_completion.finish();
        // Thread/vfork-style clones often rely on immediate child progress for
        // futex or parent/child tid handshakes. Plain fork children are seeded
        // behind the parent in CFS, so the parent can finish post-fork setup
        // before a newly-created child runs unbounded user code.
        if should_yield_after_clone(flags) {
            yield_now();
        }

        if flags.contains(CloneFlags::VFORK) {
            Self::wait_for_vfork(&new_proc_data)?;
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
    use alloc::sync::Arc;
    use core::cell::Cell;

    use axerrno::AxError;
    use linux_raw_sys::general::{CLONE_DETACHED, CLONE_PIDFD};

    use super::{
        CloneApi, CloneArgs, CloneCredentialPublicationKind, CloneFlags,
        clone_credential_publication_kind, clone_namespace_owner, clone_process_access_state,
        release_clone_lifecycle_then,
    };
    use crate::task::{Cred, Dumpability, Kgid, Kuid, ProcessAccessState, UserNamespace};

    struct DropProbe<'a> {
        state: &'a Cell<u8>,
        bit: u8,
    }

    impl Drop for DropProbe<'_> {
        fn drop(&mut self) {
            self.state.set(self.state.get() | self.bit);
        }
    }

    #[test]
    fn credential_publication_kind_excludes_shared_thread_clones() {
        assert_eq!(
            clone_credential_publication_kind(CloneFlags::empty()),
            Some(CloneCredentialPublicationKind::Fork)
        );
        assert_eq!(
            clone_credential_publication_kind(CloneFlags::NEWUSER),
            Some(CloneCredentialPublicationKind::UserNamespace)
        );
        assert_eq!(clone_credential_publication_kind(CloneFlags::THREAD), None);
    }

    #[test]
    fn credential_notification_runs_after_lifecycle_release() {
        let state = Cell::new(0_u8);
        release_clone_lifecycle_then(
            DropProbe {
                state: &state,
                bit: 1,
            },
            || {
                assert_eq!(state.get(), 1);
                state.set(1 << 1);
            },
        );
        // The caller performs runqueue publication only after this boundary.
        assert_eq!(state.get(), 1 << 1);
    }

    #[test]
    fn process_access_clone_vm_shares_and_fork_copies_image_owner() {
        let owner = UserNamespace::try_new_root().unwrap();
        let parent = ProcessAccessState::try_new(Dumpability::UserDumpable, owner.clone()).unwrap();
        let shared = clone_process_access_state(
            CloneFlags::VM | CloneFlags::NEWUSER,
            parent.dumpability(),
            parent.clone(),
        )
        .unwrap();
        let forked =
            clone_process_access_state(CloneFlags::empty(), parent.dumpability(), parent.clone())
                .unwrap();

        assert!(Arc::ptr_eq(&parent, &shared));
        assert!(!Arc::ptr_eq(&parent, &forked));
        assert!(Arc::ptr_eq(parent.owner_user_ns(), &owner));
        assert!(Arc::ptr_eq(shared.owner_user_ns(), &owner));
        assert!(Arc::ptr_eq(forked.owner_user_ns(), &owner));

        parent.set_dumpability_for_test(Dumpability::NotDumpable);
        assert_eq!(shared.dumpability(), Dumpability::NotDumpable);
        assert_eq!(forked.dumpability(), Dumpability::UserDumpable);
    }

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
                | CloneFlags::FS
                | CloneFlags::SYSVSEM,
            ..Default::default()
        };
        assert_eq!(args.validate_for(CloneApi::Clone), Ok(()));
    }

    #[test]
    fn clone_validate_rejects_process_sysvsem_without_shared_undo_state() {
        let args = CloneArgs {
            flags: CloneFlags::SYSVSEM,
            ..Default::default()
        };
        assert_eq!(
            args.validate_for(CloneApi::Clone),
            Err(AxError::OperationNotSupported)
        );
    }

    #[test]
    fn clone_validate_rejects_newuser_inside_thread_group() {
        let args = CloneArgs {
            flags: CloneFlags::NEWUSER
                | CloneFlags::THREAD
                | CloneFlags::VM
                | CloneFlags::SIGHAND
                | CloneFlags::FILES
                | CloneFlags::FS,
            ..Default::default()
        };
        assert_eq!(
            args.validate_for(CloneApi::Clone),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn clone_validate_rejects_newuser_with_shared_fs() {
        let args = CloneArgs {
            flags: CloneFlags::NEWUSER | CloneFlags::FS,
            ..Default::default()
        };
        assert_eq!(
            args.validate_for(CloneApi::Clone),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn clone_validate_rejects_newuser_with_clone_parent() {
        let args = CloneArgs {
            flags: CloneFlags::NEWUSER | CloneFlags::PARENT,
            ..Default::default()
        };
        assert_eq!(
            args.validate_for(CloneApi::Clone),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn namespace_owner_clone_rejects_thread_with_process_namespace_flags() {
        let thread_flags = CloneFlags::THREAD
            | CloneFlags::VM
            | CloneFlags::SIGHAND
            | CloneFlags::FILES
            | CloneFlags::FS;
        for namespace_flag in [
            CloneFlags::NEWCGROUP,
            CloneFlags::NEWUTS,
            CloneFlags::NEWPID,
            CloneFlags::NEWNET,
        ] {
            let args = CloneArgs {
                flags: thread_flags | namespace_flag,
                ..Default::default()
            };
            assert_eq!(
                args.validate_for(CloneApi::Clone),
                Err(AxError::InvalidInput)
            );
        }
    }

    #[test]
    fn namespace_owner_clone_uses_post_newuser_credential_snapshot() {
        let root = UserNamespace::try_new_root().unwrap();
        let parent_cred = Cred::try_root(root.clone()).unwrap();
        let child = root
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let child_cred = Cred::try_with_user_namespace(&parent_cred, child.clone()).unwrap();
        let flags = CloneFlags::NEWUSER
            | CloneFlags::NEWCGROUP
            | CloneFlags::NEWUTS
            | CloneFlags::NEWPID
            | CloneFlags::NEWNET;

        let owner = clone_namespace_owner(flags, &parent_cred, &child_cred).unwrap();
        assert!(Arc::ptr_eq(&owner, &child));
        assert!(!Arc::ptr_eq(&owner, &root));

        let inherited_owner =
            clone_namespace_owner(CloneFlags::NEWUTS, &parent_cred, &child_cred).unwrap();
        assert!(Arc::ptr_eq(&inherited_owner, &child));
    }
}
