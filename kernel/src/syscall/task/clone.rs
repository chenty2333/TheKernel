use alloc::sync::Arc;
use core::sync::atomic::AtomicU16;

use axerrno::{AxError, AxResult, LinuxError};
use axhal::uspace::UserContext;
use axtask::{
    AxTaskExt, SchedClass, current, prepare_task_with_sched_from, publish_prepared_task,
    reclaim_exited_tasks, reserve_prepared_task, scheduler_state_snapshot,
    set_prepared_task_sched_reset_on_fork, yield_now,
};
use bitflags::bitflags;
use linux_raw_sys::general::*;
use thekernel_linux_process_adapter::{Pid, ProcessError};
use thekernel_linux_signal::{
    SignalInfo, Signo,
    api::{SharedSignalActions, SignalActions},
};

use crate::{
    file::{FdTable, FileDescription, PidFd, current_fd_table, reserve_fd, try_new_process_scope},
    keyring::{self, KeyTaskOwner},
    mm::{UserMemoryCapability, copy_from_kernel, map_usercopy_error},
    pseudofs::cgroup,
    readiness::block_on_poll_set_uninterruptible,
    syscall::prepare_proc_shm_inheritance,
    task::{
        AsThread, Cred, CredentialSlot, Dumpability, FsContextSlot, InitialProcessThreadAdmission,
        NetworkNamespace, PendingCredentialPublication, PendingThreadPublication,
        ProcessAccessState, ProcessData, ProcessInitialAdmission, ProcessThreadAdmission,
        SchedulerSeed, TaskParentChoice, Thread, fs_context_publication, get_process_data, linux_pid_from_task_id,
        lock_task_parent_publication, prepare_task_table_admission, process_domain,
        send_signal_thread_inner, set_task_user_address_space, try_new_user_task, try_tasks,
    },
};

fn should_yield_after_clone(flags: CloneFlags) -> bool {
    // VFORK enters the readiness-armed parent wait immediately after publication;
    // yielding first adds a scheduler window without advancing its handshake.
    !flags.contains(CloneFlags::VFORK)
        && flags.intersects(
            CloneFlags::THREAD
                | CloneFlags::VM
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

const IOPRIO_CLASS_SHIFT: u32 = 13;

fn inherited_ioprio(raw: u16) -> Option<u16> {
    let class = (raw as u32) >> IOPRIO_CLASS_SHIFT;
    (1..=3).contains(&class).then_some(raw)
}

/// Select the child's Linux I/O-priority context before any child becomes
/// visible. `CLONE_IO` shares the parent's context; ordinary fork/clone
/// copies only an explicitly selected class and otherwise starts in
/// `IOPRIO_CLASS_NONE`, matching Linux's `copy_io()` path.
fn clone_io_context_snapshot(
    flags: CloneFlags,
    parent_context: Option<Arc<AtomicU16>>,
) -> AxResult<Option<Arc<AtomicU16>>> {
    if flags.contains(CloneFlags::IO) {
        return Ok(parent_context);
    }

    let Some(parent_context) = parent_context else {
        // Linux's copy_io() leaves a task with no io_context unallocated.
        return Ok(None);
    };
    let raw = parent_context.load(core::sync::atomic::Ordering::Acquire);
    let Some(raw) = inherited_ioprio(raw) else {
        // A CLASS_NONE context carries no effective I/O priority. Linux's
        // ordinary fork path copies only a valid class and keeps the child
        // context unmaterialized, so a later CLONE_IO cannot accidentally
        // share this empty snapshot.
        return Ok(None);
    };
    Arc::try_new(AtomicU16::new(raw))
        .map(Some)
        .map_err(|_| AxError::NoMemory)
}

fn clone_io_context(
    flags: CloneFlags,
    parent: &crate::task::Thread,
) -> AxResult<Option<Arc<AtomicU16>>> {
    clone_io_context_snapshot(flags, parent.io_context())
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
    let uid = thread.real_uid();
    let cred = thread.current_cred();
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
            !thread.pending_exit() && thread.real_uid() == uid
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
            CloneFlags::NEWNS | CloneFlags::NEWIPC | CloneFlags::PTRACE | CloneFlags::UNTRACED,
        ) {
            return Err(AxError::OperationNotSupported);
        }

        if flags.contains(CloneFlags::THREAD)
            && !flags.contains(CloneFlags::VM | CloneFlags::SIGHAND)
        {
            return Err(AxError::InvalidInput);
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
        // Linux's CLONE_CLEAR_SIGHAND is an alternative to inheriting the
        // caller's sighand table.  Asking for both modes at once is invalid;
        // do not let the construction path choose one based on branch order.
        if flags.contains(CloneFlags::SIGHAND | CloneFlags::CLEAR_SIGHAND) {
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

    pub(super) fn do_clone(
        self,
        uctx: &UserContext,
        api: CloneApi,
        caller_memory: &UserMemoryCapability,
    ) -> AxResult<isize> {
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
            "do_clone <= flags: {flags:?}, exit_signal: {exit_signal}, stack: {stack:#x}, tls: \
             {tls:#x}"
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
        // Reserve the Linux rseq child snapshot before any fallible clone
        // construction. The guard cancels automatically on every error path
        // and is committed only with the final child publication steps.
        let rseq_fork = calling_thread.prepare_rseq_fork(flags.contains(CloneFlags::VM))?;
        let inherited_seccomp = calling_thread.seccomp_snapshot();
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
            if !calling_thread.fs_context().lock().root_dir().is_root() {
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
        let child_io_context = clone_io_context(flags, calling_thread)?;
        let child_ioport = calling_thread.ioport_snapshot();

        // Long fork/exit workloads can leave already-reaped tasks queued on
        // this CPU. Nudge its pinned recycler before allocating another child
        // so fork bursts provide an explicit progress edge for retained
        // task-stack and address-space pressure. This is request-only: it is
        // neither a local drain nor a destructor-completion barrier.
        reclaim_exited_tasks();
        check_rlimit_nproc(calling_thread)?;

        let parent_sched = scheduler_state_snapshot(&curr).map_err(|_| AxError::NoSuchProcess)?;
        let mut child_sched_state = parent_sched.state;
        let parent_reset_on_fork = parent_sched.reset_on_fork;
        // sched_fork applies RESET_ON_FORK before either a process or a
        // thread child becomes runnable. The child never inherits the flag.
        let child_reset_on_fork = false;
        if parent_reset_on_fork {
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
        let child_ids = child_cred.ids();
        let pending_key_fork = keyring::prepare_fork(
            KeyTaskOwner::new(calling_thread.kernel_tid(), old_proc_data.proc.pid()),
            KeyTaskOwner::new(
                tid,
                if flags.contains(CloneFlags::THREAD) {
                    old_proc_data.proc.pid()
                } else {
                    tid
                },
            ),
            flags.contains(CloneFlags::THREAD),
            child_ids.ruid,
            child_ids.rgid,
        )?;
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

        // Hold this through task publication: pivot_root snapshots task
        // fs_structs under the same gate, so a private child cannot publish
        // an old-root clone after that snapshot.
        let _fs_context_publication = fs_context_publication();
        let child_fs_context = if flags.contains(CloneFlags::FS) {
            calling_thread.fs_context_for_child()
        } else {
            let cloned = calling_thread.fs_context().lock().clone();
            FsContextSlot::new(Arc::try_new(axsync::Mutex::new(cloned)).map_err(|_| AxError::NoMemory)?)
        };
        let child_fd_table = if flags.contains(CloneFlags::FILES) {
            calling_thread.fd_table_for_child()
        } else {
            crate::task::FdTableSlot::new(Arc::try_new(current_fd_table().fork_copy()?).map_err(|_| AxError::NoMemory)?)
        };
        let (new_proc_data, thread_publication, pid_reservation) = if flags
            .contains(CloneFlags::THREAD)
        {
            set_task_user_address_space(
                new_task.ctx_mut(),
                parent_aspace.lock().address_space_token(),
            );
            let proc_data = old_proc_data.clone();
            // Threads have distinct Linux TIDs even though they share the
            // process PID. Reserve their namespace identity before core/task
            // admission so `gettid` never observes a global task ID.
            let tid_reservation = proc_data.pid_ns().reserve_process(tid)?;
            let thread_admission = proc_data.prepare_thread(tid)?;
            (
                proc_data,
                CloneThreadPublication::Live(thread_admission),
                Some(tid_reservation),
            )
        } else {
            let parent = fork_parent_data.as_ref().ok_or(AxError::BadState)?;
            let prepared_zombie_snapshot = ProcessData::try_prepare_zombie_snapshot()?;
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
            set_task_user_address_space(new_task.ctx_mut(), aspace.lock().address_space_token());

            let signal_actions = if flags.contains(CloneFlags::SIGHAND) {
                old_proc_data.signal.shared_actions().clone()
            } else if flags.contains(CloneFlags::CLEAR_SIGHAND) {
                SharedSignalActions::try_new(SignalActions::default())
                    .map_err(|_| AxError::NoMemory)?
            } else {
                old_proc_data
                    .signal
                    .shared_actions()
                    .try_snapshot()
                    .map_err(|_| AxError::NoMemory)?
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
            let (pid_ns, child_reaper_scope) = if flags.contains(CloneFlags::NEWPID) {
                let reaper_scope = process_domain()?
                    .try_new_reaper_scope()
                    .map_err(map_process_error)?;
                (
                    old_proc_data.pid_ns().try_fork_with_reaper_scope(
                        tid,
                        namespace_owner.clone(),
                        reaper_scope.clone(),
                    )?,
                    Some(reaper_scope),
                )
            } else {
                (old_proc_data.pid_ns(), None)
            };
            // Reserve this process's locally rendered PID before core process
            // publication. The reservation rolls back on every remaining
            // fallible construction path and is committed with the initial
            // thread/process identity below.
            let pid_reservation = pid_ns.reserve_process(tid)?;
            let domain = process_domain()?;
            let process_admission = if let Some(reaper_scope) = child_reaper_scope {
                ProcessInitialAdmission::ScopeInit(
                    domain
                        .prepare_fork_as_reaper_scope_init_with_identity(
                            &parent.proc,
                            &reaper_scope,
                            tid,
                            exit_signal.map(|signo| signo as u8),
                            pid_ns.clone(),
                        )
                        .map_err(map_process_error)?
                        .prepare_initial_thread(tid)
                        .map_err(map_process_error)?,
                )
            } else {
                let reaper_scope = pid_ns.reaper_scope().ok_or(AxError::BadState)?;
                ProcessInitialAdmission::Ordinary(
                    domain
                        .prepare_fork_in_reaper_scope_with_identity(
                            &parent.proc,
                            &reaper_scope,
                            tid,
                            exit_signal.map(|signo| signo as u8),
                            pid_ns.clone(),
                        )
                        .map_err(map_process_error)?
                        .prepare_initial_thread(tid)
                        .map_err(map_process_error)?,
                )
            };
            let proc = process_admission.process().clone();
            let uts_ns = if flags.contains(CloneFlags::NEWUTS) {
                old_proc_data.uts_ns().try_fork(namespace_owner)?
            } else {
                old_proc_data.uts_ns()
            };
            let time_ns = old_proc_data.time_ns_for_children();

            let (child_exe_path, child_cmdline) = (
                old_proc_data.try_exe_path()?,
                old_proc_data.cmdline.read().clone(),
            );

            // Construct the resources that replace scope-local defaults before
            // taking the child scope lock. The lock section below performs
            // only pointer swaps; displaced defaults are dropped afterwards.
            let scope = try_new_process_scope()?;
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
            let inherited_rlimits = old_proc_data.rlim.read().clone();
            let inherited_cpu_limit_active = inherited_rlimits[linux_raw_sys::general::RLIMIT_CPU]
                .current
                != linux_raw_sys::general::RLIM_INFINITY as i64 as u64;
            *proc_data.rlim.write() = inherited_rlimits;
            proc_data.process_rlimit_cpu_active.store(
                inherited_cpu_limit_active,
                core::sync::atomic::Ordering::Release,
            );
            proc_data.set_heap_layout(old_proc_data.heap_base());
            proc_data.set_heap_top(old_proc_data.get_heap_top());
            proc_data.try_inherit_mempolicy_from(old_proc_data)?;
            proc_data.inherit_timerslack_from(old_proc_data);
            let thread_admission = proc_data.prepare_initial_thread_admission(process_admission)?;
            (
                proc_data,
                CloneThreadPublication::Initial(thread_admission),
                Some(pid_reservation),
            )
        };
        let (thr, signal_registration) = Thread::try_new_with_io_context(
            tid,
            new_proc_data.clone(),
            child_credential,
            inherited_seccomp,
            child_io_context,
            child_fs_context,
            child_fd_table,
            calling_thread.personality(),
            SchedulerSeed {
                state: child_sched_state,
                reset_on_fork: child_reset_on_fork,
                // The child owns a fresh scheduler commit stream. Its first
                // prepared scheduler state is version zero.
                version: 0,
            },
        )?;
        // Linux inherits ioperm/iopl state for every fork/clone child. The
        // state stays private after this point: the bitmap Arc is copied on
        // either task's first ioperm mutation.
        thr.install_ioport_snapshot(child_ioport);
        if thread_publication.is_initial() {
            new_proc_data.bind_initial_group_leader_signal(tid, thr.signal.clone())?;
        }
        let task_parent_choice = if flags.intersects(CloneFlags::PARENT | CloneFlags::THREAD) {
            TaskParentChoice::Inherit(calling_thread.task_parent_node().clone())
        } else {
            TaskParentChoice::Caller(calling_thread.task_parent_node().clone())
        };
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
        set_prepared_task_sched_reset_on_fork(&task, child_reset_on_fork);
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
            caller_memory
                .write_value(pidfd as *mut i32, fd)
                .map_err(map_usercopy_error)?;
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
        let child_rseq = rseq_fork.commit();
        task.as_thread().install_rseq_state(child_rseq);
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
        let thread_completion = task_table_admission.commit_with_publication(|| {
            let completion = thread_publication.commit();
            pending_key_fork.commit();
            completion
        });
        // The child fs_struct is now visible through TASK_TABLE, so a pivot
        // can include it. Do not hold the publication gate across later user
        // copies, scheduler handoff, or vfork waiting.
        drop(_fs_context_publication);
        if let Some(pid_reservation) = pid_reservation {
            pid_reservation.commit();
        }
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

        // Read only after the reservation has committed with process/thread
        // publication. This is the caller namespace's view, including the
        // outer binding of a CLONE_NEWPID init child.
        let caller_visible_tid = old_proc_data.pid_ns().visible_pid(tid);

        // Linux performs PARENT_SETTID only after child construction succeeds,
        // and a copy fault does not cancel an otherwise successful clone. The
        // CHILD_SETTID address was attached to the private Thread above; the
        // child consumes it before first entering user mode, after publication.
        if flags.contains(CloneFlags::PARENT_SETTID) {
            let _ = caller_memory
                .write_value(parent_tid as *mut Pid, caller_visible_tid)
                .map_err(map_usercopy_error);
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
        // behind the parent in EEVDF, so the parent can finish post-fork setup
        // before a newly-created child runs unbounded user code.
        if should_yield_after_clone(flags) {
            yield_now();
        }

        if flags.contains(CloneFlags::VFORK) {
            Self::wait_for_vfork(&new_proc_data)?;
        }

        Ok(caller_visible_tid as _)
    }
}

pub fn sys_clone(
    caller_memory: UserMemoryCapability,
    uctx: &UserContext,
    flags: u32,
    stack: usize,
    parent_tid: usize,
    child_tid: usize,
    tls: usize,
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

    args.do_clone(uctx, CloneApi::Clone, &caller_memory)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_fork(caller_memory: UserMemoryCapability, uctx: &UserContext) -> AxResult<isize> {
    sys_clone(caller_memory, uctx, SIGCHLD, 0, 0, 0, 0)
}

/// Implements the x86_64 `vfork(2)` ABI through the common clone publication
/// path. Linux's vfork flags share only the address space while the child is
/// alive, and the parent remains blocked until the child's exec or final exit
/// releases the `CLONE_VFORK` publication gate.
#[cfg(target_arch = "x86_64")]
pub fn sys_vfork(caller_memory: UserMemoryCapability, uctx: &UserContext) -> AxResult<isize> {
    sys_clone(
        caller_memory,
        uctx,
        CLONE_VM | CLONE_VFORK | SIGCHLD,
        0,
        0,
        0,
        0,
    )
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::{
        cell::Cell,
        sync::atomic::{AtomicU16, Ordering},
    };

    use axerrno::AxError;
    use linux_raw_sys::general::{CLONE_DETACHED, CLONE_PIDFD};

    use super::{
        CloneApi, CloneArgs, CloneCredentialPublicationKind, CloneFlags, IOPRIO_CLASS_SHIFT,
        clone_credential_publication_kind, clone_io_context_snapshot, clone_namespace_owner,
        clone_process_access_state, inherited_ioprio, release_clone_lifecycle_then,
        should_yield_after_clone,
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
    fn vfork_wait_owns_the_parent_child_handoff() {
        assert!(!should_yield_after_clone(CloneFlags::VFORK));
        assert!(!should_yield_after_clone(
            CloneFlags::VFORK | CloneFlags::VM
        ));
        assert!(should_yield_after_clone(CloneFlags::VM));
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
    fn clone_validate_allows_thread_with_private_fs_context() {
        let args = CloneArgs {
            flags: CloneFlags::THREAD | CloneFlags::VM | CloneFlags::SIGHAND,
            ..Default::default()
        };
        assert_eq!(args.validate_for(CloneApi::Clone), Ok(()));
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
    fn clone_validate_accepts_clone_io() {
        let args = CloneArgs {
            flags: CloneFlags::IO,
            ..Default::default()
        };
        assert_eq!(args.validate_for(CloneApi::Clone), Ok(()));
    }

    #[test]
    fn ordinary_fork_inherits_only_explicit_ioprio_classes() {
        assert_eq!(inherited_ioprio(0), None);
        assert_eq!(
            inherited_ioprio((2 << IOPRIO_CLASS_SHIFT | 3) as u16),
            Some(2 << 13 | 3)
        );
        assert_eq!(inherited_ioprio((4 << IOPRIO_CLASS_SHIFT) as u16), None);
        assert!(
            clone_io_context_snapshot(CloneFlags::empty(), Some(Arc::new(AtomicU16::new(0x100))))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn clone_io_does_not_materialize_an_empty_parent_context() {
        assert!(
            clone_io_context_snapshot(CloneFlags::IO, None)
                .unwrap()
                .is_none()
        );
        assert!(
            clone_io_context_snapshot(CloneFlags::empty(), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn clone_io_shares_existing_context_but_fork_copies_it() {
        let parent = Arc::new(AtomicU16::new((2 << IOPRIO_CLASS_SHIFT) | 4));
        let shared = clone_io_context_snapshot(CloneFlags::IO, Some(parent.clone()))
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&parent, &shared));

        let copied = clone_io_context_snapshot(CloneFlags::empty(), Some(parent.clone()))
            .unwrap()
            .unwrap();
        assert!(!Arc::ptr_eq(&parent, &copied));
        assert_eq!(
            copied.load(Ordering::Acquire),
            parent.load(Ordering::Acquire)
        );
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
    fn clone_validate_rejects_shared_and_clear_sighand_together() {
        let args = CloneArgs {
            flags: CloneFlags::VM | CloneFlags::SIGHAND | CloneFlags::CLEAR_SIGHAND,
            ..Default::default()
        };
        assert_eq!(
            args.validate_for(CloneApi::Clone3),
            Err(AxError::InvalidInput)
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
