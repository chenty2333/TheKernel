//! x86_64 Linux `FS_IOC_FIEMAP` wire layout and request policy.

use core::mem::size_of;

use bytemuck::{Pod, Zeroable};

/// The sole FIEMAP request flag supported by this kernel.
pub const FIEMAP_SUPPORTED_FLAGS: u32 = 0x0000_0001;
/// The maximum flexible-array capacity that fits in a `u32`-sized ioctl.
pub const FIEMAP_MAX_EXTENTS: u32 = u32::MAX / size_of::<FiemapExtent>() as u32;
/// Maximum number of extents retained by one kernel-to-userspace copy batch.
pub const FIEMAP_STREAM_BATCH_EXTENTS: usize = 256;

/// The fixed header of the x86_64 Linux FIEMAP ioctl.
#[repr(C)]
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct Fiemap {
    pub fm_start: u64,
    pub fm_length: u64,
    pub fm_flags: u32,
    pub fm_mapped_extents: u32,
    pub fm_extent_count: u32,
    pub fm_reserved: u32,
}

/// One x86_64 Linux FIEMAP flexible-array member.
#[repr(C)]
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct FiemapExtent {
    pub fe_logical: u64,
    pub fe_physical: u64,
    pub fe_length: u64,
    pub fe_reserved64: [u64; 2],
    pub fe_flags: u32,
    pub fe_reserved: [u32; 3],
}

/// The filesystem-neutral allocation state projected onto Linux extent flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(missing_docs)]
pub enum FiemapExtentState {
    Written,
    Unwritten,
}

impl FiemapExtent {
    /// Builds the Linux wire extent from a neutral allocated extent.
    pub const fn from_mapping(
        logical: u64,
        physical: u64,
        length: u64,
        state: FiemapExtentState,
        last: bool,
    ) -> Self {
        let mut flags = match state {
            FiemapExtentState::Written => 0,
            FiemapExtentState::Unwritten => 0x0000_0800,
        };
        if last {
            flags |= 0x0000_0001;
        }
        Self {
            fe_logical: logical,
            fe_physical: physical,
            fe_length: length,
            fe_reserved64: [0; 2],
            fe_flags: flags,
            fe_reserved: [0; 3],
        }
    }
}

/// Failure while validating a Linux FIEMAP request before filesystem access.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(missing_docs)]
pub enum FiemapRequestError {
    ExtentCapacityTooLarge,
    ZeroLength,
    StartPastMaximum,
    UnsupportedFlags,
}

impl Fiemap {
    /// Rejects a flexible-array capacity that cannot be addressed safely.
    pub const fn validate_extent_count(&self) -> Result<(), FiemapRequestError> {
        if self.fm_extent_count > FIEMAP_MAX_EXTENTS {
            Err(FiemapRequestError::ExtentCapacityTooLarge)
        } else {
            Ok(())
        }
    }

    /// Performs the filesystem-dependent Linux FIEMAP preparation.
    ///
    /// This mirrors `fiemap_prep`: zero length precedes the filesystem bound;
    /// an invalid flag is returned to userspace in `fm_flags`.
    pub const fn prepare(&mut self, max_bytes: u64) -> Result<u64, FiemapRequestError> {
        if self.fm_length == 0 {
            return Err(FiemapRequestError::ZeroLength);
        }
        if self.fm_start >= max_bytes {
            return Err(FiemapRequestError::StartPastMaximum);
        }
        let incompatible = self.unsupported_flags();
        if incompatible != 0 {
            self.fm_flags = incompatible;
            return Err(FiemapRequestError::UnsupportedFlags);
        }
        let remaining = max_bytes - self.fm_start;
        Ok(if self.fm_length < remaining {
            self.fm_length
        } else {
            remaining
        })
    }

    /// Returns request flag bits unsupported by the Linux FIEMAP contract.
    pub const fn unsupported_flags(&self) -> u32 {
        self.fm_flags & !FIEMAP_SUPPORTED_FLAGS
    }

    /// Whether the request requires synchronous filesystem state.
    pub const fn is_sync(&self) -> bool {
        self.fm_flags & FIEMAP_SUPPORTED_FLAGS != 0
    }
}

const _: () = assert!(size_of::<Fiemap>() == 32);
const _: () = assert!(size_of::<FiemapExtent>() == 56);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_x86_64_linux_uapi() {
        assert_eq!(size_of::<Fiemap>(), 32);
        assert_eq!(size_of::<FiemapExtent>(), 56);
        assert_eq!(core::mem::offset_of!(Fiemap, fm_extent_count), 24);
        assert_eq!(core::mem::offset_of!(FiemapExtent, fe_flags), 40);
    }

    #[test]
    fn request_policy_clips_a_valid_range() {
        let mut zero = Fiemap::default();
        assert_eq!(zero.prepare(8), Err(FiemapRequestError::ZeroLength));
        let mut clipped = Fiemap {
            fm_start: 7,
            fm_length: u64::MAX,
            ..Fiemap::default()
        };
        assert_eq!(clipped.prepare(8), Ok(1));
    }

    #[test]
    fn preparation_preserves_linux_validation_order() {
        let mut request = Fiemap {
            fm_start: 8,
            fm_flags: 2,
            ..Fiemap::default()
        };
        assert_eq!(request.prepare(8), Err(FiemapRequestError::ZeroLength));
        assert_eq!(request.fm_flags, 2);
        request.fm_length = 1;
        assert_eq!(
            request.prepare(8),
            Err(FiemapRequestError::StartPastMaximum)
        );
        assert_eq!(request.fm_flags, 2);
        request.fm_start = 0;
        assert_eq!(
            request.prepare(8),
            Err(FiemapRequestError::UnsupportedFlags)
        );
        assert_eq!(request.fm_flags, 2);
    }

    #[test]
    fn mapping_applies_linux_only_flags() {
        let extent = FiemapExtent::from_mapping(1, 2, 3, FiemapExtentState::Unwritten, true);
        assert_eq!(extent.fe_flags, 0x801);
    }

    #[test]
    fn flags_admit_only_sync() {
        let request = Fiemap {
            fm_flags: FIEMAP_SUPPORTED_FLAGS,
            ..Fiemap::default()
        };
        assert!(request.is_sync());
        assert_eq!(request.unsupported_flags(), 0);
        let request = Fiemap {
            fm_flags: 2,
            ..Fiemap::default()
        };
        assert_eq!(request.unsupported_flags(), 2);
    }
}
