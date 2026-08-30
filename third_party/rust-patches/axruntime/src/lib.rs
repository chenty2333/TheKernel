// Copyright 2025 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Runtime library of [ArceOS](https://github.com/arceos-org/arceos).
//!
//! Any application uses ArceOS should link this library. It does some
//! initialization work before entering the application's `main` function.
//!
//! # Cargo Features
//!
//! - `alloc`: Enable global memory allocator.
//! - `paging`: Enable page table manipulation support.
//! - `irq`: Enable interrupt handling support.
//! - `multitask`: Enable multi-threading support.
//! - `smp`: Enable SMP (symmetric multiprocessing) support.
//! - `fs`: Enable filesystem support.
//! - `net`: Enable networking support.
//! - `display`: Enable graphics support.
//!
//! All the features are optional and disabled by default.

#![cfg_attr(not(test), no_std)]
#![allow(missing_abi)]

#[macro_use]
extern crate axlog;

/// The kernel log buffer backing Linux's legacy `syslog(2)` interface.
///
/// It deliberately sits directly on the console-write boundary so all normal
/// kernel log output is retained independently from console filtering.
pub mod klog {
    use kspin::SpinNoIrq;

    /// Keep this bounded and allocation-free: logging is callable in early
    /// boot, IRQ, and allocation-failure paths.
    pub const CAPACITY: usize = 64 * 1024;

    struct Ring {
        bytes: [u8; CAPACITY],
        oldest: u64,
        end: u64,
        read: u64,
        clear: u64,
        console_enabled: bool,
        console_level: u8,
    }

    impl Ring {
        const fn new() -> Self {
            Self {
                bytes: [0; CAPACITY],
                oldest: 0, end: 0, read: 0, clear: 0,
                console_enabled: true,
                console_level: 7,
            }
        }

        fn push(&mut self, byte: u8) {
            self.bytes[(self.end as usize) % CAPACITY] = byte;
            self.end += 1;
            self.oldest = self.end.saturating_sub(CAPACITY as u64);
            self.read = self.read.max(self.oldest);
            self.clear = self.clear.max(self.oldest);
        }

        fn copy_into(&self, dst: &mut [u8]) -> usize {
            let copied = dst.len().min((self.end - self.clear) as usize);
            for (index, byte) in dst.iter_mut().take(copied).enumerate() {
                *byte = self.bytes[((self.clear as usize) + index) % CAPACITY];
            }
            copied
        }
    }

    static RING: SpinNoIrq<Ring> = SpinNoIrq::new(Ring::new());

    /// Records bytes before they are emitted to the physical console.
    pub fn record(bytes: &[u8]) {
        let mut ring = RING.lock();
        for &byte in bytes {
            ring.push(byte);
        }
    }

    /// Copies the oldest unread bytes; optionally consumes precisely those
    /// bytes, matching `SYSLOG_ACTION_READ` rather than clearing new records
    /// produced concurrently after the copy began.
    pub fn snapshot_into(dst: &mut [u8], destructive: bool) -> (usize, u64) {
        let ring = RING.lock();
        let mut start = if destructive { ring.read } else { ring.clear };
        let available = (ring.end - start) as usize;
        let copied = dst.len().min(available);
        // Linux READ_ALL/READ_CLEAR returns the newest records that fit.
        if !destructive { start = ring.end - copied as u64; }
        for (index, byte) in dst.iter_mut().take(copied).enumerate() {
            *byte = ring.bytes[((start as usize) + index) % CAPACITY];
        }
        (copied, start + copied as u64)
    }

    /// Advances only a successful destructive read's independent cursor.
    pub fn commit_read(end: u64) {
        let mut ring = RING.lock();
        ring.read = ring.read.max(end);
    }

    /// Clears only records included by an already copied snapshot.
    pub fn commit_clear(end: u64) {
        let mut ring = RING.lock();
        ring.clear = ring.clear.max(end.min(ring.end));
        ring.read = ring.read.max(ring.clear);
    }

    /// Returns unread bytes in the ring.
    pub fn unread_len() -> usize {
        let ring = RING.lock(); (ring.end - ring.read) as usize
    }

    /// Discards all unread bytes.
    pub fn clear() {
        let mut ring = RING.lock();
        ring.clear = ring.end;
        ring.read = ring.end;
    }

    /// Enables or disables physical console output. Logging still reaches the
    /// ring while disabled.
    pub fn set_console_enabled(enabled: bool) {
        RING.lock().console_enabled = enabled;
    }

    /// Returns whether physical console output is enabled.
    pub fn console_enabled() -> bool {
        RING.lock().console_enabled
    }

    /// Stores the Linux console threshold (1 through 8). The current axlog
    /// boundary does not preserve record priority, so it cannot filter output
    /// by severity yet; retaining the value prevents silently accepting an
    /// invalid control action and makes the setting observable to a priority-
    /// aware logger backend.
    pub fn set_console_level(level: u8) {
        RING.lock().console_level = level;
    }

    /// Returns the configured Linux console threshold.
    pub fn console_level() -> u8 {
        RING.lock().console_level
    }

    /// Determines the Linux syslog priority from axlog's ANSI level color.
    /// The ring gets the unfiltered stream; this only gates console output.
    pub fn should_print_to_console(bytes: &[u8]) -> bool {
        let ring = RING.lock();
        if !ring.console_enabled {
            return false;
        }
        let priority = if bytes.windows(4).any(|part| part == b"[31m") {
            3 // error
        } else if bytes.windows(4).any(|part| part == b"[33m") {
            4 // warn
        } else if bytes.windows(4).any(|part| part == b"[32m") {
            6 // info
        } else {
            7 // debug, trace, and unclassified print output
        };
        priority < ring.console_level
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ring_wraps_and_consumes_in_fifo_order() {
            let mut ring = Ring::new();
            for byte in 0..(CAPACITY + 3) {
                ring.push(byte as u8);
            }
            let mut out = [0; 4];
            assert_eq!(ring.copy_into(&mut out), 4);
            assert_eq!(out, [3, 4, 5, 6]);
            ring.read = 7;
            assert_eq!(ring.copy_into(&mut out), 4);
            assert_eq!(out, [3, 4, 5, 6]);
        }

        #[test]
        fn console_filter_uses_linux_priority_order() {
            set_console_enabled(true);
            set_console_level(5);
            assert!(should_print_to_console(b"\x1b[31merror"));
            assert!(should_print_to_console(b"\x1b[33mwarn"));
            assert!(!should_print_to_console(b"\x1b[32minfo"));
            set_console_level(8);
        }
    }
}

#[cfg(all(target_os = "none", not(test)))]
mod lang_items;

#[cfg(feature = "smp")]
mod mp;

#[cfg(feature = "paging")]
mod klib;

#[cfg(feature = "smp")]
pub use self::mp::rust_main_secondary;

const LOGO: &str = r#"
       d8888                            .d88888b.   .d8888b.
      d88888                           d88P" "Y88b d88P  Y88b
     d88P888                           888     888 Y88b.
    d88P 888 888d888  .d8888b  .d88b.  888     888  "Y888b.
   d88P  888 888P"   d88P"    d8P  Y8b 888     888     "Y88b.
  d88P   888 888     888      88888888 888     888       "888
 d8888888888 888     Y88b.    Y8b.     Y88b. .d88P Y88b  d88P
d88P     888 888      "Y8888P  "Y8888   "Y88888P"   "Y8888P"
"#;

unsafe extern "C" {
    /// Application's entry point.
    fn main();
}

struct LogIfImpl;

#[cfg(feature = "irq-exit")]
struct IrqExitIfImpl;

#[cfg(feature = "irq-exit")]
#[crate_interface::impl_interface]
impl axtask::IrqExitIf for IrqExitIfImpl {
    fn register_irq_exit_hook(hook: fn()) -> bool {
        axhal::irq::register_irq_exit_hook(hook)
    }

    fn in_irq_context() -> bool {
        axhal::irq::in_irq_context()
    }
}

#[crate_interface::impl_interface]
impl axlog::LogIf for LogIfImpl {
    fn console_write_str(s: &str) {
        klog::record(s.as_bytes());
        if klog::should_print_to_console(s.as_bytes()) {
            axhal::console::write_bytes(s.as_bytes());
        }
    }

    fn current_time() -> core::time::Duration {
        axhal::time::monotonic_time()
    }

    fn current_cpu_id() -> Option<usize> {
        #[cfg(feature = "smp")]
        if is_init_ok() {
            Some(axhal::percpu::this_cpu_id())
        } else {
            None
        }
        #[cfg(not(feature = "smp"))]
        Some(0)
    }

    fn current_task_id() -> Option<u64> {
        if is_init_ok() {
            #[cfg(feature = "multitask")]
            {
                axtask::current_may_uninit().map(|curr| curr.id().as_u64())
            }
            #[cfg(not(feature = "multitask"))]
            None
        } else {
            None
        }
    }
}

use core::sync::atomic::{AtomicUsize, Ordering};

/// Number of CPUs that have completed initialization.
static INITED_CPUS: AtomicUsize = AtomicUsize::new(0);

#[cfg(all(feature = "irq", feature = "multitask", feature = "ipi"))]
const CALL_FUNCTION_TIMER_EVENT_RETRIGGER: usize = 1 << 0;

#[cfg(all(feature = "irq", feature = "multitask", feature = "ipi"))]
static CALL_FUNCTION_WORK: [AtomicUsize; axconfig::plat::MAX_CPU_NUM] =
    [const { AtomicUsize::new(0) }; axconfig::plat::MAX_CPU_NUM];

fn is_init_ok() -> bool {
    INITED_CPUS.load(Ordering::Acquire) == axhal::cpu_num()
}

fn build_flag_enabled(value: Option<&'static str>) -> bool {
    matches!(
        value,
        Some("1" | "y" | "Y" | "yes" | "YES" | "true" | "TRUE")
    )
}

#[cfg(feature = "irq")]
const PERIODIC_INTERVAL_NANOS: u64 = axhal::time::NANOS_PER_SEC / axconfig::TICKS_PER_SEC as u64;

#[cfg(feature = "irq")]
#[percpu::def_percpu]
static NEXT_PERIODIC_DEADLINE: u64 = 0;

#[cfg(feature = "irq")]
#[percpu::def_percpu]
static EARLY_DEADLINE: u64 = u64::MAX;

#[cfg(feature = "irq")]
fn periodic_deadline(now_ns: u64) -> u64 {
    // Safety: callers either run in the timer IRQ path or hold the IRQ-save guard.
    let deadline = unsafe { NEXT_PERIODIC_DEADLINE.read_current_raw() };
    if deadline == 0 || deadline <= now_ns {
        let next = now_ns.saturating_add(PERIODIC_INTERVAL_NANOS);
        // Task-context rearming can observe a stale periodic deadline after a delayed
        // IRQ delivery. Never reprogram the hardware with an already-expired deadline.
        unsafe { NEXT_PERIODIC_DEADLINE.write_current_raw(next) };
        next
    } else {
        deadline
    }
}

#[cfg(feature = "irq")]
fn advance_periodic_deadline(now_ns: u64) -> bool {
    // Safety: callers either run in the timer IRQ path or hold the IRQ-save guard.
    let deadline = unsafe { NEXT_PERIODIC_DEADLINE.read_current_raw() };
    if deadline == 0 {
        unsafe {
            NEXT_PERIODIC_DEADLINE.write_current_raw(now_ns.saturating_add(PERIODIC_INTERVAL_NANOS))
        };
        return true;
    }
    if now_ns < deadline {
        return false;
    }

    let mut next = deadline.saturating_add(PERIODIC_INTERVAL_NANOS);
    if now_ns >= next {
        next = now_ns + PERIODIC_INTERVAL_NANOS;
    }
    // Safety: callers either run in the timer IRQ path or hold the IRQ-save guard.
    unsafe { NEXT_PERIODIC_DEADLINE.write_current_raw(next) };
    true
}

#[cfg(feature = "irq")]
fn consume_early_deadline(now_ns: u64) -> bool {
    // Safety: callers either run in the timer IRQ path or hold the IRQ-save guard.
    let deadline = unsafe { EARLY_DEADLINE.read_current_raw() };
    if now_ns < deadline {
        return false;
    }
    unsafe { EARLY_DEADLINE.write_current_raw(u64::MAX) };
    true
}

#[cfg(feature = "irq")]
fn rearm_timer(now_ns: u64) {
    let periodic = periodic_deadline(now_ns);
    // Safety: callers either run in the timer IRQ path or hold the IRQ-save guard.
    let early = unsafe { EARLY_DEADLINE.read_current_raw() };
    axhal::time::set_oneshot_timer(core::cmp::min(periodic, early));
}

#[cfg(all(feature = "irq", feature = "multitask"))]
pub fn set_early_timer_deadline(deadline: Option<axhal::time::TimeValue>) {
    let _guard = kernel_guard::IrqSave::new();
    let deadline_ns = deadline.map_or(u64::MAX, |deadline| {
        deadline.as_nanos().min(u128::from(u64::MAX)) as u64
    });
    // Safety: `IrqSave` pins us to the current CPU while updating its local timer state.
    unsafe { EARLY_DEADLINE.write_current_raw(deadline_ns) };
    rearm_timer(axhal::time::monotonic_time_nanos());
}

#[cfg(all(feature = "irq", feature = "multitask"))]
fn retrigger_local_timer_events() {
    axtask::on_timer_event();
    rearm_timer(axhal::time::monotonic_time_nanos());
}

#[cfg(all(feature = "irq", feature = "multitask", feature = "ipi"))]
fn call_function_ipi_handler() {
    let cpu = axhal::percpu::this_cpu_id();
    let pending = CALL_FUNCTION_WORK[cpu].swap(0, Ordering::AcqRel);
    if pending & CALL_FUNCTION_TIMER_EVENT_RETRIGGER != 0 {
        retrigger_local_timer_events();
    }
    let unknown = pending & !CALL_FUNCTION_TIMER_EVENT_RETRIGGER;
    assert_eq!(unknown, 0, "unknown bounded call-function work: {unknown:#x}");
}

/// Retriggers the ordinary timer-event callback chain on every online CPU.
///
/// Remote requests use one coalescible, allocation-free work bit. Each target
/// CPU re-evaluates its own timer callbacks and reprograms its local hardware
/// deadline; no caller writes another CPU's per-CPU timer state directly.
#[cfg(all(feature = "irq", feature = "multitask"))]
pub fn retrigger_timer_events_all() {
    let _guard = kernel_guard::NoPreemptIrqSave::new();

    #[cfg(feature = "ipi")]
    let (current_cpu, cpu_num) = {
        let current_cpu = axhal::percpu::this_cpu_id();
        let cpu_num = axhal::cpu_num();
        for cpu in 0..cpu_num {
            if cpu != current_cpu {
                CALL_FUNCTION_WORK[cpu]
                    .fetch_or(CALL_FUNCTION_TIMER_EVENT_RETRIGGER, Ordering::Release);
            }
        }
        (current_cpu, cpu_num)
    };

    retrigger_local_timer_events();

    #[cfg(feature = "ipi")]
    if cpu_num > 1 {
        axhal::irq::send_ipi_reason(
            axhal::irq::IpiReason::CallFunction,
            axhal::irq::IpiTarget::AllExceptCurrent {
                cpu_id: current_cpu,
                cpu_num,
            },
        )
        .unwrap_or_else(|error| panic!("failed to retrigger remote timer events: {error:?}"));
    }
}

/// The main entry point of the ArceOS runtime.
///
/// It is called from the bootstrapping code in the specific platform crate (see
/// [`axplat::main`]).
///
/// `cpu_id` is the logic ID of the current CPU, and `arg` is passed from the
/// bootloader (typically the device tree blob address).
///
/// In multi-core environment, this function is called on the primary core, and
/// secondary cores call [`rust_main_secondary`].
#[cfg_attr(not(test), axplat::main)]
pub fn rust_main(cpu_id: usize, arg: usize) -> ! {
    #[cfg(not(feature = "plat-dyn"))]
    unsafe {
        axhal::mem::clear_bss()
    };
    axhal::percpu::init_primary(cpu_id);
    axhal::init_early(cpu_id, arg);
    let log_level = option_env!("AX_LOG").unwrap_or("info");
    let show_banner = build_flag_enabled(option_env!("AX_START_BANNER"));
    let enable_backtrace = build_flag_enabled(option_env!("AX_BACKTRACE"));

    if show_banner {
        ax_println!("{}", LOGO);
        ax_println!(
            indoc::indoc! {"
                arch = {}
                platform = {}
                target = {}
                build_mode = {}
                log_level = {}
                backtrace = {}
                smp = {}
            "},
            axconfig::ARCH,
            axconfig::PLATFORM,
            option_env!("AX_TARGET").unwrap_or(""),
            option_env!("AX_MODE").unwrap_or(""),
            log_level,
            enable_backtrace,
            axhal::cpu_num()
        );
    }

    #[cfg(feature = "rtc")]
    if show_banner {
        ax_println!(
            "Boot at {}\n",
            chrono::DateTime::from_timestamp_nanos(axhal::time::wall_time_nanos() as _),
        );
    }

    axlog::init();
    axlog::set_max_level(log_level); // no effect if set `log-level-*` features
    info!("Logging is enabled.");
    info!("Primary CPU {cpu_id} started, arg = {arg:#x}.");

    info!("Found physcial memory regions:");
    for r in axhal::mem::memory_regions() {
        info!(
            "  [{:x?}, {:x?}) {} ({:?})",
            r.paddr,
            r.paddr + r.size,
            r.name,
            r.flags
        );
    }

    #[cfg(feature = "alloc")]
    init_allocator();

    if enable_backtrace {
        use core::ops::Range;

        unsafe extern "C" {
            safe static _stext: [u8; 0];
            safe static _etext: [u8; 0];
            safe static _edata: [u8; 0];
        }

        axbacktrace::init(
            Range {
                start: _stext.as_ptr() as usize,
                end: _etext.as_ptr() as usize,
            },
            Range {
                start: _edata.as_ptr() as usize,
                end: usize::MAX,
            },
        );
    }

    let (kernel_space_start, kernel_space_size) = axhal::mem::kernel_aspace();

    info!(
        "kernel aspace: [{:#x?}, {:#x?})",
        kernel_space_start,
        kernel_space_start + kernel_space_size,
    );

    #[cfg(feature = "paging")]
    axmm::init_memory_management();

    // #[cfg(feature = "plat-dyn")]
    // axdriver::setup(arg);

    info!("Initialize platform devices...");
    axhal::init_later(cpu_id, arg);

    #[cfg(feature = "multitask")]
    if let Err(error) = axtask::init_scheduler() {
        error!("Primary task scheduler initialization failed: {error:?}");
        axhal::power::system_off();
    }

    #[cfg(feature = "axdriver")]
    {
        #[allow(unused_variables)]
        let all_devices = axdriver::init_drivers();

        cfg_if::cfg_if! {
            if #[cfg(feature = "fs-ng")] {
                axfs_ng::init_filesystems(all_devices.block);
            } else
            if #[cfg(feature = "fs")] {
                axfs::init_filesystems(all_devices.block, axhal::dtb::get_chosen_bootargs());
            }
        }

        cfg_if::cfg_if! {
            if #[cfg(feature = "net-ng")] {
                if let Err(error) = axnet_ng::init_network(all_devices.net) {
                    error!("Network subsystem initialization failed: {error:?}");
                    axhal::power::system_off();
                }

                #[cfg(feature = "vsock")]
                axnet_ng::init_vsock(all_devices.vsock);
            } else if #[cfg(feature = "net")] {
                axnet::init_network(all_devices.net);
            }
        }

        #[cfg(feature = "display")]
        axdisplay::init_display(all_devices.display);

        #[cfg(feature = "input")]
        axinput::init_input(all_devices.input);
    }

    #[cfg(feature = "smp")]
    self::mp::start_secondary_cpus(cpu_id);

    #[cfg(feature = "irq")]
    {
        info!("Initialize interrupt handlers...");
        init_interrupt();
    }

    #[cfg(all(feature = "tls", not(feature = "multitask")))]
    {
        info!("Initialize thread local storage...");
        init_tls();
    }

    ctor_bare::call_ctors();

    info!("Primary CPU {cpu_id} init OK.");
    INITED_CPUS.fetch_add(1, Ordering::Release);

    while !is_init_ok() {
        core::hint::spin_loop();
    }

    unsafe { main() };

    #[cfg(feature = "multitask")]
    axtask::exit(0);
    #[cfg(not(feature = "multitask"))]
    {
        debug!("main task exited: exit_code={}", 0);
        axhal::power::system_off();
    }
}

#[cfg(feature = "alloc")]
fn init_allocator() {
    use axhal::mem::{MemRegionFlags, memory_regions, phys_to_virt};

    info!("Initialize global memory allocator...");
    info!("  use {} allocator.", axalloc::global_allocator().name());

    let mut max_region_size = 0;
    let mut max_region_paddr = 0.into();
    let mut use_next_free = false;

    for r in memory_regions() {
        if r.name == ".bss" {
            use_next_free = true;
        } else if r.flags.contains(MemRegionFlags::FREE) {
            if use_next_free {
                max_region_paddr = r.paddr;
                break;
            } else if r.size > max_region_size {
                max_region_size = r.size;
                max_region_paddr = r.paddr;
            }
        }
    }

    #[cfg(feature = "hv")]
    {
        struct AddrTranslatorImpl;
        impl axalloc::AddrTranslator for AddrTranslatorImpl {
            fn virt_to_phys(&self, va: usize) -> Option<usize> {
                Some(axhal::mem::virt_to_phys(va.into()).as_usize())
            }
        }

        static TRANSLATOR: AddrTranslatorImpl = AddrTranslatorImpl;

        for r in memory_regions() {
            if r.flags.contains(MemRegionFlags::FREE) && r.paddr == max_region_paddr {
                axalloc::global_init(phys_to_virt(r.paddr).as_usize(), r.size, &TRANSLATOR);
                break;
            }
        }
    }

    #[cfg(not(feature = "hv"))]
    {
        for r in memory_regions() {
            if r.flags.contains(MemRegionFlags::FREE) && r.paddr == max_region_paddr {
                axalloc::global_init(phys_to_virt(r.paddr).as_usize(), r.size);
                break;
            }
        }
    }

    for r in memory_regions() {
        if r.flags.contains(MemRegionFlags::FREE) && r.paddr != max_region_paddr {
            axalloc::global_add_memory(phys_to_virt(r.paddr).as_usize(), r.size)
                .expect("add heap memory region failed");
        }
    }
}

#[cfg(feature = "irq")]
fn init_interrupt() {
    #[cfg(feature = "ipi")]
    axhal::irq::init_ipi_broker(axhal::cpu_num())
        .unwrap_or_else(|error| panic!("failed to initialize the raw IPI broker: {error:?}"));

    #[cfg(all(feature = "ipi", feature = "multitask"))]
    assert!(
        axhal::irq::register_ipi_reason(
            axhal::irq::IpiReason::CallFunction,
            call_function_ipi_handler,
        ),
        "failed to register the bounded call-function IPI consumer"
    );

    assert!(
        axhal::irq::register(axhal::time::irq_num(), || {
            let now_ns = axhal::time::monotonic_time_nanos();
            let periodic_due = advance_periodic_deadline(now_ns);
            let early_due = consume_early_deadline(now_ns);

            #[cfg(not(feature = "multitask"))]
            let _ = (periodic_due, early_due);

            #[cfg(feature = "multitask")]
            if periodic_due {
                axtask::on_timer_tick();
            } else if early_due {
                axtask::on_timer_event();
            }

            rearm_timer(axhal::time::monotonic_time_nanos());
        }),
        "failed to register the timer IRQ handler"
    );

    // Kick the per-CPU timer chain explicitly instead of depending on platform
    // reset state to generate the first interrupt.
    rearm_timer(axhal::time::monotonic_time_nanos());

    // Enable IRQs before starting app
    axhal::asm::enable_irqs();
}

#[cfg(all(feature = "tls", not(feature = "multitask")))]
fn init_tls() {
    let main_tls = axhal::tls::TlsArea::alloc();
    unsafe { axhal::asm::write_thread_pointer(main_tls.tls_ptr() as usize) };
    core::mem::forget(main_tls);
}
