use alloc::{borrow::ToOwned, format, string::String};

use axerrno::{AxError, AxResult};
use axtask::{AxTaskRef, TaskState};
use starry_process::Process;
use starry_signal::Signo;

use crate::task::{AsThread, ProcStateHint, TaskUsage};

fn task_state(task: &AxTaskRef) -> char {
    let thread = task.as_thread();
    let proc_data = &thread.proc_data;
    let state = task.state();

    if proc_data.is_stopped() {
        return 'T';
    }

    // A task in an interruptible sleep with a pending wakeup from signal
    // delivery should be reported as runnable. LTP polls /proc/[pid]/stat to
    // synchronize signal-driven children and expects the state to stop being
    // `S` once the signal has been sent.
    if task.is_interrupted() {
        return 'R';
    }

    match state {
        TaskState::Running => 'R',
        TaskState::Ready => match thread.proc_state_hint() {
            ProcStateHint::Interruptible => 'S',
            ProcStateHint::Uninterruptible => 'D',
            ProcStateHint::None => 'R',
        },
        TaskState::Exited => 'Z',
        TaskState::Blocked => match thread.proc_state_hint() {
            ProcStateHint::Interruptible => 'S',
            ProcStateHint::Uninterruptible => 'D',
            ProcStateHint::None => 'S',
        },
    }
}

fn process_usage(task: &AxTaskRef, num_threads: u32) -> TaskUsage {
    let thread = task.as_thread();
    if num_threads <= 1 {
        TaskUsage::from_thread(thread)
    } else {
        thread.proc_data.self_usage()
    }
}

/// Renders `/proc/[pid]/stat`.
///
/// Keep the fields that LTP actually consumes accurate, and default the rest
/// to stable zero values so procfs state polling stays cheap even under very
/// large fork storms.
pub fn render_task_stat(task: &AxTaskRef) -> AxResult<String> {
    let thread = task.as_thread();
    let proc_data = &thread.proc_data;
    let proc = &proc_data.proc;
    let pid = proc.pid();
    let comm = task.name();
    let comm = comm[..comm.len().min(16)].to_owned();
    let state = task_state(task);
    let ppid = proc.parent().map_or(0, |parent| parent.pid());
    let pgrp = proc.group().pgid();
    let session = proc.group().session().sid();
    let num_threads = proc.threads().len() as u32;
    let self_usage = process_usage(task, num_threads);
    let child_usage = proc_data.children_usage();
    let exit_signal = proc.exit_signal().unwrap_or(Signo::SIGCHLD as u8);
    let exit_code = proc.exit_code();

    Ok(format!(
        "{pid} ({comm}) {state} {ppid} {pgrp} {session} 0 0 0 0 0 0 0 {} {} {} {} 20 0 \
         {num_threads} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 {exit_signal} 0 0 0 0 0 0 0 0 0 0 0 \
         {exit_code}\n",
        self_usage.utime_ticks(),
        self_usage.stime_ticks(),
        child_usage.utime_ticks(),
        child_usage.stime_ticks(),
    ))
}

pub fn render_zombie_stat(process: &Process) -> AxResult<String> {
    let snapshot = process.zombie_snapshot().ok_or(AxError::NoSuchProcess)?;
    let pid = process.pid();
    let comm = "zombie";
    let state = 'Z';
    let ppid = process.parent().map_or(0, |parent| parent.pid());
    let pgrp = process.group().pgid();
    let session = process.group().session().sid();
    let num_threads = 1;
    let self_usage: TaskUsage = snapshot.self_usage.into();
    let child_usage: TaskUsage = snapshot.child_usage.into();
    let exit_signal = process.exit_signal().unwrap_or(Signo::SIGCHLD as u8);
    let exit_code = snapshot.wait_status;

    Ok(format!(
        "{pid} ({comm}) {state} {ppid} {pgrp} {session} 0 0 0 0 0 0 0 {} {} {} {} 20 0 \
         {num_threads} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 {exit_signal} 0 0 0 0 0 0 0 0 0 0 0 \
         {exit_code}\n",
        self_usage.utime_ticks(),
        self_usage.stime_ticks(),
        child_usage.utime_ticks(),
        child_usage.stime_ticks(),
    ))
}
