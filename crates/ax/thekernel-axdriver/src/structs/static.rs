#[cfg(feature = "block")]
use crate::{drivers::RegisteredStaticBlockDevice, prelude::*};

/// The unified static block-device type.
///
/// Hardware probes are wrapped as `Existing`; an immutable Multiboot rootfs
/// module uses `BootModule`. Keeping both variants in one static enum avoids a
/// second filesystem or shared-device path.
#[cfg(feature = "block")]
pub enum StaticBlockDevice {
    /// A driver discovered through the normal global/MMIO/PCI probes.
    Existing(RegisteredStaticBlockDevice),
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
            Self::BootModule(device) => device.device_name(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.device_name(),
        }
    }

    fn device_type(&self) -> DeviceType {
        match self {
            Self::Existing(device) => device.device_type(),
            Self::BootModule(device) => device.device_type(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.device_type(),
        }
    }

    fn irq_num(&self) -> Option<usize> {
        match self {
            Self::Existing(device) => device.irq_num(),
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
            Self::BootModule(device) => device.num_blocks(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.num_blocks(),
        }
    }
    fn block_size(&self) -> usize {
        match self {
            Self::Existing(device) => device.block_size(),
            Self::BootModule(device) => device.block_size(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.block_size(),
        }
    }
    fn block_geometry(&self) -> DevResult<axdriver_block::BlockGeometry> {
        match self {
            Self::Existing(device) => device.block_geometry(),
            Self::BootModule(device) => device.block_geometry(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.block_geometry(),
        }
    }
    fn block_capabilities(&self) -> axdriver_block::BlockCapabilities {
        match self {
            Self::Existing(device) => device.block_capabilities(),
            Self::BootModule(device) => device.block_capabilities(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.block_capabilities(),
        }
    }
    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DevResult {
        match self {
            Self::Existing(device) => device.read_block(block_id, buf),
            Self::BootModule(device) => device.read_block(block_id, buf),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.read_block(block_id, buf),
        }
    }
    fn read_block_vectored(&mut self, block_id: u64, bufs: &mut [&mut [u8]]) -> DevResult {
        match self {
            Self::Existing(device) => device.read_block_vectored(block_id, bufs),
            Self::BootModule(device) => device.read_block_vectored(block_id, bufs),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.read_block_vectored(block_id, bufs),
        }
    }
    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DevResult {
        match self {
            Self::Existing(device) => device.write_block(block_id, buf),
            Self::BootModule(device) => device.write_block(block_id, buf),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.write_block(block_id, buf),
        }
    }
    fn write_block_vectored(&mut self, block_id: u64, bufs: &[&[u8]]) -> DevResult {
        match self {
            Self::Existing(device) => device.write_block_vectored(block_id, bufs),
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
        match self {
            Self::Existing(device) => unsafe { device.read_block_physical_sg(block_id, segments) },
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
        match self {
            Self::Existing(device) => unsafe { device.write_block_physical_sg(block_id, segments) },
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
            Self::BootModule(device) => device.flush(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.flush(),
        }
    }
    fn write_block_fua(&mut self, block_id: u64, buf: &[u8]) -> DevResult {
        match self {
            Self::Existing(device) => device.write_block_fua(block_id, buf),
            Self::BootModule(device) => device.write_block_fua(block_id, buf),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.write_block_fua(block_id, buf),
        }
    }
    fn fence(&mut self) -> DevResult {
        match self {
            Self::Existing(device) => device.fence(),
            Self::BootModule(device) => device.fence(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.fence(),
        }
    }
    fn discard_blocks(&mut self, range: BlockRange) -> DevResult {
        match self {
            Self::Existing(device) => device.discard_blocks(range),
            Self::BootModule(device) => device.discard_blocks(range),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.discard_blocks(range),
        }
    }
    fn write_zeroes(&mut self, range: BlockRange) -> DevResult {
        match self {
            Self::Existing(device) => device.write_zeroes(range),
            Self::BootModule(device) => device.write_zeroes(range),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.write_zeroes(range),
        }
    }
    fn async_queue_caps(&self) -> Option<BlockQueueCaps> {
        match self {
            Self::Existing(device) => device.async_queue_caps(),
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
            Self::BootModule(device) => device.submit_sync_batch(requests),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.submit_sync_batch(requests),
        }
    }
    unsafe fn submit_physical_batch(
        &mut self,
        requests: &mut [BlockPhysicalRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        match self {
            Self::Existing(device) => unsafe { device.submit_physical_batch(requests) },
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
            Self::BootModule(device) => device.install_completion_notifier(notifier, context),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.install_completion_notifier(notifier, context),
        }
    }
    fn reset_device(&mut self) -> DevResult<BlockResetOutcome> {
        match self {
            Self::Existing(device) => device.reset_device(),
            Self::BootModule(device) => device.reset_device(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.reset_device(),
        }
    }
    fn poll_async_complete(&mut self, budget: usize) -> DevResult<usize> {
        match self {
            Self::Existing(device) => device.poll_async_complete(budget),
            Self::BootModule(device) => device.poll_async_complete(budget),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.poll_async_complete(budget),
        }
    }
    fn wait_async_all(&mut self, handles: &[BlockRequestHandle]) -> DevResult {
        match self {
            Self::Existing(device) => device.wait_async_all(handles),
            Self::BootModule(device) => device.wait_async_all(handles),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.wait_async_all(handles),
        }
    }
    fn enable_irq(&mut self) -> DevResult {
        match self {
            Self::Existing(device) => device.enable_irq(),
            Self::BootModule(device) => device.enable_irq(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.enable_irq(),
        }
    }
    fn disable_irq(&mut self) -> DevResult {
        match self {
            Self::Existing(device) => device.disable_irq(),
            Self::BootModule(device) => device.disable_irq(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.disable_irq(),
        }
    }
    fn is_irq_enabled(&self) -> bool {
        match self {
            Self::Existing(device) => device.is_irq_enabled(),
            Self::BootModule(device) => device.is_irq_enabled(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.is_irq_enabled(),
        }
    }
    fn handle_irq(&mut self) -> DevResult<usize> {
        match self {
            Self::Existing(device) => device.handle_irq(),
            Self::BootModule(device) => device.handle_irq(),
            #[cfg(feature = "usb-xhci")]
            Self::Usb(device) => device.handle_irq(),
        }
    }
    fn fence_async(&mut self) -> DevResult {
        match self {
            Self::Existing(device) => device.fence_async(),
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
#[cfg(feature = "net")]
pub use crate::drivers::AxNetDevice;
#[cfg(feature = "vsock")]
pub use crate::drivers::AxVsockDevice;

impl super::AxDeviceEnum {
    /// Constructs a network device.
    #[cfg(feature = "net")]
    pub const fn from_net(dev: AxNetDevice) -> Self {
        Self::Net(dev)
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
