//! FreeBSD `sys/dev/hid/hidbus.c` device-info and input-dispatch adaptation.
//! TheKernel already owns the bounded, shared HID report grammar used by USB;
//! I2C-HID reuses it instead of maintaining a second parser.

use alloc::{string::String, vec::Vec};

use axdriver_base::{DevError, DevResult};
use axdriver_input::InputDeviceId;

use crate::hid_report::{Report, ReportKind};

pub(super) const QUIRK_NOWRITE: u32 = 1 << 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ReportSize {
    pub report_id: u8,
    pub bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ReportDescriptorInfo {
    pub input: ReportSize,
    pub output: ReportSize,
    pub feature: ReportSize,
}

pub(super) struct DeviceInfo {
    pub id: InputDeviceId,
    pub hardware_id: String,
    pub path: String,
    pub report_descriptor: Vec<u8>,
    pub report_info: ReportDescriptorInfo,
    pub quirks: u32,
}

pub(super) fn attach_report_descriptor(bytes: &[u8]) -> DevResult<(Report, ReportDescriptorInfo)> {
    // upstream: hidbus.c hidbus_fill_rdesc_info() / hidbus_attach()
    let report = Report::parse(bytes)?;
    let size = |kind| {
        let (report_id, _) = report.report_size_max(kind);
        let bytes = report.report_size(kind, report_id);
        ReportSize { report_id, bytes }
    };
    let info = ReportDescriptorInfo {
        input: size(ReportKind::Input),
        output: size(ReportKind::Output),
        feature: size(ReportKind::Feature),
    };
    Ok((report, info))
}

impl DeviceInfo {
    // upstream: hidbus.c hidbus_fill_device_info()
    pub(super) fn new(
        id: InputDeviceId,
        hardware_id: String,
        path: String,
        report_descriptor: Vec<u8>,
        report_info: ReportDescriptorInfo,
        descriptor: tk_i2c_hid::Descriptor,
    ) -> Self {
        let quirks = if descriptor.output_register == 0 || descriptor.max_output_length == 0 {
            QUIRK_NOWRITE
        } else {
            0
        };
        Self {
            id,
            hardware_id,
            path,
            report_descriptor,
            report_info,
            // upstream: iichid.c iichid_fill_device_info() HQ_NOWRITE
            quirks,
        }
    }

    // upstream: hidbus.c hidbus_get_rdesc() / hid.c hid_get_report_descr()
    pub(super) fn report_descriptor(&self, out: &mut [u8]) -> DevResult<usize> {
        if out.len() < self.report_descriptor.len() {
            return Err(DevError::InvalidParam);
        }
        out[..self.report_descriptor.len()].copy_from_slice(&self.report_descriptor);
        Ok(self.report_descriptor.len())
    }
}
