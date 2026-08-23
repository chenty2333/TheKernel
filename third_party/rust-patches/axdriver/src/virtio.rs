use core::{marker::PhantomData, ptr, ptr::NonNull};

use axalloc::{UsageKind, global_allocator};
use axdriver_base::{BaseDriverOps, DevResult, DeviceType};
use axdriver_virtio::{
    BufferDirection, DmaMapping, PhysAddr, VirtIoError, VirtIoHal, VirtIoResult,
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
            Err(error) => warn!("failed to initialize VirtIO entropy source: {error:?}"),
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
                Ok(dev) => return BusProbeResult::Device(dev),
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

unsafe impl VirtIoHal for VirtIoHalImpl {
    fn dma_alloc(pages: usize, direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        let _ = direction;
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
        let ptr = NonNull::new(vaddr as _).unwrap();
        (paddr, ptr)
    }

    unsafe fn dma_dealloc(_paddr: PhysAddr, vaddr: NonNull<u8>, pages: usize) -> i32 {
        global_allocator().dealloc_pages(vaddr.as_ptr() as usize, pages, UsageKind::Dma);
        0
    }

    unsafe fn map_physical(
        paddr: PhysAddr,
        len: usize,
        _direction: BufferDirection,
    ) -> VirtIoResult<DmaMapping> {
        if paddr == 0 || len == 0 || paddr.checked_add(len).is_none() {
            return Err(VirtIoError::DmaError);
        }
        // q35 currently exposes coherent identity DMA. Keep this explicit in
        // the mapping API so an IOMMU-capable HAL can return a distinct device
        // address without changing the block descriptor path.
        Ok(DmaMapping::identity(paddr, len))
    }

    unsafe fn unmap_physical(_mapping: DmaMapping, _direction: BufferDirection) {}

    #[inline]
    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        NonNull::new(phys_to_virt(paddr.into()).as_mut_ptr()).unwrap()
    }

    #[inline]
    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> PhysAddr {
        let _ = direction;
        let vaddr = buffer.as_ptr() as *mut u8 as usize;
        virt_to_phys(vaddr.into()).into()
    }

    #[inline]
    unsafe fn unshare(paddr: PhysAddr, buffer: NonNull<[u8]>, direction: BufferDirection) {
        let _ = (paddr, buffer, direction);
    }
}
