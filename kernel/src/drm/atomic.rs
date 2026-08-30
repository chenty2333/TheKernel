//! Lock-safe first-stage atomic KMS state validation and commit.

use super::{
    DrmFile,
    device::{DrmError, DrmResult},
    kms::{Framebuffer, Mode},
    property,
};

#[derive(Clone, Copy, Default)]
pub struct State {
    pub connector_crtc: u32,
    pub active: bool,
    pub mode: Option<Mode>,
    pub mode_blob: u32,
    pub fb: u32,
    pub plane_crtc: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    pub crtc_x: u32,
    pub crtc_y: u32,
    pub crtc_w: u32,
    pub crtc_h: u32,
}
#[derive(Clone, Copy)]
pub struct Change {
    pub object: u32,
    pub property: u32,
    pub value: u64,
}

pub fn initial(resources: &super::kms::KmsResources) -> State {
    State {
        connector_crtc: resources.crtc.id,
        plane_crtc: resources.crtc.id,
        active: false,
        ..State::default()
    }
}
pub fn value(state: &State, property: u32) -> Option<u64> {
    Some(match property {
        property::CONNECTOR_CRTC_ID => state.connector_crtc as u64,
        property::CRTC_ACTIVE => state.active as u64,
        property::CRTC_MODE_ID => state.mode_blob as u64,
        property::PLANE_FB_ID => state.fb as u64,
        property::PLANE_CRTC_ID => state.plane_crtc as u64,
        property::PLANE_SRC_X => state.src_x as u64,
        property::PLANE_SRC_Y => state.src_y as u64,
        property::PLANE_SRC_W => state.src_w as u64,
        property::PLANE_SRC_H => state.src_h as u64,
        property::PLANE_CRTC_X => state.crtc_x as u64,
        property::PLANE_CRTC_Y => state.crtc_y as u64,
        property::PLANE_CRTC_W => state.crtc_w as u64,
        property::PLANE_CRTC_H => state.crtc_h as u64,
        property::PLANE_TYPE => 1,
        _ => return None,
    })
}

pub fn propose(
    file: &DrmFile,
    changes: &[Change],
    mode_blob: Option<(u32, Mode)>,
) -> DrmResult<(u64, State, State, Option<Framebuffer>)> {
    let device = file.device_state();
    if device.atomic_generation_poisoned {
        return Err(DrmError::Overflow);
    }
    let generation = device.atomic_generation;
    let base = device.atomic_tail;
    let mut next = base;
    let r = &device.resources;
    for c in changes {
        if !matches_object(r, c.object, c.property)
            || property::get(c.property).is_none()
            || c.value > u32::MAX as u64
        {
            return Err(DrmError::Invalid);
        }
        match c.property {
            property::CONNECTOR_CRTC_ID => next.connector_crtc = c.value as u32,
            property::CRTC_ACTIVE => {
                if c.value > 1 {
                    return Err(DrmError::Invalid);
                }
                next.active = c.value != 0
            }
            property::CRTC_MODE_ID => {
                next.mode_blob = c.value as u32;
                next.mode = mode_blob.filter(|x| x.0 == next.mode_blob).map(|x| x.1);
            }
            property::PLANE_FB_ID => next.fb = c.value as u32,
            property::PLANE_CRTC_ID => next.plane_crtc = c.value as u32,
            property::PLANE_SRC_X => next.src_x = c.value as u32,
            property::PLANE_SRC_Y => next.src_y = c.value as u32,
            property::PLANE_SRC_W => next.src_w = c.value as u32,
            property::PLANE_SRC_H => next.src_h = c.value as u32,
            property::PLANE_CRTC_X => next.crtc_x = c.value as u32,
            property::PLANE_CRTC_Y => next.crtc_y = c.value as u32,
            property::PLANE_CRTC_W => next.crtc_w = c.value as u32,
            property::PLANE_CRTC_H => next.crtc_h = c.value as u32,
            property::PLANE_TYPE => return Err(DrmError::PermissionDenied),
            _ => return Err(DrmError::Invalid),
        }
    }
    if next.connector_crtc != 0 && next.connector_crtc != r.crtc.id
        || next.plane_crtc != 0 && next.plane_crtc != r.crtc.id
    {
        return Err(DrmError::Invalid);
    }
    let fb = if next.active {
        let mode = next.mode.ok_or(DrmError::Invalid)?;
        let fb = device
            .framebuffers
            .get(&next.fb)
            .filter(|fb| fb.owner == file.id())
            .cloned()
            .ok_or(DrmError::NotFound)?;
        if mode.width != fb.width
            || mode.height != fb.height
            || next.plane_crtc != r.crtc.id
            || next.src_x != 0
            || next.src_y != 0
            || next.src_w != fb.width.checked_shl(16).ok_or(DrmError::Overflow)?
            || next.src_h != fb.height.checked_shl(16).ok_or(DrmError::Overflow)?
            || next.crtc_w != fb.width
            || next.crtc_h != fb.height
        {
            return Err(DrmError::Invalid);
        }
        Some(fb)
    } else {
        if next.fb != 0 {
            return Err(DrmError::Invalid);
        }
        None
    };
    // Keep the lock only for validation; adapter submission and state mutation happen later.
    drop(device);
    Ok((generation, base, next, fb))
}
fn matches_object(r: &super::kms::KmsResources, object: u32, prop: u32) -> bool {
    match prop {
        property::CONNECTOR_CRTC_ID => object == r.connector.id,
        property::CRTC_ACTIVE | property::CRTC_MODE_ID => object == r.crtc.id,
        _ => object == r.primary_plane_id,
    }
}
