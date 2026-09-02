//! Lock-safe first-stage atomic KMS state validation and commit.

use axgpu::{
    AtomicPlanner, DisplayLimits, FrameLayout, Mode as GpuMode, PlaneState, PresentationState,
    ResourceDescriptor, ResourceHandle, ScanoutId,
};

use super::{
    DrmFile,
    device::{DrmError, DrmResult},
    kms::{Framebuffer, Mode},
    property,
};

pub const DPMS_ON: u32 = 0;
pub const DPMS_STANDBY: u32 = 1;
pub const DPMS_SUSPEND: u32 = 2;
pub const DPMS_OFF: u32 = 3;

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
    pub dpms: u32,
    pub gamma_lut_blob: u32,
    pub damage_clips_blob: u32,
    pub cursor_fb: u32,
    pub cursor_crtc: u32,
    pub cursor_src_x: u32,
    pub cursor_src_y: u32,
    pub cursor_src_w: u32,
    pub cursor_src_h: u32,
    pub cursor_crtc_x: u32,
    pub cursor_crtc_y: u32,
    pub cursor_crtc_w: u32,
    pub cursor_crtc_h: u32,
    pub cursor_hot_x: u32,
    pub cursor_hot_y: u32,
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
        cursor_crtc: 0,
        active: false,
        ..State::default()
    }
}
pub fn value(state: &State, property: u32) -> Option<u64> {
    Some(match property {
        property::CONNECTOR_CRTC_ID => state.connector_crtc as u64,
        property::CONNECTOR_DPMS => state.dpms as u64,
        property::CRTC_ACTIVE => state.active as u64,
        property::CRTC_MODE_ID => state.mode_blob as u64,
        property::CRTC_GAMMA_LUT => state.gamma_lut_blob as u64,
        property::CRTC_OUT_FENCE_PTR => 0,
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
        property::PLANE_FB_DAMAGE_CLIPS => state.damage_clips_blob as u64,
        property::PLANE_IN_FENCE_FD => u64::MAX,
        _ => return None,
    })
}
pub fn cursor_value(state: &State, property: u32) -> Option<u64> {
    Some(match property {
        property::PLANE_FB_ID => state.cursor_fb as u64,
        property::PLANE_CRTC_ID => state.cursor_crtc as u64,
        property::PLANE_SRC_X => state.cursor_src_x as u64,
        property::PLANE_SRC_Y => state.cursor_src_y as u64,
        property::PLANE_SRC_W => state.cursor_src_w as u64,
        property::PLANE_SRC_H => state.cursor_src_h as u64,
        property::PLANE_CRTC_X => state.cursor_crtc_x as u64,
        property::PLANE_CRTC_Y => state.cursor_crtc_y as u64,
        property::PLANE_CRTC_W => state.cursor_crtc_w as u64,
        property::PLANE_CRTC_H => state.cursor_crtc_h as u64,
        property::PLANE_TYPE => 2,
        property::PLANE_FB_DAMAGE_CLIPS => 0,
        _ => return None,
    })
}

pub fn value_with_resources(
    resources: &super::kms::KmsResources,
    state: &State,
    property: u32,
) -> Option<u64> {
    match property {
        property::CONNECTOR_EDID => Some(resources.connector.edid_blob as u64),
        _ => value(state, property),
    }
}
pub fn value_for_object(
    resources: &super::kms::KmsResources,
    state: &State,
    object: u32,
    property: u32,
) -> Option<u64> {
    if object == resources.cursor_plane_id {
        cursor_value(state, property)
    } else {
        value_with_resources(resources, state, property)
    }
}

pub(crate) fn referenced_blobs(state: &State) -> [u32; 3] {
    [
        state.mode_blob,
        state.gamma_lut_blob,
        state.damage_clips_blob,
    ]
}

pub fn propose(
    file: &DrmFile,
    changes: &[Change],
    mode_blob: Option<(u32, Mode)>,
) -> DrmResult<(u64, State, State, Option<Framebuffer>)> {
    propose_with_mode(file, changes, mode_blob, None)
}

/// Translate a legacy KMS mode set into the same property proposal used by an
/// atomic ioctl.  Legacy mode records are inline rather than property blobs,
/// so the MODE_ID property deliberately remains zero in the resulting state.
pub fn propose_legacy(
    file: &DrmFile,
    changes: &[Change],
    mode: Option<Mode>,
) -> DrmResult<(u64, State, State, Option<Framebuffer>)> {
    propose_with_mode(file, changes, None, Some(mode))
}

fn propose_with_mode(
    file: &DrmFile,
    changes: &[Change],
    mode_blob: Option<(u32, Mode)>,
    legacy_mode: Option<Option<Mode>>,
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
            || (c.property != property::PLANE_IN_FENCE_FD
                && c.property != property::CRTC_OUT_FENCE_PTR
                && c.value > u32::MAX as u64)
        {
            return Err(DrmError::Invalid);
        }
        let cursor = c.object == r.cursor_plane_id;
        match c.property {
            property::CONNECTOR_CRTC_ID => next.connector_crtc = c.value as u32,
            property::CONNECTOR_EDID => return Err(DrmError::PermissionDenied),
            property::CONNECTOR_DPMS => {
                if c.value > DPMS_OFF as u64 {
                    return Err(DrmError::Invalid);
                }
                next.dpms = c.value as u32;
            }
            property::CRTC_ACTIVE => {
                if c.value > 1 {
                    return Err(DrmError::Invalid);
                }
                next.active = c.value != 0
            }
            property::CRTC_MODE_ID => {
                next.mode_blob = c.value as u32;
                next.mode = match legacy_mode {
                    Some(mode) => {
                        if c.value != 0 {
                            return Err(DrmError::Invalid);
                        }
                        mode
                    }
                    None => mode_blob.filter(|x| x.0 == next.mode_blob).map(|x| x.1),
                };
            }
            property::CRTC_GAMMA_LUT => {
                next.gamma_lut_blob = c.value as u32;
                validate_gamma_lut_blob(&device, next.gamma_lut_blob)?;
            }
            // Explicit fences are request-local.  Their fds/pointers must
            // never leak into a later atomic state, including TEST_ONLY.
            property::CRTC_OUT_FENCE_PTR => {}
            property::PLANE_FB_ID => {
                if cursor {
                    next.cursor_fb = c.value as u32
                } else {
                    next.fb = c.value as u32
                }
            }
            property::PLANE_CRTC_ID => {
                if cursor {
                    next.cursor_crtc = c.value as u32
                } else {
                    next.plane_crtc = c.value as u32
                }
            }
            property::PLANE_SRC_X => {
                if cursor {
                    next.cursor_src_x = c.value as u32
                } else {
                    next.src_x = c.value as u32
                }
            }
            property::PLANE_SRC_Y => {
                if cursor {
                    next.cursor_src_y = c.value as u32
                } else {
                    next.src_y = c.value as u32
                }
            }
            property::PLANE_SRC_W => {
                if cursor {
                    next.cursor_src_w = c.value as u32
                } else {
                    next.src_w = c.value as u32
                }
            }
            property::PLANE_SRC_H => {
                if cursor {
                    next.cursor_src_h = c.value as u32
                } else {
                    next.src_h = c.value as u32
                }
            }
            property::PLANE_CRTC_X => {
                if cursor {
                    next.cursor_crtc_x = c.value as u32
                } else {
                    next.crtc_x = c.value as u32
                }
            }
            property::PLANE_CRTC_Y => {
                if cursor {
                    next.cursor_crtc_y = c.value as u32
                } else {
                    next.crtc_y = c.value as u32
                }
            }
            property::PLANE_CRTC_W => {
                if cursor {
                    next.cursor_crtc_w = c.value as u32
                } else {
                    next.crtc_w = c.value as u32
                }
            }
            property::PLANE_CRTC_H => {
                if cursor {
                    next.cursor_crtc_h = c.value as u32
                } else {
                    next.crtc_h = c.value as u32
                }
            }
            property::PLANE_TYPE => return Err(DrmError::PermissionDenied),
            property::PLANE_FB_DAMAGE_CLIPS => {
                if cursor {
                    return Err(DrmError::Invalid);
                }
                next.damage_clips_blob = c.value as u32
            }
            property::PLANE_IN_FENCE_FD => {
                if cursor || (c.value != u64::MAX && c.value > i32::MAX as u64) {
                    return Err(DrmError::Invalid);
                }
            }
            _ => return Err(DrmError::Invalid),
        }
    }
    if next.connector_crtc != 0 && next.connector_crtc != r.crtc.id
        || next.plane_crtc != 0 && next.plane_crtc != r.crtc.id
        || next.cursor_crtc != 0 && next.cursor_crtc != r.crtc.id
    {
        return Err(DrmError::Invalid);
    }
    let fb = if next.active {
        if !r.connector.connected {
            return Err(DrmError::NotFound);
        }
        let mode = next.mode.ok_or(DrmError::Invalid)?;
        let fb = device
            .framebuffers
            .get(&next.fb)
            .filter(|fb| fb.owner == file.id())
            .cloned()
            .ok_or(DrmError::NotFound)?;
        if mode.width != next.crtc_w
            || mode.height != next.crtc_h
            || next.src_x & 0xffff != 0
            || next.src_y & 0xffff != 0
            || next.src_w != mode.width.checked_shl(16).ok_or(DrmError::Overflow)?
            || next.src_h != mode.height.checked_shl(16).ok_or(DrmError::Overflow)?
            || next.src_x >> 16 > fb.width.saturating_sub(mode.width)
            || next.src_y >> 16 > fb.height.saturating_sub(mode.height)
            || next.plane_crtc != r.crtc.id
            || next.crtc_x != 0
            || next.crtc_y != 0
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
    if next.damage_clips_blob != 0 {
        validate_damage_blob(&device, next.damage_clips_blob, fb.as_ref())?;
    }
    if next.cursor_fb != 0 {
        let cursor = device
            .framebuffers
            .get(&next.cursor_fb)
            .filter(|fb| fb.owner == file.id())
            .ok_or(DrmError::NotFound)?;
        if next.cursor_crtc != r.crtc.id
            || cursor.width != 64
            || cursor.height != 64
            || cursor.bpp != 32
            || next.cursor_src_x != 0
            || next.cursor_src_y != 0
            || next.cursor_src_w != 64 << 16
            || next.cursor_src_h != 64 << 16
            || next.cursor_crtc_w != 64
            || next.cursor_crtc_h != 64
        {
            return Err(DrmError::Invalid);
        }
    } else if next.cursor_crtc != 0 {
        return Err(DrmError::Invalid);
    }
    // Keep Linux object/property policy above this boundary.  Once ownership
    // and the framebuffer are resolved, axgpu owns the device-neutral linear
    // frame and display-state validation used by every presentation path.
    let scanout = ScanoutId::new(r.crtc.id).ok_or(DrmError::Invalid)?;
    let limits = DisplayLimits {
        scanout,
        max_width: u32::MAX,
        max_height: u32::MAX,
        max_stride_bytes: u32::MAX,
    };
    let current_fb = base
        .active
        .then(|| device.framebuffers.get(&base.fb).cloned())
        .flatten();
    AtomicPlanner::new(limits)
        .plan(
            presentation_state(scanout, base, current_fb.as_ref())?,
            presentation_state(scanout, next, fb.as_ref())?,
        )
        .map_err(|_| DrmError::Invalid)?;
    // Keep the lock only for validation; adapter submission and state mutation happen later.
    drop(device);
    Ok((generation, base, next, fb))
}

fn presentation_state(
    scanout: ScanoutId,
    state: State,
    framebuffer: Option<&Framebuffer>,
) -> DrmResult<PresentationState> {
    if !state.active || state.dpms != DPMS_ON {
        return Ok(PresentationState::disabled(scanout));
    }
    let framebuffer = framebuffer.ok_or(DrmError::NotFound)?;
    let mode = state.mode.ok_or(DrmError::Invalid)?;
    let bytes_per_pixel = u8::try_from(framebuffer.bpp / 8).map_err(|_| DrmError::Invalid)?;
    let resource = ResourceHandle::new(state.fb).ok_or(DrmError::Invalid)?;
    let descriptor = ResourceDescriptor {
        bytes: framebuffer.object.size,
        width: framebuffer.width,
        height: framebuffer.height,
        stride_bytes: framebuffer.pitch,
        bytes_per_pixel,
    };
    // Legacy DRM stores source coordinates in 16.16 fixed-point.  Earlier
    // validation already requires a full-frame source; destination origin is
    // not representable by the virtio scanout path and has always been
    // ignored, so normalize it before handing the device-neutral plan over.
    let source_width = state.src_w.checked_shr(16).ok_or(DrmError::Invalid)?;
    let source_height = state.src_h.checked_shr(16).ok_or(DrmError::Invalid)?;
    Ok(PresentationState {
        scanout,
        enabled: true,
        mode: Some(GpuMode {
            width: mode.width,
            height: mode.height,
            refresh_millihz: mode.refresh_millihz.max(1),
        }),
        primary_plane: Some(PlaneState {
            frame: FrameLayout {
                resource,
                descriptor,
            },
            source_x: state.src_x >> 16,
            source_y: state.src_y >> 16,
            source_width,
            source_height,
            destination_x: 0,
            destination_y: 0,
            destination_width: state.crtc_w,
            destination_height: state.crtc_h,
        }),
    })
}
fn matches_object(r: &super::kms::KmsResources, object: u32, prop: u32) -> bool {
    match prop {
        property::CONNECTOR_CRTC_ID | property::CONNECTOR_EDID | property::CONNECTOR_DPMS => {
            object == r.connector.id
        }
        property::CRTC_ACTIVE
        | property::CRTC_MODE_ID
        | property::CRTC_GAMMA_LUT
        | property::CRTC_OUT_FENCE_PTR => object == r.crtc.id,
        _ => object == r.primary_plane_id || object == r.cursor_plane_id,
    }
}

fn validate_gamma_lut_blob(device: &super::device::DeviceState, blob: u32) -> DrmResult<()> {
    if blob == 0 {
        return Ok(());
    }
    let blob = device.property_blobs.get(&blob).ok_or(DrmError::NotFound)?;
    let entries = device.gamma_lut.len() / 3;
    if blob.destroyed || blob.bytes.len() != entries * 8 {
        return Err(DrmError::Invalid);
    }
    if blob
        .bytes
        .chunks_exact(8)
        .any(|entry| entry[6] != 0 || entry[7] != 0)
    {
        return Err(DrmError::Invalid);
    }
    Ok(())
}

fn validate_damage_blob(
    device: &super::device::DeviceState,
    blob: u32,
    framebuffer: Option<&Framebuffer>,
) -> DrmResult<()> {
    let framebuffer = framebuffer.ok_or(DrmError::Invalid)?;
    let blob = device.property_blobs.get(&blob).ok_or(DrmError::NotFound)?;
    if blob.destroyed || blob.bytes.len() % 8 != 0 {
        return Err(DrmError::Invalid);
    }
    for rect in blob.bytes.chunks_exact(8) {
        let x1 = i16::from_ne_bytes([rect[0], rect[1]]);
        let y1 = i16::from_ne_bytes([rect[2], rect[3]]);
        let x2 = i16::from_ne_bytes([rect[4], rect[5]]);
        let y2 = i16::from_ne_bytes([rect[6], rect[7]]);
        if x1 < 0
            || y1 < 0
            || x1 >= x2
            || y1 >= y2
            || u32::try_from(x2).map_or(true, |x| x > framebuffer.width)
            || u32::try_from(y2).map_or(true, |y| y > framebuffer.height)
        {
            return Err(DrmError::Invalid);
        }
    }
    Ok(())
}
