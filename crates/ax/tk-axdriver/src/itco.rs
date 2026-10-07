//! PCI/platform seam for iTCO; opt-in only. N305 path 未在硬件上验证.
use axdriver_pci::{DeviceFunction, DeviceFunctionInfo, PciRoot};
use axdriver_watchdog::{
    Error,
    bringup::Tco,
    ids::{self, Version},
    probe,
    regs::Bus,
};
use kspin::SpinNoIrq;
static DEVICE: SpinNoIrq<Option<Tco<Ports>>> = SpinNoIrq::new(None);
struct Ports {
    tco: u16,
    smi: Option<u16>,
    gcs: usize,
}
fn input16(port: u16) -> u16 {
    let value;
    // SAFETY: admitted PCI identity supplies a bounded, assigned chipset IO/MMIO resource.
    unsafe {
        core::arch::asm!("in ax, dx", in("dx") port, out("ax") value, options(nostack, preserves_flags));
    }
    value
}
fn output16(port: u16, value: u16) {
    // SAFETY: admitted PCI identity supplies a bounded, assigned chipset IO/MMIO resource.
    unsafe {
        core::arch::asm!("out dx, ax", in("dx") port, in("ax") value, options(nostack, preserves_flags));
    }
}
fn input32(port: u16) -> u32 {
    let value;
    // SAFETY: admitted PCI identity supplies a bounded, assigned chipset IO/MMIO resource.
    unsafe {
        core::arch::asm!("in eax, dx", in("dx") port, out("eax") value, options(nostack, preserves_flags));
    }
    value
}
fn output32(port: u16, value: u32) {
    // SAFETY: admitted PCI identity supplies a bounded, assigned chipset IO/MMIO resource.
    unsafe {
        core::arch::asm!("out dx, eax", in("dx") port, in("eax") value, options(nostack, preserves_flags));
    }
}
impl Bus for Ports {
    fn read16(&mut self, offset: u16) -> u16 {
        input16(self.tco + offset)
    }
    fn write16(&mut self, offset: u16, value: u16) {
        output16(self.tco + offset, value);
    }
    fn read_no_reboot(&mut self) -> u32 {
        // SAFETY: admitted PCI identity supplies a bounded, assigned chipset IO/MMIO resource.
        unsafe { (self.gcs as *const u32).read_volatile() }
    }
    fn write_no_reboot(&mut self, value: u32) {
        // SAFETY: admitted PCI identity supplies a bounded, assigned chipset IO/MMIO resource.
        unsafe {
            (self.gcs as *mut u32).write_volatile(value);
        }
    }
    fn read_smi_enable(&mut self) -> u32 {
        self.smi.map_or(0, input32)
    }
    fn write_smi_enable(&mut self, value: u32) {
        if let Some(port) = self.smi {
            output32(port, value);
        }
    }
}
pub(crate) fn probe(_root: &PciRoot, bdf: DeviceFunction, info: &DeviceFunctionInfo) {
    let line = axhal::boot::command_line().unwrap_or("");
    let timeout = match probe::timeout(line) {
        Ok(Some(value)) => value,
        Ok(None) => return,
        Err(_) => {
            log::warn!("itco: invalid watchdog.timeout; not armed");
            return;
        }
    };
    let Some(version) = ids::identify(info.vendor_id, info.device_id) else {
        return;
    };
    if DEVICE.lock().is_some() {
        return;
    }
    let result = (|| {
        let (first, last) = axhal::pci::ecam_bus_range();
        if !(first..=last).contains(&bdf.bus) {
            return Err(Error::Invalid);
        }
        let address = axhal::pci::ecam_base()
            + (usize::from(bdf.bus - first) << 20)
            + (usize::from(bdf.device) << 15)
            + (usize::from(bdf.function) << 12);
        let config = axklib::mem::iomap(address.into(), 4096)
            .map_err(|_| Error::Io)?
            .as_usize();
        // SAFETY: aligned offsets in the dedicated mapped PCI function page.
        let read = |offset| unsafe { ((config + offset) as *const u32).read_volatile() };
        let resources = match version {
            Version::CnlV6 => probe::resources(version, read(0x50), read(0x54))?,
            Version::Ich9V2 => probe::resources(version, read(0x40), read(0xf0))?,
        };
        let gcs = if let Some(address) = resources.gcs {
            axklib::mem::iomap((address as usize & !4095).into(), 4096)
                .map_err(|_| Error::Io)?
                .as_usize()
                + (address as usize & 4095)
        } else {
            0
        };
        let ports = Ports {
            tco: resources.tco,
            smi: resources.smi,
            gcs,
        };
        let mut device = Tco::new(ports, version);
        let start = device.start(timeout);
        if start.is_ok() || device.available() {
            // Keep a verified running controller if takeover could not halt a
            // firmware-started timer, or retain retryable setup state after a
            // later control-register readback failure.
            *DEVICE.lock() = Some(device);
        }
        start?;
        log::info!(
            "itco: {bdf} v{} base={:#x} armed timeout={}s; N305 hardware-unverified",
            version.number(),
            resources.tco,
            timeout
        );
        Ok::<(), Error>(())
    })();
    if let Err(error) = result {
        log::warn!("itco: {bdf} not armed: {error:?}");
    }
}
pub fn available() -> bool {
    DEVICE.lock().as_ref().is_some_and(Tco::available)
}
pub fn info() -> Option<(u32, u32, bool, bool)> {
    // `boot_status` reports the recognized ICH9 v2 flag. False on v6 means
    // that this driver has no supported status bit to report, not a verified
    // absence of an earlier reset.
    DEVICE
        .lock()
        .as_ref()
        .map(|d| (d.version.number(), d.timeout, d.boot_status, d.running))
}
pub fn keepalive() -> Result<(), Error> {
    let mut slot = DEVICE.lock();
    let d = slot.as_mut().ok_or(Error::Invalid)?;
    d.ping()
}
pub fn set_timeout(value: u32) -> Result<(), Error> {
    let mut slot = DEVICE.lock();
    let d = slot.as_mut().ok_or(Error::Invalid)?;
    d.set_timeout(value)?;
    if d.running {
        d.ping()?;
    }
    Ok(())
}
pub fn set_enabled(value: bool) -> Result<(), Error> {
    let mut slot = DEVICE.lock();
    let d = slot.as_mut().ok_or(Error::Invalid)?;
    if value { d.enable() } else { d.stop() }
}
pub fn time_left() -> Result<u32, Error> {
    DEVICE
        .lock()
        .as_mut()
        .map(|d| d.time_left())
        .ok_or(Error::Invalid)
}
