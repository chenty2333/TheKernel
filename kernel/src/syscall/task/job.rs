use axerrno::{AxError, AxResult, LinuxError};
use axsync::Mutex;
use axtask::{AxTaskRef, current};
use thekernel_linux_process_adapter::{Pid, ProcessError};

use crate::task::{
    AsThread, Cred, PidNamespace, Process, get_process_data, get_process_group,
    get_process_including_zombie, get_visible_task_including_exiting, prepare_session_sid_binding,
    process_domain, release_dead_session_sid_binding,
    security::{
        SecurityTaskGetpgidContext, SecurityTaskGetsidContext, dispatch_task_getpgid,
        dispatch_task_getsid,
    },
};

/// Serializes process-group/session identity admission with the corresponding
/// Starry membership mutation. These syscalls are cold paths, and a sleepable
/// mutex avoids holding a spin lock across object construction.
static JOB_CONTROL_OPERATION: Mutex<()> = Mutex::new(());

fn process_error(error: ProcessError) -> AxError {
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

enum GetsidTarget {
    Live(AxTaskRef),
    Zombie(alloc::sync::Arc<Process>),
}

impl GetsidTarget {
    fn process(&self) -> alloc::sync::Arc<Process> {
        match self {
            Self::Live(task) => task.as_thread().proc_data.proc.clone(),
            Self::Zombie(process) => process.clone(),
        }
    }

    fn credential(&self) -> AxResult<alloc::sync::Arc<Cred>> {
        match self {
            Self::Live(task) => Ok(task.as_thread().current_cred()),
            Self::Zombie(process) => process
                .zombie_payload()
                .map(|snapshot| snapshot.credential.clone())
                .ok_or(AxError::NoSuchProcess),
        }
    }
}

/// Resolves a caller-visible PID without scanning the global process table.
/// A retained namespace binding identifies both live non-leader TIDs and
/// unreaped zombie leaders; exited non-leader TIDs have already released it.
fn resolve_getsid_target(pid: Pid, caller_ns: &PidNamespace) -> AxResult<GetsidTarget> {
    let global_pid = caller_ns
        .resolve_visible_pid(pid)
        .ok_or(AxError::NoSuchProcess)?;

    // The alias table is authoritative after non-leader execve: it maps the
    // process PID to the executor while the retired leader's immutable kernel
    // TID may still be present in TASK_TABLE.  Looking up the visible task
    // first therefore cannot let that old leader steal getsid(getpid()).
    if let Ok(task) = get_visible_task_including_exiting(global_pid)
        && caller_ns.contains(&task.as_thread().proc_data.pid_ns())
    {
        return Ok(GetsidTarget::Live(task));
    }

    let process = get_process_including_zombie(global_pid)?;
    let target_ns = process
        .identity::<alloc::sync::Arc<PidNamespace>>()
        .ok_or(AxError::NoSuchProcess)?;
    if process.is_zombie() && caller_ns.contains(&target_ns) {
        Ok(GetsidTarget::Zombie(process))
    } else {
        Err(AxError::NoSuchProcess)
    }
}

pub fn sys_getsid(pid: Pid) -> AxResult<isize> {
    let caller = current();
    let caller_thread = caller.as_thread();
    let caller_ns = caller_thread.proc_data.pid_ns();
    if pid == 0 {
        // Linux returns the caller's session directly for the current-task
        // form; security_task_getsid is only invoked after a nonzero lookup.
        let process = &caller_thread.proc_data.proc;
        let target_ns = process
            .identity::<alloc::sync::Arc<PidNamespace>>()
            .ok_or(AxError::NoSuchProcess)?;
        return Ok(caller_ns
            .visible_pid_for(&target_ns, process.group().session().sid())
            .map(|sid| sid as isize)
            .unwrap_or(0));
    }

    let target = resolve_getsid_target(pid, &caller_ns)?;
    let process = target.process();
    let target_ns = process
        .identity::<alloc::sync::Arc<PidNamespace>>()
        .ok_or(AxError::NoSuchProcess)?;
    let credential = target.credential()?;
    dispatch_task_getsid(&SecurityTaskGetsidContext::new(&credential))?;
    Ok(caller_ns
        .visible_pid_for(&target_ns, process.group().session().sid())
        .map(|sid| sid as isize)
        .unwrap_or(0))
}

pub fn sys_setsid() -> AxResult<isize> {
    let _operation = JOB_CONTROL_OPERATION.lock();
    let curr = current();
    let thread = curr.as_thread();
    let proc = &thread.proc_data.proc;
    let pid_ns = thread.proc_data.pid_ns();
    if proc.group().pgid() == proc.pid() {
        return Err(AxError::OperationNotPermitted);
    }

    let old_session = proc.group().session();
    let prepared_sid_binding = prepare_session_sid_binding()?;
    if let Some((session, _group)) = process_domain()?
        .try_create_session(proc)
        .map_err(process_error)?
    {
        if !old_session.is_live() {
            release_dead_session_sid_binding(&old_session, &pid_ns);
        }
        prepared_sid_binding.commit(session.clone(), pid_ns.clone());
        pid_ns
            .visible_pid_for(&pid_ns, session.sid())
            .map(|sid| sid as isize)
            .ok_or(AxError::NoSuchProcess)
    } else {
        pid_ns
            .visible_pid_for(&pid_ns, proc.pid())
            .map(|pid| pid as isize)
            .ok_or(AxError::NoSuchProcess)
    }
}

pub fn sys_getpgid(pid: Pid) -> AxResult<isize> {
    let caller = current();
    let caller_thread = caller.as_thread();
    let caller_ns = caller_thread.proc_data.pid_ns();
    if pid == 0 {
        let process = &caller_thread.proc_data.proc;
        let target_ns = process
            .identity::<alloc::sync::Arc<PidNamespace>>()
            .ok_or(AxError::NoSuchProcess)?;
        return Ok(caller_ns
            .visible_pid_for(&target_ns, process.group().pgid())
            .map(|pgid| pgid as isize)
            .unwrap_or(0));
    }

    // getpgid shares getsid's PIDTYPE_PID lookup: a visible live TID wins;
    // a zombie is addressable only while its leader is unreaped.
    let target = resolve_getsid_target(pid, &caller_ns)?;
    let process = target.process();
    let target_ns = process
        .identity::<alloc::sync::Arc<PidNamespace>>()
        .ok_or(AxError::NoSuchProcess)?;
    let credential = target.credential()?;
    dispatch_task_getpgid(&SecurityTaskGetpgidContext::new(&credential))?;
    Ok(caller_ns
        .visible_pid_for(&target_ns, process.group().pgid())
        .map(|pgid| pgid as isize)
        .unwrap_or(0))
}

pub fn sys_setpgid(pid: i32, pgid: i32) -> AxResult<isize> {
    let _operation = JOB_CONTROL_OPERATION.lock();
    let curr = current();
    let caller = &curr.as_thread().proc_data.proc;

    if pgid < 0 {
        return Err(AxError::InvalidInput);
    }

    let proc = if pid == 0 {
        caller.clone()
    } else if pid > 0 {
        let target = get_process_data(pid as Pid)?;
        if !target.proc.is_live() {
            return Err(AxError::from(LinuxError::ESRCH));
        }
        target.proc.clone()
    } else {
        return Err(AxError::from(LinuxError::ESRCH));
    };

    if proc.pid() != caller.pid() {
        let is_child = proc
            .parent()
            .is_some_and(|parent| parent.pid() == caller.pid());
        if !is_child {
            return Err(AxError::from(LinuxError::ESRCH));
        }
    }

    if proc.group().session().sid() == proc.pid() {
        return Err(AxError::OperationNotPermitted);
    }

    let caller_session = caller.group().session();
    if !alloc::sync::Arc::ptr_eq(&proc.group().session(), &caller_session) {
        return Err(AxError::OperationNotPermitted);
    }

    let pgid = if pgid == 0 { proc.pid() } else { pgid as Pid };
    if pgid == proc.pid() {
        if proc.group().pgid() == pgid {
            return Ok(0);
        }
        process_domain()?
            .try_create_group(&proc)
            .map_err(process_error)?;
        return Ok(0);
    }

    let group = get_process_group(pgid).map_err(|_| AxError::OperationNotPermitted)?;
    if !alloc::sync::Arc::ptr_eq(&group.session(), &caller_session) {
        return Err(AxError::OperationNotPermitted);
    }
    if !process_domain()?
        .move_to_group(&proc, &group)
        .map_err(process_error)?
    {
        return Err(AxError::OperationNotPermitted);
    }

    Ok(0)
}

// TODO: job control
