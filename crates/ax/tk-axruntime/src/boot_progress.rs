//! Opt-in atomic boot/console progress, never a scheduling or locking fix.
//! IRQ paths only update counters; snapshots never take console/scheduler locks.
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering};

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum Stage {
    Unknown       = 0,
    Runtime       = 1,
    Heap          = 2,
    Paging        = 3,
    Platform      = 4,
    Scheduler     = 5,
    Drivers       = 6,
    Filesystems   = 7,
    SecondaryCpus = 8,
    Interrupts    = 9,
    KernelMain    = 10,
    KernelInit    = 11,
    Pid1Published = 12,
    AlarmStart    = 13,
    AlarmDone     = 14,
    InitJoin      = 15,
}
impl Stage {
    pub fn name(value: u8) -> &'static str {
        match value {
            1 => "runtime",
            2 => "heap",
            3 => "paging",
            4 => "platform",
            5 => "scheduler",
            6 => "drivers",
            7 => "filesystems",
            8 => "secondary-cpus",
            9 => "interrupts",
            10 => "kernel-main",
            11 => "kernel-init",
            12 => "pid1-published",
            13 => "alarm-start",
            14 => "alarm-done",
            15 => "init-join",
            _ => "unknown",
        }
    }
}
struct Cpu {
    phase: AtomicU8,
    timers: AtomicU64,
    last_ns: AtomicU64,
    write_start: AtomicU64,
    write_done: AtomicU64,
    bytes: AtomicU64,
    present_start: AtomicU64,
    present_done: AtomicU64,
    log_records: AtomicU64,
    last_point: AtomicU8,
}
impl Cpu {
    const fn new() -> Self {
        Self {
            phase: AtomicU8::new(0),
            timers: AtomicU64::new(0),
            last_ns: AtomicU64::new(0),
            write_start: AtomicU64::new(0),
            write_done: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            present_start: AtomicU64::new(0),
            present_done: AtomicU64::new(0),
            log_records: AtomicU64::new(0),
            last_point: AtomicU8::new(0),
        }
    }
    fn snapshot(&self) -> CpuSnapshot {
        CpuSnapshot {
            phase: self.phase.load(Ordering::Relaxed),
            timers: self.timers.load(Ordering::Relaxed),
            last_ns: self.last_ns.load(Ordering::Relaxed),
            write_start: self.write_start.load(Ordering::Relaxed),
            write_done: self.write_done.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            present_start: self.present_start.load(Ordering::Relaxed),
            present_done: self.present_done.load(Ordering::Relaxed),
            log_records: self.log_records.load(Ordering::Relaxed),
            last_point: self.last_point.load(Ordering::Relaxed),
        }
    }
}
static ENABLED: AtomicBool = AtomicBool::new(false);
static BOOT_TRACE: AtomicBool = AtomicBool::new(false);
static LAST_ENTER_STEP: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
static LAST_ENTER_LEN: AtomicUsize = AtomicUsize::new(0);
static LAST_ENTER_TIME_NS: AtomicU64 = AtomicU64::new(0);
static LAST_HEARTBEAT_NS: AtomicU64 = AtomicU64::new(0);

static STAGE: AtomicU8 = AtomicU8::new(0);
static CPUS: [Cpu; axconfig::plat::MAX_CPU_NUM] =
    [const { Cpu::new() }; axconfig::plat::MAX_CPU_NUM];
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuSnapshot {
    pub phase: u8,
    pub timers: u64,
    pub last_ns: u64,
    pub write_start: u64,
    pub write_done: u64,
    pub bytes: u64,
    pub present_start: u64,
    pub present_done: u64,
    pub log_records: u64,
    pub last_point: u8,
}
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}
pub fn boot_trace_enabled() -> bool {
    BOOT_TRACE.load(Ordering::Relaxed)
}
pub fn init() {
    ENABLED.store(
        axhal::boot::command_line_value("boot.progress") == Some("1"),
        Ordering::Release,
    );
    BOOT_TRACE.store(
        axhal::boot::command_line_value("n305.boot_trace") == Some("1"),
        Ordering::Release,
    );
}

pub fn record_trace_enter(step: &'static str) {
    LAST_ENTER_STEP.store(step.as_ptr() as *mut u8, Ordering::Release);
    LAST_ENTER_LEN.store(step.len(), Ordering::Release);
    LAST_ENTER_TIME_NS.store(axhal::time::monotonic_time_nanos(), Ordering::Release);
}

pub fn record_trace_exit(_step: &'static str) {
    LAST_ENTER_STEP.store(core::ptr::null_mut(), Ordering::Release);
}

#[inline]
pub fn trace_enter(step: &'static str) {
    if boot_trace_enabled() {
        record_trace_enter(step);
        axlog::warn!("BT> {step}");
    }
}

#[inline]
pub fn trace_exit(step: &'static str) {
    if boot_trace_enabled() {
        record_trace_exit(step);
        axlog::warn!("BT< {step}");
    }
}

pub fn heartbeat(now_ns: u64) {
    let ptr = LAST_ENTER_STEP.load(Ordering::Acquire);
    if ptr.is_null() {
        return;
    }
    let last_hb = LAST_HEARTBEAT_NS.load(Ordering::Acquire);
    if now_ns.saturating_sub(last_hb) >= 5_000_000_000 {
        if LAST_HEARTBEAT_NS
            .compare_exchange(last_hb, now_ns, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let len = LAST_ENTER_LEN.load(Ordering::Acquire);
            let enter_ns = LAST_ENTER_TIME_NS.load(Ordering::Acquire);
            let elapsed_s = now_ns.saturating_sub(enter_ns) / 1_000_000_000;
            // SAFETY: step pointer came from &'static str
            let step = unsafe {
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr, len))
            };
            let cpu = axhal::percpu::this_cpu_id();
            axlog::warn!("BT~ heartbeat CPU {cpu}: stuck in {step} for {elapsed_s}s");
        }
    }
}
pub fn mark(stage: Stage) {
    if enabled() {
        STAGE.store(stage as u8, Ordering::Relaxed);
    }
}
pub fn stage() -> u8 {
    STAGE.load(Ordering::Relaxed)
}
pub fn cpu(index: usize) -> Option<CpuSnapshot> {
    CPUS.get(index).map(Cpu::snapshot)
}
pub fn cpu_phase(index: usize, phase: u8) {
    if enabled()
        && let Some(cpu) = CPUS.get(index)
    {
        cpu.phase.store(phase, Ordering::Relaxed);
    }
}
pub fn timer(index: usize, now: u64) {
    if enabled()
        && let Some(cpu) = CPUS.get(index)
    {
        cpu.timers.fetch_add(1, Ordering::Relaxed);
        cpu.last_ns.store(now, Ordering::Relaxed);
    }
}
fn current_cpu() -> usize {
    #[cfg(target_os = "none")]
    {
        axhal::percpu::this_cpu_id()
    }
    #[cfg(not(target_os = "none"))]
    {
        0
    }
}
/// Scope counters use the initiating CPU; the last point is an approximate
/// observation, not a lock-owner proof (tasks may migrate or interleave).
pub struct Scope {
    cpu: Option<usize>,
    present: bool,
}
impl Scope {
    fn begin(present: bool, bytes: usize) -> Self {
        let cpu = enabled().then(current_cpu).filter(|id| *id < CPUS.len());
        if let Some(id) = cpu {
            let state = &CPUS[id];
            if present {
                state.present_start.fetch_add(1, Ordering::Relaxed);
            } else {
                state.write_start.fetch_add(1, Ordering::Relaxed);
                state.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
            }
            state
                .last_point
                .store(if present { 5 } else { 1 }, Ordering::Relaxed);
        }
        Self { cpu, present }
    }
    pub fn write(bytes: usize) -> Self {
        Self::begin(false, bytes)
    }
    pub fn present() -> Self {
        Self::begin(true, 0)
    }
    pub fn point(&self, value: u8) {
        if let Some(id) = self.cpu {
            CPUS[id].last_point.store(value, Ordering::Relaxed);
        }
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        if let Some(id) = self.cpu {
            let state = &CPUS[id];
            if self.present {
                state.present_done.fetch_add(1, Ordering::Relaxed);
            } else {
                state.write_done.fetch_add(1, Ordering::Relaxed);
            }
            state.last_point.store(0, Ordering::Relaxed);
        }
    }
}
pub fn log_record() {
    if enabled()
        && let Some(cpu) = CPUS.get(current_cpu())
    {
        cpu.log_records.fetch_add(1, Ordering::Relaxed);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opt_in_scope_completions_cover_early_return_without_console_locks() {
        let old = ENABLED.swap(true, Ordering::Relaxed);
        let old_stage = stage();
        let before = cpu(0).unwrap();
        {
            let guard = Scope::write(4);
            guard.point(2);
            log_record();
            let pending = cpu(0).unwrap();
            assert_eq!(pending.write_start, before.write_start + 1);
            assert_eq!(pending.write_done, before.write_done);
            assert_eq!(pending.last_point, 2);
            let _draw = Scope::present();
            mark(Stage::AlarmStart);
        }
        let done = cpu(0).unwrap();
        assert_eq!(done.write_done, before.write_done + 1);
        assert_eq!(done.present_start, before.present_start + 1);
        assert_eq!(done.present_done, before.present_done + 1);
        assert_eq!(done.bytes, before.bytes + 4);
        assert_eq!(done.log_records, before.log_records + 1);
        STAGE.store(old_stage, Ordering::Relaxed);
        ENABLED.store(false, Ordering::Relaxed);
        let before = cpu(0).unwrap();
        drop(Scope::write(99));
        assert_eq!(cpu(0).unwrap(), before);
        ENABLED.store(old, Ordering::Relaxed);
    }
    #[test]
    fn atomic_snapshot_distinguishes_pending_write_render_and_timer_progress() {
        let cpu = Cpu::new();
        cpu.phase.store(4, Ordering::Relaxed);
        cpu.timers.fetch_add(1, Ordering::Relaxed);
        cpu.last_ns.store(123, Ordering::Relaxed);
        cpu.write_start.fetch_add(1, Ordering::Relaxed);
        cpu.bytes.store(64, Ordering::Relaxed);
        cpu.last_point.store(2, Ordering::Relaxed);
        let pending = cpu.snapshot();
        assert_eq!(pending.write_start, 1);
        assert_eq!(pending.write_done, 0);
        assert_eq!(pending.last_point, 2);
        cpu.write_done.fetch_add(1, Ordering::Relaxed);
        cpu.present_start.fetch_add(1, Ordering::Relaxed);
        assert_eq!(cpu.snapshot().present_start, 1);
        assert_eq!(cpu.snapshot().present_done, 0);
        cpu.present_done.fetch_add(1, Ordering::Relaxed);
        let done = cpu.snapshot();
        assert_eq!(done.timers, 1);
        assert_eq!(done.last_ns, 123);
        assert_eq!(done.bytes, 64);
        assert_eq!(Stage::name(Stage::AlarmStart as u8), "alarm-start");
        assert_eq!(Stage::name(255), "unknown");
        assert_eq!(super::cpu(usize::MAX), None);
    }
}
