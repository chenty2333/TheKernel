#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
use alloc::collections::{BTreeMap, BTreeSet};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axdriver_pci::{
    BarInfo, Cam, Command, DeviceFunction, DeviceFunctionInfo, HeaderType, MemoryBarType,
    PciRangeAllocator, PciRoot,
};
use axhal::mem::{PAGE_SIZE_4K, PhysAddr, VirtAddr};
#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
use axsync::Mutex;
#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
use lazyinit::LazyInit;

use crate::{AllDevices, drivers::BusProbeResult, prelude::*};

#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
use crate::AxInputDevice;

const PCI_BAR_NUM: u8 = 6;
/// ECAM address per PCI bus: one 1 MiB region of 32 devices x 8 functions x
/// 4 KiB, so a bus number becomes a byte offset by shifting left by this much.
/// Linux spells the same fact `PCI_MMCFG_BUS_OFFSET`
/// (arch/x86/include/asm/pci_x86.h:193).
const PCI_ECAM_BUS_SHIFT: u32 = 20;
/// The Q35 topology is shallow, but discovery must bound hostile or malformed
/// bridge graphs independently from the ECAM's 256 possible bus numbers.
const MAX_REACHABLE_PCI_BUSES: usize = 64;

/// Visits every function reachable from root bus zero once.
///
/// A bridge contributes only its directly attached secondary bus. The
/// subordinate number validates that edge; it is not expanded as an assumed
/// contiguous bus range. This keeps hotplug discovery topology-driven while
/// bounding malformed bridge graphs.
fn walk_reachable_pci_functions(
    root: &mut PciRoot,
    mut visit: impl FnMut(&mut PciRoot, DeviceFunction, &DeviceFunctionInfo),
) {
    let bus_end = pci_scan_bus_end();
    let mut visited = [false; u8::MAX as usize + 1];
    let mut pending = [0_u8; MAX_REACHABLE_PCI_BUSES];
    let mut pending_len = 1;
    let mut next = 0;
    pending[0] = 0;
    visited[0] = true;

    while next < pending_len {
        let bus = pending[next];
        next += 1;

        for (bdf, info) in root.enumerate_bus(bus) {
            let bridge = info.header_type == HeaderType::PciPciBridge;
            visit(root, bdf, &info);

            if !bridge {
                continue;
            }
            let numbers = root.bridge_bus_numbers(bdf);
            let Some(secondary) = valid_bridge_secondary_bus(
                bus,
                numbers.primary,
                numbers.secondary,
                numbers.subordinate,
                bus_end,
            ) else {
                continue;
            };
            if visited[secondary as usize] {
                continue;
            }
            if pending_len == MAX_REACHABLE_PCI_BUSES {
                // A bus whose topology always fills the budget makes this true on
                // every walk, including the input reconcile's per-second one. It
                // describes the machine, not an event, so it is `dmesg` material.
                debug!(
                    "PCI reachable-bus budget ({MAX_REACHABLE_PCI_BUSES}) exhausted at {bdf}; stopping discovery"
                );
                return;
            }
            visited[secondary as usize] = true;
            pending[pending_len] = secondary;
            pending_len += 1;
        }
    }
}

/// Validates the one topology edge a Type-1 bridge may contribute.
const fn valid_bridge_secondary_bus(
    scanned_bus: u8,
    primary: u8,
    secondary: u8,
    subordinate: u8,
    bus_end: u8,
) -> Option<u8> {
    if primary != scanned_bus
        || secondary == 0
        || secondary > subordinate
        || secondary > bus_end
        || subordinate > bus_end
    {
        None
    } else {
        Some(secondary)
    }
}

/// Orderable, stable key for a PCI bus/device/function.  The upstream PCI
/// type used by this tree intentionally does not implement `Ord`, while the
/// registry needs an allocation-free ordered map in `no_std`.
#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PciBdf {
    bus: u8,
    device: u8,
    function: u8,
}

#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
impl From<DeviceFunction> for PciBdf {
    fn from(value: DeviceFunction) -> Self {
        Self {
            bus: value.bus,
            device: value.device,
            function: value.function,
        }
    }
}

#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
impl From<PciBdf> for DeviceFunction {
    fn from(value: PciBdf) -> Self {
        Self {
            bus: value.bus,
            device: value.device,
            function: value.function,
        }
    }
}

/// One BDF-keyed PCI owner.  Only VirtIO-input devices are retained here:
/// their event-node owner lives in `axinput`, so removal is represented by its
/// stable registration token instead of a transient eventN minor.
#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
enum ManagedPciDevice {
    BootPending {
        device: AxInputDevice,
        identity: crate::InputBusIdentity,
    },
    ActiveInput {
        token: u64,
    },
    /// A driver has been probed and is being delivered to axinput outside the
    /// registry lock.  Reserving the BDF prevents a concurrent scan from
    /// constructing a second transport for the same PCI function.
    Registering,
    /// Boot input publication is in progress.  A concurrent scan may observe
    /// removal before its callback returns; retain that fact so activation can
    /// revoke the returned token immediately.
    BootRegistering {
        removal_observed: bool,
    },
    /// Removal has been handed to axinput outside the registry lock.  Retain
    /// the BDF until revocation completes so a rapid re-add is observed by a
    /// later scan rather than racing the old event-node teardown.
    Removing {
        token: u64,
    },
}

#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BootRegistrationAction {
    Activate,
    Revoke,
}

#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
const fn boot_registration_action(removal_observed: bool) -> BootRegistrationAction {
    if removal_observed {
        BootRegistrationAction::Revoke
    } else {
        BootRegistrationAction::Activate
    }
}

#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
fn input_identity(
    bdf: DeviceFunction,
    info: &axdriver_pci::DeviceFunctionInfo,
) -> crate::InputBusIdentity {
    crate::InputBusIdentity {
        domain: 0,
        bus: bdf.bus,
        device: bdf.device,
        function: bdf.function,
        vendor_id: info.vendor_id,
        device_id: info.device_id,
        virtio_index: ((bdf.bus as u32) << 8) | ((bdf.device as u32) << 3) | bdf.function as u32,
    }
}

/// PCI discovery state retained after boot.  `PciRoot` is cheaply rebuilt for
/// each bounded scan from ECAM; the allocator and BDF map keep their identity
/// across scans.
#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
struct BusDeviceRegistry {
    allocator: Option<PciRangeAllocator>,
    devices: BTreeMap<PciBdf, ManagedPciDevice>,
}

#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
impl BusDeviceRegistry {
    fn new() -> Self {
        Self {
            allocator: axconfig::devices::PCI_RANGES
                .get(1)
                .map(|range| PciRangeAllocator::new(range.0 as u64, range.1 as u64)),
            devices: BTreeMap::new(),
        }
    }
}

#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
static PCI_DEVICE_REGISTRY: LazyInit<Mutex<BusDeviceRegistry>> = LazyInit::new();

#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
fn input_registry() -> &'static Mutex<BusDeviceRegistry> {
    PCI_DEVICE_REGISTRY
        .get()
        .expect("PCI device registry not initialized")
}

/// Highest bus number the ECAM walk may reach.
///
/// Two independent bounds apply: `[devices] pci-bus-end` says how far this
/// kernel is willing to look, and the ECAM region says how far configuration
/// space is actually decoded.  Linux honors the second one literally --
/// `pci_mmconfig_lookup` (arch/x86/pci/mmconfig-shared.c:119-129) answers
/// `NULL` for a bus outside the region's `[start_bus, end_bus]`, so a read
/// there reports an absent device instead of forming an address past the
/// window.  Walking beyond it would read memory that is not configuration
/// space at all.
const fn ecam_scan_bus_end(configured: u8, ecam_end: u8) -> u8 {
    if ecam_end < configured {
        ecam_end
    } else {
        configured
    }
}

/// The walk bound for the ECAM region the platform published.
fn pci_scan_bus_end() -> u8 {
    ecam_scan_bus_end(
        axconfig::devices::PCI_BUS_END as u8,
        axhal::pci::ecam_bus_range().1,
    )
}

/// Virtual base of the mapped ECAM window; zero until [`ecam_window`] runs.
///
/// [`axhal::mem::phys_to_virt`] is arithmetic on the direct-map offset: it
/// names where a physical address *would* appear, not whether the kernel page
/// table maps it, and that table is built once from the compile-time
/// `[devices] mmio-ranges` list.  So an ECAM base that firmware reports outside
/// that list has no mapping, and the first configuration read faults before any
/// driver exists to explain itself -- which on real hardware looks exactly like
/// a dead console.  Linux maps the MCFG-declared span rather than assuming it
/// is reachable (`mcfg_ioremap`, arch/x86/pci/mmconfig_64.c:99-112), and when
/// that fails it leaves `raw_pci_ext_ops` unset (`pci_mmcfg_arch_init`,
/// :133-146) so nothing touches the window.  This kernel has ECAM only, with no
/// port-I/O fallback to retreat to, so a failure here ends the PCI scan.
static ECAM_WINDOW: AtomicUsize = AtomicUsize::new(0);
/// Whether the machine has already been told that ECAM cannot be mapped. The
/// mapping is retried (a refusal is not permanent), this is not: see
/// [`ecam_window`].
static ECAM_REFUSAL_REPORTED: AtomicBool = AtomicBool::new(false);

/// Establish the ECAM mapping once, then report it.
fn ecam_window() -> Option<VirtAddr> {
    let mapped = ECAM_WINDOW.load(Ordering::Acquire);
    if mapped != 0 {
        return Some(VirtAddr::from(mapped));
    }

    // The platform accepts only an MCFG region that starts at bus 0
    // (`select_pci_ecam`), and the configured fallback starts there too, so the
    // region base is also bus 0's base and `PciRoot` can index it directly.
    let (bus_begin, bus_end) = axhal::pci::ecam_bus_range();
    let base = axhal::pci::ecam_base();
    let size = (bus_end.saturating_sub(bus_begin) as usize + 1) << PCI_ECAM_BUS_SHIFT;
    let Some(window_end) = base.checked_add(size) else {
        if !ECAM_REFUSAL_REPORTED.swap(true, Ordering::AcqRel) {
            error!(
                "pci-ecam: region at {base:#x} for {size:#x} bytes overflows the address space; \
                 PCI configuration space is unreachable"
            );
        }
        return None;
    };

    match axklib::mem::iomap(PhysAddr::from_usize(base), size) {
        Ok(virt) => {
            info!(
                "pci-ecam: mapped [{base:#x}, {window_end:#x}) at {virt:#x} for buses \
                 {bus_begin}..={bus_end}"
            );
            ECAM_WINDOW.store(virt.as_usize(), Ordering::Release);
            Some(virt)
        }
        Err(error) => {
            // Retried on a timer by the PCI input reconcile worker, so the
            // refusal is news once and not news once a second: an unthrottled
            // `error!` on a timer both floods the console and is the fastest way
            // this kernel has of erasing its own boot ring. The attempt itself
            // still repeats, because a refused mapping is not a refused mapping
            // forever -- this latch speaks only about who gets told.
            if !ECAM_REFUSAL_REPORTED.swap(true, Ordering::AcqRel) {
                error!(
                    "pci-ecam: cannot map [{base:#x}, {window_end:#x}) for buses {bus_begin}..=\
                     {bus_end}: {error:?}; PCI configuration space is unreachable, so no bus \
                     scan runs. A repeated refusal is not reported again."
                );
            }
            None
        }
    }
}

fn pci_root() -> Option<PciRoot> {
    // The ECAM base is a machine fact: the platform discovers it from the
    // firmware's MCFG table during early initialization and falls back to
    // `[devices] pci-ecam-base` only when firmware supplies nothing usable.
    // Asking the platform is what makes this kernel bootable on hardware whose
    // ECAM base differs from the configured value.
    let base = ecam_window()?;
    // SAFETY: `ecam_window` mapped this exact virtual range as device memory,
    // covering every bus `pci_scan_bus_end` admits.
    Some(unsafe { PciRoot::new(base.as_mut_ptr(), Cam::Ecam) })
}

#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
fn pci_snapshot(
    root: &mut PciRoot,
) -> BTreeMap<PciBdf, (DeviceFunction, axdriver_pci::DeviceFunctionInfo)> {
    let mut snapshot = BTreeMap::new();
    walk_reachable_pci_functions(root, |_root, bdf, info| {
        if info.header_type == HeaderType::Standard {
            snapshot.insert(PciBdf::from(bdf), (bdf, info.clone()));
        }
    });
    snapshot
}

#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
fn probe_virtio_input(
    root: &mut PciRoot,
    bdf: DeviceFunction,
    dev_info: &axdriver_pci::DeviceFunctionInfo,
) -> Option<AxInputDevice> {
    use crate::drivers::DriverProbe;
    use crate::virtio::{VirtIoDevMeta, VirtIoInput};

    match <VirtIoInput as VirtIoDevMeta>::Driver::probe_pci(root, bdf, dev_info) {
        BusProbeResult::Device(crate::AxDeviceEnum::Input(device)) => Some(device),
        BusProbeResult::NotMatched | BusProbeResult::Claimed => None,
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
fn quiesce_pci_function(root: &mut PciRoot, bdf: DeviceFunction) {
    // Stop DMA and mask INTx before transferring the removal to axinput.  The
    // VirtIO input owner then drops its queues and resets the transport while
    // tearing down the event node.
    let (_status, command) = root.get_status_command(bdf);
    root.set_command(
        bdf,
        (command & !(Command::IO_SPACE | Command::MEMORY_SPACE | Command::BUS_MASTER))
            | Command::INTERRUPT_DISABLE,
    );
}

/// Return the smallest page-aligned physical range containing one PCI memory
/// BAR. Invalid or unrepresentable BARs are rejected before they can reach the
/// MMIO mapper.
fn memory_bar_mapping_range(address: u64, size: u64) -> Option<(PhysAddr, usize)> {
    if address == 0 || size == 0 {
        return None;
    }

    let start = usize::try_from(address).ok()?;
    let end = usize::try_from(address.checked_add(size)?).ok()?;
    let aligned_start = start & !(PAGE_SIZE_4K - 1);
    let aligned_end = end.checked_add(PAGE_SIZE_4K - 1)? & !(PAGE_SIZE_4K - 1);
    let aligned_size = aligned_end.checked_sub(aligned_start)?;
    if aligned_size == 0 {
        return None;
    }
    Some((PhysAddr::from_usize(aligned_start), aligned_size))
}

fn map_memory_bar(address: u64, size: u64) -> DevResult {
    let (start, size) = memory_bar_mapping_range(address, size).ok_or(DevError::InvalidParam)?;
    axklib::mem::iomap(start, size).map_err(|error| {
        warn!(
            "failed to map PCI memory BAR [{:#x}, {:#x}): {:?}",
            start.as_usize(),
            start.as_usize() + size,
            error
        );
        DevError::Io
    })?;
    Ok(())
}

fn config_pci_device(
    root: &mut PciRoot,
    bdf: DeviceFunction,
    allocator: &mut Option<PciRangeAllocator>,
) -> DevResult {
    let mut bar = 0;
    while bar < PCI_BAR_NUM {
        let info = root.bar_info(bdf, bar).unwrap();
        if let BarInfo::Memory {
            address_type,
            address,
            size,
            ..
        } = info
        {
            // if the BAR address is not assigned, call the allocator and assign it.
            if size > 0 && address == 0 {
                let new_addr = allocator
                    .as_mut()
                    .expect("No memory ranges available for PCI BARs!")
                    .alloc(size as _)
                    .ok_or(DevError::NoMemory)?;
                if address_type == MemoryBarType::Width32 {
                    root.set_bar_32(bdf, bar, new_addr as _);
                } else if address_type == MemoryBarType::Width64 {
                    root.set_bar_64(bdf, bar, new_addr);
                }
            }
        }

        // read the BAR info again after assignment.
        let info = root.bar_info(bdf, bar).unwrap();
        match info {
            BarInfo::IO { address, size } => {
                if address > 0 && size > 0 {
                    let end = address.checked_add(size).ok_or(DevError::InvalidParam)?;
                    debug!("  BAR {}: IO  [{:#x}, {:#x})", bar, address, end);
                }
            }
            BarInfo::Memory {
                address_type,
                prefetchable,
                address,
                size,
            } => {
                if address > 0 && size > 0 {
                    let end = address
                        .checked_add(size as u64)
                        .ok_or(DevError::InvalidParam)?;
                    debug!(
                        "  BAR {}: MEM [{:#x}, {:#x}){}{}",
                        bar,
                        address,
                        end,
                        if address_type == MemoryBarType::Width64 {
                            " 64bit"
                        } else {
                            ""
                        },
                        if prefetchable { " pref" } else { "" },
                    );
                    map_memory_bar(address, size as u64)?;
                }
            }
        }

        bar += 1;
        if info.takes_two_entries() {
            bar += 1;
        }
    }

    // Enable resource access, but leave INTx masked until a driver admits an
    // acknowledgment owner. Polling-only devices must not hold shared lines.
    let (_status, cmd) = root.get_status_command(bdf);
    root.set_command(
        bdf,
        cmd | Command::IO_SPACE | Command::MEMORY_SPACE | Command::BUS_MASTER
            | Command::INTERRUPT_DISABLE,
    );
    Ok(())
}

impl AllDevices {
    pub(crate) fn probe_bus_devices(&mut self) {
        #[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
        PCI_DEVICE_REGISTRY.init_once(Mutex::new(BusDeviceRegistry::new()));

        let Some(mut root) = pci_root() else {
            // `ecam_window` has said why configuration space is unreachable.
            // The registry above still exists, because the input paths that use
            // it must not panic over a machine with no reachable PCI.
            return;
        };

        // PCI 32-bit MMIO space
        let mut allocator = axconfig::devices::PCI_RANGES
            .get(1)
            .map(|range| PciRangeAllocator::new(range.0 as u64, range.1 as u64));

        #[cfg(feature = "usb-xhci")]
        let mut usb_devices = alloc::vec::Vec::new();
        walk_reachable_pci_functions(&mut root, |root, bdf, dev_info| {
            debug!("PCI {bdf}: {dev_info}");
            if dev_info.header_type != HeaderType::Standard {
                return;
            }
            match config_pci_device(root, bdf, &mut allocator) {
                Ok(_) => {
                    #[cfg(feature = "usb-xhci")]
                    if dev_info.class == 0x0c
                        && dev_info.subclass == 0x03
                        && dev_info.prog_if == 0x30
                    {
                        if let Ok(BarInfo::Memory { address, .. }) = root.bar_info(bdf, 0) {
                            let mmio =
                                axhal::mem::phys_to_virt((address as usize).into()).as_mut_ptr();
                            if let Some(mmio) = core::ptr::NonNull::new(mmio) {
                                match crate::usb::probe(mmio) {
                                    Ok(devices) => {
                                        for device in devices {
                                            usb_devices.push(device);
                                        }
                                    }
                                    Err(error) => warn!("USB xHCI at {bdf} failed: {error:?}"),
                                }
                            }
                        }
                        return;
                    }
                    for_each_drivers!(type Driver, {
                    match Driver::probe_pci(root, bdf, dev_info) {
                        BusProbeResult::NotMatched => {}
                        BusProbeResult::Claimed => return,
                        BusProbeResult::Device(dev) => {
                            #[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
                            match dev {
                                crate::AxDeviceEnum::Input(input) => {
                                    // The BDF registry owns discovery identity.  `axinput`
                                    // obtains the driver only after it has installed its
                                    // listener, and returns a stable token for remove.
                                    input_registry().lock().devices.insert(
                                        PciBdf::from(bdf),
                                        ManagedPciDevice::BootPending {
                                            device: input,
                                            identity: input_identity(bdf, dev_info),
                                        },
                                    );
                                    return;
                                }
                                dev => {
                                    info!(
                                        "registered a new {:?} device at {}: {:?}",
                                        dev.device_type(),
                                        bdf,
                                        dev.device_name(),
                                    );
                                    self.add_device(dev);
                                    return;
                                }
                            }

                            #[cfg(not(all(not(feature = "dyn"), input_dev = "virtio-input")))]
                            {
                                info!(
                                    "registered a new {:?} device at {}: {:?}",
                                    dev.device_type(),
                                    bdf,
                                    dev.device_name(),
                                );
                                self.add_device(dev);
                                return;
                            }
                        }
                    }
                    });
                }
                Err(e) => warn!("failed to enable PCI device at {bdf}({dev_info}): {e:?}"),
            }
        });
        // USB is auxiliary storage; preserve the existing root disk ordering
        // even when the xHCI function precedes VirtIO block on the PCI bus.
        #[cfg(feature = "usb-xhci")]
        for device in usb_devices { self.add_device(device); }

        // The igc driver's negative case, printed once so that a machine
        // without the assumed part says so in one greppable line.
        #[cfg(net_dev = "igc")]
        crate::igc::finish_probe(pci_scan_bus_end());
    }
}

/// Reconcile the bounded Q35 PCI topology and transfer just VirtIO-input
/// additions/removals to the input owner.  PCI BDF is the identity; eventN
/// remains a reusable devfs name and is never used to identify a device.
///
/// PCI functions outside the VirtIO-input class are deliberately left
/// boot-only.  Their upper layers do not yet offer an equivalent safe removal
/// protocol, so reporting a generic PCI hotplug capability would be false.
#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
pub(crate) fn reconcile_input_devices<Register, Unregister>(
    mut register: Register,
    mut unregister: Unregister,
) where
    Register: FnMut(AxInputDevice, crate::InputBusIdentity) -> u64,
    Unregister: FnMut(u64),
{
    let Some(mut root) = pci_root() else {
        // The mapping is established once and never torn down, so reaching here
        // without it means boot never enumerated a PCI function either, and
        // there is no registered device to remove.
        return;
    };
    let snapshot = pci_snapshot(&mut root);

    let (pending, removals) = {
        let mut registry = input_registry().lock();

        // A removal is terminal. Quiesce PCI first, then let axinput revoke
        // old file descriptions. Dropping the driver from that callback
        // clears VirtIO queues and resets its transport. A remove/re-add that
        // preserves one BDF entirely between scans is not observable through
        // PCI presence alone; it requires a platform hotplug notification or
        // a non-owning VirtIO status accessor.
        let absent = registry
            .devices
            .keys()
            .filter(|bdf| !snapshot.contains_key(bdf))
            .copied()
            .collect::<BTreeSet<_>>();
        let mut removals = alloc::vec::Vec::new();
        for key in absent {
            let bdf = DeviceFunction::from(key);
            quiesce_pci_function(&mut root, bdf);
            match registry.devices.remove(&key) {
                Some(ManagedPciDevice::ActiveInput { token }) => {
                    registry
                        .devices
                        .insert(key, ManagedPciDevice::Removing { token });
                    removals.push((key, token));
                }
                Some(ManagedPciDevice::BootPending { device, .. }) => drop(device),
                // The registering caller owns the device outside this lock.
                // Removing its reservation makes that caller revoke the
                // just-published token instead of creating a stale owner.
                Some(ManagedPciDevice::Registering) => {}
                Some(ManagedPciDevice::BootRegistering {
                    removal_observed: _,
                }) => {
                    registry.devices.insert(
                        key,
                        ManagedPciDevice::BootRegistering {
                            removal_observed: true,
                        },
                    );
                }
                // Another reconcile has already begun the external removal.
                // Keep its reservation until that callback completes.
                Some(state @ ManagedPciDevice::Removing { .. }) => {
                    registry.devices.insert(key, state);
                }
                None => {}
            }
            info!("removed PCI VirtIO-input device at {bdf}");
        }

        // Probe only BDFs that have never been owned.  This makes reconcile
        // idempotent and prevents reinitializing a live transport on each
        // hotplug scan.
        let additions = snapshot
            .iter()
            // This registry owns input functions only. Reconfiguring a boot
            // block/NIC during an input scan would mask its live INTx gate.
            .filter(|(_, (_, info))| {
                axdriver_virtio::pci_device_type(info) == Some(DeviceType::Input)
            })
            .filter(|(bdf, _)| !registry.devices.contains_key(bdf))
            .map(|(key, (bdf, info))| (*key, *bdf, info.clone()))
            .collect::<alloc::vec::Vec<_>>();
        let mut pending = alloc::vec::Vec::new();
        for (key, bdf, info) in additions {
            if config_pci_device(&mut root, bdf, &mut registry.allocator).is_err() {
                // A function that will not configure stays "never owned", so the
                // next tick retries it and would report it again: once a second,
                // forever, at a level no filter caps. The debug band is what says
                // "this is a fault, and it is also routine"; the `\x014` keeps it a
                // warning for whoever opened that band, and the window budget
                // bounds it there.
                debug!("\x014failed to configure hotplugged PCI function at {bdf}");
                continue;
            }
            if let Some(device) = probe_virtio_input(&mut root, bdf, &info) {
                // Reserve the BDF before releasing the registry lock for the
                // axinput callback below.  A concurrent scan must observe
                // this state rather than creating a second transport.
                registry.devices.insert(key, ManagedPciDevice::Registering);
                pending.push((key, bdf, input_identity(bdf, &info), device));
            }
        }
        (pending, removals)
    };

    // No external callbacks under the registry lock: input publication can
    // synchronously create devfs/sysfs nodes and acquire unrelated locks.
    for (key, token) in removals {
        unregister(token);
        let mut registry = input_registry().lock();
        if matches!(
            registry.devices.get(&key),
            Some(ManagedPciDevice::Removing { token: current }) if *current == token
        ) {
            registry.devices.remove(&key);
        }
    }
    for (key, bdf, identity, device) in pending {
        let token = register(device, identity);
        let mut registry = input_registry().lock();
        if matches!(
            registry.devices.get(&key),
            Some(ManagedPciDevice::Registering)
        ) {
            registry
                .devices
                .insert(key, ManagedPciDevice::ActiveInput { token });
            info!("registered hotplugged PCI VirtIO-input device at {bdf}");
        } else {
            // The function vanished while its registration callback ran.
            // Revoke the token exactly once; no registry state owns it.
            drop(registry);
            unregister(token);
            warn!("discarded stale PCI VirtIO-input registration at {bdf}");
        }
    }
}

/// Transfers boot-discovered PCI input devices after `axinput` has installed
/// its listener.  This is the same BDF registry used by runtime reconcile.
#[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
pub(crate) fn activate_boot_input_devices<Register, Unregister>(
    mut register: Register,
    mut unregister: Unregister,
) where
    Register: FnMut(AxInputDevice, crate::InputBusIdentity) -> u64,
    Unregister: FnMut(u64),
{
    let pending = {
        let mut registry = input_registry().lock();
        let bdfs = registry
            .devices
            .iter()
            .filter_map(|(bdf, state)| {
                matches!(state, ManagedPciDevice::BootPending { .. }).then_some(*bdf)
            })
            .collect::<alloc::vec::Vec<_>>();
        bdfs.into_iter()
            .filter_map(|bdf| match registry.devices.remove(&bdf) {
                Some(ManagedPciDevice::BootPending { device, identity }) => {
                    registry.devices.insert(
                        bdf,
                        ManagedPciDevice::BootRegistering {
                            removal_observed: false,
                        },
                    );
                    Some((bdf, identity, device))
                }
                Some(other) => {
                    registry.devices.insert(bdf, other);
                    None
                }
                None => None,
            })
            .collect::<alloc::vec::Vec<_>>()
    };
    for (bdf, identity, device) in pending {
        let token = register(device, identity);
        let mut registry = input_registry().lock();
        match registry.devices.remove(&bdf) {
            Some(ManagedPciDevice::BootRegistering { removal_observed }) => {
                match boot_registration_action(removal_observed) {
                    BootRegistrationAction::Activate => {
                        registry
                            .devices
                            .insert(bdf, ManagedPciDevice::ActiveInput { token });
                    }
                    BootRegistrationAction::Revoke => {
                        drop(registry);
                        unregister(token);
                    }
                }
            }
            Some(state) => {
                registry.devices.insert(bdf, state);
                drop(registry);
                unregister(token);
            }
            None => {
                drop(registry);
                unregister(token);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ecam_scan_bus_end, memory_bar_mapping_range, valid_bridge_secondary_bus};
    use axdriver_pci::BridgeBusNumbers;

    #[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
    use super::{BootRegistrationAction, boot_registration_action};

    #[cfg(all(not(feature = "dyn"), input_dev = "virtio-input"))]
    #[test]
    fn boot_registration_revokes_when_removal_was_observed() {
        assert_eq!(
            boot_registration_action(false),
            BootRegistrationAction::Activate
        );
        assert_eq!(
            boot_registration_action(true),
            BootRegistrationAction::Revoke
        );
    }

    #[test]
    fn memory_bar_range_is_page_exact() {
        let (start, size) = memory_bar_mapping_range(0x3800_0000_0123, 0x2345).unwrap();
        assert_eq!(start.as_usize(), 0x3800_0000_0000);
        assert_eq!(size, 0x3000);
    }

    #[test]
    fn memory_bar_range_preserves_aligned_bounds() {
        let (start, size) = memory_bar_mapping_range(0xc000_0000_00, 0x1000).unwrap();
        assert_eq!(start.as_usize(), 0xc000_0000_00);
        assert_eq!(size, 0x1000);
    }

    #[test]
    fn memory_bar_range_rejects_empty_and_overflow() {
        assert!(memory_bar_mapping_range(0, 0x1000).is_none());
        assert!(memory_bar_mapping_range(0x1000, 0).is_none());
        assert!(memory_bar_mapping_range(u64::MAX - 0x7ff, 0x1000).is_none());
        assert!(memory_bar_mapping_range(u64::MAX - 0xfff, 0xfff).is_none());
    }

    #[test]
    fn scan_bus_end_takes_the_tighter_of_two_bounds() {
        // The configured fallback always describes buses 0-255, so a profile
        // that scans to 0xff is unchanged by it.
        assert_eq!(ecam_scan_bus_end(0xff, 0xff), 0xff);
        // A narrow MCFG window ends the walk where firmware stopped decoding,
        // even though `pci-bus-end` would allow more.
        assert_eq!(ecam_scan_bus_end(0xff, 0x7f), 0x7f);
        // A profile may still ask for less than the window covers.
        assert_eq!(ecam_scan_bus_end(0x1f, 0xff), 0x1f);
    }

    fn reachable_buses<const MAX: usize>(
        bus_end: u8,
        bridges: &[(u8, BridgeBusNumbers)],
    ) -> ([u8; MAX], usize, bool) {
        let mut visited = [false; u8::MAX as usize + 1];
        let mut buses = [0; MAX];
        let mut count = 1;
        let mut next = 0;
        buses[0] = 0;
        visited[0] = true;

        while next < count {
            let bus = buses[next];
            next += 1;
            for &(bridge_bus, numbers) in bridges {
                if bridge_bus != bus {
                    continue;
                }
                let Some(secondary) = valid_bridge_secondary_bus(
                    bus,
                    numbers.primary,
                    numbers.secondary,
                    numbers.subordinate,
                    bus_end,
                ) else {
                    continue;
                };
                if visited[secondary as usize] {
                    continue;
                }
                if count == MAX {
                    return (buses, count, true);
                }
                visited[secondary as usize] = true;
                buses[count] = secondary;
                count += 1;
            }
        }
        (buses, count, false)
    }

    #[test]
    fn reachable_bus_walk_follows_bridge_chain_once() {
        let bridges = [
            (
                0,
                BridgeBusNumbers {
                    primary: 0,
                    secondary: 1,
                    subordinate: 2,
                },
            ),
            (
                1,
                BridgeBusNumbers {
                    primary: 1,
                    secondary: 2,
                    subordinate: 2,
                },
            ),
        ];
        let (buses, count, exhausted) = reachable_buses::<8>(0xff, &bridges);
        assert_eq!(&buses[..count], &[0, 1, 2]);
        assert!(!exhausted);
    }

    #[test]
    fn reachable_bus_walk_rejects_invalid_duplicate_and_cycle_edges() {
        let bridges = [
            (
                0,
                BridgeBusNumbers {
                    primary: 0,
                    secondary: 1,
                    subordinate: 3,
                },
            ),
            (
                0,
                BridgeBusNumbers {
                    primary: 0,
                    secondary: 1,
                    subordinate: 3,
                },
            ),
            (
                1,
                BridgeBusNumbers {
                    primary: 1,
                    secondary: 2,
                    subordinate: 3,
                },
            ),
            (
                2,
                BridgeBusNumbers {
                    primary: 2,
                    secondary: 1,
                    subordinate: 3,
                },
            ),
            (
                0,
                BridgeBusNumbers {
                    primary: 7,
                    secondary: 3,
                    subordinate: 3,
                },
            ),
            (
                1,
                BridgeBusNumbers {
                    primary: 1,
                    secondary: 0,
                    subordinate: 3,
                },
            ),
            (
                2,
                BridgeBusNumbers {
                    primary: 2,
                    secondary: 4,
                    subordinate: 3,
                },
            ),
        ];
        let (buses, count, exhausted) = reachable_buses::<8>(3, &bridges);
        assert_eq!(&buses[..count], &[0, 1, 2]);
        assert!(!exhausted);
    }

    #[test]
    fn reachable_bus_walk_stops_at_budget() {
        let bridges = [
            (
                0,
                BridgeBusNumbers {
                    primary: 0,
                    secondary: 1,
                    subordinate: 2,
                },
            ),
            (
                0,
                BridgeBusNumbers {
                    primary: 0,
                    secondary: 2,
                    subordinate: 2,
                },
            ),
        ];
        let (buses, count, exhausted) = reachable_buses::<2>(2, &bridges);
        assert_eq!(&buses[..count], &[0, 1]);
        assert!(exhausted);
    }
}
