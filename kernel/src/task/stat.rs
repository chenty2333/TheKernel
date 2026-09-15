use alloc::{format, string::String, vec::Vec};

use axerrno::{AxError, AxResult};
use axtask::{AxTaskRef, SchedClass, TaskState, sched_state};
use linux_raw_sys::general::{
    RLIMIT_RSS, SCHED_BATCH, SCHED_DEADLINE, SCHED_FIFO, SCHED_IDLE, SCHED_NORMAL, SCHED_RR,
};
use memory_addr::PAGE_SIZE_4K;
use tk_linux_signal::Signo;

use crate::task::{
    AsThread, PidNamespace, ProcStateHint, Process, TaskUsage, nanos_to_clock_ticks,
    zombie_scheduler_state,
};

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
            ProcStateHint::IoWait => 'S',
            ProcStateHint::Uninterruptible => 'D',
            ProcStateHint::None => 'R',
        },
        TaskState::Exited => 'Z',
        TaskState::Blocked => match thread.proc_state_hint() {
            ProcStateHint::Interruptible => 'S',
            ProcStateHint::IoWait => 'S',
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

fn sched_stat(class: SchedClass, nice: i8, rt_priority: u8) -> (i32, i8, u8, u32) {
    let priority = match class {
        SchedClass::Fifo | SchedClass::RoundRobin => -(rt_priority as i32) - 1,
        SchedClass::Deadline => -101,
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => 20 + nice as i32,
    };
    let policy = match class {
        SchedClass::Normal => SCHED_NORMAL,
        SchedClass::Batch => SCHED_BATCH,
        SchedClass::Idle => SCHED_IDLE,
        SchedClass::Fifo => SCHED_FIFO,
        SchedClass::RoundRobin => SCHED_RR,
        SchedClass::Deadline => SCHED_DEADLINE,
    };
    (priority, nice, rt_priority, policy)
}

fn task_sched_stat(task: &AxTaskRef) -> (i32, i8, u8, u32) {
    let sched = sched_state(task);
    sched_stat(sched.class, sched.nice, sched.rt_priority)
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
pub fn render_task_stat(
    task: &AxTaskRef,
    pid_ns: &PidNamespace,
    process_view: bool,
) -> AxResult<Vec<u8>> {
    let thread = task.as_thread();
    let proc_data = &thread.proc_data;
    let proc = &proc_data.proc;
    let pid = pid_ns
        .visible_pid_checked(if process_view {
            proc.pid()
        } else {
            thread.tid()
        })
        .ok_or(AxError::NoSuchProcess)?;
    // `/proc/<pid>/stat` prints `task_struct::comm` verbatim through
    // `proc_task_name()` (`fs/proc/array.c:100-119`, called with
    // `escape = false` from `do_task_stat()` at `:588-590`), so the field is
    // spliced in as raw bytes. It must not be routed through `str` at all:
    // `from_utf8_lossy()` rewrites a trailing partial UTF-8 sequence as
    // U+FFFD, which both fabricates bytes Linux never prints and can make a
    // fifteen-byte name exceed the sixteen-byte field, so a later byte slice
    // would panic on the replacement character's boundary.
    let task_comm = task.comm();
    let comm = tk_linux_process::proc_stat_comm_field(task_comm.as_bytes());
    let state = task_state(task);
    let ppid = proc
        .parent()
        .and_then(|parent| pid_ns.visible_pid_checked(parent.pid()))
        .unwrap_or(0);
    let pgrp = pid_ns.visible_pid_checked(proc.group().pgid()).unwrap_or(0);
    let session = pid_ns
        .visible_pid_checked(proc.group().session().sid())
        .unwrap_or(0);
    let num_threads = proc.thread_count() as u32;
    let self_usage = process_usage(task, num_threads);
    let child_usage = proc_data.children_usage();
    // Field 42 is delayacct_blkio_ticks.  io_uring's actual CQ sleep path
    // contributes its nested I/O-wait accumulator through this normal task
    // accounting surface rather than a private diagnostic-only counter.
    let (_, iowait_ns) = thread.iowait_accounting();
    let iowait_ticks = nanos_to_clock_ticks(iowait_ns);
    let (priority, nice, rt_priority, policy) = task_sched_stat(task);
    let (vsize, rss, rsslim) = process_memory_stat(task);
    let mm = proc_data.mm_layout();
    let starttime = nanos_to_clock_ticks(proc_data.start_monotonic_ns());
    let processor = task.cpu_id();
    let exit_signal = proc.exit_signal().unwrap_or(Signo::SIGCHLD as u8);
    let exit_code = proc.exit_code();

    // The numeric fields are rendered as text and the raw `comm` bytes are
    // spliced between the two halves, so the field cannot be re-encoded. The
    // parentheses come from `do_task_stat()` (`fs/proc/array.c:588-590`).
    let head = format!("{pid} (");
    let tail = format!(
        ") {state} {ppid} {pgrp} {session} 0 0 0 0 0 0 0 {utime} {stime} {cutime} \
         {cstime} {priority} {nice} {num_threads} 0 {starttime} {vsize} {rss} {rsslim} \
         {start_code} {end_code} {start_stack} 0 0 0 0 0 0 0 0 0 {exit_signal} {processor} \
         {rt_priority} {policy} {iowait_ticks} 0 0 {start_data} {end_data} {start_brk} \
         {arg_start} {arg_end} {env_start} {env_end} {exit_code}\n",
        utime = self_usage.utime_ticks(),
        stime = self_usage.stime_ticks(),
        cutime = child_usage.utime_ticks(),
        cstime = child_usage.stime_ticks(),
        start_code = mm.start_code,
        end_code = mm.end_code,
        start_stack = mm.start_stack,
        start_data = mm.start_data,
        end_data = mm.end_data,
        start_brk = mm.start_brk,
        arg_start = mm.arg_start,
        arg_end = mm.arg_end,
        env_start = mm.env_start,
        env_end = mm.env_end,
    );
    let mut stat = Vec::with_capacity(head.len() + comm.len() + tail.len());
    stat.extend_from_slice(head.as_bytes());
    stat.extend_from_slice(comm);
    stat.extend_from_slice(tail.as_bytes());
    Ok(stat)
}

pub fn render_zombie_stat(process: &Process, pid_ns: &PidNamespace) -> AxResult<String> {
    let snapshot = process.zombie_payload().ok_or(AxError::NoSuchProcess)?;
    let pid = pid_ns
        .visible_pid_checked(process.pid())
        .ok_or(AxError::NoSuchProcess)?;
    let ppid = process
        .parent()
        .and_then(|parent| pid_ns.visible_pid_checked(parent.pid()))
        .unwrap_or(0);
    let pgrp = pid_ns
        .visible_pid_checked(process.group().pgid())
        .unwrap_or(0);
    let session = pid_ns
        .visible_pid_checked(process.group().session().sid())
        .unwrap_or(0);
    let self_usage: TaskUsage = snapshot.self_usage.into();
    let child_usage: TaskUsage = snapshot.child_usage.into();
    let scheduler = zombie_scheduler_state(process)?;
    let (priority, nice, rt_priority, policy) =
        sched_stat(scheduler.class, scheduler.nice, scheduler.rt_priority);
    let exit_signal = process.exit_signal().unwrap_or(Signo::SIGCHLD as u8);
    let exit_code = snapshot.wait_status;

    Ok(format_zombie_stat(
        pid,
        ppid,
        pgrp,
        session,
        self_usage,
        child_usage,
        priority,
        nice,
        rt_priority,
        policy,
        exit_signal,
        exit_code,
    ))
}

#[allow(clippy::too_many_arguments)]
fn format_zombie_stat(
    pid: u32,
    ppid: u32,
    pgrp: u32,
    session: u32,
    self_usage: TaskUsage,
    child_usage: TaskUsage,
    priority: i32,
    nice: i8,
    rt_priority: u8,
    policy: u32,
    exit_signal: u8,
    exit_code: i32,
) -> String {
    let comm = "zombie";
    let state = 'Z';
    let num_threads = 1;
    format!(
        "{pid} ({comm}) {state} {ppid} {pgrp} {session} 0 0 0 0 0 0 0 {} {} {} {} {priority} \
         {nice} {num_threads} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 {exit_signal} 0 {rt_priority} \
         {policy} 0 0 0 0 0 0 0 0 0 0 {exit_code}\n",
        self_usage.utime_ticks(),
        self_usage.stime_ticks(),
        child_usage.utime_ticks(),
        child_usage.stime_ticks(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zombie_stat_has_all_linux_fields() {
        let text = format_zombie_stat(
            2,
            1,
            2,
            2,
            TaskUsage::default(),
            TaskUsage::default(),
            20,
            0,
            7,
            1,
            17,
            42 << 8,
        );
        let fields: alloc::vec::Vec<_> = text.split_whitespace().collect();
        assert_eq!(fields.len(), 52);
        assert_eq!(fields[37], "17");
        assert_eq!(fields[39], "7");
        assert_eq!(fields[40], "1");
        assert_eq!(fields[51], "10752");
    }

    #[test]
    fn deadline_sched_stat_uses_linux_deadline_priority() {
        assert_eq!(
            sched_stat(SchedClass::Deadline, 0, 0),
            (-101, 0, 0, SCHED_DEADLINE)
        );
    }
}
