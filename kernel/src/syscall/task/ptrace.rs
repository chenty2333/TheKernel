use axerrno::{AxError, AxResult, LinuxError};
use axtask::{
    TaskState, current, replace_inactive_task_user_cet_state,
    snapshot_inactive_task_user_cet_state, yield_now,
};
use thekernel_linux_arch_x86_64::{ARCH_SHSTK_UNLOCK, NT_X86_SHSTK, X86ShstkRegset};
use thekernel_linux_process_adapter::Pid;
use thekernel_linux_signal::{SignalInfo, Signo};

use crate::{
    mm::{IoVec, UserMemoryCapability, map_usercopy_error},
    task::{
        AsThread, ProcessData, PtraceAccessMode, PtraceRelationshipOrigin,
        PtraceRelationshipSnapshot, PtraceReverseLink, PtraceSession, TaskParentCredentialPin,
        Thread, check_thread_ptrace_image_access_with_actor, get_task, get_visible_task,
        notify_ptrace_attach_stop, reinject_ptrace_signal,
        security::{ProcessImageSecurityRef, PtraceTracemeContext, dispatch_ptrace_traceme},
        send_signal_to_process,
    },
};

const PTRACE_TRACEME: u32 = 0;
const PTRACE_PEEKTEXT: u32 = 1;
const PTRACE_PEEKDATA: u32 = 2;
const PTRACE_PEEKUSER: u32 = 3;
const PTRACE_POKETEXT: u32 = 4;
const PTRACE_POKEDATA: u32 = 5;
const PTRACE_POKEUSER: u32 = 6;
const PTRACE_CONT: u32 = 7;
const PTRACE_KILL: u32 = 8;
const PTRACE_SINGLESTEP: u32 = 9;
const PTRACE_ATTACH: u32 = 16;
const PTRACE_DETACH: u32 = 17;
const PTRACE_SYSCALL: u32 = 24;
const PTRACE_ARCH_PRCTL: u32 = 30;
const PTRACE_SETOPTIONS: u32 = 0x4200;
const PTRACE_GETEVENTMSG: u32 = 0x4201;
const PTRACE_GETSIGINFO: u32 = 0x4202;
const PTRACE_SETSIGINFO: u32 = 0x4203;
const PTRACE_GETREGSET: u32 = 0x4204;
const PTRACE_SETREGSET: u32 = 0x4205;
const PTRACE_SEIZE: u32 = 0x4206;
const PTRACE_INTERRUPT: u32 = 0x4207;
const PTRACE_LISTEN: u32 = 0x4208;

const PTRACE_O_MASK: usize = 0x2f_ffff;

#[cfg(target_arch = "x86_64")]
const ARCH_SHSTK_FEATURES: usize = 0b11;

fn ptrace_io_error() -> AxError {
    LinuxError::EIO.into()
}

fn current_pid() -> Pid {
    current().as_thread().proc_data.proc.pid()
}

fn current_kernel_tid() -> Pid {
    current().as_thread().kernel_tid()
}

fn check_tracee(target: &ProcessData) -> AxResult<PtraceSession> {
    target
        .ptrace_session_if_traced_by(current_pid(), current_kernel_tid())
        .ok_or(AxError::NoSuchProcess)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum InactiveScan {
    Inactive,
    Retry,
    Gone,
}

fn scan_tracee_task_states(
    states: impl IntoIterator<Item = AxResult<TaskState>>,
) -> AxResult<InactiveScan> {
    let mut saw_task = false;
    let mut scan = InactiveScan::Inactive;
    for state in states {
        saw_task = true;
        match state? {
            TaskState::Blocked => {}
            TaskState::Running | TaskState::Ready => scan = InactiveScan::Retry,
            TaskState::Exited => return Ok(InactiveScan::Gone),
        }
    }
    if saw_task {
        Ok(scan)
    } else {
        Ok(InactiveScan::Gone)
    }
}

fn check_inactive_tracee(target: &ProcessData) -> AxResult<PtraceSession> {
    loop {
        let session = target
            .ptrace_inactive_session_if_traced_by(current_pid(), current_kernel_tid())
            .ok_or(AxError::NoSuchProcess)?;
        let scan = scan_tracee_task_states(target.proc.thread_ids().map(|tid| {
            get_task(tid)
                .map(|task| task.state())
                .map_err(|_| AxError::NoSuchProcess)
        }))?;
        match scan {
            InactiveScan::Gone => return Err(AxError::NoSuchProcess),
            InactiveScan::Retry => {
                // The stop publisher has already interrupted every member.
                // Yielding here is the wait_task_inactive analogue: it gives
                // Ready tasks a chance to enter wait_if_stopped and does not
                // burn a CPU in a hidden polling loop.
                yield_now();
            }
            InactiveScan::Inactive => {
                // The outer ptrace action mutex prevents a sibling tracer
                // thread from resuming the group. Revalidate the exact stop
                // generation after observing every task blocked.
                if target.ptrace_inactive_session_if_traced_by(current_pid(), current_kernel_tid())
                    == Some(session)
                {
                    return Ok(session);
                }
                return Err(AxError::NoSuchProcess);
            }
        }
    }
}

/// Runs one remote-memory operation against the image that was current after
/// this tracer's session was verified.
///
/// The owned address-space handle deliberately stays in this scope so an exec
/// publication cannot make the operation re-sample a different image between
/// validation/population and the final transfer.
fn pinned_tracee_memory(
    target: &ProcessData,
    session: PtraceSession,
) -> AxResult<UserMemoryCapability> {
    let aspace_handle = target
        .ptrace_inactive_image_if_session(session)
        .ok_or(AxError::NoSuchProcess)?;
    Ok(UserMemoryCapability::new(aspace_handle))
}

fn parse_signal(data: usize) -> AxResult<Option<SignalInfo>> {
    if data == 0 {
        return Ok(None);
    }
    let raw = u8::try_from(data).map_err(|_| AxError::InvalidInput)?;
    let signo = Signo::from_repr(raw).ok_or(AxError::InvalidInput)?;
    Ok(Some(SignalInfo::new_kernel(signo)))
}

fn interrupt_process_threads(target: &ProcessData) {
    for tid in target.proc.thread_ids() {
        if let Ok(task) = get_task(tid) {
            task.interrupt();
        }
    }
}

fn do_attach(target_thread: &Thread, seized: bool, initial_options: u32) -> AxResult<isize> {
    let curr = current();
    let ptracer = curr.as_thread();
    // Pin the exact actor before core/LSM authorization.  The same guard is
    // carried through relationship publication, so a concurrent credential
    // transition cannot turn an admitted actor into a different stored one.
    let ptracer_credential = ptracer.lock_credential_snapshot();
    let tracer_data = ptracer.proc_data.clone();
    let tracer = tracer_data.proc.pid();
    let tracer_kernel_tid = ptracer.kernel_tid();
    let target = &target_thread.proc_data;
    if target.proc.pid() == tracer {
        return Err(AxError::OperationNotPermitted);
    }
    if target.exec_in_progress() {
        return Err(AxError::OperationNotPermitted);
    }
    if !ptracer
        .landlock_domain()
        .is_ancestor_of(&target_thread.landlock_domain())
    {
        return Err(AxError::OperationNotPermitted);
    }
    let authorized_image = check_thread_ptrace_image_access_with_actor(
        ptracer,
        ptracer_credential.credential(),
        target_thread,
        PtraceAccessMode::AttachReal,
    )?;
    let reverse_link =
        tracer_data.try_prepare_ptrace_reverse_link(target.proc.pid(), tracer_kernel_tid)?;
    let publication = target.lock_ptrace_publication();
    let session = target.publish_ptrace_relationship(
        &publication,
        target_thread,
        ptracer,
        &ptracer_credential,
        PtraceRelationshipOrigin::Attach,
        ptracer_credential.credential(),
        seized,
        initial_options,
        &authorized_image,
        reverse_link,
    )?;
    if !seized && target.ptrace_stop(session, Signo::SIGSTOP as u8) {
        notify_ptrace_attach_stop(target);
        interrupt_process_threads(target);
    }
    drop(publication);
    Ok(0)
}

fn do_continue(
    target: &ProcessData,
    session: PtraceSession,
    data: usize,
    detach: bool,
) -> AxResult<PtraceContinueOutcome> {
    let curr = current();
    let tracer_data = curr.as_thread().proc_data.clone();
    let signal = parse_signal(data)?.map(|info| info.signo());
    let (resume_result, record, retired_relationship) = target
        .resume_ptrace(session, detach)
        .ok_or(AxError::NoSuchProcess)?;
    if detach {
        tracer_data.remove_ptrace_tracee(PtraceReverseLink::new(target.proc.pid(), session));
    }
    let reinjected = reinject_ptrace_signal(target, record, signal);
    target.finish_ptrace_resume(resume_result);
    Ok(PtraceContinueOutcome {
        result: reinjected.map(|()| 0),
        retired_relationship,
    })
}

/// Carries a detached relationship beyond the syscall's sleepable ptrace
/// action guard. Finishing earlier could run credential free hooks while that
/// guard is still held.
#[must_use = "detached relationship retirement must cross the ptrace action guard"]
struct PtraceContinueOutcome {
    result: AxResult<isize>,
    retired_relationship: Option<PtraceRelationshipSnapshot>,
}

impl PtraceContinueOutcome {
    fn finish(self) -> AxResult<isize> {
        let Self {
            result,
            retired_relationship,
        } = self;
        drop(retired_relationship);
        result
    }
}

fn peek_word(target: &ProcessData, session: PtraceSession, addr: usize) -> AxResult<isize> {
    let memory = pinned_tracee_memory(target, session)?;
    memory
        .read_value(addr as *const usize)
        .map(|word| word as isize)
        .map_err(|_| ptrace_io_error())
}

fn poke_word(
    target: &ProcessData,
    session: PtraceSession,
    addr: usize,
    data: usize,
) -> AxResult<isize> {
    let memory = pinned_tracee_memory(target, session)?;
    memory
        .write_value(addr as *mut usize, data)
        .map_err(|_| ptrace_io_error())?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
fn canonical_user_address(address: u64) -> bool {
    // The supported x86_64 product ABI uses canonical 48-bit user addresses.
    // Keep this check separate from VMA validation so malformed pointers never
    // reach address-space policy.
    ((address as i64) << 16 >> 16) as u64 == address
}

#[cfg(target_arch = "x86_64")]
fn ptrace_shstk_regset(
    tracer_memory: &UserMemoryCapability,
    target_task: &axtask::AxTaskRef,
    target: &ProcessData,
    session: PtraceSession,
    request: u32,
    note: usize,
    iov_address: usize,
) -> AxResult<isize> {
    // Linux checks tracee-stop authorization before interpreting the regset.
    check_inactive_tracee(target).and_then(|observed| {
        (observed == session)
            .then_some(())
            .ok_or(AxError::NoSuchProcess)
    })?;
    if note != NT_X86_SHSTK {
        return Err(ptrace_io_error());
    }
    if !axhal::asm::user_shadow_stack_enabled() {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    let mut iov = unsafe {
        tracer_memory
            .read_value_uninit(iov_address as *const IoVec)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
    let required = core::mem::size_of::<X86ShstkRegset>();
    if iov.iov_base == 0 || iov.iov_len < required as i64 {
        return Err(ptrace_io_error());
    }
    match request {
        PTRACE_GETREGSET => {
            let state = snapshot_inactive_task_user_cet_state(target_task)
                .map_err(|_| AxError::NoSuchProcess)?;
            tracer_memory
                .write_value(
                    iov.iov_base as *mut X86ShstkRegset,
                    X86ShstkRegset { ssp: state.pl3_ssp },
                )
                .map_err(map_usercopy_error)?;
            iov.iov_len = required as i64;
            tracer_memory
                .write_value(iov_address as *mut IoVec, iov)
                .map_err(map_usercopy_error)?;
            Ok(0)
        }
        PTRACE_SETREGSET => {
            if iov.iov_len != required as i64 {
                return Err(AxError::InvalidInput);
            }
            let regset = tracer_memory
                .read_value(iov.iov_base as *const X86ShstkRegset)
                .map_err(map_usercopy_error)?;
            if !canonical_user_address(regset.ssp) || regset.ssp & 7 != 0 {
                return Err(AxError::InvalidInput);
            }
            let mut state = snapshot_inactive_task_user_cet_state(target_task)
                .map_err(|_| AxError::NoSuchProcess)?;
            if !target
                .aspace()
                .lock()
                .cet_shadow_stack_pointer_valid(regset.ssp)
            {
                return Err(AxError::InvalidInput);
            }
            state.pl3_ssp = regset.ssp;
            replace_inactive_task_user_cet_state(target_task, state)
                .map_err(|_| AxError::NoSuchProcess)?;
            Ok(0)
        }
        _ => unreachable!(),
    }
}

/// Execute the one x86 arch_prctl operation Linux permits a tracer to apply
/// to a stopped tracee.  The normal `arch_prctl(ARCH_SHSTK_UNLOCK)` syscall
/// remains EPERM: permitting it here is deliberately tied to both a live
/// ptrace relationship and the stop generation checked by
/// `check_inactive_tracee`.
#[cfg(target_arch = "x86_64")]
fn ptrace_arch_prctl(
    target_task: &axtask::AxTaskRef,
    target: &ProcessData,
    session: PtraceSession,
    code: usize,
    requested_features: usize,
) -> AxResult<isize> {
    // PTRACE_ARCH_PRCTL is a stopped-tracee operation, just like the CET
    // regset.  Establish that boundary before interpreting the operation or
    // its feature mask.
    let observed = check_inactive_tracee(target)?;
    if observed != session {
        return Err(AxError::NoSuchProcess);
    }
    if i32::try_from(code).ok() != Some(ARCH_SHSTK_UNLOCK) {
        return Err(ptrace_io_error());
    }
    if !axhal::asm::user_shadow_stack_enabled() {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    if requested_features == 0 || requested_features & !ARCH_SHSTK_FEATURES != 0 {
        return Err(AxError::InvalidInput);
    }

    // The outer ptrace action guard prevents a sibling tracer from resuming
    // or detaching after the stop generation above was observed.
    let mut state =
        snapshot_inactive_task_user_cet_state(target_task).map_err(|_| AxError::NoSuchProcess)?;
    state.locked &= !(requested_features as u64);
    replace_inactive_task_user_cet_state(target_task, state).map_err(|_| AxError::NoSuchProcess)?;
    Ok(0)
}

fn sys_ptrace_traceme() -> AxResult<isize> {
    let curr = current();
    let child = curr.as_thread();
    let proc_data = &child.proc_data;
    let parent_snapshot = child
        .task_parent_snapshot()
        .ok_or(AxError::OperationNotPermitted)?;
    let resolve_exact_parent = || {
        let task =
            get_task(parent_snapshot.kernel_tid()).map_err(|_| AxError::OperationNotPermitted)?;
        let parent = task.try_as_thread().ok_or(AxError::OperationNotPermitted)?;
        if parent.kernel_tid() != parent_snapshot.kernel_tid()
            || !alloc::sync::Arc::ptr_eq(parent.task_parent_node(), parent_snapshot.parent_node())
            || !child.task_parent_security_snapshot_matches(&parent_snapshot)
        {
            return Err(AxError::OperationNotPermitted);
        }
        Ok::<_, AxError>(task)
    };
    let parent_task = resolve_exact_parent()?;
    if !parent_task
        .as_thread()
        .landlock_domain()
        .is_ancestor_of(&child.landlock_domain())
    {
        return Err(AxError::OperationNotPermitted);
    }
    let authorized_image = proc_data.thread_image_access_snapshot(child)?;

    let child_image_ref =
        ProcessImageSecurityRef::new(authorized_image.owner_user_ns(), authorized_image.aspace());
    let context = PtraceTracemeContext::new(
        parent_snapshot.credential(),
        authorized_image.credential(),
        child_image_ref.owner_user_ns(),
        &child_image_ref,
    );
    // All process-image locks used to create `authorized_image` have already
    // been released, and reverse-link/ptrace spin locks are acquired only
    // after the dedicated traceme hook stack admits this frozen context.
    dispatch_ptrace_traceme(&context)?;
    drop(parent_task);

    let parent_task = resolve_exact_parent()?;
    let parent_data = parent_task.as_thread().proc_data.clone();
    let mut reverse_link = Some(
        parent_data
            .try_prepare_ptrace_reverse_link(proc_data.proc.pid(), parent_snapshot.kernel_tid())?,
    );
    // Reservation is fallible and may sleep. Re-resolve the immutable parent
    // task and hook-actor credential, then separately pin the calling child's
    // current credential which Linux stores as ptracer_cred for TRACEME.
    drop(parent_task);
    loop {
        let publication = proc_data.lock_ptrace_traceme_publication(&parent_data)?;
        let graph = publication.task_parent_publication();
        let parent_task =
            get_task(parent_snapshot.kernel_tid()).map_err(|_| AxError::OperationNotPermitted)?;
        let parent = parent_task
            .try_as_thread()
            .ok_or(AxError::OperationNotPermitted)?;
        if parent.kernel_tid() != parent_snapshot.kernel_tid()
            || !alloc::sync::Arc::ptr_eq(parent.task_parent_node(), parent_snapshot.parent_node())
            || !alloc::sync::Arc::ptr_eq(&parent.proc_data, &parent_data)
            || !child.task_parent_security_snapshot_matches_locked(graph, &parent_snapshot)
        {
            return Err(AxError::OperationNotPermitted);
        }

        match child.try_lock_task_parent_security_snapshot(graph, &parent_snapshot) {
            TaskParentCredentialPin::Pinned(parent_credential) => {
                let Some(child_credential) = child.try_lock_credential_snapshot() else {
                    drop(parent_credential);
                    drop(parent_task);
                    drop(publication);
                    yield_now();
                    continue;
                };
                let result = proc_data.publish_ptrace_relationship(
                    &publication,
                    child,
                    parent,
                    &parent_credential,
                    PtraceRelationshipOrigin::Traceme,
                    child_credential.credential(),
                    false,
                    0,
                    &authorized_image,
                    reverse_link.take().expect("reserved ptrace reverse link"),
                );
                drop(child_credential);
                drop(parent_credential);
                drop(parent_task);
                drop(publication);
                result?;
                return Ok(0);
            }
            TaskParentCredentialPin::Stale => {
                return Err(AxError::OperationNotPermitted);
            }
            TaskParentCredentialPin::Busy => {
                drop(parent_task);
                drop(publication);
                yield_now();
            }
        }
    }
}

fn sys_ptrace_for_target(
    tracer_memory: &UserMemoryCapability,
    request: u32,
    pid: Pid,
    addr: usize,
    data: usize,
) -> AxResult<isize> {
    let target_pid = current()
        .as_thread()
        .pid_ns()
        .resolve_visible_pid(pid)
        .ok_or(AxError::NoSuchProcess)?;
    let target_task = get_visible_task(target_pid)?;
    let target_thread = target_task.as_thread();
    let target = target_thread.proc_data.clone();
    match request {
        // Relationship publication has a stronger outer lock order:
        // process_lifecycle -> ptrace_actions -> exec/image/ptrace. Keep it
        // out of the ordinary action gate below; publication acquires the
        // composite guard in that order.
        PTRACE_ATTACH => return do_attach(target_thread, false, 0),
        PTRACE_SEIZE => {
            if addr != 0 {
                return Err(ptrace_io_error());
            }
            if data & !PTRACE_O_MASK != 0 {
                return Err(AxError::InvalidInput);
            }
            return do_attach(target_thread, true, data as u32);
        }
        _ => {}
    }

    // Ordinary actions need only the sleepable per-target gate. Exact
    // ptrace/image/job-control spin checks remain short, while the relationship
    // cannot be resumed or detached by a sibling tracer thread during remote
    // memory or usercopy.
    let ptrace_action = target.lock_ptrace_actions();
    match request {
        PTRACE_CONT | PTRACE_SYSCALL | PTRACE_SINGLESTEP => {
            let session = check_inactive_tracee(&target)?;
            do_continue(&target, session, data, false)?.finish()
        }
        PTRACE_DETACH => {
            let session = check_inactive_tracee(&target)?;
            let outcome = do_continue(&target, session, data, true)?;
            drop(ptrace_action);
            outcome.finish()
        }
        PTRACE_KILL => {
            check_tracee(&target)?;
            send_signal_to_process(
                target.proc.pid(),
                Some(SignalInfo::new_kernel(Signo::SIGKILL)),
            )?;
            Ok(0)
        }
        PTRACE_PEEKTEXT | PTRACE_PEEKDATA => {
            let session = check_inactive_tracee(&target)?;
            peek_word(&target, session, addr)
        }
        PTRACE_POKETEXT | PTRACE_POKEDATA => {
            let session = check_inactive_tracee(&target)?;
            poke_word(&target, session, addr, data)
        }
        PTRACE_PEEKUSER | PTRACE_POKEUSER => {
            check_inactive_tracee(&target)?;
            Err(ptrace_io_error())
        }
        PTRACE_SETOPTIONS => {
            let session = check_inactive_tracee(&target)?;
            if data & !PTRACE_O_MASK != 0 {
                return Err(AxError::InvalidInput);
            }
            if !target.ptrace_set_options(session, data as u32) {
                return Err(AxError::NoSuchProcess);
            }
            Ok(0)
        }
        PTRACE_GETEVENTMSG => {
            let session = check_inactive_tracee(&target)?;
            let event_message = target
                .ptrace_event_message(session)
                .ok_or(AxError::NoSuchProcess)?;
            tracer_memory
                .write_value(data as *mut usize, event_message)
                .map_err(map_usercopy_error)?;
            Ok(0)
        }
        PTRACE_INTERRUPT => {
            let session = check_tracee(&target)?;
            let stopped = target
                .ptrace_interrupt(session, Signo::SIGTRAP as u8)
                .ok_or_else(ptrace_io_error)?;
            if stopped {
                notify_ptrace_attach_stop(&target);
                interrupt_process_threads(&target);
            }
            Ok(0)
        }
        PTRACE_LISTEN => {
            check_inactive_tracee(&target)?;
            // LISTEN is not an ordinary resume: Linux retains a seized
            // group-stop in a distinct listening state until an event or
            // INTERRUPT re-traps it. Do not fake that state with CONT.
            Err(ptrace_io_error())
        }
        PTRACE_GETSIGINFO => {
            let session = check_inactive_tracee(&target)?;
            let info = target
                .ptrace_signal_info(session)
                .ok_or_else(ptrace_io_error)?;
            unsafe {
                tracer_memory
                    .write_value_unchecked(data as *mut SignalInfo, info)
                    .map_err(map_usercopy_error)?;
            }
            Ok(0)
        }
        PTRACE_SETSIGINFO => {
            let session = check_inactive_tracee(&target)?;
            let info = unsafe {
                tracer_memory
                    .read_value_uninit(data as *const SignalInfo)
                    .map_err(map_usercopy_error)?
                    .assume_init()
            };
            target.replace_ptrace_signal_info(session, info)?;
            Ok(0)
        }
        PTRACE_ARCH_PRCTL => {
            #[cfg(target_arch = "x86_64")]
            {
                let session = check_tracee(&target)?;
                return ptrace_arch_prctl(&target_task, &target, session, addr, data);
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                Err(ptrace_io_error())
            }
        }
        PTRACE_GETREGSET | PTRACE_SETREGSET => {
            #[cfg(target_arch = "x86_64")]
            {
                let session = check_inactive_tracee(&target)?;
                return ptrace_shstk_regset(
                    tracer_memory,
                    &target_task,
                    &target,
                    session,
                    request,
                    addr,
                    data,
                );
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                check_inactive_tracee(&target)?;
                Err(ptrace_io_error())
            }
        }
        PTRACE_ATTACH | PTRACE_SEIZE => unreachable!(),
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_ptrace(
    tracer_memory: UserMemoryCapability,
    request: u32,
    pid: i32,
    addr: usize,
    data: usize,
) -> AxResult<isize> {
    match request {
        PTRACE_TRACEME => sys_ptrace_traceme(),
        _ => {
            if pid <= 0 {
                return Err(AxError::NoSuchProcess);
            }
            sys_ptrace_for_target(&tracer_memory, request, pid as Pid, addr, data)
        }
    }
}

#[cfg(test)]
mod tests {
    use axerrno::AxResult;
    use axtask::TaskState;

    use super::{InactiveScan, scan_tracee_task_states};

    fn scan(states: &[TaskState]) -> InactiveScan {
        scan_tracee_task_states(states.iter().copied().map(Ok::<_, axerrno::AxError>)).unwrap()
    }

    #[test]
    fn process_access_ptrace_scheduler_inactive_scan_waits_for_every_task() {
        assert_eq!(scan(&[]), InactiveScan::Gone);
        assert_eq!(scan(&[TaskState::Blocked]), InactiveScan::Inactive);
        assert_eq!(
            scan(&[TaskState::Blocked, TaskState::Ready]),
            InactiveScan::Retry
        );
        assert_eq!(
            scan(&[TaskState::Running, TaskState::Blocked]),
            InactiveScan::Retry
        );
        assert_eq!(
            scan(&[TaskState::Running, TaskState::Exited]),
            InactiveScan::Gone
        );
    }

    #[test]
    fn process_access_ptrace_scheduler_scan_propagates_lookup_failure() {
        let states: [AxResult<TaskState>; 1] = [Err(axerrno::AxError::NoSuchProcess)];
        assert_eq!(
            scan_tracee_task_states(states),
            Err(axerrno::AxError::NoSuchProcess)
        );
    }
}
