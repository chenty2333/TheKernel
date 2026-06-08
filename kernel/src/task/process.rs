use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use axerrno::{AxError, AxResult};
use axnet::NetStack;
use axpoll::PollSet;
use axsync::{Mutex, spin::SpinNoIrq};
use linux_raw_sys::general::{CAP_SETGID, CAP_SETUID};
use scope_local::Scope;
use spin::RwLock;
use starry_process::{Pid, Process};
use starry_signal::{
    Signo,
    api::{ProcessSignalManager, SignalActions},
};

use crate::file::executable::{self, ExecutableKey};
use super::{
    accounting::{AtomicTaskUsage, live_process_usage},
    creds::{CAPABILITY_WORDS, CapabilityState, Credentials},
    futex::FutexTable,
    jobctl::{ContinueResult, ExecControlState, JobControlState, StopState, VforkControlState},
    resources::Rlimits,
    timer::PosixTimer,
};
use crate::mm::AddrSpace;

pub(crate) const UTS_FIELD_LEN: usize = 64;

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
    /// Parent-death signal configured through prctl(PR_SET_PDEATHSIG).
    pdeath_signal: AtomicU32,
    /// Current timer slack in nanoseconds.
    timerslack_current_ns: AtomicUsize,
    /// Default timer slack in nanoseconds, used when PR_SET_TIMERSLACK is 0.
    timerslack_default_ns: AtomicUsize,
    /// no_new_privs state configured through prctl(PR_SET_NO_NEW_PRIVS).
    no_new_privs: AtomicU32,
    /// Process-scoped membarrier registration state.
    membarrier_state: AtomicU32,
    /// POSIX interval timers created by this process.
    pub(crate) posix_timers: SpinNoIrq<Vec<Option<PosixTimer>>>,

    /// CPU time accumulated from sibling threads that have already exited.
    pub(in crate::task) exited_threads_usage: AtomicTaskUsage,
    /// CPU time accumulated from waited-for child subtrees.
    waited_children_usage: AtomicTaskUsage,

    /// Serializes wait* selection and consumption for this process.
    pub wait_lock: Mutex<()>,

    /// Job-control stop state shared by all threads in the process.
    job_ctl: SpinNoIrq<JobControlState>,
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
    /// The UTS namespace for this process.
    pub(crate) uts_ns: Arc<UtsNamespace>,
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
        uts_ns: Arc<UtsNamespace>,
    ) -> Arc<Self> {
        Arc::new(Self {
            proc,
            exe_path: RwLock::new(exe_path),
            executable: SpinNoIrq::new(executable),
            cmdline: RwLock::new(cmdline),
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
            supplementary_groups: SpinNoIrq::new(Vec::new()),
            personality: AtomicU32::new(0),
            pdeath_signal: AtomicU32::new(0),
            timerslack_current_ns: AtomicUsize::new(50_000),
            timerslack_default_ns: AtomicUsize::new(50_000),
            no_new_privs: AtomicU32::new(0),
            membarrier_state: AtomicU32::new(0),
            posix_timers: SpinNoIrq::new(Vec::new()),
            exited_threads_usage: AtomicTaskUsage::new(),
            waited_children_usage: AtomicTaskUsage::new(),
            wait_lock: Mutex::new(()),

            job_ctl: SpinNoIrq::new(JobControlState::default()),
            exec_ctl: SpinNoIrq::new(ExecControlState::default()),
            vfork_ctl: SpinNoIrq::new(VforkControlState::default()),
            stop_event: Arc::default(),
            vfork_event: Arc::default(),

            net_ns,
            uts_ns,
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

    /// Linux manual: A "clone" child is one which delivers no signal, or a
    /// signal other than SIGCHLD to its parent upon termination.
    pub fn is_clone_child(&self) -> bool {
        self.exit_signal != Some(Signo::SIGCHLD)
    }

    /// Returns process CPU usage, including live threads and exited siblings.
    pub fn self_usage(&self) -> super::accounting::TaskUsage {
        live_process_usage(self)
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

    pub fn membarrier_registered(&self, flags: u32) -> bool {
        self.membarrier_state.load(Ordering::Relaxed) & flags == flags
    }

    pub fn bounding_capability_enabled(&self, cap: u32) -> bool {
        self.capability_state().bounding_contains(cap)
    }

    pub fn drop_bounding_capability(&self, cap: u32) -> AxResult<()> {
        self.caps.lock().drop_bounding(cap)
    }

    pub fn securebits(&self) -> u32 {
        self.caps.lock().securebits
    }

    pub fn set_securebits(&self, securebits: u32) {
        self.caps.lock().securebits = securebits;
    }

    fn fixup_capabilities_for_euid_change(&self, old_euid: u32, new_euid: u32) {
        if old_euid == new_euid {
            return;
        }

        let mut caps = self.caps.lock();
        if old_euid == 0 && new_euid != 0 {
            caps.effective = [0; CAPABILITY_WORDS];
        } else if old_euid != 0 && new_euid == 0 {
            caps.effective = caps.permitted;
        }
    }

    pub fn setuid(&self, uid: u32) -> AxResult<()> {
        let can_setuid = self.has_effective_capability(CAP_SETUID);
        let mut creds = self.creds.lock();
        let old_euid = creds.euid;
        if can_setuid {
            creds.ruid = uid;
            creds.euid = uid;
            creds.suid = uid;
            creds.fsuid = uid;
            let new_euid = creds.euid;
            drop(creds);
            self.fixup_capabilities_for_euid_change(old_euid, new_euid);
            return Ok(());
        }
        if uid == creds.ruid || uid == creds.suid {
            creds.euid = uid;
            creds.fsuid = uid;
            let new_euid = creds.euid;
            drop(creds);
            self.fixup_capabilities_for_euid_change(old_euid, new_euid);
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
            for id in [ruid, euid].into_iter().flatten() {
                if id != old.ruid && id != old.euid && id != old.suid {
                    return Err(AxError::OperationNotPermitted);
                }
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
        drop(creds);
        self.fixup_capabilities_for_euid_change(old.euid, new_euid);
        Ok(())
    }

    pub fn setregid(&self, rgid: Option<u32>, egid: Option<u32>) -> AxResult<()> {
        let can_setgid = self.has_effective_capability(CAP_SETGID);
        let mut creds = self.creds.lock();
        let old = *creds;
        if !can_setgid {
            for id in [rgid, egid].into_iter().flatten() {
                if id != old.rgid && id != old.egid && id != old.sgid {
                    return Err(AxError::OperationNotPermitted);
                }
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
        let new_euid = creds.euid;
        drop(creds);
        self.fixup_capabilities_for_euid_change(old.euid, new_euid);
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
                    job_ctl.continued = true;
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
    pub fn take_stop_status(&self) -> Option<u8> {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state == StopState::Stopped && !job_ctl.stop_reported {
            job_ctl.stop_reported = true;
            Some(job_ctl.stop_signal)
        } else {
            None
        }
    }

    /// Peeks at the stopped status without consuming it (for WNOWAIT).
    pub fn peek_stop_status(&self) -> Option<u8> {
        let job_ctl = self.job_ctl.lock();
        if job_ctl.state == StopState::Stopped && !job_ctl.stop_reported {
            Some(job_ctl.stop_signal)
        } else {
            None
        }
    }

    /// Claims the pending stop report so a waiter can complete userspace copies first.
    pub fn claim_stop_status(&self) -> Option<u8> {
        self.take_stop_status()
    }

    /// Restores a previously claimed stop report after a failed userspace copy.
    pub fn restore_stop_status(&self, stop_signal: u8) {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state == StopState::Stopped && job_ctl.stop_signal == stop_signal {
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
