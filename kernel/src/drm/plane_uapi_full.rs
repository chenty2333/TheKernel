// SPDX-License-Identifier: MIT
/*
 * Copyright (c) 2016 Intel Corporation
 *
 * Permission to use, copy, modify, distribute, and sell this software and its
 * documentation for any purpose is hereby granted without fee, provided that
 * the above copyright notice appear in all copies and that both that copyright
 * notice and this permission notice appear in supporting documentation, and
 * that the name of the copyright holders not be used in advertising or
 * publicity pertaining to distribution of the software without specific,
 * written prior permission. The copyright holders make no representations
 * about the suitability of this software for any purpose. It is provided "as
 * is" without express or implied warranty.
 *
 * THE COPYRIGHT HOLDERS DISCLAIM ALL WARRANTIES WITH REGARD TO THIS SOFTWARE,
 * INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS, IN NO
 * EVENT SHALL THE COPYRIGHT HOLDERS BE LIABLE FOR ANY SPECIAL, INDIRECT OR
 * CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE,
 * DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER
 * TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE
 * OF THIS SOFTWARE.
 */

//! Function-level translation of Linux v7.2.3 `drivers/gpu/drm/drm_plane.c`.
//! Object/property registration, user copies, modeset locking, atomic state,
//! framebuffer references, driver callbacks, and event/vblank services are
//! explicit `DrmPlaneIo` hooks; the format, legacy plane, cursor, page-flip,
//! damage-clip, and property algorithms are kept here.

extern crate alloc;
use alloc::{string::String, vec, vec::Vec};

pub type DrmId = u32;
pub type UserPtr = u64;
pub type PlaneResult<T> = Result<T, i32>;

pub const EACCES: i32 = -13;
pub const EBUSY: i32 = -16;
pub const EDEADLK: i32 = -35;
pub const EFAULT: i32 = -14;
pub const EINVAL: i32 = -22;
pub const ENOENT: i32 = -2;
pub const ENOMEM: i32 = -12;
pub const ENODEV: i32 = -19;
pub const ENOSPC: i32 = -28;
pub const ENXIO: i32 = -6;
pub const EOPNOTSUPP: i32 = -95;
pub const ERANGE: i32 = -34;
pub const DRM_FORMAT_MOD_INVALID: u64 = u64::MAX;
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
pub const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;
pub const DRM_PLANE_TYPE_OVERLAY: u32 = 0;
pub const DRM_PLANE_TYPE_PRIMARY: u32 = 1;
pub const DRM_PLANE_TYPE_CURSOR: u32 = 2;
pub const DRM_MODE_CURSOR_BO: u32 = 1;
pub const DRM_MODE_CURSOR_MOVE: u32 = 2;
pub const DRM_MODE_CURSOR_FLAGS: u32 = DRM_MODE_CURSOR_BO | DRM_MODE_CURSOR_MOVE;
pub const DRM_MODE_PAGE_FLIP_EVENT: u32 = 1 << 0;
pub const DRM_MODE_PAGE_FLIP_ASYNC: u32 = 1 << 1;
pub const DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE: u32 = 1 << 2;
pub const DRM_MODE_PAGE_FLIP_TARGET_RELATIVE: u32 = 1 << 3;
pub const DRM_MODE_PAGE_FLIP_TARGET: u32 =
    DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE | DRM_MODE_PAGE_FLIP_TARGET_RELATIVE;
pub const DRM_MODE_PAGE_FLIP_FLAGS: u32 = DRM_MODE_PAGE_FLIP_EVENT
    | DRM_MODE_PAGE_FLIP_ASYNC
    | DRM_MODE_PAGE_FLIP_TARGET;
pub const DRM_MODE_PROP_ENUM: u32 = 1 << 3;
pub const DRM_MODE_PROP_ATOMIC: u32 = 1 << 31;
pub const DRM_SCALING_FILTER_DEFAULT: u32 = 0;
pub const DRM_SCALING_FILTER_NEAREST_NEIGHBOR: u32 = 1;
pub const FORMAT_BLOB_CURRENT: u32 = 1;
pub const FORMAT_MODIFIER_BLOB_HEADER_SIZE: usize = 24;
pub const DRM_EVENT_FLIP_COMPLETE: u32 = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaneProperties {
    pub type_property: DrmId,
    pub fb_id: DrmId,
    pub in_fence_fd: DrmId,
    pub crtc_id: DrmId,
    pub crtc_x: DrmId,
    pub crtc_y: DrmId,
    pub crtc_w: DrmId,
    pub crtc_h: DrmId,
    pub src_x: DrmId,
    pub src_y: DrmId,
    pub src_w: DrmId,
    pub src_h: DrmId,
    pub modifiers: DrmId,
    pub async_modifiers: DrmId,
    pub damage_clips: DrmId,
    pub size_hints: DrmId,
}

#[derive(Clone, Debug, Default)]
pub struct ModeConfig {
    pub num_total_plane: usize,
    pub fb_modifiers_not_supported: bool,
    pub async_page_flip: bool,
    pub properties: PlaneProperties,
}

#[derive(Clone, Debug, Default)]
pub struct PlaneFuncs {
    pub destroy: bool,
    pub atomic_destroy_state: bool,
    pub atomic_duplicate_state: bool,
    pub format_mod_supported: bool,
    pub format_mod_supported_async: bool,
    pub late_register: bool,
    pub early_unregister: bool,
    pub set_property: bool,
    pub disable_plane: bool,
    pub update_plane: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PropertyValue {
    pub property: DrmId,
    pub value: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FormatModifier {
    pub formats: u64,
    pub offset: u32,
    pub pad: u32,
    pub modifier: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FormatModifierBlob {
    pub version: u32,
    pub flags: u32,
    pub count_formats: u32,
    pub formats_offset: u32,
    pub count_modifiers: u32,
    pub modifiers_offset: u32,
    pub formats: Vec<u32>,
    pub modifiers: Vec<FormatModifier>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PropertyBlob {
    pub id: DrmId,
    pub bytes: Vec<u8>,
    pub format_modifier: Option<FormatModifierBlob>,
    pub damage_clips: Vec<ModeRect>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaneSizeHint {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeRect {
    pub x1: u32,
    pub y1: u32,
    pub x2: u32,
    pub y2: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaneState {
    pub plane: DrmId,
    pub crtc: Option<DrmId>,
    pub fb: Option<DrmId>,
    pub rotation: u64,
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub fb_damage_clips: Option<DrmId>,
}

#[derive(Clone, Debug, Default)]
pub struct Plane {
    pub id: DrmId,
    pub device_id: DrmId,
    pub index: usize,
    pub possible_crtcs: u32,
    pub kind: u32,
    pub funcs: PlaneFuncs,
    pub name: String,
    pub format_types: Vec<u32>,
    pub modifiers: Vec<u64>,
    pub properties: Vec<PropertyValue>,
    pub hotspot_x_property: Option<DrmId>,
    pub hotspot_y_property: Option<DrmId>,
    pub scaling_filter_property: Option<DrmId>,
    pub color_pipeline_property: Option<DrmId>,
    pub zpos_property: Option<DrmId>,
    pub state: Option<PlaneState>,
    pub legacy_crtc: Option<DrmId>,
    pub legacy_fb: Option<DrmId>,
    pub old_fb: Option<DrmId>,
}

#[derive(Clone, Debug, Default)]
pub struct Framebuffer {
    pub id: DrmId,
    pub format: u32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    pub refs: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CrtcFuncs {
    pub cursor_set: bool,
    pub cursor_set2: bool,
    pub cursor_move: bool,
    pub page_flip: bool,
    pub page_flip_target: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Crtc {
    pub id: DrmId,
    pub device_id: DrmId,
    pub primary_plane: Option<DrmId>,
    pub cursor_plane: Option<DrmId>,
    pub funcs: CrtcFuncs,
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub x: i32,
    pub y: i32,
    pub mode_width: u32,
    pub mode_height: u32,
}

#[derive(Clone, Debug, Default)]
pub struct DrmFile {
    pub id: DrmId,
    pub universal_planes: bool,
    pub atomic: bool,
    pub supports_virtualized_cursor_plane: bool,
    pub leases: Vec<DrmId>,
}

#[derive(Clone, Debug, Default)]
pub struct DrmDevice {
    pub id: DrmId,
    pub modeset: bool,
    pub atomic_modeset: bool,
    pub cursor_hotspot: bool,
    pub mode_config: ModeConfig,
    pub planes: Vec<Plane>,
    pub crtcs: Vec<Crtc>,
    pub framebuffers: Vec<Framebuffer>,
    pub blobs: Vec<PropertyBlob>,
    pub next_id: DrmId,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ModesetContext {
    pub id: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PlaneUpdate {
    pub crtc: DrmId,
    pub fb: DrmId,
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ModeGetPlaneRes {
    pub count_planes: u32,
    pub plane_id_ptr: UserPtr,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ModeGetPlane {
    pub plane_id: DrmId,
    pub crtc_id: DrmId,
    pub fb_id: DrmId,
    pub possible_crtcs: u32,
    pub gamma_size: u32,
    pub count_format_types: u32,
    pub format_type_ptr: UserPtr,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ModeSetPlane {
    pub plane_id: DrmId,
    pub crtc_id: DrmId,
    pub fb_id: DrmId,
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ModeCursor2 {
    pub crtc_id: DrmId,
    pub flags: u32,
    pub handle: DrmId,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    pub hot_x: i32,
    pub hot_y: i32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ModeCursor {
    pub crtc_id: DrmId,
    pub flags: u32,
    pub handle: DrmId,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PageFlipRequest {
    pub crtc_id: DrmId,
    pub fb_id: DrmId,
    pub flags: u32,
    pub sequence: u32,
    pub user_data: u64,
}

#[derive(Clone, Debug, Default)]
pub struct FlipEvent {
    pub event_type: u32,
    pub length: u32,
    pub user_data: u64,
    pub crtc_id: DrmId,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PropEnum {
    pub value: u32,
    pub name: &'static str,
}

#[derive(Clone, Debug)]
pub struct PlaneInit {
    pub plane_id: DrmId,
    pub possible_crtcs: u32,
    pub funcs: PlaneFuncs,
    pub formats: Vec<u32>,
    /// Optional list terminated by `DRM_FORMAT_MOD_INVALID`.
    pub modifiers: Option<Vec<u64>>,
    pub kind: u32,
    pub name: Option<String>,
}

/// All external DRM core services used by drm_plane.c. Defaults are inert;
/// integrations must override operations whose side effects they require.
pub trait DrmPlaneIo {
    fn warn(&mut self, _device: DrmId, _message: &'static str) {}
    fn error(&mut self, _device: DrmId, _message: &'static str) {}
    fn debug(&mut self, _device: DrmId, _message: &'static str) {}
    fn mode_object_add(&mut self, _device: DrmId, _object: DrmId) -> i32 { 0 }
    fn mode_object_unregister(&mut self, _device: DrmId, _object: DrmId) {}
    fn plane_mutex_init(&mut self, _plane: DrmId) {}
    fn plane_mutex_fini(&mut self, _plane: DrmId) {}
    fn plane_lock(&mut self, _plane: DrmId, _ctx: Option<&mut ModesetContext>) -> i32 { 0 }
    fn plane_unlock(&mut self, _plane: DrmId) {}
    fn crtc_lock(&mut self, _crtc: DrmId, _ctx: &mut ModesetContext) -> i32 { 0 }
    fn crtc_unlock(&mut self, _crtc: DrmId) {}
    fn acquire_init(&mut self, _ctx: &mut ModesetContext) {}
    fn lock_all(&mut self, _device: DrmId, _ctx: &mut ModesetContext) -> i32 { 0 }
    fn unlock_all(&mut self, _device: DrmId, _ctx: &mut ModesetContext, _ret: i32) {}
    fn modeset_backoff(&mut self, _ctx: &mut ModesetContext) -> i32 { 0 }
    fn drop_locks(&mut self, _ctx: &mut ModesetContext) {}
    fn acquire_fini(&mut self, _ctx: &mut ModesetContext) {}
    fn attach_property(&mut self, _object: DrmId, _property: DrmId, _value: u64) {}
    fn property_create(&mut self, _device: DrmId, _name: &'static str, _flags: u32,
                       _enum_count: usize) -> PlaneResult<DrmId> { Ok(0) }
    fn property_add_enum(&mut self, _property: DrmId, _value: u32, _name: &'static str) -> i32 { 0 }
    fn property_destroy(&mut self, _device: DrmId, _property: DrmId) {}
    fn property_set_value(&mut self, _object: DrmId, _property: DrmId, _value: u64) {}
    fn create_blob(&mut self, _device: DrmId, _blob: &PropertyBlob) -> PlaneResult<DrmId> { Ok(0) }
    fn destroy_blob(&mut self, _device: DrmId, _blob: DrmId) {}
    fn format_mod_supported(&mut self, _plane: DrmId, _format: u32, _modifier: u64,
                            _asynchronous: bool) -> Option<bool> { None }
    fn plane_late_register(&mut self, _plane: DrmId) -> i32 { 0 }
    fn plane_early_unregister(&mut self, _plane: DrmId) {}
    fn atomic_destroy_state(&mut self, _plane: DrmId, _state: &PlaneState) {}
    fn set_property(&mut self, _plane: DrmId, _property: DrmId, _value: u64) -> i32 { EINVAL }
    fn disable_plane(&mut self, _plane: DrmId, _ctx: Option<&mut ModesetContext>) -> i32 { EOPNOTSUPP }
    fn update_plane(&mut self, _plane: DrmId, _update: PlaneUpdate,
                    _ctx: Option<&mut ModesetContext>) -> i32 { EOPNOTSUPP }
    fn lease_held(&mut self, _file: DrmId, _object: DrmId) -> bool { true }
    fn filter_crtcs(&mut self, _file: DrmId, mask: u32) -> u32 { mask }
    fn put_user_u32(&mut self, _ptr: UserPtr, _index: usize, _value: u32) -> bool { true }
    fn copy_formats_to_user(&mut self, _ptr: UserPtr, _formats: &[u32]) -> bool { true }
    fn find_plane(&mut self, device: &DrmDevice, file: &DrmFile, id: DrmId) -> Option<usize> {
        device.planes.iter().position(|p| p.id == id && self.lease_held(file.id, id))
    }
    fn framebuffer_lookup(&mut self, device: &mut DrmDevice, file: &DrmFile,
                          id: DrmId) -> Option<DrmId> {
        if !self.lease_held(file.id, id) { return None; }
        let found = device.framebuffers.iter().position(|fb| fb.id == id)?;
        device.framebuffers[found].refs += 1;
        Some(id)
    }
    fn crtc_find(&mut self, device: &DrmDevice, file: &DrmFile, id: DrmId) -> Option<usize> {
        if !self.lease_held(file.id, id) { return None; }
        device.crtcs.iter().position(|crtc| crtc.id == id)
    }
    fn framebuffer_get(&mut self, device: &mut DrmDevice, fb: DrmId) {
        if let Some(fb) = device.framebuffers.iter_mut().find(|f| f.id == fb) { fb.refs += 1; }
    }
    fn framebuffer_put(&mut self, device: &mut DrmDevice, fb: DrmId) {
        if let Some(fb) = device.framebuffers.iter_mut().find(|f| f.id == fb) { fb.refs = fb.refs.saturating_sub(1); }
    }
    fn check_src_coords(&mut self, src_x: u32, src_y: u32, src_w: u32, src_h: u32,
                        fb: &Framebuffer) -> i32 {
        let max_w = (fb.width as u64) << 16;
        let max_h = (fb.height as u64) << 16;
        let end_x = src_x as u64 + src_w as u64;
        let end_y = src_y as u64 + src_h as u64;
        if src_w == 0 || src_h == 0 || end_x > max_w || end_y > max_h { EINVAL } else { 0 }
    }
    fn crtc_check_viewport(&mut self, _crtc: &Crtc, _fb: &Framebuffer) -> i32 { 0 }
    fn cursor_framebuffer_create(&mut self, _device: &mut DrmDevice, _file: &DrmFile,
                                 _width: u32, _height: u32, _handle: DrmId) -> PlaneResult<DrmId> { Err(EOPNOTSUPP) }
    fn cursor_set(&mut self, _crtc: DrmId, _file: DrmId, _handle: DrmId,
                  _width: u32, _height: u32, _hot_x: i32, _hot_y: i32, _v2: bool) -> i32 { EOPNOTSUPP }
    fn cursor_move(&mut self, _crtc: DrmId, _x: i32, _y: i32) -> i32 { EOPNOTSUPP }
    fn page_flip(&mut self, _crtc: DrmId, _fb: DrmId, _event: Option<&FlipEvent>,
                 _flags: u32, _target: u32, _ctx: &mut ModesetContext, _target_fn: bool) -> i32 { EOPNOTSUPP }
    fn vblank_get(&mut self, _crtc: DrmId) -> i32 { 0 }
    fn vblank_count(&mut self, _crtc: DrmId) -> u32 { 0 }
    fn vblank_put(&mut self, _crtc: DrmId) {}
    fn reserve_event(&mut self, _file: DrmId, _event: &FlipEvent) -> i32 { 0 }
    fn cancel_event(&mut self, _device: DrmId, _event: &FlipEvent) {}
    fn allocate_container(&mut self, _device: DrmId, _size: usize) -> PlaneResult<DrmId> { Ok(0) }
    fn bind_plane_container(&mut self, _device: DrmId, _container: DrmId,
                            _offset: usize, _plane: DrmId) -> i32 { 0 }
    fn free_container(&mut self, _device: DrmId, _container: DrmId) {}
    fn managed_add_plane_action(&mut self, _device: DrmId, _plane: DrmId) -> i32 { 0 }
}

fn plane_index(device: &DrmDevice, plane: DrmId) -> Option<usize> {
    device.planes.iter().position(|p| p.id == plane)
}
fn alloc_id(device: &mut DrmDevice) -> DrmId {
    device.next_id = device.next_id.wrapping_add(1).max(1);
    device.next_id
}
fn property_set(plane: &mut Plane, property: DrmId, value: u64) {
    if let Some(entry) = plane.properties.iter_mut().find(|entry| entry.property == property) {
        entry.value = value;
    } else {
        plane.properties.push(PropertyValue { property, value });
    }
}
fn plane_format_supported<I: DrmPlaneIo>(io: &mut I, plane: &Plane, format: u32, modifier: u64,
                                          asynchronous: bool) -> bool {
    if !plane.format_types.contains(&format) { return false; }
    let callback = if asynchronous { plane.funcs.format_mod_supported_async }
                   else { plane.funcs.format_mod_supported };
    if callback {
        return io.format_mod_supported(plane.id, format, modifier, asynchronous).unwrap_or(false);
    }
    if plane.modifiers.is_empty() { return true; }
    plane.modifiers.contains(&modifier)
}

// upstream: drm_plane.c drm_num_planes()
pub fn drm_num_planes(device: &DrmDevice) -> usize {
    device.planes.len()
}

// upstream: drm_plane.c formats_ptr()
pub fn formats_ptr(blob: &FormatModifierBlob) -> usize {
    blob.formats_offset as usize
}

// upstream: drm_plane.c modifiers_ptr()
pub fn modifiers_ptr(blob: &FormatModifierBlob) -> usize {
    blob.modifiers_offset as usize
}

// upstream: drm_plane.c create_in_format_blob()
pub fn create_in_format_blob<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice, plane_id: DrmId,
                                             asynchronous: bool) -> PlaneResult<DrmId> {
    let index = plane_index(device, plane_id).ok_or(ENOENT)?;
    let plane = device.planes[index].clone();
    let formats_size = plane.format_types.len().checked_mul(4).ok_or(ENOMEM)?;
    if formats_size == 0 {
        io.warn(device.id, "plane has no formats");
        return Err(EINVAL);
    }
    let modifiers_size = plane.modifiers.len().checked_mul(24).ok_or(ENOMEM)?;
    let formats_aligned = formats_size.checked_add(7).ok_or(ENOMEM)? & !7;
    let blob_size = FORMAT_MODIFIER_BLOB_HEADER_SIZE
        .checked_add(formats_aligned).and_then(|n| n.checked_add(modifiers_size)).ok_or(ENOMEM)?;
    let formats_offset = FORMAT_MODIFIER_BLOB_HEADER_SIZE;
    let modifiers_offset = formats_offset.checked_add(formats_size).ok_or(ENOMEM)?
        .checked_add(7).ok_or(ENOMEM)? & !7;
    let mut modifiers = Vec::new();
    modifiers.try_reserve_exact(plane.modifiers.len()).map_err(|_| ENOMEM)?;
    for &modifier in &plane.modifiers {
        let mut format_mask = 0u64;
        for (j, &format) in plane.format_types.iter().enumerate() {
            let callback_present = if asynchronous { plane.funcs.format_mod_supported_async }
                                   else { plane.funcs.format_mod_supported };
            if !callback_present || io.format_mod_supported(plane.id, format, modifier, asynchronous).unwrap_or(false) {
                if j < 64 { format_mask |= 1u64 << j; }
            }
        }
        modifiers.push(FormatModifier { formats: format_mask, modifier, offset: 0, pad: 0 });
    }
    let mut blob = PropertyBlob::default();
    blob.id = alloc_id(device);
    blob.bytes.try_reserve_exact(blob_size).map_err(|_| ENOMEM)?;
    blob.bytes.resize(blob_size, 0);
    let format_blob = FormatModifierBlob {
        version: FORMAT_BLOB_CURRENT,
        flags: 0,
        count_formats: plane.format_types.len() as u32,
        formats_offset: formats_offset as u32,
        count_modifiers: plane.modifiers.len() as u32,
        modifiers_offset: modifiers_offset as u32,
        formats: plane.format_types,
        modifiers,
    };
    blob.bytes[0..4].copy_from_slice(&format_blob.version.to_ne_bytes());
    blob.bytes[4..8].copy_from_slice(&format_blob.flags.to_ne_bytes());
    blob.bytes[8..12].copy_from_slice(&format_blob.count_formats.to_ne_bytes());
    blob.bytes[12..16].copy_from_slice(&format_blob.formats_offset.to_ne_bytes());
    blob.bytes[16..20].copy_from_slice(&format_blob.count_modifiers.to_ne_bytes());
    blob.bytes[20..24].copy_from_slice(&format_blob.modifiers_offset.to_ne_bytes());
    let formats_begin = format_blob.formats_offset as usize;
    for (i, format) in format_blob.formats.iter().enumerate() {
        let begin = formats_begin + i * 4;
        blob.bytes[begin..begin + 4].copy_from_slice(&format.to_ne_bytes());
    }
    let modifiers_begin = format_blob.modifiers_offset as usize;
    for (i, modifier) in format_blob.modifiers.iter().enumerate() {
        let begin = modifiers_begin + i * 24;
        blob.bytes[begin..begin + 8].copy_from_slice(&modifier.formats.to_ne_bytes());
        blob.bytes[begin + 8..begin + 12].copy_from_slice(&modifier.offset.to_ne_bytes());
        blob.bytes[begin + 12..begin + 16].copy_from_slice(&modifier.pad.to_ne_bytes());
        blob.bytes[begin + 16..begin + 24].copy_from_slice(&modifier.modifier.to_ne_bytes());
    }
    blob.format_modifier = Some(format_blob);
    let id = io.create_blob(device.id, &blob)?;
    if id != 0 { blob.id = id; }
    let result_id = blob.id;
    device.blobs.push(blob);
    Ok(result_id)
}

// upstream: drm_plane.c drm_plane_create_hotspot_properties()
pub fn drm_plane_create_hotspot_properties<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                                            plane_id: DrmId) -> i32 {
    let Some(index) = plane_index(device, plane_id) else { return ENOENT };
    if !device.cursor_hotspot { io.warn(device.id, "hotspot properties without cursor-hotspot feature"); }
    let prop_x = match io.property_create(device.id, "HOTSPOT_X", 0, 0) {
        Ok(id) => id, Err(_) => return ENOMEM,
    };
    let prop_y = match io.property_create(device.id, "HOTSPOT_Y", 0, 0) {
        Ok(id) => id,
        Err(_) => { io.property_destroy(device.id, prop_x); return ENOMEM; }
    };
    io.attach_property(plane_id, prop_x, 0);
    io.attach_property(plane_id, prop_y, 0);
    property_set(&mut device.planes[index], prop_x, 0);
    property_set(&mut device.planes[index], prop_y, 0);
    device.planes[index].hotspot_x_property = Some(prop_x);
    device.planes[index].hotspot_y_property = Some(prop_y);
    0
}

// upstream: drm_plane.c __drm_universal_plane_init()
pub fn __drm_universal_plane_init<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                                  init: PlaneInit) -> i32 {
    let plane_id = init.plane_id;
    let preexisting_plane_count = drm_num_planes(device);
    if device.mode_config.num_total_plane >= 32 {
        io.warn(device.id, "plane count exceeds 32-bit plane masks");
        return EINVAL;
    }
    if init.formats.len() > 64 {
        io.warn(device.id, "plane format count exceeds 64-bit format masks");
        return EINVAL;
    }
    if device.atomic_modeset && (!init.funcs.atomic_destroy_state || !init.funcs.atomic_duplicate_state) {
        io.warn(device.id, "atomic plane is missing state callbacks");
    }
    let ret = io.mode_object_add(device.id, plane_id);
    if ret != 0 { return ret; }
    io.plane_mutex_init(plane_id);

    let mut formats = Vec::new();
    if formats.try_reserve_exact(init.formats.len()).is_err() {
        io.error(device.id, "out of memory allocating plane formats");
        io.mode_object_unregister(device.id, plane_id);
        return ENOMEM;
    }
    formats.extend_from_slice(&init.formats);
    let (modifier_values, modifier_count) = if let Some(input) = init.modifiers.as_ref() {
        let count = input.iter().position(|m| *m == DRM_FORMAT_MOD_INVALID).unwrap_or(input.len());
        (input[..count].to_vec(), count)
    } else if !device.mode_config.fb_modifiers_not_supported {
        (vec![DRM_FORMAT_MOD_LINEAR], 1)
    } else { (Vec::new(), 0) };
    if device.mode_config.fb_modifiers_not_supported && modifier_count != 0 {
        io.warn(device.id, "modifiers supplied while framebuffer modifiers are disabled");
    }
    let async_format_callback = init.funcs.format_mod_supported_async;
    let funcs = init.funcs.clone();
    let mut plane = Plane {
        id: plane_id,
        device_id: device.id,
        index: device.mode_config.num_total_plane,
        possible_crtcs: init.possible_crtcs,
        kind: init.kind,
        funcs,
        name: init.name.unwrap_or_else(|| alloc::format!("plane-{}", preexisting_plane_count)),
        format_types: formats,
        modifiers: Vec::new(),
        ..Plane::default()
    };
    if modifier_count != 0 {
        if plane.modifiers.try_reserve_exact(modifier_count).is_err() {
            io.error(device.id, "out of memory allocating plane modifiers");
            io.mode_object_unregister(device.id, plane_id);
            return ENOMEM;
        }
        plane.modifiers.extend_from_slice(&modifier_values);
    }
    if plane.name.is_empty() {
        io.mode_object_unregister(device.id, plane_id);
        return ENOMEM;
    }
    let index = device.planes.len();
    device.planes.push(plane);
    device.mode_config.num_total_plane += 1;
    let type_prop = device.mode_config.properties.type_property;
    io.attach_property(plane_id, type_prop, init.kind as u64);
    property_set(&mut device.planes[index], type_prop, init.kind as u64);

    if device.atomic_modeset {
        let props = device.mode_config.properties;
        for property in [props.fb_id, props.in_fence_fd, props.crtc_id, props.crtc_x,
                         props.crtc_y, props.crtc_w, props.crtc_h, props.src_x,
                         props.src_y, props.src_w, props.src_h] {
            let value = if property == props.in_fence_fd { u64::MAX } else { 0 };
            io.attach_property(plane_id, property, value);
            property_set(&mut device.planes[index], property, value);
        }
    }
    if device.cursor_hotspot && init.kind == DRM_PLANE_TYPE_CURSOR {
        let _ = drm_plane_create_hotspot_properties(io, device, plane_id);
    }
    if modifier_count != 0 {
        if let Ok(blob) = create_in_format_blob(io, device, plane_id, false) {
            let property = device.mode_config.properties.modifiers;
            io.attach_property(plane_id, property, blob as u64);
            if let Some(i) = plane_index(device, plane_id) { property_set(&mut device.planes[i], property, blob as u64); }
        }
    }
    if async_format_callback {
        if let Ok(blob) = create_in_format_blob(io, device, plane_id, true) {
            let property = device.mode_config.properties.async_modifiers;
            io.attach_property(plane_id, property, blob as u64);
            if let Some(i) = plane_index(device, plane_id) { property_set(&mut device.planes[i], property, blob as u64); }
        }
    }
    0
}

// upstream: drm_plane.c drm_universal_plane_init()
pub fn drm_universal_plane_init<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                                init: PlaneInit) -> i32 {
    if !init.funcs.destroy { io.warn(device.id, "plane destroy callback is missing"); }
    __drm_universal_plane_init(io, device, init)
}

// upstream: drm_plane.c drmm_universal_plane_alloc_release()
pub fn drmm_universal_plane_alloc_release<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                                           plane_id: DrmId) {
    let Some(index) = plane_index(device, plane_id) else { return };
    if device.planes[index].device_id == 0 {
        io.warn(device.id, "managed plane was not initialized");
        return;
    }
    drm_plane_cleanup(io, device, plane_id);
}

// upstream: drm_plane.c __drmm_universal_plane_alloc()
pub fn __drmm_universal_plane_alloc<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                                    init: PlaneInit, size: usize,
                                                    _offset: usize) -> PlaneResult<DrmId> {
    if init.funcs.destroy { return Err(EINVAL); }
    let container = io.allocate_container(device.id, size)?;
    let plane_id = init.plane_id;
    let ret = io.bind_plane_container(device.id, container, _offset, plane_id);
    if ret != 0 { io.free_container(device.id, container); return Err(ret); }
    let ret = __drm_universal_plane_init(io, device, init);
    if ret != 0 { io.free_container(device.id, container); return Err(ret); }
    let ret = io.managed_add_plane_action(device.id, plane_id);
    if ret != 0 {
        drmm_universal_plane_alloc_release(io, device, plane_id);
        io.free_container(device.id, container);
        return Err(ret);
    }
    Ok(container)
}

// upstream: drm_plane.c __drm_universal_plane_alloc()
pub fn __drm_universal_plane_alloc<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                                   init: PlaneInit, size: usize,
                                                   _offset: usize) -> PlaneResult<DrmId> {
    let container = io.allocate_container(device.id, size)?;
    let ret = io.bind_plane_container(device.id, container, _offset, init.plane_id);
    if ret != 0 { io.free_container(device.id, container); return Err(ret); }
    let ret = __drm_universal_plane_init(io, device, init);
    if ret != 0 { io.free_container(device.id, container); return Err(ret); }
    Ok(container)
}

// upstream: drm_plane.c drm_plane_register_all()
pub fn drm_plane_register_all<I: DrmPlaneIo>(io: &mut I, device: &DrmDevice) -> i32 {
    let mut num_planes = 0usize;
    let mut num_zpos = 0usize;
    for plane in &device.planes {
        if plane.funcs.late_register {
            let ret = io.plane_late_register(plane.id);
            if ret != 0 { return ret; }
        }
        if plane.zpos_property.is_some() { num_zpos += 1; }
        num_planes += 1;
    }
    if num_zpos != 0 && num_planes != num_zpos {
        io.warn(device.id, "mixing planes with and without zpos property is invalid");
    }
    0
}

// upstream: drm_plane.c drm_plane_unregister_all()
pub fn drm_plane_unregister_all<I: DrmPlaneIo>(io: &mut I, device: &DrmDevice) {
    for plane in &device.planes {
        if plane.funcs.early_unregister { io.plane_early_unregister(plane.id); }
    }
}

// upstream: drm_plane.c drm_plane_cleanup()
pub fn drm_plane_cleanup<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice, plane_id: DrmId) {
    let Some(index) = plane_index(device, plane_id) else { return };
    let plane = device.planes[index].clone();
    io.plane_mutex_fini(plane_id);
    io.mode_object_unregister(device.id, plane_id);
    if device.planes.is_empty() {
        io.warn(device.id, "plane list unexpectedly empty during cleanup");
        return;
    }
    device.planes.remove(index);
    device.mode_config.num_total_plane = device.mode_config.num_total_plane.saturating_sub(1);
    if let Some(state) = plane.state.as_ref() {
        if !plane.funcs.atomic_destroy_state { io.warn(device.id, "plane state without atomic destroy callback"); }
        else { io.atomic_destroy_state(plane_id, &state); }
    }
    let blob_ids: Vec<DrmId> = device.blobs.iter().filter(|blob| {
        plane.properties.iter().any(|p| p.property == device.mode_config.properties.modifiers && p.value == blob.id as u64)
            || plane.properties.iter().any(|p| p.property == device.mode_config.properties.async_modifiers && p.value == blob.id as u64)
    }).map(|blob| blob.id).collect();
    for blob in &blob_ids { io.destroy_blob(device.id, *blob); }
    device.blobs.retain(|blob| !blob_ids.contains(&blob.id));
}

// upstream: drm_plane.c drm_plane_from_index()
pub fn drm_plane_from_index(device: &DrmDevice, index: i32) -> Option<DrmId> {
    if index < 0 { return None; }
    device.planes.iter().find(|plane| plane.index == index as usize).map(|plane| plane.id)
}

// upstream: drm_plane.c drm_plane_force_disable()
pub fn drm_plane_force_disable<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                               plane_id: DrmId) {
    let Some(index) = plane_index(device, plane_id) else { return };
    let fb = device.planes[index].legacy_fb;
    if fb.is_none() { return; }
    if device.atomic_modeset { io.warn(device.id, "force-disable used with atomic modesetting"); }
    device.planes[index].old_fb = fb;
    let ret = io.disable_plane(plane_id, None);
    if ret != 0 {
        io.error(device.id, "failed to disable plane with busy framebuffer");
        device.planes[index].old_fb = None;
        return;
    }
    if let Some(old) = device.planes[index].old_fb { io.framebuffer_put(device, old); }
    if let Some(index) = plane_index(device, plane_id) {
        device.planes[index].old_fb = None;
        device.planes[index].legacy_fb = None;
        device.planes[index].legacy_crtc = None;
    }
}

// upstream: drm_plane.c drm_mode_plane_set_obj_prop()
pub fn drm_mode_plane_set_obj_prop<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                                   plane_id: DrmId, property: DrmId,
                                                   value: u64) -> i32 {
    let Some(index) = plane_index(device, plane_id) else { return EINVAL };
    let mut ret = EINVAL;
    if device.planes[index].funcs.set_property { ret = io.set_property(plane_id, property, value); }
    if ret == 0 {
        io.property_set_value(plane_id, property, value);
        if let Some(index) = plane_index(device, plane_id) { property_set(&mut device.planes[index], property, value); }
    }
    ret
}

// upstream: drm_plane.c drm_mode_getplane_res()
pub fn drm_mode_getplane_res<I: DrmPlaneIo>(io: &mut I, device: &DrmDevice, file: &DrmFile,
                                             request: &mut ModeGetPlaneRes) -> i32 {
    if !device.modeset { return EOPNOTSUPP; }
    let mut count = 0usize;
    for plane in &device.planes {
        if plane.kind != DRM_PLANE_TYPE_OVERLAY && !file.universal_planes { continue; }
        if plane.kind == DRM_PLANE_TYPE_CURSOR && device.cursor_hotspot && file.atomic
            && !file.supports_virtualized_cursor_plane { continue; }
        if io.lease_held(file.id, plane.id) {
            if count < request.count_planes as usize
                && !io.put_user_u32(request.plane_id_ptr, count, plane.id) { return EFAULT; }
            count += 1;
        }
    }
    request.count_planes = count as u32;
    0
}

// upstream: drm_plane.c drm_mode_getplane()
pub fn drm_mode_getplane<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice, file: &DrmFile,
                                         request: &mut ModeGetPlane) -> i32 {
    if !device.modeset { return EOPNOTSUPP; }
    let Some(index) = io.find_plane(device, file, request.plane_id) else { return ENOENT };
    let plane_id = device.planes[index].id;
    let ret = io.plane_lock(plane_id, None);
    if ret != 0 { return ret; }
    let plane = device.planes[index].clone();
    let crtc_id = if let Some(state) = plane.state.as_ref() {
        state.crtc.filter(|id| io.lease_held(file.id, *id)).unwrap_or(0)
    } else {
        plane.legacy_crtc.filter(|id| io.lease_held(file.id, *id)).unwrap_or(0)
    };
    let fb_id = if let Some(state) = plane.state.as_ref() { state.fb.unwrap_or(0) }
                else { plane.legacy_fb.unwrap_or(0) };
    io.plane_unlock(plane_id);

    request.crtc_id = crtc_id;
    request.fb_id = fb_id;
    request.plane_id = plane.id;
    request.possible_crtcs = io.filter_crtcs(file.id, plane.possible_crtcs);
    request.gamma_size = 0;
    if !plane.format_types.is_empty() && request.count_format_types as usize >= plane.format_types.len()
        && !io.copy_formats_to_user(request.format_type_ptr, &plane.format_types) { return EFAULT; }
    request.count_format_types = plane.format_types.len() as u32;
    0
}

// upstream: drm_plane.c drm_plane_has_format()
pub fn drm_plane_has_format<I: DrmPlaneIo>(io: &mut I, device: &DrmDevice, plane_id: DrmId,
                                            format: u32, modifier: u64) -> bool {
    let Some(index) = plane_index(device, plane_id) else { return false };
    plane_format_supported(io, &device.planes[index], format, modifier, false)
}

// upstream: drm_plane.c __setplane_check()
pub fn __setplane_check<I: DrmPlaneIo>(io: &mut I, device: &DrmDevice, plane_id: DrmId,
                                        crtc_id: DrmId, fb_id: DrmId, update: PlaneUpdate) -> i32 {
    let Some(pidx) = plane_index(device, plane_id) else { return EINVAL };
    let Some(fb) = device.framebuffers.iter().find(|fb| fb.id == fb_id) else { return EINVAL };
    let plane = &device.planes[pidx];
    let Some(crtc_index) = device.crtcs.iter().position(|c| c.id == crtc_id) else { return EINVAL };
    if plane.possible_crtcs & (1u32.checked_shl(crtc_index as u32).unwrap_or(0)) == 0 {
        io.debug(device.id, "invalid crtc for plane");
        return EINVAL;
    }
    if !plane_format_supported(io, plane, fb.format, fb.modifier, false) {
        io.debug(device.id, "invalid pixel format or modifier for plane");
        return EINVAL;
    }
    if update.crtc_w > i32::MAX as u32
        || update.crtc_x > i32::MAX.saturating_sub(update.crtc_w as i32)
        || update.crtc_h > i32::MAX as u32
        || update.crtc_y > i32::MAX.saturating_sub(update.crtc_h as i32) {
        io.debug(device.id, "invalid crtc coordinates");
        return ERANGE;
    }
    io.check_src_coords(update.src_x, update.src_y, update.src_w, update.src_h, fb)
}

// upstream: drm_plane.c drm_any_plane_has_format()
pub fn drm_any_plane_has_format<I: DrmPlaneIo>(io: &mut I, device: &DrmDevice,
                                                format: u32, modifier: u64) -> bool {
    device.planes.iter().any(|plane| plane_format_supported(io, plane, format, modifier, false))
}

// upstream: drm_plane.c __setplane_internal()
pub fn __setplane_internal<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                           plane_id: DrmId, crtc_id: Option<DrmId>,
                                           fb_id: Option<DrmId>, update: PlaneUpdate,
                                           ctx: &mut ModesetContext) -> i32 {
    let Some(index) = plane_index(device, plane_id) else { return EINVAL };
    if device.atomic_modeset { io.warn(device.id, "legacy plane update used by atomic driver"); }
    if fb_id.is_none() {
        device.planes[index].old_fb = device.planes[index].legacy_fb;
        let ret = io.disable_plane(plane_id, Some(ctx));
        if ret == 0 {
            device.planes[index].legacy_crtc = None;
            device.planes[index].legacy_fb = None;
        } else {
            device.planes[index].old_fb = None;
        }
        if let Some(old) = device.planes[index].old_fb { io.framebuffer_put(device, old); }
        if let Some(index) = plane_index(device, plane_id) { device.planes[index].old_fb = None; }
        return ret;
    }
    let fb_id = fb_id.unwrap();
    let Some(crtc_id) = crtc_id else { return EINVAL };
    let ret = __setplane_check(io, device, plane_id, crtc_id, fb_id, update);
    if ret != 0 { return ret; }
    device.planes[index].old_fb = device.planes[index].legacy_fb;
    let ret = io.update_plane(plane_id, PlaneUpdate { crtc: crtc_id, fb: fb_id, ..update }, Some(ctx));
    if ret == 0 {
        device.planes[index].legacy_crtc = Some(crtc_id);
        device.planes[index].legacy_fb = Some(fb_id);
        io.framebuffer_get(device, fb_id);
    } else {
        device.planes[index].old_fb = None;
    }
    if let Some(old) = device.planes[index].old_fb { io.framebuffer_put(device, old); }
    if let Some(index) = plane_index(device, plane_id) { device.planes[index].old_fb = None; }
    ret
}

// upstream: drm_plane.c __setplane_atomic()
pub fn __setplane_atomic<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                         plane_id: DrmId, crtc_id: Option<DrmId>,
                                         fb_id: Option<DrmId>, update: PlaneUpdate,
                                         ctx: &mut ModesetContext) -> i32 {
    if !device.atomic_modeset { io.warn(device.id, "atomic plane update used by legacy driver"); }
    let Some(fb_id) = fb_id else { return io.disable_plane(plane_id, Some(ctx)); };
    let Some(crtc_id) = crtc_id else { return EINVAL };
    let ret = __setplane_check(io, device, plane_id, crtc_id, fb_id, update);
    if ret != 0 { return ret; }
    io.update_plane(plane_id, PlaneUpdate { crtc: crtc_id, fb: fb_id, ..update }, Some(ctx))
}

// upstream: drm_plane.c setplane_internal()
pub fn setplane_internal<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                         plane_id: DrmId, crtc_id: Option<DrmId>,
                                         fb_id: Option<DrmId>, update: PlaneUpdate) -> i32 {
    let mut ctx = ModesetContext::default();
    io.acquire_init(&mut ctx);
    loop {
        let lock_ret = io.lock_all(device.id, &mut ctx);
        if lock_ret == EDEADLK {
            let ret = io.modeset_backoff(&mut ctx);
            if ret == 0 { continue; }
            io.drop_locks(&mut ctx);
            io.acquire_fini(&mut ctx);
            return ret;
        }
        if lock_ret != 0 {
            io.acquire_fini(&mut ctx);
            return lock_ret;
        }
        let ret = if device.atomic_modeset {
            __setplane_atomic(io, device, plane_id, crtc_id, fb_id, update, &mut ctx)
        } else {
            __setplane_internal(io, device, plane_id, crtc_id, fb_id, update, &mut ctx)
        };
        io.unlock_all(device.id, &mut ctx, ret);
        if ret != EDEADLK { io.drop_locks(&mut ctx); io.acquire_fini(&mut ctx); return ret; }
        let ret = io.modeset_backoff(&mut ctx);
        if ret != 0 { io.drop_locks(&mut ctx); io.acquire_fini(&mut ctx); return ret; }
    }
}

// upstream: drm_plane.c drm_mode_setplane()
pub fn drm_mode_setplane<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice, file: &DrmFile,
                                         request: ModeSetPlane) -> i32 {
    if !device.modeset { return EOPNOTSUPP; }
    let Some(pidx) = io.find_plane(device, file, request.plane_id) else {
        io.debug(device.id, "unknown plane id");
        return ENOENT;
    };
    let plane_id = device.planes[pidx].id;
    let mut fb_id = None;
    let mut crtc_id = None;
    if request.fb_id != 0 {
        fb_id = io.framebuffer_lookup(device, file, request.fb_id);
        if fb_id.is_none() { io.debug(device.id, "unknown framebuffer id"); return ENOENT; }
        let Some(cidx) = io.crtc_find(device, file, request.crtc_id) else {
            if let Some(fb) = fb_id { io.framebuffer_put(device, fb); }
            io.debug(device.id, "unknown crtc id");
            return ENOENT;
        };
        crtc_id = Some(device.crtcs[cidx].id);
    }
    let update = PlaneUpdate { crtc: crtc_id.unwrap_or(0), fb: fb_id.unwrap_or(0),
        crtc_x: request.crtc_x, crtc_y: request.crtc_y, crtc_w: request.crtc_w,
        crtc_h: request.crtc_h, src_x: request.src_x, src_y: request.src_y,
        src_w: request.src_w, src_h: request.src_h };
    let ret = setplane_internal(io, device, plane_id, crtc_id, fb_id, update);
    if let Some(fb) = fb_id { io.framebuffer_put(device, fb); }
    ret
}

// upstream: drm_plane.c drm_mode_cursor_universal()
pub fn drm_mode_cursor_universal<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                                 crtc_id: DrmId, request: &ModeCursor2,
                                                 file: &DrmFile, ctx: &mut ModesetContext) -> i32 {
    let Some(cidx) = device.crtcs.iter().position(|crtc| crtc.id == crtc_id) else { return ENOENT };
    let Some(plane_id) = device.crtcs[cidx].cursor_plane else {
        io.warn(device.id, "universal cursor path has no cursor plane");
        return ENODEV;
    };
    if device.planes.iter().find(|p| p.id == plane_id).is_some_and(|p| p.legacy_crtc.is_some_and(|c| c != crtc_id)) {
        io.warn(device.id, "cursor plane is attached to a different crtc");
    }
    let mut fb_id = None;
    if request.flags & DRM_MODE_CURSOR_BO != 0 {
        if request.handle != 0 {
            let created = match io.cursor_framebuffer_create(device, file, request.width, request.height, request.handle) {
                Ok(id) => id,
                Err(ret) => { io.debug(device.id, "failed to wrap cursor buffer in framebuffer"); return ret; }
            };
            if !device.framebuffers.iter().any(|fb| fb.id == created) {
                device.framebuffers.push(Framebuffer { id: created, format: DRM_FORMAT_ARGB8888,
                    modifier: DRM_FORMAT_MOD_LINEAR, width: request.width, height: request.height, refs: 1 });
            }
            fb_id = Some(created);
            if let Some(pidx) = plane_index(device, plane_id) {
                if device.planes[pidx].hotspot_x_property.is_some() {
                    if let Some(state) = device.planes[pidx].state.as_mut() { state.hotspot_x = request.hot_x; }
                }
                if device.planes[pidx].hotspot_y_property.is_some() {
                    if let Some(state) = device.planes[pidx].state.as_mut() { state.hotspot_y = request.hot_y; }
                }
            }
        }
    } else if let Some(pidx) = plane_index(device, plane_id) {
        fb_id = if let Some(state) = device.planes[pidx].state.as_ref() { state.fb }
                else { device.planes[pidx].legacy_fb };
        if let Some(fb) = fb_id { io.framebuffer_get(device, fb); }
    }
    let (crtc_x, crtc_y) = if request.flags & DRM_MODE_CURSOR_MOVE != 0 {
        (request.x, request.y)
    } else { (device.crtcs[cidx].cursor_x, device.crtcs[cidx].cursor_y) };
    let (crtc_w, crtc_h, src_w, src_h) = if let Some(fb_id) = fb_id {
        let fb = device.framebuffers.iter().find(|fb| fb.id == fb_id);
        if let Some(fb) = fb {
            (fb.width, fb.height, fb.width.wrapping_shl(16), fb.height.wrapping_shl(16))
        } else { (0, 0, 0, 0) }
    } else { (0, 0, 0, 0) };
    let update = PlaneUpdate { crtc: crtc_id, fb: fb_id.unwrap_or(0), crtc_x, crtc_y,
        crtc_w, crtc_h, src_x: 0, src_y: 0, src_w, src_h };
    let ret = if device.atomic_modeset {
        __setplane_atomic(io, device, plane_id, Some(crtc_id), fb_id, update, ctx)
    } else {
        __setplane_internal(io, device, plane_id, Some(crtc_id), fb_id, update, ctx)
    };
    if let Some(fb) = fb_id { io.framebuffer_put(device, fb); }
    if ret == 0 && request.flags & DRM_MODE_CURSOR_MOVE != 0 {
        if let Some(cidx) = device.crtcs.iter().position(|crtc| crtc.id == crtc_id) {
            device.crtcs[cidx].cursor_x = request.x;
            device.crtcs[cidx].cursor_y = request.y;
        }
    }
    ret
}

// upstream: drm_plane.c drm_mode_cursor_common()
pub fn drm_mode_cursor_common<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                              request: &ModeCursor2, file: &DrmFile) -> i32 {
    if !device.modeset { return EOPNOTSUPP; }
    if request.flags == 0 || request.flags & !DRM_MODE_CURSOR_FLAGS != 0 { return EINVAL; }
    let Some(cidx) = io.crtc_find(device, file, request.crtc_id) else {
        io.debug(device.id, "unknown crtc id");
        return ENOENT;
    };
    let crtc_id = device.crtcs[cidx].id;
    let mut ctx = ModesetContext::default();
    io.acquire_init(&mut ctx);
    loop {
        let mut ret = io.crtc_lock(crtc_id, &mut ctx);
        if ret == 0 {
            let cursor_plane = device.crtcs[cidx].cursor_plane;
            if let Some(plane_id) = cursor_plane {
                ret = io.plane_lock(plane_id, Some(&mut ctx));
                if ret == 0 {
                    if !io.lease_held(file.id, plane_id) { ret = EACCES; }
                    else { ret = drm_mode_cursor_universal(io, device, crtc_id, request, file, &mut ctx); }
                }
            } else {
                if request.flags & DRM_MODE_CURSOR_BO != 0 {
                    let crtc = device.crtcs[cidx].clone();
                    if !crtc.funcs.cursor_set && !crtc.funcs.cursor_set2 { ret = ENXIO; }
                    else {
                        ret = io.cursor_set(crtc_id, file.id, request.handle, request.width,
                            request.height, request.hot_x, request.hot_y, crtc.funcs.cursor_set2);
                    }
                }
                if request.flags & DRM_MODE_CURSOR_MOVE != 0 {
                    let crtc = &device.crtcs[cidx];
                    if crtc.funcs.cursor_move { ret = io.cursor_move(crtc_id, request.x, request.y); }
                    else { ret = EFAULT; }
                }
            }
        }
        if ret != EDEADLK {
            io.drop_locks(&mut ctx);
            io.acquire_fini(&mut ctx);
            return ret;
        }
        ret = io.modeset_backoff(&mut ctx);
        if ret != 0 {
            io.drop_locks(&mut ctx);
            io.acquire_fini(&mut ctx);
            return ret;
        }
    }
}

// upstream: drm_plane.c drm_mode_cursor_ioctl()
pub fn drm_mode_cursor_ioctl<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                             request: &ModeCursor, file: &DrmFile) -> i32 {
    let converted = ModeCursor2 { crtc_id: request.crtc_id, flags: request.flags,
        handle: request.handle, width: request.width, height: request.height,
        x: request.x, y: request.y, hot_x: 0, hot_y: 0 };
    drm_mode_cursor_common(io, device, &converted, file)
}

// upstream: drm_plane.c drm_mode_cursor2_ioctl()
pub fn drm_mode_cursor2_ioctl<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                              request: &ModeCursor2, file: &DrmFile) -> i32 {
    drm_mode_cursor_common(io, device, request, file)
}

// upstream: drm_plane.c drm_mode_page_flip_ioctl()
pub fn drm_mode_page_flip_ioctl<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                                request: PageFlipRequest, file: &DrmFile) -> i32 {
    if !device.modeset { return EOPNOTSUPP; }
    if request.flags & !DRM_MODE_PAGE_FLIP_FLAGS != 0 { return EINVAL; }
    if request.sequence != 0 && request.flags & DRM_MODE_PAGE_FLIP_TARGET == 0 { return EINVAL; }
    if request.flags & DRM_MODE_PAGE_FLIP_TARGET == DRM_MODE_PAGE_FLIP_TARGET { return EINVAL; }
    if request.flags & DRM_MODE_PAGE_FLIP_ASYNC != 0 && !device.mode_config.async_page_flip { return EINVAL; }
    let Some(cidx) = io.crtc_find(device, file, request.crtc_id) else { return ENOENT };
    let crtc_id = device.crtcs[cidx].id;
    let Some(plane_id) = device.crtcs[cidx].primary_plane else { return EINVAL };
    if !io.lease_held(file.id, plane_id) { return EACCES; }
    let target_fn = device.crtcs[cidx].funcs.page_flip_target;
    let normal_fn = device.crtcs[cidx].funcs.page_flip;
    let mut target_vblank = request.sequence;
    if target_fn {
        let ret = io.vblank_get(crtc_id);
        if ret != 0 { return ret; }
        let current = io.vblank_count(crtc_id);
        match request.flags & DRM_MODE_PAGE_FLIP_TARGET {
            DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE => {
                if (target_vblank.wrapping_sub(current) as i32) > 1 {
                    io.debug(device.id, "invalid absolute flip target");
                    io.vblank_put(crtc_id);
                    return EINVAL;
                }
            }
            DRM_MODE_PAGE_FLIP_TARGET_RELATIVE => {
                if target_vblank > 1 { io.debug(device.id, "invalid relative flip target"); io.vblank_put(crtc_id); return EINVAL; }
                target_vblank = target_vblank.wrapping_add(current);
            }
            _ => target_vblank = current.wrapping_add(if request.flags & DRM_MODE_PAGE_FLIP_ASYNC == 0 { 1 } else { 0 }),
        }
    } else if !normal_fn || request.flags & DRM_MODE_PAGE_FLIP_TARGET != 0 { return EINVAL; }

    let mut ctx = ModesetContext::default();
    io.acquire_init(&mut ctx);
    let mut ret;
    let mut fb_id: Option<DrmId> = None;
    let mut reserved_event: Option<FlipEvent> = None;
    loop {
        ret = io.crtc_lock(crtc_id, &mut ctx);
        if ret == 0 { ret = io.plane_lock(plane_id, Some(&mut ctx)); }
        if ret == 0 {
            let pidx = plane_index(device, plane_id).unwrap_or(usize::MAX);
            let old_fb = if pidx == usize::MAX { None } else if let Some(state) = device.planes[pidx].state.as_ref() {
                state.fb
            } else { device.planes[pidx].legacy_fb };
            if old_fb.is_none() { ret = EBUSY; }
            if ret == 0 {
                fb_id = io.framebuffer_lookup(device, file, request.fb_id);
                if fb_id.is_none() { ret = ENOENT; }
            }
            if ret == 0 {
                let fb = device.framebuffers.iter().find(|fb| Some(fb.id) == fb_id).cloned();
                if let Some(fb) = fb {
                    if pidx != usize::MAX {
                        if let Some(state) = device.planes[pidx].state.as_ref() {
                            ret = io.check_src_coords(state.src_x, state.src_y, state.src_w, state.src_h, &fb);
                        } else {
                            ret = io.crtc_check_viewport(&device.crtcs[cidx], &fb);
                        }
                    } else { ret = EINVAL; }
                    if ret == 0 {
                        let old_format = old_fb.and_then(|id| device.framebuffers.iter().find(|f| f.id == id)).map(|f| f.format);
                        if old_format != Some(fb.format) {
                            io.debug(device.id, "page flip may not change framebuffer format");
                            ret = EINVAL;
                        }
                    }
                } else { ret = ENOENT; }
            }
            if ret == 0 && request.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
                let event = FlipEvent { event_type: DRM_EVENT_FLIP_COMPLETE, length: 32,
                    user_data: request.user_data, crtc_id };
                ret = io.reserve_event(file.id, &event);
                if ret == 0 { reserved_event = Some(event); }
            }
            if ret == 0 {
                if pidx != usize::MAX { device.planes[pidx].old_fb = device.planes[pidx].legacy_fb; }
                ret = io.page_flip(crtc_id, fb_id.unwrap(), reserved_event.as_ref(), request.flags,
                    target_vblank, &mut ctx, target_fn);
                if ret != 0 {
                    if let Some(event) = reserved_event.as_ref() { io.cancel_event(device.id, event); }
                    if pidx != usize::MAX { device.planes[pidx].old_fb = None; }
                } else if pidx != usize::MAX && device.planes[pidx].state.is_none() {
                    device.planes[pidx].legacy_fb = fb_id;
                    io.framebuffer_get(device, fb_id.unwrap());
                }
            }
            if let Some(fb) = fb_id { io.framebuffer_put(device, fb); fb_id = None; }
            if let Some(pidx) = plane_index(device, plane_id) {
                if let Some(old) = device.planes[pidx].old_fb { io.framebuffer_put(device, old); }
                device.planes[pidx].old_fb = None;
            }
        }
        if ret != EDEADLK { break; }
        ret = io.modeset_backoff(&mut ctx);
        if ret != 0 { break; }
    }
    if let Some(fb) = fb_id { io.framebuffer_put(device, fb); }
    io.drop_locks(&mut ctx);
    io.acquire_fini(&mut ctx);
    if ret != 0 && target_fn { io.vblank_put(crtc_id); }
    ret
}

// upstream: drm_plane.c drm_plane_enable_fb_damage_clips()
pub fn drm_plane_enable_fb_damage_clips<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                                        plane_id: DrmId) -> i32 {
    let Some(index) = plane_index(device, plane_id) else { return ENOENT };
    let property = device.mode_config.properties.damage_clips;
    io.attach_property(plane_id, property, 0);
    property_set(&mut device.planes[index], property, 0);
    0
}

// upstream: drm_plane.c drm_plane_get_damage_clips_count()
pub fn drm_plane_get_damage_clips_count(device: &DrmDevice, state: Option<&PlaneState>) -> usize {
    let Some(blob_id) = state.and_then(|state| state.fb_damage_clips) else { return 0 };
    device.blobs.iter().find(|blob| blob.id == blob_id)
        .map(|blob| if blob.bytes.is_empty() { blob.damage_clips.len() } else { blob.bytes.len() / 16 })
        .unwrap_or(0)
}

// upstream: drm_plane.c __drm_plane_get_damage_clips()
pub fn __drm_plane_get_damage_clips<'a>(device: &'a DrmDevice,
                                          state: Option<&PlaneState>) -> Option<&'a [ModeRect]> {
    let blob_id = state?.fb_damage_clips?;
    device.blobs.iter().find(|blob| blob.id == blob_id).map(|blob| blob.damage_clips.as_slice())
}

// upstream: drm_plane.c drm_plane_get_damage_clips()
pub fn drm_plane_get_damage_clips<'a, I: DrmPlaneIo>(io: &mut I, device: &'a DrmDevice,
                                                       state: &'a PlaneState) -> Option<&'a [ModeRect]> {
    let plane_id = state.plane;
    let enabled = plane_index(device, plane_id).is_some_and(|index| {
        device.planes[index].properties.iter().any(|p| p.property == device.mode_config.properties.damage_clips)
    });
    if !enabled { io.warn(device.id, "drm_plane_enable_fb_damage_clips() was not called"); }
    __drm_plane_get_damage_clips(device, Some(state))
}

// upstream: drm_plane.c drm_create_scaling_filter_prop()
pub fn drm_create_scaling_filter_prop<I: DrmPlaneIo>(io: &mut I, device: DrmId,
                                                      supported_filters: u32) -> PlaneResult<DrmId> {
    const VALID_MASK: u32 = (1 << DRM_SCALING_FILTER_DEFAULT) | (1 << DRM_SCALING_FILTER_NEAREST_NEIGHBOR);
    if supported_filters & !VALID_MASK != 0 || supported_filters & (1 << DRM_SCALING_FILTER_DEFAULT) == 0 {
        io.warn(device, "invalid supported scaling filter mask");
        return Err(EINVAL);
    }
    let enum_count = supported_filters.count_ones() as usize;
    let property = io.property_create(device, "SCALING_FILTER", DRM_MODE_PROP_ENUM, enum_count)?;
    for (value, name) in [(DRM_SCALING_FILTER_DEFAULT, "Default"),
                          (DRM_SCALING_FILTER_NEAREST_NEIGHBOR, "Nearest Neighbor")] {
        if supported_filters & (1 << value) == 0 { continue; }
        let ret = io.property_add_enum(property, value, name);
        if ret != 0 { io.property_destroy(device, property); return Err(ret); }
    }
    Ok(property)
}

// upstream: drm_plane.c drm_plane_create_scaling_filter_property()
pub fn drm_plane_create_scaling_filter_property<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                                                plane_id: DrmId,
                                                                supported_filters: u32) -> i32 {
    let Some(index) = plane_index(device, plane_id) else { return ENOENT };
    let property = match drm_create_scaling_filter_prop(io, device.id, supported_filters) {
        Ok(id) => id, Err(ret) => return ret,
    };
    io.attach_property(plane_id, property, DRM_SCALING_FILTER_DEFAULT as u64);
    property_set(&mut device.planes[index], property, DRM_SCALING_FILTER_DEFAULT as u64);
    device.planes[index].scaling_filter_property = Some(property);
    0
}

// upstream: drm_plane.c drm_plane_add_size_hints_property()
pub fn drm_plane_add_size_hints_property<I: DrmPlaneIo>(io: &mut I, device: &mut DrmDevice,
                                                         plane_id: DrmId,
                                                         hints: &[PlaneSizeHint]) -> i32 {
    let Some(index) = plane_index(device, plane_id) else { return ENOENT };
    if device.planes[index].kind != DRM_PLANE_TYPE_CURSOR {
        io.warn(device.id, "size hints are currently only supported on cursor planes");
        return EINVAL;
    }
    let Some(byte_len) = hints.len().checked_mul(8) else { return ENOMEM };
    let mut blob = PropertyBlob { id: alloc_id(device), ..PropertyBlob::default() };
    if blob.bytes.try_reserve_exact(byte_len).is_err() { return ENOMEM; }
    for hint in hints {
        blob.bytes.extend_from_slice(&hint.width.to_ne_bytes());
        blob.bytes.extend_from_slice(&hint.height.to_ne_bytes());
    }
    let id = match io.create_blob(device.id, &blob) { Ok(id) => id, Err(ret) => return ret };
    if id != 0 { blob.id = id; }
    let blob_id = blob.id;
    device.blobs.push(blob);
    let property = device.mode_config.properties.size_hints;
    io.attach_property(plane_id, property, blob_id as u64);
    property_set(&mut device.planes[index], property, blob_id as u64);
    0
}

// upstream: drm_plane.c drm_plane_create_color_pipeline_property()
pub fn drm_plane_create_color_pipeline_property<I: DrmPlaneIo>(io: &mut I,
                                                                device: &mut DrmDevice,
                                                                plane_id: DrmId,
                                                                pipelines: &[PropEnum]) -> i32 {
    let Some(index) = plane_index(device, plane_id) else { return ENOENT };
    let Some(capacity) = pipelines.len().checked_add(1) else { return ENOMEM };
    let mut all_pipelines = Vec::new();
    if all_pipelines.try_reserve_exact(capacity).is_err() {
        io.error(device.id, "failed to allocate color pipeline enum list");
        return ENOMEM;
    }
    all_pipelines.push(PropEnum { value: 0, name: "Bypass" });
    all_pipelines.extend_from_slice(pipelines);
    let property = match io.property_create(device.id, "COLOR_PIPELINE", DRM_MODE_PROP_ATOMIC,
                                             all_pipelines.len()) {
        Ok(id) => id,
        Err(_) => return ENOMEM,
    };
    for item in &all_pipelines {
        let ret = io.property_add_enum(property, item.value, item.name);
        if ret != 0 { io.property_destroy(device.id, property); return ret; }
    }
    io.attach_property(plane_id, property, 0);
    property_set(&mut device.planes[index], property, 0);
    device.planes[index].color_pipeline_property = Some(property);
    0
}
