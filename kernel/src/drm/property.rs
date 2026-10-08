//! Stable atomic-KMS property descriptions.

use super::uapi;
use alloc::vec::Vec;

pub const CONNECTOR_CRTC_ID: u32 = 1;
pub const CRTC_ACTIVE: u32 = 2;
pub const CRTC_MODE_ID: u32 = 3;
pub const PLANE_FB_ID: u32 = 4;
pub const PLANE_CRTC_ID: u32 = 5;
pub const PLANE_SRC_X: u32 = 6;
pub const PLANE_SRC_Y: u32 = 7;
pub const PLANE_SRC_W: u32 = 8;
pub const PLANE_SRC_H: u32 = 9;
pub const PLANE_CRTC_X: u32 = 10;
pub const PLANE_CRTC_Y: u32 = 11;
pub const PLANE_CRTC_W: u32 = 12;
pub const PLANE_CRTC_H: u32 = 13;
pub const PLANE_TYPE: u32 = 14;
pub const CONNECTOR_EDID: u32 = 15;
pub const CONNECTOR_DPMS: u32 = 16;
pub const CRTC_GAMMA_LUT: u32 = 17;
pub const PLANE_FB_DAMAGE_CLIPS: u32 = 18;
/// A transient sync_file input for the primary plane.  It deliberately is
/// not retained in atomic state: every atomic request must reset it to -1.
pub const PLANE_IN_FENCE_FD: u32 = 19;
/// Userspace pointer to the transient sync_file fd produced by this CRTC
/// commit.  Like Linux, the property reads back as zero rather than retaining
/// a userspace address from an earlier request.
pub const CRTC_OUT_FENCE_PTR: u32 = 20;
pub const PLANE_IN_FORMATS: u32 = 21;
pub const FORMAT_XRGB8888: u32 = 0x3432_5258;
pub const FORMAT_ARGB8888: u32 = 0x3432_5241;
pub const IN_FORMATS_PRIMARY_BLOB_ID: u32 = 2;
pub const IN_FORMATS_CURSOR_BLOB_ID: u32 = 3;
pub const CRTC_GAMMA_LUT_SIZE: u32 = 22;

#[derive(Clone, Copy)]
pub struct Property {
    pub id: u32,
    pub name: &'static str,
    pub flags: u32,
    pub min: u64,
    pub max: u64,
}

const ATOMIC_RANGE: u32 = uapi::DRM_MODE_PROP_RANGE | uapi::DRM_MODE_PROP_ATOMIC;
const ATOMIC_OBJECT: u32 = uapi::DRM_MODE_PROP_OBJECT | uapi::DRM_MODE_PROP_ATOMIC;
pub const PROPERTIES: [Property; 22] = [
    Property {
        id: CONNECTOR_CRTC_ID,
        name: "CRTC_ID",
        flags: ATOMIC_OBJECT,
        min: 0,
        max: u32::MAX as u64,
    },
    Property {
        id: CRTC_ACTIVE,
        name: "ACTIVE",
        flags: ATOMIC_RANGE,
        min: 0,
        max: 1,
    },
    Property {
        id: CRTC_MODE_ID,
        name: "MODE_ID",
        flags: uapi::DRM_MODE_PROP_BLOB | uapi::DRM_MODE_PROP_ATOMIC,
        min: 0,
        max: u32::MAX as u64,
    },
    Property {
        id: PLANE_FB_ID,
        name: "FB_ID",
        flags: ATOMIC_OBJECT,
        min: 0,
        max: u32::MAX as u64,
    },
    Property {
        id: PLANE_CRTC_ID,
        name: "CRTC_ID",
        flags: ATOMIC_OBJECT,
        min: 0,
        max: u32::MAX as u64,
    },
    Property {
        id: PLANE_SRC_X,
        name: "SRC_X",
        flags: ATOMIC_RANGE,
        min: 0,
        max: u32::MAX as u64,
    },
    Property {
        id: PLANE_SRC_Y,
        name: "SRC_Y",
        flags: ATOMIC_RANGE,
        min: 0,
        max: u32::MAX as u64,
    },
    Property {
        id: PLANE_SRC_W,
        name: "SRC_W",
        flags: ATOMIC_RANGE,
        min: 0,
        max: u32::MAX as u64,
    },
    Property {
        id: PLANE_SRC_H,
        name: "SRC_H",
        flags: ATOMIC_RANGE,
        min: 0,
        max: u32::MAX as u64,
    },
    Property {
        id: PLANE_CRTC_X,
        name: "CRTC_X",
        flags: ATOMIC_RANGE,
        min: 0,
        max: i32::MAX as u64,
    },
    Property {
        id: PLANE_CRTC_Y,
        name: "CRTC_Y",
        flags: ATOMIC_RANGE,
        min: 0,
        max: i32::MAX as u64,
    },
    Property {
        id: PLANE_CRTC_W,
        name: "CRTC_W",
        flags: ATOMIC_RANGE,
        min: 0,
        max: u32::MAX as u64,
    },
    Property {
        id: PLANE_CRTC_H,
        name: "CRTC_H",
        flags: ATOMIC_RANGE,
        min: 0,
        max: u32::MAX as u64,
    },
    Property {
        id: PLANE_TYPE,
        name: "type",
        flags: uapi::DRM_MODE_PROP_ENUM
            | uapi::DRM_MODE_PROP_IMMUTABLE
            | uapi::DRM_MODE_PROP_ATOMIC,
        min: 1,
        max: 1,
    },
    Property {
        id: CONNECTOR_EDID,
        name: "EDID",
        flags: uapi::DRM_MODE_PROP_BLOB | uapi::DRM_MODE_PROP_IMMUTABLE,
        min: 0,
        max: u32::MAX as u64,
    },
    Property {
        id: CONNECTOR_DPMS,
        name: "DPMS",
        flags: uapi::DRM_MODE_PROP_ENUM | uapi::DRM_MODE_PROP_ATOMIC,
        min: 0,
        max: 3,
    },
    Property {
        id: CRTC_GAMMA_LUT,
        name: "GAMMA_LUT",
        flags: uapi::DRM_MODE_PROP_BLOB | uapi::DRM_MODE_PROP_ATOMIC,
        min: 0,
        max: u32::MAX as u64,
    },
    Property {
        id: PLANE_FB_DAMAGE_CLIPS,
        name: "FB_DAMAGE_CLIPS",
        flags: uapi::DRM_MODE_PROP_BLOB | uapi::DRM_MODE_PROP_ATOMIC,
        min: 0,
        max: u32::MAX as u64,
    },
    Property {
        id: PLANE_IN_FENCE_FD,
        name: "IN_FENCE_FD",
        flags: uapi::DRM_MODE_PROP_SIGNED_RANGE | uapi::DRM_MODE_PROP_ATOMIC,
        min: (-1i64) as u64,
        max: i32::MAX as u64,
    },
    Property {
        id: CRTC_OUT_FENCE_PTR,
        name: "OUT_FENCE_PTR",
        flags: ATOMIC_RANGE,
        min: 0,
        max: u64::MAX,
    },
    Property {
        id: PLANE_IN_FORMATS,
        name: "IN_FORMATS",
        flags: uapi::DRM_MODE_PROP_BLOB | uapi::DRM_MODE_PROP_IMMUTABLE,
        min: 0,
        max: u32::MAX as u64,
    },
    Property {
        id: CRTC_GAMMA_LUT_SIZE,
        name: "GAMMA_LUT_SIZE",
        flags: uapi::DRM_MODE_PROP_RANGE | uapi::DRM_MODE_PROP_IMMUTABLE,
        min: 256,
        max: 256,
    },
];

pub fn get(id: u32) -> Option<&'static Property> {
    PROPERTIES.iter().find(|p| p.id == id)
}
pub fn object_properties(object_type: u32) -> &'static [u32] {
    match object_type {
        uapi::DRM_MODE_OBJECT_CONNECTOR => &[CONNECTOR_CRTC_ID, CONNECTOR_EDID, CONNECTOR_DPMS],
        uapi::DRM_MODE_OBJECT_CRTC => &[
            CRTC_ACTIVE,
            CRTC_MODE_ID,
            CRTC_GAMMA_LUT,
            CRTC_GAMMA_LUT_SIZE,
            CRTC_OUT_FENCE_PTR,
        ],
        uapi::DRM_MODE_OBJECT_PLANE => &[
            PLANE_FB_ID,
            PLANE_CRTC_ID,
            PLANE_SRC_X,
            PLANE_SRC_Y,
            PLANE_SRC_W,
            PLANE_SRC_H,
            PLANE_CRTC_X,
            PLANE_CRTC_Y,
            PLANE_CRTC_W,
            PLANE_CRTC_H,
            PLANE_TYPE,
            PLANE_FB_DAMAGE_CLIPS,
            PLANE_IN_FENCE_FD,
            PLANE_IN_FORMATS,
        ],
        _ => &[],
    }
}

/// Serialize the DRM `drm_format_modifier_blob` ABI for a plane's linear
/// formats. The modifier record's bitmap covers the ordered fourcc array;
/// modifier zero is `DRM_FORMAT_MOD_LINEAR`.
pub(crate) fn linear_in_formats_blob(formats: &[u32]) -> Vec<u8> {
    const HEADER_SIZE: usize = 24;
    const MODIFIER_SIZE: usize = 24;
    let formats_offset = HEADER_SIZE;
    let modifier_offset = (formats_offset + formats.len() * 4 + 7) & !7;
    let mut bytes = Vec::with_capacity(modifier_offset + MODIFIER_SIZE);
    for value in [
        1u32,
        0,
        formats.len() as u32,
        formats_offset as u32,
        1,
        modifier_offset as u32,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    for format in formats {
        bytes.extend_from_slice(&format.to_le_bytes());
    }
    bytes.resize(modifier_offset, 0);
    let format_mask = if formats.len() >= 64 {
        u64::MAX
    } else {
        (1u64 << formats.len()) - 1
    };
    bytes.extend_from_slice(&format_mask.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::{
        CRTC_GAMMA_LUT_SIZE, FORMAT_ARGB8888, FORMAT_XRGB8888, PLANE_IN_FORMATS,
        linear_in_formats_blob,
    };
    use crate::drm::uapi;

    #[test]
    fn in_formats_blob_has_aligned_offsets_and_linear_modifier_mask() {
        let bytes = linear_in_formats_blob(&[FORMAT_XRGB8888, FORMAT_ARGB8888]);
        assert_eq!(bytes.len(), 56);
        assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(bytes[12..16].try_into().unwrap()), 24);
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(bytes[20..24].try_into().unwrap()), 32);
        assert_eq!(u64::from_le_bytes(bytes[32..40].try_into().unwrap()), 0b11);
        assert_eq!(u64::from_le_bytes(bytes[48..56].try_into().unwrap()), 0);
    }

    #[test]
    fn cursor_format_blob_advertises_only_argb_linear() {
        let bytes = linear_in_formats_blob(&[FORMAT_ARGB8888]);
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), FORMAT_ARGB8888);
        assert_eq!(u64::from_le_bytes(bytes[32..40].try_into().unwrap()), 1);
    }

    #[test]
    fn in_formats_is_an_immutable_blob_property_on_planes() {
        let property = super::get(PLANE_IN_FORMATS).unwrap();
        assert_ne!(property.flags & uapi::DRM_MODE_PROP_BLOB, 0);
        assert_ne!(property.flags & uapi::DRM_MODE_PROP_IMMUTABLE, 0);
        assert!(super::object_properties(uapi::DRM_MODE_OBJECT_PLANE).contains(&PLANE_IN_FORMATS));
    }

    #[test]
    fn gamma_lut_size_is_immutable_and_matches_getcrtc_size() {
        let property = super::get(CRTC_GAMMA_LUT_SIZE).unwrap();
        assert_ne!(property.flags & uapi::DRM_MODE_PROP_RANGE, 0);
        assert_ne!(property.flags & uapi::DRM_MODE_PROP_IMMUTABLE, 0);
        assert_eq!((property.min, property.max), (256, 256));
        assert!(super::object_properties(uapi::DRM_MODE_OBJECT_CRTC).contains(&CRTC_GAMMA_LUT_SIZE));
    }
}
