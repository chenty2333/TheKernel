use alloc::{borrow::ToOwned, fmt, string::String};

use axerrno::AxResult;
use axtask::{AxTaskRef, TaskState, sched_state};
use linux_raw_sys::general::{SCHED_BATCH, SCHED_FIFO, SCHED_IDLE, SCHED_NORMAL, SCHED_RR};
use starry_signal::Signo;

use crate::task::AsThread;

/// Represents the `/proc/[pid]/stat` file.
///
/// See ['https://man7.org/linux/man-pages/man5/proc_pid_stat.5.html'] for details.
#[allow(missing_docs)]
#[derive(Default)]
pub struct TaskStat {
    pub pid: u32,
    pub comm: String,
    pub state: char,
    pub ppid: u32,
    pub pgrp: u32,
    pub session: u32,
    pub tty_nr: u32,
    pub tpgid: u32,
    pub flags: u32,
    pub minflt: u64,
    pub cminflt: u64,
    pub majflt: u64,
    pub cmajflt: u64,
    pub utime: u64,
    pub stime: u64,
    pub cutime: u64,
    pub cstime: u64,
    pub priority: i32,
    pub nice: i32,
    pub num_threads: u32,
    pub itrealvalue: u32,
    pub starttime: u64,
    pub vsize: u64,
    pub rss: i64,
    pub rsslim: u64,
    pub start_code: u64,
    pub end_code: u64,
    pub start_stack: u64,
    pub kstk_esp: u64,
    pub kstk_eip: u64,
    pub signal: u32,
    pub blocked: u32,
    pub sigignore: u32,
    pub sigcatch: u32,
    pub wchan: u64,
    pub nswap: u64,
    pub cnswap: u64,
    pub exit_signal: u8,
    pub processor: u32,
    pub rt_priority: u32,
    pub policy: u32,
    pub delayacct_blkio_ticks: u64,
    pub guest_time: u64,
    pub cguest_time: u64,
    pub start_data: u64,
    pub end_data: u64,
    pub start_brk: u64,
    pub arg_start: u64,
    pub arg_end: u64,
    pub env_start: u64,
    pub env_end: u64,
    pub exit_code: i32,
}

impl TaskStat {
    /// Create a new [`TaskStat`] from a task reference.
    pub fn from_task(task: &AxTaskRef) -> AxResult<Self> {
        let thread = task.as_thread();
        let proc_data = &thread.proc_data;
        let proc = &proc_data.proc;

        let pid = proc.pid();
        let comm = task.name();
        let comm = comm[..comm.len().min(16)].to_owned();
        let state = if proc_data.is_stopped() {
            'T'
        } else {
            match task.state() {
                TaskState::Running | TaskState::Ready => 'R',
                TaskState::Blocked => 'S',
                TaskState::Exited => 'Z',
            }
        };
        let ppid = proc.parent().map_or(0, |p| p.pid());
        let pgrp = proc.group().pgid();
        let session = proc.group().session().sid();
        let self_usage = proc_data.self_usage();
        let child_usage = proc_data.children_usage();
        let sched = sched_state(task);
        let (policy, priority, nice, rt_priority) = match sched.class {
            axtask::SchedClass::Normal => (
                SCHED_NORMAL as u32,
                sched.nice as i32 + 20,
                sched.nice as i32,
                0,
            ),
            axtask::SchedClass::Batch => (
                SCHED_BATCH as u32,
                sched.nice as i32 + 20,
                sched.nice as i32,
                0,
            ),
            axtask::SchedClass::Idle => (
                SCHED_IDLE as u32,
                sched.nice as i32 + 20,
                sched.nice as i32,
                0,
            ),
            axtask::SchedClass::Fifo => (
                SCHED_FIFO as u32,
                -(sched.rt_priority as i32) - 1,
                0,
                sched.rt_priority as u32,
            ),
            axtask::SchedClass::RoundRobin => (
                SCHED_RR as u32,
                -(sched.rt_priority as i32) - 1,
                0,
                sched.rt_priority as u32,
            ),
        };
        Ok(Self {
            pid,
            comm: comm.to_owned(),
            state,
            ppid,
            pgrp,
            session,
            utime: self_usage.utime_ticks(),
            stime: self_usage.stime_ticks(),
            cutime: child_usage.utime_ticks(),
            cstime: child_usage.stime_ticks(),
            priority,
            nice,
            num_threads: proc.threads().len() as u32,
            exit_signal: proc.exit_signal().unwrap_or(Signo::SIGCHLD as u8),
            rt_priority,
            policy,
            exit_code: proc.exit_code(),
            ..Default::default()
        })
    }
}

impl fmt::Display for TaskStat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            pid,
            comm,
            state,
            ppid,
            pgrp,
            session,
            tty_nr,
            tpgid,
            flags,
            minflt,
            cminflt,
            majflt,
            cmajflt,
            utime,
            stime,
            cutime,
            cstime,
            priority,
            nice,
            num_threads,
            itrealvalue,
            starttime,
            vsize,
            rss,
            rsslim,
            start_code,
            end_code,
            start_stack,
            kstk_esp,
            kstk_eip,
            signal,
            blocked,
            sigignore,
            sigcatch,
            wchan,
            nswap,
            cnswap,
            exit_signal,
            processor,
            rt_priority,
            policy,
            delayacct_blkio_ticks,
            guest_time,
            cguest_time,
            start_data,
            end_data,
            start_brk,
            arg_start,
            arg_end,
            env_start,
            env_end,
            exit_code,
        } = self;
        writeln!(
            f,
            "{pid} ({comm}) {state} {ppid} {pgrp} {session} {tty_nr} {tpgid} {flags} {minflt} \
             {cminflt} {majflt} {cmajflt} {utime} {stime} {cutime} {cstime} {priority} {nice} \
             {num_threads} {itrealvalue} {starttime} {vsize} {rss} {rsslim} {start_code} \
             {end_code} {start_stack} {kstk_esp} {kstk_eip} {signal} {blocked} {sigignore} \
             {sigcatch} {wchan} {nswap} {cnswap} {exit_signal} {processor} {rt_priority} {policy} \
             {delayacct_blkio_ticks} {guest_time} {cguest_time} {start_data} {end_data} \
             {start_brk} {arg_start} {arg_end} {env_start} {env_end} {exit_code}",
        )
    }
}
