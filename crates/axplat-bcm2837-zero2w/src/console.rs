//! PL011 UART0 console for the Raspberry Pi Zero 2W.
//!
//! PL011 register map (BCM2835 ARM Peripherals, section 13): DR 0x00, FR
//! 0x18, IBRD 0x24, FBRD 0x28, LCRH 0x2c, CR 0x30, IFLS 0x34, IMSC 0x38,
//! MIS 0x40, ICR 0x44. The firmware configures the UART clock (48 MHz);
//! initializing at 115200 baud keeps the console working if firmware left
//! the UART disabled or misconfigured.

use core::fmt::{self, Write};
use core::ptr::{read_volatile, write_volatile};

#[cfg(not(feature = "legacy"))]
use ax_plat::console::{ConsoleDeviceIdError, ConsoleDeviceIdResult, ConsoleIf, ConsoleIrqEvent};
use kspin::SpinNoIrq;

use crate::config::{PHYS_VIRT_OFFSET, UART0_PADDR};
#[cfg(not(feature = "legacy"))]
use crate::config::UART0_IRQ;

// PL011 registers (byte offsets).
const PL011_DR: usize = 0x00;
const PL011_FR: usize = 0x18;
const PL011_IBRD: usize = 0x24;
const PL011_FBRD: usize = 0x28;
const PL011_LCRH: usize = 0x2c;
const PL011_CR: usize = 0x30;
const PL011_IMSC: usize = 0x38;
#[cfg(feature = "irq")]
const PL011_MIS: usize = 0x40;
const PL011_ICR: usize = 0x44;

const FR_TXFF: u32 = 1 << 5; // transmit FIFO full
const FR_RXFE: u32 = 1 << 4; // receive FIFO empty
const FR_BUSY: u32 = 1 << 3;

const LCRH_WLEN_8: u32 = 0b11 << 5;
const LCRH_FEN: u32 = 1 << 4;
const CR_UARTEN: u32 = 1 << 0;
const CR_TXE: u32 = 1 << 8;
const CR_RXE: u32 = 1 << 9;
#[cfg(feature = "irq")]
const IMSC_RXIM: u32 = 1 << 4; // receive interrupt mask

/// Static UART instance. The firmware already enables the PL011 clock and
/// lines; `init_early` reconfigures baud/line settings in place.
static UART: SpinNoIrq<Uart> = SpinNoIrq::new(Uart::new(UART0_PADDR + PHYS_VIRT_OFFSET));

struct Uart {
    base: usize,
}

impl Uart {
    const fn new(base: usize) -> Self {
        Self { base }
    }

    #[inline]
    fn read(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    #[inline]
    fn write(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.base + offset) as *mut u32, value) }
    }

    fn init(&self) {
        // Disable the UART before programming the line configuration.
        self.write(PL011_CR, 0);
        // 115200 baud from a 48 MHz UART clock: div = 48e6 / (16 * 115200)
        // = 26.04, so IBRD = 26, FBRD = round(0.0417 * 64) = 3.
        self.write(PL011_IBRD, 26);
        self.write(PL011_FBRD, 3);
        self.write(PL011_LCRH, LCRH_WLEN_8 | LCRH_FEN);
        self.write(PL011_IMSC, 0);
        self.write(PL011_ICR, 0x7ff);
        self.write(PL011_CR, CR_UARTEN | CR_TXE | CR_RXE);
    }

    fn send_byte(&self, byte: u8) {
        while self.read(PL011_FR) & FR_TXFF != 0 {}
        self.write(PL011_DR, byte as u32);
    }

    fn try_receive(&self) -> Option<u8> {
        if self.read(PL011_FR) & FR_RXFE != 0 {
            None
        } else {
            Some((self.read(PL011_DR) & 0xff) as u8)
        }
    }

    #[allow(dead_code)]
    fn flush(&self) {
        while self.read(PL011_FR) & FR_BUSY != 0 {}
    }
}

/// Initializes the console as early as possible. Called from `InitIf::init_early`.
pub(crate) fn init_early() {
    UART.lock().init();
}

/// Console used before `init_early` (e.g. from boot diagnostics).
#[allow(dead_code)]
pub(crate) struct EarlyConsole;

impl Write for EarlyConsole {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let base = UART0_PADDR + PHYS_VIRT_OFFSET;
        let uart = Uart::new(base);
        for byte in s.bytes() {
            match byte {
                b'\n' => uart.send_byte(b'\r'),
                _ => uart.send_byte(byte),
            }
        }
        Ok(())
    }
}

/// Writes bytes to the console (shared with the legacy interface).
pub(crate) fn write_bytes(bytes: &[u8]) {
    let lock = UART.lock();
    for &byte in bytes {
        lock.send_byte(byte);
    }
}

/// Reads bytes from the console (shared with the legacy interface).
pub(crate) fn read_bytes(bytes: &mut [u8]) -> usize {
    let lock = UART.lock();
    let mut read = 0;
    for byte in bytes.iter_mut() {
        match lock.try_receive() {
            Some(value) => {
                *byte = value;
                read += 1;
            }
            None => break,
        }
    }
    read
}

#[cfg(not(feature = "legacy"))]
struct ConsoleIfImpl;

#[cfg(not(feature = "legacy"))]
#[impl_plat_interface]
impl ConsoleIf for ConsoleIfImpl {
    fn write_bytes(bytes: &[u8]) {
        write_bytes(bytes);
    }

    fn read_bytes(bytes: &mut [u8]) -> usize {
        read_bytes(bytes)
    }

    fn device_id() -> ConsoleDeviceIdResult {
        // Static platform without a runtime device manager.
        Err(ConsoleDeviceIdError::NotSpecified)
    }

    fn claim_runtime_output() {
        // The static console is not backed by a runtime-owned device, so the
        // low-level write path stays in charge.
    }

    #[cfg(feature = "irq")]
    fn irq_num() -> Option<ax_plat::irq::IrqId> {
        Some(ax_plat::irq::IrqNumber(UART0_IRQ).expect("UART0 IRQ fits legacy IRQ width"))
    }

    #[cfg(feature = "irq")]
    fn set_input_irq_enabled(enabled: bool) {
        let lock = UART.lock();
        let mut imsc = lock.read(PL011_IMSC);
        if enabled {
            imsc |= IMSC_RXIM;
        } else {
            imsc &= !IMSC_RXIM;
        }
        lock.write(PL011_IMSC, imsc);
    }

    #[cfg(feature = "irq")]
    fn handle_irq() -> ConsoleIrqEvent {
        let lock = UART.lock();
        let mis = lock.read(PL011_MIS);
        if mis & IMSC_RXIM == 0 {
            return ConsoleIrqEvent::empty();
        }
        lock.write(PL011_ICR, IMSC_RXIM);
        let mut event = ConsoleIrqEvent::RX_READY;
        // PL011 reports errors via the RX status flags in DR reads; poll a
        // single byte's flags for the common framing/overrun cases.
        if let Some(byte) = lock.try_receive() {
            if byte & 0x0e != 0 {
                event |= ConsoleIrqEvent::RX_ERROR;
            }
        }
        event
    }
}
