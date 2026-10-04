//! x86 coherent-DMA seam for the opt-in DbC transport. Hardware unverified.
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use axalloc::{UsageKind, global_allocator};
use axdriver_dbc::{
    Bus,
    bringup::{Driver, State},
    desc::DMA_BYTES,
};
use kspin::SpinNoIrq;
struct Window {
    mmio: usize,
    size: usize,
    dma: usize,
    physical: u64,
}
impl Bus for Window {
    fn mmio_bytes(&self) -> usize {
        self.size
    }
    fn read32(&self, o: usize) -> u32 {
        debug_assert!(o.is_multiple_of(4) && o + 4 <= self.size);
        unsafe { core::ptr::read_volatile((self.mmio + o) as *const u32) }
    }
    fn write32(&mut self, o: usize, v: u32) {
        debug_assert!(o.is_multiple_of(4) && o + 4 <= self.size);
        unsafe { core::ptr::write_volatile((self.mmio + o) as *mut u32, v) }
    }
    fn dma_read32(&self, o: usize) -> u32 {
        debug_assert!(o.is_multiple_of(4) && o + 4 <= DMA_BYTES);
        unsafe { core::ptr::read_volatile((self.dma + o) as *const u32) }
    }
    fn dma_write32(&mut self, o: usize, v: u32) {
        debug_assert!(o.is_multiple_of(4) && o + 4 <= DMA_BYTES);
        unsafe { core::ptr::write_volatile((self.dma + o) as *mut u32, v) }
    }
    fn dma_read8(&self, o: usize) -> u8 {
        debug_assert!(o < DMA_BYTES);
        unsafe { core::ptr::read_volatile((self.dma + o) as *const u8) }
    }
    fn dma_write8(&mut self, o: usize, v: u8) {
        debug_assert!(o < DMA_BYTES);
        unsafe { core::ptr::write_volatile((self.dma + o) as *mut u8, v) }
    }
    fn physical(&self) -> u64 {
        self.physical
    }
}
static DEVICE: SpinNoIrq<Option<Driver<Window>>> = SpinNoIrq::new(None);
static ATTEMPTED: AtomicBool = AtomicBool::new(false);
static PRESENT: AtomicBool = AtomicBool::new(false);
static TTY_DROPPED: AtomicU64 = AtomicU64::new(0);
/// Call only after the normal xHCI driver's reset and initialization. The PCI
/// direct map must cover the BAR; the core's read-only walk validates its size.
pub(crate) fn probe(mmio: usize, size: usize) {
    let mut bus = Window {
        mmio,
        size,
        dma: 0,
        physical: 0,
    };
    let cap = match axdriver_dbc::probe::find(&bus, size) {
        Ok(Some(cap)) => cap,
        Ok(None) => {
            warn!("\x013usb-dbc: capability absent; normal consoles unchanged");
            return;
        }
        Err(error) => {
            warn!("\x013usb-dbc: probe refused: {error:?}; normal consoles unchanged");
            return;
        }
    };
    if ATTEMPTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let pages = DMA_BYTES / 4096;
    let Ok(dma) = global_allocator().alloc_pages(pages, 65536, UsageKind::Dma) else {
        warn!("\x013usb-dbc: no DMA memory; normal consoles unchanged");
        return;
    };
    // This allocation is intentionally retained until reboot (including on
    // failed enable/disable). No worker or disconnect can free hardware DMA.
    unsafe { core::ptr::write_bytes(dma as *mut u8, 0, DMA_BYTES) };
    bus.dma = dma;
    bus.physical = axhal::mem::virt_to_phys(dma.into()).as_usize() as u64;
    let supports_64 = bus.read32(0x10) & 1 != 0;
    match Driver::start(bus, cap, supports_64) {
        Ok(driver) => {
            *DEVICE.lock() = Some(driver);
            PRESENT.store(true, Ordering::Release);
            info!("usb-dbc: armed at cap={cap:#x}; waiting for debug host (hardware unverified)");
        }
        Err(error) => warn!(
            "\x013usb-dbc: initialization refused: {error:?}; DMA retained, normal consoles \
             unchanged"
        ),
    }
}
pub fn available() -> bool {
    // A momentarily busy TTY writer must not suppress worker creation.
    PRESENT.load(Ordering::Acquire)
}
/// Enqueue without waiting for any lock, host connection or USB completion.
/// Success is admission, not delivery. The independent log cursor retries.
pub fn try_write(bytes: &[u8]) -> usize {
    DEVICE
        .try_lock()
        .and_then(|mut d| d.as_mut().map(|d| d.write(bytes)))
        .unwrap_or(0)
}
/// TTY mirrors are best effort: never stall the screen/serial output on DbC.
pub fn mirror_tty(bytes: &[u8]) {
    let n = try_write(bytes);
    TTY_DROPPED.fetch_add((bytes.len() - n) as u64, Ordering::Relaxed);
}
pub fn input_ready() -> bool {
    DEVICE
        .try_lock()
        .is_some_and(|d| d.as_ref().is_some_and(|d| d.input_ready()))
}
pub fn read(bytes: &mut [u8]) -> usize {
    DEVICE
        .try_lock()
        .and_then(|mut d| d.as_mut().map(|d| d.read(bytes)))
        .unwrap_or(0)
}
/// Bounded poll. Emit state changes only after releasing the transport lock.
pub fn poll() -> bool {
    let Some(mut device) = DEVICE.try_lock() else {
        return true;
    };
    let Some(d) = device.as_mut() else {
        return false;
    };
    let old = d.state();
    d.poll(axhal::time::monotonic_time_nanos() / 1_000_000);
    let new = d.state();
    if matches!(new, State::Failed(_)) {
        PRESENT.store(false, Ordering::Release);
    }
    let stats = d.stats();
    drop(device);
    if old != new {
        match new {
            State::Running => info!("usb-dbc: debug host configured"),
            State::Failed(error) => warn!(
                "\x013usb-dbc: stopped {error:?}; tx={} rx={} events={} tty_dropped={}; reconnect \
                 requires reboot; normal consoles unchanged",
                stats.tx_bytes,
                stats.rx_bytes,
                stats.events,
                TTY_DROPPED.load(Ordering::Relaxed)
            ),
            State::Waiting => {}
        }
    }
    !matches!(new, State::Failed(_))
}
