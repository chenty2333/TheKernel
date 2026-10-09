//! PCI xHCI host integration. Class drivers use the existing block/evdev APIs.
mod bluetooth;
mod dma;
mod hid;
pub mod observations;
mod root_partition;
mod storage;
mod sync;

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc, vec::Vec};
use core::{
    future::Future,
    pin::pin,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, Waker},
    time::Duration,
};

use axdriver_base::{DevError, DevResult};
pub use bluetooth::{UsbBluetoothHci, bluetooth_devices};
use crab_usb::{
    DmaCoherency, EventHandler, USBHost,
    device::{Device, InterfaceSession, ProbeChanges, ProbedDevice},
    usb_if::{
        descriptor::InterfaceDescriptor,
        endpoint::{RequestId, TransferCompletion, TransferRequest, TransferStatus},
        host::ControlSetup,
        transfer::{Recipient, Request, RequestType},
    },
};
pub use hid::UsbInput;
use spin::Mutex;
pub use storage::UsbBlock;

use crate::{AxDeviceEnum, AxInputDevice, InputBusIdentity, UsbInputIdentity};

static USB_CONTROLLERS: Mutex<Vec<Arc<UsbController>>> = Mutex::new(Vec::new());
static USB_INPUT_READY: AtomicBool = AtomicBool::new(false);

pub(super) struct Host {
    events: kspin::SpinNoIrq<EventHandler>,
    operational: usize,
    running: kspin::SpinNoIrq<bool>,
}

impl Host {
    // Every submission/future poll shares this gate with halt. Waiters cannot
    // drop DMA-owned buffers until the halting CPU has observed HCHalted.
    fn with_running<T>(&self, operation: impl FnOnce() -> T) -> DevResult<T> {
        let running = self.running.lock();
        if !*running {
            return Err(DevError::Io);
        }
        Ok(operation())
    }

    fn pump(&self) -> DevResult {
        self.with_running(|| {
            self.events.lock().handle_event();
        })
    }

    fn submit(
        &self,
        ep: &crab_usb::EndpointHandle,
        request: TransferRequest,
    ) -> DevResult<RequestId> {
        self.with_running(|| ep.submit(request))?
            .map_err(|_| DevError::Io)
    }

    fn reclaim(
        &self,
        ep: &crab_usb::EndpointHandle,
        id: RequestId,
    ) -> DevResult<Option<TransferCompletion>> {
        self.with_running(|| ep.reclaim(id))?
            .map_err(|_| DevError::Io)
    }

    // Futures in CrabUSB do not cancel their requests on drop. Halt DMA before
    // allowing a timed-out future (and its caller-owned buffers) to be dropped.
    fn try_halt(&self) -> bool {
        let mut running = self.running.lock();
        if !*running {
            return true;
        }
        unsafe {
            let command = self.operational as *mut u32;
            command.write_volatile(command.read_volatile() & !1);
            let status = (self.operational + 4) as *const u32;
            let end = axhal::time::monotonic_time() + Duration::from_secs(1);
            while status.read_volatile() & 1 == 0 {
                if axhal::time::monotonic_time() >= end {
                    warn!("USB xHCI did not halt; retaining DMA-backed USB owners");
                    return false;
                }
                core::hint::spin_loop();
            }
        }
        *running = false;
        true
    }

    fn halt(&self) {
        assert!(self.try_halt(), "xHCI failed to halt DMA");
    }

    fn wait<F: Future>(&self, future: F) -> DevResult<F::Output> {
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        let end = axhal::time::monotonic_time() + Duration::from_secs(10);
        loop {
            let poll = self.with_running(|| {
                self.events.lock().handle_event();
                future.as_mut().poll(&mut context)
            })?;
            if let Poll::Ready(value) = poll {
                return Ok(value);
            }
            if axhal::time::monotonic_time() >= end {
                self.halt();
                warn!("USB xHCI request timed out; controller halted");
                return Err(DevError::Io);
            }
            core::hint::spin_loop();
        }
    }

    fn transfer(
        &self,
        ep: &crab_usb::EndpointHandle,
        request: TransferRequest,
    ) -> DevResult<usize> {
        let completion = self.wait(ep.wait(request))?.map_err(|_| DevError::Io)?;
        if completion.status != TransferStatus::Completed {
            return Err(DevError::Io);
        }
        Ok(completion.actual_length)
    }
}

struct ProbeGuard<'a> {
    host: &'a Host,
    armed: bool,
}
impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.host.halt();
        }
    }
}

struct DeviceOwner {
    _device: Arc<Mutex<Device>>,
    _session: InterfaceSession,
    dma_quiesced: Arc<AtomicBool>,
}

struct UsbController {
    backend: Mutex<USBHost>,
    host: Arc<Host>,
    bus: u8,
    devices: Mutex<BTreeMap<usize, ManagedUsbDevice>>,
    scan_active: AtomicBool,
    poisoned: AtomicBool,
}

struct ManagedUsbDevice {
    owner: Arc<Mutex<Device>>,
    dma_quiesced: Arc<AtomicBool>,
    interfaces: BTreeMap<u8, ManagedUsbInterface>,
}

enum ManagedUsbInterface {
    Pending {
        input: UsbInput,
        identity: UsbInputIdentity,
    },
    Registering,
    Active {
        token: u64,
    },
}

struct ScanGuard<'a>(&'a AtomicBool);

impl ScanGuard<'_> {
    fn try_acquire(active: &AtomicBool) -> Option<ScanGuard<'_>> {
        active
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| ScanGuard(active))
    }
}

impl Drop for ScanGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl UsbController {
    fn register_pending<Register, Unregister>(
        &self,
        register: &mut Register,
        unregister: &mut Unregister,
    ) where
        Register: FnMut(AxInputDevice, InputBusIdentity) -> u64,
        Unregister: FnMut(u64),
    {
        let pending = {
            let mut devices = self.devices.lock();
            let mut pending = Vec::new();
            for (device_id, device) in devices.iter_mut() {
                for (interface, state) in device.interfaces.iter_mut() {
                    let old = core::mem::replace(state, ManagedUsbInterface::Registering);
                    match old {
                        ManagedUsbInterface::Pending { input, identity } => {
                            pending.push((*device_id, *interface, input, identity));
                        }
                        other => *state = other,
                    }
                }
            }
            pending
        };
        for (device_id, interface, input, identity) in pending {
            let token = register(into_input_device(input), InputBusIdentity::Usb(identity));
            let mut devices = self.devices.lock();
            let state = devices
                .get_mut(&device_id)
                .and_then(|device| device.interfaces.get_mut(&interface));
            match state {
                Some(state @ ManagedUsbInterface::Registering) => {
                    *state = ManagedUsbInterface::Active { token };
                }
                _ => {
                    drop(devices);
                    unregister(token);
                }
            }
        }
    }

    fn unregister_all_quiesced<Unregister>(&self, unregister: &mut Unregister)
    where
        Unregister: FnMut(u64),
    {
        let devices = core::mem::take(&mut *self.devices.lock());
        let mut tokens = Vec::new();
        for (_, mut device) in devices {
            device.dma_quiesced.store(true, Ordering::Release);
            for (_, state) in core::mem::take(&mut device.interfaces) {
                if let ManagedUsbInterface::Active { token } = state {
                    tokens.push(token);
                }
            }
        }
        for token in tokens {
            unregister(token);
        }
        // `devices` is dropped only after the controller-wide HCHalted
        // boundary, so any still-pending HID request buffers are safe.
    }

    fn remove_device<Unregister>(&self, device_id: usize, unregister: &mut Unregister)
    where
        Unregister: FnMut(u64),
    {
        let Some(device) = self.devices.lock().remove(&device_id) else {
            return;
        };
        let disconnected = {
            let mut owner = device.owner.lock();
            self.host.wait(owner.disconnect())
        };
        match disconnected {
            Ok(Ok(())) => {
                device.dma_quiesced.store(true, Ordering::Release);
                let tokens = device
                    .interfaces
                    .values()
                    .filter_map(|state| match state {
                        ManagedUsbInterface::Active { token } => Some(*token),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                for token in tokens {
                    unregister(token);
                }
                // Pending drivers are dropped only after successful endpoint
                // stop/Disable Slot retirement.
            }
            _ => {
                self.poisoned.store(true, Ordering::Release);
                self.devices.lock().insert(device_id, device);
                if self.host.try_halt() {
                    // HCHalted fences every device on this controller. Keep
                    // all backing pinned until that boundary, then revoke the
                    // input registrations without freeing any active DMA.
                    self.unregister_all_quiesced(unregister);
                } else {
                    warn!(
                        "USB device {device_id} disconnect failed and xHCI did not halt; \
                         retaining input registrations and DMA owners"
                    );
                }
            }
        }
    }

    fn reconcile<Register, Unregister>(&self, register: &mut Register, unregister: &mut Unregister)
    where
        Register: FnMut(AxInputDevice, InputBusIdentity) -> u64,
        Unregister: FnMut(u64),
    {
        if self.poisoned.load(Ordering::Acquire) {
            return;
        }
        // Own the guard for the complete scan so a second notifier cannot
        // duplicate registration or race a disconnect with a pending HID add.
        let Some(_scan) = ScanGuard::try_acquire(&self.scan_active) else {
            return;
        };
        let changes = match self.host.wait(self.backend.lock().probe_devices()) {
            Ok(Ok(changes)) => changes,
            Ok(Err(error)) => {
                warn!("USB xHCI input rescan failed: {error:?}");
                return;
            }
            Err(error) => {
                warn!("USB xHCI input rescan stopped: {error:?}");
                self.poisoned.store(true, Ordering::Release);
                return;
            }
        };
        for device_id in changes.disconnected {
            self.remove_device(device_id, unregister);
            if self.poisoned.load(Ordering::Acquire) {
                return;
            }
        }
        let mut ignored = Vec::new();
        for probed in changes.connected {
            if self.devices.lock().contains_key(&probed.id()) {
                warn!("USB xHCI repeated active device identity {}", probed.id());
                continue;
            }
            if let Some((id, managed)) = process_connected(self, probed, false, &mut ignored) {
                self.devices.lock().insert(id, managed);
            }
        }
        if USB_INPUT_READY.load(Ordering::Acquire) {
            self.register_pending(register, unregister);
        }
    }
}

#[cfg(not(feature = "dyn"))]
fn into_input_device(input: UsbInput) -> AxInputDevice {
    AxInputDevice::Usb(input)
}

#[cfg(feature = "dyn")]
fn into_input_device(input: UsbInput) -> AxInputDevice {
    Box::new(input)
}

pub(crate) fn activate_boot_input_devices<Register, Unregister>(
    mut register: Register,
    mut unregister: Unregister,
) where
    Register: FnMut(AxInputDevice, InputBusIdentity) -> u64,
    Unregister: FnMut(u64),
{
    USB_INPUT_READY.store(true, Ordering::Release);
    let controllers = USB_CONTROLLERS.lock().clone();
    for controller in controllers {
        if let Some(_scan) = ScanGuard::try_acquire(&controller.scan_active) {
            controller.register_pending(&mut register, &mut unregister);
        }
    }
}

pub(crate) fn reconcile_input_devices<Register, Unregister>(
    mut register: Register,
    mut unregister: Unregister,
) where
    Register: FnMut(AxInputDevice, InputBusIdentity) -> u64,
    Unregister: FnMut(u64),
{
    if !USB_INPUT_READY.load(Ordering::Acquire) {
        return;
    }
    let controllers = USB_CONTROLLERS.lock().clone();
    for controller in controllers {
        controller.reconcile(&mut register, &mut unregister);
    }
}

fn class_control(
    host: &Host,
    device: &mut Device,
    interface: u8,
    request: u8,
    value: u16,
) -> DevResult {
    host.wait(device.control_out(
        ControlSetup {
            request_type: RequestType::Class,
            recipient: Recipient::Interface,
            request: Request::Other(request),
            value,
            index: interface as u16,
        },
        &[],
    ))?
    .map_err(|_| DevError::Io)?;
    Ok(())
}

/// Called only after the existing PCI enumerator has mapped/enabled BAR0.
fn supported_interface(interface: &InterfaceDescriptor) -> bool {
    interface.alternate_setting == 0
        && (is_bluetooth_hci(interface.class, interface.subclass, interface.protocol)
            || interface.class == 3
            || (interface.class == 8 && interface.subclass == 6 && interface.protocol == 0x50))
}

fn is_bluetooth_hci(class: u8, subclass: u8, protocol: u8) -> bool {
    class == 0xe0 && subclass == 1 && protocol == 1
}

fn input_device(input: UsbInput) -> AxDeviceEnum {
    #[cfg(not(feature = "dyn"))]
    return AxDeviceEnum::Input(AxInputDevice::Usb(input));
    #[cfg(feature = "dyn")]
    return AxDeviceEnum::from_input(input);
}

fn output_block(block: UsbBlock) -> AxDeviceEnum {
    #[cfg(not(feature = "dyn"))]
    return AxDeviceEnum::Block(crate::AxBlockDevice::Usb(block));
    #[cfg(feature = "dyn")]
    return AxDeviceEnum::from_block(block);
}

fn managed_hid_configuration(
    config: &crab_usb::usb_if::descriptor::ConfigurationDescriptor,
) -> bool {
    let mut has_hid = false;
    for interface in config
        .interfaces
        .iter()
        .flat_map(|group| &group.alt_settings)
        .filter(|interface| supported_interface(interface))
    {
        if interface.class == 3 {
            has_hid = true;
        } else {
            // This lifecycle can disconnect only devices whose other active
            // interfaces are not owned by a boot-only class driver.
            return false;
        }
    }
    has_hid
}

fn process_connected(
    controller: &UsbController,
    probed: ProbedDevice,
    boot: bool,
    devices: &mut Vec<AxDeviceEnum>,
) -> Option<(usize, ManagedUsbDevice)> {
    let device_id = probed.id();
    let location = probed.observed_location().copied();
    let mut observation = if boot {
        match observations::observe(controller.bus, &probed) {
            Ok(observation) => observation,
            Err(error) => {
                warn!("USB observation unavailable: {error:?}");
                None
            }
        }
    } else {
        None
    };
    let Some(info) = probed.into_device_info() else {
        if let Some(observation) = observation.take() {
            observations::publish(observation);
        }
        return None;
    };
    let selected = info.configurations().iter().find(|config| {
        config
            .interfaces
            .iter()
            .flat_map(|group| &group.alt_settings)
            .any(|interface| {
                if boot {
                    supported_interface(interface)
                } else {
                    interface.class == 3 && supported_interface(interface)
                }
            })
    });
    let Some(config) = selected else {
        if let Some(observation) = observation.take() {
            observations::publish(observation);
        }
        return None;
    };
    let track_hid = managed_hid_configuration(config);
    if !boot && !track_hid {
        if let Some(observation) = observation.take() {
            observations::publish(observation);
        }
        return None;
    }
    let opened: DevResult<Arc<Mutex<Device>>> = (|| {
        let mut backend = controller.backend.lock();
        let mut device = controller
            .host
            .wait(backend.open_device(&info))?
            .map_err(|_| DevError::Io)?;
        controller
            .host
            .wait(device.set_configuration(config.configuration_value))?
            .map_err(|_| DevError::Io)?;
        Arc::try_new(Mutex::new(device)).map_err(|_| DevError::NoMemory)
    })();
    if opened.is_ok()
        && let Some(observation) = &mut observation
    {
        observation.location.configuration = Some(config.configuration_value);
    }
    if let Some(observation) = observation.take() {
        observations::publish(observation);
    }
    let device = match opened {
        Ok(device) => device,
        Err(error) => {
            warn!(
                "USB {:04x}:{:04x} configuration failed: {error:?}",
                info.vendor_id(),
                info.product_id()
            );
            return None;
        }
    };
    let dma_quiesced = Arc::new(AtomicBool::new(false));
    let mut managed = ManagedUsbDevice {
        owner: device.clone(),
        dma_quiesced: dma_quiesced.clone(),
        interfaces: BTreeMap::new(),
    };

    for interface in config
        .interfaces
        .iter()
        .flat_map(|group| &group.alt_settings)
        .filter(|interface| {
            if boot {
                supported_interface(interface)
            } else {
                interface.class == 3 && supported_interface(interface)
            }
        })
    {
        let result: DevResult<()> = (|| {
            let mut guard = device.lock();
            let session = controller
                .host
                .wait(guard.claim_interface(interface.interface_number, 0))?
                .map_err(|_| DevError::Io)?;
            if interface.class == 3 && interface.subclass == 1 {
                class_control(
                    &controller.host,
                    &mut guard,
                    interface.interface_number,
                    0x0b,
                    1, // Always report protocol; keyboard arrays share the generic parser.
                )?;
            }
            drop(guard);
            if is_bluetooth_hci(interface.class, interface.subclass, interface.protocol) {
                if !boot {
                    return Ok(());
                }
                static NEXT_BT_INDEX: core::sync::atomic::AtomicU16 =
                    core::sync::atomic::AtomicU16::new(0);
                let bluetooth = bluetooth::UsbBluetoothHci::new(
                    controller.host.clone(),
                    device.clone(),
                    session,
                    interface,
                    NEXT_BT_INDEX.fetch_add(1, Ordering::Relaxed),
                )?;
                bluetooth::register(bluetooth);
                info!(
                    "USB Bluetooth HCI interface {} registered",
                    interface.interface_number
                );
                return Ok(());
            }
            if interface.class == 3 {
                let input = UsbInput::new(
                    controller.host.clone(),
                    device.clone(),
                    session,
                    interface,
                    dma_quiesced.clone(),
                )?;
                if !track_hid && boot {
                    devices.push(input_device(input));
                    return Ok(());
                }
                // A route-less HID remains owned by this controller but is
                // not published under a synthetic PCI/virtio identity.
                let Some(location) = location else {
                    warn!("USB HID interface has no observed port identity; not publishing input");
                    return Ok(());
                };
                let Some(identity) = observations::input_identity(
                    controller.bus,
                    location,
                    config.configuration_value,
                    interface.interface_number,
                ) else {
                    warn!(
                        "USB HID interface has invalid observed port identity; not publishing \
                         input"
                    );
                    return Ok(());
                };
                managed.interfaces.insert(
                    interface.interface_number,
                    ManagedUsbInterface::Pending { input, identity },
                );
                info!(
                    "USB HID interface {} staged for input registration",
                    interface.interface_number
                );
                return Ok(());
            }
            if interface.class == 8 && boot {
                let block =
                    UsbBlock::new(controller.host.clone(), device.clone(), session, interface)?;
                devices.push(output_block(block));
            }
            Ok(())
        })();
        if let Err(error) = result {
            warn!(
                "USB {:04x}:{:04x} interface {} failed: {error:?}",
                info.vendor_id(),
                info.product_id(),
                interface.interface_number
            );
        }
    }

    if track_hid {
        Some((device_id, managed))
    } else {
        None
    }
}

fn process_changes(
    controller: &UsbController,
    changes: ProbeChanges,
    boot: bool,
    devices: &mut Vec<AxDeviceEnum>,
) {
    for probed in changes.connected {
        if let Some((id, managed)) = process_connected(controller, probed, boot, devices) {
            controller.devices.lock().insert(id, managed);
        }
    }
}

pub(crate) fn probe(mmio: NonNull<u8>) -> DevResult<Vec<AxDeviceEnum>> {
    let mut backend =
        USBHost::new_xhci(mmio, DmaCoherency::Coherent, &dma::KERNEL).map_err(|_| DevError::Io)?;
    let events = backend.create_event_handler();
    let host = Arc::new(Host {
        events: kspin::SpinNoIrq::new(events),
        operational: mmio.as_ptr() as usize + unsafe { mmio.as_ptr().read_volatile() } as usize,
        running: kspin::SpinNoIrq::new(true),
    });
    let bus = observations::allocate_bus().ok_or(DevError::NoMemory)?;
    let controller = Arc::try_new(UsbController {
        backend: Mutex::new(backend),
        host: host.clone(),
        bus,
        devices: Mutex::new(BTreeMap::new()),
        scan_active: AtomicBool::new(false),
        poisoned: AtomicBool::new(false),
    })
    .map_err(|_| DevError::NoMemory)?;
    let mut guard = ProbeGuard {
        host: &host,
        armed: true,
    };
    host.wait(controller.backend.lock().init())?
        .map_err(|_| DevError::Io)?;
    controller
        .backend
        .lock()
        .disable_irq()
        .map_err(|_| DevError::Io)?;
    let changes = host
        .wait(controller.backend.lock().probe_devices())?
        .map_err(|_| DevError::Io)?;
    info!(
        "USB xHCI boot enumeration found {} device(s)",
        changes.connected.len()
    );
    let mut devices = Vec::new();
    process_changes(&controller, changes, true, &mut devices);
    let mut controllers = USB_CONTROLLERS.lock();
    if controllers.try_reserve(1).is_err() {
        return Err(DevError::NoMemory);
    }
    guard.armed = false;
    controllers.push(controller);
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::is_bluetooth_hci;

    #[test]
    fn bluetooth_requires_hci_interface_subclass_and_protocol() {
        assert!(is_bluetooth_hci(0xe0, 1, 1));
        assert!(!is_bluetooth_hci(0xe0, 0, 0));
        assert!(!is_bluetooth_hci(0xe0, 1, 2));
        assert!(!is_bluetooth_hci(3, 1, 1));
    }
}
