//! FreeBSD `sys/dev/hid/hidbus.c` device-info and input-dispatch adaptation.
//! TheKernel already owns the bounded, shared HID report grammar used by USB;
//! I2C-HID reuses it instead of maintaining a second parser.

use alloc::{string::String, vec::Vec};

use axdriver_base::{DevError, DevResult};
use axdriver_input::InputDeviceId;
use tk_i2c_hid::{Device, Error, Transport};

use crate::hid_report::{HidLocation, Report, ReportKind};

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

// upstream: hidbus.c hidbus_locate()
pub(super) fn locate(
    report: &Report,
    kind: ReportKind,
    page: u32,
    usage: u32,
    tlc_index: u8,
    usage_index: usize,
) -> Option<HidLocation> {
    report.locate_usage_in_collection(kind, page, usage, tlc_index, usage_index)
}

// upstream: hidbus.c hidbus_get_report()
pub(super) fn get_report<T: Transport>(
    device: &mut Device<T>,
    report_type: u8,
    report_id: u8,
    out: &mut [u8],
) -> Result<usize, Error> {
    device.get_report(report_type, report_id, out)
}

// upstream: hidbus.c hidbus_set_report()
pub(super) fn set_report<T: Transport>(
    device: &mut Device<T>,
    report_type: u8,
    report_id: u8,
    report: &[u8],
) -> Result<(), Error> {
    device.set_report(report_type, report_id, report)
}

// upstream: hidbus.c hidbus_read()
pub(super) fn read<T: Transport>(device: &mut Device<T>, out: &mut [u8]) -> Result<usize, Error> {
    device.read_input(out)
}

// upstream: hidbus.c hidbus_write()
pub(super) fn write<T: Transport>(device: &mut Device<T>, report: &[u8]) -> Result<(), Error> {
    device.write_output(report)
}

// upstream: hidbus.c hidbus_set_idle()
pub(super) fn set_idle<T: Transport>(
    device: &mut Device<T>,
    duration: u16,
    report_id: u8,
) -> Result<(), Error> {
    device.set_idle(duration, report_id)
}

// upstream: hidbus.c hidbus_set_protocol()
pub(super) fn set_protocol<T: Transport>(
    device: &mut Device<T>,
    protocol: u16,
) -> Result<(), Error> {
    device.set_protocol(protocol)
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
