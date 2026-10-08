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
 * written prior permission.  The copyright holders make no representations
 * about the suitability of this software for any purpose.  It is provided "as
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

//! Function-level Rust translation of Linux v7.2.3 `drm_mode_config.c`.
//! DRM object registration/destruction, lock primitives, deferred connector
//! freeing, and userspace memory access are explicit integration hooks.

#![no_std]
extern crate alloc;

use alloc::{format, string::String, vec::Vec};

pub type KResult<T> = Result<T, i32>;
pub type ObjectId = u32;
pub type UserPtr = u64;

pub const EDEADLK: i32 = -35;
pub const EFAULT: i32 = -14;
pub const ENOMEM: i32 = -12;
pub const EOPNOTSUPP: i32 = -95;
pub const DRM_MODE_PROP_IMMUTABLE: u32 = 1 << 2;
pub const DRM_MODE_PROP_BLOB: u32 = 1 << 4;
pub const DRM_MODE_PROP_ATOMIC: u32 = 1 << 31;
pub const DRM_MODE_OBJECT_CRTC: u32 = 0xcccc_cccc;
pub const DRM_MODE_OBJECT_FB: u32 = 0xfbfb_fbfb;
pub const DRM_MODE_CONNECTOR_WRITEBACK: u32 = 18;
pub const DRM_PLANE_TYPE_OVERLAY: u32 = 0;
pub const DRM_PLANE_TYPE_PRIMARY: u32 = 1;
pub const DRM_PLANE_TYPE_CURSOR: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PropertyEnum {
    pub value: u64,
    pub name: &'static str,
}

const PLANE_TYPE_ENUMS: [PropertyEnum; 3] = [
    PropertyEnum { value: DRM_PLANE_TYPE_OVERLAY as u64, name: "Overlay" },
    PropertyEnum { value: DRM_PLANE_TYPE_PRIMARY as u64, name: "Primary" },
    PropertyEnum { value: DRM_PLANE_TYPE_CURSOR as u64, name: "Cursor" },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyKind {
    Enum(&'static [PropertyEnum]),
    Range { min: u64, max: u64 },
    SignedRange { min: i64, max: i64 },
    Object { object_type: u32 },
    Boolean,
    Plain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PropertySpec {
    pub flags: u32,
    pub name: &'static str,
    pub kind: PropertyKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertySlot {
    PlaneType,
    SrcX,
    SrcY,
    SrcW,
    SrcH,
    CrtcX,
    CrtcY,
    CrtcW,
    CrtcH,
    FbId,
    InFenceFd,
    OutFencePtr,
    CrtcId,
    FbDamageClips,
    Active,
    ModeId,
    VrrEnabled,
    DegammaLut,
    DegammaLutSize,
    Ctm,
    GammaLut,
    GammaLutSize,
    BackgroundColor,
    Modifiers,
    AsyncModifiers,
    SizeHints,
}

#[derive(Clone, Debug, Default)]
pub struct Properties {
    pub plane_type_property: Option<ObjectId>,
    pub prop_src_x: Option<ObjectId>,
    pub prop_src_y: Option<ObjectId>,
    pub prop_src_w: Option<ObjectId>,
    pub prop_src_h: Option<ObjectId>,
    pub prop_crtc_x: Option<ObjectId>,
    pub prop_crtc_y: Option<ObjectId>,
    pub prop_crtc_w: Option<ObjectId>,
    pub prop_crtc_h: Option<ObjectId>,
    pub prop_fb_id: Option<ObjectId>,
    pub prop_in_fence_fd: Option<ObjectId>,
    pub prop_out_fence_ptr: Option<ObjectId>,
    pub prop_crtc_id: Option<ObjectId>,
    pub prop_fb_damage_clips: Option<ObjectId>,
    pub prop_active: Option<ObjectId>,
    pub prop_mode_id: Option<ObjectId>,
    pub prop_vrr_enabled: Option<ObjectId>,
    pub degamma_lut_property: Option<ObjectId>,
    pub degamma_lut_size_property: Option<ObjectId>,
    pub ctm_property: Option<ObjectId>,
    pub gamma_lut_property: Option<ObjectId>,
    pub gamma_lut_size_property: Option<ObjectId>,
    pub background_color_property: Option<ObjectId>,
    pub modifiers_property: Option<ObjectId>,
    pub async_modifiers_property: Option<ObjectId>,
    pub size_hints_property: Option<ObjectId>,
}

impl Properties {
    fn set(&mut self, slot: PropertySlot, id: ObjectId) {
        match slot {
            PropertySlot::PlaneType => self.plane_type_property = Some(id),
            PropertySlot::SrcX => self.prop_src_x = Some(id),
            PropertySlot::SrcY => self.prop_src_y = Some(id),
            PropertySlot::SrcW => self.prop_src_w = Some(id),
            PropertySlot::SrcH => self.prop_src_h = Some(id),
            PropertySlot::CrtcX => self.prop_crtc_x = Some(id),
            PropertySlot::CrtcY => self.prop_crtc_y = Some(id),
            PropertySlot::CrtcW => self.prop_crtc_w = Some(id),
            PropertySlot::CrtcH => self.prop_crtc_h = Some(id),
            PropertySlot::FbId => self.prop_fb_id = Some(id),
            PropertySlot::InFenceFd => self.prop_in_fence_fd = Some(id),
            PropertySlot::OutFencePtr => self.prop_out_fence_ptr = Some(id),
            PropertySlot::CrtcId => self.prop_crtc_id = Some(id),
            PropertySlot::FbDamageClips => self.prop_fb_damage_clips = Some(id),
            PropertySlot::Active => self.prop_active = Some(id),
            PropertySlot::ModeId => self.prop_mode_id = Some(id),
            PropertySlot::VrrEnabled => self.prop_vrr_enabled = Some(id),
            PropertySlot::DegammaLut => self.degamma_lut_property = Some(id),
            PropertySlot::DegammaLutSize => self.degamma_lut_size_property = Some(id),
            PropertySlot::Ctm => self.ctm_property = Some(id),
            PropertySlot::GammaLut => self.gamma_lut_property = Some(id),
            PropertySlot::GammaLutSize => self.gamma_lut_size_property = Some(id),
            PropertySlot::BackgroundColor => self.background_color_property = Some(id),
            PropertySlot::Modifiers => self.modifiers_property = Some(id),
            PropertySlot::AsyncModifiers => self.async_modifiers_property = Some(id),
            PropertySlot::SizeHints => self.size_hints_property = Some(id),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    Plane,
    Crtc,
    Encoder,
    Connector,
    Colorop,
    Framebuffer,
    Property,
    PropertyBlob,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Component {
    Plane,
    Crtc,
    Encoder,
    Connector,
}

#[derive(Clone, Debug, Default)]
pub struct Framebuffer {
    pub id: ObjectId,
}

#[derive(Clone, Debug, Default)]
pub struct Property {
    pub id: ObjectId,
}

#[derive(Clone, Debug, Default)]
pub struct PropertyBlob {
    pub id: ObjectId,
}

#[derive(Clone, Debug, Default)]
pub struct Colorop {
    pub id: ObjectId,
}

#[derive(Clone, Debug, Default)]
pub struct Plane {
    pub id: ObjectId,
    pub name: String,
    pub mask: u32,
    pub possible_crtcs: u32,
    pub plane_type: u32,
    pub has_reset: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Crtc {
    pub id: ObjectId,
    pub name: String,
    pub mask: u32,
    pub primary: Option<Plane>,
    pub cursor: Option<Plane>,
    pub has_reset: bool,
    pub has_cursor_set: bool,
    pub has_cursor_set2: bool,
    pub has_cursor_move: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Encoder {
    pub id: ObjectId,
    pub name: String,
    pub mask: u32,
    pub possible_clones: u32,
    pub possible_crtcs: u32,
    pub has_funcs: bool,
    pub has_reset: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Connector {
    pub id: ObjectId,
    pub name: String,
    pub connector_type: u32,
    pub has_reset: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ModeConfig {
    pub max_height: u32,
    pub min_height: u32,
    pub max_width: u32,
    pub min_width: u32,
    pub num_fb: u32,
    pub num_connector: u32,
    pub num_crtc: u32,
    pub num_encoder: u32,
    pub num_total_plane: u32,
    pub num_colorop: u32,
    pub properties: Properties,
    pub framebuffers: Vec<Framebuffer>,
    pub crtcs: Vec<Crtc>,
    pub connectors: Vec<Connector>,
    pub encoders: Vec<Encoder>,
    pub property_list: Vec<Property>,
    pub property_blob_list: Vec<PropertyBlob>,
    pub planes: Vec<Plane>,
    pub colorops: Vec<Colorop>,
    pub private_objects: Vec<ObjectId>,
    pub mutex_initialized: bool,
    pub connection_mutex_initialized: bool,
    pub idr_mutex_initialized: bool,
    pub fb_lock_initialized: bool,
    pub blob_lock_initialized: bool,
    pub connector_list_lock_initialized: bool,
    pub connector_free_list_initialized: bool,
    pub object_idr_base: Option<u32>,
    pub tile_idr_base: Option<u32>,
    pub connector_ida_initialized: bool,
    pub connector_free_work_initialized: bool,
}

#[derive(Clone, Debug, Default)]
pub struct DrmDevice {
    pub id: ObjectId,
    pub has_modeset: bool,
    pub mode_config: ModeConfig,
}

#[derive(Clone, Debug, Default)]
pub struct DrmFile {
    pub id: ObjectId,
    pub framebuffers: Vec<ObjectId>,
    pub writeback_connectors: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceRequest {
    pub fb_id_ptr: UserPtr,
    pub count_fbs: usize,
    pub crtc_id_ptr: UserPtr,
    pub count_crtcs: usize,
    pub connector_id_ptr: UserPtr,
    pub count_connectors: usize,
    pub encoder_id_ptr: UserPtr,
    pub count_encoders: usize,
    pub max_height: u32,
    pub min_height: u32,
    pub max_width: u32,
    pub min_width: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockKind {
    ModeConfigMutex,
    ConnectionMutex,
    IdrMutex,
    FbLock,
    BlobLock,
    ConnectorListSpinlock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListKind {
    Framebuffers,
    Crtcs,
    Connectors,
    Encoders,
    Properties,
    PropertyBlobs,
    Planes,
    Colorops,
    PrivateObjects,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdStoreKind {
    ObjectIdr,
    TileIdr,
    ConnectorIda,
}

/// Kernel services that are deliberately outside this policy translation.
/// Implementations provide the object lifetime, synchronization, ioctl-copy,
/// and diagnostic effects at the integration boundary.
pub trait ModeConfigOps {
    fn register_all(&mut self, device: ObjectId, component: Component) -> KResult<()>;
    fn unregister_all(&mut self, device: ObjectId, component: Component);
    fn lock_file_framebuffers(&mut self, file: ObjectId);
    fn unlock_file_framebuffers(&mut self, file: ObjectId);
    fn put_user(&mut self, pointer: UserPtr, index: usize, value: ObjectId) -> bool;
    fn lease_held(&mut self, file: ObjectId, object: ObjectId) -> bool;
    fn connector_list_iter_begin(&mut self, device: ObjectId);
    fn connector_list_iter_end(&mut self, device: ObjectId);
    fn reset_object(&mut self, kind: ObjectKind, object: ObjectId);
    fn connector_create_standard_properties(&mut self, device: ObjectId) -> KResult<()>;
    fn create_property(&mut self, device: ObjectId, spec: PropertySpec) -> Option<ObjectId>;
    fn init_lock(&mut self, device: ObjectId, lock: LockKind);
    fn init_idr(&mut self, device: ObjectId, store: IdStoreKind, base: u32);
    fn init_connector_ida(&mut self, device: ObjectId);
    fn init_connector_free_list(&mut self, device: ObjectId);
    fn init_work(&mut self, device: ObjectId);
    fn lockdep_enabled(&mut self) -> bool;
    fn dma_resv_init(&mut self);
    fn modeset_acquire_init(&mut self);
    fn modeset_lock(&mut self, device: ObjectId) -> i32;
    fn modeset_backoff(&mut self, device: ObjectId) -> i32;
    fn might_fault(&mut self);
    fn ww_acquire_init(&mut self);
    fn dma_resv_lock(&mut self) -> i32;
    fn dma_resv_lock_slow(&mut self);
    fn dma_resv_unlock(&mut self);
    fn ww_acquire_fini(&mut self);
    fn modeset_drop_locks(&mut self);
    fn modeset_acquire_fini(&mut self);
    fn dma_resv_fini(&mut self);
    fn add_mode_config_cleanup_action(&mut self, device: ObjectId) -> KResult<()>;
    fn connector_put(&mut self, device: &mut DrmDevice, connector: ObjectId);
    fn flush_connector_free_work(&mut self, device: &mut DrmDevice);
    fn warn(&mut self, message: String);
    fn destroy_object(&mut self, device: ObjectId, kind: ObjectKind, object: ObjectId);
    fn property_blob_put(&mut self, device: ObjectId, blob: ObjectId);
    fn framebuffer_print_info(&mut self, device: ObjectId, framebuffer: ObjectId);
    fn framebuffer_free(&mut self, device: ObjectId, framebuffer: ObjectId);
    fn ida_destroy(&mut self, device: ObjectId);
    fn idr_destroy(&mut self, device: ObjectId, store: IdStoreKind);
    fn modeset_lock_fini(&mut self, device: ObjectId);
    fn encoder_mask(&mut self, encoder: &Encoder) -> u32;
    fn crtc_mask(&mut self, crtc: &Crtc) -> u32;
    fn plane_mask(&mut self, plane: &Plane) -> u32;
}

// upstream: drm_mode_config.c drm_modeset_register_all()
pub fn drm_modeset_register_all<O: ModeConfigOps>(device: &mut DrmDevice, ops: &mut O) -> KResult<()> {
    if let Err(ret) = ops.register_all(device.id, Component::Plane) {
        return Err(ret);
    }
    if let Err(ret) = ops.register_all(device.id, Component::Crtc) {
        ops.unregister_all(device.id, Component::Plane);
        return Err(ret);
    }
    if let Err(ret) = ops.register_all(device.id, Component::Encoder) {
        ops.unregister_all(device.id, Component::Crtc);
        ops.unregister_all(device.id, Component::Plane);
        return Err(ret);
    }
    if let Err(ret) = ops.register_all(device.id, Component::Connector) {
        ops.unregister_all(device.id, Component::Encoder);
        ops.unregister_all(device.id, Component::Crtc);
        ops.unregister_all(device.id, Component::Plane);
        return Err(ret);
    }
    Ok(())
}

// upstream: drm_mode_config.c drm_modeset_unregister_all()
pub fn drm_modeset_unregister_all<O: ModeConfigOps>(device: &DrmDevice, ops: &mut O) {
    ops.unregister_all(device.id, Component::Connector);
    ops.unregister_all(device.id, Component::Encoder);
    ops.unregister_all(device.id, Component::Crtc);
    ops.unregister_all(device.id, Component::Plane);
}

// upstream: drm_mode_config.c drm_mode_getresources()
pub fn drm_mode_getresources<O: ModeConfigOps>(
    device: &DrmDevice,
    request: &mut ResourceRequest,
    file: &DrmFile,
    ops: &mut O,
) -> KResult<()> {
    if !device.has_modeset {
        return Err(EOPNOTSUPP);
    }

    ops.lock_file_framebuffers(file.id);
    let mut count = 0usize;
    for framebuffer in &file.framebuffers {
        if count < request.count_fbs && !ops.put_user(request.fb_id_ptr, count, *framebuffer) {
            ops.unlock_file_framebuffers(file.id);
            return Err(EFAULT);
        }
        count += 1;
    }
    request.count_fbs = count;
    ops.unlock_file_framebuffers(file.id);

    request.max_height = device.mode_config.max_height;
    request.min_height = device.mode_config.min_height;
    request.max_width = device.mode_config.max_width;
    request.min_width = device.mode_config.min_width;

    count = 0;
    for crtc in &device.mode_config.crtcs {
        if ops.lease_held(file.id, crtc.id) {
            if count < request.count_crtcs && !ops.put_user(request.crtc_id_ptr, count, crtc.id) {
                return Err(EFAULT);
            }
            count += 1;
        }
    }
    request.count_crtcs = count;

    count = 0;
    for encoder in &device.mode_config.encoders {
        if count < request.count_encoders
            && !ops.put_user(request.encoder_id_ptr, count, encoder.id)
        {
            return Err(EFAULT);
        }
        count += 1;
    }
    request.count_encoders = count;

    ops.connector_list_iter_begin(device.id);
    count = 0;
    for connector in &device.mode_config.connectors {
        if !file.writeback_connectors && connector.connector_type == DRM_MODE_CONNECTOR_WRITEBACK {
            continue;
        }
        if ops.lease_held(file.id, connector.id) {
            if count < request.count_connectors
                && !ops.put_user(request.connector_id_ptr, count, connector.id)
            {
                ops.connector_list_iter_end(device.id);
                return Err(EFAULT);
            }
            count += 1;
        }
    }
    request.count_connectors = count;
    ops.connector_list_iter_end(device.id);
    Ok(())
}

// upstream: drm_mode_config.c drm_mode_config_reset()
pub fn drm_mode_config_reset<O: ModeConfigOps>(device: &DrmDevice, ops: &mut O) {
    for colorop in &device.mode_config.colorops {
        ops.reset_object(ObjectKind::Colorop, colorop.id);
    }
    for plane in &device.mode_config.planes {
        if plane.has_reset {
            ops.reset_object(ObjectKind::Plane, plane.id);
        }
    }
    for crtc in &device.mode_config.crtcs {
        if crtc.has_reset {
            ops.reset_object(ObjectKind::Crtc, crtc.id);
        }
    }
    for encoder in &device.mode_config.encoders {
        if encoder.has_funcs && encoder.has_reset {
            ops.reset_object(ObjectKind::Encoder, encoder.id);
        }
    }
    ops.connector_list_iter_begin(device.id);
    for connector in &device.mode_config.connectors {
        if connector.has_reset {
            ops.reset_object(ObjectKind::Connector, connector.id);
        }
    }
    ops.connector_list_iter_end(device.id);
}

// upstream: drm_mode_config.c drm_mode_create_standard_properties()
fn drm_mode_create_standard_properties<O: ModeConfigOps>(
    device: &mut DrmDevice,
    ops: &mut O,
) -> KResult<()> {
    ops.connector_create_standard_properties(device.id)?;

    let standard: [(PropertySlot, PropertySpec); 26] = [
        (PropertySlot::PlaneType, PropertySpec { flags: DRM_MODE_PROP_IMMUTABLE, name: "type", kind: PropertyKind::Enum(&PLANE_TYPE_ENUMS) }),
        (PropertySlot::SrcX, PropertySpec { flags: DRM_MODE_PROP_ATOMIC, name: "SRC_X", kind: PropertyKind::Range { min: 0, max: u32::MAX as u64 } }),
        (PropertySlot::SrcY, PropertySpec { flags: DRM_MODE_PROP_ATOMIC, name: "SRC_Y", kind: PropertyKind::Range { min: 0, max: u32::MAX as u64 } }),
        (PropertySlot::SrcW, PropertySpec { flags: DRM_MODE_PROP_ATOMIC, name: "SRC_W", kind: PropertyKind::Range { min: 0, max: u32::MAX as u64 } }),
        (PropertySlot::SrcH, PropertySpec { flags: DRM_MODE_PROP_ATOMIC, name: "SRC_H", kind: PropertyKind::Range { min: 0, max: u32::MAX as u64 } }),
        (PropertySlot::CrtcX, PropertySpec { flags: DRM_MODE_PROP_ATOMIC, name: "CRTC_X", kind: PropertyKind::SignedRange { min: i32::MIN as i64, max: i32::MAX as i64 } }),
        (PropertySlot::CrtcY, PropertySpec { flags: DRM_MODE_PROP_ATOMIC, name: "CRTC_Y", kind: PropertyKind::SignedRange { min: i32::MIN as i64, max: i32::MAX as i64 } }),
        (PropertySlot::CrtcW, PropertySpec { flags: DRM_MODE_PROP_ATOMIC, name: "CRTC_W", kind: PropertyKind::Range { min: 0, max: i32::MAX as u64 } }),
        (PropertySlot::CrtcH, PropertySpec { flags: DRM_MODE_PROP_ATOMIC, name: "CRTC_H", kind: PropertyKind::Range { min: 0, max: i32::MAX as u64 } }),
        (PropertySlot::FbId, PropertySpec { flags: DRM_MODE_PROP_ATOMIC, name: "FB_ID", kind: PropertyKind::Object { object_type: DRM_MODE_OBJECT_FB } }),
        (PropertySlot::InFenceFd, PropertySpec { flags: DRM_MODE_PROP_ATOMIC, name: "IN_FENCE_FD", kind: PropertyKind::SignedRange { min: -1, max: i32::MAX as i64 } }),
        (PropertySlot::OutFencePtr, PropertySpec { flags: DRM_MODE_PROP_ATOMIC, name: "OUT_FENCE_PTR", kind: PropertyKind::Range { min: 0, max: u64::MAX } }),
        (PropertySlot::CrtcId, PropertySpec { flags: DRM_MODE_PROP_ATOMIC, name: "CRTC_ID", kind: PropertyKind::Object { object_type: DRM_MODE_OBJECT_CRTC } }),
        (PropertySlot::FbDamageClips, PropertySpec { flags: DRM_MODE_PROP_ATOMIC | DRM_MODE_PROP_BLOB, name: "FB_DAMAGE_CLIPS", kind: PropertyKind::Plain }),
        (PropertySlot::Active, PropertySpec { flags: DRM_MODE_PROP_ATOMIC, name: "ACTIVE", kind: PropertyKind::Boolean }),
        (PropertySlot::ModeId, PropertySpec { flags: DRM_MODE_PROP_ATOMIC | DRM_MODE_PROP_BLOB, name: "MODE_ID", kind: PropertyKind::Plain }),
        (PropertySlot::VrrEnabled, PropertySpec { flags: 0, name: "VRR_ENABLED", kind: PropertyKind::Boolean }),
        (PropertySlot::DegammaLut, PropertySpec { flags: DRM_MODE_PROP_BLOB, name: "DEGAMMA_LUT", kind: PropertyKind::Plain }),
        (PropertySlot::DegammaLutSize, PropertySpec { flags: DRM_MODE_PROP_IMMUTABLE, name: "DEGAMMA_LUT_SIZE", kind: PropertyKind::Range { min: 0, max: u32::MAX as u64 } }),
        (PropertySlot::Ctm, PropertySpec { flags: DRM_MODE_PROP_BLOB, name: "CTM", kind: PropertyKind::Plain }),
        (PropertySlot::GammaLut, PropertySpec { flags: DRM_MODE_PROP_BLOB, name: "GAMMA_LUT", kind: PropertyKind::Plain }),
        (PropertySlot::GammaLutSize, PropertySpec { flags: DRM_MODE_PROP_IMMUTABLE, name: "GAMMA_LUT_SIZE", kind: PropertyKind::Range { min: 0, max: u32::MAX as u64 } }),
        (PropertySlot::BackgroundColor, PropertySpec { flags: 0, name: "BACKGROUND_COLOR", kind: PropertyKind::Range { min: 0, max: u64::MAX } }),
        (PropertySlot::Modifiers, PropertySpec { flags: DRM_MODE_PROP_IMMUTABLE | DRM_MODE_PROP_BLOB, name: "IN_FORMATS", kind: PropertyKind::Plain }),
        (PropertySlot::AsyncModifiers, PropertySpec { flags: DRM_MODE_PROP_IMMUTABLE | DRM_MODE_PROP_BLOB, name: "IN_FORMATS_ASYNC", kind: PropertyKind::Plain }),
        (PropertySlot::SizeHints, PropertySpec { flags: DRM_MODE_PROP_IMMUTABLE | DRM_MODE_PROP_BLOB, name: "SIZE_HINTS", kind: PropertyKind::Plain }),
    ];

    for (slot, spec) in standard {
        let id = ops.create_property(device.id, spec).ok_or(ENOMEM)?;
        device.mode_config.properties.set(slot, id);
        device.mode_config.property_list.push(Property { id });
    }
    Ok(())
}

// upstream: drm_mode_config.c drm_mode_config_init_release()
fn drm_mode_config_init_release<O: ModeConfigOps>(device: &mut DrmDevice, ops: &mut O) {
    drm_mode_config_cleanup(device, ops);
}

// upstream: drm_mode_config.c drmm_mode_config_init()
pub fn drmm_mode_config_init<O: ModeConfigOps>(
    device: &mut DrmDevice,
    ops: &mut O,
) -> KResult<()> {
    ops.init_lock(device.id, LockKind::ModeConfigMutex);
    device.mode_config.mutex_initialized = true;
    ops.init_lock(device.id, LockKind::ConnectionMutex);
    device.mode_config.connection_mutex_initialized = true;
    ops.init_lock(device.id, LockKind::IdrMutex);
    device.mode_config.idr_mutex_initialized = true;
    ops.init_lock(device.id, LockKind::FbLock);
    device.mode_config.fb_lock_initialized = true;
    ops.init_lock(device.id, LockKind::BlobLock);
    device.mode_config.blob_lock_initialized = true;

    device.mode_config.framebuffers.clear();
    device.mode_config.crtcs.clear();
    device.mode_config.connectors.clear();
    device.mode_config.encoders.clear();
    device.mode_config.property_list.clear();
    device.mode_config.property_blob_list.clear();
    device.mode_config.planes.clear();
    device.mode_config.colorops.clear();
    device.mode_config.private_objects.clear();
    ops.init_idr(device.id, IdStoreKind::ObjectIdr, 1);
    device.mode_config.object_idr_base = Some(1);
    ops.init_idr(device.id, IdStoreKind::TileIdr, 1);
    device.mode_config.tile_idr_base = Some(1);
    ops.init_connector_ida(device.id);
    device.mode_config.connector_ida_initialized = true;
    ops.init_lock(device.id, LockKind::ConnectorListSpinlock);
    device.mode_config.connector_list_lock_initialized = true;
    ops.init_connector_free_list(device.id);
    device.mode_config.connector_free_list_initialized = true;
    ops.init_work(device.id);
    device.mode_config.connector_free_work_initialized = true;

    if let Err(ret) = drm_mode_create_standard_properties(device, ops) {
        drm_mode_config_cleanup(device, ops);
        return Err(ret);
    }

    // Just to be sure, matching the explicit counter resets in the C source.
    device.mode_config.num_fb = 0;
    device.mode_config.num_connector = 0;
    device.mode_config.num_crtc = 0;
    device.mode_config.num_encoder = 0;
    device.mode_config.num_total_plane = 0;
    device.mode_config.num_colorop = 0;

    if ops.lockdep_enabled() {
        ops.dma_resv_init();
        ops.modeset_acquire_init();
        let ret = ops.modeset_lock(device.id);
        if ret == EDEADLK {
            let _ = ops.modeset_backoff(device.id);
        }
        ops.might_fault();
        ops.ww_acquire_init();
        let ret = ops.dma_resv_lock();
        if ret == EDEADLK {
            ops.dma_resv_lock_slow();
        }
        ops.dma_resv_unlock();
        ops.ww_acquire_fini();
        ops.modeset_drop_locks();
        ops.modeset_acquire_fini();
        ops.dma_resv_fini();
    }

    match ops.add_mode_config_cleanup_action(device.id) {
        Ok(()) => Ok(()),
        Err(ret) => {
            // drmm_add_action_or_reset() runs the cleanup action on failure.
            drm_mode_config_init_release(device, ops);
            Err(ret)
        }
    }
}

// upstream: drm_mode_config.c drm_mode_config_cleanup()
pub fn drm_mode_config_cleanup<O: ModeConfigOps>(device: &mut DrmDevice, ops: &mut O) {
    let id = device.id;
    for encoder in device.mode_config.encoders.clone() {
        ops.destroy_object(id, ObjectKind::Encoder, encoder.id);
        device.mode_config.encoders.retain(|item| item.id != encoder.id);
    }

    ops.connector_list_iter_begin(id);
    let connector_ids: Vec<ObjectId> = device.mode_config.connectors.iter().map(|c| c.id).collect();
    for connector in connector_ids {
        ops.connector_put(device, connector);
    }
    ops.connector_list_iter_end(id);
    // connector_list_iter drops references from a work item.
    ops.flush_connector_free_work(device);
    if !device.mode_config.connectors.is_empty() {
        ops.warn("connector list is not empty during DRM mode-config cleanup".into());
        ops.connector_list_iter_begin(id);
        for connector in &device.mode_config.connectors {
            ops.warn(format!("connector {} leaked!", connector.name));
        }
        ops.connector_list_iter_end(id);
    }

    for property in device.mode_config.property_list.clone() {
        ops.destroy_object(id, ObjectKind::Property, property.id);
        device.mode_config.property_list.retain(|item| item.id != property.id);
    }
    for colorop in device.mode_config.colorops.clone() {
        ops.destroy_object(id, ObjectKind::Colorop, colorop.id);
        device.mode_config.colorops.retain(|item| item.id != colorop.id);
    }
    for plane in device.mode_config.planes.clone() {
        ops.destroy_object(id, ObjectKind::Plane, plane.id);
        device.mode_config.planes.retain(|item| item.id != plane.id);
    }
    for crtc in device.mode_config.crtcs.clone() {
        ops.destroy_object(id, ObjectKind::Crtc, crtc.id);
        device.mode_config.crtcs.retain(|item| item.id != crtc.id);
    }
    for blob in device.mode_config.property_blob_list.clone() {
        ops.property_blob_put(id, blob.id);
        device.mode_config.property_blob_list.retain(|item| item.id != blob.id);
    }

    // Teardown is single-threaded: no fb_lock is taken, matching the source.
    if !device.mode_config.framebuffers.is_empty() {
        ops.warn("framebuffer list is not empty during DRM mode-config cleanup".into());
    }
    for framebuffer in device.mode_config.framebuffers.clone() {
        ops.framebuffer_print_info(id, framebuffer.id);
        ops.framebuffer_free(id, framebuffer.id);
        device.mode_config.framebuffers.retain(|item| item.id != framebuffer.id);
    }

    ops.ida_destroy(id);
    ops.idr_destroy(id, IdStoreKind::TileIdr);
    ops.idr_destroy(id, IdStoreKind::ObjectIdr);
    ops.modeset_lock_fini(id);
}

// upstream: drm_mode_config.c full_encoder_mask()
fn full_encoder_mask<O: ModeConfigOps>(device: &DrmDevice, ops: &mut O) -> u32 {
    let mut encoder_mask = 0;
    for encoder in &device.mode_config.encoders {
        encoder_mask |= ops.encoder_mask(encoder);
    }
    encoder_mask
}

// upstream: drm_mode_config.c fixup_encoder_possible_clones()
fn fixup_encoder_possible_clones<O: ModeConfigOps>(encoder: &mut Encoder, ops: &mut O) {
    if encoder.possible_clones == 0 {
        encoder.possible_clones = ops.encoder_mask(encoder);
    }
}

// upstream: drm_mode_config.c validate_encoder_possible_clones()
fn validate_encoder_possible_clones<O: ModeConfigOps>(
    device: &DrmDevice,
    encoder: &Encoder,
    ops: &mut O,
) {
    let encoder_mask = full_encoder_mask(device, ops);
    for other in &device.mode_config.encoders {
        let encoder_bit = ops.encoder_mask(encoder);
        let other_bit = ops.encoder_mask(other);
        if (encoder.possible_clones & other_bit != 0) != (other.possible_clones & encoder_bit != 0) {
            ops.warn(format!(
                "possible_clones mismatch: [ENCODER:{}:{}] mask=0x{:x} possible_clones=0x{:x} vs. [ENCODER:{}:{}] mask=0x{:x} possible_clones=0x{:x}",
                encoder.id, encoder.name, encoder_bit, encoder.possible_clones,
                other.id, other.name, other_bit, other.possible_clones
            ));
        }
    }
    let self_bit = ops.encoder_mask(encoder);
    if encoder.possible_clones & self_bit == 0 || encoder.possible_clones & !encoder_mask != 0 {
        ops.warn(format!(
            "Bogus possible_clones: [ENCODER:{}:{}] possible_clones=0x{:x} (full encoder mask=0x{:x})",
            encoder.id, encoder.name, encoder.possible_clones, encoder_mask
        ));
    }
}

// upstream: drm_mode_config.c full_crtc_mask()
fn full_crtc_mask<O: ModeConfigOps>(device: &DrmDevice, ops: &mut O) -> u32 {
    let mut crtc_mask = 0;
    for crtc in &device.mode_config.crtcs {
        crtc_mask |= ops.crtc_mask(crtc);
    }
    crtc_mask
}

// upstream: drm_mode_config.c validate_encoder_possible_crtcs()
fn validate_encoder_possible_crtcs<O: ModeConfigOps>(
    device: &DrmDevice,
    encoder: &Encoder,
    ops: &mut O,
) {
    let crtc_mask = full_crtc_mask(device, ops);
    if encoder.possible_crtcs & crtc_mask == 0 || encoder.possible_crtcs & !crtc_mask != 0 {
        ops.warn(format!(
            "Bogus possible_crtcs: [ENCODER:{}:{}] possible_crtcs=0x{:x} (full crtc mask=0x{:x})",
            encoder.id, encoder.name, encoder.possible_crtcs, crtc_mask
        ));
    }
}

// upstream: drm_mode_config.c drm_mode_config_validate()
pub fn drm_mode_config_validate<O: ModeConfigOps>(device: &mut DrmDevice, ops: &mut O) {
    if !device.has_modeset {
        return;
    }

    for encoder in &mut device.mode_config.encoders {
        fixup_encoder_possible_clones(encoder, ops);
    }

    let encoders = device.mode_config.encoders.clone();
    for encoder in &encoders {
        validate_encoder_possible_clones(device, encoder, ops);
        validate_encoder_possible_crtcs(device, encoder, ops);
    }

    let mut primary_with_crtc = 0u32;
    let mut cursor_with_crtc = 0u32;
    for crtc in &device.mode_config.crtcs {
        if crtc.primary.is_none() {
            ops.warn(format!("Missing primary plane on [CRTC:{}:{}]", crtc.id, crtc.name));
        }
        if crtc.cursor.is_some() && crtc.has_cursor_set {
            ops.warn(format!("[CRTC:{}:{}] must not have both a cursor plane and a cursor_set func", crtc.id, crtc.name));
        }
        if crtc.cursor.is_some() && crtc.has_cursor_set2 {
            ops.warn(format!("[CRTC:{}:{}] must not have both a cursor plane and a cursor_set2 func", crtc.id, crtc.name));
        }
        if crtc.cursor.is_some() && crtc.has_cursor_move {
            ops.warn(format!("[CRTC:{}:{}] must not have both a cursor plane and a cursor_move func", crtc.id, crtc.name));
        }

        if let Some(primary) = &crtc.primary {
            let crtc_bit = ops.crtc_mask(crtc);
            if primary.possible_crtcs & crtc_bit == 0 {
                ops.warn(format!(
                    "Bogus primary plane possible_crtcs: [PLANE:{}:{}] must be compatible with [CRTC:{}:{}]",
                    primary.id, primary.name, crtc.id, crtc.name
                ));
            }
            let plane_bit = ops.plane_mask(primary);
            if primary_with_crtc & plane_bit != 0 {
                ops.warn(format!("Primary plane [PLANE:{}:{}] used for multiple CRTCs", primary.id, primary.name));
            }
            primary_with_crtc |= plane_bit;
        }
        if let Some(cursor) = &crtc.cursor {
            let crtc_bit = ops.crtc_mask(crtc);
            if cursor.possible_crtcs & crtc_bit == 0 {
                ops.warn(format!(
                    "Bogus cursor plane possible_crtcs: [PLANE:{}:{}] must be compatible with [CRTC:{}:{}]",
                    cursor.id, cursor.name, crtc.id, crtc.name
                ));
            }
            let plane_bit = ops.plane_mask(cursor);
            if cursor_with_crtc & plane_bit != 0 {
                ops.warn(format!("Cursor plane [PLANE:{}:{}] used for multiple CRTCs", cursor.id, cursor.name));
            }
            cursor_with_crtc |= plane_bit;
        }
    }

    let num_primary = device.mode_config.planes.iter()
        .filter(|plane| plane.plane_type == DRM_PLANE_TYPE_PRIMARY)
        .count();
    if num_primary != device.mode_config.num_crtc as usize {
        ops.warn(format!(
            "Must have as many primary planes as there are CRTCs, but have {} primary planes and {} CRTCs",
            num_primary, device.mode_config.num_crtc
        ));
    }
}
