//! Wrappers of some devices in the [`virtio-drivers`][1] crate, that implement
//! traits in the [`axdriver_base`][2] series crates.
//!
//! Like the [`virtio-drivers`][1] crate, you must implement the [`VirtIoHal`]
//! trait (alias of [`virtio-drivers::Hal`][3]), to allocate DMA regions and
//! translate between physical addresses (as seen by devices) and virtual
//! addresses (as seen by your program).
//!
//! [1]: https://docs.rs/virtio-drivers/latest/virtio_drivers/
//! [2]: https://github.com/arceos-org/axdriver_crates/tree/main/axdriver_base
//! [3]: https://docs.rs/virtio-drivers/latest/virtio_drivers/trait.Hal.html

#![no_std]
#![cfg_attr(doc, feature(doc_cfg))]

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "block")]
mod blk;
#[cfg(feature = "block")]
pub use self::blk::{VirtIoBlkDev, dispatch_irq};

#[cfg(feature = "gpu")]
mod gpu;
#[cfg(feature = "gpu")]
pub use self::gpu::VirtIoGpuDev;

#[cfg(feature = "input")]
mod input;
#[cfg(feature = "input")]
pub use self::input::VirtIoInputDev;

#[cfg(feature = "net")]
mod net;
#[cfg(feature = "net")]
pub use self::net::VirtIoNetDev;

#[cfg(feature = "socket")]
mod socket;
use axdriver_base::{DevError, DeviceType};
use virtio_drivers::transport::DeviceType as VirtIoDevType;
pub use virtio_drivers::{
    BufferDirection, DmaMapping, Error as VirtIoError, Hal as VirtIoHal, PhysAddr,
    Result as VirtIoResult,
    device::entropy::VirtIOEntropy,
    stats::{
        AsyncBlockWaitPolicy, VirtioIoCounters, async_block_enabled as virtio_async_block_enabled,
        async_block_wait_policy as virtio_async_block_wait_policy,
        io_counters_snapshot as virtio_io_counters_snapshot,
        reset_async_block_adaptive_depth as reset_virtio_async_block_adaptive_depth,
        reset_io_counters as reset_virtio_io_counters,
        set_async_block_adaptive_enabled as set_virtio_async_block_adaptive_enabled,
        set_async_block_depth as set_virtio_async_block_depth,
        set_async_block_enabled as set_virtio_async_block_enabled,
        set_async_block_merge_write_enabled as set_virtio_async_block_merge_write_enabled,
        set_async_block_wait_policy as set_virtio_async_block_wait_policy,
        set_io_counters_enabled as set_virtio_io_counters_enabled,
    },
    transport::{
        Transport,
        mmio::MmioTransport,
        pci::{PciTransport, bus as pci},
    },
};

use self::pci::{DeviceFunction, DeviceFunctionInfo, PciRoot};
#[cfg(feature = "socket")]
pub use self::socket::VirtIoSocketDev;

/// x86 external-interrupt vectors start after the architectural exception
/// range.  PCI configuration space reports the legacy INTx line/GSI, while
/// the x86 platform IRQ API consumes the corresponding CPU vector.
#[cfg(target_arch = "x86_64")]
const PCI_IRQ_VECTOR_BASE: usize = 0x20;

/// Translate a firmware PCI INTx line/GSI into the x86 vector accepted by
/// `axhal::irq`.  Keep the LAPIC-reserved vector range unavailable to PCI
/// devices; such a route is not usable by the current platform contract.
#[cfg(target_arch = "x86_64")]
const fn pci_irq_vector_from_line(line: u8) -> Option<usize> {
    let vector = PCI_IRQ_VECTOR_BASE + line as usize;
    if vector < 0xf0 { Some(vector) } else { None }
}

/// Try to probe a VirtIO MMIO device from the given memory region.
///
/// If the device is recognized, returns the device type and a transport object
/// for later operations. Otherwise, returns [`None`].
pub fn probe_mmio_device(
    reg_base: *mut u8,
    _reg_size: usize,
) -> Option<(DeviceType, MmioTransport)> {
    use core::ptr::NonNull;

    use virtio_drivers::transport::mmio::VirtIOHeader;

    let header = NonNull::new(reg_base as *mut VirtIOHeader).unwrap();
    let transport = unsafe { MmioTransport::new(header) }.ok()?;
    let dev_type = as_dev_type(transport.device_type())?;
    Some((dev_type, transport))
}

/// Tries to probe a VirtIO MMIO entropy source from the given memory region.
pub fn probe_mmio_entropy_device(reg_base: *mut u8, _reg_size: usize) -> Option<MmioTransport> {
    use core::ptr::NonNull;

    use virtio_drivers::transport::mmio::VirtIOHeader;

    let header = NonNull::new(reg_base as *mut VirtIOHeader)?;
    let transport = unsafe { MmioTransport::new(header) }.ok()?;
    (transport.device_type() == VirtIoDevType::EntropySource).then_some(transport)
}

/// Try to probe a VirtIO PCI device from the given PCI address.
///
/// If the device is recognized, returns the device type and a transport object
/// for later operations. Otherwise, returns [`None`].
pub fn probe_pci_device<H: VirtIoHal>(
    root: &mut PciRoot,
    bdf: DeviceFunction,
    dev_info: &DeviceFunctionInfo,
) -> Option<(DeviceType, PciTransport, Option<usize>)> {
    use virtio_drivers::transport::pci::virtio_device_type;

    let dev_type = virtio_device_type(dev_info).and_then(as_dev_type)?;
    #[cfg(target_arch = "x86_64")]
    let irq = root
        .interrupt_line_and_pin(bdf)
        .and_then(|(line, _pin)| pci_irq_vector_from_line(line));
    let transport = PciTransport::new::<H>(root, bdf).ok()?;
    Some((dev_type, transport, irq))
}

/// Tries to probe a VirtIO PCI entropy source at the given PCI address.
pub fn probe_pci_entropy_device<H: VirtIoHal>(
    root: &mut PciRoot,
    bdf: DeviceFunction,
    dev_info: &DeviceFunctionInfo,
) -> Option<PciTransport> {
    use virtio_drivers::transport::pci::virtio_device_type;

    if virtio_device_type(dev_info)? != VirtIoDevType::EntropySource {
        return None;
    }
    PciTransport::new::<H>(root, bdf).ok()
}

const fn as_dev_type(t: VirtIoDevType) -> Option<DeviceType> {
    use VirtIoDevType::*;
    match t {
        Block => Some(DeviceType::Block),
        Network => Some(DeviceType::Net),
        GPU => Some(DeviceType::Display),
        Input => Some(DeviceType::Input),
        Socket => Some(DeviceType::Vsock),
        _ => None,
    }
}

#[allow(dead_code)]
const fn as_dev_err(e: virtio_drivers::Error) -> DevError {
    use virtio_drivers::{Error::*, device::socket::SocketError::*};
    match e {
        QueueFull => DevError::BadState,
        NotReady => DevError::Again,
        WrongToken => DevError::BadState,
        AlreadyUsed => DevError::AlreadyExists,
        InvalidParam => DevError::InvalidParam,
        DmaError => DevError::NoMemory,
        IoError => DevError::Io,
        Quarantined => DevError::BadState,
        Unsupported => DevError::Unsupported,
        ConfigSpaceTooSmall => DevError::BadState,
        ConfigSpaceMissing => DevError::BadState,
        SocketDeviceError(e) => match e {
            ConnectionExists => DevError::AlreadyExists,
            NotConnected => DevError::BadState,
            InvalidOperation | InvalidNumber | UnknownOperation(_) => DevError::InvalidParam,
            OutputBufferTooShort(_) | BufferTooShort | BufferTooLong(..) => DevError::InvalidParam,
            UnexpectedDataInPacket | PeerSocketShutdown | NoResponseReceived | ConnectionFailed => {
                DevError::Io
            }
            InsufficientBufferSpaceInPeer => DevError::Again,
            RecycledWrongBuffer => DevError::BadState,
        },
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::pci_irq_vector_from_line;

    #[test]
    fn pci_irq_vector_uses_firmware_line_not_bdf_bits() {
        // q35/OVMF assigns the VirtIO block device GSI/line 4, which the
        // x86 platform delivers as vector 0x24 (36), regardless of BDF.
        assert_eq!(pci_irq_vector_from_line(4), Some(0x24));
        assert_eq!(pci_irq_vector_from_line(0), Some(0x20));
    }

    #[test]
    fn pci_irq_vector_rejects_platform_reserved_vectors() {
        assert_eq!(pci_irq_vector_from_line(0xd0), None);
        assert_eq!(pci_irq_vector_from_line(0xff), None);
    }
}
