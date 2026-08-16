use core::{
    any::Any,
    ffi::c_int,
    mem::{align_of, offset_of, size_of},
};

use axfs_ng_vfs::{DeviceId, NodeFlags, VfsError, VfsResult};
use chrono::{Datelike, Timelike};
use linux_raw_sys::ioctl::RTC_RD_TIME;

use crate::{file::IoctlContext, pseudofs::DeviceOps, time::wall_time_nanos};

/// The device ID for /dev/rtc0
pub const RTC0_DEVICE_ID: DeviceId = DeviceId::new(250, 0);

#[repr(C)]
#[allow(non_camel_case_types, dead_code)]
#[derive(Clone, Copy)]
struct rtc_time {
    tm_sec: c_int,
    tm_min: c_int,
    tm_hour: c_int,
    tm_mday: c_int,
    tm_mon: c_int,
    tm_year: c_int,
    tm_wday: c_int,
    tm_yday: c_int,
    tm_isdst: c_int,
}

const _: () = {
    assert!(size_of::<rtc_time>() == 9 * size_of::<c_int>());
    assert!(align_of::<rtc_time>() == align_of::<c_int>());
};

fn rtc_time_to_user_bytes(value: rtc_time) -> [u8; size_of::<rtc_time>()] {
    let mut bytes = [0u8; size_of::<rtc_time>()];
    for (offset, field) in [
        (offset_of!(rtc_time, tm_sec), value.tm_sec),
        (offset_of!(rtc_time, tm_min), value.tm_min),
        (offset_of!(rtc_time, tm_hour), value.tm_hour),
        (offset_of!(rtc_time, tm_mday), value.tm_mday),
        (offset_of!(rtc_time, tm_mon), value.tm_mon),
        (offset_of!(rtc_time, tm_year), value.tm_year),
        (offset_of!(rtc_time, tm_wday), value.tm_wday),
        (offset_of!(rtc_time, tm_yday), value.tm_yday),
        (offset_of!(rtc_time, tm_isdst), value.tm_isdst),
    ] {
        bytes[offset..][..size_of::<c_int>()].copy_from_slice(&field.to_ne_bytes());
    }
    bytes
}

/// RTC device
pub struct Rtc;

impl DeviceOps for Rtc {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::Unsupported)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::Unsupported)
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            RTC_RD_TIME => {
                let wall = chrono::DateTime::from_timestamp_nanos(wall_time_nanos() as _);
                let value = rtc_time {
                    tm_sec: wall.second() as _,
                    tm_min: wall.minute() as _,
                    tm_hour: wall.hour() as _,
                    tm_mday: wall.day() as _,
                    tm_mon: wall.month0() as _,
                    tm_year: (wall.year() - 1900) as _,
                    tm_wday: wall.weekday().num_days_from_sunday() as _,
                    tm_yday: wall.ordinal0() as _,
                    tm_isdst: 0,
                };
                let bytes = rtc_time_to_user_bytes(value);
                context
                    .user_memory()
                    .write_bytes(arg, &bytes)
                    .map_err(crate::mm::map_usercopy_error)?;
            }
            _ => return Err(VfsError::NotATty),
        }
        Ok(0)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
            | NodeFlags::STREAM
            | NodeFlags::NO_POSITIONED_READ
            | NodeFlags::NO_POSITIONED_WRITE
            | NodeFlags::NO_SEEK
    }
}
