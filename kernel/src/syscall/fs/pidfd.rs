use alloc::sync::Arc;

use axerrno::{AxError, AxResult, LinuxError};
use axtask::{AxTaskRef, current};
use linux_raw_sys::general::{O_EXCL, O_NONBLOCK, O_RDWR, SI_TKILL};
use tk_linux_process::{Pid, PidfdPlan};
use tk_linux_signal::{SignalInfo, api::ThreadSignalManager};
use tk_linux_usercopy::{UserMemory, UserMemoryContext, VmPtr};

use crate::{
    file::{Directory, FileHandle, FileLike, PidFd, add_file_description, add_file_like_with_flags},
    mm::map_usercopy_error,
    pseudofs::{ProcDirProcess, process_data_from_proc_dir},
    syscall::signal::{
        make_siginfo, parse_signo, process_group_targets, queued_signal_required,
        send_signal_to_authorized_thread, send_user_signal_to_process_group_targets,
        signal_operation,
    },
    task::{
        AsThread, Cred, Process, ProcessData, ProcessImageAccessSnapshot, PtraceAccessMode,
        SignalDeliveryScope, SignalSecuritySource, SignalTargetKind,
        check_current_pinned_process_identity_signal_access,
        check_current_pinned_process_signal_access, check_current_pinned_thread_signal_access,
        check_current_ptrace_image_snapshot, check_current_zombie_signal_access,
        generate_signal_for_exited_leader, get_process_data, get_visible_task, process_domain,
        send_queued_signal_to_process_data_with_credential,
        send_signal_to_process_data_with_credential,
    },
};

fn process_data_from_proc_dir_fd(fd: i32) -> AxResult<alloc::sync::Arc<crate::task::ProcessData>> {
    let dir = Directory::from_fd(fd).map_err(|err| {
        if matches!(err, AxError::InvalidInput | AxError::NotADirectory) {
            AxError::BadFileDescriptor
        } else {
            err
        }
    })?;
    match process_data_from_proc_dir(dir.inner()) {
        ProcDirProcess::Live(proc_data) => Ok(proc_data),
        ProcDirProcess::Stale => Err(AxError::NoSuchProcess),
        ProcDirProcess::NotProcDir => Err(AxError::BadFileDescriptor),
    }
}

enum ResolvedPidFdSignalTarget {
    Process {
        process: Arc<ProcessData>,
        identity: Arc<Process>,
        credential: Arc<Cred>,
        leader_signal: Arc<ThreadSignalManager>,
    },
    Zombie {
        process: Arc<Process>,
        credential: Arc<Cred>,
    },
    ExitedLeader {
        pidfd: FileHandle<PidFd>,
        process: Arc<Process>,
        runtime: Option<Arc<ProcessData>>,
        leader_signal: Option<Arc<ThreadSignalManager>>,
        credential: Arc<Cred>,
    },
    Thread {
        pidfd: FileHandle<PidFd>,
        task: AxTaskRef,
        credential: Arc<Cred>,
        visible_tid: u32,
    },
    /// `pidfd == PIDFD_SELF_THREAD`: the calling thread, addressed without a
    /// descriptor.  There is no pidfd to re-validate against, so the delivery
    /// loop never retries this target.
    SelfThread {
        task: AxTaskRef,
        credential: Arc<Cred>,
        visible_tid: u32,
    },
}

impl ResolvedPidFdSignalTarget {
    fn visible_id(&self) -> u32 {
        match self {
            Self::Process { identity, .. }
            | Self::Zombie {
                process: identity, ..
            }
            | Self::ExitedLeader {
                process: identity, ..
            } => identity.pid(),
            Self::Thread { visible_tid, .. } | Self::SelfThread { visible_tid, .. } => {
                *visible_tid
            }
        }
    }

    fn delivery_scope(&self) -> SignalDeliveryScope {
        match self {
            Self::Process { .. } | Self::Zombie { .. } => SignalDeliveryScope::ThreadGroup,
            Self::ExitedLeader { .. } | Self::Thread { .. } | Self::SelfThread { .. } => {
                SignalDeliveryScope::Thread
            }
        }
    }

}

fn exact_identity_matches<T>(expected: &Arc<T>, published: Option<&Arc<T>>) -> bool {
    published.is_some_and(|published| Arc::ptr_eq(expected, published))
}

const fn thread_pidfd_candidate_matches(
    stable_tid: u32,
    process_pid: u32,
    candidate_tid: u32,
    same_process: bool,
    same_original_task: bool,
) -> bool {
    same_process && candidate_tid == stable_tid && (same_original_task || stable_tid == process_pid)
}

fn exact_process_is_published(process: &Arc<Process>) -> AxResult<bool> {
    let published = process_domain()?.registry().get(process.pid());
    Ok(exact_identity_matches(process, published.as_ref()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PidFdProcessPostHook {
    Complete,
    Deliver,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PidFdLeaderPostHook {
    CompleteProbe,
    Deliver,
    RetryHandoff,
}

const fn pidfd_leader_post_hook(has_signal: bool, leader_matches: bool) -> PidFdLeaderPostHook {
    if !has_signal {
        PidFdLeaderPostHook::CompleteProbe
    } else if leader_matches {
        PidFdLeaderPostHook::Deliver
    } else {
        PidFdLeaderPostHook::RetryHandoff
    }
}

const fn pidfd_process_post_hook(is_zombie: bool, has_signal: bool) -> PidFdProcessPostHook {
    if is_zombie || !has_signal {
        PidFdProcessPostHook::Complete
    } else {
        PidFdProcessPostHook::Deliver
    }
}

fn reduce_pidfd_process_delivery_result(
    result: AxResult<()>,
    exact_zombie_is_published: bool,
) -> AxResult<()> {
    match result {
        // Linux retries a process-directed send when exit races signal
        // publication. An unreaped zombie still names the same pid identity,
        // so a post-authorization ESRCH completes successfully only while
        // that exact zombie remains published. Thread pidfds deliberately do
        // not use this reducer.
        Err(AxError::NoSuchProcess) if exact_zombie_is_published => Ok(()),
        result => result,
    }
}

fn complete_pidfd_process_delivery(identity: &Arc<Process>, result: AxResult<()>) -> AxResult<()> {
    let exact_zombie_is_published =
        if matches!(&result, Err(AxError::NoSuchProcess)) && identity.is_zombie() {
            exact_process_is_published(identity)?
        } else {
            false
        };
    reduce_pidfd_process_delivery_result(result, exact_zombie_is_published)
}

const PIDFD_THREAD_SIGNAL_RETRY_LIMIT: usize = 4;

fn should_retry_pidfd_thread_delivery(
    result: &AxResult<()>,
    stable_identity_is_leader: bool,
    retries: usize,
) -> bool {
    stable_identity_is_leader
        && retries < PIDFD_THREAD_SIGNAL_RETRY_LIMIT
        && matches!(result, Err(AxError::NoSuchProcess))
}

fn resolve_exited_leader_pidfd_signal_target(
    pidfd: &FileHandle<PidFd>,
) -> AxResult<ResolvedPidFdSignalTarget> {
    let identity = pidfd
        .signal_exited_leader_process()?
        .ok_or(AxError::NoSuchProcess)?;
    if !exact_process_is_published(&identity)? {
        return Err(AxError::NoSuchProcess);
    }
    if let Ok(task) = get_visible_task(identity.pid()) {
        let thread = task.try_as_thread().ok_or(AxError::NoSuchProcess)?;
        if !thread_pidfd_candidate_matches(
            identity.pid(),
            identity.pid(),
            thread.tid(),
            Arc::ptr_eq(&thread.proc_data.proc, &identity),
            false,
        ) {
            return Err(AxError::NoSuchProcess);
        }
        let credential = thread.current_cred();
        let revalidated = get_visible_task(identity.pid())?;
        if !Arc::ptr_eq(&task, &revalidated) || !exact_process_is_published(&identity)? {
            return Err(AxError::NoSuchProcess);
        }
        return Ok(ResolvedPidFdSignalTarget::Thread {
            pidfd: pidfd.clone(),
            task,
            credential,
            visible_tid: identity.pid(),
        });
    }
    let (runtime, leader_signal, credential) = if identity.is_zombie() {
        (
            None,
            None,
            identity
                .zombie_payload()
                .map(|snapshot| snapshot.credential.clone())
                .ok_or(AxError::NoSuchProcess)?,
        )
    } else {
        match get_process_data(identity.pid()) {
            Ok(process) if Arc::ptr_eq(&process.proc, &identity) => {
                let (credential, leader_signal) = process.group_leader_signal_identity()?;
                (Some(process), Some(leader_signal), credential)
            }
            Ok(_) => return Err(AxError::NoSuchProcess),
            Err(AxError::NoSuchProcess) if identity.is_zombie() => (
                None,
                None,
                identity
                    .zombie_payload()
                    .map(|snapshot| snapshot.credential.clone())
                    .ok_or(AxError::NoSuchProcess)?,
            ),
            Err(error) => return Err(error),
        }
    };
    if !exact_process_is_published(&identity)? {
        return Err(AxError::NoSuchProcess);
    }
    Ok(ResolvedPidFdSignalTarget::ExitedLeader {
        pidfd: pidfd.clone(),
        process: identity,
        runtime,
        leader_signal,
        credential,
    })
}

fn resolve_process_pidfd_signal_target(pidfd: &PidFd) -> AxResult<ResolvedPidFdSignalTarget> {
    let identity = pidfd.process()?;
    if !exact_process_is_published(&identity)? {
        return Err(AxError::NoSuchProcess);
    }
    if identity.is_zombie() {
        let snapshot = identity.zombie_payload().ok_or(AxError::NoSuchProcess)?;
        let credential = snapshot.credential.clone();
        if !exact_process_is_published(&identity)? {
            return Err(AxError::NoSuchProcess);
        }
        return Ok(ResolvedPidFdSignalTarget::Zombie {
            process: identity,
            credential,
        });
    }
    match pidfd.process_data() {
        Ok(process) => {
            if !Arc::ptr_eq(&process.proc, &identity) {
                return Err(AxError::NoSuchProcess);
            }
            let (credential, leader_signal) = process.group_leader_signal_identity()?;
            if !exact_process_is_published(&identity)? {
                return Err(AxError::NoSuchProcess);
            }
            Ok(ResolvedPidFdSignalTarget::Process {
                process,
                identity,
                credential,
                leader_signal,
            })
        }
        Err(AxError::NoSuchProcess) if identity.is_zombie() => {
            let snapshot = identity.zombie_payload().ok_or(AxError::NoSuchProcess)?;
            let credential = snapshot.credential.clone();
            if !exact_process_is_published(&identity)? {
                return Err(AxError::NoSuchProcess);
            }
            Ok(ResolvedPidFdSignalTarget::Zombie {
                process: identity,
                credential,
            })
        }
        Err(error) => Err(error),
    }
}

fn refresh_exact_process_pidfd_signal_target(
    identity: Arc<Process>,
    runtime: Arc<ProcessData>,
) -> AxResult<ResolvedPidFdSignalTarget> {
    if !exact_process_is_published(&identity)? {
        return Err(AxError::NoSuchProcess);
    }
    if identity.is_zombie() {
        let snapshot = identity.zombie_payload().ok_or(AxError::NoSuchProcess)?;
        return Ok(ResolvedPidFdSignalTarget::Zombie {
            process: identity,
            credential: snapshot.credential.clone(),
        });
    }
    if !Arc::ptr_eq(&runtime.proc, &identity) {
        return Err(AxError::NoSuchProcess);
    }
    let published_runtime = match get_process_data(identity.pid()) {
        Ok(runtime) => runtime,
        Err(AxError::NoSuchProcess) if identity.is_zombie() => {
            let snapshot = identity.zombie_payload().ok_or(AxError::NoSuchProcess)?;
            return Ok(ResolvedPidFdSignalTarget::Zombie {
                process: identity,
                credential: snapshot.credential.clone(),
            });
        }
        Err(error) => return Err(error),
    };
    if !Arc::ptr_eq(&published_runtime, &runtime) {
        return Err(AxError::NoSuchProcess);
    }
    let (credential, leader_signal) = runtime.group_leader_signal_identity()?;
    if identity.is_zombie() {
        let snapshot = identity.zombie_payload().ok_or(AxError::NoSuchProcess)?;
        return Ok(ResolvedPidFdSignalTarget::Zombie {
            process: identity,
            credential: snapshot.credential.clone(),
        });
    }
    Ok(ResolvedPidFdSignalTarget::Process {
        process: runtime,
        identity,
        credential,
        leader_signal,
    })
}

fn signal_target_from_pidfd(pidfd: FileHandle<PidFd>) -> AxResult<ResolvedPidFdSignalTarget> {
    match pidfd.signal_thread_task() {
        Ok(Some(task)) => {
            let visible_tid = pidfd
                .signal_thread_tid()
                .ok_or(AxError::OperationNotPermitted)?;
            let credential = pidfd.credential_snapshot()?;
            let revalidated = pidfd.signal_thread_task()?.ok_or(AxError::NoSuchProcess)?;
            if !Arc::ptr_eq(&task, &revalidated) {
                return Err(AxError::NoSuchProcess);
            }
            Ok(ResolvedPidFdSignalTarget::Thread {
                pidfd,
                task,
                credential,
                visible_tid,
            })
        }
        Ok(None) => resolve_process_pidfd_signal_target(&pidfd),
        Err(AxError::NoSuchProcess) => resolve_exited_leader_pidfd_signal_target(&pidfd),
        Err(error) => Err(error),
    }
}

fn signal_target_from_fd(fd: i32) -> AxResult<ResolvedPidFdSignalTarget> {
    match PidFd::from_fd(fd) {
        Ok(pidfd) => signal_target_from_pidfd(pidfd),
        Err(AxError::InvalidInput) => {
            let process = process_data_from_proc_dir_fd(fd)?;
            let (credential, leader_signal) = process.group_leader_signal_identity()?;
            Ok(ResolvedPidFdSignalTarget::Process {
                identity: process.proc.clone(),
                process,
                credential,
                leader_signal,
            })
        }
        Err(err) => Err(err),
    }
}

/// Resolves `pidfd == PIDFD_SELF_THREAD`, which Linux short-circuits to
/// `get_task_pid(current, PIDTYPE_PID)` before any descriptor lookup.
fn self_thread_signal_target() -> AxResult<ResolvedPidFdSignalTarget> {
    let task = current().clone();
    let thread = task.as_thread();
    Ok(ResolvedPidFdSignalTarget::SelfThread {
        credential: thread.current_cred(),
        visible_tid: thread.tid(),
        task,
    })
}

/// Resolves `pidfd == PIDFD_SELF_THREAD_GROUP`, which Linux short-circuits to
/// `get_task_pid(current, PIDTYPE_TGID)` — the calling process's group leader.
fn self_thread_group_signal_target() -> AxResult<ResolvedPidFdSignalTarget> {
    let process = current().as_thread().proc_data.clone();
    let (credential, leader_signal) = process.group_leader_signal_identity()?;
    Ok(ResolvedPidFdSignalTarget::Process {
        identity: process.proc.clone(),
        process,
        credential,
        leader_signal,
    })
}

fn retry_pidfd_thread_signal_target(
    pidfd: FileHandle<PidFd>,
    stable_tid: u32,
) -> AxResult<ResolvedPidFdSignalTarget> {
    let next = signal_target_from_pidfd(pidfd)?;
    if next.delivery_scope() != SignalDeliveryScope::Thread || next.visible_id() != stable_tid {
        return Err(AxError::NoSuchProcess);
    }
    Ok(next)
}

fn check_pidfd_getfd_permission(
    pidfd: &PidFd,
    target: &ProcessData,
) -> AxResult<ProcessImageAccessSnapshot> {
    if target.exec_in_progress() {
        return Err(AxError::OperationNotPermitted);
    }
    let target_image = pidfd.image_access_snapshot()?;
    check_current_ptrace_image_snapshot(target, &target_image, PtraceAccessMode::AttachReal)?;
    // kernel/pid.c:889-893: the ptrace verdict precedes the exit verdict, and
    // an exiting target is ESRCH because its descriptor table is no longer a
    // valid source (kernel/pid.c:905-907 maps the freed files_struct to ESRCH
    // too).
    if target.proc.is_zombie() {
        return Err(AxError::NoSuchProcess);
    }
    Ok(target_image)
}

pub fn sys_pidfd_open(pid: i32, flags: u32) -> AxResult<isize> {
    debug!("sys_pidfd_open <= pid: {pid}, flags: {flags}");

    if pid <= 0 {
        return Err(AxError::InvalidInput);
    }
    let plan = PidfdPlan::open(pid as u32, flags).map_err(|_| AxError::InvalidInput)?;

    let target = current()
        .as_thread()
        .pid_ns()
        .resolve_visible_pid(plan.target)
        .ok_or(AxError::NoSuchProcess)?;
    let fd = if plan.thread {
        let task = get_visible_task(target)?;
        PidFd::new_thread(&task)?
    } else {
        match get_process_data(target) {
            Ok(process) => PidFd::new_process(&process),
            // kernel/fork.c:1911-1919: a reaped pid is ESRCH, but a live pid
            // that is not a thread-group leader has no process pidfd and is
            // reported as ENOENT unless PIDFD_THREAD was requested.
            Err(error) => {
                if error == AxError::NoSuchProcess && get_visible_task(target).is_ok() {
                    return Err(AxError::NotFound);
                }
                return Err(error);
            }
        }
    };
    if plan.nonblocking {
        fd.set_nonblocking(true)?;
    }

    // fs/pidfs.c:932-944: the descriptor is created with O_RDWR, PIDFD_THREAD
    // is carried as O_EXCL, and PIDFD_NONBLOCK is carried as O_NONBLOCK, so
    // F_GETFL reproduces the requested flags exactly.
    let mut status = O_RDWR;
    if plan.nonblocking {
        status |= O_NONBLOCK;
    }
    if plan.thread {
        status |= O_EXCL;
    }
    let file = Arc::try_new(fd).map_err(|_| AxError::NoMemory)?;
    add_file_like_with_flags(file, true, status).map(|fd| fd as _)
}

pub fn sys_pidfd_getfd(pidfd: i32, target_fd: i32, flags: u32) -> AxResult<isize> {
    debug!("sys_pidfd_getfd <= pidfd: {pidfd}, target_fd: {target_fd}, flags: {flags}");

    if flags != 0 {
        return Err(AxError::InvalidInput);
    }
    // fs/pidfs.c:706-711 pidfd_pid() reports EBADF for any descriptor whose
    // file operations are not pidfs, exactly like the fd_empty() EBADF at
    // kernel/pid.c:967-969 for a closed descriptor.
    let pidfd = PidFd::from_fd(pidfd).map_err(|error| match error {
        AxError::InvalidInput => AxError::BadFileDescriptor,
        error => error,
    })?;
    let proc_data = pidfd.process_data()?;
    let authorized_image = check_pidfd_getfd_permission(&pidfd, &proc_data)?.into_aspace();
    let target = match pidfd.signal_thread_task()? {
        Some(task) => task,
        None => crate::task::get_visible_task(proc_data.proc.pid())?,
    };
    let description = target
        .as_thread()
        .try_fd_table()
        .ok_or(AxError::NoSuchProcess)?
        .get_description(target_fd)?;
    if proc_data.exec_in_progress() || !proc_data.image_matches(&authorized_image) {
        return Err(AxError::OperationNotPermitted);
    }
    add_file_description(description, true).map(|fd| fd as isize)
}

struct PidFdSignalRequest {
    signal: Option<SignalInfo>,
    code: i32,
}

/// Resolves the PID number a `PIDFD_SIGNAL_PROCESS_GROUP` request addresses as
/// a process-group ID.
///
/// Linux keeps the descriptor's `struct pid` and passes it to
/// `kill_pgrp_info()` as `pgrp`, so the group addressed is the one whose
/// `PGID` **is the descriptor's own PID number** — not the group the target
/// process currently belongs to.  `__kill_pgrp_info()` then walks
/// `pid->tasks[PIDTYPE_PGID]`, which is empty, and therefore `ESRCH`, whenever
/// no process group carries that number.
///
/// `PIDFD_SELF_THREAD` / `PIDFD_SELF_THREAD_GROUP` short-circuit before the
/// descriptor table to `get_task_pid(current, PIDTYPE_PID)` and
/// `get_task_pid(current, PIDTYPE_TGID)`, so their numbers are the calling
/// thread's TID and the calling process's PID.
fn process_group_pid_of_pidfd_target(
    target: tk_linux_fd::SignalTarget,
    pidfd: i32,
) -> AxResult<Pid> {
    match target {
        tk_linux_fd::SignalTarget::SelfThread => Ok(current().as_thread().kernel_tid()),
        tk_linux_fd::SignalTarget::SelfThreadGroup => Ok(current().as_thread().proc_data.proc.pid()),
        tk_linux_fd::SignalTarget::Descriptor => match PidFd::from_fd(pidfd) {
            Ok(pidfd) => {
                if pidfd.signal_thread_tid().is_some() {
                    // A `PIDFD_THREAD` descriptor names the thread's own PID
                    // object, so the group it can address is the one whose
                    // PGID equals that kernel-global TID.
                    let task = pidfd.signal_thread_task()?.ok_or(AxError::NoSuchProcess)?;
                    return Ok(task.as_thread().kernel_tid());
                }
                let identity = pidfd.process()?;
                // The descriptor does not pin the PID number the way Linux's
                // retained `struct pid` does, so a reaped target must not let
                // a recycled number resolve an unrelated group.
                if !exact_process_is_published(&identity)? {
                    return Err(AxError::NoSuchProcess);
                }
                Ok(identity.pid())
            }
            Err(AxError::InvalidInput) => Ok(process_data_from_proc_dir_fd(pidfd)?.proc.pid()),
            Err(err) => Err(err),
        },
    }
}

/// `do_pidfd_send_signal()` with `type == PIDTYPE_PGID`:
///
/// ```text
/// 	case PIDFD_SIGNAL_PROCESS_GROUP:
/// 		type = PIDTYPE_PGID;
/// 		break;
/// 	...
/// 	if (type == PIDTYPE_PGID)
/// 		return kill_pgrp_info(sig, &kinfo, pid);
/// ```
///
/// `do_pidfd_send_signal()` runs `copy_siginfo_from_user()` and
/// `prepare_kill_siginfo()` (EFAULT/EINVAL/EPERM) before it reaches
/// `kill_pgrp_info()`, which is the first point that walks the group and can
/// report `ESRCH` for a group with no members; the signal record is therefore
/// complete before the group is resolved.  Only the descriptor's own PID
/// number, which `pidfd_get_pid()` fixes before the siginfo copy, is resolved
/// ahead of it.
fn send_pidfd_process_group_signal<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pidfd: i32,
    signo: u32,
    sig: *mut SignalInfo,
    target: tk_linux_fd::SignalTarget,
) -> AxResult<isize> {
    let pgid = process_group_pid_of_pidfd_target(target, pidfd)?;
    let request = if sig.is_null() {
        // `prepare_kill_siginfo(sig, &kinfo, PIDTYPE_PGID)` synthesizes
        // `SI_USER` for every scope except `PIDTYPE_PID`, and signal 0 stays a
        // delivery-free existence/permission probe.
        PidFdSignalRequest {
            signal: make_siginfo(signo, linux_raw_sys::general::SI_USER as i32)?,
            code: linux_raw_sys::general::SI_USER as i32,
        }
    } else {
        // `type > PIDTYPE_TGID` makes the arbitrary-`si_code` rule
        // unconditional for this scope, so the exact target identity the
        // caller compares against is irrelevant here.
        make_pidfd_signal_info(memory, current().as_thread().tid(), signo, sig, true)?
    };
    let operation = signal_operation(
        request.signal.as_ref().and_then(SignalInfo::try_signo),
        SignalSecuritySource::PidFd { code: request.code },
        SignalDeliveryScope::ThreadGroup,
    )?;
    let targets = process_group_targets(pgid)?;
    send_user_signal_to_process_group_targets(targets, request.signal, operation)?;
    Ok(0)
}

fn make_pidfd_signal_info<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    target_id: u32,
    signo: u32,
    sig: *const SignalInfo,
    process_group_scope: bool,
) -> AxResult<PidFdSignalRequest> {
    // `SignalInfo` is the signal crate's fixed-size, layout-checked mirror of
    // Linux siginfo_t (including its union storage).  Read the complete record
    // through the explicit address-space context before interpreting fields.
    let sig = unsafe {
        VmPtr::vm_read_uninit(sig, memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
    let parsed_signo = (signo != 0).then(|| parse_signo(signo)).transpose()?;
    let raw_signo = sig.try_signo().ok_or(AxError::InvalidInput)? as i32;
    if i32::try_from(signo).ok() != Some(raw_signo) {
        return Err(AxError::InvalidInput);
    }
    // `do_pidfd_send_signal()`:
    //     /* Only allow sending arbitrary signals to yourself. */
    //     if ((task_pid(current) != pid || type > PIDTYPE_TGID) &&
    //         (kinfo.si_code >= 0 || kinfo.si_code == SI_TKILL))
    //             return -EPERM;
    // `type > PIDTYPE_TGID` is exactly the process-group scope, which can
    // never accept a caller-supplied `si_code`.
    if (current().as_thread().tid() != target_id || process_group_scope)
        && (sig.code() >= 0 || sig.code() == SI_TKILL)
    {
        return Err(AxError::OperationNotPermitted);
    }
    let code = sig.code();
    Ok(PidFdSignalRequest {
        signal: parsed_signo.map(|_| sig),
        code,
    })
}

pub fn sys_pidfd_send_signal<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pidfd: i32,
    signo: u32,
    sig: *mut SignalInfo,
    flags: u32,
) -> AxResult<isize> {
    // kernel/signal.c `SYSCALL_DEFINE4(pidfd_send_signal, ...)` validates the
    // scope word before it touches the descriptor table:
    //     /* Enforce flags be set to 0 until we add an extension. */
    //     if (flags & ~PIDFD_SEND_SIGNAL_FLAGS)                     return -EINVAL;
    //     /* Ensure that only a single signal scope determining flag is set. */
    //     if (hweight32(flags & PIDFD_SEND_SIGNAL_FLAGS) > 1)       return -EINVAL;
    // so an unknown bit or two scope flags at once is -EINVAL even for a
    // nonexistent `pidfd`, and `PIDFD_SELF_THREAD` / `PIDFD_SELF_THREAD_GROUP`
    // are negative magic descriptors that never reach the descriptor table.
    let plan = tk_linux_fd::pidfd_signal_plan(pidfd, flags, tk_linux_fd::SignalScope::ThreadGroup)
        .map_err(|tk_linux_fd::PidfdSignalError::InvalidInput| AxError::InvalidInput)?;
    // `do_pidfd_send_signal()` overrides the inferred type from the flags:
    //     case PIDFD_SIGNAL_PROCESS_GROUP:
    //             type = PIDTYPE_PGID;
    //             break;
    // and delivers with `kill_pgrp_info()` once the target PID object is
    // resolved, so the process-group scope selects a different object set
    // rather than a different publication scope.
    if plan.scope.is_process_group() {
        return send_pidfd_process_group_signal(memory, pidfd, signo, sig, plan.target);
    }

    let (target, scope) = match plan.target {
        tk_linux_fd::SignalTarget::SelfThread => (self_thread_signal_target()?, plan.scope),
        tk_linux_fd::SignalTarget::SelfThreadGroup => {
            (self_thread_group_signal_target()?, plan.scope)
        }
        tk_linux_fd::SignalTarget::Descriptor => {
            let target = signal_target_from_fd(pidfd)?;
            // A real pidfd's implied scope follows the descriptor kind
            // (`PIDFD_THREAD` ⇒ PIDTYPE_PID, otherwise PIDTYPE_TGID), which the
            // resolved target already records.  An explicit scope flag wins.
            let scope = if flags == 0 {
                match target.delivery_scope() {
                    SignalDeliveryScope::Thread => tk_linux_fd::SignalScope::Thread,
                    SignalDeliveryScope::ThreadGroup => tk_linux_fd::SignalScope::ThreadGroup,
                }
            } else {
                plan.scope
            };
            (target, scope)
        }
    };

    // prepare_kill_siginfo() synthesizes SI_TKILL only for PIDTYPE_PID and
    // SI_USER otherwise, keyed on the final type after the flag override, not
    // on the descriptor's implied kind.
    let synthesized_code = match scope {
        tk_linux_fd::SignalScope::Thread => SI_TKILL,
        _ => linux_raw_sys::general::SI_USER as i32,
    };
    let request = if sig.is_null() {
        if signo == 0 {
            PidFdSignalRequest {
                signal: None,
                code: synthesized_code,
            }
        } else {
            let signo = parse_signo(signo)?;
            let curr = current();
            let thread = curr.as_thread();
            let credential = thread.current_cred();
            // Match Linux prepare_kill_siginfo(): generated SI_USER/SI_TKILL
            // records report the sender's real UID, not the effective UID.
            let sender_uid = credential.user_ns().from_kuid_munged(credential.ids().ruid);
            PidFdSignalRequest {
                signal: Some(SignalInfo::new_user(
                    signo,
                    synthesized_code,
                    thread.proc_data.proc.pid(),
                    sender_uid,
                )),
                code: synthesized_code,
            }
        }
    } else {
        make_pidfd_signal_info(
            memory,
            target.visible_id(),
            signo,
            sig,
            scope.is_process_group(),
        )?
    };
    let delivery_scope = match scope {
        tk_linux_fd::SignalScope::Thread => SignalDeliveryScope::Thread,
        tk_linux_fd::SignalScope::ThreadGroup => SignalDeliveryScope::ThreadGroup,
        // The process-group scope is resolved as its own target set by
        // `send_pidfd_process_group_signal()` before this point; every
        // member delivery it performs is a thread-group publication.
        tk_linux_fd::SignalScope::ProcessGroup => return Err(LinuxError::EOPNOTSUPP.into()),
    };
    let operation = signal_operation(
        request.signal.as_ref().and_then(SignalInfo::try_signo),
        SignalSecuritySource::PidFd { code: request.code },
        delivery_scope,
    )?;
    let sig = request.signal;
    let queue_required = queued_signal_required(&sig);
    let mut target = target;
    let mut thread_retries = 0;
    let mut process_retries = 0;
    loop {
        match target {
            ResolvedPidFdSignalTarget::Process {
                process,
                identity,
                credential,
                leader_signal,
            } => {
                if !exact_process_is_published(&identity)? {
                    return Err(AxError::NoSuchProcess);
                }
                check_current_pinned_process_signal_access(
                    &process,
                    &credential,
                    SignalTargetKind::PidFdProcess,
                    operation,
                )?;
                if pidfd_process_post_hook(false, sig.is_some()) == PidFdProcessPostHook::Complete {
                    debug_assert_eq!(
                        pidfd_leader_post_hook(false, false),
                        PidFdLeaderPostHook::CompleteProbe
                    );
                    return Ok(0);
                }
                let lifecycle = process.lock_process_lifecycle();
                if pidfd_leader_post_hook(
                    true,
                    process.group_leader_signal_identity_matches(&leader_signal),
                ) == PidFdLeaderPostHook::RetryHandoff
                {
                    drop(lifecycle);
                    if process_retries >= 1 {
                        return Err(AxError::NoSuchProcess);
                    }
                    target = refresh_exact_process_pidfd_signal_target(identity, process)?;
                    process_retries += 1;
                    continue;
                }
                let result = if delivery_scope == SignalDeliveryScope::Thread {
                    // `PIDFD_SIGNAL_THREAD` overrides the scope to
                    // PIDTYPE_PID, which names only the group leader: publish
                    // to the leader's private queue instead of the shared one.
                    generate_signal_for_exited_leader(
                        &process,
                        &leader_signal,
                        &credential,
                        sig.clone(),
                        queue_required,
                    )
                } else if queue_required {
                    send_queued_signal_to_process_data_with_credential(
                        &process,
                        &credential,
                        sig.clone(),
                    )
                    .map(|_| ())
                } else {
                    send_signal_to_process_data_with_credential(&process, &credential, sig.clone())
                };
                drop(lifecycle);
                complete_pidfd_process_delivery(&identity, result)?;
                break;
            }
            ResolvedPidFdSignalTarget::Zombie {
                process,
                credential,
            } => {
                if !exact_process_is_published(&process)? {
                    return Err(AxError::NoSuchProcess);
                }
                check_current_zombie_signal_access(&process, &credential, operation)?;
                debug_assert_eq!(
                    pidfd_process_post_hook(true, sig.is_some()),
                    PidFdProcessPostHook::Complete
                );
                break;
            }
            ResolvedPidFdSignalTarget::ExitedLeader {
                pidfd,
                process,
                runtime,
                leader_signal,
                credential,
            } => {
                if !exact_process_is_published(&process)? {
                    return Err(AxError::NoSuchProcess);
                }
                check_current_pinned_process_identity_signal_access(
                    &process,
                    &credential,
                    SignalTargetKind::ExitedLeader,
                    operation,
                )?;
                // Linux signal 0 is only an existence/permission probe. A
                // concurrent de-thread after this exact hook must not trigger
                // identity retry or re-authorization.
                if pidfd_leader_post_hook(sig.is_some(), false)
                    == PidFdLeaderPostHook::CompleteProbe
                {
                    break;
                }
                if let Some(runtime) = runtime {
                    let leader_signal = leader_signal.ok_or(AxError::BadState)?;
                    let lifecycle = runtime.lock_process_lifecycle();
                    if pidfd_leader_post_hook(
                        true,
                        runtime.group_leader_signal_identity_matches(&leader_signal),
                    ) == PidFdLeaderPostHook::RetryHandoff
                    {
                        drop(lifecycle);
                        let retry_result = Err(AxError::NoSuchProcess);
                        if should_retry_pidfd_thread_delivery(&retry_result, true, thread_retries) {
                            target = retry_pidfd_thread_signal_target(pidfd, process.pid())?;
                            thread_retries += 1;
                            continue;
                        }
                        return Err(AxError::NoSuchProcess);
                    }
                    let result = if delivery_scope == SignalDeliveryScope::ThreadGroup {
                        // `PIDFD_SIGNAL_THREAD_GROUP` overrides the scope to
                        // PIDTYPE_TGID, which addresses the whole thread group
                        // of the descriptor's thread.
                        if queue_required {
                            send_queued_signal_to_process_data_with_credential(
                                &runtime,
                                &credential,
                                sig.clone(),
                            )
                            .map(|_| ())
                        } else {
                            send_signal_to_process_data_with_credential(
                                &runtime,
                                &credential,
                                sig.clone(),
                            )
                        }
                    } else {
                        generate_signal_for_exited_leader(
                            &runtime,
                            &leader_signal,
                            &credential,
                            sig.clone(),
                            queue_required,
                        )
                    };
                    drop(lifecycle);
                    result?;
                }
                break;
            }
            ResolvedPidFdSignalTarget::SelfThread {
                task,
                credential,
                visible_tid,
            } => {
                let thread = task.try_as_thread().ok_or(AxError::NoSuchProcess)?;
                check_current_pinned_thread_signal_access(
                    thread,
                    &task,
                    &credential,
                    visible_tid,
                    SignalTargetKind::PidFdThread,
                    operation,
                )?;
                if delivery_scope == SignalDeliveryScope::ThreadGroup {
                    // `PIDFD_SELF_THREAD` + `PIDFD_SIGNAL_THREAD_GROUP` is
                    // PIDTYPE_TGID on the calling thread: the whole calling
                    // thread group receives the signal.
                    let result = if queue_required {
                        send_queued_signal_to_process_data_with_credential(
                            &thread.proc_data,
                            &credential,
                            sig.clone(),
                        )
                        .map(|_| ())
                    } else {
                        send_signal_to_process_data_with_credential(
                            &thread.proc_data,
                            &credential,
                            sig.clone(),
                        )
                    };
                    result?;
                } else {
                    send_signal_to_authorized_thread(
                        &task,
                        &credential,
                        visible_tid,
                        sig.clone(),
                        queue_required,
                    )?;
                }
                break;
            }
            ResolvedPidFdSignalTarget::Thread {
                pidfd,
                task,
                credential,
                visible_tid,
            } => {
                let thread = task.try_as_thread().ok_or(AxError::NoSuchProcess)?;
                check_current_pinned_thread_signal_access(
                    thread,
                    &task,
                    &credential,
                    visible_tid,
                    SignalTargetKind::PidFdThread,
                    operation,
                )?;
                let stable_identity_is_leader = visible_tid == thread.proc_data.proc.pid();
                let result = if delivery_scope == SignalDeliveryScope::ThreadGroup {
                    // `PIDFD_SIGNAL_THREAD_GROUP` overrides the scope to
                    // PIDTYPE_TGID, which addresses the whole thread group of
                    // the descriptor's thread.
                    if queue_required {
                        send_queued_signal_to_process_data_with_credential(
                            &thread.proc_data,
                            &credential,
                            sig.clone(),
                        )
                        .map(|_| ())
                    } else {
                        send_signal_to_process_data_with_credential(
                            &thread.proc_data,
                            &credential,
                            sig.clone(),
                        )
                    }
                } else {
                    send_signal_to_authorized_thread(
                        &task,
                        &credential,
                        visible_tid,
                        sig.clone(),
                        queue_required,
                    )
                };
                if should_retry_pidfd_thread_delivery(
                    &result,
                    stable_identity_is_leader,
                    thread_retries,
                ) {
                    target = retry_pidfd_thread_signal_target(pidfd, visible_tid)?;
                    thread_retries += 1;
                    continue;
                }
                result?;
                break;
            }
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use axerrno::{AxError, AxResult, LinuxError};

    use super::{
        PIDFD_THREAD_SIGNAL_RETRY_LIMIT, PidFdLeaderPostHook, PidFdProcessPostHook,
        exact_identity_matches, pidfd_leader_post_hook, pidfd_process_post_hook,
        reduce_pidfd_process_delivery_result, should_retry_pidfd_thread_delivery,
        thread_pidfd_candidate_matches,
    };

    #[test]
    fn process_pidfd_identity_rejects_reap_and_pid_reuse() {
        let expected = Arc::new(7_u32);
        let published = expected.clone();
        let reused = Arc::new(7_u32);

        assert!(exact_identity_matches(&expected, Some(&published)));
        assert!(!exact_identity_matches(&expected, None));
        assert!(!exact_identity_matches(&expected, Some(&reused)));
    }

    #[test]
    fn process_pidfd_probe_and_zombie_complete_without_delivery() {
        assert_eq!(
            pidfd_process_post_hook(false, false),
            PidFdProcessPostHook::Complete
        );
        assert_eq!(
            pidfd_process_post_hook(true, false),
            PidFdProcessPostHook::Complete
        );
        assert_eq!(
            pidfd_process_post_hook(true, true),
            PidFdProcessPostHook::Complete
        );
        assert_eq!(
            pidfd_process_post_hook(false, true),
            PidFdProcessPostHook::Deliver
        );
    }

    #[test]
    fn pidfd_leader_post_hook_probe_never_retries_a_handoff() {
        assert_eq!(
            pidfd_leader_post_hook(false, false),
            PidFdLeaderPostHook::CompleteProbe
        );
        assert_eq!(
            pidfd_leader_post_hook(false, true),
            PidFdLeaderPostHook::CompleteProbe
        );
        assert_eq!(
            pidfd_leader_post_hook(true, true),
            PidFdLeaderPostHook::Deliver
        );
        assert_eq!(
            pidfd_leader_post_hook(true, false),
            PidFdLeaderPostHook::RetryHandoff
        );
    }

    #[test]
    fn process_pidfd_post_hook_esrch_only_completes_for_exact_zombie() {
        assert!(reduce_pidfd_process_delivery_result(Err(AxError::NoSuchProcess), true).is_ok());
        assert_eq!(
            reduce_pidfd_process_delivery_result(Err(AxError::NoSuchProcess), false),
            Err(AxError::NoSuchProcess)
        );
        assert_eq!(
            reduce_pidfd_process_delivery_result(Err(AxError::ResourceBusy), true),
            Err(AxError::ResourceBusy)
        );
        assert_eq!(
            reduce_pidfd_process_delivery_result(Ok(()) as AxResult<()>, false),
            Ok(())
        );
    }

    #[test]
    fn thread_pidfd_candidate_tracks_only_the_stable_pid_identity() {
        assert!(thread_pidfd_candidate_matches(41, 41, 41, true, true));
        assert!(thread_pidfd_candidate_matches(41, 41, 41, true, false));
        assert!(thread_pidfd_candidate_matches(42, 41, 42, true, true));
        assert!(!thread_pidfd_candidate_matches(42, 41, 41, true, true));
        assert!(!thread_pidfd_candidate_matches(42, 41, 42, true, false));
        assert!(!thread_pidfd_candidate_matches(41, 41, 41, false, false));
    }

    #[test]
    fn thread_pidfd_retries_only_a_leader_delivery_esrch_within_the_bound() {
        assert!(should_retry_pidfd_thread_delivery(
            &Err(AxError::NoSuchProcess),
            true,
            0
        ));
        assert!(!should_retry_pidfd_thread_delivery(
            &Err(AxError::NoSuchProcess),
            false,
            0
        ));
        assert!(!should_retry_pidfd_thread_delivery(
            &Err(AxError::WouldBlock),
            true,
            0
        ));
        assert!(!should_retry_pidfd_thread_delivery(
            &Err(AxError::NoSuchProcess),
            true,
            PIDFD_THREAD_SIGNAL_RETRY_LIMIT
        ));
    }
}
