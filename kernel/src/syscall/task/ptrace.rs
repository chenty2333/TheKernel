use axerrno::{AxError, AxResult, LinuxError};
use axhal::paging::MappingFlags;
use axtask::{TaskState, current, yield_now};
use memory_addr::{MemoryAddr, VirtAddr};
use starry_process::Pid;
use starry_signal::{SignalInfo, Signo};
use starry_vm::{VmMutPtr, VmPtr};

use crate::task::{
    AsThread, ProcessData, PtraceAccessMode, PtraceReverseLink, PtraceSession,
    TaskParentCredentialPin, Thread, check_current_thread_ptrace_image_access, get_task,
    get_visible_task, notify_ptrace_attach_stop, reinject_ptrace_signal,
    security::{ProcessImageSecurityRef, PtraceTracemeContext, dispatch_ptrace_traceme},
    send_signal_to_process,
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
fn with_pinned_tracee_aspace<T>(
    target: &ProcessData,
    session: PtraceSession,
    operation: impl FnOnce(&mut crate::mm::AddrSpace) -> AxResult<T>,
) -> AxResult<T> {
    let aspace_handle = target
        .ptrace_inactive_image_if_session(session)
        .ok_or(AxError::NoSuchProcess)?;
    let mut aspace = aspace_handle.lock();
    operation(&mut aspace)
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
    let tracer_data = curr.as_thread().proc_data.clone();
    let tracer = tracer_data.proc.pid();
    let tracer_kernel_tid = curr.as_thread().kernel_tid();
    let target = &target_thread.proc_data;
    if target.proc.pid() == tracer {
        return Err(AxError::OperationNotPermitted);
    }
    if target.exec_in_progress() {
        return Err(AxError::OperationNotPermitted);
    }
    let authorized_image =
        check_current_thread_ptrace_image_access(target_thread, PtraceAccessMode::AttachReal)?;
    let reverse_link =
        tracer_data.try_prepare_ptrace_reverse_link(target.proc.pid(), tracer_kernel_tid)?;
    let publication = target.lock_ptrace_publication();
    let session = target.publish_ptrace_relationship(
        &publication,
        target_thread,
        tracer,
        tracer_kernel_tid,
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
) -> AxResult<isize> {
    let curr = current();
    let tracer_data = curr.as_thread().proc_data.clone();
    let signal = parse_signal(data)?.map(|info| info.signo());
    let (resume_result, record) = target
        .resume_ptrace(session, detach)
        .ok_or(AxError::NoSuchProcess)?;
    if detach {
        tracer_data.remove_ptrace_tracee(PtraceReverseLink::new(target.proc.pid(), session));
    }
    let reinjected = reinject_ptrace_signal(target, record, signal);
    target.finish_ptrace_resume(resume_result);
    reinjected?;
    Ok(0)
}

fn validate_remote_access(
    aspace: &mut crate::mm::AddrSpace,
    addr: usize,
    len: usize,
    flags: MappingFlags,
) -> AxResult<()> {
    let start = VirtAddr::from_usize(addr);
    let end = start.checked_add(len).ok_or_else(ptrace_io_error)?;
    let page_start = start.align_down_4k();
    let page_end = VirtAddr::from_usize(
        crate::mm::checked_align_up_4k(end.as_usize()).ok_or_else(ptrace_io_error)?,
    );
    if !aspace.can_access_range(start, len, flags) {
        return Err(ptrace_io_error());
    }
    aspace
        .populate_area(page_start, page_end.sub_addr(page_start), flags)
        .map_err(|_| ptrace_io_error())
}

fn peek_word(target: &ProcessData, session: PtraceSession, addr: usize) -> AxResult<isize> {
    with_pinned_tracee_aspace(target, session, |aspace| {
        let mut word = [0u8; size_of::<usize>()];
        validate_remote_access(aspace, addr, word.len(), MappingFlags::READ)?;
        aspace
            .read(VirtAddr::from_usize(addr), &mut word)
            .map_err(|_| ptrace_io_error())?;
        Ok(usize::from_ne_bytes(word) as isize)
    })
}

fn poke_word(
    target: &ProcessData,
    session: PtraceSession,
    addr: usize,
    data: usize,
) -> AxResult<isize> {
    with_pinned_tracee_aspace(target, session, |aspace| {
        let word = data.to_ne_bytes();
        validate_remote_access(aspace, addr, word.len(), MappingFlags::WRITE)?;
        aspace
            .write(VirtAddr::from_usize(addr), &word)
            .map_err(|_| ptrace_io_error())?;
        Ok(0)
    })
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
    // Reservation is fallible and may sleep. Re-resolve the immutable task
    // identity and revalidate the exact credential object used by the hook
    // immediately before relationship publication.
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
                let parent_pid = parent_data.proc.pid();
                let result = proc_data.publish_ptrace_relationship(
                    &publication,
                    child,
                    parent_pid,
                    parent_snapshot.kernel_tid(),
                    false,
                    0,
                    &authorized_image,
                    reverse_link.take().expect("reserved ptrace reverse link"),
                );
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

fn sys_ptrace_for_target(request: u32, pid: Pid, addr: usize, data: usize) -> AxResult<isize> {
    let target_task = get_visible_task(pid)?;
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
    let _ptrace_action = target.lock_ptrace_actions();
    match request {
        PTRACE_CONT | PTRACE_SYSCALL | PTRACE_SINGLESTEP => {
            let session = check_inactive_tracee(&target)?;
            do_continue(&target, session, data, false)
        }
        PTRACE_DETACH => {
            let session = check_inactive_tracee(&target)?;
            do_continue(&target, session, data, true)
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
            (data as *mut usize).vm_write(event_message)?;
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
            (data as *mut SignalInfo).vm_write(info)?;
            Ok(0)
        }
        PTRACE_SETSIGINFO => {
            let session = check_inactive_tracee(&target)?;
            let info = unsafe { (data as *const SignalInfo).vm_read_uninit()?.assume_init() };
            target.replace_ptrace_signal_info(session, info)?;
            Ok(0)
        }
        PTRACE_GETREGSET | PTRACE_SETREGSET => {
            check_inactive_tracee(&target)?;
            Err(ptrace_io_error())
        }
        PTRACE_ATTACH | PTRACE_SEIZE => unreachable!(),
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_ptrace(request: u32, pid: i32, addr: usize, data: usize) -> AxResult<isize> {
    match request {
        PTRACE_TRACEME => sys_ptrace_traceme(),
        _ => {
            if pid <= 0 {
                return Err(AxError::NoSuchProcess);
            }
            sys_ptrace_for_target(request, pid as Pid, addr, data)
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
