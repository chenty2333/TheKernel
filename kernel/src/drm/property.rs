//! Stable atomic-KMS property descriptions.

use super::uapi;

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
pub const PROPERTIES: [Property; 14] = [
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
];

pub fn get(id: u32) -> Option<&'static Property> {
    PROPERTIES.iter().find(|p| p.id == id)
}
pub fn object_properties(object_type: u32) -> &'static [u32] {
    match object_type {
        uapi::DRM_MODE_OBJECT_CONNECTOR => &[CONNECTOR_CRTC_ID],
        uapi::DRM_MODE_OBJECT_CRTC => &[CRTC_ACTIVE, CRTC_MODE_ID],
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
        ],
        _ => &[],
    }
}
