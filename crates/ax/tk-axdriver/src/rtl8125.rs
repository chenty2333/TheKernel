//! PCI/platform seam for the shared RTL8125B / RTL8168H driver.
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
    length: usize,
    #[cfg(all(feature = "rtl8168", target_os = "none"))]
    msi: Option<msi::Owner>,
}
impl Bus for Window {
    fn irq_num(&self) -> Option<usize> {
        #[cfg(all(feature = "rtl8168", target_os = "none"))]
        {
            self.msi.as_ref().map(|owner| owner.vector())
        }
        #[cfg(not(all(feature = "rtl8168", target_os = "none")))]
        {
            None
        }
    }
    fn interrupts_available(&self) -> bool {
        #[cfg(all(feature = "rtl8168", target_os = "none"))]
        {
            self.msi.is_some()
        }
        #[cfg(not(all(feature = "rtl8168", target_os = "none")))]
        {
            false
        }
    }
    fn read(&mut self, offset: usize, width: Width) -> u32 {
        let bytes = match width {
            Width::Byte => 1,
            Width::Word => 2,
            Width::Dword => 4,
        };
        if offset
            .checked_add(bytes)
            .is_none_or(|end| end > self.length)
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
            .is_none_or(|end| end > self.length)
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
        "r8169: {bdf} {:04x}:{:04x} phase 0; hardware-unverified (未在硬件上验证)",
        info.vendor_id,
        info.device_id
    );
    if info.class != 2 || info.subclass != 0 {
        return BusProbeResult::Claimed;
    }
    // Both admitted chips use BAR2; 8168H maps 4 KiB, not 8125's 64 KiB.
    let length = if info.device_id == 0x8168 {
        0x1000
    } else {
        rtl8125::regs::WINDOW
    };
    let Ok(BarInfo::Memory { address, size, .. }) = root.bar_info(bdf, 2) else {
        log::warn!("r8169: {bdf}: BAR2 is not assigned memory; stopped");
        return BusProbeResult::Claimed;
    };
    if address == 0 || size < length as u32 {
        log::warn!("r8169: {bdf}: BAR2 {address:#x}/{size:#x} is too small; stopped");
        return BusProbeResult::Claimed;
    }
    let Ok(base) = axklib::mem::iomap(PhysAddr::from_usize(address as usize), length) else {
        log::warn!("r8169: {bdf}: MMIO mapping failed; stopped");
        return BusProbeResult::Claimed;
    };
    let mut window = Window {
        base: base.as_usize(),
        length,
        #[cfg(all(feature = "rtl8168", target_os = "none"))]
        msi: None,
    };
    let revision = window.read(rtl8125::regs::TX_CONFIG, Width::Dword);
    let xid = rtl8125::ids::xid(revision);
    let Some(chip) = rtl8125::ids::pci_chip(info.device_id, revision) else {
        log::warn!("r8169: {bdf}: TxConfig={revision:#010x} XID={xid:#05x} unsupported; no reset");
        return BusProbeResult::Claimed;
    };
    log::info!(
        "r8169: {bdf}: {} TxConfig={revision:#010x} XID={xid:#05x}; warm PXE PHY",
        chip.name()
    );
    #[cfg(all(feature = "rtl8168", target_os = "none"))]
    {
        let (mask, _, width) = chip.irq();
        window.write(mask, width, 0);
        window.msi = msi::install(root, bdf, window.base, chip);
        log::info!(
            "r8169: {bdf}: MSI={}, 10ms polling fallback retained",
            window.interrupts_available()
        );
    }
    match RtlNic::<PlatformHal, _, 256>::new(window) {
        Ok(mut nic) => {
            log::info!(
                "r8169: {bdf}: phase 1/2 rings enabled, link={}, polling, hardware packet \
                 transfer unverified",
                nic.link_up()
            );
            #[cfg(all(net_dev = "n305-net", not(feature = "dyn")))]
            match crate::AxDeviceEnum::try_from_net(nic) {
                Ok(device) => BusProbeResult::Device(device),
                Err(error) => {
                    log::warn!("r8169: {bdf}: could not publish NIC: {error:?}");
                    BusProbeResult::Claimed
                }
            }
            #[cfg(not(all(net_dev = "n305-net", not(feature = "dyn"))))]
            BusProbeResult::Device(crate::AxDeviceEnum::from_net(nic))
        }
        Err(error) => {
            log::warn!("r8169: {bdf}: initialization stopped: {error:?}; no interface registered");
            BusProbeResult::Claimed
        }
    }
}
pub(crate) fn finish_probe() {
    if !FOUND.load(Ordering::Acquire) {
        log::info!(
            "r8169: no supported PCI device 10ec:8125/8168 present; no RTL hardware was touched"
        );
    }
}

// MSI is optional. The polling deadline remains active even with MSI so lost
// interrupts cannot strand a native ring. IRQ handling touches status only.
#[cfg(all(feature = "rtl8168", target_os = "none"))]
mod msi {
    use core::sync::atomic::AtomicUsize;

    use super::*;
    static NIC_BASE: AtomicUsize = AtomicUsize::new(0);
    static IS_8168: AtomicBool = AtomicBool::new(false);
    pub struct Owner {
        config: usize,
        offset: usize,
        vector: usize,
    }
    fn acknowledge() {
        let base = NIC_BASE.load(Ordering::Acquire);
        if base == 0 {
            return;
        }
        // SAFETY: owner retains the mapped window until handler removal. No
        // ring pointers, allocation, logging or sleeping locks in this IRQ.
        unsafe {
            if IS_8168.load(Ordering::Relaxed) {
                let status = (base + 0x3e) as *mut u16;
                status.write_volatile(status.read_volatile());
            } else {
                let status = (base + 0x3c) as *mut u32;
                status.write_volatile(status.read_volatile());
            }
        }
    }
    impl Owner {
        pub fn vector(&self) -> usize {
            self.vector
        }
    }
    impl Drop for Owner {
        fn drop(&mut self) {
            // SAFETY: the dedicated capability page remains mapped; disable
            // MSI before disconnecting the owner and IRQ dispatcher.
            unsafe {
                let control = (self.config + self.offset + 2) as *mut u16;
                control.write_volatile(control.read_volatile() & !1);
            }
            let _ = axhal::irq::unregister(self.vector);
            NIC_BASE.store(0, Ordering::Release);
        }
    }
    pub fn install(
        root: &PciRoot,
        bdf: DeviceFunction,
        base: usize,
        chip: rtl8125::ids::Chip,
    ) -> Option<Owner> {
        let cap = root.capabilities(bdf).take(48).find(|cap| cap.id == 5)?;
        let offset = usize::from(cap.offset);
        if !(0x40..=0xe0).contains(&offset) || !offset.is_multiple_of(4) {
            return None;
        }
        let cpuid = core::arch::x86_64::__cpuid_count(0x0b, 0);
        let destination = if cpuid.ebx != 0 {
            cpuid.edx
        } else {
            core::arch::x86_64::__cpuid(1).ebx >> 24
        };
        if destination > 255 {
            return None;
        }
        let (first, last) = axhal::pci::ecam_bus_range();
        if !(first..=last).contains(&bdf.bus) {
            return None;
        }
        let address = axhal::pci::ecam_base()
            .checked_add(usize::from(bdf.bus - first) << 20)?
            .checked_add(usize::from(bdf.device) << 15)?
            .checked_add(usize::from(bdf.function) << 12)?;
        let config = axklib::mem::iomap(PhysAddr::from_usize(address), 4096)
            .ok()?
            .as_usize();
        if NIC_BASE
            .compare_exchange(0, base, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        IS_8168.store(chip == rtl8125::ids::Chip::Rtl8168H, Ordering::Release);
        let Some(vector) = (0xd0..0xe0).find(|vector| axhal::irq::register(*vector, acknowledge))
        else {
            NIC_BASE.store(0, Ordering::Release);
            return None;
        };
        // MSI and MSI-X must not both remain enabled after the EFI handoff.
        if let Some(cap) = root.capabilities(bdf).take(48).find(|cap| cap.id == 0x11) {
            let offset = usize::from(cap.offset);
            if (0x40..=0xf0).contains(&offset) && offset.is_multiple_of(4) {
                unsafe {
                    let control = (config + offset + 2) as *mut u16;
                    control.write_volatile(control.read_volatile() & !(1 << 15));
                }
            }
        }
        // One message, 32/64-bit address supported; preserve capability header
        // and adjacent data. MMIO port masks are still zero at this point.
        unsafe {
            let ctrl = (config + offset + 2) as *mut u16;
            let old = ctrl.read_volatile();
            ctrl.write_volatile(old & !0x71);
            ((config + offset + 4) as *mut u32).write_volatile(0xfee0_0000 | (destination << 12));
            let data_offset = if old & 0x80 != 0 {
                ((config + offset + 8) as *mut u32).write_volatile(0);
                12
            } else {
                8
            };
            ((config + offset + data_offset) as *mut u16).write_volatile(vector as u16);
            if old & 0x100 != 0 {
                let mask_offset = if old & 0x80 != 0 { 16 } else { 12 };
                ((config + offset + mask_offset) as *mut u32).write_volatile(0);
            }
            ctrl.write_volatile((old & !0x70) | 1);
            let owner = Owner {
                config,
                offset,
                vector,
            };
            if ctrl.read_volatile() & 1 == 0 {
                drop(owner);
                return None;
            }
            Some(owner)
        }
    }
}
