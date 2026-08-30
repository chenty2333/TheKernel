use alloc::{borrow::ToOwned, string::String};

use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};
use axdriver_input::{AbsInfo, Event, EventType, InputDeviceId, InputDriverOps};
use virtio_drivers::{
    Hal,
    device::input::{InputConfigSelect, VirtIOInput as InnerDev},
    transport::Transport,
};

use crate::as_dev_err;

/// The VirtIO Input device driver.
pub struct VirtIoInputDev<H: Hal, T: Transport> {
    inner: InnerDev<H, T>,
    device_id: InputDeviceId,
    name: String,
    serial: String,
    irq: Option<usize>,
}

unsafe impl<H: Hal, T: Transport> Send for VirtIoInputDev<H, T> {}
unsafe impl<H: Hal, T: Transport> Sync for VirtIoInputDev<H, T> {}

impl<H: Hal, T: Transport> VirtIoInputDev<H, T> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    pub fn try_new(transport: T, irq: Option<usize>) -> DevResult<Self> {
        let mut virtio = InnerDev::new(transport).unwrap();
        let name = virtio.name().unwrap_or_else(|_| "<unknown>".to_owned());
        // The virtio input configuration is the sole source for a device
        // unique ID. Do not advertise a made-up shared value: libinput uses
        // this metadata to distinguish controllers.
        let serial = virtio.serial_number().unwrap_or_default();
        let device_id = virtio.ids().map_err(as_dev_err)?;
        let device_id = InputDeviceId {
            bus_type: device_id.bustype,
            vendor: device_id.vendor,
            product: device_id.product,
            version: device_id.version,
        };

        Ok(Self {
            inner: virtio,
            device_id,
            name,
            serial,
            irq,
        })
    }
}

impl<H: Hal, T: Transport> BaseDriverOps for VirtIoInputDev<H, T> {
    fn device_name(&self) -> &str {
        &self.name
    }

    fn device_type(&self) -> DeviceType {
        DeviceType::Input
    }

    fn irq_num(&self) -> Option<usize> {
        self.irq
    }
}

impl<H: Hal, T: Transport> InputDriverOps for VirtIoInputDev<H, T> {
    fn device_id(&self) -> InputDeviceId {
        self.device_id
    }

    fn physical_location(&self) -> &str {
        // The transport abstraction does not expose a stable physical path.
        // An empty EVIOCGPHYS is preferable to claiming every controller is
        // the same input0 device.
        ""
    }

    fn unique_id(&self) -> &str {
        &self.serial
    }

    fn get_event_bits(&mut self, ty: EventType, out: &mut [u8]) -> DevResult<bool> {
        let read = self
            .inner
            .query_config_select(InputConfigSelect::EvBits, ty as u8, out);
        Ok(read != 0)
    }

    fn get_property_bits(&mut self, out: &mut [u8]) -> DevResult<bool> {
        let read = self
            .inner
            .query_config_select(InputConfigSelect::PropBits, 0, out);
        Ok(read != 0)
    }

    fn get_abs_info(&mut self, axis: u8) -> DevResult<Option<AbsInfo>> {
        self.inner
            .abs_info(axis)
            .map(|info| {
                Some(AbsInfo {
                    min: info.min,
                    max: info.max,
                    fuzz: info.fuzz,
                    flat: info.flat,
                    res: info.res,
                })
            })
            .map_err(as_dev_err)
    }

    fn read_event(&mut self) -> DevResult<Event> {
        self.inner.ack_interrupt();
        self.inner
            .pop_pending_event()
            .map(|e| Event {
                event_type: e.event_type,
                code: e.code,
                value: e.value,
            })
            .ok_or(DevError::Again)
    }
}
