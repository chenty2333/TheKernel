//! PCI xHCI host integration. Class drivers use the existing block/evdev APIs.
mod dma;
mod hid;
mod storage;
mod sync;

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    future::Future,
    pin::pin,
    ptr::NonNull,
    task::{Context, Poll, Waker},
    time::Duration,
};

use axdriver_base::{BaseDriverOps, DevError, DevResult};
use crab_usb::{
    DmaCoherency, EventHandler, USBHost,
    device::{Device, InterfaceSession},
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
    fn halt(&self) {
        let mut running = self.running.lock();
        if !*running {
            return;
        }
        unsafe {
            let command = self.operational as *mut u32;
            command.write_volatile(command.read_volatile() & !1);
            let status = (self.operational + 4) as *const u32;
            let end = axhal::time::monotonic_time() + Duration::from_secs(1);
            while status.read_volatile() & 1 == 0 {
                assert!(
                    axhal::time::monotonic_time() < end,
                    "xHCI failed to halt DMA"
                );
                core::hint::spin_loop();
            }
        }
        *running = false;
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
    _device: Device,
    _session: InterfaceSession,
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
pub(crate) fn probe(mmio: NonNull<u8>) -> DevResult<Vec<crate::AxDeviceEnum>> {
    let mut controller = Box::new(
        USBHost::new_xhci(mmio, DmaCoherency::Coherent, &dma::KERNEL).map_err(|_| DevError::Io)?,
    );
    let host = Arc::new(Host {
        events: kspin::SpinNoIrq::new(controller.create_event_handler()),
        operational: mmio.as_ptr() as usize + unsafe { mmio.as_ptr().read_volatile() } as usize,
        running: kspin::SpinNoIrq::new(true),
    });
    let mut guard = ProbeGuard {
        host: &host,
        armed: true,
    };
    host.wait(controller.init())?.map_err(|_| DevError::Io)?;
    controller.disable_irq().map_err(|_| DevError::Io)?;
    let changes = host
        .wait(controller.probe_devices())?
        .map_err(|_| DevError::Io)?;
    let mut devices = Vec::new();
    for probed in changes.connected {
        let Some(info) = probed.into_device_info() else {
            continue;
        };
        let selected = info.configurations().iter().find_map(|config| {
            config
                .interfaces
                .iter()
                .flat_map(|group| &group.alt_settings)
                .find(|interface| {
                    interface.alternate_setting == 0
                        && matches!(
                            (interface.class, interface.subclass, interface.protocol),
                            (3, 1, 1 | 2) | (8, 6, 0x50)
                        )
                })
                .map(|interface| (config.configuration_value, interface.clone()))
        });
        let Some((configuration, interface)) = selected else {
            continue;
        };
        let result: DevResult<crate::AxDeviceEnum> = (|| {
            let mut device = host
                .wait(controller.open_device(&info))?
                .map_err(|_| DevError::Io)?;
            host.wait(device.set_configuration(configuration))?
                .map_err(|_| DevError::Io)?;
            let session = host
                .wait(device.claim_interface(interface.interface_number, 0))?
                .map_err(|_| DevError::Io)?;
            if interface.class == 3 {
                class_control(&host, &mut device, interface.interface_number, 0x0b, 0)?; // SET_PROTOCOL(boot)
                let input = UsbInput::new(host.clone(), device, session, &interface)?;
                #[cfg(not(feature = "dyn"))]
                return Ok(crate::AxDeviceEnum::Input(crate::AxInputDevice::Usb(input)));
                #[cfg(feature = "dyn")]
                return Ok(crate::AxDeviceEnum::Input(Box::new(input)));
            }
            let block = UsbBlock::new(host.clone(), device, session, &interface)?;
            #[cfg(not(feature = "dyn"))]
            return Ok(crate::AxDeviceEnum::Block(crate::AxBlockDevice::Usb(block)));
            #[cfg(feature = "dyn")]
            return Ok(crate::AxDeviceEnum::Block(Box::new(block)));
        })();
        match result {
            Ok(device) => {
                info!("USB registered {}", device.device_name());
                devices.push(device);
            }
            Err(error) => warn!(
                "USB {:04x}:{:04x} interface {} failed: {:?}",
                info.vendor_id(),
                info.product_id(),
                interface.interface_number,
                error
            ),
        }
    }
    // The controller owns DMA rings for the lifetime of these boot devices.
    guard.armed = false;
    Box::leak(controller);
    Ok(devices)
}
