use alloc::sync::Arc;

use axerrno::{AxError, AxResult};
use axtask::{AxTaskRef, current};
use bitflags::bitflags;
use linux_raw_sys::general::SI_TKILL;
use starry_signal::{SignalInfo, api::ThreadSignalManager};
use starry_vm::VmPtr;

use crate::{
    file::{Directory, FD_TABLE, FileHandle, FileLike, PidFd, add_file_description},
    pseudofs::{ProcDirProcess, process_data_from_proc_dir},
    syscall::signal::{
        parse_signo, queued_signal_required, send_signal_to_authorized_thread, signal_operation,
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
            Self::Thread { visible_tid, .. } => *visible_tid,
        }
    }

    fn delivery_scope(&self) -> SignalDeliveryScope {
        match self {
            Self::Process { .. } | Self::Zombie { .. } => SignalDeliveryScope::ThreadGroup,
            Self::ExitedLeader { .. } | Self::Thread { .. } => SignalDeliveryScope::Thread,
        }
    }

    fn synthesized_code(&self) -> i32 {
        match self {
            Self::Process { .. } | Self::Zombie { .. } => linux_raw_sys::general::SI_USER as i32,
            Self::ExitedLeader { .. } | Self::Thread { .. } => SI_TKILL,
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
    Ok(target_image)
}

bitflags! {
    #[derive(Debug, Clone, Copy, Default)]
    pub struct PidFdFlags: u32 {
        const NONBLOCK = 2048;
        const THREAD = 128;
    }
}

pub fn sys_pidfd_open(pid: i32, flags: u32) -> AxResult<isize> {
    debug!("sys_pidfd_open <= pid: {pid}, flags: {flags}");

    let flags = PidFdFlags::from_bits(flags).ok_or(AxError::InvalidInput)?;
    if pid <= 0 {
        return Err(AxError::InvalidInput);
    }
    let pid = pid as u32;

    let fd = if flags.contains(PidFdFlags::THREAD) {
        let task = get_visible_task(pid)?;
        PidFd::new_thread(&task)?
    } else {
        PidFd::new_process(&get_process_data(pid)?)
    };
    if flags.contains(PidFdFlags::NONBLOCK) {
        fd.set_nonblocking(true)?;
    }

    fd.add_to_fd_table(true).map(|fd| fd as _)
}

pub fn sys_pidfd_getfd(pidfd: i32, target_fd: i32, flags: u32) -> AxResult<isize> {
    debug!("sys_pidfd_getfd <= pidfd: {pidfd}, target_fd: {target_fd}, flags: {flags}");

    if flags != 0 {
        return Err(AxError::InvalidInput);
    }
    let pidfd = PidFd::from_fd(pidfd)?;
    let proc_data = pidfd.process_data()?;
    let authorized_image = check_pidfd_getfd_permission(&pidfd, &proc_data)?.into_aspace();
    let description = FD_TABLE
        .scope(&proc_data.scope.read())
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

fn make_pidfd_signal_info(
    target_id: u32,
    signo: u32,
    sig: *const SignalInfo,
) -> AxResult<PidFdSignalRequest> {
    let sig = unsafe { sig.vm_read_uninit()?.assume_init() };
    let parsed_signo = (signo != 0).then(|| parse_signo(signo)).transpose()?;
    // SAFETY: `sig` is a fully initialized local copy of the userspace
    // `siginfo_t`; reading the bindgen-exposed common header field is valid for
    // every union variant.
    let raw_signo = unsafe { sig.0.__bindgen_anon_1.__bindgen_anon_1.si_signo };
    if i32::try_from(signo).ok() != Some(raw_signo) {
        return Err(AxError::InvalidInput);
    }
    if current().as_thread().tid() != target_id && (sig.code() >= 0 || sig.code() == SI_TKILL) {
        return Err(AxError::OperationNotPermitted);
    }
    let code = sig.code();
    Ok(PidFdSignalRequest {
        signal: parsed_signo.map(|_| sig),
        code,
    })
}

pub fn sys_pidfd_send_signal(
    pidfd: i32,
    signo: u32,
    sig: *mut SignalInfo,
    flags: u32,
) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }

    let target = signal_target_from_fd(pidfd)?;

    let request = if sig.is_null() {
        if signo == 0 {
            let code = target.synthesized_code();
            PidFdSignalRequest { signal: None, code }
        } else {
            let signo = parse_signo(signo)?;
            let code = target.synthesized_code();
            let sender = current().as_thread().proc_data.proc.pid();
            PidFdSignalRequest {
                signal: Some(SignalInfo::new_user(signo, code, sender)),
                code,
            }
        }
    } else {
        make_pidfd_signal_info(target.visible_id(), signo, sig)?
    };
    let operation = signal_operation(
        request.signal.as_ref().map(SignalInfo::signo),
        SignalSecuritySource::PidFd { code: request.code },
        target.delivery_scope(),
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
                let result = if queue_required {
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
                    let result = generate_signal_for_exited_leader(
                        &runtime,
                        &leader_signal,
                        &credential,
                        sig.clone(),
                        queue_required,
                    );
                    drop(lifecycle);
                    result?;
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
                let result = send_signal_to_authorized_thread(
                    &task,
                    &credential,
                    visible_tid,
                    sig.clone(),
                    queue_required,
                );
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

    use axerrno::{AxError, AxResult};

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
