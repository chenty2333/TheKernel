//! Uart 16550 serial port.
//!
//! # A legacy UART is not always there, and its absence is not benign
//!
//! This machine's primary console is the 16550 that PC firmware has decoded at
//! 0x3f8 since 1984.  Nothing guarantees that one exists.  The target hardware
//! for this kernel is a mini PC whose firmware implements no legacy UART at
//! all, and it is not the only such machine.
//!
//! An absent port cannot simply be written to and ignored, because the two
//! ways an undecoded I/O port can answer are both fatal to a driver that
//! assumes a UART:
//!
//! * Reads on an unclaimed port commonly return `0xff`, the floating-bus
//!   value.  `0xff` in the line status register sets *both* "data ready" and
//!   "transmitter empty", so [`getchar`] reports a byte on every call and hands
//!   the line discipline an endless stream of `0xff`.  A console reader never
//!   blocks, and the machine spins consuming its own garbage input.
//! * Reads can instead return `0x00`, which never sets "transmitter empty".
//!   `uart_16550`'s send path retries until that bit is set, so the very first
//!   byte of kernel output hangs the machine before any console exists to say
//!   so -- including the framebuffer console that was supposed to be the
//!   fallback.
//!
//! Neither is fixed by checking the status register, because the status
//! register is the thing that is lying.  The port is therefore *probed* before
//! it is used, using the 16550's scratch register: a real UART preserves a byte
//! written to it and an undecoded port cannot.  Every access to 0x3f8 in this
//! module is gated on the answer, so a machine without a UART writes to no
//! port, reads from no port, and registers no interrupt for one.
//!
//! The probe is the same test the diagnostic port at 0x2f8 has always used.
//! It now has one implementation and two callers instead of two copies.

use axplat::console::ConsoleIf;
use kspin::SpinNoIrq;
use uart_16550::SerialPort;

const COM1_BASE: u16 = 0x3f8;
const COM1_IRQ_VECTOR: usize = 0x24;

static COM1: SpinNoIrq<SerialPort> = unsafe { SpinNoIrq::new(SerialPort::new(COM1_BASE)) };

/// Whether [`COM1_BASE`] answered as a 16550 during [`init`].
///
/// Deliberately `false` until the probe says otherwise.  The first console
/// output in the kernel happens after the platform's early initialization
/// calls [`init`] (see `axruntime::rust_main`, which reaches `init_early`
/// before its first log record), so nothing is lost by starting pessimistic --
/// and starting optimistic would mean touching an unprobed port, which is the
/// hang this module exists to avoid.
#[cfg(target_os = "none")]
static COM1_PRESENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether a 16550 was found at the console's I/O base.
pub fn available() -> bool {
    #[cfg(target_os = "none")]
    {
        COM1_PRESENT.load(core::sync::atomic::Ordering::Acquire)
    }
    #[cfg(not(target_os = "none"))]
    {
        false
    }
}

/// Writes a byte to the console, or drops it if the machine has no UART.
pub fn putchar(c: u8) {
    if !available() {
        return;
    }
    COM1.lock().send(c)
}

/// TTY output has already passed through termios. Do not expand LF or erase
/// bytes again (SerialPort::send would expand both LF and backspace).
pub fn write_tty_bytes(bytes: &[u8]) {
    if !available() { return; }
    for &byte in bytes { COM1.lock().send_raw(byte); }
}

/// Reads a byte from the console.
///
/// Returns [`None`] when no input is available *and* when there is no UART to
/// read from: a machine with no console hardware must not report input it does
/// not have.
pub fn getchar() -> Option<u8> {
    if !available() {
        return None;
    }
    COM1.lock().try_receive().ok()
}

pub fn init() {
    if probe_uart(COM1_BASE) {
        COM1.lock().init();
        #[cfg(target_os = "none")]
        COM1_PRESENT.store(true, core::sync::atomic::Ordering::Release);
    }
    init_diagnostic();
}

/// The byte-level port access [`probe_uart_with`] needs.
///
/// A trait rather than direct `inb`/`outb` calls so the decision the probe
/// makes -- present or absent -- can be tested on a host with no I/O ports at
/// all, and so each way firmware can fail to have a UART has a test naming
/// which way it was rejected.
#[cfg(any(target_os = "none", test))]
trait PortIo {
    /// # Safety
    ///
    /// Performs a real port read on the target; the caller must know the port
    /// belongs to a device it may touch.
    unsafe fn read(&mut self, port: u16) -> u8;
    /// # Safety
    ///
    /// Performs a real port write on the target; see [`PortIo::read`].
    unsafe fn write(&mut self, port: u16, value: u8);
}

#[cfg(target_os = "none")]
struct HardwarePorts;

#[cfg(target_os = "none")]
impl PortIo for HardwarePorts {
    unsafe fn read(&mut self, port: u16) -> u8 {
        unsafe { x86::io::inb(port) }
    }

    unsafe fn write(&mut self, port: u16, value: u8) {
        unsafe { x86::io::outb(port, value) }
    }
}

/// Probes `base` for a 16550 and leaves it configured for 115200 8-N-1.
///
/// Returns whether one was found.  Both checks are needed and each rejects a
/// different way of being absent: `0xff` in the line status register is the
/// floating-bus read that would otherwise look like permanent input, and a
/// scratch register that does not hold what was written to it is a decoded
/// port with no UART behind it, which would otherwise look like a working one
/// with a permanently busy transmitter.
#[cfg(any(target_os = "none", test))]
fn probe_uart_with(io: &mut impl PortIo, base: u16) -> bool {
    unsafe {
        // Clear DLAB before disabling interrupts, because IER shares DLM.
        io.write(base + 3, 0x03);
        io.write(base + 1, 0);
        if io.read(base + 5) == 0xff {
            return false;
        }
        let scratch = io.read(base + 7);
        io.write(base + 7, 0x5a);
        let first = io.read(base + 7);
        io.write(base + 7, 0xa5);
        let second = io.read(base + 7);
        io.write(base + 7, scratch);
        if first != 0x5a || second != 0xa5 {
            return false;
        }
        io.write(base + 3, 0x80);
        io.write(base, 1); // 115200 baud, 8-N-1.
        io.write(base + 1, 0);
        io.write(base + 3, 0x03);
        io.write(base + 2, 0xc7);
        io.write(base + 4, 0x03); // DTR/RTS; no interrupt output.
        io.write(base + 1, 0);
        true
    }
}

/// Probes the real hardware.  Host builds have no ports and report absence.
fn probe_uart(base: u16) -> bool {
    #[cfg(target_os = "none")]
    {
        probe_uart_with(&mut HardwarePorts, base)
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = base;
        false
    }
}

struct ConsoleIfImpl;

#[cfg_attr(target_os = "none", impl_plat_interface)]
impl ConsoleIf for ConsoleIfImpl {
    /// Writes given bytes to the console.
    fn write_bytes(bytes: &[u8]) {
        for c in bytes {
            putchar(*c);
        }
    }

    /// Reads bytes from the console into the given mutable slice.
    ///
    /// Returns the number of bytes read.
    fn read_bytes(bytes: &mut [u8]) -> usize {
        let mut read_len = 0;
        while read_len < bytes.len() {
            if let Some(c) = getchar() {
                bytes[read_len] = c;
            } else {
                break;
            }
            read_len += 1;
        }
        read_len
    }

    /// Returns the IRQ number for the console input interrupt.
    ///
    /// Returns `None` if input interrupt is not supported.
    #[cfg(feature = "irq")]
    fn irq_num() -> Option<usize> {
        // Registering an interrupt for a UART that is not there would wake
        // console readers on a spurious line and, worse, invite them to read
        // the floating bus.  No port, no interrupt: callers fall back to
        // on-demand reads, which a machine with no console hardware answers
        // with "no input", forever.
        available().then_some(COM1_IRQ_VECTOR)
    }
}

// COM2 belongs exclusively to diagnostics. No receive interrupt or console
// interface is installed for it. Keep the lock independent of COM1 and logs.
#[cfg(target_os = "none")]
static DIAGNOSTIC: SpinNoIrq<()> = SpinNoIrq::new(());
#[cfg(target_os = "none")]
static DIAGNOSTIC_PRESENT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
#[cfg(target_os = "none")]
const DIAGNOSTIC_BASE: u16 = 0x2f8;
#[cfg(target_os = "none")]
const DIAGNOSTIC_POLL_BUDGET: usize = 1_000_000;

fn init_diagnostic() {
    #[cfg(target_os = "none")]
    if let Some(_guard) = DIAGNOSTIC.try_lock()
        && probe_uart_with(&mut HardwarePorts, DIAGNOSTIC_BASE)
    {
        DIAGNOSTIC_PRESENT.store(true, core::sync::atomic::Ordering::Release);
    }
}

#[cfg(any(target_os = "none", test))]
fn wait_bounded(budget: &mut usize, mut ready: impl FnMut() -> bool) -> bool {
    while *budget != 0 {
        *budget -= 1;
        if ready() {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

#[cfg(any(target_os = "none", test))]
fn write_bounded(
    bytes: &[u8],
    budget: &mut usize,
    mut ready: impl FnMut() -> bool,
    mut send: impl FnMut(u8),
) -> usize {
    let mut written = 0;
    for &byte in bytes {
        if !wait_bounded(budget, &mut ready) {
            break;
        }
        send(byte);
        written += 1;
    }
    written
}

/// Gives accepted diagnostic bytes a bounded chance to leave the UART before
/// power-off. TEMT includes the shift register; THRE alone is insufficient.
pub(crate) fn flush_diagnostic() {
    #[cfg(target_os = "none")]
    if let Some(_guard) = DIAGNOSTIC.try_lock()
        && diagnostic_available()
    {
        let mut budget = DIAGNOSTIC_POLL_BUDGET;
        let _ = wait_bounded(&mut budget, || unsafe {
            let status = x86::io::inb(DIAGNOSTIC_BASE + 5);
            status != 0xff && status & 0x40 != 0
        });
    }
}

#[cfg(target_os = "none")]
fn write_diagnostic(bytes: &[u8], budget: &mut usize) -> usize {
    write_bounded(
        bytes,
        budget,
        || unsafe {
            let status = x86::io::inb(DIAGNOSTIC_BASE + 5);
            status != 0xff && status & 0x20 != 0
        },
        |byte| unsafe { x86::io::outb(DIAGNOSTIC_BASE, byte) },
    )
}

/// Whether early initialization detected a diagnostic UART. This read-only
/// snapshot never touches hardware or waits for the transmit lock.
pub fn diagnostic_available() -> bool {
    #[cfg(target_os = "none")]
    {
        DIAGNOSTIC_PRESENT.load(core::sync::atomic::Ordering::Acquire)
    }
    #[cfg(not(target_os = "none"))]
    {
        false
    }
}

/// Attempts raw diagnostic output without waiting for a lock or UART capacity.
/// Returns the consumed prefix length; callers retain the remaining bytes.
pub fn try_write_diagnostic_bytes(bytes: &[u8]) -> usize {
    #[cfg(target_os = "none")]
    if let Some(_guard) = DIAGNOSTIC.try_lock()
        && diagnostic_available()
    {
        let mut written = 0;
        for byte in bytes {
            if write_diagnostic(core::slice::from_ref(byte), &mut 1) == 0 {
                break;
            }
            written += 1;
        }
        return written;
    }
    let _ = bytes;
    0
}

/// Allocation-free early/panic diagnostics. Drops output if the port lock is
/// busy and stops after a finite polling budget, even when hardware disappears.
/// Host builds deliberately do not access I/O ports or format arguments.
pub fn emergency_diagnostic_print(args: core::fmt::Arguments<'_>) {
    #[cfg(target_os = "none")]
    if let Some(_guard) = DIAGNOSTIC.try_lock()
        && diagnostic_available()
    {
        struct Writer(usize);
        impl core::fmt::Write for Writer {
            fn write_str(&mut self, text: &str) -> core::fmt::Result {
                if write_diagnostic(text.as_bytes(), &mut self.0) == text.len() {
                    Ok(())
                } else {
                    Err(core::fmt::Error)
                }
            }
        }
        use core::fmt::Write;
        let _ = Writer(DIAGNOSTIC_POLL_BUDGET).write_fmt(args);
    }
    let _ = args;
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn terminal_flush_stops_when_transmitter_becomes_empty() {
        let mut budget = 7;
        let mut checks = 0;
        assert!(wait_bounded(&mut budget, || {
            checks += 1;
            checks == 3
        }));
        assert_eq!(checks, 3);
        assert_eq!(budget, 4);
    }

    #[test]
    fn stalled_terminal_flush_exhausts_shared_budget() {
        let mut budget = 7;
        let mut checks = 0;
        assert!(!wait_bounded(&mut budget, || {
            checks += 1;
            false
        }));
        assert_eq!(checks, 7);
        assert_eq!(budget, 0);
    }

    #[test]
    fn stalled_uart_exhausts_shared_budget() {
        let mut budget = 7;
        let mut checks = 0;
        assert_eq!(
            write_bounded(
                b"abc",
                &mut budget,
                || {
                    checks += 1;
                    false
                },
                |_| panic!("busy UART")
            ),
            0
        );
        assert_eq!(checks, 7);
        assert_eq!(budget, 0);
    }

    #[test]
    fn partial_write_reports_exact_prefix() {
        let mut budget = 3;
        let mut sent = std::vec::Vec::new();
        assert_eq!(
            write_bounded(b"abcd", &mut budget, || true, |b| sent.push(b)),
            3
        );
        assert_eq!(sent, b"abc");
    }

    #[test]
    fn host_diagnostics_do_not_access_ports() {
        assert!(!diagnostic_available());
        assert_eq!(try_write_diagnostic_bytes(b"host"), 0);
        emergency_diagnostic_print(format_args!("host {}", 42));
        flush_diagnostic();
    }

    #[test]
    fn the_host_console_never_reports_hardware_it_does_not_have() {
        // The host has no 0x3f8 to read and no privilege to read one with, so
        // these must be the answers that touch nothing.
        assert!(!available());
        assert_eq!(getchar(), None);
        putchar(b'x');
    }
}

/// The ways a machine can fail to have a UART, and the one way it can have
/// one, modelled as a port space.
///
/// These tests exist because the failure modes are not interchangeable: a
/// floating bus looks like permanent input, a zeroed port looks like a
/// transmitter that is never ready, and a decoded port with no UART behind it
/// looks like both a working status register and a working scratch register.
/// A probe that rejects only one of them is a probe that hangs or floods on
/// the machines that do the others.
#[cfg(test)]
mod probe_tests {
    use std::vec::Vec;

    use super::{PortIo, probe_uart_with};

    const BASE: u16 = 0x3f8;

    /// A 16550 that behaves, plus the three ways one can be missing.
    enum Machine {
        /// A real 16550: it holds its scratch register and reports a sane
        /// line status.
        Uart { scratch: u8, status: u8 },
        /// An undecoded port on the usual floating bus: every read is `0xff`.
        Floating,
        /// An undecoded port on a chipset that drives reads to zero.
        Zeroed,
        /// A decoded port with no UART behind it: the line status reads like a
        /// ready transmitter, but nothing is retained.
        Hollow,
    }

    struct FakePorts {
        machine: Machine,
        reads: Vec<u16>,
        writes: Vec<(u16, u8)>,
    }

    impl FakePorts {
        fn new(machine: Machine) -> Self {
            Self {
                machine,
                reads: Vec::new(),
                writes: Vec::new(),
            }
        }

        fn wrote(&self, port: u16, value: u8) -> bool {
            self.writes.contains(&(port, value))
        }
    }

    impl PortIo for FakePorts {
        unsafe fn read(&mut self, port: u16) -> u8 {
            self.reads.push(port);
            let offset = port - BASE;
            match &mut self.machine {
                Machine::Uart { scratch, status } => match offset {
                    5 => *status,
                    7 => *scratch,
                    _ => 0,
                },
                Machine::Floating => 0xff,
                Machine::Zeroed => 0,
                // A ready transmitter and an empty scratch register: the
                // combination that makes a naive driver hang on the first byte
                // it ever writes.
                Machine::Hollow => match offset {
                    5 => 0x60,
                    _ => 0,
                },
            }
        }

        unsafe fn write(&mut self, port: u16, value: u8) {
            self.writes.push((port, value));
            if let Machine::Uart { scratch, .. } = &mut self.machine
                && port == BASE + 7
            {
                *scratch = value;
            }
        }
    }

    #[test]
    fn a_floating_bus_is_not_mistaken_for_a_uart() {
        let mut ports = FakePorts::new(Machine::Floating);
        assert!(!probe_uart_with(&mut ports, BASE));
        // The rejection must come from the status register, before anything
        // reads the data port: on this machine a data read is what would look
        // like endless input.
        assert!(!ports.reads.contains(&BASE));
    }

    #[test]
    fn a_port_that_reads_as_zero_is_not_mistaken_for_a_uart() {
        let mut ports = FakePorts::new(Machine::Zeroed);
        assert!(!probe_uart_with(&mut ports, BASE));
    }

    #[test]
    fn a_decoded_port_with_no_uart_behind_it_is_not_mistaken_for_one() {
        // This is the dangerous machine: its status register says the
        // transmitter is ready, so only the scratch register reveals that
        // anything written to it goes nowhere.
        let mut ports = FakePorts::new(Machine::Hollow);
        assert!(!probe_uart_with(&mut ports, BASE));
        assert!(ports.wrote(BASE + 7, 0x5a));
        assert!(ports.wrote(BASE + 7, 0xa5));
    }

    #[test]
    fn a_real_uart_is_found_and_left_configured() {
        let mut ports = FakePorts::new(Machine::Uart {
            scratch: 0x00,
            status: 0x60,
        });
        assert!(probe_uart_with(&mut ports, BASE));
        // 8-N-1 with DLAB cleared, FIFO enabled and cleared, DTR/RTS asserted,
        // receive interrupts left off.
        assert!(ports.wrote(BASE + 3, 0x03));
        assert!(ports.wrote(BASE + 2, 0xc7));
        assert!(ports.wrote(BASE + 4, 0x03));
        assert!(ports.wrote(BASE + 1, 0));
    }

    #[test]
    fn the_scratch_register_is_restored_whatever_it_held() {
        // A UART already in use by firmware must come back with its scratch
        // byte unchanged; clobbering it is a side effect the probe has no
        // right to.
        for original in [0x00u8, 0x5a, 0xa5, 0xff] {
            let mut ports = FakePorts::new(Machine::Uart {
                scratch: original,
                status: 0x60,
            });
            assert!(probe_uart_with(&mut ports, BASE));
            let restored = ports
                .writes
                .iter()
                .filter(|(port, _)| *port == BASE + 7)
                .map(|(_, value)| *value)
                .next_back();
            assert_eq!(restored, Some(original), "scratch not restored");
        }
    }

    #[test]
    fn the_probe_does_not_poll_a_status_register_it_cannot_trust() {
        // Every read the probe performs must be a single, unconditional read:
        // a loop on a lying status register is the hang this design exists to
        // prevent.  Counting reads bounds that: the probe reads the status
        // once and the scratch register three times, whatever the machine.
        for machine in [
            Machine::Floating,
            Machine::Zeroed,
            Machine::Hollow,
            Machine::Uart {
                scratch: 0,
                status: 0,
            },
        ] {
            let mut ports = FakePorts::new(machine);
            probe_uart_with(&mut ports, BASE);
            let status_reads = ports.reads.iter().filter(|port| **port == BASE + 5).count();
            assert!(status_reads <= 1, "status register polled {status_reads} times");
        }
    }
}
