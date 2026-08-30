//! Common traits and types for input device drivers.

#![no_std]

#[doc(no_inline)]
pub use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};
use strum::FromRepr;

#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq, FromRepr)]
pub enum EventType {
    Synchronization     = 0x00,
    Key                 = 0x01,
    Relative            = 0x02,
    Absolute            = 0x03,
    Misc                = 0x04,
    Switch              = 0x05,
    Led                 = 0x11,
    Sound               = 0x12,
    Repeat              = 0x14,
    ForceFeedback       = 0x15,
    Power               = 0x16,
    ForceFeedbackStatus = 0x17,
}

impl EventType {
    pub const MAX: u8 = 0x1f;
    pub const COUNT: u8 = Self::MAX + 1;

    pub const fn bits_count(&self) -> usize {
        match self {
            Self::Synchronization => 0x10,
            Self::Key => 0x300,
            Self::Relative => 0x10,
            Self::Absolute => 0x40,
            Self::Misc => 0x08,
            Self::Switch => 0x12,
            Self::Led => 0x10,
            Self::Sound => 0x08,
            // REP_DELAY and REP_PERIOD.
            Self::Repeat => 0x02,
            Self::ForceFeedback => 0x80,
            // EV_PWR has one defined code; FF_STATUS has PLAYING and
            // MAX. Drivers still decide support through get_event_bits.
            Self::Power => 0x01,
            Self::ForceFeedbackStatus => 0x02,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Event {
    pub event_type: u16,
    pub code: u16,
    pub value: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct InputDeviceId {
    pub bus_type: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub struct AbsInfo {
    pub min: u32,
    pub max: u32,
    pub fuzz: u32,
    pub flat: u32,
    pub res: u32,
}

/// Operations for input hardware.  `get_property_bits` and `get_abs_info`
/// default to unsupported so existing non-virtio drivers remain source
/// compatible while evdev can faithfully expose hardware that provides them.
pub trait InputDriverOps: BaseDriverOps {
    fn device_id(&self) -> InputDeviceId;
    fn physical_location(&self) -> &str;
    fn unique_id(&self) -> &str;
    fn get_event_bits(&mut self, ty: EventType, out: &mut [u8]) -> DevResult<bool>;
    fn get_property_bits(&mut self, _out: &mut [u8]) -> DevResult<bool> {
        Ok(false)
    }
    fn get_abs_info(&mut self, _axis: u8) -> DevResult<Option<AbsInfo>> {
        Ok(None)
    }
    fn read_event(&mut self) -> DevResult<Event>;
}
