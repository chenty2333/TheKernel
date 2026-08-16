use alloc::sync::Arc;
use core::future::pending;

use axerrno::{AxError, AxResult, LinuxError};
use axhal::{
    time::{TimeValue, wall_time},
    uspace::{UserContext, UserReturnHookAction},
};
use axtask::{
    AxTaskRef, current,
    future::{self, block_on},
};
use linux_raw_sys::general::{
    MINSIGSTKSZ, SI_TKILL, SI_USER, SIG_BLOCK, SIG_SETMASK, SIG_UNBLOCK, SS_DISABLE, SS_ONSTACK,
    siginfo, timespec,
};
use thekernel_linux_process_adapter::Pid;
use thekernel_linux_signal::{
    RawSignalAction, SignalAction, SignalInfo, SignalSet, SignalStack, SignalStackRestoreError,
    Signo,
    api::{SignalDeliveryPreflight, SignalFrame, SignalWaitObservation, ThreadSignalManager},
};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmMutPtr, VmPtr};

use crate::{
    mm::{AddressSpaceUserMemory, map_usercopy_error},
    task::{
        AsThread, Cred, ProcStateHint, Process, ProcessData, SignalDeliveryScope, SignalNumber,
        SignalSecurityOperation, SignalSecuritySource, SignalTargetKind, Thread,
        acknowledge_posix_timer_signal, check_current_pinned_process_identity_signal_access,
        check_current_pinned_process_signal_access, check_current_pinned_thread_signal_access,
        check_current_zombie_signal_access, check_signals, complete_signal_delivery,
        force_rseq_fault_signal_current_thread, force_signal_current_thread,
        generate_signal_for_exited_leader, get_process_data, get_process_group,
        get_process_including_zombie, get_visible_task, process_domain, process_error,
        send_authorized_signal_thread_inner, send_queued_signal_to_process_data_with_credential,
        send_signal_to_process_data_with_credential, terminate_rseq_fault_current_thread,
        with_proc_state_hint,
    },
    time::TimeValueLike,
};

pub(crate) fn check_sigset_size(size: usize) -> AxResult<()> {
    if size != size_of::<SignalSet>() {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn check_sigpending_size(size: usize) -> AxResult<()> {
    if size > size_of::<SignalSet>() {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn pending_mask_for_sigpending(pending: SignalSet, blocked: SignalSet) -> SignalSet {
    pending & blocked
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

pub fn sys_rt_sigprocmask<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
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
    let new = if let Some(set) = VmPtr::nullable(set) {
        let set = unsafe {
            VmPtr::vm_read_uninit(set, memory)
                .map_err(map_usercopy_error)?
                .assume_init()
        };
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

    if let Some(oldset) = VmPtr::nullable(oldset) {
        // SAFETY: SignalSet is repr(transparent) over a u64, so every byte
        // of the value is initialized and safe to copy to userspace.
        unsafe { VmMutPtr::vm_write_unchecked(oldset, memory, old).map_err(map_usercopy_error)? }
    }

    Ok(0)
}

pub fn sys_rt_sigaction<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
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

    let new_action = if !act.is_null() {
        let mut action: SignalAction = RawSignalAction::read_from_user(memory, act)
            .map_err(map_usercopy_error)?
            .into();
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
        proc_data.signal.action(signo)
    };

    // Linux commits the new action before copying the previous one out. If
    // this user copy faults, the action transition and required queue flush
    // therefore remain visible.
    if !oldact.is_null() {
        RawSignalAction::from(old_action)
            .write_to_user(memory, oldact)
            .map_err(map_usercopy_error)?;
    }
    Ok(0)
}

pub fn sys_rt_sigpending<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    set: *mut SignalSet,
    sigsetsize: usize,
) -> AxResult<isize> {
    check_sigpending_size(sigsetsize)?;
    if sigsetsize == 0 {
        return Ok(0);
    }
    let curr = current();
    let thread = curr.as_thread();
    // `pending` is the canonical union of thread-private and process-shared
    // queues, while `blocked` is the current thread's mask.  Take both through
    // the signal manager accessors before applying the Linux pending-mask
    // intersection.
    let pending = pending_mask_for_sigpending(thread.signal.pending(), thread.signal.blocked());
    // SAFETY: SignalSet is repr(transparent) over a u64, so every byte of
    // the value is initialized and safe to copy to userspace. The Linux
    // interface accepts a shorter pending-mask size and copies exactly that
    // many leading bytes.
    let bytes = unsafe {
        core::slice::from_raw_parts((&pending as *const SignalSet).cast::<u8>(), sigsetsize)
    };
    memory
        .write_bytes(set as usize, bytes)
        .map_err(map_usercopy_error)?;
    Ok(0)
}

fn make_siginfo(signo: u32, code: i32) -> AxResult<Option<SignalInfo>> {
    if signo == 0 {
        return Ok(None);
    }
    let signo = parse_signo(signo)?;
    let curr = current();
    let thread = curr.as_thread();
    let credential = thread.current_cred();
    // Linux's generated SI_USER/SI_TKILL records carry current_uid(), i.e.
    // the sender's real UID, rendered in the sender's user namespace.
    let uid = credential.user_ns().from_kuid_munged(credential.ids().ruid);
    Ok(Some(SignalInfo::new_user(
        signo,
        code,
        thread.proc_data.proc.pid(),
        uid,
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
    check_current_zombie_signal_access(process, &snapshot.credential, operation)?;
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
            SignalTargetAggregation::Broadcast => self.broadcast_result.unwrap_or({
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

fn make_queue_signal_info<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    target_tid: Pid,
    signo: u32,
    sig: *const SignalInfo,
) -> AxResult<QueuedSignalRequest> {
    let mut sig = unsafe {
        VmPtr::vm_read_uninit(sig, memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
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

pub fn sys_rt_sigqueueinfo<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pid: Pid,
    signo: u32,
    sig: *const SignalInfo,
) -> AxResult<isize> {
    let request = make_queue_signal_info(memory, pid, signo, sig)?;
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

pub fn sys_rt_tgsigqueueinfo<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    tgid: i32,
    tid: i32,
    signo: u32,
    sig: *const SignalInfo,
) -> AxResult<isize> {
    if tgid <= 0 || tid <= 0 {
        return Err(AxError::InvalidInput);
    }

    let request = make_queue_signal_info(memory, tid as Pid, signo, sig)?;
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

const SIGNAL_PC_ALIGNMENT: usize = 1;
const SIGNAL_SP_ALIGNMENT: usize = 1;

fn valid_signal_user_address(address: usize, alignment: usize) -> bool {
    let end = crate::config::USER_SPACE_BASE + crate::config::USER_SPACE_SIZE;
    address >= crate::config::USER_SPACE_BASE && address < end && address.is_multiple_of(alignment)
}

fn reject_bad_sigreturn(reason: &str) -> AxResult<isize> {
    warn!("rejecting invalid rt_sigreturn frame: {reason}");
    force_signal_current_thread(SignalInfo::new_kernel(Signo::SIGSEGV));
    Ok(0)
}

fn validate_sigreturn_stack(
    configured: &SignalStack,
    syscall_sp: usize,
    candidate: &SignalStack,
) -> Result<(), SignalStackRestoreError> {
    if configured.contains_sp(syscall_sp) {
        return Err(SignalStackRestoreError::ActiveStack);
    }
    if candidate.disabled() {
        return Ok(());
    }
    if candidate.size < MINSIGSTKSZ as usize {
        return Err(SignalStackRestoreError::TooSmall);
    }
    let user_start = crate::config::USER_SPACE_BASE;
    let user_end = user_start
        .checked_add(crate::config::USER_SPACE_SIZE)
        .ok_or(SignalStackRestoreError::InvalidAddress)?;
    let candidate_end = candidate
        .sp
        .checked_add(candidate.size)
        .ok_or(SignalStackRestoreError::InvalidAddress)?;
    if candidate.sp < user_start || candidate_end > user_end {
        return Err(SignalStackRestoreError::InvalidAddress);
    }
    Ok(())
}

pub fn sys_rt_sigreturn<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    uctx: &mut UserContext,
) -> AxResult<isize> {
    let curr = current();
    let thr = curr.as_thread();

    if !thr.in_signal_handler() {
        return reject_bad_sigreturn("no active signal handler");
    }

    let frame = match SignalFrame::read_from_user(memory, uctx.sp() as *const SignalFrame) {
        Ok(frame) => frame,
        Err(_) => return reject_bad_sigreturn("frame copy-in fault"),
    };

    let prepared = match thr.signal.prepare_restore(
        uctx,
        frame,
        |pc| valid_signal_user_address(pc, SIGNAL_PC_ALIGNMENT),
        |sp| valid_signal_user_address(sp, SIGNAL_SP_ALIGNMENT),
        validate_sigreturn_stack,
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
    Interrupted,
    TimedOut,
    Failed(AxError),
}

#[derive(Debug, Eq, PartialEq)]
enum SignalWaitStep<T> {
    Accepted(T),
    Delivered,
    Retry,
    Fault,
    Replaced,
    Fatal,
    Block,
    TimedOut,
    Failed(AxError),
}

/// Resolves the observation after one `rt_sigtimedwait` block session.
///
/// Linux performs the final dequeue of the selected set regardless of why the
/// scheduler returned. Only a genuine signal interruption may then run the
/// asynchronous-delivery path. In particular, an unrelated signal published
/// after the timer elapsed cannot turn `EAGAIN` into `EINTR`.
fn sigtimedwait_post_wait_step<T>(
    wake: SignalWaitWake,
    accept_selected: impl FnOnce() -> Option<T>,
    observe_interrupted: impl FnOnce() -> SignalWaitStep<T>,
) -> SignalWaitStep<T> {
    match wake {
        SignalWaitWake::Interrupted => observe_interrupted(),
        SignalWaitWake::TimedOut => accept_selected()
            .map(SignalWaitStep::Accepted)
            .unwrap_or(SignalWaitStep::TimedOut),
        SignalWaitWake::Failed(error) => accept_selected()
            .map(SignalWaitStep::Accepted)
            .unwrap_or(SignalWaitStep::Failed(error)),
    }
}

/// Owns Linux's `real_blocked` transaction for one `rt_sigtimedwait`.
///
/// Each wait session temporarily unblocks the selected set. The owner restores
/// the real mask before the final selected dequeue and before any unrelated
/// handler frame is published, matching `do_sigtimedwait()`'s lock ordering.
struct SigtimedwaitMask<'a> {
    signal: &'a ThreadSignalManager,
    old_blocked: SignalSet,
    waited: SignalSet,
    active: bool,
}

impl<'a> SigtimedwaitMask<'a> {
    fn new(signal: &'a ThreadSignalManager, waited: SignalSet) -> Self {
        Self {
            signal,
            old_blocked: signal.blocked(),
            waited,
            active: false,
        }
    }

    fn old_blocked(&self) -> SignalSet {
        self.old_blocked
    }

    fn activate(&mut self) {
        debug_assert!(!self.active);
        // Preserve the real mask before exposing the temporary one so an
        // ignored selected signal which was originally blocked remains
        // queueable while the synchronous wait is active.
        self.signal.set_real_blocked(Some(self.old_blocked));
        self.signal.set_blocked(self.old_blocked & !self.waited);
        self.active = true;
    }

    fn restore(&mut self) {
        if self.active {
            // Restore the visible mask before removing the original-mask
            // sidecar, closing the inverse race of `activate` above.
            self.signal.set_blocked(self.old_blocked);
            self.signal.set_real_blocked(None);
            self.active = false;
        }
    }
}

impl Drop for SigtimedwaitMask<'_> {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Owns the saved-mask handoff for one `rt_sigsuspend`.
///
/// Unlike `rt_sigtimedwait`, Linux does not populate `real_blocked` here: an
/// ignored signal unblocked by the temporary suspend mask must remain ignored.
/// A caught handler inherits the temporary visible mask and owns restoration
/// of `old_blocked` through its userspace signal frame.
struct SigsuspendMask<'a> {
    signal: &'a ThreadSignalManager,
    old_blocked: SignalSet,
    restore_on_drop: bool,
}

impl<'a> SigsuspendMask<'a> {
    fn install(signal: &'a ThreadSignalManager, temporary: SignalSet) -> Self {
        let old_blocked = signal.set_blocked(temporary);
        Self {
            signal,
            old_blocked,
            restore_on_drop: true,
        }
    }

    fn old_blocked(&self) -> SignalSet {
        self.old_blocked
    }

    fn hand_off_to_handler(&mut self) {
        self.restore_on_drop = false;
    }

    fn restore(&mut self) {
        if self.restore_on_drop {
            self.signal.set_blocked(self.old_blocked);
            self.restore_on_drop = false;
        }
    }
}

impl Drop for SigsuspendMask<'_> {
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

/// Waits until one signal has actually entered a userspace handler.
///
/// Both `pause(2)` and `rt_sigsuspend(2)` use the same interruptible wait
/// protocol.  The caller supplies the mask which a handler frame must restore;
/// the visible mask itself is owned by the caller so `pause` can leave the
/// current mask untouched while `sigsuspend` temporarily replaces it.
fn wait_for_caught_signal(
    thr: &Thread,
    uctx: &mut UserContext,
    restore_blocked: SignalSet,
    on_handler: impl FnOnce(),
) -> AxResult<()> {
    with_proc_state_hint(ProcStateHint::Interruptible, || {
        let mut block = SignalWaitBlock::new(None);
        loop {
            if thr.pending_exit() {
                return Ok(());
            }

            let handler_depth = thr.signal_handler_depth();
            if check_signals(thr, uctx, Some(restore_blocked)) {
                if thr.signal_handler_depth() > handler_depth {
                    on_handler();
                    return Ok(());
                }
                if thr.pending_exit() {
                    return Ok(());
                }
                // Default stop/continue actions do not complete either wait.
                // A stopped task resumes here and keeps waiting until a
                // userspace handler is actually entered.
                continue;
            }

            match block.wait() {
                SignalWaitWake::Interrupted => {}
                SignalWaitWake::Failed(error) => return Err(error),
                SignalWaitWake::TimedOut => return Err(AxError::BadState),
            }
        }
    })
}

pub fn sys_rt_sigtimedwait<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    uctx: &mut UserContext,
    set: *const SignalSet,
    info: *mut siginfo,
    timeout: *const timespec,
    sigsetsize: usize,
) -> AxResult<isize> {
    check_sigset_size(sigsetsize)?;

    let set = sanitize_synchronous_wait_set(unsafe {
        VmPtr::vm_read_uninit(set, memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    });

    let timeout = if let Some(ts) = VmPtr::nullable(timeout) {
        let ts = unsafe {
            VmPtr::vm_read_uninit(ts, memory)
                .map_err(map_usercopy_error)?
                .assume_init()
        };
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

    // Linux checks the selected queue before installing `real_blocked`. A zero
    // timeout is a pure nonblocking dequeue: unrelated pending signals neither
    // acquire a handler frame here nor change EAGAIN into EINTR.
    let sig = if let Some(sig) = signal.dequeue_signal(&set) {
        sig
    } else if timeout.is_some_and(|duration| duration.is_zero()) {
        return Err(AxError::WouldBlock);
    } else {
        uctx.set_retval(-LinuxError::EINTR.code() as usize);
        with_proc_state_hint(ProcStateHint::Interruptible, || {
            let mut mask = SigtimedwaitMask::new(signal, set);
            let mut block = SignalWaitBlock::new(deadline);
            let mut retry_delivery = false;
            loop {
                mask.activate();

                // Close the initial-dequeue-to-unblock gap and every later
                // restore-to-reactivate gap. A selected signal published while
                // the old mask was visible may not have requested a wake.
                if let Some(sig) = signal.dequeue_signal(&set) {
                    mask.restore();
                    return Ok(sig);
                }

                // A transient rseq pre-delivery rejection has already
                // requeued an asynchronously deliverable signal and may have
                // consumed its wake. Re-enter observation immediately instead
                // of sleeping with that signal still pending.
                let wake = if retry_delivery {
                    retry_delivery = false;
                    SignalWaitWake::Interrupted
                } else {
                    block.wait()
                };
                // Linux restores the real mask before its final selected
                // dequeue. This also makes an unrelated handler's visible mask
                // derive from old_blocked rather than the temporary wait mask.
                mask.restore();

                let step = sigtimedwait_post_wait_step(
                    wake,
                    || signal.dequeue_signal(&set),
                    || {
                        let saved_uctx = *uctx;
                        let aspace = thr.proc_data.aspace();
                        let mut provider = AddressSpaceUserMemory::new(aspace.clone());
                        let mut memory = UserMemoryContext::new(&mut provider);
                        match signal.observe_signal_wait_with_pre_delivery(
                            &mut memory,
                            uctx,
                            &set,
                            mask.old_blocked(),
                            |uctx, sig, _| {
                                // Resolve the current image immediately before
                                // this pre-delivery operation. The handle and
                                // UserContext passed to rseq are then the same
                                // pair for the complete nofault gate.
                                if thr.signal.take_signal_delivery_bypass(sig.signo()) {
                                    return SignalDeliveryPreflight::Proceed;
                                }
                                match thr.pre_signal_rseq_delivery(uctx, &aspace) {
                                    UserReturnHookAction::EnterUser => {
                                        SignalDeliveryPreflight::Proceed
                                    }
                                    UserReturnHookAction::Retry => SignalDeliveryPreflight::Retry,
                                    UserReturnHookAction::Fault => {
                                        if force_rseq_fault_signal_current_thread() {
                                            SignalDeliveryPreflight::Replaced
                                        } else {
                                            SignalDeliveryPreflight::Fatal
                                        }
                                    }
                                }
                            },
                        ) {
                            SignalWaitObservation::Accepted(sig) => SignalWaitStep::Accepted(sig),
                            SignalWaitObservation::Delivered(delivered) => {
                                complete_signal_delivery(thr, uctx, delivered);
                                SignalWaitStep::Delivered
                            }
                            SignalWaitObservation::Retry => {
                                *uctx = saved_uctx;
                                SignalWaitStep::Retry
                            }
                            SignalWaitObservation::Fault => {
                                *uctx = saved_uctx;
                                SignalWaitStep::Fault
                            }
                            SignalWaitObservation::Replaced => SignalWaitStep::Replaced,
                            SignalWaitObservation::Fatal => {
                                *uctx = saved_uctx;
                                terminate_rseq_fault_current_thread();
                                SignalWaitStep::Fatal
                            }
                            SignalWaitObservation::None => SignalWaitStep::Block,
                        }
                    },
                );
                match step {
                    SignalWaitStep::Accepted(sig) => return Ok(sig),
                    SignalWaitStep::Delivered => {
                        // A handler frame owns EINTR when one was published; a
                        // stop/continue delivery returns EINTR directly.
                        // Terminal delivery has published exit state and must
                        // not reblock.
                        return Err(AxError::Interrupted);
                    }
                    SignalWaitStep::Retry => {
                        retry_delivery = true;
                        continue;
                    }
                    SignalWaitStep::Replaced => {
                        retry_delivery = true;
                        continue;
                    }
                    SignalWaitStep::Fatal => return Err(AxError::Interrupted),
                    SignalWaitStep::Fault => return Err(AxError::BadAddress),
                    SignalWaitStep::Block if thr.pending_exit() => {
                        return Err(AxError::Interrupted);
                    }
                    SignalWaitStep::Block => {}
                    SignalWaitStep::TimedOut => return Err(AxError::WouldBlock),
                    SignalWaitStep::Failed(error) => return Err(error),
                }
            }
        })?
    };
    acknowledge_posix_timer_signal(&thr.proc_data, &sig);

    if let Some(info) = VmPtr::nullable(info) {
        // SignalInfo owns a fully initialized Linux siginfo record. Copy its
        // bytes through the explicit user-memory context rather than exposing
        // the canonical crate's private storage.
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (sig.as_raw() as *const siginfo).cast::<u8>(),
                size_of::<siginfo>(),
            )
        };
        memory
            .write_bytes(info as usize, bytes)
            .map_err(map_usercopy_error)?;
    }

    Ok(sig.signo() as _)
}

pub fn sys_rt_sigsuspend<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    uctx: &mut UserContext,
    set: *const SignalSet,
    sigsetsize: usize,
) -> AxResult<isize> {
    check_sigset_size(sigsetsize)?;

    let curr = current();
    let thr = curr.as_thread();

    let set = unsafe {
        VmPtr::vm_read_uninit(set, memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
    let mut suspended_mask = SigsuspendMask::install(&thr.signal, set);

    // sigsuspend always returns -EINTR when a signal is caught
    // We set this in uctx before check_signals so it's saved in SignalFrame
    uctx.set_retval(-LinuxError::EINTR.code() as usize);

    let old_blocked = suspended_mask.old_blocked();
    wait_for_caught_signal(thr, uctx, old_blocked, || {
        suspended_mask.hand_off_to_handler();
    })?;

    // sigsuspend always returns -EINTR
    Err(AxError::Interrupted)
}

/// Implements Linux x86_64 `pause(2)`.
///
/// `pause` waits with the current signal mask in force. It completes only
/// after a signal handler has been entered and always reports `EINTR`, even
/// when that handler was installed with `SA_RESTART`; ignored, blocked,
/// stop/continue, and fatal signals never manufacture a successful return.
pub fn sys_pause(uctx: &mut UserContext) -> AxResult<isize> {
    let curr = current();
    let thr = curr.as_thread();
    let restore_blocked = thr.signal.blocked();

    // The return value must already be present in the saved context if a
    // handler frame is published while pause is sleeping.
    uctx.set_retval(-LinuxError::EINTR.code() as usize);
    wait_for_caught_signal(thr, uctx, restore_blocked, || {})?;

    // pause(2) is never restartable, including for SA_RESTART handlers.
    Err(AxError::Interrupted)
}

fn prepare_sigaltstack_update(
    current_stack: &SignalStack,
    current_sp: usize,
    candidate: SignalStack,
) -> AxResult<SignalStack> {
    let valid_flags = SS_DISABLE;
    if candidate.flags & !valid_flags != 0 || candidate.flags & SS_ONSTACK != 0 {
        return Err(AxError::InvalidInput);
    }
    if current_stack.contains_sp(current_sp) {
        return Err(AxError::OperationNotPermitted);
    }
    if candidate.flags == SS_DISABLE {
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

pub fn sys_sigaltstack<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
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
    let prepared = if let Some(ss) = VmPtr::nullable(ss) {
        let candidate = unsafe {
            VmPtr::vm_read_uninit(ss, memory)
                .map_err(map_usercopy_error)?
                .assume_init()
        };
        Some(prepare_sigaltstack_update(
            &current_stack,
            uctx.sp(),
            candidate,
        )?)
    } else {
        None
    };

    if let Some(old_ss) = VmPtr::nullable(old_ss) {
        let mut visible_stack = current_stack;
        visible_stack.flags = current_stack.flags_at(uctx.sp());
        // SAFETY: SignalStack::new/default construction initializes its
        // explicit ABI padding, and the manager returns a fully initialized
        // value before this copyout.
        unsafe {
            VmMutPtr::vm_write_unchecked(old_ss, memory, visible_stack)
                .map_err(map_usercopy_error)?
        }
    }

    if let Some(prepared) = prepared {
        sig.set_stack(prepared);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use core::{cell::Cell, mem::size_of, time::Duration};

    use axerrno::AxError;
    use axtask::future::TimerRegistrationError;
    use linux_raw_sys::general::{MINSIGSTKSZ, SI_TKILL, SI_USER, SS_DISABLE, SS_ONSTACK};
    use thekernel_linux_signal::{
        RawSignalAction, SignalAction, SignalActionFlags, SignalDisposition, SignalInfo, SignalSet,
        SignalStack, Signo,
    };

    use super::{
        ProcessSignalPostHook, SignalTargetAggregation, SignalTargetAuthorizationError,
        SignalTargetResultReducer, SignalWaitStep, SignalWaitWake, check_sigpending_size,
        check_sigset_size, complete_specific_thread_signal, exited_leader_identity_matches,
        parse_signo, pending_mask_for_sigpending, prepare_sigaltstack_update,
        process_signal_post_hook, queued_signal_required, reduce_process_signal_delivery_result,
        sanitize_synchronous_wait_set, signal_wait_deadline, sigtimedwait_post_wait_step,
    };

    #[test]
    fn signal_set_size_rules_match_each_linux_syscall_contract() {
        let native = size_of::<SignalSet>();
        assert!(check_sigset_size(native).is_ok());
        assert!(check_sigset_size(0).is_err());
        assert!(check_sigset_size(native + 1).is_err());

        assert!(check_sigpending_size(0).is_ok());
        assert!(check_sigpending_size(native).is_ok());
        assert!(check_sigpending_size(native + 1).is_err());
    }

    #[test]
    fn sigpending_only_reports_blocked_pending_signals() {
        let mut pending = SignalSet::default();
        pending.add(Signo::SIGUSR1);
        pending.add(Signo::SIGUSR2);

        let mut blocked = SignalSet::default();
        blocked.add(Signo::SIGUSR2);
        blocked.add(Signo::SIGTERM);

        let visible = pending_mask_for_sigpending(pending, blocked);
        assert!(!visible.has(Signo::SIGUSR1));
        assert!(visible.has(Signo::SIGUSR2));
        assert!(!visible.has(Signo::SIGTERM));
    }

    #[test]
    fn canonical_sigaction_raw_roundtrip_preserves_record_semantics() {
        let mut mask = SignalSet::default();
        mask.add(Signo::SIGUSR1);
        mask.add(Signo::SIGRT32);
        let action = SignalAction {
            flags: SignalActionFlags::SIGINFO | SignalActionFlags::RESTORER,
            mask,
            disposition: SignalDisposition::Handler(0x1234_5678),
            restorer: Some(0x8765_4321),
        };

        let raw = RawSignalAction::from(action);
        let roundtrip = SignalAction::from(raw);
        assert_eq!(roundtrip.flags.bits(), action.flags.bits());
        assert!(roundtrip.mask.has(Signo::SIGUSR1));
        assert!(roundtrip.mask.has(Signo::SIGRT32));
        assert!(matches!(
            roundtrip.disposition,
            SignalDisposition::Handler(0x1234_5678)
        ));
        assert_eq!(roundtrip.restorer, action.restorer);
    }

    #[test]
    fn canonical_signal_set_and_stack_records_are_explicitly_initialized() {
        let mut set = SignalSet::default();
        set.add(Signo::SIGKILL);
        set.add(Signo::SIGSTOP);
        set.add(Signo::SIGUSR1);
        assert!(set.has(Signo::SIGKILL));
        assert!(set.has(Signo::SIGSTOP));
        assert!(set.has(Signo::SIGUSR1));

        let stack = SignalStack::new(0x8000, 0, MINSIGSTKSZ as usize);
        assert_eq!(stack, SignalStack::new(0x8000, 0, MINSIGSTKSZ as usize));
    }

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
    fn final_selected_signal_wins_over_elapsed_timeout() {
        let interrupted_observation_checked = Cell::new(false);
        let step = sigtimedwait_post_wait_step(
            SignalWaitWake::TimedOut,
            || Some(17),
            || {
                interrupted_observation_checked.set(true);
                SignalWaitStep::Delivered
            },
        );

        assert_eq!(step, SignalWaitStep::Accepted(17));
        assert!(!interrupted_observation_checked.get());
    }

    #[test]
    fn elapsed_timeout_never_observes_unrelated_async_delivery() {
        let interrupted_observation_checked = Cell::new(false);
        assert_eq!(
            sigtimedwait_post_wait_step(
                SignalWaitWake::TimedOut,
                || None::<u8>,
                || {
                    interrupted_observation_checked.set(true);
                    SignalWaitStep::Delivered
                },
            ),
            SignalWaitStep::TimedOut
        );
        assert!(!interrupted_observation_checked.get());
    }

    #[test]
    fn final_selected_observation_wins_over_internal_wait_failure() {
        let interrupted_observation_checked = Cell::new(false);
        assert_eq!(
            sigtimedwait_post_wait_step(
                SignalWaitWake::Failed(AxError::ResourceBusy),
                || Some(23_u8),
                || {
                    interrupted_observation_checked.set(true);
                    SignalWaitStep::Delivered
                },
            ),
            SignalWaitStep::Accepted(23)
        );
        assert!(!interrupted_observation_checked.get());
        assert_eq!(
            sigtimedwait_post_wait_step(
                SignalWaitWake::Failed(AxError::ResourceBusy),
                || None::<u8>,
                || SignalWaitStep::Delivered,
            ),
            SignalWaitStep::Failed(AxError::ResourceBusy)
        );
    }

    #[test]
    fn genuine_interrupt_uses_combined_selected_first_observation() {
        assert_eq!(
            sigtimedwait_post_wait_step(
                SignalWaitWake::Interrupted,
                || panic!("interrupted waits must use the combined observation"),
                || SignalWaitStep::Accepted(31_u8),
            ),
            SignalWaitStep::Accepted(31)
        );
        assert_eq!(
            sigtimedwait_post_wait_step(
                SignalWaitWake::Interrupted,
                || panic!("interrupted waits must use the combined observation"),
                || SignalWaitStep::<u8>::Block,
            ),
            SignalWaitStep::Block
        );
    }

    #[test]
    fn empty_final_recheck_times_out() {
        assert_eq!(
            sigtimedwait_post_wait_step(
                SignalWaitWake::TimedOut,
                || None::<u8>,
                || SignalWaitStep::Delivered,
            ),
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
            1000,
        ))));
        assert!(!queued_signal_required(&Some(SignalInfo::new_user(
            Signo::SIGRTMIN,
            SI_USER as i32,
            1,
            1000,
        ))));
        assert!(queued_signal_required(&Some(SignalInfo::new_user(
            Signo::SIGRTMIN,
            SI_TKILL,
            1,
            1000,
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
        let current = SignalStack::new(0x1000, 0, 0x2000);
        let replacement = SignalStack::new(0x8000, 0, MINSIGSTKSZ as usize);
        assert_eq!(
            prepare_sigaltstack_update(&current, 0x1800, replacement.clone()).err(),
            Some(AxError::OperationNotPermitted)
        );
        assert_eq!(
            prepare_sigaltstack_update(
                &current,
                0x4000,
                SignalStack::new(usize::MAX - 8, 0, MINSIGSTKSZ as usize),
            )
            .err(),
            Some(AxError::InvalidInput)
        );
        assert_eq!(
            prepare_sigaltstack_update(
                &current,
                0x4000,
                SignalStack::new(0x8000, SS_ONSTACK, MINSIGSTKSZ as usize),
            )
            .err(),
            Some(AxError::InvalidInput)
        );
        assert!(
            prepare_sigaltstack_update(&current, 0x4000, SignalStack::new(1, SS_DISABLE, 1),)
                .unwrap()
                .disabled()
        );
    }
}
