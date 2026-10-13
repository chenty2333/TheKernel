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

struct TraceSlotEntry {
    ptr: AtomicPtr<u8>,
    len: AtomicUsize,
    enter_ns: AtomicU64,
}

impl TraceSlotEntry {
    const fn new() -> Self {
        Self {
            ptr: AtomicPtr::new(core::ptr::null_mut()),
            len: AtomicUsize::new(0),
            enter_ns: AtomicU64::new(0),
        }
    }
}

struct WriteGuard<'a>(&'a AtomicBool);

impl<'a> Drop for WriteGuard<'a> {
    #[inline]
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub struct CpuTraceSlot {
    write_lock: AtomicBool,
    seq: AtomicU64,
    depth: AtomicUsize,
    stack: [TraceSlotEntry; 4],
}

impl CpuTraceSlot {
    pub const fn new() -> Self {
        Self {
            write_lock: AtomicBool::new(false),
            seq: AtomicU64::new(0),
            depth: AtomicUsize::new(0),
            stack: [const { TraceSlotEntry::new() }; 4],
        }
    }

    pub fn enter(&self, step: &'static str, now_ns: u64) {
        // Diagnostic code must NEVER block, spin, or deadlock.
        // If contended or interrupted/reentered on the same CPU, drop this trace event.
        if self
            .write_lock
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let _guard = WriteGuard(&self.write_lock);

        // Standard Seqlock writer protocol:
        // 1. Increment seq to odd to indicate write transaction in progress.
        self.seq.fetch_add(1, Ordering::Relaxed);
        // 2. Release fence: ensures subsequent relaxed writes are not reordered before seq is odd.
        core::sync::atomic::fence(Ordering::Release);

        let d = self.depth.load(Ordering::Relaxed);
        let target_idx = if d < 4 {
            self.depth.store(d + 1, Ordering::Relaxed);
            d
        } else {
            // Note: fixed stack capacity is 4. If nesting exceeds 4, slot 3 is overwritten with
            // the newest step. When that step exits, slot 3 is cleared and depth returns to 3.
            3
        };
        self.stack[target_idx]
            .ptr
            .store(step.as_ptr() as *mut u8, Ordering::Relaxed);
        self.stack[target_idx].len.store(step.len(), Ordering::Relaxed);
        self.stack[target_idx]
            .enter_ns
            .store(now_ns, Ordering::Relaxed);

        // 3. Release fence: ensures all data writes are committed before seq becomes even.
        core::sync::atomic::fence(Ordering::Release);
        // 4. Publish even seq with Release ordering.
        self.seq.fetch_add(1, Ordering::Release);
    }

    pub fn exit(&self, step: &'static str) {
        // Diagnostic code must NEVER block or deadlock.
        if self
            .write_lock
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let _guard = WriteGuard(&self.write_lock);

        let d = self.depth.load(Ordering::Relaxed);
        if d == 0 {
            return;
        }

        // Search stack for matching step.
        let mut found = None;
        for i in (0..d.min(4)).rev() {
            let ptr = self.stack[i].ptr.load(Ordering::Relaxed);
            let len = self.stack[i].len.load(Ordering::Relaxed);
            if !ptr.is_null() && len == step.len() {
                let slice = unsafe { core::slice::from_raw_parts(ptr, len) };
                if slice == step.as_bytes() {
                    found = Some(i);
                    break;
                }
            }
        }

        let Some(target_idx) = found else {
            // If the step is not found on this CPU (e.g. task migrated to another CPU
            // between enter and exit, or mismatched exit), do NOT pop unrelated steps!
            return;
        };

        // Standard Seqlock writer protocol:
        self.seq.fetch_add(1, Ordering::Relaxed);
        core::sync::atomic::fence(Ordering::Release);

        self.depth.store(target_idx, Ordering::Relaxed);
        for i in target_idx..d.min(4) {
            self.stack[i]
                .ptr
                .store(core::ptr::null_mut(), Ordering::Relaxed);
            self.stack[i].len.store(0, Ordering::Relaxed);
            self.stack[i].enter_ns.store(0, Ordering::Relaxed);
        }

        core::sync::atomic::fence(Ordering::Release);
        self.seq.fetch_add(1, Ordering::Release);
    }

    pub fn read(&self) -> Option<(&'static str, u64)> {
        // Standard Seqlock reader protocol:
        // 1. Acquire load of initial sequence number.
        let s1 = self.seq.load(Ordering::Acquire);
        if s1 & 1 != 0 {
            return None;
        }

        let d = self.depth.load(Ordering::Relaxed);
        if d == 0 {
            return None;
        }
        let target_idx = d.min(4) - 1;
        let slot = &self.stack[target_idx];
        let ptr = slot.ptr.load(Ordering::Relaxed);
        let len = slot.len.load(Ordering::Relaxed);
        let enter_ns = slot.enter_ns.load(Ordering::Relaxed);

        // 2. Acquire fence: prevents relaxed data loads above from being reordered after loading s2.
        core::sync::atomic::fence(Ordering::Acquire);
        let s2 = self.seq.load(Ordering::Acquire);

        // 3. Verify sequence number consistency and valid pointer/length.
        if s1 != s2 || ptr.is_null() || len == 0 {
            return None;
        }

        // SAFETY:
        // 1. `ptr` and `len` originated from a valid &'static str slice passed to `enter()`.
        // 2. Seqlock memory order guarantees:
        //    - The writer increments `seq` to odd, issues `fence(Release)`, updates data fields with `Relaxed`,
        //      issues `fence(Release)`, and increments `seq` to even with `Release`.
        //    - The reader loads `s1` with Acquire, returns None if odd (writer active), loads data
        //      fields with `Relaxed`, executes `fence(Acquire)`, and loads `s2` with Acquire.
        //    - If `s1 == s2`, no writer updated the slot during the read interval, ensuring `ptr`,
        //      `len`, and `enter_ns` belong to the exact same completed atomic write transaction without tearing.
        // 3. The memory range [ptr .. ptr + len] is guaranteed to be a valid static string in the kernel binary.
        // 4. Safe UTF-8 decoding via `core::str::from_utf8` ensures no UB even under corruption.
        let slice = unsafe { core::slice::from_raw_parts(ptr, len) };
        let step = core::str::from_utf8(slice).ok()?;
        Some((step, enter_ns))
    }
}

static CPU_TRACES: [CpuTraceSlot; axconfig::plat::MAX_CPU_NUM] =
    [const { CpuTraceSlot::new() }; axconfig::plat::MAX_CPU_NUM];

pub fn record_trace_enter(step: &'static str) {
    let cpu = current_cpu();
    if let Some(slot) = CPU_TRACES.get(cpu) {
        slot.enter(step, axhal::time::monotonic_time_nanos());
    }
}

pub fn record_trace_exit(step: &'static str) {
    let cpu = current_cpu();
    if let Some(slot) = CPU_TRACES.get(cpu) {
        slot.exit(step);
    }
}

struct StackBuf<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> StackBuf<N> {
    const fn new() -> Self {
        Self {
            buf: [0u8; N],
            len: 0,
        }
    }
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl<const N: usize> core::fmt::Write for StackBuf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let remaining = N.saturating_sub(self.len);
        let bytes = s.as_bytes();
        let to_copy = bytes.len().min(remaining);
        self.buf[self.len..self.len + to_copy].copy_from_slice(&bytes[..to_copy]);
        self.len += to_copy;
        Ok(())
    }
}

#[inline]
pub fn trace_enter(step: &'static str) {
    if boot_trace_enabled() {
        record_trace_enter(step);
        axlog::warn!("BT> {step}");
        let mut buf = StackBuf::<96>::new();
        use core::fmt::Write;
        let _ = write!(buf, "BT> {step}");
        crate::early_screen_milestone(buf.as_str());
    }
}

#[inline]
pub fn trace_exit(step: &'static str) {
    if boot_trace_enabled() {
        record_trace_exit(step);
        axlog::warn!("BT< {step}");
        let mut buf = StackBuf::<96>::new();
        use core::fmt::Write;
        let _ = write!(buf, "BT< {step}");
        crate::early_screen_milestone(buf.as_str());
    }
}

pub fn heartbeat(now_ns: u64) {
    let last_hb = LAST_HEARTBEAT_NS.load(Ordering::Acquire);
    if now_ns.saturating_sub(last_hb) < 5_000_000_000 {
        return;
    }
    let cpu_count = if cfg!(target_os = "none") {
        axhal::cpu_num().max(1).min(axconfig::plat::MAX_CPU_NUM)
    } else {
        axconfig::plat::MAX_CPU_NUM
    };
    let mut any_stuck = false;
    for cpu in 0..cpu_count {
        if CPU_TRACES[cpu].read().is_some() {
            any_stuck = true;
            break;
        }
    }
    if !any_stuck {
        return;
    }
    if LAST_HEARTBEAT_NS
        .compare_exchange(last_hb, now_ns, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    for cpu in 0..cpu_count {
        if let Some((step, enter_ns)) = CPU_TRACES[cpu].read() {
            let elapsed_s = now_ns.saturating_sub(enter_ns) / 1_000_000_000;
            axlog::warn!("BT~ heartbeat CPU {cpu}: stuck in {step} for {elapsed_s}s");
            let mut buf = StackBuf::<96>::new();
            use core::fmt::Write;
            let _ = write!(buf, "BT~ CPU {cpu}: {step} {elapsed_s}s");
            crate::early_screen_milestone(buf.as_str());
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

    #[test]
    fn trace_enter_exit_sequence_and_nesting() {
        let slot = CpuTraceSlot::new();
        assert_eq!(slot.read(), None);

        slot.enter("step1", 100);
        assert_eq!(slot.read(), Some(("step1", 100)));
        slot.exit("step1");
        assert_eq!(slot.read(), None);

        slot.enter("step2", 200);
        assert_eq!(slot.read(), Some(("step2", 200)));
        slot.exit("step2");
        assert_eq!(slot.read(), None);

        // Nested steps
        slot.enter("outer", 1000);
        assert_eq!(slot.read(), Some(("outer", 1000)));
        slot.enter("inner", 2000);
        assert_eq!(slot.read(), Some(("inner", 2000)));
        slot.exit("inner");
        assert_eq!(slot.read(), Some(("outer", 1000)));
        slot.exit("outer");
        assert_eq!(slot.read(), None);
    }

    #[test]
    fn heartbeat_idle_after_exit() {
        BOOT_TRACE.store(true, Ordering::SeqCst);
        LAST_HEARTBEAT_NS.store(0, Ordering::SeqCst);
        for s in CPU_TRACES.iter() {
            s.depth.store(0, Ordering::SeqCst);
        }
        heartbeat(10_000_000_000);
        // Idle CPUs do not trigger heartbeat timestamp updates
        assert_eq!(LAST_HEARTBEAT_NS.load(Ordering::SeqCst), 0);
        BOOT_TRACE.store(false, Ordering::SeqCst);
    }

    #[test]
    fn heartbeat_five_second_throttling() {
        BOOT_TRACE.store(true, Ordering::SeqCst);
        LAST_HEARTBEAT_NS.store(0, Ordering::SeqCst);
        CPU_TRACES[0].enter("stuck_step", 1_000);

        // Under 5s: throttled, no heartbeat
        heartbeat(4_999_999_999);
        assert_eq!(LAST_HEARTBEAT_NS.load(Ordering::SeqCst), 0);

        // At 5s: triggers and records timestamp
        heartbeat(5_000_000_000);
        assert_eq!(LAST_HEARTBEAT_NS.load(Ordering::SeqCst), 5_000_000_000);

        // Under next 5s interval: throttled
        heartbeat(9_999_999_999);
        assert_eq!(LAST_HEARTBEAT_NS.load(Ordering::SeqCst), 5_000_000_000);

        // At 10s: triggers again
        heartbeat(10_000_000_000);
        assert_eq!(LAST_HEARTBEAT_NS.load(Ordering::SeqCst), 10_000_000_000);

        CPU_TRACES[0].exit("stuck_step");
        BOOT_TRACE.store(false, Ordering::SeqCst);
    }

    #[test]
    fn concurrent_tearing_test() {
        use alloc::sync::Arc;
        use alloc::vec::Vec;

        let slot = Arc::new(CpuTraceSlot::new());
        let done = Arc::new(AtomicBool::new(false));
        let steps: [&'static str; 4] = [
            "x",
            "medium_step_42",
            "a",
            "a_very_long_step_name_to_stress_torn_read_detection_between_pointer_and_length",
        ];
        let mut handles = Vec::new();
        for i in 0..4 {
            let slot_clone = Arc::clone(&slot);
            let done_clone = Arc::clone(&done);
            let step = steps[i];
            handles.push(std::thread::spawn(move || {
                let mut iter = 0u64;
                while !done_clone.load(Ordering::Relaxed) {
                    iter += 1;
                    slot_clone.enter(step, iter);
                    for _ in 0..10 {
                        core::hint::spin_loop();
                    }
                    slot_clone.exit(step);
                }
            }));
        }

        let reader_slot = Arc::clone(&slot);
        let reader_done = Arc::clone(&done);
        let reader_handle = std::thread::spawn(move || {
            let mut valid_reads = 0;
            for _ in 0..100_000 {
                if let Some((read_step, _ts)) = reader_slot.read() {
                    assert!(
                        steps.contains(&read_step),
                        "Torn read detected: got invalid step {:?}",
                        read_step
                    );
                    valid_reads += 1;
                }
            }
            reader_done.store(true, Ordering::Relaxed);
            valid_reads
        });

        let valid_reads = reader_handle.join().unwrap();
        for h in handles {
            h.join().unwrap();
        }
        assert!(valid_reads > 0, "Observed valid reads without tearing");
    }

    #[test]
    fn trace_exit_mismatch_does_not_pop() {
        let slot = CpuTraceSlot::new();
        slot.enter("step1", 100);
        assert_eq!(slot.read(), Some(("step1", 100)));

        // Exit with non-existent step: must NOT pop step1
        slot.exit("nonexistent_step");
        assert_eq!(slot.read(), Some(("step1", 100)));

        slot.exit("step1");
        assert_eq!(slot.read(), None);
    }

    #[test]
    fn trace_try_lock_nonblocking_on_contention() {
        let slot = CpuTraceSlot::new();
        // Simulate lock held by interrupted non-IRQ code
        slot.write_lock.store(true, Ordering::Release);

        // Attempting to enter or exit must not spin/block and safely drops the record
        slot.enter("step1", 100);
        assert_eq!(slot.read(), None);

        slot.exit("step1");
        assert_eq!(slot.read(), None);

        // Once lock is released, tracing resumes normally
        slot.write_lock.store(false, Ordering::Release);
        slot.enter("step2", 200);
        assert_eq!(slot.read(), Some(("step2", 200)));
        slot.exit("step2");
        assert_eq!(slot.read(), None);
    }

    #[test]
    fn trace_stack_capacity_and_overflow_behavior() {
        let slot = CpuTraceSlot::new();
        slot.enter("s0", 10);
        slot.enter("s1", 20);
        slot.enter("s2", 30);
        slot.enter("s3", 40);
        assert_eq!(slot.read(), Some(("s3", 40)));

        // 5th level overflows capacity 4: overwrites slot 3
        slot.enter("s4", 50);
        assert_eq!(slot.read(), Some(("s4", 50)));

        // Exiting s4 clears slot 3, returning depth to 3 (which points to s2)
        slot.exit("s4");
        assert_eq!(slot.read(), Some(("s2", 30)));

        slot.exit("s2");
        assert_eq!(slot.read(), Some(("s1", 20)));
        slot.exit("s1");
        assert_eq!(slot.read(), Some(("s0", 10)));
        slot.exit("s0");
        assert_eq!(slot.read(), None);
    }
}
