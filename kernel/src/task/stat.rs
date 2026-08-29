use alloc::{borrow::ToOwned, format, string::String};

use axerrno::{AxError, AxResult};
use axtask::{AxTaskRef, SchedClass, TaskState, sched_state};
use linux_raw_sys::general::{
    RLIMIT_RSS, SCHED_BATCH, SCHED_FIFO, SCHED_IDLE, SCHED_NORMAL, SCHED_RR,
};
use memory_addr::PAGE_SIZE_4K;
use thekernel_linux_signal::Signo;

use crate::task::{AsThread, ProcStateHint, Process, TaskUsage, nanos_to_clock_ticks};

pub(crate) fn task_state(task: &AxTaskRef) -> char {
    let thread = task.as_thread();
    let proc_data = &thread.proc_data;
    let state = task.state();

    if proc_data.is_stopped() {
        return 'T';
    }

    // A pending signal never wakes a Linux TASK_UNINTERRUPTIBLE waiter.  Test
    // this authoritative blocked+D pairing before the generic interrupt bit;
    // the readiness raw-wake edge clears D before publishing Ready.
    if state == TaskState::Blocked && thread.proc_state_hint() == ProcStateHint::Uninterruptible {
        return 'D';
    }

    // Signal interruption makes an interruptible sleeper runnable before the
    // handler executes, so procfs must not keep reporting it as sleeping.
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

fn task_sched_stat(task: &AxTaskRef) -> (i32, i8, u8, u32) {
    let sched = sched_state(task);
    let priority = match sched.class {
        SchedClass::Fifo | SchedClass::RoundRobin => -(sched.rt_priority as i32) - 1,
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => 20 + sched.nice as i32,
    };
    let policy = match sched.class {
        SchedClass::Normal => SCHED_NORMAL,
        SchedClass::Batch => SCHED_BATCH,
        SchedClass::Idle => SCHED_IDLE,
        SchedClass::Fifo => SCHED_FIFO,
        SchedClass::RoundRobin => SCHED_RR,
    };
    (priority, sched.nice, sched.rt_priority, policy)
}

fn process_memory_stat(task: &AxTaskRef) -> (usize, isize, u64) {
    let proc_data = &task.as_thread().proc_data;
    let aspace_handle = proc_data.aspace();
    let aspace = aspace_handle.lock();
    let vsize = aspace
        .areas()
        .filter(|area| area.flags().contains(axhal::paging::MappingFlags::USER))
        .map(|area| area.size())
        .sum();
    let rss = (aspace.resident_user_bytes() / PAGE_SIZE_4K) as isize;
    let rsslim = proc_data.rlim.read()[RLIMIT_RSS].current;
    (vsize, rss, rsslim)
}

/// Renders `/proc/[pid]/stat`.
///
/// Fields without a backing kernel counter remain zero until that counter is
/// owned by the corresponding task, scheduler, VM, or tty subsystem.
pub fn render_task_stat(task: &AxTaskRef) -> AxResult<String> {
    let thread = task.as_thread();
    let proc_data = &thread.proc_data;
    let proc = &proc_data.proc;
    let pid = proc.pid();
    let comm = task.try_name().map_err(|error| match error {
        axtask::TaskNameError::OutOfMemory => AxError::NoMemory,
        axtask::TaskNameError::ConcurrentMutation => AxError::ResourceBusy,
    })?;
    let comm = comm[..comm.len().min(16)].to_owned();
    let state = task_state(task);
    let ppid = proc.parent().map_or(0, |parent| parent.pid());
    let pgrp = proc.group().pgid();
    let session = proc.group().session().sid();
    let num_threads = proc.thread_count() as u32;
    let self_usage = process_usage(task, num_threads);
    let child_usage = proc_data.children_usage();
    let (priority, nice, rt_priority, policy) = task_sched_stat(task);
    let (vsize, rss, rsslim) = process_memory_stat(task);
    let starttime = nanos_to_clock_ticks(proc_data.start_monotonic_ns());
    let processor = task.cpu_id();
    let exit_signal = proc.exit_signal().unwrap_or(Signo::SIGCHLD as u8);
    let exit_code = proc.exit_code();

    Ok(format!(
        "{pid} ({comm}) {state} {ppid} {pgrp} {session} 0 0 0 0 0 0 0 {utime} {stime} {cutime} \
         {cstime} {priority} {nice} {num_threads} 0 {starttime} {vsize} {rss} {rsslim} 0 0 0 0 0 \
         0 0 0 0 0 0 0 {exit_signal} {processor} {rt_priority} {policy} 0 0 0 0 0 0 0 0 0 0 \
         {exit_code}\n",
        utime = self_usage.utime_ticks(),
        stime = self_usage.stime_ticks(),
        cutime = child_usage.utime_ticks(),
        cstime = child_usage.stime_ticks(),
    ))
}

pub fn render_zombie_stat(process: &Process) -> AxResult<String> {
    let snapshot = process.zombie_payload().ok_or(AxError::NoSuchProcess)?;
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
