use alloc::{format, string::String, sync::Arc, vec, vec::Vec};
use core::{
    sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};

use axerrno::{AxError, AxResult};
use axhal::time::monotonic_time_nanos;
use axnet::NetStack;
use axpoll::PollSet;
use axsync::{Mutex, spin::SpinNoIrq};
use linux_raw_sys::general::{
    CAP_CHOWN, CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH, CAP_FOWNER, CAP_FSETID, CAP_LINUX_IMMUTABLE,
    CAP_MAC_OVERRIDE, CAP_MKNOD, CAP_SETGID, CAP_SETUID,
};
use scope_local::Scope;
use spin::RwLock;
use starry_process::{Pid, Process};
use starry_signal::{
    Signo,
    api::{ProcessSignalManager, SignalActions},
};

use super::{
    accounting::{AtomicTaskUsage, live_process_usage},
    creds::{CAPABILITY_WORDS, CapabilityState, Credentials},
    futex::FutexTable,
    jobctl::{
        ContinueResult, ExecControlState, JobControlState, PtraceControlState, StopKind,
        StopReport, StopState, VforkControlState,
    },
    resources::Rlimits,
    timer::PosixTimer,
};
use crate::{
    file::executable::{self, ExecutableKey},
    mm::AddrSpace,
    time::wall_time,
};

pub(crate) const UTS_FIELD_LEN: usize = 64;
const PROC_NS_INO_BASE: u64 = 0x9_0000_0000;
const SECBIT_NO_SETUID_FIXUP: u32 = 1 << 2;
const SECBIT_KEEP_CAPS: u32 = 1 << 4;
const SECBIT_KEEP_CAPS_LOCKED: u32 = 1 << 5;
const SECBIT_NO_CAP_AMBIENT_RAISE: u32 = 1 << 6;
const SECURE_ALL_BITS: u32 =
    (1 << 0) | SECBIT_NO_SETUID_FIXUP | SECBIT_KEEP_CAPS | SECBIT_NO_CAP_AMBIENT_RAISE;
const SECURE_ALL_LOCKS: u32 = SECURE_ALL_BITS << 1;
static PROC_NS_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct CgroupNamespace {
    id: u64,
}

impl CgroupNamespace {
    pub(crate) fn new_root() -> Arc<Self> {
        Self::new()
    }

    fn new() -> Arc<Self> {
        Arc::new(Self {
            id: PROC_NS_ID.fetch_add(1, Ordering::Relaxed),
        })
    }

    pub(crate) fn fork(self: &Arc<Self>) -> Arc<Self> {
        Self::new()
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }
}

#[derive(Clone)]
pub(crate) struct PidNamespace {
    id: u64,
    parent: Option<Arc<PidNamespace>>,
    init_pid: Option<Pid>,
}

impl PidNamespace {
    pub(crate) fn new_root() -> Arc<Self> {
        Self::new(None, None)
    }

    fn new(parent: Option<Arc<Self>>, init_pid: Option<Pid>) -> Arc<Self> {
        Arc::new(Self {
            id: PROC_NS_ID.fetch_add(1, Ordering::Relaxed),
            parent,
            init_pid,
        })
    }

    pub(crate) fn fork(self: &Arc<Self>, init_pid: Pid) -> Arc<Self> {
        Self::new(Some(self.clone()), Some(init_pid))
    }

    pub(crate) fn parent(&self) -> Option<Arc<Self>> {
        self.parent.clone()
    }

    pub(crate) fn visible_pid(&self, global_pid: Pid) -> Pid {
        if self.init_pid == Some(global_pid) {
            1
        } else {
            global_pid
        }
    }

    pub(crate) fn proc_inode(&self) -> u64 {
        PROC_NS_INO_BASE + self.id.saturating_mul(8)
    }
}

#[derive(Clone)]
pub(crate) struct UserNamespace {
    id: u64,
    parent: Option<Arc<UserNamespace>>,
    owner_uid: u32,
}

impl UserNamespace {
    pub(crate) fn new_root() -> Arc<Self> {
        Self::new(None, 0)
    }

    fn new(parent: Option<Arc<Self>>, owner_uid: u32) -> Arc<Self> {
        Arc::new(Self {
            id: PROC_NS_ID.fetch_add(1, Ordering::Relaxed),
            parent,
            owner_uid,
        })
    }

    pub(crate) fn fork(self: &Arc<Self>, owner_uid: u32) -> Arc<Self> {
        Self::new(Some(self.clone()), owner_uid)
    }

    pub(crate) fn parent(&self) -> Option<Arc<Self>> {
        self.parent.clone()
    }

    pub(crate) fn owner_uid(&self) -> u32 {
        self.owner_uid
    }

    pub(crate) fn proc_inode(&self) -> u64 {
        PROC_NS_INO_BASE + self.id.saturating_mul(8)
    }
}

#[derive(Clone, Copy)]
struct UtsState {
    nodename: [u8; UTS_FIELD_LEN],
    nodename_len: usize,
    domainname: [u8; UTS_FIELD_LEN],
    domainname_len: usize,
}

const fn copy_uts_field(dst: &mut [u8; UTS_FIELD_LEN], src: &[u8]) -> usize {
    let len = if src.len() < UTS_FIELD_LEN {
        src.len()
    } else {
        UTS_FIELD_LEN
    };
    let mut index = 0;
    while index < len {
        dst[index] = src[index];
        index += 1;
    }
    len
}

const fn init_uts_state() -> UtsState {
    let mut state = UtsState {
        nodename: [0; UTS_FIELD_LEN],
        nodename_len: 0,
        domainname: [0; UTS_FIELD_LEN],
        domainname_len: 0,
    };
    state.nodename_len = copy_uts_field(&mut state.nodename, b"starry");
    state.domainname_len = copy_uts_field(
        &mut state.domainname,
        b"https://github.com/Starry-OS/StarryOS",
    );
    state
}

impl UtsState {
    fn set_nodename(&mut self, value: &[u8]) {
        self.nodename = [0; UTS_FIELD_LEN];
        self.nodename_len = copy_uts_field(&mut self.nodename, value);
    }

    fn set_domainname(&mut self, value: &[u8]) {
        self.domainname = [0; UTS_FIELD_LEN];
        self.domainname_len = copy_uts_field(&mut self.domainname, value);
    }
}

pub(crate) struct UtsNamespace {
    state: SpinNoIrq<UtsState>,
}

impl UtsNamespace {
    pub(crate) fn new_default() -> Self {
        Self {
            state: SpinNoIrq::new(init_uts_state()),
        }
    }

    pub(crate) fn fork(&self) -> Arc<Self> {
        Arc::new(Self {
            state: SpinNoIrq::new(*self.state.lock()),
        })
    }

    pub(crate) fn nodename(&self) -> Vec<u8> {
        let state = self.state.lock();
        state.nodename[..state.nodename_len].to_vec()
    }

    pub(crate) fn domainname(&self) -> Vec<u8> {
        let state = self.state.lock();
        state.domainname[..state.domainname_len].to_vec()
    }

    pub(crate) fn set_nodename(&self, value: &[u8]) {
        self.state.lock().set_nodename(value);
    }

    pub(crate) fn set_domainname(&self, value: &[u8]) {
        self.state.lock().set_domainname(value);
    }
}

#[derive(Clone, Copy, Default)]
struct TimeNamespaceState {
    monotonic_offset_ns: i64,
    boottime_offset_ns: i64,
}

pub(crate) struct TimeNamespace {
    state: SpinNoIrq<TimeNamespaceState>,
}

impl TimeNamespace {
    pub(crate) fn new_default() -> Self {
        Self {
            state: SpinNoIrq::new(TimeNamespaceState::default()),
        }
    }

    pub(crate) fn fork(&self) -> Arc<Self> {
        Arc::new(Self {
            state: SpinNoIrq::new(*self.state.lock()),
        })
    }

    fn offset_ns(&self, boottime: bool) -> i64 {
        let state = self.state.lock();
        if boottime {
            state.boottime_offset_ns
        } else {
            state.monotonic_offset_ns
        }
    }

    pub(crate) fn apply_monotonic_offset(&self, value: Duration) -> Duration {
        apply_time_offset(value, self.offset_ns(false))
    }

    pub(crate) fn apply_boottime_offset(&self, value: Duration) -> Duration {
        apply_time_offset(value, self.offset_ns(true))
    }

    pub(crate) fn host_monotonic_deadline(&self, value: Duration) -> Duration {
        apply_time_offset(value, self.offset_ns(false).saturating_neg())
    }

    pub(crate) fn host_boottime_deadline(&self, value: Duration) -> Duration {
        apply_time_offset(value, self.offset_ns(true).saturating_neg())
    }

    pub(crate) fn set_monotonic_offset(&self, secs: i64, nsecs: u32) {
        self.state.lock().monotonic_offset_ns = offset_to_nanos(secs, nsecs);
    }

    pub(crate) fn set_boottime_offset(&self, secs: i64, nsecs: u32) {
        self.state.lock().boottime_offset_ns = offset_to_nanos(secs, nsecs);
    }

    pub(crate) fn render_offsets(&self) -> Vec<u8> {
        let state = self.state.lock();
        let (mono_sec, mono_nsec) = nanos_to_offset(state.monotonic_offset_ns);
        let (boot_sec, boot_nsec) = nanos_to_offset(state.boottime_offset_ns);
        format!("monotonic  {mono_sec:10} {mono_nsec:9}\nboottime   {boot_sec:10} {boot_nsec:9}\n")
            .into_bytes()
    }
}

fn offset_to_nanos(secs: i64, nsecs: u32) -> i64 {
    secs.saturating_mul(1_000_000_000)
        .saturating_add(nsecs as i64)
}

fn nanos_to_offset(nanos: i64) -> (i64, u32) {
    let secs = nanos.div_euclid(1_000_000_000);
    let nsecs = nanos.rem_euclid(1_000_000_000) as u32;
    (secs, nsecs)
}

fn apply_time_offset(value: Duration, offset_ns: i64) -> Duration {
    let adjusted = value.as_nanos() as i128 + offset_ns as i128;
    Duration::from_nanos(adjusted.clamp(0, u64::MAX as i128) as u64)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mempolicy {
    pub mode: u32,
    pub nodemask: usize,
}

impl Mempolicy {
    pub const fn new(mode: u32, nodemask: usize) -> Self {
        Self { mode, nodemask }
    }
}

#[derive(Clone, Copy, Debug)]
struct MempolicyRange {
    start: usize,
    end: usize,
    policy: Mempolicy,
}

#[derive(Clone, Debug)]
struct MempolicyState {
    process_policy: Mempolicy,
    ranges: Vec<MempolicyRange>,
}

impl Default for MempolicyState {
    fn default() -> Self {
        Self {
            process_policy: Mempolicy::new(0, 0),
            ranges: Vec::new(),
        }
    }
}

impl MempolicyState {
    fn first_node(mask: usize) -> Option<usize> {
        (mask != 0).then(|| mask.trailing_zeros() as usize)
    }

    fn node_mask(node: usize) -> usize {
        1usize.checked_shl(node as u32).unwrap_or(0)
    }

    fn node_ordinal(mask: usize, node: usize) -> Option<usize> {
        let mut ordinal = 0usize;
        for candidate in 0..usize::BITS as usize {
            if mask & Self::node_mask(candidate) == 0 {
                continue;
            }
            if candidate == node {
                return Some(ordinal);
            }
            ordinal += 1;
        }
        None
    }

    fn nth_node(mask: usize, ordinal: usize) -> Option<usize> {
        let mut seen = 0usize;
        for candidate in 0..usize::BITS as usize {
            if mask & Self::node_mask(candidate) == 0 {
                continue;
            }
            if seen == ordinal {
                return Some(candidate);
            }
            seen += 1;
        }
        None
    }

    fn migration_destination(
        old_mask: usize,
        new_mask: usize,
        source_node: usize,
    ) -> Option<usize> {
        let source_mask = Self::node_mask(source_node);
        if old_mask & source_mask == 0 || new_mask == 0 {
            return None;
        }

        if old_mask == new_mask {
            return None;
        }

        if old_mask.count_ones() == new_mask.count_ones() {
            return Self::node_ordinal(old_mask, source_node)
                .and_then(|ordinal| Self::nth_node(new_mask, ordinal));
        }

        if new_mask & source_mask != 0 {
            return None;
        }

        Self::first_node(new_mask)
    }

    fn migrate_policy(policy: &mut Mempolicy, old_mask: usize, new_mask: usize) -> bool {
        let source_node = Self::first_node(policy.nodemask).unwrap_or(0);
        let Some(dest_node) = Self::migration_destination(old_mask, new_mask, source_node) else {
            return false;
        };
        if dest_node == source_node {
            return false;
        }
        policy.nodemask = Self::node_mask(dest_node);
        true
    }

    fn migrate_ranges(&mut self, old_mask: usize, new_mask: usize) -> usize {
        let mut migrated = 0;
        for range in &mut self.ranges {
            migrated += usize::from(Self::migrate_policy(&mut range.policy, old_mask, new_mask));
        }
        migrated
    }

    fn remove_range(&mut self, start: usize, end: usize) {
        let old_ranges = core::mem::take(&mut self.ranges);
        for range in old_ranges {
            if range.end <= start || range.start >= end {
                self.ranges.push(range);
                continue;
            }
            if range.start < start {
                self.ranges.push(MempolicyRange {
                    start: range.start,
                    end: start,
                    policy: range.policy,
                });
            }
            if range.end > end {
                self.ranges.push(MempolicyRange {
                    start: end,
                    end: range.end,
                    policy: range.policy,
                });
            }
        }
    }

    fn policy_for_addr(&self, addr: usize) -> Option<Mempolicy> {
        self.ranges
            .iter()
            .rev()
            .find(|range| addr >= range.start && addr < range.end)
            .map(|range| range.policy)
    }
}

/// [`Process`]-shared data.
pub struct ProcessData {
    /// The process.
    pub proc: Arc<Process>,
    /// The executable path
    pub exe_path: RwLock<String>,
    /// The inode currently held busy as this process image.
    pub(crate) executable: SpinNoIrq<Option<ExecutableKey>>,
    /// The command line arguments
    pub cmdline: RwLock<Arc<Vec<String>>>,
    /// Realtime process creation timestamp, in seconds.
    start_realtime_sec: u64,
    /// Monotonic process creation timestamp, in nanoseconds.
    start_monotonic_ns: u64,
    /// The virtual memory address space.
    // TODO: scopify
    aspace_handle: RwLock<Arc<Mutex<AddrSpace>>>,
    /// The resource scope
    pub scope: RwLock<Scope>,
    /// The user heap top
    heap_top: AtomicUsize,

    /// The resource limits
    pub rlim: RwLock<Rlimits>,

    /// The child exit wait event
    pub child_exit_event: Arc<PollSet>,
    /// Self exit event
    pub exit_event: Arc<PollSet>,
    /// Woken when exec de-thread state changes or a sibling exits.
    pub exec_event: Arc<PollSet>,
    /// The exit signal of the thread
    pub exit_signal: Option<Signo>,

    /// The process signal manager
    pub signal: Arc<ProcessSignalManager>,

    /// The futex table.
    pub(in crate::task) futex_table: Arc<FutexTable>,

    /// The default mask for file permissions.
    umask: AtomicU32,
    /// Process credentials shared by all threads.
    creds: SpinNoIrq<Credentials>,
    /// Process capabilities shared by all threads.
    caps: SpinNoIrq<CapabilityState>,
    /// Supplementary group IDs shared by all threads in the process.
    supplementary_groups: SpinNoIrq<Vec<u32>>,
    /// Linux personality flags shared by all threads in the process.
    personality: AtomicU32,
    /// Raw Linux I/O priority value configured through ioprio_set(2).
    ioprio: AtomicU32,
    /// Linux I/O context identity shared by CLONE_IO.
    pub(crate) io_context: Arc<()>,
    /// System V semaphore undo-list identity shared by CLONE_SYSVSEM.
    pub(crate) sysvsem_undo: Arc<()>,
    /// NUMA memory policy state for the single-node kernel memory model.
    mempolicy: SpinNoIrq<MempolicyState>,
    /// Parent-death signal configured through prctl(PR_SET_PDEATHSIG).
    pdeath_signal: AtomicU32,
    /// Current timer slack in nanoseconds.
    timerslack_current_ns: AtomicUsize,
    /// Default timer slack in nanoseconds, used when PR_SET_TIMERSLACK is 0.
    timerslack_default_ns: AtomicUsize,
    /// no_new_privs state configured through prctl(PR_SET_NO_NEW_PRIVS).
    no_new_privs: AtomicU32,
    /// Seccomp mode reported through prctl(PR_GET_SECCOMP).
    seccomp_mode: AtomicU32,
    /// Transparent huge-page disable flag reported through prctl(PR_GET_THP_DISABLE).
    thp_disabled: AtomicU32,
    /// Process-scoped membarrier registration state.
    membarrier_state: AtomicU32,
    /// POSIX interval timers created by this process.
    pub(crate) posix_timers: SpinNoIrq<Vec<Option<PosixTimer>>>,

    /// CPU time accumulated from sibling threads that have already exited.
    pub(in crate::task) exited_threads_usage: AtomicTaskUsage,
    /// CPU time accumulated from waited-for child subtrees.
    waited_children_usage: AtomicTaskUsage,
    /// Maximum resident set size observed for this process, in kilobytes.
    maxrss_kb: AtomicU64,

    /// Serializes wait* selection and consumption for this process.
    pub wait_lock: Mutex<()>,

    /// Job-control stop state shared by all threads in the process.
    job_ctl: SpinNoIrq<JobControlState>,
    /// ptrace ownership and options shared by all threads in the process.
    ptrace_ctl: SpinNoIrq<PtraceControlState>,
    /// Multi-thread exec coordination state.
    exec_ctl: SpinNoIrq<ExecControlState>,
    /// CLONE_VFORK coordination state.
    vfork_ctl: SpinNoIrq<VforkControlState>,
    /// Woken when threads should resume from stopped state.
    pub stop_event: Arc<PollSet>,
    /// Woken when a vfork child releases the parent.
    pub vfork_event: Arc<PollSet>,

    /// The network namespace (network stack) for this process.
    pub net_ns: Arc<NetStack>,
    /// Lightweight cgroup namespace identity for cgroup.procs open-time checks.
    cgroup_ns: Arc<CgroupNamespace>,
    /// Lightweight PID namespace identity for procfs namespace fd ABI.
    pid_ns: Arc<PidNamespace>,
    /// Lightweight user namespace identity for procfs namespace fd ABI.
    user_ns: Arc<UserNamespace>,
    /// The UTS namespace for this process.
    uts_ns: RwLock<Arc<UtsNamespace>>,
    /// The time namespace visible to this process.
    time_ns: RwLock<Arc<TimeNamespace>>,
    /// The time namespace inherited by children created after unshare/setns.
    time_ns_for_children: RwLock<Arc<TimeNamespace>>,
}

impl ProcessData {
    /// Create a new [`ProcessData`].
    pub(crate) fn new(
        proc: Arc<Process>,
        exe_path: String,
        executable: Option<ExecutableKey>,
        cmdline: Arc<Vec<String>>,
        aspace: Arc<Mutex<AddrSpace>>,
        signal_actions: Arc<SpinNoIrq<SignalActions>>,
        exit_signal: Option<Signo>,
        net_ns: Arc<NetStack>,
        cgroup_ns: Arc<CgroupNamespace>,
        pid_ns: Arc<PidNamespace>,
        user_ns: Arc<UserNamespace>,
        uts_ns: Arc<UtsNamespace>,
        time_ns: Arc<TimeNamespace>,
        io_context: Arc<()>,
        sysvsem_undo: Arc<()>,
    ) -> Arc<Self> {
        let start_realtime_sec = wall_time().as_secs();
        let start_monotonic_ns = monotonic_time_nanos();

        Arc::new(Self {
            proc,
            exe_path: RwLock::new(exe_path),
            executable: SpinNoIrq::new(executable),
            cmdline: RwLock::new(cmdline),
            start_realtime_sec,
            start_monotonic_ns,
            aspace_handle: RwLock::new(aspace),
            scope: RwLock::new(Scope::new()),
            heap_top: AtomicUsize::new(
                crate::config::USER_HEAP_BASE + crate::config::USER_HEAP_SIZE,
            ),

            rlim: RwLock::default(),

            child_exit_event: Arc::default(),
            exit_event: Arc::default(),
            exec_event: Arc::default(),
            exit_signal,

            signal: Arc::new(ProcessSignalManager::new(
                signal_actions,
                crate::config::SIGNAL_TRAMPOLINE,
            )),

            futex_table: Arc::new(FutexTable::new()),

            umask: AtomicU32::new(0o022),
            creds: SpinNoIrq::new(Credentials::default()),
            caps: SpinNoIrq::new(CapabilityState::full()),
            supplementary_groups: SpinNoIrq::new(vec![0]),
            personality: AtomicU32::new(0),
            ioprio: AtomicU32::new(0),
            io_context,
            sysvsem_undo,
            mempolicy: SpinNoIrq::new(MempolicyState::default()),
            pdeath_signal: AtomicU32::new(0),
            timerslack_current_ns: AtomicUsize::new(50_000),
            timerslack_default_ns: AtomicUsize::new(50_000),
            no_new_privs: AtomicU32::new(0),
            seccomp_mode: AtomicU32::new(0),
            thp_disabled: AtomicU32::new(0),
            membarrier_state: AtomicU32::new(0),
            posix_timers: SpinNoIrq::new(Vec::new()),
            exited_threads_usage: AtomicTaskUsage::new(),
            waited_children_usage: AtomicTaskUsage::new(),
            maxrss_kb: AtomicU64::new(0),
            wait_lock: Mutex::new(()),

            job_ctl: SpinNoIrq::new(JobControlState::default()),
            ptrace_ctl: SpinNoIrq::new(PtraceControlState::default()),
            exec_ctl: SpinNoIrq::new(ExecControlState::default()),
            vfork_ctl: SpinNoIrq::new(VforkControlState::default()),
            stop_event: Arc::default(),
            vfork_event: Arc::default(),

            net_ns,
            cgroup_ns,
            pid_ns,
            user_ns,
            uts_ns: RwLock::new(uts_ns),
            time_ns: RwLock::new(time_ns.clone()),
            time_ns_for_children: RwLock::new(time_ns),
        })
    }

    /// Get the top address of the user heap.
    pub fn get_heap_top(&self) -> usize {
        self.heap_top.load(Ordering::Acquire)
    }

    /// Returns the current address-space handle for this process.
    pub fn aspace(&self) -> Arc<Mutex<AddrSpace>> {
        self.aspace_handle.read().clone()
    }

    /// Rebinds the process to a new address-space handle and returns the old one.
    pub fn replace_aspace(&self, aspace: Arc<Mutex<AddrSpace>>) -> Arc<Mutex<AddrSpace>> {
        core::mem::replace(&mut *self.aspace_handle.write(), aspace)
    }

    pub(crate) fn uts_ns(&self) -> Arc<UtsNamespace> {
        self.uts_ns.read().clone()
    }

    pub(crate) fn cgroup_ns(&self) -> Arc<CgroupNamespace> {
        self.cgroup_ns.clone()
    }

    pub(crate) fn cgroup_ns_id(&self) -> u64 {
        self.cgroup_ns.id()
    }

    pub(crate) fn pid_ns(&self) -> Arc<PidNamespace> {
        self.pid_ns.clone()
    }

    pub(crate) fn user_ns(&self) -> Arc<UserNamespace> {
        self.user_ns.clone()
    }

    pub(crate) fn replace_uts_ns(&self, uts_ns: Arc<UtsNamespace>) {
        *self.uts_ns.write() = uts_ns;
    }

    pub(crate) fn time_ns(&self) -> Arc<TimeNamespace> {
        self.time_ns.read().clone()
    }

    pub(crate) fn time_ns_for_children(&self) -> Arc<TimeNamespace> {
        self.time_ns_for_children.read().clone()
    }

    pub(crate) fn replace_time_ns(&self, time_ns: Arc<TimeNamespace>) {
        *self.time_ns.write() = time_ns.clone();
        *self.time_ns_for_children.write() = time_ns;
    }

    pub(crate) fn unshare_time_ns(&self) {
        let new_ns = self.time_ns_for_children().fork();
        *self.time_ns_for_children.write() = new_ns;
    }

    pub(crate) fn start_realtime_sec(&self) -> u64 {
        self.start_realtime_sec
    }

    pub(crate) fn start_monotonic_ns(&self) -> u64 {
        self.start_monotonic_ns
    }

    pub(crate) fn executable(&self) -> Option<ExecutableKey> {
        *self.executable.lock()
    }

    pub(crate) fn retain_executable(&self) -> Option<ExecutableKey> {
        executable::retain(self.executable())
    }

    pub(crate) fn replace_executable(&self, new_executable: Option<ExecutableKey>) {
        let old_executable = core::mem::replace(&mut *self.executable.lock(), new_executable);
        executable::release(old_executable);
    }

    pub(crate) fn release_executable(&self) {
        self.replace_executable(None);
    }

    /// Set the top address of the user heap.
    pub fn set_heap_top(&self, top: usize) {
        self.heap_top.store(top, Ordering::Release)
    }

    pub fn pdeath_signal(&self) -> u32 {
        self.pdeath_signal.load(Ordering::Acquire)
    }

    pub fn set_pdeath_signal(&self, signo: u32) {
        self.pdeath_signal.store(signo, Ordering::Release)
    }

    pub fn timerslack_ns(&self) -> usize {
        self.timerslack_current_ns.load(Ordering::Acquire)
    }

    pub fn set_timerslack_ns(&self, value: usize) {
        let value = if value == 0 {
            self.timerslack_default_ns.load(Ordering::Acquire)
        } else {
            value
        };
        self.timerslack_current_ns.store(value, Ordering::Release)
    }

    pub fn inherit_timerslack_from(&self, parent: &Self) {
        let value = parent.timerslack_ns();
        self.timerslack_current_ns.store(value, Ordering::Release);
        self.timerslack_default_ns.store(value, Ordering::Release);
    }

    pub fn no_new_privs(&self) -> bool {
        self.no_new_privs.load(Ordering::Acquire) != 0
    }

    pub fn set_no_new_privs(&self) {
        self.no_new_privs.store(1, Ordering::Release)
    }

    pub fn seccomp_mode(&self) -> u32 {
        self.seccomp_mode.load(Ordering::Acquire)
    }

    pub fn set_seccomp_mode(&self, mode: u32) {
        self.seccomp_mode.store(mode, Ordering::Release)
    }

    pub fn thp_disabled(&self) -> bool {
        self.thp_disabled.load(Ordering::Acquire) != 0
    }

    pub fn set_thp_disabled(&self, disabled: bool) {
        self.thp_disabled.store(disabled as u32, Ordering::Release)
    }

    /// Linux manual: A "clone" child is one which delivers no signal, or a
    /// signal other than SIGCHLD to its parent upon termination.
    pub fn is_clone_child(&self) -> bool {
        self.exit_signal != Some(Signo::SIGCHLD)
    }

    /// Returns process CPU usage, including live threads and exited siblings.
    pub fn self_usage(&self) -> super::accounting::TaskUsage {
        live_process_usage(self).with_maxrss_floor(self.sample_maxrss_kb())
    }

    /// Returns waited-for child CPU usage accumulated for this process.
    pub fn children_usage(&self) -> super::accounting::TaskUsage {
        self.waited_children_usage.snapshot()
    }

    /// Returns the total usage that should be published when this process exits.
    pub fn total_usage(&self) -> super::accounting::TaskUsage {
        self.self_usage().saturating_add(self.children_usage())
    }

    /// Records the final CPU usage of a thread that is exiting.
    pub fn account_exited_thread(&self, usage: super::accounting::TaskUsage) {
        self.exited_threads_usage.add(usage);
    }

    /// Records a waited-for child subtree into the process's child ledger.
    pub fn account_waited_child(&self, usage: super::accounting::TaskUsage) {
        self.waited_children_usage.add(usage);
    }

    fn sample_maxrss_kb(&self) -> u64 {
        let resident_kb = self.aspace().lock().resident_user_bytes() as u64 / 1024;
        let mut current = self.maxrss_kb.load(Ordering::Acquire);
        while resident_kb > current {
            match self.maxrss_kb.compare_exchange_weak(
                current,
                resident_kb,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return resident_kb,
                Err(observed) => current = observed,
            }
        }
        current
    }

    /// Get the umask.
    pub fn umask(&self) -> u32 {
        self.umask.load(Ordering::SeqCst)
    }

    /// Set the umask.
    pub fn set_umask(&self, umask: u32) {
        self.umask.store(umask, Ordering::SeqCst);
    }

    /// Set the umask and return the old value.
    pub fn replace_umask(&self, umask: u32) -> u32 {
        self.umask.swap(umask, Ordering::SeqCst)
    }

    pub(crate) fn credentials(&self) -> Credentials {
        *self.creds.lock()
    }

    pub(crate) fn set_credentials(&self, creds: Credentials) {
        *self.creds.lock() = creds;
    }

    pub(crate) fn capability_state(&self) -> CapabilityState {
        *self.caps.lock()
    }

    pub(crate) fn set_capability_state(&self, caps: CapabilityState) {
        *self.caps.lock() = caps;
    }

    pub fn supplementary_groups(&self) -> Vec<u32> {
        self.supplementary_groups.lock().clone()
    }

    pub fn set_supplementary_groups(&self, groups: Vec<u32>) {
        *self.supplementary_groups.lock() = groups;
    }

    pub fn personality(&self) -> u32 {
        self.personality.load(Ordering::Acquire)
    }

    pub fn set_personality(&self, personality: u32) {
        self.personality.store(personality, Ordering::Release);
    }

    pub fn ioprio(&self) -> u32 {
        self.ioprio.load(Ordering::Acquire)
    }

    pub fn set_ioprio(&self, ioprio: u32) {
        self.ioprio.store(ioprio, Ordering::Release);
    }

    pub fn mempolicy(&self) -> Mempolicy {
        self.mempolicy.lock().process_policy
    }

    pub fn set_mempolicy(&self, policy: Mempolicy) {
        self.mempolicy.lock().process_policy = policy;
    }

    pub fn inherit_mempolicy_from(&self, parent: &Self) {
        let parent_state = parent.mempolicy.lock().clone();
        *self.mempolicy.lock() = parent_state;
    }

    pub fn bind_mempolicy_range(&self, start: usize, size: usize, policy: Mempolicy) {
        if size == 0 {
            return;
        }
        let Some(end) = start.checked_add(size) else {
            return;
        };
        let mut state = self.mempolicy.lock();
        state.remove_range(start, end);
        state.ranges.push(MempolicyRange { start, end, policy });
    }

    pub fn clear_mempolicy_range(&self, start: usize, size: usize) {
        if size == 0 {
            return;
        }
        let Some(end) = start.checked_add(size) else {
            return;
        };
        self.mempolicy.lock().remove_range(start, end);
    }

    pub fn clear_mempolicy_ranges(&self) {
        self.mempolicy.lock().ranges.clear();
    }

    pub fn migrate_mempolicy_ranges(&self, old_mask: usize, new_mask: usize) -> usize {
        self.mempolicy.lock().migrate_ranges(old_mask, new_mask)
    }

    pub fn mempolicy_for_addr(&self, addr: usize) -> Option<Mempolicy> {
        self.mempolicy.lock().policy_for_addr(addr)
    }

    pub fn uid(&self) -> u32 {
        self.creds.lock().ruid
    }

    pub fn euid(&self) -> u32 {
        self.creds.lock().euid
    }

    pub fn gid(&self) -> u32 {
        self.creds.lock().rgid
    }

    pub fn egid(&self) -> u32 {
        self.creds.lock().egid
    }

    pub fn suid(&self) -> u32 {
        self.creds.lock().suid
    }

    pub fn fsuid(&self) -> u32 {
        self.creds.lock().fsuid
    }

    pub fn sgid(&self) -> u32 {
        self.creds.lock().sgid
    }

    pub fn fsgid(&self) -> u32 {
        self.creds.lock().fsgid
    }

    pub fn is_in_group(&self, gid: u32) -> bool {
        self.egid() == gid || self.supplementary_groups.lock().contains(&gid)
    }

    pub fn is_in_fs_group(&self, gid: u32) -> bool {
        self.fsgid() == gid || self.supplementary_groups.lock().contains(&gid)
    }

    pub fn has_effective_capability(&self, cap: u32) -> bool {
        self.capability_state().has_effective(cap)
    }

    pub fn register_membarrier(&self, flags: u32) {
        self.membarrier_state.fetch_or(flags, Ordering::Relaxed);
    }

    pub fn membarrier_registrations(&self) -> u32 {
        self.membarrier_state.load(Ordering::Relaxed)
    }

    pub fn membarrier_registered(&self, flags: u32) -> bool {
        self.membarrier_state.load(Ordering::Relaxed) & flags == flags
    }

    pub fn clear_membarrier_registrations(&self) {
        self.membarrier_state.store(0, Ordering::SeqCst);
    }

    pub fn bounding_capability_enabled(&self, cap: u32) -> AxResult<bool> {
        if CapabilityState::cap_mask(cap).is_none() {
            return Err(AxError::InvalidInput);
        }
        Ok(self.capability_state().bounding_contains(cap))
    }

    pub fn drop_bounding_capability(&self, cap: u32) -> AxResult<()> {
        self.caps.lock().drop_bounding(cap)
    }

    pub fn ambient_capability_enabled(&self, cap: u32) -> AxResult<bool> {
        if CapabilityState::cap_mask(cap).is_none() {
            return Err(AxError::InvalidInput);
        }
        Ok(self.capability_state().ambient_contains(cap))
    }

    pub fn raise_ambient_capability(&self, cap: u32) -> AxResult<()> {
        let Some((word, mask)) = CapabilityState::cap_mask(cap) else {
            return Err(AxError::InvalidInput);
        };
        let mut caps = self.caps.lock();
        if caps.securebits & SECBIT_NO_CAP_AMBIENT_RAISE != 0
            || caps.permitted[word] & mask == 0
            || caps.inheritable[word] & mask == 0
        {
            return Err(AxError::OperationNotPermitted);
        }
        caps.raise_ambient(cap)
    }

    pub fn lower_ambient_capability(&self, cap: u32) -> AxResult<()> {
        self.caps.lock().lower_ambient(cap)
    }

    pub fn clear_ambient_capabilities(&self) {
        self.caps.lock().clear_ambient();
    }

    pub fn securebits(&self) -> u32 {
        self.caps.lock().securebits
    }

    pub fn set_securebits(&self, securebits: u32) -> AxResult<()> {
        let mut caps = self.caps.lock();
        if (((caps.securebits & SECURE_ALL_LOCKS) >> 1) & (caps.securebits ^ securebits)) != 0
            || (caps.securebits & SECURE_ALL_LOCKS & !securebits) != 0
            || (securebits & !(SECURE_ALL_LOCKS | SECURE_ALL_BITS)) != 0
        {
            return Err(AxError::OperationNotPermitted);
        }
        caps.securebits = securebits;
        Ok(())
    }

    pub fn keep_caps(&self) -> bool {
        self.caps.lock().securebits & SECBIT_KEEP_CAPS != 0
    }

    pub fn set_keep_caps(&self, enabled: bool) -> AxResult<()> {
        let mut caps = self.caps.lock();
        if caps.securebits & SECBIT_KEEP_CAPS_LOCKED != 0 {
            return Err(AxError::OperationNotPermitted);
        }
        if enabled {
            caps.securebits |= SECBIT_KEEP_CAPS;
        } else {
            caps.securebits &= !SECBIT_KEEP_CAPS;
        }
        Ok(())
    }

    pub fn clear_keep_caps_on_exec(&self) {
        self.caps.lock().securebits &= !SECBIT_KEEP_CAPS;
    }

    fn fixup_capabilities_for_uid_change(&self, old: Credentials, new: Credentials) {
        if old.ruid == new.ruid && old.euid == new.euid && old.suid == new.suid {
            return;
        }

        let mut caps = self.caps.lock();
        if caps.securebits & SECBIT_NO_SETUID_FIXUP != 0 {
            return;
        }
        if old.euid == 0 && new.euid != 0 {
            caps.effective = [0; CAPABILITY_WORDS];
            if old.ruid == 0
                && old.suid == 0
                && new.ruid != 0
                && new.euid != 0
                && new.suid != 0
                && caps.securebits & SECBIT_KEEP_CAPS == 0
            {
                caps.permitted = [0; CAPABILITY_WORDS];
                caps.clear_ambient();
            }
        } else if old.euid != 0 && new.euid == 0 {
            caps.effective = caps.permitted;
        }
    }

    fn fixup_capabilities_for_fsuid_change(&self, old_fsuid: u32, new_fsuid: u32) {
        if old_fsuid == new_fsuid {
            return;
        }

        const FS_CAPS: [u32; 8] = [
            CAP_CHOWN,
            CAP_MKNOD,
            CAP_DAC_OVERRIDE,
            CAP_DAC_READ_SEARCH,
            CAP_FOWNER,
            CAP_FSETID,
            CAP_MAC_OVERRIDE,
            CAP_LINUX_IMMUTABLE,
        ];

        let mut caps = self.caps.lock();
        if caps.securebits & SECBIT_NO_SETUID_FIXUP != 0 {
            return;
        }
        for cap in FS_CAPS {
            let Some((word, mask)) = CapabilityState::cap_mask(cap) else {
                continue;
            };
            if old_fsuid == 0 && new_fsuid != 0 {
                caps.effective[word] &= !mask;
            } else if old_fsuid != 0 && new_fsuid == 0 && caps.permitted[word] & mask != 0 {
                caps.effective[word] |= mask;
            }
        }
    }

    pub fn setuid(&self, uid: u32) -> AxResult<()> {
        let can_setuid = self.has_effective_capability(CAP_SETUID);
        let mut creds = self.creds.lock();
        let old = *creds;
        if can_setuid {
            creds.ruid = uid;
            creds.euid = uid;
            creds.suid = uid;
            creds.fsuid = uid;
            let new = *creds;
            drop(creds);
            self.fixup_capabilities_for_uid_change(old, new);
            return Ok(());
        }
        if uid == creds.ruid || uid == creds.suid {
            creds.euid = uid;
            creds.fsuid = uid;
            let new = *creds;
            drop(creds);
            self.fixup_capabilities_for_uid_change(old, new);
            return Ok(());
        }
        Err(AxError::OperationNotPermitted)
    }

    pub fn setgid(&self, gid: u32) -> AxResult<()> {
        let can_setgid = self.has_effective_capability(CAP_SETGID);
        let mut creds = self.creds.lock();
        if can_setgid {
            creds.rgid = gid;
            creds.egid = gid;
            creds.sgid = gid;
            creds.fsgid = gid;
            return Ok(());
        }
        if gid == creds.rgid || gid == creds.sgid {
            creds.egid = gid;
            creds.fsgid = gid;
            return Ok(());
        }
        Err(AxError::OperationNotPermitted)
    }

    pub fn setreuid(&self, ruid: Option<u32>, euid: Option<u32>) -> AxResult<()> {
        let can_setuid = self.has_effective_capability(CAP_SETUID);
        let mut creds = self.creds.lock();
        let old = *creds;
        if !can_setuid {
            if let Some(id) = ruid
                && id != old.ruid
                && id != old.euid
            {
                return Err(AxError::OperationNotPermitted);
            }
            if let Some(id) = euid
                && id != old.ruid
                && id != old.euid
                && id != old.suid
            {
                return Err(AxError::OperationNotPermitted);
            }
        }

        let new_ruid = ruid.unwrap_or(old.ruid);
        let new_euid = euid.unwrap_or(old.euid);
        creds.ruid = new_ruid;
        creds.euid = new_euid;
        creds.fsuid = new_euid;
        if ruid.is_some() || euid.is_some_and(|id| id != old.ruid) {
            creds.suid = new_euid;
        }
        let new = *creds;
        drop(creds);
        self.fixup_capabilities_for_uid_change(old, new);
        Ok(())
    }

    pub fn setregid(&self, rgid: Option<u32>, egid: Option<u32>) -> AxResult<()> {
        let can_setgid = self.has_effective_capability(CAP_SETGID);
        let mut creds = self.creds.lock();
        let old = *creds;
        if !can_setgid {
            if let Some(id) = rgid
                && id != old.rgid
                && id != old.egid
            {
                return Err(AxError::OperationNotPermitted);
            }
            if let Some(id) = egid
                && id != old.rgid
                && id != old.egid
                && id != old.sgid
            {
                return Err(AxError::OperationNotPermitted);
            }
        }

        let new_rgid = rgid.unwrap_or(old.rgid);
        let new_egid = egid.unwrap_or(old.egid);
        creds.rgid = new_rgid;
        creds.egid = new_egid;
        creds.fsgid = new_egid;
        if rgid.is_some() || egid.is_some_and(|id| id != old.rgid) {
            creds.sgid = new_egid;
        }
        Ok(())
    }

    pub fn setresuid(
        &self,
        ruid: Option<u32>,
        euid: Option<u32>,
        suid: Option<u32>,
    ) -> AxResult<()> {
        let can_setuid = self.has_effective_capability(CAP_SETUID);
        let mut creds = self.creds.lock();
        let old = *creds;
        if !can_setuid {
            for id in [ruid, euid, suid].into_iter().flatten() {
                if id != old.ruid && id != old.euid && id != old.suid {
                    return Err(AxError::OperationNotPermitted);
                }
            }
        }

        if let Some(id) = ruid {
            creds.ruid = id;
        }
        if let Some(id) = euid {
            creds.euid = id;
        }
        if let Some(id) = suid {
            creds.suid = id;
        }
        creds.fsuid = creds.euid;
        let new = *creds;
        drop(creds);
        self.fixup_capabilities_for_uid_change(old, new);
        Ok(())
    }

    pub fn setresgid(
        &self,
        rgid: Option<u32>,
        egid: Option<u32>,
        sgid: Option<u32>,
    ) -> AxResult<()> {
        let can_setgid = self.has_effective_capability(CAP_SETGID);
        let mut creds = self.creds.lock();
        let old = *creds;
        if !can_setgid {
            for id in [rgid, egid, sgid].into_iter().flatten() {
                if id != old.rgid && id != old.egid && id != old.sgid {
                    return Err(AxError::OperationNotPermitted);
                }
            }
        }

        if let Some(id) = rgid {
            creds.rgid = id;
        }
        if let Some(id) = egid {
            creds.egid = id;
        }
        if let Some(id) = sgid {
            creds.sgid = id;
        }
        creds.fsgid = creds.egid;
        Ok(())
    }

    pub fn setfsuid(&self, fsuid: u32) -> u32 {
        let can_setuid = self.has_effective_capability(CAP_SETUID);
        let mut creds = self.creds.lock();
        let old_fsuid = creds.fsuid;
        if fsuid == u32::MAX {
            return old_fsuid;
        }
        if can_setuid
            || fsuid == creds.ruid
            || fsuid == creds.euid
            || fsuid == creds.suid
            || fsuid == creds.fsuid
        {
            creds.fsuid = fsuid;
        }
        let new_fsuid = creds.fsuid;
        drop(creds);
        self.fixup_capabilities_for_fsuid_change(old_fsuid, new_fsuid);
        old_fsuid
    }

    pub fn setfsgid(&self, fsgid: u32) -> u32 {
        let can_setgid = self.has_effective_capability(CAP_SETGID);
        let mut creds = self.creds.lock();
        let old_fsgid = creds.fsgid;
        if fsgid == u32::MAX {
            return old_fsgid;
        }
        if can_setgid
            || fsgid == creds.rgid
            || fsgid == creds.egid
            || fsgid == creds.sgid
            || fsgid == creds.fsgid
        {
            creds.fsgid = fsgid;
        }
        old_fsgid
    }

    pub fn ptrace_tracer(&self) -> Option<Pid> {
        self.ptrace_ctl.lock().tracer
    }

    pub fn ptrace_options(&self) -> u32 {
        self.ptrace_ctl.lock().options
    }

    pub fn ptrace_event_message(&self) -> usize {
        self.ptrace_ctl.lock().event_message
    }

    pub fn ptrace_set_options(&self, options: u32) {
        self.ptrace_ctl.lock().options = options;
    }

    pub fn ptrace_set_event_message(&self, event_message: usize) {
        self.ptrace_ctl.lock().event_message = event_message;
    }

    pub fn is_traced_by(&self, tracer: Pid) -> bool {
        self.ptrace_tracer() == Some(tracer)
    }

    pub fn begin_ptrace(&self, tracer: Pid) -> bool {
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        if ptrace_ctl.tracer.is_some() {
            return false;
        }
        ptrace_ctl.tracer = Some(tracer);
        ptrace_ctl.options = 0;
        ptrace_ctl.event_message = 0;
        true
    }

    pub fn end_ptrace(&self, tracer: Pid) -> bool {
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        if ptrace_ctl.tracer != Some(tracer) {
            return false;
        }
        *ptrace_ctl = PtraceControlState::default();
        true
    }

    pub fn clear_ptrace(&self) {
        *self.ptrace_ctl.lock() = PtraceControlState::default();
    }

    fn stop_state(&self) -> StopState {
        self.job_ctl.lock().state
    }

    /// Returns whether the process is currently stopped.
    pub fn is_stopped(&self) -> bool {
        self.stop_state() == StopState::Stopped
    }

    /// Returns whether threads should park for a job-control stop.
    pub fn should_wait_for_stop(&self) -> bool {
        self.stop_state() != StopState::Running
    }

    /// Begins a job-control stop transition.
    pub fn begin_stop(&self, signo: u8) -> bool {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state != StopState::Running {
            return false;
        }
        job_ctl.state = StopState::Stopping;
        job_ctl.stop_signal = signo;
        job_ctl.stop_kind = StopKind::JobControl;
        true
    }

    /// Finalizes a stop transition if it has not been canceled by SIGCONT.
    pub fn finish_stop(&self) -> bool {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state != StopState::Stopping {
            return false;
        }
        job_ctl.state = StopState::Stopped;
        job_ctl.stop_reported = false;
        job_ctl.continued = false;
        true
    }

    /// Stops a traced process at a signal-delivery or attach boundary.
    pub fn ptrace_stop(&self, signo: u8) -> bool {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state != StopState::Running {
            return false;
        }
        job_ctl.state = StopState::Stopped;
        job_ctl.stop_signal = signo;
        job_ctl.stop_kind = StopKind::Ptrace;
        job_ctl.stop_reported = false;
        job_ctl.continued = false;
        true
    }

    /// Resumes or cancels a job-control stop transition.
    pub(crate) fn continue_job(&self) -> ContinueResult {
        let result = {
            let mut job_ctl = self.job_ctl.lock();
            match job_ctl.state {
                StopState::Running => ContinueResult::None,
                StopState::Stopping => {
                    job_ctl.state = StopState::Running;
                    ContinueResult::CanceledStopping
                }
                StopState::Stopped => {
                    job_ctl.state = StopState::Running;
                    if job_ctl.stop_kind == StopKind::JobControl {
                        job_ctl.continued = true;
                    }
                    ContinueResult::ResumedStopped
                }
            }
        };
        if result != ContinueResult::None {
            self.stop_event.wake();
        }
        result
    }

    /// Atomically takes the continued flag (returns true at most once per continuation).
    pub fn take_continued(&self) -> bool {
        let mut job_ctl = self.job_ctl.lock();
        let continued = job_ctl.continued;
        job_ctl.continued = false;
        continued
    }

    /// Takes the current stopped status for waitpid reporting, if it has not been reported yet.
    pub(crate) fn take_stop_status(&self) -> Option<StopReport> {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state == StopState::Stopped && !job_ctl.stop_reported {
            job_ctl.stop_reported = true;
            Some(StopReport {
                signal: job_ctl.stop_signal,
                traced: job_ctl.stop_kind == StopKind::Ptrace,
            })
        } else {
            None
        }
    }

    /// Peeks at the stopped status without consuming it (for WNOWAIT).
    pub(crate) fn peek_stop_status(&self) -> Option<StopReport> {
        let job_ctl = self.job_ctl.lock();
        if job_ctl.state == StopState::Stopped && !job_ctl.stop_reported {
            Some(StopReport {
                signal: job_ctl.stop_signal,
                traced: job_ctl.stop_kind == StopKind::Ptrace,
            })
        } else {
            None
        }
    }

    /// Claims the pending stop report so a waiter can complete userspace copies first.
    pub(crate) fn claim_stop_status(&self) -> Option<StopReport> {
        self.take_stop_status()
    }

    /// Restores a previously claimed stop report after a failed userspace copy.
    pub(crate) fn restore_stop_status(&self, report: StopReport) {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state == StopState::Stopped
            && job_ctl.stop_signal == report.signal
            && (job_ctl.stop_kind == StopKind::Ptrace) == report.traced
        {
            job_ctl.stop_reported = false;
        }
    }

    /// Peeks at the continued flag without consuming it (for WNOWAIT).
    pub fn peek_continued(&self) -> bool {
        self.job_ctl.lock().continued
    }

    /// Claims the pending continued report so a waiter can complete userspace copies first.
    pub fn claim_continued(&self) -> bool {
        self.take_continued()
    }

    /// Restores a previously claimed continued report after a failed userspace copy.
    pub fn restore_continued(&self) {
        let mut job_ctl = self.job_ctl.lock();
        job_ctl.continued = true;
    }

    /// Begins a multi-thread exec de-threading phase.
    pub fn begin_exec(&self, owner: Pid) -> bool {
        let mut exec_ctl = self.exec_ctl.lock();
        match exec_ctl.owner {
            Some(curr) => curr == owner,
            None => {
                exec_ctl.owner = Some(owner);
                true
            }
        }
    }

    /// Returns whether this thread should exit because another thread is committing execve().
    pub fn should_exit_for_exec(&self, tid: Pid) -> bool {
        matches!(self.exec_ctl.lock().owner, Some(owner) if owner != tid)
    }

    /// Returns whether the given thread still owns the in-flight exec.
    pub fn is_exec_owner(&self, tid: Pid) -> bool {
        self.exec_ctl.lock().owner == Some(tid)
    }

    /// Returns whether an exec de-thread phase is currently in progress.
    pub fn exec_in_progress(&self) -> bool {
        self.exec_ctl.lock().owner.is_some()
    }

    /// Adds a thread to the process unless an exec de-thread phase is already
    /// in progress.
    pub fn try_add_thread(&self, tid: Pid) -> bool {
        let exec_ctl = self.exec_ctl.lock();
        if exec_ctl.owner.is_some() {
            return false;
        }
        self.proc.add_thread(tid);
        true
    }

    /// Returns whether the thread group has drained to the exec owner only.
    pub fn exec_ready(&self, owner: Pid) -> bool {
        self.is_exec_owner(owner) && self.proc.threads().as_slice() == [owner]
    }

    /// Finishes or cancels the in-flight exec owned by `owner`.
    pub fn end_exec(&self, owner: Pid) {
        let mut exec_ctl = self.exec_ctl.lock();
        if exec_ctl.owner == Some(owner) {
            exec_ctl.owner = None;
            drop(exec_ctl);
            self.exec_event.wake();
        }
    }

    /// Marks the process as a vfork child whose parent thread must remain blocked.
    pub fn begin_vfork(&self, parent_tid: Pid) {
        self.vfork_ctl.lock().parent_tid = Some(parent_tid);
    }

    /// Returns whether an active CLONE_VFORK relationship is still blocking the parent.
    pub fn vfork_in_progress(&self) -> bool {
        self.vfork_ctl.lock().parent_tid.is_some()
    }

    /// Releases a blocked vfork parent after execve commits or the last thread exits.
    pub fn release_vfork(&self) {
        let mut vfork_ctl = self.vfork_ctl.lock();
        if vfork_ctl.parent_tid.take().is_some() {
            drop(vfork_ctl);
            self.vfork_event.wake();
        }
    }
}

impl Drop for ProcessData {
    fn drop(&mut self) {
        executable::release(*self.executable.lock());
    }
}
