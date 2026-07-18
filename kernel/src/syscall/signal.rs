use alloc::sync::Arc;
use core::future::pending;

use axerrno::{AxError, AxResult, LinuxError};
use axhal::{
    time::{TimeValue, wall_time},
    uspace::UserContext,
};
use axtask::{
    AxTaskRef, current,
    future::{self, block_on},
};
use linux_raw_sys::general::{
    MINSIGSTKSZ, SI_TKILL, SI_USER, SIG_BLOCK, SIG_SETMASK, SIG_UNBLOCK, SS_DISABLE, SS_ONSTACK,
    siginfo, timespec,
};
use starry_process::Pid;
use starry_signal::{
    RawSignalAction, SignalAction, SignalInfo, SignalSet, SignalStack, Signo,
    api::{SignalFrame, SignalWaitObservation, ThreadSignalManager},
};
use starry_vm::{VmMutPtr, VmPtr};

use crate::{
    task::{
        AsThread, Cred, ProcStateHint, Process, ProcessData, SignalDeliveryScope, SignalNumber,
        SignalSecurityOperation, SignalSecuritySource, SignalTargetKind,
        acknowledge_posix_timer_signal, check_current_pinned_process_identity_signal_access,
        check_current_pinned_process_signal_access, check_current_pinned_thread_signal_access,
        check_current_zombie_signal_access, check_signals, complete_signal_delivery,
        force_signal_current_thread, generate_signal_for_exited_leader, get_process_data,
        get_process_group, get_process_including_zombie, get_visible_task, process_domain,
        process_error, send_authorized_signal_thread_inner,
        send_queued_signal_to_process_data_with_credential,
        send_signal_to_process_data_with_credential, with_proc_state_hint,
    },
    time::TimeValueLike,
};

pub(crate) fn check_sigset_size(size: usize) -> AxResult<()> {
    if size != size_of::<SignalSet>() && size != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

pub(crate) fn parse_signo(signo: u32) -> AxResult<Signo> {
    u8::try_from(signo)
        .ok()
        .and_then(Signo::from_repr)
        .ok_or(AxError::InvalidInput)
}

fn current_visible_tid() -> Pid {
    current().as_thread().tid()
}

pub fn sys_rt_sigprocmask(
    how: i32,
    set: *const SignalSet,
    oldset: *mut SignalSet,
    sigsetsize: usize,
) -> AxResult<isize> {
    check_sigset_size(sigsetsize)?;

    let curr = current();
    let sig = &curr.as_thread().signal;
    let old = sig.blocked();

    // Snapshot the requested mask before writing the old mask back. Linux
    // permits `set` and `oldset` to alias; BusyBox relies on that contract in
    // its wait path when it atomically blocks signals and saves the old mask.
    let new = if let Some(set) = set.nullable() {
        let set = unsafe { set.vm_read_uninit()?.assume_init() };
        Some(match how as u32 {
            SIG_BLOCK => old | set,
            SIG_UNBLOCK => old & !set,
            SIG_SETMASK => set,
            _ => return Err(AxError::InvalidInput),
        })
    } else {
        None
    };

    if let Some(new) = new {
        debug!("sys_rt_sigprocmask <= {new:?}");
        sig.set_blocked(new);
    }

    if let Some(oldset) = oldset.nullable() {
        oldset.vm_write(old)?;
    }

    Ok(0)
}

pub fn sys_rt_sigaction(
    signo: u32,
    act: *const RawSignalAction,
    oldact: *mut RawSignalAction,
    sigsetsize: usize,
) -> AxResult<isize> {
    check_sigset_size(sigsetsize)?;

    let signo = parse_signo(signo)?;
    if matches!(signo, Signo::SIGKILL | Signo::SIGSTOP) {
        return Err(AxError::InvalidInput);
    }

    let new_action = if let Some(act) = act.nullable() {
        let mut action: SignalAction = RawSignalAction::read_from_user(act)?.into();
        action.mask.remove(Signo::SIGKILL);
        action.mask.remove(Signo::SIGSTOP);
        Some(action)
    } else {
        None
    };

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let old_action = if let Some(action) = new_action {
        debug!("sys_rt_sigaction <= signo: {signo:?}, act: {action:?}");
        proc_data
            .signal
            .try_replace_action(signo, action)
            .map_err(|_| AxError::NoMemory)?
    } else {
        proc_data.signal.actions.lock()[signo].clone()
    };

    // Linux commits the new action before copying the previous one out. If
    // this user copy faults, the action transition and required queue flush
    // therefore remain visible.
    if let Some(oldact) = oldact.nullable() {
        RawSignalAction::from(old_action).write_to_user(oldact)?;
    }
    Ok(0)
}

pub fn sys_rt_sigpending(set: *mut SignalSet, sigsetsize: usize) -> AxResult<isize> {
    check_sigset_size(sigsetsize)?;
    set.vm_write(current().as_thread().signal.pending())?;
    Ok(0)
}

fn make_siginfo(signo: u32, code: i32) -> AxResult<Option<SignalInfo>> {
    if signo == 0 {
        return Ok(None);
    }
    let signo = parse_signo(signo)?;
    Ok(Some(SignalInfo::new_user(
        signo,
        code,
        current().as_thread().proc_data.proc.pid(),
    )))
}

pub(crate) fn queued_signal_required(signal: &Option<SignalInfo>) -> bool {
    signal
        .as_ref()
        .is_some_and(|info| info.signo().is_realtime() && info.code() != SI_USER as i32)
}

pub(crate) fn signal_operation(
    signal: Option<Signo>,
    source: SignalSecuritySource,
    delivery_scope: SignalDeliveryScope,
) -> AxResult<SignalSecurityOperation> {
    let signal = signal
        .map(|signal| SignalNumber::try_new(signal as u32).ok_or(AxError::InvalidInput))
        .transpose()?;
    Ok(SignalSecurityOperation::from_optional_signal(
        signal,
        source,
        delivery_scope,
    ))
}

struct AuthorizedProcessSignalTarget {
    process: Arc<ProcessData>,
    credential: Arc<Cred>,
    selection: AuthorizedProcessSelection,
}

enum AuthorizedProcessSelection {
    NamedTask {
        task: AxTaskRef,
        expected_visible_tid: Pid,
    },
    StableLeader {
        signal: Arc<ThreadSignalManager>,
    },
}

const PROCESS_SIGNAL_HANDOFF_RETRIES: usize = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthorizedProcessSignalDelivery {
    Complete,
    RetryHandoff,
    NamedTaskGone,
    DeliveryFailed(AxError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessSignalPostHook {
    CompleteProbe,
    Deliver,
    RetryHandoff,
    NamedTaskGone,
}

const fn process_signal_post_hook(
    has_signal: bool,
    named_task_valid: Option<bool>,
    leader_matches: bool,
) -> ProcessSignalPostHook {
    if !has_signal {
        ProcessSignalPostHook::CompleteProbe
    } else if let Some(valid) = named_task_valid {
        if valid {
            ProcessSignalPostHook::Deliver
        } else {
            ProcessSignalPostHook::NamedTaskGone
        }
    } else if leader_matches {
        ProcessSignalPostHook::Deliver
    } else {
        ProcessSignalPostHook::RetryHandoff
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SignalTargetAuthorizationError {
    MissingTarget,
    Failed(AxError),
}

impl SignalTargetAuthorizationError {
    const fn into_ax_error(self) -> AxError {
        match self {
            Self::MissingTarget => AxError::NoSuchProcess,
            Self::Failed(error) => error,
        }
    }
}

fn authorize_process_signal_target(
    pid: Pid,
    operation: SignalSecurityOperation,
) -> Result<AuthorizedProcessSignalTarget, SignalTargetAuthorizationError> {
    if pid == 0 {
        return Err(SignalTargetAuthorizationError::MissingTarget);
    }
    match get_visible_task(pid) {
        Ok(task) => {
            let thread = task
                .try_as_thread()
                .ok_or(SignalTargetAuthorizationError::Failed(
                    AxError::NoSuchProcess,
                ))?;
            let process = thread.proc_data.clone();
            let (credential, selection) = if pid != process.proc.pid() {
                let credential = thread.current_cred();
                check_current_pinned_thread_signal_access(
                    thread,
                    &task,
                    &credential,
                    pid,
                    SignalTargetKind::ProcessTask,
                    operation,
                )
                .map_err(SignalTargetAuthorizationError::Failed)?;
                (
                    credential,
                    AuthorizedProcessSelection::NamedTask {
                        task,
                        expected_visible_tid: pid,
                    },
                )
            } else {
                let (credential, signal) = process
                    .group_leader_signal_identity()
                    .map_err(SignalTargetAuthorizationError::Failed)?;
                check_current_pinned_process_signal_access(
                    &process,
                    &credential,
                    SignalTargetKind::Process,
                    operation,
                )
                .map_err(SignalTargetAuthorizationError::Failed)?;
                (
                    credential,
                    AuthorizedProcessSelection::StableLeader { signal },
                )
            };
            Ok(AuthorizedProcessSignalTarget {
                process,
                credential,
                selection,
            })
        }
        Err(AxError::NoSuchProcess) => {
            let process = get_process_data(pid).map_err(|error| {
                if error == AxError::NoSuchProcess {
                    SignalTargetAuthorizationError::MissingTarget
                } else {
                    SignalTargetAuthorizationError::Failed(error)
                }
            })?;
            let (credential, leader_signal) = process
                .group_leader_signal_identity()
                .map_err(SignalTargetAuthorizationError::Failed)?;
            check_current_pinned_process_signal_access(
                &process,
                &credential,
                SignalTargetKind::Process,
                operation,
            )
            .map_err(SignalTargetAuthorizationError::Failed)?;
            Ok(AuthorizedProcessSignalTarget {
                process,
                credential,
                selection: AuthorizedProcessSelection::StableLeader {
                    signal: leader_signal,
                },
            })
        }
        Err(error) => Err(SignalTargetAuthorizationError::Failed(error)),
    }
}

fn send_signal_to_authorized_process(
    target: &AuthorizedProcessSignalTarget,
    signal: Option<SignalInfo>,
    queue_required: bool,
) -> AuthorizedProcessSignalDelivery {
    let Some(signal) = signal else {
        debug_assert_eq!(
            process_signal_post_hook(false, None, false),
            ProcessSignalPostHook::CompleteProbe
        );
        return AuthorizedProcessSignalDelivery::Complete;
    };
    let lifecycle = target.process.lock_process_lifecycle();
    let post_hook = match &target.selection {
        AuthorizedProcessSelection::NamedTask {
            task,
            expected_visible_tid,
        } => {
            let thread = task.as_thread();
            let valid = thread.tid() == *expected_visible_tid
                && Arc::ptr_eq(&thread.proc_data, &target.process)
                && !thread.exit.load(core::sync::atomic::Ordering::Acquire)
                && target
                    .process
                    .proc
                    .thread_ids()
                    .any(|tid| tid == thread.kernel_tid());
            process_signal_post_hook(true, Some(valid), true)
        }
        AuthorizedProcessSelection::StableLeader { signal } => process_signal_post_hook(
            true,
            None,
            target.process.group_leader_signal_identity_matches(signal),
        ),
    };
    match post_hook {
        ProcessSignalPostHook::CompleteProbe => {
            drop(lifecycle);
            return AuthorizedProcessSignalDelivery::Complete;
        }
        ProcessSignalPostHook::RetryHandoff => {
            drop(lifecycle);
            return AuthorizedProcessSignalDelivery::RetryHandoff;
        }
        ProcessSignalPostHook::NamedTaskGone => {
            drop(lifecycle);
            return AuthorizedProcessSignalDelivery::NamedTaskGone;
        }
        ProcessSignalPostHook::Deliver => {}
    }
    let result = if queue_required {
        send_queued_signal_to_process_data_with_credential(
            &target.process,
            &target.credential,
            Some(signal),
        )
        .map(|_| ())
    } else {
        send_signal_to_process_data_with_credential(
            &target.process,
            &target.credential,
            Some(signal),
        )
    };
    drop(lifecycle);
    match result {
        Ok(()) => AuthorizedProcessSignalDelivery::Complete,
        Err(error) => AuthorizedProcessSignalDelivery::DeliveryFailed(error),
    }
}

fn check_zombie_process_signal_permission(
    process: &Arc<Process>,
    operation: SignalSecurityOperation,
) -> AxResult<bool> {
    if !process.is_zombie() || !exact_process_is_published(process)? {
        return Ok(false);
    }

    let snapshot = process.zombie_payload().ok_or(AxError::NoSuchProcess)?;
    check_current_zombie_signal_access(&process, &snapshot.credential, operation)?;
    Ok(true)
}

fn zombie_signal_succeeds(pid: Pid, operation: SignalSecurityOperation) -> AxResult<bool> {
    let process = get_process_including_zombie(pid)?;
    check_zombie_process_signal_permission(&process, operation)
}

const fn exited_leader_identity_matches(tgid: Option<Pid>, tid: Pid, process_pid: Pid) -> bool {
    if tid != process_pid {
        return false;
    }
    match tgid {
        Some(tgid) => tgid == process_pid,
        None => true,
    }
}

struct AuthorizedExitedLeaderSignalTarget {
    _process: Arc<Process>,
    runtime: Option<Arc<ProcessData>>,
    leader_signal: Option<Arc<ThreadSignalManager>>,
    credential: Arc<Cred>,
}

fn authorize_exited_leader_signal_target(
    tgid: Option<Pid>,
    tid: Pid,
    operation: SignalSecurityOperation,
) -> AxResult<Option<AuthorizedExitedLeaderSignalTarget>> {
    let process = get_process_including_zombie(tid)?;
    if !exited_leader_identity_matches(tgid, tid, process.pid())
        || !exact_process_is_published(&process)?
    {
        return Ok(None);
    }
    let (runtime, leader_signal, credential) = if process.is_zombie() {
        (
            None,
            None,
            process
                .zombie_payload()
                .map(|snapshot| snapshot.credential.clone())
                .ok_or(AxError::NoSuchProcess)?,
        )
    } else {
        match get_process_data(process.pid()) {
            Ok(process_data) if Arc::ptr_eq(&process_data.proc, &process) => {
                let (credential, leader_signal) = process_data.group_leader_signal_identity()?;
                (Some(process_data), Some(leader_signal), credential)
            }
            Ok(_) => return Err(AxError::NoSuchProcess),
            Err(AxError::NoSuchProcess) if process.is_zombie() => (
                None,
                None,
                process
                    .zombie_payload()
                    .map(|snapshot| snapshot.credential.clone())
                    .ok_or(AxError::NoSuchProcess)?,
            ),
            Err(error) => return Err(error),
        }
    };
    if !exact_process_is_published(&process)? {
        return Ok(None);
    }
    check_current_pinned_process_identity_signal_access(
        &process,
        &credential,
        SignalTargetKind::ExitedLeader,
        operation,
    )?;
    Ok(Some(AuthorizedExitedLeaderSignalTarget {
        _process: process,
        runtime,
        leader_signal,
        credential,
    }))
}

fn signal_signo(signal: &Option<SignalInfo>) -> Option<Signo> {
    signal.as_ref().map(SignalInfo::signo)
}

enum GroupSignalTarget {
    Live(AuthorizedProcessSignalTarget),
    Zombie {
        process: Arc<Process>,
        credential: Arc<Cred>,
    },
}

fn exact_process_is_published(process: &Arc<Process>) -> AxResult<bool> {
    Ok(process_domain()?
        .registry()
        .get(process.pid())
        .is_some_and(|published| Arc::ptr_eq(&published, process)))
}

fn reduce_process_signal_delivery_result(
    result: AxResult<()>,
    exact_zombie_is_published: bool,
    named_nonleader: bool,
) -> AxResult<()> {
    match result {
        // Linux retries process-directed sends across final exit. Treat the
        // signal as delivered while the same unreaped zombie still owns the
        // numeric TGID, but never after reap, pid reuse, or when the original
        // numeric name was a nonleader TID whose private pid identity died.
        Err(AxError::NoSuchProcess) if exact_zombie_is_published && !named_nonleader => Ok(()),
        result => result,
    }
}

fn complete_process_signal_delivery(
    process: &Arc<Process>,
    result: AxResult<()>,
    named_nonleader: bool,
) -> AxResult<()> {
    let exact_zombie_is_published = if !named_nonleader
        && matches!(&result, Err(AxError::NoSuchProcess))
        && process.is_zombie()
    {
        exact_process_is_published(process)?
    } else {
        false
    };
    reduce_process_signal_delivery_result(result, exact_zombie_is_published, named_nonleader)
}

fn send_signal_to_exact_process_with_attempts(
    process: Arc<Process>,
    signal: Option<SignalInfo>,
    operation: SignalSecurityOperation,
    queue_required: bool,
    attempts: usize,
) -> AxResult<()> {
    debug_assert!(attempts != 0);
    for attempt in 0..attempts {
        match resolve_group_signal_target(process.clone())? {
            GroupSignalTarget::Live(target) => {
                if !exact_process_is_published(&target.process.proc)? {
                    return Err(AxError::NoSuchProcess);
                }
                check_current_pinned_process_signal_access(
                    &target.process,
                    &target.credential,
                    SignalTargetKind::Process,
                    operation,
                )?;
                match send_signal_to_authorized_process(&target, signal.clone(), queue_required) {
                    AuthorizedProcessSignalDelivery::Complete => return Ok(()),
                    AuthorizedProcessSignalDelivery::RetryHandoff if attempt + 1 < attempts => {
                        continue;
                    }
                    AuthorizedProcessSignalDelivery::RetryHandoff
                    | AuthorizedProcessSignalDelivery::NamedTaskGone => {
                        return Err(AxError::NoSuchProcess);
                    }
                    AuthorizedProcessSignalDelivery::DeliveryFailed(error) => {
                        return complete_process_signal_delivery(&process, Err(error), false);
                    }
                }
            }
            GroupSignalTarget::Zombie {
                process,
                credential,
            } => {
                if !exact_process_is_published(&process)? {
                    return Err(AxError::NoSuchProcess);
                }
                check_current_zombie_signal_access(&process, &credential, operation)?;
                return Ok(());
            }
        }
    }
    Err(AxError::NoSuchProcess)
}

fn complete_initial_process_signal(
    target: AuthorizedProcessSignalTarget,
    signal: Option<SignalInfo>,
    operation: SignalSecurityOperation,
    queue_required: bool,
) -> AxResult<()> {
    let process = target.process.proc.clone();
    let named_nonleader = matches!(
        &target.selection,
        AuthorizedProcessSelection::NamedTask { .. }
    );
    match send_signal_to_authorized_process(&target, signal.clone(), queue_required) {
        AuthorizedProcessSignalDelivery::Complete => Ok(()),
        AuthorizedProcessSignalDelivery::RetryHandoff => {
            send_signal_to_exact_process_with_attempts(
                process,
                signal,
                operation,
                queue_required,
                PROCESS_SIGNAL_HANDOFF_RETRIES,
            )
        }
        AuthorizedProcessSignalDelivery::NamedTaskGone => Err(AxError::NoSuchProcess),
        AuthorizedProcessSignalDelivery::DeliveryFailed(error) => {
            complete_process_signal_delivery(&process, Err(error), named_nonleader)
        }
    }
}

fn resolve_group_signal_target(process: Arc<Process>) -> AxResult<GroupSignalTarget> {
    if !exact_process_is_published(&process)? {
        return Err(AxError::NoSuchProcess);
    }
    if process.is_zombie() {
        let snapshot = process.zombie_payload().ok_or(AxError::NoSuchProcess)?;
        return Ok(GroupSignalTarget::Zombie {
            process,
            credential: snapshot.credential.clone(),
        });
    }
    let process_data = match get_process_data(process.pid()) {
        Ok(process_data) if Arc::ptr_eq(&process_data.proc, &process) => process_data,
        Ok(_) => return Err(AxError::NoSuchProcess),
        Err(_error) if process.is_zombie() => {
            let snapshot = process.zombie_payload().ok_or(AxError::NoSuchProcess)?;
            return Ok(GroupSignalTarget::Zombie {
                process,
                credential: snapshot.credential.clone(),
            });
        }
        Err(error) => return Err(error),
    };
    let (credential, leader_signal) = process_data.group_leader_signal_identity()?;
    Ok(GroupSignalTarget::Live(AuthorizedProcessSignalTarget {
        process: process_data,
        credential,
        selection: AuthorizedProcessSelection::StableLeader {
            signal: leader_signal,
        },
    }))
}

#[derive(Clone, Copy)]
enum SignalTargetAggregation {
    ProcessGroup,
    Broadcast,
}

struct SignalTargetResultReducer {
    aggregation: SignalTargetAggregation,
    saw_target: bool,
    any_success: bool,
    last_error: AxError,
    broadcast_result: Option<AxResult<()>>,
}

impl SignalTargetResultReducer {
    const fn new(aggregation: SignalTargetAggregation) -> Self {
        Self {
            aggregation,
            saw_target: false,
            any_success: false,
            last_error: AxError::NoSuchProcess,
            broadcast_result: None,
        }
    }

    fn record(&mut self, result: AxResult<()>) {
        self.saw_target = true;
        match result {
            Ok(()) => {
                self.any_success = true;
                if matches!(self.aggregation, SignalTargetAggregation::Broadcast) {
                    self.broadcast_result = Some(Ok(()));
                }
            }
            Err(error) => {
                self.last_error = error;
                if matches!(self.aggregation, SignalTargetAggregation::Broadcast)
                    && error != AxError::OperationNotPermitted
                {
                    self.broadcast_result = Some(Err(error));
                }
            }
        }
    }

    fn finish(self) -> AxResult<()> {
        match self.aggregation {
            SignalTargetAggregation::ProcessGroup => {
                if self.any_success {
                    Ok(())
                } else {
                    Err(self.last_error)
                }
            }
            // Linux's historical kill(-1) reducer starts at success and lets
            // every result except EPERM replace it. Thus an existing set of
            // entirely forbidden targets still returns success.
            SignalTargetAggregation::Broadcast => self.broadcast_result.unwrap_or_else(|| {
                if self.saw_target {
                    Ok(())
                } else {
                    Err(AxError::NoSuchProcess)
                }
            }),
        }
    }
}

fn send_user_signal_to_targets(
    targets: impl IntoIterator<Item = Arc<Process>>,
    signal: Option<SignalInfo>,
    operation: SignalSecurityOperation,
    aggregation: SignalTargetAggregation,
) -> AxResult<()> {
    let mut reducer = SignalTargetResultReducer::new(aggregation);

    for process in targets {
        // The Arc is this iteration's tasklist-style identity pin. Retry only
        // the same process once when de-thread replaces its leader token; never
        // redirect to a reused numeric PID or retry a security-hook failure.
        let result = send_signal_to_exact_process_with_attempts(
            process,
            signal.clone(),
            operation,
            false,
            PROCESS_SIGNAL_HANDOFF_RETRIES + 1,
        );
        reducer.record(result);
    }

    reducer.finish()
}

struct AuthorizedThreadSignalTarget {
    task: AxTaskRef,
    credential: Arc<Cred>,
    visible_tid: Pid,
}

enum AuthorizedNumericThreadSignalTarget {
    Live(AuthorizedThreadSignalTarget),
    ExitedLeader(AuthorizedExitedLeaderSignalTarget),
}

fn authorize_visible_thread_signal_target(
    tgid: Option<Pid>,
    tid: Pid,
    operation: SignalSecurityOperation,
) -> Result<AuthorizedThreadSignalTarget, SignalTargetAuthorizationError> {
    let task = get_visible_task(tid).map_err(|error| {
        if error == AxError::NoSuchProcess {
            SignalTargetAuthorizationError::MissingTarget
        } else {
            SignalTargetAuthorizationError::Failed(error)
        }
    })?;
    let thread = task
        .try_as_thread()
        .ok_or(SignalTargetAuthorizationError::Failed(
            AxError::OperationNotPermitted,
        ))?;
    if tgid.is_some_and(|tgid| thread.proc_data.proc.pid() != tgid) {
        return Err(SignalTargetAuthorizationError::MissingTarget);
    }
    let credential = thread.current_cred();
    check_current_pinned_thread_signal_access(
        thread,
        &task,
        &credential,
        tid,
        SignalTargetKind::Thread,
        operation,
    )
    .map_err(SignalTargetAuthorizationError::Failed)?;
    Ok(AuthorizedThreadSignalTarget {
        task,
        credential,
        visible_tid: tid,
    })
}

fn authorize_numeric_thread_signal_target(
    tgid: Option<Pid>,
    tid: Pid,
    operation: SignalSecurityOperation,
) -> AxResult<AuthorizedNumericThreadSignalTarget> {
    match authorize_visible_thread_signal_target(tgid, tid, operation) {
        Ok(target) => Ok(AuthorizedNumericThreadSignalTarget::Live(target)),
        Err(SignalTargetAuthorizationError::MissingTarget) => {
            authorize_exited_leader_signal_target(tgid, tid, operation)?
                .map(AuthorizedNumericThreadSignalTarget::ExitedLeader)
                .ok_or(AxError::NoSuchProcess)
        }
        Err(error) => Err(error.into_ax_error()),
    }
}

/// Publishes to the exact task object retained across authorization. Numeric
/// TID lookup is deliberately absent here: a retired TID must not redirect an
/// already-authorized request to a newly published task with the same number.
pub(crate) fn send_signal_to_authorized_thread(
    task: &AxTaskRef,
    target_cred: &Cred,
    expected_visible_tid: Pid,
    signal: Option<SignalInfo>,
    queue_required: bool,
) -> AxResult<()> {
    let thread = task.try_as_thread().ok_or(AxError::NoSuchProcess)?;
    let Some(signal) = signal else {
        return Ok(());
    };
    let lifecycle = thread.proc_data.lock_process_lifecycle();
    if thread.tid() != expected_visible_tid
        || thread.exit.load(core::sync::atomic::Ordering::Acquire)
        || !thread
            .proc_data
            .proc
            .thread_ids()
            .any(|tid| tid == thread.kernel_tid())
    {
        return Err(AxError::NoSuchProcess);
    }
    let result =
        send_authorized_signal_thread_inner(task, thread, target_cred, signal, queue_required)
            .map(|_| ());
    drop(lifecycle);
    result
}

fn send_signal_to_authorized_numeric_thread(
    target: AuthorizedNumericThreadSignalTarget,
    signal: Option<SignalInfo>,
    queue_required: bool,
) -> AxResult<()> {
    match target {
        AuthorizedNumericThreadSignalTarget::Live(target) => {
            complete_specific_thread_signal(send_signal_to_authorized_thread(
                &target.task,
                &target.credential,
                target.visible_tid,
                signal,
                queue_required,
            ))
        }
        AuthorizedNumericThreadSignalTarget::ExitedLeader(target) => {
            match (target.runtime, target.leader_signal) {
                (Some(runtime), Some(leader_signal)) => {
                    let lifecycle = runtime.lock_process_lifecycle();
                    if !runtime.group_leader_signal_identity_matches(&leader_signal) {
                        drop(lifecycle);
                        return Ok(());
                    }
                    let result = generate_signal_for_exited_leader(
                        &runtime,
                        &leader_signal,
                        &target.credential,
                        signal,
                        queue_required,
                    );
                    drop(lifecycle);
                    result
                }
                (None, None) => Ok(()),
                _ => Err(AxError::BadState),
            }
        }
    }
}

fn complete_specific_thread_signal(result: AxResult<()>) -> AxResult<()> {
    match result {
        // Linux's do_send_specific() treats a task which disappears after
        // authorization as having died just after receiving its private
        // signal. pidfd_send_signal does not use this reducer.
        Err(AxError::NoSuchProcess) => Ok(()),
        result => result,
    }
}

pub fn sys_kill(pid: i32, signo: u32) -> AxResult<isize> {
    debug!("sys_kill: pid = {pid}, signo = {signo}");
    let sig = make_siginfo(signo, SI_USER as _)?;
    let permission_signal = signal_signo(&sig);
    let operation = signal_operation(
        permission_signal,
        SignalSecuritySource::Kill,
        SignalDeliveryScope::ThreadGroup,
    )?;

    match pid {
        1.. => {
            let pid = pid as Pid;
            match authorize_process_signal_target(pid, operation) {
                Ok(target) => {
                    complete_initial_process_signal(target, sig, operation, false)?;
                }
                Err(SignalTargetAuthorizationError::MissingTarget)
                    if zombie_signal_succeeds(pid, operation)? => {}
                Err(error) => return Err(error.into_ax_error()),
            }
        }
        0 => {
            let group = current().as_thread().proc_data.proc.group();
            let targets = group
                .try_processes(process_domain()?.registry())
                .map_err(process_error)?;
            send_user_signal_to_targets(
                targets,
                sig,
                operation,
                SignalTargetAggregation::ProcessGroup,
            )?;
        }
        -1 => {
            let current_process = current().as_thread().proc_data.proc.clone();
            let targets = process_domain()?
                .registry()
                .try_processes()
                .map_err(process_error)?
                .into_iter()
                .filter(|process| !process.is_init() && !Arc::ptr_eq(process, &current_process));
            send_user_signal_to_targets(
                targets,
                sig,
                operation,
                SignalTargetAggregation::Broadcast,
            )?;
        }
        ..-1 => {
            let pgid = pid.checked_neg().ok_or(AxError::NoSuchProcess)? as Pid;
            let targets = get_process_group(pgid)?
                .try_processes(process_domain()?.registry())
                .map_err(process_error)?;
            send_user_signal_to_targets(
                targets,
                sig,
                operation,
                SignalTargetAggregation::ProcessGroup,
            )?;
        }
    }
    Ok(0)
}

pub fn sys_tkill(tid: i32, signo: u32) -> AxResult<isize> {
    if tid <= 0 {
        return Err(AxError::InvalidInput);
    }
    let sig = make_siginfo(signo, SI_TKILL)?;
    let operation = signal_operation(
        signal_signo(&sig),
        SignalSecuritySource::Thread,
        SignalDeliveryScope::Thread,
    )?;
    let target = authorize_numeric_thread_signal_target(None, tid as Pid, operation)?;
    send_signal_to_authorized_numeric_thread(target, sig, true)?;
    Ok(0)
}

pub fn sys_tgkill(tgid: i32, tid: i32, signo: u32) -> AxResult<isize> {
    if tgid <= 0 || tid <= 0 {
        return Err(AxError::InvalidInput);
    }
    let sig = make_siginfo(signo, SI_TKILL)?;
    let operation = signal_operation(
        signal_signo(&sig),
        SignalSecuritySource::Thread,
        SignalDeliveryScope::Thread,
    )?;
    let target = authorize_numeric_thread_signal_target(Some(tgid as Pid), tid as Pid, operation)?;
    send_signal_to_authorized_numeric_thread(target, sig, true)?;
    Ok(0)
}

struct QueuedSignalRequest {
    signal: Option<SignalInfo>,
    code: i32,
}

fn make_queue_signal_info(
    target_tid: Pid,
    signo: u32,
    sig: *const SignalInfo,
) -> AxResult<QueuedSignalRequest> {
    let mut sig = unsafe { sig.vm_read_uninit()?.assume_init() };
    let signo = (signo != 0).then(|| parse_signo(signo)).transpose()?;
    if (sig.code() >= 0 || sig.code() == SI_TKILL) && current_visible_tid() != target_tid {
        return Err(AxError::OperationNotPermitted);
    }
    let code = sig.code();
    if let Some(signo) = signo {
        sig.set_signo(signo);
        Ok(QueuedSignalRequest {
            signal: Some(sig),
            code,
        })
    } else {
        Ok(QueuedSignalRequest { signal: None, code })
    }
}

pub fn sys_rt_sigqueueinfo(pid: Pid, signo: u32, sig: *const SignalInfo) -> AxResult<isize> {
    let request = make_queue_signal_info(pid, signo, sig)?;
    let permission_signal = signal_signo(&request.signal);
    let operation = signal_operation(
        permission_signal,
        SignalSecuritySource::Queued { code: request.code },
        SignalDeliveryScope::ThreadGroup,
    )?;
    let sig = request.signal;
    let queue_required = queued_signal_required(&sig);
    match authorize_process_signal_target(pid, operation) {
        Ok(target) => {
            complete_initial_process_signal(target, sig, operation, queue_required)?;
        }
        Err(SignalTargetAuthorizationError::MissingTarget)
            if zombie_signal_succeeds(pid, operation)? => {}
        Err(error) => return Err(error.into_ax_error()),
    }
    Ok(0)
}

pub fn sys_rt_tgsigqueueinfo(
    tgid: i32,
    tid: i32,
    signo: u32,
    sig: *const SignalInfo,
) -> AxResult<isize> {
    if tgid <= 0 || tid <= 0 {
        return Err(AxError::InvalidInput);
    }

    let request = make_queue_signal_info(tid as Pid, signo, sig)?;
    let operation = signal_operation(
        signal_signo(&request.signal),
        SignalSecuritySource::Queued { code: request.code },
        SignalDeliveryScope::Thread,
    )?;
    let sig = request.signal;
    let queue_required = queued_signal_required(&sig);
    let target = authorize_numeric_thread_signal_target(Some(tgid as Pid), tid as Pid, operation)?;
    send_signal_to_authorized_numeric_thread(target, sig, queue_required)?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
const SIGNAL_PC_ALIGNMENT: usize = 1;
#[cfg(target_arch = "riscv64")]
const SIGNAL_PC_ALIGNMENT: usize = 2;
#[cfg(any(target_arch = "loongarch64", target_arch = "aarch64"))]
const SIGNAL_PC_ALIGNMENT: usize = 4;

#[cfg(target_arch = "aarch64")]
const SIGNAL_SP_ALIGNMENT: usize = 16;
#[cfg(not(target_arch = "aarch64"))]
const SIGNAL_SP_ALIGNMENT: usize = 1;

fn valid_signal_user_address(address: usize, alignment: usize) -> bool {
    let end = crate::config::USER_SPACE_BASE + crate::config::USER_SPACE_SIZE;
    address >= crate::config::USER_SPACE_BASE && address < end && address % alignment == 0
}

fn reject_bad_sigreturn(reason: &str) -> AxResult<isize> {
    warn!("rejecting invalid rt_sigreturn frame: {reason}");
    force_signal_current_thread(SignalInfo::new_kernel(Signo::SIGSEGV));
    Ok(0)
}

pub fn sys_rt_sigreturn(uctx: &mut UserContext) -> AxResult<isize> {
    let curr = current();
    let thr = curr.as_thread();

    if !thr.in_signal_handler() {
        return reject_bad_sigreturn("no active signal handler");
    }

    let frame = match SignalFrame::read_from_user(uctx.sp() as *const SignalFrame) {
        Ok(frame) => frame,
        Err(_) => return reject_bad_sigreturn("frame copy-in fault"),
    };

    let prepared = match thr.signal.prepare_restore(
        uctx,
        frame,
        |pc| valid_signal_user_address(pc, SIGNAL_PC_ALIGNMENT),
        |sp| valid_signal_user_address(sp, SIGNAL_SP_ALIGNMENT),
    ) {
        Ok(prepared) => prepared,
        Err(err) => {
            warn!("rt_sigreturn context validation failed: {err:?}");
            return reject_bad_sigreturn("invalid machine context");
        }
    };

    // No operation after this point may fail: context, mask and restart state
    // become visible only after the complete frame has passed validation.
    thr.signal.commit_restore(uctx, prepared);
    thr.complete_sigreturn(uctx);
    Ok(uctx.retval() as isize)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SignalWaitWake {
    Initial,
    Interrupted,
    TimedOut,
    Failed(AxError),
}

#[derive(Debug, Eq, PartialEq)]
enum SignalWaitStep<T> {
    Accepted(T),
    Delivered { handler_entered: bool },
    Block,
    TimedOut,
    Failed(AxError),
}

/// Resolves one synchronous-signal-wait observation outside the task's block
/// session. A signal selected by `rt_sigtimedwait` wins over an asynchronously
/// delivered signal, and both win over an elapsed timer observed by the
/// previous wait-only session.
fn signal_wait_step<T>(
    wake: SignalWaitWake,
    accept: impl FnOnce() -> Option<T>,
    deliver: impl FnOnce() -> Option<bool>,
) -> SignalWaitStep<T> {
    if let Some(value) = accept() {
        return SignalWaitStep::Accepted(value);
    }
    if let Some(handler_entered) = deliver() {
        return SignalWaitStep::Delivered { handler_entered };
    }
    match wake {
        SignalWaitWake::Initial | SignalWaitWake::Interrupted => SignalWaitStep::Block,
        SignalWaitWake::TimedOut => SignalWaitStep::TimedOut,
        SignalWaitWake::Failed(error) => SignalWaitStep::Failed(error),
    }
}

/// Owns restoration of the mask temporarily replaced by a synchronous signal
/// wait. A successfully installed handler takes that ownership through its
/// signal frame; every other return path restores the old mask here.
struct TemporarySignalMask<'a> {
    signal: &'a ThreadSignalManager,
    old_blocked: SignalSet,
    restore_on_drop: bool,
}

impl<'a> TemporarySignalMask<'a> {
    fn replace(
        signal: &'a ThreadSignalManager,
        old_blocked: SignalSet,
        temporary: SignalSet,
    ) -> Self {
        // Preserve the real mask before exposing the temporary one so an
        // ignored signal which was originally blocked remains queueable.
        signal.set_real_blocked(Some(old_blocked));
        signal.set_blocked(temporary);
        Self {
            signal,
            old_blocked,
            restore_on_drop: true,
        }
    }

    fn old_blocked(&self) -> SignalSet {
        self.old_blocked
    }

    fn restore(&mut self) {
        if self.restore_on_drop {
            // Restore the visible mask before removing the original-mask
            // sidecar, closing the inverse race of `replace` above.
            self.signal.set_blocked(self.old_blocked);
            self.signal.set_real_blocked(None);
            self.restore_on_drop = false;
        }
    }

    fn hand_off_to_handler(&mut self) {
        if self.restore_on_drop {
            self.signal.set_real_blocked(None);
            self.restore_on_drop = false;
        }
    }

    fn finish_delivery(&mut self, handler_entered: bool) {
        if handler_entered {
            self.hand_off_to_handler();
        } else {
            self.restore();
        }
    }
}

impl Drop for TemporarySignalMask<'_> {
    fn drop(&mut self) {
        self.restore();
    }
}

fn signal_wait_deadline(
    now: TimeValue,
    timeout: Option<TimeValue>,
) -> Result<Option<TimeValue>, future::TimerRegistrationError> {
    timeout
        .map(|duration| {
            now.checked_add(duration)
                .ok_or(future::TimerRegistrationError::DeadlineOverflow)
        })
        .transpose()
}

fn sanitize_synchronous_wait_set(mut set: SignalSet) -> SignalSet {
    // Linux never lets SIGKILL or SIGSTOP become synchronously accepted.
    set.remove(Signo::SIGKILL);
    set.remove(Signo::SIGSTOP);
    set
}

/// Reusable wait-only state for one synchronous signal syscall.
///
/// The absolute deadline is computed by the syscall, but its bounded timer
/// slot is admitted lazily only after a signal observation found no work. One
/// reservation is then reused by every stale-interrupt session. Each borrowed
/// race disarms its waker automatically while retaining the timer slot until
/// this owner is dropped or the deadline elapses.
struct SignalWaitBlock {
    deadline: Option<TimeValue>,
    reservation: Option<future::DeadlineReservation>,
}

impl SignalWaitBlock {
    const fn new(deadline: Option<TimeValue>) -> Self {
        Self {
            deadline,
            reservation: None,
        }
    }

    /// Blocks only on the task interrupt token and the optional bounded timer.
    ///
    /// Signal dequeue, handler-frame publication, exit, and job-control work
    /// stay in the caller, outside this synchronous block session. Failures are
    /// returned as observations too: the caller performs one final signal
    /// transaction before allowing timer admission or scheduler state to win.
    fn wait(&mut self) -> SignalWaitWake {
        let Some(deadline) = self.deadline else {
            return match block_on(future::interruptible(pending::<()>())) {
                Err(error) => SignalWaitWake::Failed(error.into()),
                Ok(Err(_)) => SignalWaitWake::Interrupted,
                // `pending()` cannot complete. Keep the impossible edge typed
                // rather than turning an invariant failure into a spin.
                Ok(Ok(())) => SignalWaitWake::Failed(AxError::BadState),
            };
        };

        if self.reservation.is_none() {
            self.reservation = match future::DeadlineReservation::reserve(deadline) {
                Ok(reservation) => Some(reservation),
                Err(error) => return SignalWaitWake::Failed(error.into()),
            };
        }
        let Some(reservation) = self.reservation.as_mut() else {
            return SignalWaitWake::Failed(AxError::BadState);
        };
        match block_on(reservation.race(future::interruptible(pending::<()>()))) {
            Err(error) => SignalWaitWake::Failed(error.into()),
            Ok(Err(future::Elapsed)) => SignalWaitWake::TimedOut,
            Ok(Ok(Err(_))) => SignalWaitWake::Interrupted,
            Ok(Ok(Ok(()))) => SignalWaitWake::Failed(AxError::BadState),
        }
    }
}

pub fn sys_rt_sigtimedwait(
    uctx: &mut UserContext,
    set: *const SignalSet,
    info: *mut siginfo,
    timeout: *const timespec,
    sigsetsize: usize,
) -> AxResult<isize> {
    check_sigset_size(sigsetsize)?;

    let set = sanitize_synchronous_wait_set(unsafe { set.vm_read_uninit()?.assume_init() });

    let timeout = if let Some(ts) = timeout.nullable() {
        let ts = unsafe { ts.vm_read_uninit()?.assume_init() };
        Some(ts.try_into_time_value()?)
    } else {
        None
    };

    debug!("sys_rt_sigtimedwait => set = {set:?}, timeout = {timeout:?}");

    let curr = current();
    let thr = curr.as_thread();
    let signal = &thr.signal;

    // Compute the absolute deadline once. Rechecking after stale interrupts
    // or concurrent wakeups must never extend the caller's relative timeout.
    let deadline = signal_wait_deadline(wall_time(), timeout).map_err(AxError::from)?;

    let old_blocked = signal.blocked();
    let mut temporary_mask = TemporarySignalMask::replace(signal, old_blocked, old_blocked & !set);

    uctx.set_retval(-LinuxError::EINTR.code() as usize);
    let sig = with_proc_state_hint(ProcStateHint::Interruptible, || {
        let mut block = SignalWaitBlock::new(deadline);
        let mut wake = SignalWaitWake::Initial;
        loop {
            match signal.observe_signal_wait(uctx, &set, temporary_mask.old_blocked()) {
                SignalWaitObservation::Accepted(sig) => {
                    temporary_mask.restore();
                    return Ok(sig);
                }
                SignalWaitObservation::Delivered(delivered) => {
                    let handler_depth = thr.signal_handler_depth();
                    complete_signal_delivery(thr, uctx, delivered);
                    let handler_entered = thr.signal_handler_depth() > handler_depth;
                    temporary_mask.finish_delivery(handler_entered);
                    // The handler frame owns EINTR when one was published; a
                    // stop/continue delivery returns EINTR directly. Terminal
                    // delivery has published exit state and must not reblock.
                    return Err(AxError::Interrupted);
                }
                SignalWaitObservation::None if thr.pending_exit() => {
                    temporary_mask.restore();
                    return Err(AxError::Interrupted);
                }
                SignalWaitObservation::None if wake == SignalWaitWake::TimedOut => {
                    temporary_mask.restore();
                    return Err(AxError::WouldBlock);
                }
                SignalWaitObservation::None => {
                    if let SignalWaitWake::Failed(error) = wake {
                        temporary_mask.restore();
                        return Err(error);
                    }
                    wake = block.wait();
                }
            }
        }
    })?;
    acknowledge_posix_timer_signal(&thr.proc_data, &sig);

    if let Some(info) = info.nullable() {
        info.vm_write(sig.0)?;
    }

    Ok(sig.signo() as _)
}

pub fn sys_rt_sigsuspend(
    uctx: &mut UserContext,
    set: *const SignalSet,
    sigsetsize: usize,
) -> AxResult<isize> {
    check_sigset_size(sigsetsize)?;

    let curr = current();
    let thr = curr.as_thread();

    let set = unsafe { set.vm_read_uninit()?.assume_init() };
    let old_blocked = thr.signal.blocked();
    let mut temporary_mask = TemporarySignalMask::replace(&thr.signal, old_blocked, set);

    // sigsuspend always returns -EINTR when a signal is caught
    // We set this in uctx before check_signals so it's saved in SignalFrame
    uctx.set_retval(-LinuxError::EINTR.code() as usize);

    with_proc_state_hint(ProcStateHint::Interruptible, || {
        let mut block = SignalWaitBlock::new(None);
        let mut wake = SignalWaitWake::Initial;
        loop {
            let step = signal_wait_step(
                wake,
                || None::<()>,
                || {
                    if thr.pending_exit() {
                        return Some(false);
                    }
                    let handler_depth = thr.signal_handler_depth();
                    check_signals(thr, uctx, Some(temporary_mask.old_blocked()))
                        .then(|| thr.signal_handler_depth() > handler_depth)
                },
            );
            match step {
                SignalWaitStep::Delivered {
                    handler_entered: true,
                } => {
                    temporary_mask.hand_off_to_handler();
                    return Ok(());
                }
                SignalWaitStep::Delivered {
                    handler_entered: false,
                } if thr.pending_exit() => {
                    temporary_mask.restore();
                    return Ok(());
                }
                SignalWaitStep::Delivered {
                    handler_entered: false,
                }
                | SignalWaitStep::Block => {
                    // Default stop/continue actions do not complete
                    // sigsuspend. After a stop is resumed, keep waiting with
                    // the temporary mask until a handler is actually entered.
                    if let SignalWaitWake::Failed(error) = wake {
                        return Err(error);
                    }
                    wake = block.wait();
                }
                SignalWaitStep::Failed(error) => return Err(error),
                SignalWaitStep::Accepted(()) | SignalWaitStep::TimedOut => {
                    return Err(AxError::BadState);
                }
            }
        }
    })?;

    // sigsuspend always returns -EINTR
    Err(AxError::Interrupted)
}

fn prepare_sigaltstack_update(
    current_stack: &SignalStack,
    current_sp: usize,
    candidate: SignalStack,
) -> AxResult<SignalStack> {
    let valid_flags = SS_DISABLE as u32;
    if candidate.flags & !valid_flags != 0 || candidate.flags & SS_ONSTACK as u32 != 0 {
        return Err(AxError::InvalidInput);
    }
    if current_stack.contains_sp(current_sp) {
        return Err(AxError::OperationNotPermitted);
    }
    if candidate.flags == SS_DISABLE as u32 {
        return Ok(SignalStack::default());
    }
    if candidate.size < MINSIGSTKSZ as usize {
        return Err(AxError::NoMemory);
    }
    if candidate.sp.checked_add(candidate.size).is_none() {
        return Err(AxError::InvalidInput);
    }
    Ok(candidate)
}

pub fn sys_sigaltstack(
    uctx: &UserContext,
    ss: *const SignalStack,
    old_ss: *mut SignalStack,
) -> AxResult<isize> {
    let curr = current();
    let sig = &curr.as_thread().signal;
    let current_stack = sig.stack();

    // Read and validate the proposed state before writing `old_ss`. Besides
    // keeping publication last, this gives overlapping `ss == old_ss` the
    // Linux ordering: the input value is captured before the old state is
    // copied back to the same userspace address.
    let prepared = if let Some(ss) = ss.nullable() {
        let candidate = unsafe { ss.vm_read_uninit()?.assume_init() };
        Some(prepare_sigaltstack_update(
            &current_stack,
            uctx.sp(),
            candidate,
        )?)
    } else {
        None
    };

    if let Some(old_ss) = old_ss.nullable() {
        let mut visible_stack = current_stack.clone();
        visible_stack.flags = current_stack.flags_at(uctx.sp());
        old_ss.vm_write(visible_stack)?;
    }

    if let Some(prepared) = prepared {
        sig.set_stack(prepared);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use core::{cell::Cell, time::Duration};

    use axerrno::AxError;
    use axtask::future::TimerRegistrationError;
    use linux_raw_sys::general::{MINSIGSTKSZ, SI_TKILL, SI_USER, SS_DISABLE, SS_ONSTACK};
    use starry_signal::{SignalInfo, SignalSet, SignalStack, Signo};

    use super::{
        ProcessSignalPostHook, SignalTargetAggregation, SignalTargetAuthorizationError,
        SignalTargetResultReducer, SignalWaitStep, SignalWaitWake, complete_specific_thread_signal,
        exited_leader_identity_matches, parse_signo, prepare_sigaltstack_update,
        process_signal_post_hook, queued_signal_required, reduce_process_signal_delivery_result,
        sanitize_synchronous_wait_set, signal_wait_deadline, signal_wait_step,
    };

    fn reduce_target_results(
        aggregation: SignalTargetAggregation,
        results: impl IntoIterator<Item = Result<(), AxError>>,
    ) -> Result<(), AxError> {
        let mut reducer = SignalTargetResultReducer::new(aggregation);
        for result in results {
            reducer.record(result);
        }
        reducer.finish()
    }

    #[test]
    fn signal_numbers_are_range_checked_before_narrowing() {
        assert!(parse_signo(0).is_err());
        assert_eq!(parse_signo(1).unwrap(), Signo::SIGHUP);
        assert_eq!(parse_signo(64).unwrap(), Signo::SIGRT32);
        assert!(parse_signo(65).is_err());
        assert!(parse_signo(257).is_err());
        assert!(parse_signo(u32::MAX).is_err());
    }

    #[test]
    fn synchronous_wait_set_never_selects_kill_or_stop() {
        let mut requested = SignalSet::default();
        requested.add(Signo::SIGKILL);
        requested.add(Signo::SIGSTOP);
        requested.add(Signo::SIGUSR1);

        let sanitized = sanitize_synchronous_wait_set(requested);
        assert!(!sanitized.has(Signo::SIGKILL));
        assert!(!sanitized.has(Signo::SIGSTOP));
        assert!(sanitized.has(Signo::SIGUSR1));
    }

    #[test]
    fn synchronous_signal_acceptance_wins_over_delivery_and_timeout() {
        let delivery_checked = Cell::new(false);
        let step = signal_wait_step(
            SignalWaitWake::TimedOut,
            || Some(17),
            || {
                delivery_checked.set(true);
                Some(true)
            },
        );

        assert_eq!(step, SignalWaitStep::Accepted(17));
        assert!(!delivery_checked.get());
    }

    #[test]
    fn asynchronous_delivery_wins_over_an_elapsed_timeout() {
        assert_eq!(
            signal_wait_step(SignalWaitWake::TimedOut, || None::<u8>, || Some(true),),
            SignalWaitStep::Delivered {
                handler_entered: true,
            }
        );
    }

    #[test]
    fn final_signal_observation_wins_over_internal_wait_failure() {
        assert_eq!(
            signal_wait_step(
                SignalWaitWake::Failed(AxError::ResourceBusy),
                || Some(23_u8),
                || Some(true),
            ),
            SignalWaitStep::Accepted(23)
        );
        assert_eq!(
            signal_wait_step(
                SignalWaitWake::Failed(AxError::BadState),
                || None::<u8>,
                || Some(false),
            ),
            SignalWaitStep::Delivered {
                handler_entered: false,
            }
        );
        assert_eq!(
            signal_wait_step(
                SignalWaitWake::Failed(AxError::ResourceBusy),
                || None::<u8>,
                || None,
            ),
            SignalWaitStep::Failed(AxError::ResourceBusy)
        );
    }

    #[test]
    fn stale_interrupt_rearms_but_final_empty_recheck_times_out() {
        assert_eq!(
            signal_wait_step(SignalWaitWake::Interrupted, || None::<u8>, || None,),
            SignalWaitStep::Block
        );
        assert_eq!(
            signal_wait_step(SignalWaitWake::TimedOut, || None::<u8>, || None),
            SignalWaitStep::TimedOut
        );
    }

    #[test]
    fn relative_signal_timeout_becomes_one_checked_absolute_deadline() {
        assert_eq!(
            signal_wait_deadline(Duration::from_secs(7), Some(Duration::from_millis(250)),),
            Ok(Some(Duration::from_millis(7_250)))
        );
        assert_eq!(
            signal_wait_deadline(Duration::MAX, Some(Duration::from_nanos(1))),
            Err(TimerRegistrationError::DeadlineOverflow)
        );
    }

    #[test]
    fn realtime_queue_policy_matches_linux_siginfo_classification() {
        assert!(!queued_signal_required(&None));
        assert!(!queued_signal_required(&Some(SignalInfo::new_user(
            Signo::SIGTERM,
            SI_TKILL,
            1,
        ))));
        assert!(!queued_signal_required(&Some(SignalInfo::new_user(
            Signo::SIGRTMIN,
            SI_USER as i32,
            1,
        ))));
        assert!(queued_signal_required(&Some(SignalInfo::new_user(
            Signo::SIGRTMIN,
            SI_TKILL,
            1,
        ))));
    }

    #[test]
    fn process_group_signal_reducer_keeps_the_first_success_sticky() {
        assert_eq!(
            reduce_target_results(
                SignalTargetAggregation::ProcessGroup,
                [
                    Err(AxError::OperationNotPermitted),
                    Ok(()),
                    Err(AxError::InvalidInput),
                ],
            ),
            Ok(())
        );
        assert_eq!(
            reduce_target_results(
                SignalTargetAggregation::ProcessGroup,
                [
                    Err(AxError::OperationNotPermitted),
                    Err(AxError::InvalidInput),
                ],
            ),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            reduce_target_results(SignalTargetAggregation::ProcessGroup, core::iter::empty(),),
            Err(AxError::NoSuchProcess)
        );
    }

    #[test]
    fn broadcast_signal_reducer_matches_linux_historical_eperm_rule() {
        assert_eq!(
            reduce_target_results(
                SignalTargetAggregation::Broadcast,
                [
                    Err(AxError::OperationNotPermitted),
                    Err(AxError::OperationNotPermitted),
                ],
            ),
            Ok(())
        );
        assert_eq!(
            reduce_target_results(
                SignalTargetAggregation::Broadcast,
                [Ok(()), Err(AxError::InvalidInput)],
            ),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            reduce_target_results(
                SignalTargetAggregation::Broadcast,
                [
                    Err(AxError::InvalidInput),
                    Err(AxError::OperationNotPermitted),
                ],
            ),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            reduce_target_results(SignalTargetAggregation::Broadcast, core::iter::empty(),),
            Err(AxError::NoSuchProcess)
        );
    }

    #[test]
    fn specific_thread_signal_reducer_only_absorbs_post_authorization_esrch() {
        assert_eq!(
            complete_specific_thread_signal(Err(AxError::NoSuchProcess)),
            Ok(())
        );
        assert_eq!(
            complete_specific_thread_signal(Err(AxError::WouldBlock)),
            Err(AxError::WouldBlock)
        );
        assert_eq!(complete_specific_thread_signal(Ok(())), Ok(()));
    }

    #[test]
    fn process_signal_post_hook_distinguishes_probe_handoff_and_named_task_loss() {
        assert_eq!(
            process_signal_post_hook(false, None, false),
            ProcessSignalPostHook::CompleteProbe
        );
        assert_eq!(
            process_signal_post_hook(true, None, true),
            ProcessSignalPostHook::Deliver
        );
        assert_eq!(
            process_signal_post_hook(true, None, false),
            ProcessSignalPostHook::RetryHandoff
        );
        assert_eq!(
            process_signal_post_hook(true, Some(true), false),
            ProcessSignalPostHook::Deliver
        );
        assert_eq!(
            process_signal_post_hook(true, Some(false), true),
            ProcessSignalPostHook::NamedTaskGone
        );
    }

    #[test]
    fn process_signal_esrch_only_completes_for_the_exact_published_zombie() {
        assert_eq!(
            reduce_process_signal_delivery_result(Err(AxError::NoSuchProcess), true, false),
            Ok(())
        );
        assert_eq!(
            reduce_process_signal_delivery_result(Err(AxError::NoSuchProcess), false, false),
            Err(AxError::NoSuchProcess)
        );
        assert_eq!(
            reduce_process_signal_delivery_result(Err(AxError::NoSuchProcess), true, true),
            Err(AxError::NoSuchProcess)
        );
        assert_eq!(
            reduce_process_signal_delivery_result(Err(AxError::WouldBlock), true, false),
            Err(AxError::WouldBlock)
        );
        assert_eq!(
            reduce_process_signal_delivery_result(Ok(()), false, false),
            Ok(())
        );
    }

    #[test]
    fn exited_thread_fallback_only_names_the_retained_group_leader_identity() {
        assert!(exited_leader_identity_matches(None, 41, 41));
        assert!(exited_leader_identity_matches(Some(41), 41, 41));
        assert!(!exited_leader_identity_matches(None, 42, 41));
        assert!(!exited_leader_identity_matches(Some(40), 41, 41));
    }

    #[test]
    fn policy_esrch_never_becomes_a_missing_target_fallback() {
        let policy = SignalTargetAuthorizationError::Failed(AxError::NoSuchProcess);
        assert_ne!(policy, SignalTargetAuthorizationError::MissingTarget);
        assert_eq!(policy.into_ax_error(), AxError::NoSuchProcess);
        assert_eq!(
            SignalTargetAuthorizationError::MissingTarget.into_ax_error(),
            AxError::NoSuchProcess
        );
    }

    #[test]
    fn sigaltstack_update_rejects_onstack_mutation_and_wrapping_ranges() {
        let current = SignalStack {
            sp: 0x1000,
            flags: 0,
            size: 0x2000,
        };
        let replacement = SignalStack {
            sp: 0x8000,
            flags: 0,
            size: MINSIGSTKSZ as usize,
        };
        assert_eq!(
            prepare_sigaltstack_update(&current, 0x1800, replacement.clone()).err(),
            Some(AxError::OperationNotPermitted)
        );
        assert_eq!(
            prepare_sigaltstack_update(
                &current,
                0x4000,
                SignalStack {
                    sp: usize::MAX - 8,
                    flags: 0,
                    size: MINSIGSTKSZ as usize,
                },
            )
            .err(),
            Some(AxError::InvalidInput)
        );
        assert_eq!(
            prepare_sigaltstack_update(
                &current,
                0x4000,
                SignalStack {
                    sp: 0x8000,
                    flags: SS_ONSTACK,
                    size: MINSIGSTKSZ as usize,
                },
            )
            .err(),
            Some(AxError::InvalidInput)
        );
        assert!(
            prepare_sigaltstack_update(
                &current,
                0x4000,
                SignalStack {
                    sp: 1,
                    flags: SS_DISABLE,
                    size: 1,
                },
            )
            .unwrap()
            .disabled()
        );
    }
}
