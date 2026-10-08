//! FreeBSD `sys/dev/hid/hidbus.c` device-info and input-dispatch adaptation.
//! TheKernel already owns the bounded, shared HID report grammar used by USB;
//! I2C-HID reuses it instead of maintaining a second parser.

use alloc::{string::String, vec::Vec};

use axdriver_base::{DevError, DevResult};
use axdriver_input::InputDeviceId;

use crate::hid_report::Report;

pub(super) struct DeviceInfo {
    pub id: InputDeviceId,
    pub hardware_id: String,
    pub path: String,
    pub report_descriptor: Vec<u8>,
    pub quirks: u32,
}

pub(super) fn attach_report_descriptor(bytes: &[u8]) -> DevResult<Report> {
    // upstream: hidbus.c hidbus_attach()
    Report::parse(bytes)
}

impl DeviceInfo {
    // upstream: hidbus.c hidbus_fill_device_info()
    pub(super) fn new(
        id: InputDeviceId,
        hardware_id: String,
        path: String,
        report_descriptor: Vec<u8>,
    ) -> Self {
        Self {
            id,
            hardware_id,
            path,
            report_descriptor,
            // No device-specific FreeBSD HQ_* quirks are registered yet.
            quirks: 0,
        }
    }

    // upstream: hidbus.c hidbus_get_rdesc()
    pub(super) fn report_descriptor(&self, out: &mut [u8]) -> DevResult<usize> {
        if out.len() < self.report_descriptor.len() {
            return Err(DevError::InvalidParam);
        }
        out[..self.report_descriptor.len()].copy_from_slice(&self.report_descriptor);
        Ok(self.report_descriptor.len())
    }
}
