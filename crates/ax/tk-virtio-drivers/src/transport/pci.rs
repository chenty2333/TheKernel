//! PCI transport for VirtIO.

pub mod bus;

use core::{
    fmt::{self, Display, Formatter},
    hint::spin_loop,
    mem::{align_of, size_of},
    ptr::{NonNull, addr_of_mut},
    sync::atomic::{AtomicU8, Ordering},
};

use log::error;

use self::bus::{Command, DeviceFunction, DeviceFunctionInfo, PCI_CAP_ID_VNDR, PciError, PciRoot};
use super::{DeviceStatus, DeviceType, SharedMemoryRegion, Transport};
use crate::{
    Error,
    hal::{Hal, PhysAddr},
    nonnull_slice_from_raw_parts,
    volatile::{
        ReadOnly, Volatile, VolatileReadable, VolatileWritable, WriteOnly, volread, volwrite,
    },
};

#[inline]
fn dma_sync_barrier() {}

/// The PCI vendor ID for VirtIO devices.
const VIRTIO_VENDOR_ID: u16 = 0x1af4;

/// The offset to add to a VirtIO device ID to get the corresponding PCI device ID.
const PCI_DEVICE_ID_OFFSET: u16 = 0x1040;

const TRANSITIONAL_NETWORK: u16 = 0x1000;
const TRANSITIONAL_BLOCK: u16 = 0x1001;
const TRANSITIONAL_MEMORY_BALLOONING: u16 = 0x1002;
const TRANSITIONAL_CONSOLE: u16 = 0x1003;
const TRANSITIONAL_SCSI_HOST: u16 = 0x1004;
const TRANSITIONAL_ENTROPY_SOURCE: u16 = 0x1005;
const TRANSITIONAL_9P_TRANSPORT: u16 = 0x1009;

// A transport destructor cannot report reset failure. Keep its best-effort
// quiescence probe bounded; device wrappers own DMA queues and must retain
// those owners when their stronger reset proof fails.
const RESET_POLL_BUDGET: usize = 1 << 20;

/// The offset of the bar field within `virtio_pci_cap`.
const CAP_BAR_OFFSET: u8 = 4;
/// The offset of the offset field with `virtio_pci_cap`.
const CAP_BAR_OFFSET_OFFSET: u8 = 8;
/// The offset of the `length` field within `virtio_pci_cap`.
const CAP_LENGTH_OFFSET: u8 = 12;
/// The offset of the`notify_off_multiplier` field within `virtio_pci_notify_cap`.
const CAP_NOTIFY_OFF_MULTIPLIER_OFFSET: u8 = 16;
/// Size of conventional PCI configuration space, which contains the legacy
/// capability list used by this transport.
const PCI_CONFIG_SPACE_LEN: usize = 256;

/// Common configuration.
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
/// Notifications.
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
/// ISR Status.
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
/// Device specific configuration.
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;
/// Shared-memory region capability (VirtIO PCI 4.1.4.7).
const VIRTIO_PCI_CAP_SHARED_MEMORY_CFG: u8 = 8;
/// A PCI capability chain occupies the 192-byte capability area and each
/// entry is at least four-byte aligned, so this bounds the metadata snapshot
/// before mutable BAR reads begin.
const MAX_PCI_CAPABILITIES: usize = 48;

fn device_type(pci_device_id: u16) -> DeviceType {
    match pci_device_id {
        TRANSITIONAL_NETWORK => DeviceType::Network,
        TRANSITIONAL_BLOCK => DeviceType::Block,
        TRANSITIONAL_MEMORY_BALLOONING => DeviceType::MemoryBalloon,
        TRANSITIONAL_CONSOLE => DeviceType::Console,
        TRANSITIONAL_SCSI_HOST => DeviceType::ScsiHost,
        TRANSITIONAL_ENTROPY_SOURCE => DeviceType::EntropySource,
        TRANSITIONAL_9P_TRANSPORT => DeviceType::_9P,
        id if id >= PCI_DEVICE_ID_OFFSET => DeviceType::from(id - PCI_DEVICE_ID_OFFSET),
        _ => DeviceType::Invalid,
    }
}

/// Returns the type of VirtIO device to which the given PCI vendor and device ID corresponds, or
/// `None` if it is not a recognised VirtIO device.
pub fn virtio_device_type(device_function_info: &DeviceFunctionInfo) -> Option<DeviceType> {
    if device_function_info.vendor_id == VIRTIO_VENDOR_ID {
        let device_type = device_type(device_function_info.device_id);
        if device_type != DeviceType::Invalid {
            return Some(device_type);
        }
    }
    None
}

/// PCI transport for VirtIO.
///
/// Ref: 4.1 Virtio Over PCI Bus
#[derive(Debug)]
pub struct PciTransport {
    device_type: DeviceType,
    /// The bus, device and function identifier for the VirtIO device.
    device_function: DeviceFunction,
    /// The common configuration structure within some BAR.
    common_cfg: NonNull<CommonCfg>,
    /// The start of the queue notification region within some BAR.
    notify_region: NonNull<[WriteOnly<u16>]>,
    notify_off_multiplier: u32,
    /// The ISR status register within some BAR.
    isr_status: NonNull<Volatile<u8>>,
    command: NonNull<u16>,
    interrupt_registration: Option<PciInterruptRegistration>,
    /// The VirtIO device-specific configuration within some BAR.
    config_space: Option<NonNull<[u32]>>,
    shared_memory: [Option<SharedMemoryRegion>; 256],
    /// Set after a device wrapper observed status zero and completed reset.
    reset_complete: bool,
    /// A malformed notify offset was observed and the device was failed.
    notify_faulted: bool,
}

#[derive(Debug)]
struct PciInterruptRegistration {
    pending: &'static AtomicU8,
    context: usize,
    release: fn(usize),
}

/// Read-to-clear ISR capability used by the shared PCI interrupt dispatcher.
#[derive(Clone, Copy, Debug)]
pub struct PciInterruptSource(NonNull<Volatile<u8>>);

impl PciInterruptSource {
    /// Capture the device's queue/configuration interrupt bits.
    ///
    /// # Safety
    /// The originating transport must remain alive, and dispatcher teardown
    /// must finish all captures before dropping that transport.
    pub unsafe fn capture(self) -> u8 {
        unsafe { self.0.as_ptr().vread() & 3 }
    }

    /// Opaque MMIO address for an allocation-free dispatcher registry.
    pub fn address(self) -> usize {
        self.0.as_ptr() as usize
    }

    /// Reconstruct a capability previously obtained from a live transport.
    ///
    /// # Safety
    /// `address` must come from `address()` and obey `capture()`'s lifetime.
    pub unsafe fn from_address(address: usize) -> Self {
        Self(unsafe { NonNull::new_unchecked(address as *mut Volatile<u8>) })
    }
}

impl PciTransport {
    /// Construct a new PCI VirtIO device driver for the given device function on the given PCI
    /// root controller.
    ///
    /// The PCI device must already have had its BARs allocated.
    pub fn new<H: Hal>(
        root: &mut PciRoot,
        device_function: DeviceFunction,
    ) -> Result<Self, VirtioPciError> {
        let device_vendor = root.config_read_word(device_function, 0);
        let device_id = (device_vendor >> 16) as u16;
        let vendor_id = device_vendor as u16;
        if vendor_id != VIRTIO_VENDOR_ID {
            return Err(VirtioPciError::InvalidVendorId(vendor_id));
        }
        let device_type = device_type(device_id);

        // An initialized virtqueue can assert INTx even without a reader
        // (including configuration interrupts). Keep the function quiet
        // until a shared interrupt owner has been installed successfully.
        let (_, command) = root.get_status_command(device_function);
        root.set_command(device_function, command | Command::INTERRUPT_DISABLE);
        let command = NonNull::new(root.command_register(device_function)).unwrap();

        // Find the PCI capabilities we need.
        // `CapabilityIterator` holds an immutable borrow of `root`, while
        // capability decoding below needs mutable config/BAR access. Snapshot
        // the bounded PCI capability chain first so the two phases cannot
        // overlap.
        let mut capabilities = [None; MAX_PCI_CAPABILITIES];
        {
            let mut iterator = root.capabilities(device_function);
            for slot in &mut capabilities {
                let Some(capability) = iterator.next() else {
                    break;
                };
                *slot = Some(capability);
            }
        }

        let mut common_cfg = None;
        let mut notify_cfg = None;
        let mut notify_off_multiplier = 0;
        let mut isr_cfg = None;
        let mut device_cfg = None;
        let mut shared_memory = [None; 256];
        for capability in capabilities.iter().flatten() {
            if capability.id != PCI_CAP_ID_VNDR {
                continue;
            }
            let cap_len = capability.private_header as u8;
            let cfg_type = (capability.private_header >> 8) as u8;
            if cap_len < 16 || !capability_fits_config_space(capability.offset, cap_len) {
                continue;
            }
            let mut read_word = |offset| root.config_read_word(device_function, offset);
            let Some((struct_info, shared_memory_id)) =
                decode_virtio_pci_cap(&mut read_word, capability.offset, cap_len)
            else {
                continue;
            };

            match cfg_type {
                VIRTIO_PCI_CAP_COMMON_CFG if common_cfg.is_none() => {
                    common_cfg = Some(struct_info);
                }
                VIRTIO_PCI_CAP_NOTIFY_CFG if cap_len >= 20 && notify_cfg.is_none() => {
                    notify_cfg = Some(struct_info);
                    notify_off_multiplier = root.config_read_word(
                        device_function,
                        capability.offset + CAP_NOTIFY_OFF_MULTIPLIER_OFFSET,
                    );
                }
                VIRTIO_PCI_CAP_ISR_CFG if isr_cfg.is_none() => {
                    isr_cfg = Some(struct_info);
                }
                VIRTIO_PCI_CAP_DEVICE_CFG if device_cfg.is_none() => {
                    device_cfg = Some(struct_info);
                }
                VIRTIO_PCI_CAP_SHARED_MEMORY_CFG if cap_len >= 24 => {
                    let Some((struct_info, id)) = decode_virtio_pci_cap64(
                        &mut read_word,
                        capability.offset,
                        cap_len,
                        struct_info,
                        shared_memory_id,
                    ) else {
                        continue;
                    };
                    // First capability for an ID wins; each range is fully
                    // BAR-bounds checked before it becomes observable.
                    if shared_memory[id as usize].is_none() {
                        let (phys_base, _) =
                            get_bar_physical_range(root, device_function, &struct_info)?;
                        let region =
                            get_bar_region_slice::<H, u8>(root, device_function, &struct_info)?;
                        shared_memory[id as usize] = Some(SharedMemoryRegion {
                            phys_base,
                            virt_base: NonNull::new(region.as_ptr() as *mut u8).unwrap(),
                            len: region.len(),
                        });
                    }
                }
                _ => {}
            }
        }

        let common_cfg = get_bar_region::<H, _>(
            root,
            device_function,
            &common_cfg.ok_or(VirtioPciError::MissingCommonConfig)?,
        )?;

        let notify_cfg = notify_cfg.ok_or(VirtioPciError::MissingNotifyConfig)?;
        if notify_off_multiplier % 2 != 0 {
            return Err(VirtioPciError::InvalidNotifyOffMultiplier(
                notify_off_multiplier,
            ));
        }
        let notify_region = get_bar_region_slice::<H, _>(root, device_function, &notify_cfg)?;

        let isr_status = get_bar_region::<H, _>(
            root,
            device_function,
            &isr_cfg.ok_or(VirtioPciError::MissingIsrConfig)?,
        )?;

        let config_space = if let Some(device_cfg) = device_cfg {
            Some(get_bar_region_slice::<H, _>(
                root,
                device_function,
                &device_cfg,
            )?)
        } else {
            None
        };

        Ok(Self {
            device_type,
            device_function,
            common_cfg,
            notify_region,
            notify_off_multiplier,
            isr_status,
            command,
            interrupt_registration: None,
            config_space,
            shared_memory,
            reset_complete: false,
            notify_faulted: false,
        })
    }

    /// ISR capability whose lifetime is bounded by this transport.
    pub fn interrupt_source(&self) -> PciInterruptSource {
        PciInterruptSource(self.isr_status)
    }

    /// Admit a shared dispatcher before enabling this function's INTx.
    /// The release callback must deactivate its endpoint and synchronize
    /// interrupt readers; it runs with INTx masked before transport reset.
    pub fn register_interrupt_handler(
        &mut self,
        pending: &'static AtomicU8,
        context: usize,
        release: fn(usize),
    ) -> bool {
        if self.interrupt_registration.is_some() {
            return false;
        }
        self.interrupt_registration = Some(PciInterruptRegistration {
            pending,
            context,
            release,
        });
        true
    }

    /// Enable INTx only after its shared dispatcher owns acknowledgment.
    pub fn enable_interrupts(&mut self) -> bool {
        if self.interrupt_registration.is_none() {
            return false;
        }
        self.set_intx_mask(false);
        true
    }

    fn set_intx_mask(&mut self, masked: bool) {
        // Command is an independently writable halfword. Never rewrite the
        // adjacent W1C status bits when changing this function's INTx gate.
        unsafe {
            let command = self.command.as_ptr().read_volatile();
            let next = if masked {
                command | Command::INTERRUPT_DISABLE.bits()
            } else {
                command & !Command::INTERRUPT_DISABLE.bits()
            };
            self.command.as_ptr().write_volatile(next);
        }
    }
}

/// Return the physical BAR subrange before creating its CPU mapping.  Shared
/// memory is a device-visible physical aperture, never a translation of the
/// CPU virtual BAR mapping.
fn get_bar_physical_range(
    root: &mut PciRoot,
    device_function: DeviceFunction,
    info: &VirtioCapabilityInfo,
) -> Result<(usize, usize), VirtioPciError> {
    let (base, size) = root
        .bar_info(device_function, info.bar)?
        .memory_address_size()
        .ok_or(VirtioPciError::UnexpectedIoBar)?;
    let end = info
        .offset
        .checked_add(info.length)
        .ok_or(VirtioPciError::BarOffsetOutOfRange)?;
    if base == 0 || end > size as u64 || info.length > usize::MAX as u64 {
        return Err(VirtioPciError::BarOffsetOutOfRange);
    }
    let phys = (base as u64)
        .checked_add(info.offset)
        .ok_or(VirtioPciError::BarOffsetOutOfRange)?;
    usize::try_from(phys)
        .map(|p| (p, info.length as usize))
        .map_err(|_| VirtioPciError::BarOffsetOutOfRange)
}

impl Transport for PciTransport {
    fn shared_memory_region(&self, id: u8) -> Option<SharedMemoryRegion> {
        self.shared_memory[id as usize]
    }
    fn device_type(&self) -> DeviceType {
        self.device_type
    }

    fn read_device_features(&mut self) -> u64 {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe {
            volwrite!(self.common_cfg, device_feature_select, 0);
            let mut device_features_bits = volread!(self.common_cfg, device_feature) as u64;
            volwrite!(self.common_cfg, device_feature_select, 1);
            device_features_bits |= (volread!(self.common_cfg, device_feature) as u64) << 32;
            device_features_bits
        }
    }

    fn write_driver_features(&mut self, driver_features: u64) {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe {
            volwrite!(self.common_cfg, driver_feature_select, 0);
            volwrite!(self.common_cfg, driver_feature, driver_features as u32);
            volwrite!(self.common_cfg, driver_feature_select, 1);
            volwrite!(
                self.common_cfg,
                driver_feature,
                (driver_features >> 32) as u32
            );
        }
    }

    fn max_queue_size(&mut self, queue: u16) -> u32 {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe {
            volwrite!(self.common_cfg, queue_select, queue);
            volread!(self.common_cfg, queue_size).into()
        }
    }

    fn notify(&mut self, queue: u16) {
        // Safe because the common config and notify region pointers are valid and we checked in
        // get_bar_region that they were aligned.
        unsafe {
            volwrite!(self.common_cfg, queue_select, queue);
            // TODO: Consider caching this somewhere (per queue).
            let queue_notify_off = volread!(self.common_cfg, queue_notify_off);

            let Some(index) = notify_index(
                queue_notify_off,
                self.notify_off_multiplier,
                self.notify_region.len(),
            ) else {
                // The trait cannot return an error.  Mark the device failed
                // and do not write outside the BAR advertised by the device.
                self.set_status(self.get_status() | DeviceStatus::FAILED);
                if !self.notify_faulted {
                    error!(
                        "virtio PCI notification offset is outside the advertised notify region"
                    );
                    self.notify_faulted = true;
                }
                return;
            };
            dma_sync_barrier();
            addr_of_mut!((*self.notify_region.as_ptr())[index]).vwrite(queue);
        }
    }

    fn get_status(&self) -> DeviceStatus {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        let status = unsafe { volread!(self.common_cfg, device_status) };
        DeviceStatus::from_bits_truncate(status.into())
    }

    fn set_status(&mut self, status: DeviceStatus) {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe {
            volwrite!(self.common_cfg, device_status, status.bits() as u8);
        }
        if !status.is_empty() {
            self.reset_complete = false;
        }
    }

    fn mark_reset_complete(&mut self) {
        self.reset_complete = true;
    }

    fn set_guest_page_size(&mut self, _guest_page_size: u32) {
        // No-op, the PCI transport doesn't care.
    }

    fn requires_legacy_layout(&self) -> bool {
        false
    }

    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: PhysAddr,
        driver_area: PhysAddr,
        device_area: PhysAddr,
    ) {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe {
            volwrite!(self.common_cfg, queue_select, queue);
            volwrite!(self.common_cfg, queue_size, size as u16);
            volwrite!(self.common_cfg, queue_desc, descriptors as u64);
            volwrite!(self.common_cfg, queue_driver, driver_area as u64);
            volwrite!(self.common_cfg, queue_device, device_area as u64);
            volwrite!(self.common_cfg, queue_enable, 1);
        }
    }

    fn queue_unset(&mut self, _queue: u16) {
        // The VirtIO spec doesn't allow queues to be unset once they have been set up for the PCI
        // transport, so this is a no-op.
    }

    fn queue_used(&mut self, queue: u16) -> bool {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe {
            volwrite!(self.common_cfg, queue_select, queue);
            volread!(self.common_cfg, queue_enable) == 1
        }
    }

    fn ack_interrupt(&mut self) -> bool {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        // Reading the ISR status resets it to 0 and causes the device to de-assert the interrupt.
        let isr_status = unsafe { self.isr_status.as_ptr().vread() }
            | self
                .interrupt_registration
                .as_ref()
                .map_or(0, |registration| {
                    registration.pending.swap(0, Ordering::AcqRel)
                });
        // TODO: Distinguish between queue interrupt and device configuration interrupt.
        isr_status & 0x3 != 0
    }

    fn config_space<T>(&self) -> Result<NonNull<T>, Error> {
        if let Some(config_space) = self.config_space {
            if size_of::<T>() > config_space.len() * size_of::<u32>() {
                Err(Error::ConfigSpaceTooSmall)
            } else if align_of::<T>() > 4 {
                // Panic as this should only happen if the driver is written incorrectly.
                panic!(
                    "Driver expected config space alignment of {} bytes, but VirtIO only \
                     guarantees 4 byte alignment.",
                    align_of::<T>()
                );
            } else {
                // TODO: Use NonNull::as_non_null_ptr once it is stable.
                let config_space_ptr = NonNull::new(config_space.as_ptr() as *mut u32).unwrap();
                Ok(config_space_ptr.cast())
            }
        } else {
            Err(Error::ConfigSpaceMissing)
        }
    }
}

// SAFETY: MMIO can be done from any thread or CPU core.
unsafe impl Send for PciTransport {}

// SAFETY: `&PciTransport` only allows MMIO reads or getting the config space, both of which are
// fine to happen concurrently on different CPU cores.
unsafe impl Sync for PciTransport {}

impl Drop for PciTransport {
    fn drop(&mut self) {
        self.set_intx_mask(true);
        if let Some(registration) = self.interrupt_registration.take() {
            (registration.release)(registration.context);
        }
        if self.reset_complete {
            return;
        }
        // Reset the device when the transport is dropped.
        self.set_status(DeviceStatus::empty());
        for _ in 0..RESET_POLL_BUDGET {
            if self.get_status() == DeviceStatus::empty() {
                self.reset_complete = true;
                return;
            }
            spin_loop();
        }
    }
}

/// `virtio_pci_common_cfg`, see 4.1.4.3 "Common configuration structure layout".
#[repr(C)]
struct CommonCfg {
    device_feature_select: Volatile<u32>,
    device_feature: ReadOnly<u32>,
    driver_feature_select: Volatile<u32>,
    driver_feature: Volatile<u32>,
    msix_config: Volatile<u16>,
    num_queues: ReadOnly<u16>,
    device_status: Volatile<u8>,
    config_generation: ReadOnly<u8>,
    queue_select: Volatile<u16>,
    queue_size: Volatile<u16>,
    queue_msix_vector: Volatile<u16>,
    queue_enable: Volatile<u16>,
    queue_notify_off: Volatile<u16>,
    queue_desc: Volatile<u64>,
    queue_driver: Volatile<u64>,
    queue_device: Volatile<u64>,
}

/// Information about a VirtIO structure within some BAR, as provided by a `virtio_pci_cap`.
#[derive(Clone, Debug, Eq, PartialEq)]
struct VirtioCapabilityInfo {
    /// The bar in which the structure can be found.
    bar: u8,
    /// The offset within the bar.
    offset: u64,
    /// The length in bytes of the structure within the bar.
    length: u64,
}

/// Return whether a capability, including its advertised extent, is entirely
/// contained in conventional PCI configuration space.
fn capability_fits_config_space(capability_offset: u8, capability_len: u8) -> bool {
    usize::from(capability_offset)
        .checked_add(usize::from(capability_len))
        .is_some_and(|end| end <= PCI_CONFIG_SPACE_LEN)
}

/// Decode the aligned dwords shared by every `virtio_pci_cap`.
///
/// PCI configuration-space accesses are dword-only for this transport.  In
/// particular, the `bar` and `id` bytes share the dword at `cap + 4`; the
/// offset and length each occupy one complete little-endian dword.
fn decode_virtio_pci_cap(
    read_word: &mut impl FnMut(u8) -> u32,
    capability_offset: u8,
    capability_len: u8,
) -> Option<(VirtioCapabilityInfo, u8)> {
    if capability_len < 16 || !capability_fits_config_space(capability_offset, capability_len) {
        return None;
    }
    let bar_and_id = read_word(capability_offset + CAP_BAR_OFFSET);
    let offset = read_word(capability_offset + CAP_BAR_OFFSET_OFFSET);
    let length = read_word(capability_offset + CAP_LENGTH_OFFSET);
    Some((
        VirtioCapabilityInfo {
            bar: bar_and_id as u8,
            offset: u64::from(offset),
            length: u64::from(length),
        },
        (bar_and_id >> 8) as u8,
    ))
}

/// Decode the two high dwords of a `virtio_pci_cap64` shared-memory region.
fn decode_virtio_pci_cap64(
    read_word: &mut impl FnMut(u8) -> u32,
    capability_offset: u8,
    capability_len: u8,
    low: VirtioCapabilityInfo,
    id: u8,
) -> Option<(VirtioCapabilityInfo, u8)> {
    if capability_len < 24 || !capability_fits_config_space(capability_offset, capability_len) {
        return None;
    }
    let offset_hi = read_word(capability_offset + 16);
    let length_hi = read_word(capability_offset + 20);
    Some((
        VirtioCapabilityInfo {
            bar: low.bar,
            offset: (u64::from(offset_hi) << 32) | low.offset,
            length: (u64::from(length_hi) << 32) | low.length,
        },
        id,
    ))
}

/// Return the u16 index within the notify region for one queue notification.
/// A malformed device offset fails closed because `Transport::notify` cannot
/// report an error to its callers.
fn notify_index(
    queue_notify_off: u16,
    notify_off_multiplier: u32,
    notify_len: usize,
) -> Option<usize> {
    let offset_bytes =
        usize::try_from(u64::from(queue_notify_off).checked_mul(u64::from(notify_off_multiplier))?)
            .ok()?;
    let notify_bytes = notify_len.checked_mul(size_of::<u16>())?;
    let write_end = offset_bytes.checked_add(size_of::<u16>())?;
    if offset_bytes % size_of::<u16>() != 0 || write_end > notify_bytes {
        return None;
    }
    Some(offset_bytes / size_of::<u16>())
}

fn get_bar_region<H: Hal, T>(
    root: &mut PciRoot,
    device_function: DeviceFunction,
    struct_info: &VirtioCapabilityInfo,
) -> Result<NonNull<T>, VirtioPciError> {
    let bar_info = root.bar_info(device_function, struct_info.bar)?;
    let (bar_address, bar_size) = bar_info
        .memory_address_size()
        .ok_or(VirtioPciError::UnexpectedIoBar)?;
    if bar_address == 0 {
        return Err(VirtioPciError::BarNotAllocated(struct_info.bar));
    }
    if struct_info
        .offset
        .checked_add(struct_info.length)
        .is_none_or(|end| end > bar_size as u64)
        || size_of::<T>() > struct_info.length as usize
    {
        return Err(VirtioPciError::BarOffsetOutOfRange);
    }
    let paddr = bar_address as PhysAddr + struct_info.offset as PhysAddr;
    // Safe because the paddr and size describe a valid MMIO region, at least according to the PCI
    // bus.
    let vaddr = unsafe { H::mmio_phys_to_virt(paddr, struct_info.length as usize) };
    if vaddr.as_ptr() as usize % align_of::<T>() != 0 {
        return Err(VirtioPciError::Misaligned {
            vaddr,
            alignment: align_of::<T>(),
        });
    }
    Ok(vaddr.cast())
}

fn get_bar_region_slice<H: Hal, T>(
    root: &mut PciRoot,
    device_function: DeviceFunction,
    struct_info: &VirtioCapabilityInfo,
) -> Result<NonNull<[T]>, VirtioPciError> {
    let ptr = get_bar_region::<H, T>(root, device_function, struct_info)?;
    Ok(nonnull_slice_from_raw_parts(
        ptr,
        struct_info.length as usize / size_of::<T>(),
    ))
}

/// An error encountered initialising a VirtIO PCI transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VirtioPciError {
    /// PCI device vender ID was not the VirtIO vendor ID.
    InvalidVendorId(u16),
    /// No valid `VIRTIO_PCI_CAP_COMMON_CFG` capability was found.
    MissingCommonConfig,
    /// No valid `VIRTIO_PCI_CAP_NOTIFY_CFG` capability was found.
    MissingNotifyConfig,
    /// `VIRTIO_PCI_CAP_NOTIFY_CFG` capability has a `notify_off_multiplier` that is not a multiple
    /// of 2.
    InvalidNotifyOffMultiplier(u32),
    /// No valid `VIRTIO_PCI_CAP_ISR_CFG` capability was found.
    MissingIsrConfig,
    /// An IO BAR was provided rather than a memory BAR.
    UnexpectedIoBar,
    /// A BAR which we need was not allocated an address.
    BarNotAllocated(u8),
    /// The offset for some capability was greater than the length of the BAR.
    BarOffsetOutOfRange,
    /// The virtual address was not aligned as expected.
    Misaligned {
        /// The virtual address in question.
        vaddr: NonNull<u8>,
        /// The expected alignment in bytes.
        alignment: usize,
    },
    /// A generic PCI error,
    Pci(PciError),
}

impl Display for VirtioPciError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            Self::InvalidVendorId(vendor_id) => write!(
                f,
                "PCI device vender ID {:#06x} was not the VirtIO vendor ID {:#06x}.",
                vendor_id, VIRTIO_VENDOR_ID
            ),
            Self::MissingCommonConfig => write!(
                f,
                "No valid `VIRTIO_PCI_CAP_COMMON_CFG` capability was found."
            ),
            Self::MissingNotifyConfig => write!(
                f,
                "No valid `VIRTIO_PCI_CAP_NOTIFY_CFG` capability was found."
            ),
            Self::InvalidNotifyOffMultiplier(notify_off_multiplier) => {
                write!(
                    f,
                    "`VIRTIO_PCI_CAP_NOTIFY_CFG` capability has a `notify_off_multiplier` that is \
                     not a multiple of 2: {}",
                    notify_off_multiplier
                )
            }
            Self::MissingIsrConfig => {
                write!(f, "No valid `VIRTIO_PCI_CAP_ISR_CFG` capability was found.")
            }
            Self::UnexpectedIoBar => write!(f, "Unexpected IO BAR (expected memory BAR)."),
            Self::BarNotAllocated(bar_index) => write!(f, "Bar {} not allocated.", bar_index),
            Self::BarOffsetOutOfRange => write!(f, "Capability offset greater than BAR length."),
            Self::Misaligned { vaddr, alignment } => write!(
                f,
                "Virtual address {:#018?} was not aligned to a {} byte boundary as expected.",
                vaddr, alignment
            ),
            Self::Pci(pci_error) => pci_error.fmt(f),
        }
    }
}

impl From<PciError> for VirtioPciError {
    fn from(error: PciError) -> Self {
        Self::Pci(error)
    }
}

// SAFETY: The `vaddr` field of `VirtioPciError::Misaligned` is only used for debug output.
unsafe impl Send for VirtioPciError {}

// SAFETY: The `vaddr` field of `VirtioPciError::Misaligned` is only used for debug output.
unsafe impl Sync for VirtioPciError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intx_admission_latches_both_status_bits_and_masks_before_release() {
        static PENDING: AtomicU8 = AtomicU8::new(0);
        static RELEASED: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        fn release(context: usize) {
            // The transport must close the hardware gate before letting the
            // dispatcher release its last live MMIO reader.
            let command = unsafe { (context as *const u32).read_volatile() };
            assert_eq!(command, 0xa5a5_0407);
            RELEASED.store(true, Ordering::Release);
        }

        let mut command_status = 0xa5a5_0407u32;
        let mut isr = Volatile::new(3u8);
        let mut transport = PciTransport {
            device_type: DeviceType::Block,
            device_function: DeviceFunction {
                bus: 0,
                device: 6,
                function: 0,
            },
            // This fixture only exercises the interrupt registers. The
            // already-reset transport never accesses the unused BARs.
            common_cfg: NonNull::dangling(),
            notify_region: NonNull::slice_from_raw_parts(NonNull::dangling(), 0),
            notify_off_multiplier: 4,
            isr_status: NonNull::from(&mut isr),
            command: NonNull::from(&mut command_status).cast(),
            interrupt_registration: None,
            config_space: None,
            shared_memory: [None; 256],
            reset_complete: true,
            notify_faulted: false,
        };
        assert!(!transport.enable_interrupts());
        assert_eq!(command_status, 0xa5a5_0407);
        assert!(transport.register_interrupt_handler(
            &PENDING,
            &command_status as *const u32 as usize,
            release,
        ));
        assert!(transport.enable_interrupts());
        assert_eq!(command_status, 0xa5a5_0007);
        // Model the broker's read-to-clear capture before the driver runs.
        PENDING.fetch_or(
            unsafe { transport.interrupt_source().capture() },
            Ordering::AcqRel,
        );
        unsafe { (&mut isr as *mut Volatile<u8>).vwrite(0) };
        assert_eq!(PENDING.load(Ordering::Acquire), 3);
        assert!(transport.ack_interrupt());
        assert!(!transport.ack_interrupt());
        drop(transport);
        assert!(RELEASED.load(Ordering::Acquire));
        assert_eq!(command_status, 0xa5a5_0407);
    }

    #[test]
    fn transitional_device_ids() {
        assert_eq!(device_type(0x1000), DeviceType::Network);
        assert_eq!(device_type(0x1002), DeviceType::MemoryBalloon);
        assert_eq!(device_type(0x1009), DeviceType::_9P);
    }

    #[test]
    fn offset_device_ids() {
        assert_eq!(device_type(0x1040), DeviceType::Invalid);
        assert_eq!(device_type(0x1045), DeviceType::MemoryBalloon);
        assert_eq!(device_type(0x1049), DeviceType::_9P);
        assert_eq!(device_type(0x1058), DeviceType::Memory);
        assert_eq!(device_type(0x1059), DeviceType::Sound);
        assert_eq!(device_type(0x1060), DeviceType::Invalid);
    }

    #[test]
    fn virtio_device_type_valid() {
        assert_eq!(
            virtio_device_type(&DeviceFunctionInfo {
                vendor_id: VIRTIO_VENDOR_ID,
                device_id: TRANSITIONAL_BLOCK,
                class: 0,
                subclass: 0,
                prog_if: 0,
                revision: 0,
                header_type: bus::HeaderType::Standard,
            }),
            Some(DeviceType::Block)
        );
    }

    #[test]
    fn virtio_device_type_invalid() {
        // Non-VirtIO vendor ID.
        assert_eq!(
            virtio_device_type(&DeviceFunctionInfo {
                vendor_id: 0x1234,
                device_id: TRANSITIONAL_BLOCK,
                class: 0,
                subclass: 0,
                prog_if: 0,
                revision: 0,
                header_type: bus::HeaderType::Standard,
            }),
            None
        );

        // Invalid device ID.
        assert_eq!(
            virtio_device_type(&DeviceFunctionInfo {
                vendor_id: VIRTIO_VENDOR_ID,
                device_id: 0x1040,
                class: 0,
                subclass: 0,
                prog_if: 0,
                revision: 0,
                header_type: bus::HeaderType::Standard,
            }),
            None
        );
    }

    #[test]
    fn pci_cap_decoder_reads_only_aligned_dwords() {
        let cap = 0x40;
        let mut reads = std::vec::Vec::new();
        let mut read_word = |offset: u8| {
            assert_eq!(offset & 3, 0, "capability parser made an unaligned read");
            reads.push(offset);
            match offset {
                0x44 => 0x0000_0702,
                0x48 => 0x89ab_cdef,
                0x4c => 0x1020_3040,
                _ => panic!("unexpected configuration dword {offset:#x}"),
            }
        };

        let (info, id) = decode_virtio_pci_cap(&mut read_word, cap, 16).unwrap();

        assert_eq!(id, 7);
        assert_eq!(info.bar, 2);
        assert_eq!(info.offset, 0x89ab_cdef);
        assert_eq!(info.length, 0x1020_3040);
        assert_eq!(reads, [0x44, 0x48, 0x4c]);
    }

    #[test]
    fn pci_cap64_decoder_uses_aligned_shared_memory_high_dwords() {
        let cap = 0x40;
        let mut reads = std::vec::Vec::new();
        let mut read_word = |offset: u8| {
            assert_eq!(offset & 3, 0, "capability parser made an unaligned read");
            reads.push(offset);
            match offset {
                0x44 => 0x0000_0103,
                0x48 => 0x89ab_cdef,
                0x4c => 0x1020_3040,
                0x50 => 0x1122_3344,
                0x54 => 0x5566_7788,
                _ => panic!("unexpected configuration dword {offset:#x}"),
            }
        };

        let (low, id) = decode_virtio_pci_cap(&mut read_word, cap, 24).unwrap();
        let (info, id) = decode_virtio_pci_cap64(&mut read_word, cap, 24, low, id).unwrap();

        assert_eq!(id, 1);
        assert_eq!(info.bar, 3);
        assert_eq!(info.offset, 0x1122_3344_89ab_cdef);
        assert_eq!(info.length, 0x5566_7788_1020_3040);
        assert_eq!(reads, [0x44, 0x48, 0x4c, 0x50, 0x54]);
    }

    #[test]
    fn pci_cap_decoder_rejects_truncated_capability_without_reading() {
        let mut read_word =
            |_offset: u8| -> u32 { panic!("truncated capability must not be read") };

        assert!(decode_virtio_pci_cap(&mut read_word, 0xf4, 16).is_none());
        assert!(decode_virtio_pci_cap(&mut read_word, 0x40, 15).is_none());
        assert!(!capability_fits_config_space(0xf0, 17));
    }

    #[test]
    fn pci_cap64_decoder_rejects_truncated_capability_without_reading() {
        let low = VirtioCapabilityInfo {
            bar: 0,
            offset: 0,
            length: 0,
        };
        let mut read_word =
            |_offset: u8| -> u32 { panic!("truncated capability must not be read") };

        assert!(decode_virtio_pci_cap64(&mut read_word, 0xec, 24, low.clone(), 0).is_none());
        assert!(decode_virtio_pci_cap64(&mut read_word, 0x40, 23, low, 0).is_none());
    }

    #[test]
    fn notify_index_rejects_overflow_and_out_of_range_writes() {
        assert_eq!(notify_index(3, 2, 4), Some(3));
        assert_eq!(notify_index(4, 2, 4), None);
        assert_eq!(notify_index(1, 3, 4), None);
        assert_eq!(notify_index(u16::MAX, u32::MAX, 4), None);
    }
}
