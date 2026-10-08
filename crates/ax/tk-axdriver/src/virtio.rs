use core::{marker::PhantomData, ptr, ptr::NonNull};

use axalloc::{UsageKind, global_allocator};
use axdriver_base::{BaseDriverOps, DevResult, DeviceType};
#[cfg(feature = "display")]
use axdriver_display::DisplayDriverOps;
use axdriver_virtio::{
    BufferDirection, DmaMapping, DmaRequester, PhysAddr, VirtIoError, VirtIoHal, VirtIoResult,
};
use axhal::mem::{phys_to_virt, virt_to_phys};
use cfg_if::cfg_if;
use spin::Mutex;

use crate::{
    AxDeviceEnum,
    drivers::{BusProbeResult, DriverProbe},
};

cfg_if! {
    if #[cfg(bus = "pci")] {
        use axdriver_pci::{PciRoot, DeviceFunction, DeviceFunctionInfo};
        type VirtIoTransport = axdriver_virtio::PciTransport;
    } else if #[cfg(bus =  "mmio")] {
        type VirtIoTransport = axdriver_virtio::MmioTransport;
    }
}

#[cfg(feature = "virtio-rng")]
type VirtIoEntropyDevice = axdriver_virtio::VirtIOEntropy<VirtIoHalImpl, VirtIoTransport>;

#[cfg(feature = "virtio-rng")]
static ENTROPY_DEVICE: Mutex<Option<VirtIoEntropyDevice>> = Mutex::new(None);

/// Returns whether a hardware-backed entropy source was initialized.
#[cfg(feature = "virtio-rng")]
pub fn entropy_source_ready() -> bool {
    ENTROPY_DEVICE.lock().is_some()
}

/// Fills `buf` from the initialized hardware entropy source.
#[cfg(feature = "virtio-rng")]
pub fn fill_entropy(buf: &mut [u8]) -> DevResult {
    let mut slot = ENTROPY_DEVICE.lock();
    let device = slot.as_mut().ok_or(axdriver_base::DevError::Unsupported)?;
    device
        .fill_bytes(buf)
        .map_err(|_| axdriver_base::DevError::Io)
}

/// Side-effect-only probe for a VirtIO entropy source.
#[cfg(feature = "virtio-rng")]
pub struct VirtIoEntropyDriver;

#[cfg(feature = "virtio-rng")]
impl VirtIoEntropyDriver {
    fn install(transport: VirtIoTransport) {
        let mut slot = ENTROPY_DEVICE.lock();
        if slot.is_some() {
            return;
        }
        match VirtIoEntropyDevice::new(transport) {
            Ok(device) => {
                *slot = Some(device);
                info!("registered VirtIO entropy source");
            }
            Err(error) => debug!("failed to initialize VirtIO entropy source: {error:?}"),
        }
    }
}

#[cfg(feature = "virtio-rng")]
impl DriverProbe for VirtIoEntropyDriver {
    #[cfg(bus = "mmio")]
    fn probe_mmio(mmio_base: usize, mmio_size: usize) -> BusProbeResult {
        let base_vaddr = phys_to_virt(mmio_base.into());
        if let Some(transport) =
            axdriver_virtio::probe_mmio_entropy_device(base_vaddr.as_mut_ptr(), mmio_size)
        {
            Self::install(transport);
            BusProbeResult::Claimed
        } else {
            BusProbeResult::NotMatched
        }
    }

    #[cfg(bus = "pci")]
    fn probe_pci(
        root: &mut PciRoot,
        bdf: DeviceFunction,
        dev_info: &DeviceFunctionInfo,
    ) -> BusProbeResult {
        if dev_info.vendor_id == 0x1af4
            && let Some(transport) =
                axdriver_virtio::probe_pci_entropy_device::<VirtIoHalImpl>(root, bdf, dev_info)
        {
            Self::install(transport);
            BusProbeResult::Claimed
        } else {
            BusProbeResult::NotMatched
        }
    }
}

/// A trait for VirtIO device meta information.
pub trait VirtIoDevMeta {
    const DEVICE_TYPE: DeviceType;

    type Device: BaseDriverOps;
    type Driver = VirtIoDriver<Self>;

    fn try_new(transport: VirtIoTransport, irq: Option<usize>) -> DevResult<AxDeviceEnum>;
}

cfg_if! {
    if #[cfg(any(net_dev = "virtio-net", net_dev = "n305-net"))] {
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

            fn try_new(transport: VirtIoTransport, irq: Option<usize>) -> DevResult<AxDeviceEnum> {
                Ok(AxDeviceEnum::from_input(Self::Device::try_new(transport, irq)?))
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
    fn probe_mmio(mmio_base: usize, mmio_size: usize) -> BusProbeResult {
        let base_vaddr = phys_to_virt(mmio_base.into());
        if let Some((ty, transport)) =
            axdriver_virtio::probe_mmio_device(base_vaddr.as_mut_ptr(), mmio_size)
            && ty == D::DEVICE_TYPE
        {
            match D::try_new(transport, None) {
                Ok(dev) => return BusProbeResult::Device(dev),
                Err(e) => {
                    warn!(
                        "failed to initialize MMIO device at [PA:{:#x}, PA:{:#x}): {:?}",
                        mmio_base,
                        mmio_base + mmio_size,
                        e
                    );
                    return BusProbeResult::Claimed;
                }
            }
        }
        BusProbeResult::NotMatched
    }

    #[cfg(bus = "pci")]
    fn probe_pci(
        root: &mut PciRoot,
        bdf: DeviceFunction,
        dev_info: &DeviceFunctionInfo,
    ) -> BusProbeResult {
        if dev_info.vendor_id != 0x1af4 {
            return BusProbeResult::NotMatched;
        }
        match (D::DEVICE_TYPE, dev_info.device_id) {
            (DeviceType::Net, 0x1000) | (DeviceType::Net, 0x1041) => {}
            (DeviceType::Block, 0x1001) | (DeviceType::Block, 0x1042) => {}
            (DeviceType::Input, 0x1052) => {}
            (DeviceType::Display, 0x1050) => {}
            (DeviceType::Vsock, 0x1053) => {}
            _ => return BusProbeResult::NotMatched,
        }

        if let Some((ty, transport, irq)) =
            axdriver_virtio::probe_pci_device::<VirtIoHalImpl>(root, bdf, dev_info)
            && ty == D::DEVICE_TYPE
        {
            match D::try_new(transport, irq) {
                Ok(mut dev) => {
                    #[cfg(feature = "display")]
                    if let AxDeviceEnum::Display(display) = &mut dev {
                        let (subsystem_vendor, subsystem_device) = root.endpoint_subsystem_ids(bdf);
                        display.set_pci_identity(axdriver_display::DisplayPciIdentity {
                            bus: bdf.bus,
                            device: bdf.device,
                            function: bdf.function,
                            vendor_id: dev_info.vendor_id,
                            device_id: dev_info.device_id,
                            subsystem_vendor,
                            subsystem_device,
                            revision: dev_info.revision,
                        });
                    }
                    return BusProbeResult::Device(dev);
                }
                Err(e) => {
                    warn!("failed to initialize PCI device at {bdf}({dev_info}): {e:?}");
                    return BusProbeResult::Claimed;
                }
            }
        }
        BusProbeResult::NotMatched
    }
}

pub struct VirtIoHalImpl;

fn requester_id(requester: Option<DmaRequester>) -> Option<tk_vtd::PciRequester> {
    requester.map(|requester| tk_vtd::PciRequester {
        segment: requester.segment,
        bus: requester.bus,
        device: requester.device,
        function: requester.function,
    })
}

fn platform_map_for(requester: Option<DmaRequester>, physical: u64, length: usize) -> Result<u64, tk_vtd::Error> {
    let result = match requester_id(requester) {
        Some(requester) => tk_vtd::platform_map_for(requester, physical, length),
        None => tk_vtd::platform_map(physical, length),
    };
    if let Err(error) = result {
        log::error!("virtio: DMA map failed requester={requester:?} physical={physical:#x} length={length:#x}: {error:?}");
    }
    result
}

fn platform_unmap_for(requester: Option<DmaRequester>, address: u64, length: usize) -> Result<(), tk_vtd::Error> {
    match requester_id(requester) {
        Some(requester) => tk_vtd::platform_unmap_for(requester, address, length),
        None => tk_vtd::platform_unmap(address, length),
    }
}

unsafe impl VirtIoHal for VirtIoHalImpl {
    fn dma_alloc(pages: usize, direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        Self::dma_alloc_for(None, pages, direction)
    }
    fn dma_alloc_for(requester: Option<DmaRequester>, pages: usize, direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        let _ = direction;
        let (paddr, vaddr) = {
            let vaddr =
                if let Ok(vaddr) = global_allocator().alloc_pages(pages, 0x1000, UsageKind::Dma) {
                    vaddr
                } else {
                    return (0, NonNull::dangling());
                };
            let paddr = virt_to_phys(vaddr.into()).as_usize();
            let length = pages.saturating_mul(0x1000);
            let Ok(device_address) = platform_map_for(requester, paddr as u64, length) else {
                global_allocator().dealloc_pages(vaddr, pages, UsageKind::Dma);
                return (0, NonNull::dangling());
            };
            (device_address as usize, vaddr)
        };

        unsafe {
            ptr::write_bytes(vaddr as *mut u8, 0, pages * 0x1000);
        }
        let ptr = NonNull::new(vaddr as _).unwrap();
        (paddr, ptr)
    }

    unsafe fn dma_dealloc(paddr: PhysAddr, vaddr: NonNull<u8>, pages: usize) -> i32 {
        unsafe { Self::dma_dealloc_for(None, paddr, vaddr, pages) }
    }

    unsafe fn dma_dealloc_for(requester: Option<DmaRequester>, paddr: PhysAddr, vaddr: NonNull<u8>, pages: usize) -> i32 {
        if platform_unmap_for(requester, paddr as u64, pages.saturating_mul(0x1000)).is_err() {
            return -1;
        }
        global_allocator().dealloc_pages(vaddr.as_ptr() as usize, pages, UsageKind::Dma);
        0
    }

    unsafe fn map_physical(
        paddr: PhysAddr,
        len: usize,
        _direction: BufferDirection,
    ) -> VirtIoResult<DmaMapping> {
        unsafe { Self::map_physical_for(None, paddr, len, _direction) }
    }

    unsafe fn map_physical_for(
        requester: Option<DmaRequester>,
        paddr: PhysAddr,
        len: usize,
        _direction: BufferDirection,
    ) -> VirtIoResult<DmaMapping> {
        if paddr == 0 || len == 0 || paddr.checked_add(len).is_none() {
            return Err(VirtIoError::DmaError);
        }
        let device =
            platform_map_for(requester, paddr as u64, len).map_err(|_| VirtIoError::DmaError)? as usize;
        Ok(DmaMapping {
            source: paddr,
            device,
            len,
        })
    }

    unsafe fn unmap_physical(mapping: DmaMapping, _direction: BufferDirection) {
        unsafe { Self::unmap_physical_for(None, mapping, _direction) }
    }

    unsafe fn unmap_physical_for(requester: Option<DmaRequester>, mapping: DmaMapping, _direction: BufferDirection) {
        if platform_unmap_for(requester, mapping.device as u64, mapping.len).is_err() {
            panic!(
                "virtio: failed to invalidate DMA mapping {:#x}+{:#x}; backing memory remains \
                 owned",
                mapping.device, mapping.len
            );
        }
    }

    #[inline]
    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        NonNull::new(phys_to_virt(paddr.into()).as_mut_ptr()).unwrap()
    }

    #[inline]
    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> PhysAddr {
        unsafe { Self::share_for(None, buffer, direction) }
    }

    unsafe fn share_for(requester: Option<DmaRequester>, buffer: NonNull<[u8]>, direction: BufferDirection) -> PhysAddr {
        let _ = direction;
        let vaddr = buffer.as_ptr() as *mut u8 as usize;
        let physical = virt_to_phys(vaddr.into()).as_usize();
        let length = unsafe { buffer.as_ref().len() };
        platform_map_for(requester, physical as u64, length).map_or(0, |address| address as usize)
    }

    #[inline]
    unsafe fn unshare(paddr: PhysAddr, buffer: NonNull<[u8]>, direction: BufferDirection) {
        unsafe { Self::unshare_for(None, paddr, buffer, direction) }
    }

    unsafe fn unshare_for(requester: Option<DmaRequester>, paddr: PhysAddr, buffer: NonNull<[u8]>, direction: BufferDirection) {
        let _ = direction;
        let length = unsafe { buffer.as_ref().len() };
        if let Err(error) = platform_unmap_for(requester, paddr as u64, length) {
            panic!("virtio: failed to retire shared DMA mapping requester={requester:?} {paddr:#x}+{length:#x}: {error:?}");
        }
    }
}
