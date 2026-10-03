//! NVMe platform seam. No disk writes without the exact boot parameter.
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};
static CLAIMED: AtomicBool = AtomicBool::new(false);
use axalloc::{UsageKind, global_allocator};
use axdriver_nvme::{Controller, Hal, regs::Bus};
use axdriver_pci::{BarInfo, DeviceFunction, DeviceFunctionInfo, PciRoot};

use crate::drivers::{BusProbeResult, DriverProbe};
pub struct PlatformHal;
// SAFETY: x86 product uses coherent identity DMA, with page-owned contiguous allocations.
unsafe impl Hal for PlatformHal {
    fn allocate(pages: usize) -> Option<(u64, NonNull<u8>)> {
        let virtual_address = global_allocator()
            .alloc_pages(pages, 4096, UsageKind::Dma)
            .ok()?;
        Some((
            axhal::mem::virt_to_phys(virtual_address.into()).as_usize() as u64,
            NonNull::new(virtual_address as *mut u8)?,
        ))
    }
    unsafe fn release(_address: u64, pointer: NonNull<u8>, pages: usize) {
        global_allocator().dealloc_pages(pointer.as_ptr() as usize, pages, UsageKind::Dma);
    }
}
pub struct Window {
    base: usize,
    size: usize,
}
impl Bus for Window {
    fn read32(&mut self, offset: usize) -> u32 {
        if offset & 3 != 0 || offset.checked_add(4).is_none_or(|end| end > self.size) {
            return u32::MAX;
        }
        // SAFETY: full BAR mapped by probe, aligned bounded dword.
        unsafe { ((self.base + offset) as *const u32).read_volatile() }
    }
    fn write32(&mut self, offset: usize, value: u32) {
        if offset & 3 != 0 || offset.checked_add(4).is_none_or(|end| end > self.size) {
            return;
        }
        // SAFETY: same validated BAR as read32.
        unsafe {
            ((self.base + offset) as *mut u32).write_volatile(value);
        }
    }
    fn delay_us(&mut self, micros: u32) {
        axhal::time::busy_wait(core::time::Duration::from_micros(u64::from(micros)));
    }
}
pub type NvmeDevice = Controller<PlatformHal, Window>;
pub struct NvmeDriver;
impl DriverProbe for NvmeDriver {
    fn probe_pci(
        root: &mut PciRoot,
        bdf: DeviceFunction,
        info: &DeviceFunctionInfo,
    ) -> BusProbeResult {
        if !axdriver_nvme::ids::matches(info.class, info.subclass, info.prog_if) {
            return BusProbeResult::NotMatched;
        }
        if CLAIMED.swap(true, Ordering::AcqRel) {
            return BusProbeResult::Claimed;
        }
        let Ok(BarInfo::Memory { address, size, .. }) = root.bar_info(bdf, 0) else {
            return BusProbeResult::Claimed;
        };
        if address == 0 || size < 0x1018 {
            return BusProbeResult::Claimed;
        }
        let Ok(base) = axklib::mem::iomap((address as usize).into(), size as usize) else {
            return BusProbeResult::Claimed;
        };
        let allow_write = axhal::boot::command_line_value("nvme.allow_write") == Some("1");
        if allow_write {
            const WARNING: &[u8] =
                b"\n!!! NVMe WRITES ENABLED: nvme.allow_write=1; WINDOWS DATA AT RISK !!!\n";
            axhal::console::write_tty_bytes(WARNING);
            let _ = axhal::console::try_write_diagnostic_bytes(WARNING);
            log::warn!(
                "!!! NVMe WRITES ENABLED by nvme.allow_write=1: ALL DATA INCLUDING WINDOWS IS AT \
                 RISK !!!"
            );
        }
        match NvmeDevice::new(
            Window {
                base: base.as_usize(),
                size: size as usize,
            },
            allow_write,
            size as usize,
        ) {
            Ok(device) => {
                log::info!(
                    "nvme: {bdf} {:04x}:{:04x} /dev/nvme0n1 queues={} read_only={} polling; \
                     未在硬件上验证",
                    info.vendor_id,
                    info.device_id,
                    device.queue_count(),
                    device.read_only()
                );
                BusProbeResult::Device(crate::AxDeviceEnum::Block(crate::StaticBlockDevice::Nvme(
                    alloc::boxed::Box::new(device),
                )))
            }
            Err(error) => {
                log::warn!("nvme: {bdf} stopped safely: {error:?}");
                BusProbeResult::Claimed
            }
        }
    }
}
