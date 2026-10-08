//! NVMe platform seam. No disk writes without the exact boot parameter.
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
static CLAIMED: AtomicBool = AtomicBool::new(false);
static IRQ_GENERATION: AtomicU64 = AtomicU64::new(0);
static IRQ_OBSERVED: AtomicBool = AtomicBool::new(false);
fn irq_handler() {
    IRQ_GENERATION.fetch_add(1, Ordering::Release);
}

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
        let physical = axhal::mem::virt_to_phys(virtual_address.into()).as_usize() as u64;
        let length = pages.checked_mul(4096)?;
        let device_address = match tk_vtd::platform_map(physical, length) {
            Ok(address) => address,
            Err(_) => {
                global_allocator().dealloc_pages(virtual_address, pages, UsageKind::Dma);
                return None;
            }
        };
        Some((device_address, NonNull::new(virtual_address as *mut u8)?))
    }
    unsafe fn release(address: u64, pointer: NonNull<u8>, pages: usize) {
        if tk_vtd::platform_unmap(address, pages.saturating_mul(4096)).is_err() {
            log::error!(
                "nvme: failed to retire DMA mapping {address:#x}+{:#x}",
                pages.saturating_mul(4096)
            );
            return;
        }
        global_allocator().dealloc_pages(pointer.as_ptr() as usize, pages, UsageKind::Dma);
    }
}
pub struct Window {
    base: usize,
    size: usize,
    irq: Option<usize>,
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
    fn interrupt_enabled(&self) -> bool {
        self.irq.is_some()
    }
    fn interrupt_generation(&self) -> u64 {
        IRQ_GENERATION.load(Ordering::Acquire)
    }
    fn now_us(&self) -> Option<u64> {
        Some(axhal::time::monotonic_time_nanos() / 1000)
    }
    fn wait_completion(&mut self, observed: u64) {
        let Some(vector) = self.irq.filter(|_| axtask::can_block_current()) else {
            self.delay_us(10);
            return;
        };
        use core::{future::poll_fn, task::Poll, time::Duration};

        use axtask::future::{
            block_on, cancel_irq_waker, register_irq_waker, timeout, update_irq_waker,
        };
        let mut token = None;
        let _ = block_on(timeout(
            Some(Duration::from_micros(100)),
            poll_fn(|cx| {
                if IRQ_GENERATION.load(Ordering::Acquire) != observed {
                    return Poll::Ready(());
                }
                if let Some(existing) = token {
                    if update_irq_waker(existing, cx.waker()).is_err() {
                        return Poll::Ready(());
                    }
                } else {
                    match register_irq_waker(vector, cx.waker()) {
                        Ok(registered) => token = Some(registered),
                        Err(_) => return Poll::Ready(()),
                    }
                }
                if IRQ_GENERATION.load(Ordering::Acquire) != observed {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }),
        ));
        if let Some(token) = token {
            let _ = cancel_irq_waker(token);
        }
        if IRQ_GENERATION.load(Ordering::Acquire) != observed
            && !IRQ_OBSERVED.swap(true, Ordering::AcqRel)
        {
            log::info!(
                "nvme: MSI-X completion wake observed on vector {vector:#x}; polling fallback \
                 remains armed"
            );
        }
    }
}
struct Msix {
    table: usize,
    capability: u8,
    control: u16,
    vector: usize,
}
impl Msix {
    fn disable(root: &mut PciRoot, bdf: DeviceFunction) {
        if let Some(cap) = root
            .capabilities(bdf)
            .take(48)
            .find(|cap| cap.id == 0x11 && cap.offset <= 0xf4)
        {
            root.write_config_u16(bdf, cap.offset + 2, (cap.private_header | 0x4000) & !0x8000);
            let _ = root.read_config_dword(bdf, cap.offset);
        }
    }
    fn prepare(root: &mut PciRoot, bdf: DeviceFunction) -> Option<Self> {
        let capability = root
            .capabilities(bdf)
            .take(48)
            .find(|cap| cap.id == 0x11 && cap.offset <= 0xf4)?;
        let layout = axdriver_nvme::msix::Layout::decode(
            capability.private_header,
            root.read_config_dword(bdf, capability.offset + 4)?,
        )?;
        let BarInfo::Memory { address, size, .. } = root.bar_info(bdf, layout.bir).ok()? else {
            return None;
        };
        if address == 0 || !layout.fits(size as usize) {
            return None;
        }
        let base = axklib::mem::iomap((address as usize).into(), size as usize)
            .ok()?
            .as_usize();
        let table = base.checked_add(layout.offset)?;
        let (message, data, vector) = axhal::irq::allocate_msi(irq_handler)?;
        let control = capability.private_header | 0xc000;
        if !root.write_config_u16(bdf, capability.offset + 2, control) {
            return None;
        }
        // SAFETY: the complete MSI-X table was bounds-checked in a mapped memory BAR.
        // The function mask prevents delivery while all entries and entry zero are set.
        unsafe {
            for entry in 0..layout.vectors {
                ((table + entry * 16 + 12) as *mut u32).write_volatile(1);
            }
            (table as *mut u32).write_volatile(message as u32);
            ((table + 4) as *mut u32).write_volatile((message >> 32) as u32);
            ((table + 8) as *mut u32).write_volatile(data);
            let _ = ((table + 12) as *const u32).read_volatile();
        }
        Some(Self {
            table,
            capability: capability.offset,
            control,
            vector,
        })
    }
    fn enable(&self, root: &mut PciRoot, bdf: DeviceFunction) {
        // SAFETY: entry zero's message and permanent handler are installed; controller
        // initialization completed with every other entry still masked.
        unsafe {
            ((self.table + 12) as *mut u32).write_volatile(0);
        }
        root.write_config_u16(bdf, self.capability + 2, self.control & !0x4000);
        let _ = root.read_config_dword(bdf, self.capability);
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
        Msix::disable(root, bdf);
        let msix = if axhal::boot::command_line_value("nvme.poll") == Some("1") {
            None
        } else {
            Msix::prepare(root, bdf)
        };
        match NvmeDevice::new(
            Window {
                base: base.as_usize(),
                size: size as usize,
                irq: msix.as_ref().map(|route| route.vector),
            },
            allow_write,
            size as usize,
        ) {
            Ok(device) => {
                if let Some(route) = &msix {
                    route.enable(root, bdf);
                }
                log::info!(
                    "nvme: {bdf} {:04x}:{:04x} /dev/nvme0n1 queues={} read_only={} completion={}; \
                     未在硬件上验证",
                    info.vendor_id,
                    info.device_id,
                    device.queue_count(),
                    device.read_only(),
                    if msix.is_some() {
                        "MSI-X+poll-fallback"
                    } else {
                        "polling"
                    }
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
