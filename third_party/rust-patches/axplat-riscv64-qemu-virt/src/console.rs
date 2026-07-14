use core::ptr::write_volatile;

use axplat::console::ConsoleIf;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use uart_16550::MmioSerialPort;

use crate::config::{devices::UART_PADDR, plat::PHYS_VIRT_OFFSET};

static UART: LazyInit<SpinNoIrq<MmioSerialPort>> = LazyInit::new();

pub(crate) fn init_early() {
    let base = UART_PADDR + PHYS_VIRT_OFFSET;
    unsafe {
        // QEMU/OpenSBI already provides a usable NS16550A UART. Avoid
        // rewriting DLL/DLM here: on this platform that divisor-latch write is
        // visible as a leading 0x03 byte on the serial output stream.
        write_volatile((base + 3) as *mut u8, 0x03);
        write_volatile((base + 1) as *mut u8, 0x00);
        write_volatile((base + 2) as *mut u8, 0xC7);
        write_volatile((base + 4) as *mut u8, 0x0B);
        write_volatile((base + 1) as *mut u8, 0x01);
    }
    UART.init_once({
        let uart = unsafe { MmioSerialPort::new(base) };
        SpinNoIrq::new(uart)
    });
}

struct ConsoleIfImpl;

#[impl_plat_interface]
impl ConsoleIf for ConsoleIfImpl {
    /// Writes bytes to the console from input u8 slice.
    fn write_bytes(bytes: &[u8]) {
        for &c in bytes {
            let mut uart = UART.lock();
            match c {
                b'\n' => {
                    uart.send_raw(b'\r');
                    uart.send_raw(b'\n');
                }
                c => uart.send_raw(c),
            }
        }
    }

    /// Reads bytes from the console into the given mutable slice.
    /// Returns the number of bytes read.
    fn read_bytes(bytes: &mut [u8]) -> usize {
        let mut uart = UART.lock();
        for (i, byte) in bytes.iter_mut().enumerate() {
            match uart.try_receive() {
                Ok(c) => *byte = c,
                Err(_) => return i,
            }
        }
        bytes.len()
    }

    /// Returns the IRQ number for the console, if applicable.
    #[cfg(feature = "irq")]
    fn irq_num() -> Option<usize> {
        Some(crate::config::devices::UART_IRQ)
    }
}
