#[cfg(target_arch = "loongarch64")]
use core::sync::atomic::{AtomicUsize, Ordering};
use core::{marker::PhantomData, ptr, ptr::NonNull};

use axalloc::{UsageKind, global_allocator};
use axdriver_base::{BaseDriverOps, DevResult, DeviceType};
use axdriver_virtio::{BufferDirection, PhysAddr, VirtIoHal};
use axhal::mem::phys_to_virt;
#[cfg(not(target_arch = "loongarch64"))]
use axhal::mem::virt_to_phys;
#[cfg(target_arch = "loongarch64")]
use axhal::paging::MappingFlags;
use cfg_if::cfg_if;
#[cfg(target_arch = "loongarch64")]
use spin::Mutex;

use crate::{AxDeviceEnum, drivers::DriverProbe};

cfg_if! {
    if #[cfg(bus = "pci")] {
        use axdriver_pci::{PciRoot, DeviceFunction, DeviceFunctionInfo};
        type VirtIoTransport = axdriver_virtio::PciTransport;
    } else if #[cfg(bus =  "mmio")] {
        type VirtIoTransport = axdriver_virtio::MmioTransport;
    }
}

#[cfg(all(bus = "mmio", target_arch = "riscv64"))]
fn virtio_mmio_irq(mmio_base: usize) -> Option<usize> {
    const QEMU_VIRTIO_MMIO_BASE: usize = 0x1000_1000;
    const QEMU_VIRTIO_MMIO_STRIDE: usize = 0x1000;
    const QEMU_VIRTIO_MMIO_COUNT: usize = 8;

    let offset = mmio_base.checked_sub(QEMU_VIRTIO_MMIO_BASE)?;
    if offset % QEMU_VIRTIO_MMIO_STRIDE != 0 {
        return None;
    }
    let index = offset / QEMU_VIRTIO_MMIO_STRIDE;
    if index < QEMU_VIRTIO_MMIO_COUNT {
        Some(index + 1)
    } else {
        None
    }
}

#[cfg(all(bus = "mmio", not(target_arch = "riscv64")))]
fn virtio_mmio_irq(_mmio_base: usize) -> Option<usize> {
    None
}

/// A trait for VirtIO device meta information.
pub trait VirtIoDevMeta {
    const DEVICE_TYPE: DeviceType;

    type Device: BaseDriverOps;
    type Driver = VirtIoDriver<Self>;

    fn try_new(transport: VirtIoTransport, irq: Option<usize>) -> DevResult<AxDeviceEnum>;
}

cfg_if! {
    if #[cfg(net_dev = "virtio-net")] {
        pub struct VirtIoNet;

        impl VirtIoDevMeta for VirtIoNet {
            const DEVICE_TYPE: DeviceType = DeviceType::Net;
            type Device = axdriver_virtio::VirtIoNetDev<VirtIoHalImpl, VirtIoTransport, 64>;

            fn try_new(transport: VirtIoTransport, irq: Option<usize>) -> DevResult<AxDeviceEnum> {
                Ok(AxDeviceEnum::from_net(Self::Device::try_new(transport, irq)?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(block_dev = "virtio-blk")] {
        pub struct VirtIoBlk;

        impl VirtIoDevMeta for VirtIoBlk {
            const DEVICE_TYPE: DeviceType = DeviceType::Block;
            type Device = axdriver_virtio::VirtIoBlkDev<VirtIoHalImpl, VirtIoTransport>;

            fn try_new(transport: VirtIoTransport, irq: Option<usize>) -> DevResult<AxDeviceEnum> {
                Ok(AxDeviceEnum::from_block(Self::Device::try_new_with_irq(transport, irq)?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(display_dev = "virtio-gpu")] {
        pub struct VirtIoGpu;

        impl VirtIoDevMeta for VirtIoGpu {
            const DEVICE_TYPE: DeviceType = DeviceType::Display;
            type Device = axdriver_virtio::VirtIoGpuDev<VirtIoHalImpl, VirtIoTransport>;

            fn try_new(transport: VirtIoTransport, _irq: Option<usize>) -> DevResult<AxDeviceEnum> {
                Ok(AxDeviceEnum::from_display(Self::Device::try_new(transport)?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(input_dev = "virtio-input")] {
        pub struct VirtIoInput;

        impl VirtIoDevMeta for VirtIoInput {
            const DEVICE_TYPE: DeviceType = DeviceType::Input;
            type Device = axdriver_virtio::VirtIoInputDev<VirtIoHalImpl, VirtIoTransport>;

            fn try_new(transport: VirtIoTransport, _irq: Option<usize>) -> DevResult<AxDeviceEnum> {
                Ok(AxDeviceEnum::from_input(Self::Device::try_new(transport)?))
            }
        }
    }
}

cfg_if! {
    if #[cfg(vsock_dev = "virtio-socket")] {
        pub struct VirtIoSocket;

        impl VirtIoDevMeta for VirtIoSocket {
            const DEVICE_TYPE: DeviceType = DeviceType::Vsock;
            type Device = axdriver_virtio::VirtIoSocketDev<VirtIoHalImpl, VirtIoTransport>;

            fn try_new(transport: VirtIoTransport, _irq:  Option<usize>) -> DevResult<AxDeviceEnum> {
                Ok(AxDeviceEnum::from_vsock(Self::Device::try_new(transport)?))
            }
        }
    }
}

/// A common driver for all VirtIO devices that implements [`DriverProbe`].
pub struct VirtIoDriver<D: VirtIoDevMeta + ?Sized>(PhantomData<D>);

impl<D: VirtIoDevMeta> DriverProbe for VirtIoDriver<D> {
    #[cfg(bus = "mmio")]
    fn probe_mmio(mmio_base: usize, mmio_size: usize) -> Option<AxDeviceEnum> {
        let base_vaddr = phys_to_virt(mmio_base.into());
        if let Some((ty, transport)) =
            axdriver_virtio::probe_mmio_device(base_vaddr.as_mut_ptr(), mmio_size)
            && ty == D::DEVICE_TYPE
        {
            match D::try_new(transport, virtio_mmio_irq(mmio_base)) {
                Ok(dev) => return Some(dev),
                Err(e) => {
                    warn!(
                        "failed to initialize MMIO device at [PA:{:#x}, PA:{:#x}): {:?}",
                        mmio_base,
                        mmio_base + mmio_size,
                        e
                    );
                    return None;
                }
            }
        }
        None
    }

    #[cfg(bus = "pci")]
    fn probe_pci(
        root: &mut PciRoot,
        bdf: DeviceFunction,
        dev_info: &DeviceFunctionInfo,
    ) -> Option<AxDeviceEnum> {
        #[cfg(target_arch = "loongarch64")]
        if D::DEVICE_TYPE == DeviceType::Net {
            // Keep LA/QEMU on the in-kernel loopback path. The external
            // virtio-net device is not required by the official pre-2025
            // scripts, which exercise netperf/iperf against 127.0.0.1.
            return None;
        }

        if dev_info.vendor_id != 0x1af4 {
            return None;
        }
        match (D::DEVICE_TYPE, dev_info.device_id) {
            (DeviceType::Net, 0x1000) | (DeviceType::Net, 0x1041) => {}
            (DeviceType::Block, 0x1001) | (DeviceType::Block, 0x1042) => {}
            (DeviceType::Input, 0x1052) => {}
            (DeviceType::Display, 0x1050) => {}
            (DeviceType::Vsock, 0x1053) => {}
            _ => return None,
        }

        if let Some((ty, transport, irq)) =
            axdriver_virtio::probe_pci_device::<VirtIoHalImpl>(root, bdf, dev_info)
            && ty == D::DEVICE_TYPE
        {
            match D::try_new(transport, Some(irq)) {
                Ok(dev) => return Some(dev),
                Err(e) => {
                    warn!("failed to initialize PCI device at {bdf}({dev_info}): {e:?}");
                    return None;
                }
            }
        }
        None
    }
}

pub struct VirtIoHalImpl;

#[cfg(target_arch = "loongarch64")]
#[inline]
fn dma_sync_barrier() {
    unsafe {
        core::arch::asm!("dbar 0", options(nostack, preserves_flags));
    }
}

#[cfg(target_arch = "loongarch64")]
const DMA_PAGE_SIZE: usize = 0x1000;

#[cfg(target_arch = "loongarch64")]
const DMA_REGION_SLOTS: usize = 8;

#[cfg(target_arch = "loongarch64")]
const QUEUE_DMA_POOL_SLOTS: usize = 32;

#[cfg(target_arch = "loongarch64")]
const DMA_POOL_PADDR: usize = 0x0e00_0000;

#[cfg(target_arch = "loongarch64")]
const DMA_POOL_SIZE: usize = 0x0200_0000;

#[cfg(target_arch = "loongarch64")]
const DMA_POOL_PAGES: usize = DMA_POOL_SIZE / DMA_PAGE_SIZE;

#[cfg(target_arch = "loongarch64")]
const DMA_POOL_BITMAP_WORDS: usize =
    (DMA_POOL_PAGES + usize::BITS as usize - 1) / usize::BITS as usize;

#[cfg(target_arch = "loongarch64")]
static DMA_REGION_BASES: [AtomicUsize; DMA_REGION_SLOTS] =
    [const { AtomicUsize::new(0) }; DMA_REGION_SLOTS];

#[cfg(target_arch = "loongarch64")]
static DMA_REGION_PAGES: [AtomicUsize; DMA_REGION_SLOTS] =
    [const { AtomicUsize::new(0) }; DMA_REGION_SLOTS];

#[cfg(target_arch = "loongarch64")]
struct DmaPoolState {
    base_vaddr: usize,
    bitmap: [usize; DMA_POOL_BITMAP_WORDS],
}

#[cfg(target_arch = "loongarch64")]
impl DmaPoolState {
    const fn new() -> Self {
        Self {
            base_vaddr: 0,
            bitmap: [0; DMA_POOL_BITMAP_WORDS],
        }
    }

    fn init_if_needed(&mut self) {
        if self.base_vaddr != 0 {
            return;
        }
        self.base_vaddr = phys_to_virt(DMA_POOL_PADDR.into()).as_usize();
        update_dma_mapping(self.base_vaddr, DMA_POOL_PAGES, dma_mapping_flags());
        dma_sync_barrier();
        info!(
            "LA virtio DMA pool: [PA:{:#x}, PA:{:#x}) -> [VA:{:#x}, VA:{:#x})",
            DMA_POOL_PADDR,
            DMA_POOL_PADDR + DMA_POOL_SIZE,
            self.base_vaddr,
            self.base_vaddr + DMA_POOL_SIZE,
        );
    }

    fn contains_paddr(&self, paddr: usize, pages: usize) -> bool {
        let size = pages * DMA_PAGE_SIZE;
        paddr >= DMA_POOL_PADDR
            && paddr
                .checked_add(size)
                .is_some_and(|end| end <= DMA_POOL_PADDR + DMA_POOL_SIZE)
    }

    fn vaddr_of(&self, paddr: usize) -> usize {
        self.base_vaddr + (paddr - DMA_POOL_PADDR)
    }

    fn mark_range(&mut self, start_page: usize, pages: usize, used: bool) {
        for page in start_page..start_page + pages {
            let word = page / usize::BITS as usize;
            let bit = page % usize::BITS as usize;
            if used {
                self.bitmap[word] |= 1usize << bit;
            } else {
                self.bitmap[word] &= !(1usize << bit);
            }
        }
    }

    fn range_is_free(&self, start_page: usize, pages: usize) -> bool {
        (start_page..start_page + pages).all(|page| {
            let word = page / usize::BITS as usize;
            let bit = page % usize::BITS as usize;
            self.bitmap[word] & (1usize << bit) == 0
        })
    }

    fn alloc(&mut self, pages: usize) -> Option<(usize, usize)> {
        if pages == 0 || pages > DMA_POOL_PAGES {
            return None;
        }
        self.init_if_needed();
        for start_page in 0..=DMA_POOL_PAGES - pages {
            if !self.range_is_free(start_page, pages) {
                continue;
            }
            self.mark_range(start_page, pages, true);
            let paddr = DMA_POOL_PADDR + start_page * DMA_PAGE_SIZE;
            let vaddr = self.base_vaddr + start_page * DMA_PAGE_SIZE;
            unsafe {
                ptr::write_bytes(vaddr as *mut u8, 0, pages * DMA_PAGE_SIZE);
            }
            dma_sync_barrier();
            return Some((paddr, vaddr));
        }
        None
    }

    fn free(&mut self, paddr: usize, pages: usize) -> bool {
        if pages == 0 || !self.contains_paddr(paddr, pages) {
            return false;
        }
        let start_page = (paddr - DMA_POOL_PADDR) / DMA_PAGE_SIZE;
        for page in start_page..start_page + pages {
            let word = page / usize::BITS as usize;
            let bit = page % usize::BITS as usize;
            assert_ne!(
                self.bitmap[word] & (1usize << bit),
                0,
                "LA virtio DMA pool double-free: paddr={:#x} page={}",
                paddr,
                page,
            );
        }
        self.mark_range(start_page, pages, false);
        let vaddr = self.vaddr_of(paddr);
        unsafe {
            ptr::write_bytes(vaddr as *mut u8, 0, pages * DMA_PAGE_SIZE);
        }
        dma_sync_barrier();
        true
    }
}

#[cfg(target_arch = "loongarch64")]
#[derive(Clone, Copy)]
struct QueueDmaSlot {
    vaddr: usize,
    paddr: usize,
    in_use: bool,
}

#[cfg(target_arch = "loongarch64")]
impl QueueDmaSlot {
    const EMPTY: Self = Self {
        vaddr: 0,
        paddr: 0,
        in_use: false,
    };
}

#[cfg(target_arch = "loongarch64")]
static QUEUE_DMA_POOL: Mutex<[QueueDmaSlot; QUEUE_DMA_POOL_SLOTS]> =
    Mutex::new([QueueDmaSlot::EMPTY; QUEUE_DMA_POOL_SLOTS]);

#[cfg(target_arch = "loongarch64")]
static DMA_POOL_STATE: Mutex<DmaPoolState> = Mutex::new(DmaPoolState::new());

#[cfg(target_arch = "loongarch64")]
fn dma_mapping_flags() -> MappingFlags {
    MappingFlags::READ | MappingFlags::WRITE | MappingFlags::UNCACHED
}

#[cfg(target_arch = "loongarch64")]
fn update_dma_mapping(vaddr: usize, pages: usize, flags: MappingFlags) {
    if pages == 0 {
        return;
    }
    let size = pages * DMA_PAGE_SIZE;
    axmm::kernel_aspace()
        .lock()
        .protect(vaddr.into(), size, flags)
        .unwrap_or_else(|err| {
            panic!(
                "failed to update LA virtio DMA mapping @ {:#x} size={:#x}: {:?}",
                vaddr, size, err
            )
        });
    dma_sync_barrier();
}

#[cfg(target_arch = "loongarch64")]
fn register_dma_region(paddr: usize, pages: usize) {
    for (base, npages) in DMA_REGION_BASES.iter().zip(DMA_REGION_PAGES.iter()) {
        if base
            .compare_exchange(0, paddr, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            npages.store(pages, Ordering::SeqCst);
            return;
        }
    }
}

#[cfg(target_arch = "loongarch64")]
fn unregister_dma_region(paddr: usize) {
    for (base, npages) in DMA_REGION_BASES.iter().zip(DMA_REGION_PAGES.iter()) {
        if base.load(Ordering::SeqCst) == paddr {
            npages.store(0, Ordering::SeqCst);
            base.store(0, Ordering::SeqCst);
            return;
        }
    }
}

#[cfg(target_arch = "loongarch64")]
fn overlaps_registered_dma(paddr: usize, pages: usize) -> bool {
    let start = paddr;
    let end = paddr + pages * DMA_PAGE_SIZE;
    DMA_REGION_BASES
        .iter()
        .zip(DMA_REGION_PAGES.iter())
        .any(|(base, npages)| {
            let base = base.load(Ordering::SeqCst);
            let npages = npages.load(Ordering::SeqCst);
            if base == 0 || npages == 0 {
                return false;
            }
            let region_end = base + npages * DMA_PAGE_SIZE;
            start < region_end && base < end
        })
}

#[cfg(target_arch = "loongarch64")]
fn alloc_dma_region(pages: usize) -> Option<(usize, usize)> {
    let mut pool = DMA_POOL_STATE.lock();
    pool.alloc(pages)
}

#[cfg(target_arch = "loongarch64")]
fn free_dma_region(paddr: usize, pages: usize) -> bool {
    let mut pool = DMA_POOL_STATE.lock();
    pool.free(paddr, pages)
}

#[cfg(target_arch = "loongarch64")]
fn dma_pool_vaddr(paddr: usize, pages: usize) -> Option<usize> {
    let mut pool = DMA_POOL_STATE.lock();
    pool.init_if_needed();
    pool.contains_paddr(paddr, pages)
        .then(|| pool.vaddr_of(paddr))
}

#[cfg(target_arch = "loongarch64")]
fn alloc_queue_dma_page(direction: BufferDirection) -> Option<(usize, usize)> {
    let mut pool = QUEUE_DMA_POOL.lock();
    for slot in pool.iter_mut() {
        if slot.in_use {
            continue;
        }

        if slot.vaddr == 0 {
            let (paddr, vaddr) = alloc_dma_region(1)?;
            slot.vaddr = vaddr;
            slot.paddr = paddr;
        }

        unsafe {
            ptr::write_bytes(slot.vaddr as *mut u8, 0, DMA_PAGE_SIZE);
        }
        dma_sync_barrier();
        register_dma_region(slot.paddr, 1);
        slot.in_use = true;

        if log::log_enabled!(log::Level::Debug) {
            log::debug!(
                "LA virtio queue dma_alloc dir={:?} paddr={:#x} vaddr={:#x}",
                direction,
                slot.paddr,
                slot.vaddr
            );
        }
        return Some((slot.paddr, slot.vaddr));
    }
    None
}

#[cfg(target_arch = "loongarch64")]
fn free_queue_dma_page(paddr: usize, vaddr: usize) -> bool {
    let mut pool = QUEUE_DMA_POOL.lock();
    for slot in pool.iter_mut() {
        if slot.vaddr != vaddr {
            continue;
        }
        assert_eq!(
            slot.paddr, paddr,
            "LA virtio queue dma slot paddr mismatch: expect {:#x}, got {:#x}",
            slot.paddr, paddr
        );
        assert!(
            slot.in_use,
            "LA virtio queue dma slot double-free: paddr={:#x} vaddr={:#x}",
            paddr, vaddr
        );
        unregister_dma_region(paddr);
        unsafe {
            ptr::write_bytes(vaddr as *mut u8, 0, DMA_PAGE_SIZE);
        }
        dma_sync_barrier();
        slot.in_use = false;
        if log::log_enabled!(log::Level::Debug) {
            log::debug!(
                "LA virtio queue dma_dealloc paddr={:#x} vaddr={:#x}",
                paddr,
                vaddr
            );
        }
        return true;
    }
    false
}

#[cfg(target_arch = "loongarch64")]
fn use_queue_dma_pool(pages: usize) -> bool {
    pages == 1
}

unsafe impl VirtIoHal for VirtIoHalImpl {
    fn dma_alloc(pages: usize, direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        #[cfg(not(target_arch = "loongarch64"))]
        let _ = direction;

        #[cfg(target_arch = "loongarch64")]
        let (paddr, vaddr) = if use_queue_dma_pool(pages) {
            if let Some((paddr, vaddr)) = alloc_queue_dma_page(direction) {
                (paddr, vaddr)
            } else {
                return (0, NonNull::dangling());
            }
        } else {
            let Some((paddr, vaddr)) = alloc_dma_region(pages) else {
                return (0, NonNull::dangling());
            };
            register_dma_region(paddr, pages);
            (paddr, vaddr)
        };

        #[cfg(not(target_arch = "loongarch64"))]
        let (paddr, vaddr) = {
            let vaddr =
                if let Ok(vaddr) = global_allocator().alloc_pages(pages, 0x1000, UsageKind::Dma) {
                    vaddr
                } else {
                    return (0, NonNull::dangling());
                };
            let paddr = virt_to_phys(vaddr.into()).as_usize();
            (paddr, vaddr)
        };

        unsafe {
            ptr::write_bytes(vaddr as *mut u8, 0, pages * 0x1000);
        }
        #[cfg(target_arch = "loongarch64")]
        dma_sync_barrier();
        #[cfg(target_arch = "loongarch64")]
        if log::log_enabled!(log::Level::Debug) {
            log::debug!(
                "LA virtio dma_alloc dir={:?} paddr={:#x} vaddr={:#x} pages={}",
                direction,
                paddr,
                vaddr,
                pages
            );
        }
        let ptr = NonNull::new(vaddr as _).unwrap();
        (paddr, ptr)
    }

    unsafe fn dma_dealloc(_paddr: PhysAddr, vaddr: NonNull<u8>, pages: usize) -> i32 {
        #[cfg(target_arch = "loongarch64")]
        if use_queue_dma_pool(pages) && free_queue_dma_page(_paddr, vaddr.as_ptr() as usize) {
            return 0;
        }
        #[cfg(target_arch = "loongarch64")]
        if log::log_enabled!(log::Level::Debug) {
            log::debug!(
                "LA virtio dma_dealloc paddr={:#x} vaddr={:#x} pages={}",
                _paddr,
                vaddr.as_ptr() as usize,
                pages
            );
        }
        #[cfg(target_arch = "loongarch64")]
        unregister_dma_region(_paddr);
        #[cfg(target_arch = "loongarch64")]
        if free_dma_region(_paddr, pages) {
            return 0;
        }
        global_allocator().dealloc_pages(vaddr.as_ptr() as usize, pages, UsageKind::Dma);
        0
    }

    #[inline]
    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        NonNull::new(phys_to_virt(paddr.into()).as_mut_ptr()).unwrap()
    }

    #[inline]
    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> PhysAddr {
        #[cfg(not(target_arch = "loongarch64"))]
        let _ = direction;

        #[cfg(target_arch = "loongarch64")]
        {
            assert_ne!(buffer.len(), 0);
            // Keep LA virtio DMA fully bounce-buffered. Re-deriving whether a
            // request used direct-mapped memory during `unshare` is not stable
            // once user/kernel mappings have been cloned or otherwise
            // rearranged, and a false "bounce" decision can corrupt the page
            // allocator by freeing the original backing page.
            let pages = buffer.len().div_ceil(DMA_PAGE_SIZE);
            let Some((paddr, vaddr)) = alloc_dma_region(pages) else {
                return 0;
            };
            if matches!(
                direction,
                BufferDirection::DriverToDevice | BufferDirection::Both
            ) {
                // SAFETY: caller guarantees the source buffer is valid for the
                // duration of this call; the shared buffer is newly allocated
                // and non-overlapping.
                unsafe {
                    ptr::copy_nonoverlapping(
                        buffer.as_ptr().cast::<u8>(),
                        vaddr as *mut u8,
                        buffer.len(),
                    );
                }
            }
            dma_sync_barrier();
            if overlaps_registered_dma(paddr, pages) {
                panic!(
                    "LA virtio share reused active DMA region paddr={:#x} pages={}",
                    paddr, pages
                );
            }
            return paddr;
        }

        #[cfg(not(target_arch = "loongarch64"))]
        {
            let vaddr = buffer.as_ptr() as *mut u8 as usize;
            virt_to_phys(vaddr.into()).into()
        }
    }

    #[inline]
    unsafe fn unshare(paddr: PhysAddr, buffer: NonNull<[u8]>, direction: BufferDirection) {
        #[cfg(target_arch = "loongarch64")]
        {
            assert_ne!(buffer.len(), 0);
            assert_ne!(paddr, 0);
            let pages = buffer.len().div_ceil(DMA_PAGE_SIZE);
            if overlaps_registered_dma(paddr, pages) {
                panic!(
                    "LA virtio unshare hit active DMA region paddr={:#x} pages={}",
                    paddr, pages
                );
            }
            let vaddr = dma_pool_vaddr(paddr, pages)
                .unwrap_or_else(|| phys_to_virt(paddr.into()).as_usize());
            dma_sync_barrier();
            if matches!(
                direction,
                BufferDirection::DeviceToDriver | BufferDirection::Both
            ) {
                // SAFETY: caller guarantees the destination buffer is valid for
                // the duration of this call; the shared buffer does not alias
                // `buffer`.
                unsafe {
                    ptr::copy_nonoverlapping(
                        vaddr as *const u8,
                        buffer.as_ptr().cast::<u8>(),
                        buffer.len(),
                    );
                }
            }
            if !free_dma_region(paddr, pages) {
                global_allocator().dealloc_pages(vaddr, pages, UsageKind::Dma);
            }
            return;
        }

        #[cfg(not(target_arch = "loongarch64"))]
        let _ = (paddr, buffer, direction);
    }
}
