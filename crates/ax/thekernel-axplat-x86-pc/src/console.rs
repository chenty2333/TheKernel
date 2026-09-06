//! Uart 16550 serial port.

use axplat::console::ConsoleIf;
use kspin::SpinNoIrq;
use uart_16550::SerialPort;

static COM1: SpinNoIrq<SerialPort> = unsafe { SpinNoIrq::new(SerialPort::new(0x3f8)) };
const COM1_IRQ_VECTOR: usize = 0x24;

/// Writes a byte to the console.
pub fn putchar(c: u8) {
    COM1.lock().send(c)
}

/// Reads a byte from the console, or returns [`None`] if no input is available.
pub fn getchar() -> Option<u8> {
    COM1.lock().try_receive().ok()
}

pub fn init() {
    COM1.lock().init();
    init_diagnostic();
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
        Some(COM1_IRQ_VECTOR)
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
    if let Some(_guard) = DIAGNOSTIC.try_lock() {
        unsafe {
            use x86::io::{inb, outb};
            let base = DIAGNOSTIC_BASE;
            // Clear DLAB before disabling interrupts (IER shares DLM).
            outb(base + 3, 0x03);
            outb(base + 1, 0);
            if inb(base + 5) == 0xff {
                return;
            }
            let scratch = inb(base + 7);
            outb(base + 7, 0x5a);
            let first = inb(base + 7);
            outb(base + 7, 0xa5);
            let second = inb(base + 7);
            outb(base + 7, scratch);
            if first != 0x5a || second != 0xa5 {
                return;
            }
            outb(base + 3, 0x80);
            outb(base, 1); // 115200 baud, 8-N-1.
            outb(base + 1, 0);
            outb(base + 3, 0x03);
            outb(base + 2, 0xc7);
            outb(base + 4, 0x03); // DTR/RTS; no interrupt output.
            outb(base + 1, 0);
            DIAGNOSTIC_PRESENT.store(true, core::sync::atomic::Ordering::Release);
        }
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
}
