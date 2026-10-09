#[cfg(feature = "block")]
use crate::{drivers::RegisteredStaticBlockDevice, prelude::*};
#[cfg(feature = "net")]
use crate::prelude::*;

#[cfg(feature = "net")]
use crate::drivers::RegisteredStaticNetDevice;

/// Static product network devices. Most platforms retain their selected
/// primary NIC type; optional PCI families are boxed as additional devices.
#[cfg(all(feature = "net", not(feature = "dyn")))]
pub enum StaticNetDevice {
    Primary(RegisteredStaticNetDevice),
    #[cfg(feature = "e1000")]
    E1000(alloc::boxed::Box<dyn axdriver_net::NetDriverOps>),
}

#[cfg(all(feature = "net", not(feature = "dyn")))]
pub type AxNetDevice = StaticNetDevice;

#[cfg(all(feature = "net", not(feature = "dyn")))]
impl BaseDriverOps for StaticNetDevice {
    fn device_name(&self) -> &str {
        match self {
            Self::Primary(device) => device.device_name(),
            #[cfg(feature = "e1000")]
            Self::E1000(device) => device.device_name(),
        }
    }
    fn device_type(&self) -> DeviceType {
        match self {
            Self::Primary(device) => device.device_type(),
            #[cfg(feature = "e1000")]
            Self::E1000(device) => device.device_type(),
        }
    }
}

#[cfg(all(feature = "net", not(feature = "dyn")))]
impl axdriver_net::NetDriverOps for StaticNetDevice {
    fn mac_address(&self) -> axdriver_net::EthernetAddress {
        match self {
            Self::Primary(device) => device.mac_address(),
            #[cfg(feature = "e1000")]
            Self::E1000(device) => device.mac_address(),
        }
    }
    fn can_transmit(&self) -> bool {
        match self {
            Self::Primary(device) => device.can_transmit(),
            #[cfg(feature = "e1000")]
            Self::E1000(device) => device.can_transmit(),
        }
    }
    fn can_receive(&self) -> bool {
        match self {
            Self::Primary(device) => device.can_receive(),
            #[cfg(feature = "e1000")]
            Self::E1000(device) => device.can_receive(),
        }
    }
    fn rx_queue_size(&self) -> usize {
        match self {
            Self::Primary(device) => device.rx_queue_size(),
            #[cfg(feature = "e1000")]
            Self::E1000(device) => device.rx_queue_size(),
        }
    }
    fn tx_queue_size(&self) -> usize {
        match self {
            Self::Primary(device) => device.tx_queue_size(),
            #[cfg(feature = "e1000")]
            Self::E1000(device) => device.tx_queue_size(),
        }
    }
    fn recycle_rx_buffer(&mut self, buffer: axdriver_net::NetBufPtr) -> DevResult {
        match self {
            Self::Primary(device) => device.recycle_rx_buffer(buffer),
            #[cfg(feature = "e1000")]
            Self::E1000(device) => device.recycle_rx_buffer(buffer),
        }
    }
    fn recycle_tx_buffers(&mut self) -> DevResult {
        match self {
            Self::Primary(device) => device.recycle_tx_buffers(),
            #[cfg(feature = "e1000")]
            Self::E1000(device) => device.recycle_tx_buffers(),
        }
    }
    fn transmit(&mut self, buffer: axdriver_net::NetBufPtr) -> DevResult {
        match self {
            Self::Primary(device) => device.transmit(buffer),
            #[cfg(feature = "e1000")]
            Self::E1000(device) => device.transmit(buffer),
        }
    }
    fn receive(&mut self) -> DevResult<axdriver_net::NetBufPtr> {
        match self {
            Self::Primary(device) => device.receive(),
            #[cfg(feature = "e1000")]
            Self::E1000(device) => device.receive(),
        }
    }
    fn alloc_tx_buffer(&mut self, size: usize) -> DevResult<axdriver_net::NetBufPtr> {
        match self {
            Self::Primary(device) => device.alloc_tx_buffer(size),
            #[cfg(feature = "e1000")]
            Self::E1000(device) => device.alloc_tx_buffer(size),
        }
    }
    fn rx_poll_interval_micros(&self) -> Option<u64> {
        match self {
            Self::Primary(device) => device.rx_poll_interval_micros(),
            #[cfg(feature = "e1000")]
            Self::E1000(device) => device.rx_poll_interval_micros(),
        }
    }
    fn firmware_path(&self) -> Option<&'static str> {
        match self {
            Self::Primary(device) => device.firmware_path(),
            #[cfg(feature = "e1000")]
            Self::E1000(device) => device.firmware_path(),
        }
    }
    fn load_firmware(&mut self, firmware: &[u8]) -> DevResult {
        match self {
            Self::Primary(device) => device.load_firmware(firmware),
            #[cfg(feature = "e1000")]
            Self::E1000(device) => device.load_firmware(firmware),
        }
    }
}

#[cfg(all(feature = "net", not(feature = "dyn")))]
impl StaticNetDevice {
    #[cfg(feature = "e1000")]
    pub fn e1000(device: impl axdriver_net::NetDriverOps + 'static) -> Self {
        Self::E1000(alloc::boxed::Box::new(device))
    }
}

/// The unified static block-device type.
///
/// Hardware probes are wrapped as `Existing`; an immutable Multiboot rootfs
/// module uses `BootModule`. Keeping both variants in one static enum avoids a
/// second filesystem or shared-device path.
#[cfg(feature = "block")]
pub enum StaticBlockDevice {
    /// A driver discovered through the normal global/MMIO/PCI probes.
    Existing(RegisteredStaticBlockDevice),
    #[cfg(feature = "shared-block")]
    Partition(alloc::boxed::Box<crate::partition::PartitionBlock>),
    #[cfg(feature = "nvme")]
    Nvme(alloc::boxed::Box<crate::nvme::NvmeDevice>),
    #[cfg(feature = "ahci-pci")]
    Ahci(alloc::boxed::Box<dyn axdriver_block::BlockDriverOps>),
    #[cfg(feature = "sdhci-pci")]
    Sdhci(alloc::boxed::Box<dyn axdriver_block::BlockDriverOps>),
    #[cfg(feature = "usb-xhci")]
    Usb(crate::usb::UsbBlock),
    /// The immutable root filesystem module supplied by the bootloader.
    BootModule(axdriver_block::boot_module::BootModuleBlockDevice),
}

/// The sole public static block-device type.
#[cfg(feature = "block")]
pub type AxBlockDevice = StaticBlockDevice;

#[cfg(feature = "block")]
impl StaticBlockDevice {
    /// Builds the immutable rootfs module device after validating its image.
    pub fn boot_module(bytes: &'static [u8]) -> DevResult<Self> {
        Ok(Self::BootModule(
            axdriver_block::boot_module::BootModuleBlockDevice::new(bytes, 512)?,
        ))
    }
}

#[cfg(feature = "block")]
impl BaseDriverOps for StaticBlockDevice {
    fn device_name(&self) -> &str {
        match self {
            Self::Existing(device) => device.device_name(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.device_name(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.device_name(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.device_name(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.device_name(),
            Self::BootModule(device) => device.device_name(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.device_name(),
        }
    }

    fn device_type(&self) -> DeviceType {
        match self {
            Self::Existing(device) => device.device_type(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.device_type(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.device_type(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.device_type(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.device_type(),
            Self::BootModule(device) => device.device_type(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.device_type(),
        }
    }

    fn irq_num(&self) -> Option<usize> {
        match self {
            Self::Existing(device) => device.irq_num(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.irq_num(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.irq_num(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.irq_num(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.irq_num(),
            Self::BootModule(device) => device.irq_num(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.irq_num(),
        }
    }
}

#[cfg(feature = "block")]
impl BlockDriverOps for StaticBlockDevice {
    fn num_blocks(&self) -> u64 {
        match self {
            Self::Existing(device) => device.num_blocks(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.num_blocks(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.num_blocks(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.num_blocks(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.num_blocks(),
            Self::BootModule(device) => device.num_blocks(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.num_blocks(),
        }
    }
    fn block_size(&self) -> usize {
        match self {
            Self::Existing(device) => device.block_size(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.block_size(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.block_size(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.block_size(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.block_size(),
            Self::BootModule(device) => device.block_size(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.block_size(),
        }
    }
    fn block_geometry(&self) -> DevResult<axdriver_block::BlockGeometry> {
        match self {
            Self::Existing(device) => device.block_geometry(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.block_geometry(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.block_geometry(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.block_geometry(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.block_geometry(),
            Self::BootModule(device) => device.block_geometry(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.block_geometry(),
        }
    }
    fn block_capabilities(&self) -> axdriver_block::BlockCapabilities {
        match self {
            Self::Existing(device) => device.block_capabilities(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.block_capabilities(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.block_capabilities(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.block_capabilities(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.block_capabilities(),
            Self::BootModule(device) => device.block_capabilities(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.block_capabilities(),
        }
    }
    fn is_read_only(&self) -> bool {
        match self {
            Self::Existing(device) => device.is_read_only(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.is_read_only(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.is_read_only(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.is_read_only(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.is_read_only(),
            Self::BootModule(device) => device.is_read_only(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.is_read_only(),
        }
    }
    fn media_presence(&mut self) -> Option<bool> {
        match self {
            Self::Existing(device) => device.media_presence(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.media_presence(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.media_presence(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.media_presence(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.media_presence(),
            Self::BootModule(_) => None,
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.media_presence(),
        }
    }
    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DevResult {
        match self {
            Self::Existing(device) => device.read_block(block_id, buf),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.read_block(block_id, buf),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.read_block(block_id, buf),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.read_block(block_id, buf),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.read_block(block_id, buf),
            Self::BootModule(device) => device.read_block(block_id, buf),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.read_block(block_id, buf),
        }
    }
    fn read_block_vectored(&mut self, block_id: u64, bufs: &mut [&mut [u8]]) -> DevResult {
        match self {
            Self::Existing(device) => device.read_block_vectored(block_id, bufs),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.read_block_vectored(block_id, bufs),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.read_block_vectored(block_id, bufs),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.read_block_vectored(block_id, bufs),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.read_block_vectored(block_id, bufs),
            Self::BootModule(device) => device.read_block_vectored(block_id, bufs),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.read_block_vectored(block_id, bufs),
        }
    }
    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DevResult {
        match self {
            Self::Existing(device) => device.write_block(block_id, buf),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.write_block(block_id, buf),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.write_block(block_id, buf),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.write_block(block_id, buf),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.write_block(block_id, buf),
            Self::BootModule(device) => device.write_block(block_id, buf),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.write_block(block_id, buf),
        }
    }
    fn write_block_vectored(&mut self, block_id: u64, bufs: &[&[u8]]) -> DevResult {
        match self {
            Self::Existing(device) => device.write_block_vectored(block_id, bufs),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.write_block_vectored(block_id, bufs),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.write_block_vectored(block_id, bufs),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.write_block_vectored(block_id, bufs),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.write_block_vectored(block_id, bufs),
            Self::BootModule(device) => device.write_block_vectored(block_id, bufs),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.write_block_vectored(block_id, bufs),
        }
    }
    unsafe fn read_block_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[BlockPhysicalSegment],
    ) -> DevResult<BlockPhysicalSgOutcome> {
        // SAFETY: each arm forwards the caller's contract unchanged: `segments` and
        // the DMA memory they describe must satisfy the `BlockDriverOps` safety
        // requirements, which every variant of this enum shares.
        match self {
            Self::Existing(device) => unsafe { device.read_block_physical_sg(block_id, segments) },
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => unsafe { device.read_block_physical_sg(block_id, segments) },
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => unsafe { device.read_block_physical_sg(block_id, segments) },
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => unsafe { device.read_block_physical_sg(block_id, segments) },
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => unsafe { device.read_block_physical_sg(block_id, segments) },
            Self::BootModule(device) => unsafe {
                device.read_block_physical_sg(block_id, segments)
            },
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => unsafe { device.read_block_physical_sg(block_id, segments) },
        }
    }
    unsafe fn write_block_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[BlockPhysicalSegment],
    ) -> DevResult<BlockPhysicalSgOutcome> {
        // SAFETY: each arm forwards the caller's contract unchanged: `segments` and
        // the DMA memory they describe must satisfy the `BlockDriverOps` safety
        // requirements, which every variant of this enum shares.
        match self {
            Self::Existing(device) => unsafe { device.write_block_physical_sg(block_id, segments) },
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => unsafe {
                device.write_block_physical_sg(block_id, segments)
            },
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => unsafe { device.write_block_physical_sg(block_id, segments) },
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => unsafe { device.write_block_physical_sg(block_id, segments) },
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => unsafe { device.write_block_physical_sg(block_id, segments) },
            Self::BootModule(device) => unsafe {
                device.write_block_physical_sg(block_id, segments)
            },
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => unsafe { device.write_block_physical_sg(block_id, segments) },
        }
    }
    fn flush(&mut self) -> DevResult {
        match self {
            Self::Existing(device) => device.flush(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.flush(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.flush(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.flush(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.flush(),
            Self::BootModule(device) => device.flush(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.flush(),
        }
    }
    fn write_block_fua(&mut self, block_id: u64, buf: &[u8]) -> DevResult {
        match self {
            Self::Existing(device) => device.write_block_fua(block_id, buf),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.write_block_fua(block_id, buf),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.write_block_fua(block_id, buf),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.write_block_fua(block_id, buf),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.write_block_fua(block_id, buf),
            Self::BootModule(device) => device.write_block_fua(block_id, buf),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.write_block_fua(block_id, buf),
        }
    }
    fn fence(&mut self) -> DevResult {
        match self {
            Self::Existing(device) => device.fence(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.fence(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.fence(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.fence(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.fence(),
            Self::BootModule(device) => device.fence(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.fence(),
        }
    }
    fn discard_blocks(&mut self, range: BlockRange) -> DevResult {
        match self {
            Self::Existing(device) => device.discard_blocks(range),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.discard_blocks(range),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.discard_blocks(range),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.discard_blocks(range),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.discard_blocks(range),
            Self::BootModule(device) => device.discard_blocks(range),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.discard_blocks(range),
        }
    }
    fn write_zeroes(&mut self, range: BlockRange) -> DevResult {
        match self {
            Self::Existing(device) => device.write_zeroes(range),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.write_zeroes(range),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.write_zeroes(range),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.write_zeroes(range),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.write_zeroes(range),
            Self::BootModule(device) => device.write_zeroes(range),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.write_zeroes(range),
        }
    }
    fn async_queue_caps(&self) -> Option<BlockQueueCaps> {
        match self {
            Self::Existing(device) => device.async_queue_caps(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.async_queue_caps(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.async_queue_caps(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.async_queue_caps(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.async_queue_caps(),
            Self::BootModule(device) => device.async_queue_caps(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.async_queue_caps(),
        }
    }
    fn submit_async_batch(
        &mut self,
        requests: &mut [BlockQueueRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        match self {
            Self::Existing(device) => device.submit_async_batch(requests),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.submit_async_batch(requests),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.submit_async_batch(requests),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.submit_async_batch(requests),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.submit_async_batch(requests),
            Self::BootModule(device) => device.submit_async_batch(requests),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.submit_async_batch(requests),
        }
    }
    fn submit_sync_batch(
        &mut self,
        requests: &mut [BlockQueueRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        match self {
            Self::Existing(device) => device.submit_sync_batch(requests),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.submit_sync_batch(requests),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.submit_sync_batch(requests),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.submit_sync_batch(requests),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.submit_sync_batch(requests),
            Self::BootModule(device) => device.submit_sync_batch(requests),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.submit_sync_batch(requests),
        }
    }
    unsafe fn submit_physical_batch(
        &mut self,
        requests: &mut [BlockPhysicalRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        // SAFETY: each arm forwards the caller's contract unchanged: `segments` and
        // the DMA memory they describe must satisfy the `BlockDriverOps` safety
        // requirements, which every variant of this enum shares.
        match self {
            Self::Existing(device) => unsafe { device.submit_physical_batch(requests) },
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => unsafe { device.submit_physical_batch(requests) },
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => unsafe { device.submit_physical_batch(requests) },
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => unsafe { device.submit_physical_batch(requests) },
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => unsafe { device.submit_physical_batch(requests) },
            Self::BootModule(device) => unsafe { device.submit_physical_batch(requests) },
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => unsafe { device.submit_physical_batch(requests) },
        }
    }
    fn drain_async_completions(
        &mut self,
        output: &mut [BlockCompletion],
    ) -> DevResult<BlockCompletionDrain> {
        match self {
            Self::Existing(device) => device.drain_async_completions(output),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.drain_async_completions(output),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.drain_async_completions(output),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.drain_async_completions(output),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.drain_async_completions(output),
            Self::BootModule(device) => device.drain_async_completions(output),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.drain_async_completions(output),
        }
    }
    fn wait_any_physical_completion(
        &mut self,
        output: &mut [BlockCompletion],
    ) -> DevResult<BlockCompletionDrain> {
        match self {
            Self::Existing(device) => device.wait_any_physical_completion(output),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.wait_any_physical_completion(output),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.wait_any_physical_completion(output),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.wait_any_physical_completion(output),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.wait_any_physical_completion(output),
            Self::BootModule(device) => device.wait_any_physical_completion(output),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.wait_any_physical_completion(output),
        }
    }
    fn install_completion_notifier(
        &mut self,
        notifier: Option<BlockCompletionNotifier>,
        context: usize,
    ) -> DevResult {
        match self {
            Self::Existing(device) => device.install_completion_notifier(notifier, context),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.install_completion_notifier(notifier, context),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.install_completion_notifier(notifier, context),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.install_completion_notifier(notifier, context),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.install_completion_notifier(notifier, context),
            Self::BootModule(device) => device.install_completion_notifier(notifier, context),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.install_completion_notifier(notifier, context),
        }
    }
    fn reset_device(&mut self) -> DevResult<BlockResetOutcome> {
        match self {
            Self::Existing(device) => device.reset_device(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.reset_device(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.reset_device(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.reset_device(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.reset_device(),
            Self::BootModule(device) => device.reset_device(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.reset_device(),
        }
    }
    fn poll_async_complete(&mut self, budget: usize) -> DevResult<usize> {
        match self {
            Self::Existing(device) => device.poll_async_complete(budget),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.poll_async_complete(budget),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.poll_async_complete(budget),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.poll_async_complete(budget),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.poll_async_complete(budget),
            Self::BootModule(device) => device.poll_async_complete(budget),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.poll_async_complete(budget),
        }
    }
    fn wait_async_all(&mut self, handles: &[BlockRequestHandle]) -> DevResult {
        match self {
            Self::Existing(device) => device.wait_async_all(handles),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.wait_async_all(handles),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.wait_async_all(handles),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.wait_async_all(handles),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.wait_async_all(handles),
            Self::BootModule(device) => device.wait_async_all(handles),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.wait_async_all(handles),
        }
    }
    fn enable_irq(&mut self) -> DevResult {
        match self {
            Self::Existing(device) => device.enable_irq(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.enable_irq(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.enable_irq(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.enable_irq(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.enable_irq(),
            Self::BootModule(device) => device.enable_irq(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.enable_irq(),
        }
    }
    fn disable_irq(&mut self) -> DevResult {
        match self {
            Self::Existing(device) => device.disable_irq(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.disable_irq(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.disable_irq(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.disable_irq(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.disable_irq(),
            Self::BootModule(device) => device.disable_irq(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.disable_irq(),
        }
    }
    fn is_irq_enabled(&self) -> bool {
        match self {
            Self::Existing(device) => device.is_irq_enabled(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.is_irq_enabled(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.is_irq_enabled(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.is_irq_enabled(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.is_irq_enabled(),
            Self::BootModule(device) => device.is_irq_enabled(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.is_irq_enabled(),
        }
    }
    fn handle_irq(&mut self) -> DevResult<usize> {
        match self {
            Self::Existing(device) => device.handle_irq(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.handle_irq(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.handle_irq(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.handle_irq(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.handle_irq(),
            Self::BootModule(device) => device.handle_irq(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.handle_irq(),
        }
    }
    fn fence_async(&mut self) -> DevResult {
        match self {
            Self::Existing(device) => device.fence_async(),
            #[cfg(feature = "shared-block")]
            Self::Partition(device) => device.fence_async(),
            #[cfg(feature = "nvme")]
            Self::Nvme(device) => device.fence_async(),
            #[cfg(feature = "ahci-pci")]
            Self::Ahci(device) => device.fence_async(),
            #[cfg(feature = "sdhci-pci")]
            Self::Sdhci(device) => device.fence_async(),
            Self::BootModule(device) => device.fence_async(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.fence_async(),
        }
    }
}
#[cfg(feature = "display")]
pub use crate::drivers::AxDisplayDevice;
#[cfg(feature = "input")]
use crate::drivers::RegisteredStaticInputDevice;

#[cfg(feature = "input")]
pub enum AxInputDevice {
    Existing(RegisteredStaticInputDevice),
    #[cfg(feature = "usb-xhci")]
    Usb(crate::usb::UsbInput),
}

#[cfg(feature = "input")]
impl axdriver_base::BaseDriverOps for AxInputDevice {
    fn device_name(&self) -> &str {
        match self {
            Self::Existing(device) => device.device_name(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.device_name(),
        }
    }
    fn device_type(&self) -> axdriver_base::DeviceType {
        axdriver_base::DeviceType::Input
    }
    fn irq_num(&self) -> Option<usize> {
        match self {
            Self::Existing(device) => device.irq_num(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.irq_num(),
        }
    }
}

#[cfg(feature = "input")]
impl axdriver_input::InputDriverOps for AxInputDevice {
    fn device_id(&self) -> axdriver_input::InputDeviceId {
        match self {
            Self::Existing(device) => device.device_id(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.device_id(),
        }
    }
    fn physical_location(&self) -> &str {
        match self {
            Self::Existing(device) => device.physical_location(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.physical_location(),
        }
    }
    fn unique_id(&self) -> &str {
        match self {
            Self::Existing(device) => device.unique_id(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.unique_id(),
        }
    }
    fn get_event_bits(
        &mut self,
        ty: axdriver_input::EventType,
        out: &mut [u8],
    ) -> axdriver_base::DevResult<bool> {
        match self {
            Self::Existing(device) => device.get_event_bits(ty, out),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.get_event_bits(ty, out),
        }
    }
    fn get_property_bits(&mut self, out: &mut [u8]) -> axdriver_base::DevResult<bool> {
        match self {
            Self::Existing(device) => device.get_property_bits(out),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.get_property_bits(out),
        }
    }
    fn get_abs_info(
        &mut self,
        axis: u8,
    ) -> axdriver_base::DevResult<Option<axdriver_input::AbsInfo>> {
        match self {
            Self::Existing(device) => device.get_abs_info(axis),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.get_abs_info(axis),
        }
    }
    fn read_event(&mut self) -> axdriver_base::DevResult<axdriver_input::Event> {
        match self {
            Self::Existing(device) => device.read_event(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.read_event(),
        }
    }
}
#[cfg(feature = "vsock")]
pub use crate::drivers::AxVsockDevice;

impl super::AxDeviceEnum {
    /// Constructs a network device.
    #[cfg(all(feature = "net", not(net_dev = "n305-net")))]
    pub const fn from_net(dev: RegisteredStaticNetDevice) -> Self {
        Self::Net(StaticNetDevice::Primary(dev))
    }

    #[cfg(all(feature = "net", net_dev = "n305-net"))]
    pub(crate) fn try_from_net(dev: impl axdriver_net::NetDriverOps + 'static) -> DevResult<Self> {
        alloc::boxed::Box::try_new(dev)
            .map(|device| Self::Net(StaticNetDevice::Primary(device)))
            .map_err(|_| DevError::NoMemory)
    }

    #[cfg(all(feature = "net", net_dev = "n305-net"))]
    pub fn from_net(dev: impl axdriver_net::NetDriverOps + 'static) -> Self {
        Self::Net(StaticNetDevice::Primary(alloc::boxed::Box::new(dev)))
    }

    #[cfg(all(feature = "net", feature = "e1000"))]
    pub fn from_e1000(dev: impl axdriver_net::NetDriverOps + 'static) -> Self {
        Self::Net(StaticNetDevice::E1000(alloc::boxed::Box::new(dev)))
    }

    /// Constructs a block device.
    #[cfg(feature = "block")]
    pub const fn from_block(dev: RegisteredStaticBlockDevice) -> Self {
        Self::Block(StaticBlockDevice::Existing(dev))
    }

    /// Constructs a display device.
    #[cfg(feature = "display")]
    pub const fn from_display(dev: AxDisplayDevice) -> Self {
        Self::Display(dev)
    }

    /// Constructs a display device.
    #[cfg(feature = "input")]
    pub const fn from_input(dev: RegisteredStaticInputDevice) -> Self {
        Self::Input(AxInputDevice::Existing(dev))
    }

    /// Constructs a vsock device.
    #[cfg(feature = "vsock")]
    pub const fn from_vsock(dev: AxVsockDevice) -> Self {
        Self::Vsock(dev)
    }
}
