use core::num::NonZeroU32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct ScanoutId(NonZeroU32);

impl ScanoutId {
    pub const fn new(raw: u32) -> Option<Self> {
        match NonZeroU32::new(raw) {
            Some(raw) => Some(Self(raw)),
            None => None,
        }
    }
    pub const fn raw(self) -> u32 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
}

impl Mode {
    pub const fn is_valid(self) -> bool {
        self.width != 0 && self.height != 0 && self.refresh_millihz != 0
    }
}

/// Fixed capabilities of one scanout, supplied at adapter construction time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayLimits {
    pub scanout: ScanoutId,
    pub max_width: u32,
    pub max_height: u32,
    pub max_stride_bytes: u32,
}

impl DisplayLimits {
    pub const fn accepts_mode(self, mode: Mode) -> bool {
        mode.is_valid() && mode.width <= self.max_width && mode.height <= self.max_height
    }
}
