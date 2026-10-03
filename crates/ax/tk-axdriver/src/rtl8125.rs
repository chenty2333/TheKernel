//! PCI/platform seam for the RTL8125B warm-PHY polling driver.
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use axalloc::{UsageKind, global_allocator};
use axdriver_net::rtl8125::{
    self, Hal,
    nic::RtlNic,
    regs::{Bus, Width},
};
use axdriver_pci::{BarInfo, DeviceFunction, DeviceFunctionInfo, PciRoot};
use axhal::mem::{PhysAddr, virt_to_phys};

use crate::drivers::BusProbeResult;
static FOUND: AtomicBool = AtomicBool::new(false);
pub struct PlatformHal;
impl Hal for PlatformHal {
    fn allocate(pages: usize) -> Option<(u64, NonNull<u8>)> {
        let address = global_allocator()
            .alloc_pages(pages, 4096, UsageKind::Dma)
            .ok()?;
        let pointer = NonNull::new(address as *mut u8)?;
        Some((virt_to_phys(address.into()).as_usize() as u64, pointer))
    }
    unsafe fn deallocate(_address: u64, pointer: NonNull<u8>, pages: usize) {
        global_allocator().dealloc_pages(pointer.as_ptr() as usize, pages, UsageKind::Dma);
    }
}
pub struct Window {
    base: usize,
}
impl Bus for Window {
    fn read(&mut self, offset: usize, width: Width) -> u32 {
        let bytes = match width {
            Width::Byte => 1,
            Width::Word => 2,
            Width::Dword => 4,
        };
        if offset
            .checked_add(bytes)
            .is_none_or(|end| end > rtl8125::regs::WINDOW)
            || !offset.is_multiple_of(bytes)
        {
            return u32::MAX;
        }
        // SAFETY: probe mapped the complete window, and width/alignment were checked.
        unsafe {
            match width {
                Width::Byte => ((self.base + offset) as *const u8).read_volatile() as u32,
                Width::Word => ((self.base + offset) as *const u16).read_volatile() as u32,
                Width::Dword => ((self.base + offset) as *const u32).read_volatile(),
            }
        }
    }
    fn write(&mut self, offset: usize, width: Width, value: u32) {
        let bytes = match width {
            Width::Byte => 1,
            Width::Word => 2,
            Width::Dword => 4,
        };
        if offset
            .checked_add(bytes)
            .is_none_or(|end| end > rtl8125::regs::WINDOW)
            || !offset.is_multiple_of(bytes)
        {
            return;
        }
        // SAFETY: same bounded, aligned, device-memory window as read.
        unsafe {
            match width {
                Width::Byte => ((self.base + offset) as *mut u8).write_volatile(value as u8),
                Width::Word => ((self.base + offset) as *mut u16).write_volatile(value as u16),
                Width::Dword => ((self.base + offset) as *mut u32).write_volatile(value),
            }
        }
    }
    fn delay_us(&mut self, micros: u32) {
        axhal::time::busy_wait(core::time::Duration::from_micros(u64::from(micros)));
    }
}
pub(crate) fn probe(
    root: &mut PciRoot,
    bdf: DeviceFunction,
    info: &DeviceFunctionInfo,
) -> BusProbeResult {
    if !rtl8125::ids::matches(info.vendor_id, info.device_id) {
        return BusProbeResult::NotMatched;
    }
    FOUND.store(true, Ordering::Release);
    log::info!(
        "rtl8125: {bdf} {:04x}:{:04x} phase 0; hardware-unverified (未在硬件上验证)",
        info.vendor_id,
        info.device_id
    );
    if info.class != 2 || info.subclass != 0 {
        return BusProbeResult::Claimed;
    }
    // RTL8125 MMIO is BAR2, not the legacy I/O BAR0 (Linux rtl_init_one).
    let Ok(BarInfo::Memory { address, size, .. }) = root.bar_info(bdf, 2) else {
        log::warn!("rtl8125: {bdf}: BAR2 is not assigned memory; stopped");
        return BusProbeResult::Claimed;
    };
    if address == 0 || size < rtl8125::regs::WINDOW as u32 {
        log::warn!("rtl8125: {bdf}: BAR2 {address:#x}/{size:#x} is too small; stopped");
        return BusProbeResult::Claimed;
    }
    let Ok(base) = axklib::mem::iomap(
        PhysAddr::from_usize(address as usize),
        rtl8125::regs::WINDOW,
    ) else {
        log::warn!("rtl8125: {bdf}: MMIO mapping failed; stopped");
        return BusProbeResult::Claimed;
    };
    let mut window = Window {
        base: base.as_usize(),
    };
    let revision = window.read(rtl8125::regs::TX_CONFIG, Width::Dword);
    log::info!(
        "rtl8125: {bdf}: TxConfig={revision:#010x}; only B/BG is admitted; PHY firmware is not \
         loaded, inheriting PXE PHY state"
    );
    match RtlNic::<PlatformHal, _, 256>::new(window) {
        Ok(mut nic) => {
            log::info!(
                "rtl8125: {bdf}: phase 1/2 rings enabled, link={}, polling, hardware packet \
                 transfer unverified",
                nic.link_up()
            );
            #[cfg(all(net_dev = "n305-net", not(feature = "dyn")))]
            match crate::AxDeviceEnum::try_from_net(nic) {
                Ok(device) => BusProbeResult::Device(device),
                Err(error) => {
                    log::warn!("rtl8125: {bdf}: could not publish NIC: {error:?}");
                    BusProbeResult::Claimed
                }
            }
            #[cfg(not(all(net_dev = "n305-net", not(feature = "dyn"))))]
            BusProbeResult::Device(crate::AxDeviceEnum::from_net(nic))
        }
        Err(error) => {
            log::warn!(
                "rtl8125: {bdf}: initialization stopped: {error:?}; no interface registered"
            );
            BusProbeResult::Claimed
        }
    }
}
pub(crate) fn finish_probe() {
    if !FOUND.load(Ordering::Acquire) {
        log::info!(
            "rtl8125: no supported PCI device 10ec:8125 present; no RTL hardware was touched"
        );
    }
}
