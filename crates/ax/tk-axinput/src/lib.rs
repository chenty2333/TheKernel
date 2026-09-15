//! [ArceOS](https://github.com/arceos-org/arceos) input module.

#![no_std]

#[macro_use]
extern crate log;
extern crate alloc;

use alloc::{collections::BTreeMap, sync::Arc};
use core::sync::atomic::{AtomicU64, Ordering};

use axdriver::{AxDeviceContainer, InputBusIdentity, prelude::*};
use axsync::Mutex;
use lazyinit::LazyInit;

static INPUTS: LazyInit<Mutex<InputRegistry>> = LazyInit::new();

/// Stable identity assigned when an input driver is registered.  It is never
/// reused during a boot, so consumers can keep device identity separate from
/// the transient event-node minor number.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct InputDeviceToken(u64);

impl InputDeviceToken {
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Reconciles the PCI input topology. It is intentionally explicit: callers
/// invoke it from their PCI hotplug notification/poll point, while this module
/// remains the sole owner of event-node lifetime and removal revocation.
pub fn reconcile_pci_devices() {
    axdriver::reconcile_pci_input_devices(
        |device, identity| register_input_with_identity(device, identity).get(),
        |token| unregister_input(InputDeviceToken(token)),
    );
}

/// Ownership of a newly registered input driver delivered to the input
/// consumer.  The consumer owns the driver until the matching remove
/// notification arrives.
pub struct RegisteredInputDevice {
    pub token: InputDeviceToken,
    pub epoch: u64,
    pub identity: InputBusIdentity,
    pub device: AxInputDevice,
}

/// Dynamic input lifecycle notifications.  This deliberately models physical
/// removal separately from temporary input pause: a remove notification is
/// terminal for file descriptions belonging to the device.
pub trait InputDeviceListener: Send + Sync + 'static {
    fn device_added(&self, device: RegisteredInputDevice);
    fn device_removed(&self, token: InputDeviceToken, epoch: u64);
}

struct InputRegistry {
    devices: BTreeMap<InputDeviceToken, InputRecord>,
    listener: Option<Arc<dyn InputDeviceListener>>,
}

struct InputRecord {
    epoch: u64,
    identity: InputBusIdentity,
    pending: Option<AxInputDevice>,
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);
static DELIVERY: Mutex<()> = Mutex::new(());

fn next_token() -> InputDeviceToken {
    let token = NEXT_TOKEN
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .expect("input device token space exhausted");
    InputDeviceToken(token)
}

fn next_epoch() -> u64 {
    NEXT_EPOCH
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .expect("input delivery epoch space exhausted")
}

fn registry() -> &'static Mutex<InputRegistry> {
    INPUTS.get().expect("input subsystem not initialized")
}

/// Installs the sole kernel-side owner for input devices and transfers every
/// device discovered before devfs was mounted.  Registration and removal
/// callbacks are made after dropping the registry lock.
pub fn install_listener(listener: Arc<dyn InputDeviceListener>) {
    let _delivery = DELIVERY.lock();
    let pending = {
        let mut registry = registry().lock();
        assert!(
            registry.listener.is_none(),
            "input listener already installed"
        );
        registry.listener = Some(listener.clone());
        registry
            .devices
            .iter_mut()
            .filter_map(|(token, record)| {
                record.pending.take().map(|device| RegisteredInputDevice {
                    token: *token,
                    epoch: record.epoch,
                    identity: record.identity,
                    device,
                })
            })
            .collect::<alloc::vec::Vec<_>>()
    };
    for device in pending {
        listener.device_added(device);
    }
}

/// Registers an input driver and returns its stable removal identity.
pub fn register_input(device: AxInputDevice) -> InputDeviceToken {
    register_input_with_identity(device, InputBusIdentity::bootstrap())
}

/// Registers one input driver with its stable bus identity.  The identity is
/// retained across the registration transaction and is never derived from a
/// reusable event-node minor.
pub fn register_input_with_identity(
    device: AxInputDevice,
    identity: InputBusIdentity,
) -> InputDeviceToken {
    let token = next_token();
    let epoch = next_epoch();
    let _delivery = DELIVERY.lock();
    let delivery = {
        let mut registry = registry().lock();
        assert!(
            registry
                .devices
                .insert(
                    token,
                    InputRecord {
                        epoch,
                        identity,
                        pending: Some(device),
                    },
                )
                .is_none(),
            "input token reused"
        );
        if let Some(listener) = registry.listener.clone() {
            let device = registry
                .devices
                .get_mut(&token)
                .and_then(|record| record.pending.take())
                .expect("fresh input record lost its device");
            Some((
                listener,
                RegisteredInputDevice {
                    token,
                    epoch,
                    identity,
                    device,
                },
            ))
        } else {
            None
        }
    };
    if let Some((listener, registered)) = delivery {
        listener.device_added(registered);
    }
    token
}

/// Removes a registered physical device.  Pending devices are discarded;
/// installed consumers receive a terminal removal notification.
pub fn unregister_input(token: InputDeviceToken) {
    let _delivery = DELIVERY.lock();
    let delivery = {
        let mut registry = registry().lock();
        let Some(record) = registry.devices.remove(&token) else {
            return;
        };
        // A pending device was never published to devfs, so removal is a
        // local cancellation and must not generate an unmatched callback.
        record
            .pending
            .is_none()
            .then(|| (registry.listener.clone(), record.epoch))
    };
    if let Some((Some(listener), epoch)) = delivery {
        listener.device_removed(token, epoch);
    }
}

/// Initializes the graphics subsystem by underlayer devices.
pub fn init_input(mut input_devs: AxDeviceContainer<AxInputDevice>) {
    info!("Initialize input subsystem...");
    INPUTS.init_once(Mutex::new(InputRegistry {
        devices: BTreeMap::new(),
        listener: None,
    }));
    while let Some(dev) = input_devs.take_one() {
        info!(
            "  registered a new {:?} input device: {}",
            dev.device_type(),
            dev.device_name(),
        );
        register_input(dev);
    }
    axdriver::activate_boot_pci_input_devices(
        |device, identity| register_input_with_identity(device, identity).get(),
        |token| unregister_input(InputDeviceToken(token)),
    );
}
