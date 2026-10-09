//! Function-level Rust translation of Linux v7.2.3
//! `drivers/gpu/drm/drm_crtc.c` (upstream has no SPDX identifier; the
//! copyright header supplies the permissive license text reproduced below).
//!
//! Copyright (c) 2006-2008 Intel Corporation
//! Copyright (c) 2007 Dave Airlie <airlied@linux.ie>
//! Copyright (c) 2008 Red Hat Inc.
//!
//! DRM core CRTC related functions.  The source's object model, ioctl user
//! copies, locks, property/fence services, and driver callbacks are expressed
//! at the `CrtcUapiIo` boundary; CRTC UAPI policy and ordering remain here.
// Permission to use, copy, modify, distribute, and sell this software and its
// documentation for any purpose is hereby granted without fee, provided that
// the above copyright notice appear in all copies and that both that copyright
// notice and this permission notice appear in supporting documentation, and
// that the name of the copyright holders not be used in advertising or
// publicity pertaining to distribution of the software without specific,
// written prior permission.  The copyright holders make no representations
// about the suitability of this software for any purpose.  It is provided "as
// is" without express or implied warranty.
//
// THE COPYRIGHT HOLDERS DISCLAIM ALL WARRANTIES WITH REGARD TO THIS SOFTWARE,
// INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS, IN NO
// EVENT SHALL THE COPYRIGHT HOLDERS BE LIABLE FOR ANY SPECIAL, INDIRECT OR
// CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE,
// DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER
// TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE
// OF THIS SOFTWARE.
//
// Authors:
//      Keith Packard
//      Eric Anholt <eric@anholt.net>
//      Dave Airlie <airlied@linux.ie>
//      Jesse Barnes <jesse.barnes@intel.com>

#![no_std]
extern crate alloc;

use alloc::{format, string::String, vec::Vec};

pub type DrmId = u32;
pub type UserPtr = u64;
pub type CrtcResult<T> = Result<T, i32>;

pub const EACCES: i32 = -13;
pub const EFAULT: i32 = -14;
pub const EINVAL: i32 = -22;
pub const ENOENT: i32 = -2;
pub const ENOMEM: i32 = -12;
pub const EOPNOTSUPP: i32 = -95;
pub const ERANGE: i32 = -34;
pub const DRIVER_MODESET: u32 = 1 << 0;
pub const DRIVER_ATOMIC: u32 = 1 << 1;
pub const DRM_MODE_OBJECT_CRTC: u32 = 0xcccc_cccc;
pub const DRM_MODE_FLAG_PIC_AR_MASK: u32 = 0x0f << 19;
pub const DRM_MODE_FLAG_PIC_AR_NONE: u32 = 0;
pub const DRM_PLANE_TYPE_PRIMARY: u32 = 1;
pub const DRM_PLANE_TYPE_CURSOR: u32 = 2;
pub const DRM_SCALING_FILTER_DEFAULT: u64 = 0;
pub const DRM_MODE_PROP_RANGE: u32 = 1 << 1;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayMode {
    pub name: String,
    pub flags: u32,
    pub hdisplay: u32,
    pub vdisplay: u32,
    pub status: i32,
    pub opaque: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct CrtcState {
    pub enable: bool,
    pub encoder_mask: u32,
    pub mode: DisplayMode,
}

#[derive(Clone, Debug, Default)]
pub struct PlaneState {
    pub fb: Option<DrmId>,
    pub src_x: u32,
    pub src_y: u32,
    pub rotation: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Plane {
    pub id: DrmId,
    pub plane_type: u32,
    pub possible_crtcs: u32,
    pub format_default: bool,
    pub format_modifiers: Vec<(u32, u64)>,
    pub state: Option<PlaneState>,
    pub crtc: Option<DrmId>,
    pub fb: Option<DrmId>,
    pub old_fb: Option<DrmId>,
}

#[derive(Clone, Debug, Default)]
pub struct CrtcFuncs {
    pub destroy: bool,
    pub late_register: bool,
    pub early_unregister: bool,
    pub atomic_destroy_state: bool,
    pub atomic_duplicate_state: bool,
    pub set_config: bool,
    pub set_property: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Crtc {
    pub id: DrmId,
    pub device: DrmId,
    pub name: String,
    pub index: usize,
    pub funcs: CrtcFuncs,
    pub primary: Option<DrmId>,
    pub cursor: Option<DrmId>,
    pub state: Option<CrtcState>,
    pub mode: DisplayMode,
    pub enabled: bool,
    pub x: i32,
    pub y: i32,
    pub gamma_size: u32,
    pub gamma_store: Option<Vec<u16>>,
    pub timeline_name: String,
    pub fence_context: u64,
    pub fence_seqno: u64,
    pub scaling_filter_property: Option<DrmId>,
    pub sharpness_strength_property: Option<DrmId>,
    pub property_values: Vec<(DrmId, u64)>,
    pub crc_source: Option<String>,
    pub crc_lock_initialized: bool,
    pub crc_waitqueue_initialized: bool,
    pub mutex_initialized: bool,
    pub commit_lock_initialized: bool,
    pub fence_lock_initialized: bool,
    pub commit_list_initialized: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Framebuffer {
    pub id: DrmId,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub modifier: u64,
    pub refs: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Connector {
    pub id: DrmId,
    pub name: String,
    pub refs: usize,
}

#[derive(Clone, Debug, Default)]
pub struct ModeConfig {
    pub num_crtc: usize,
    pub num_connector: usize,
    pub prop_active: DrmId,
    pub prop_mode_id: DrmId,
    pub prop_out_fence_ptr: DrmId,
    pub prop_vrr_enabled: DrmId,
}

#[derive(Clone, Debug, Default)]
pub struct DrmDevice {
    pub id: DrmId,
    pub driver_name: String,
    pub driver_features: u32,
    pub atomic_modeset: bool,
    pub mode_config: ModeConfig,
    pub crtcs: Vec<Crtc>,
    pub planes: Vec<Plane>,
    pub connectors: Vec<Connector>,
    pub framebuffers: Vec<Framebuffer>,
    pub next_object_id: DrmId,
}

#[derive(Clone, Debug, Default)]
pub struct DrmFile {
    pub id: DrmId,
    pub aspect_ratio_allowed: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ModesetAcquireCtx {
    pub id: u64,
}

#[derive(Clone, Debug, Default)]
pub struct ModeSet {
    pub crtc: DrmId,
    pub x: i32,
    pub y: i32,
    pub mode: Option<DisplayMode>,
    pub connectors: Vec<DrmId>,
    pub fb: Option<DrmId>,
}

#[derive(Clone, Debug, Default)]
pub struct ModeCrtc {
    pub crtc_id: DrmId,
    pub fb_id: i64,
    pub x: i32,
    pub y: i32,
    pub count_connectors: usize,
    pub set_connectors_ptr: UserPtr,
    pub mode_valid: bool,
    pub mode: DisplayMode,
    pub gamma_size: u32,
    pub returned_fb_id: DrmId,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ModeObject {
    pub id: DrmId,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DrmProperty {
    pub id: DrmId,
}

#[derive(Clone, Debug, Default)]
pub struct DmaFence {
    pub crtc: DrmId,
    pub context: u64,
    pub seqno: u64,
    pub uses_crtc_ops: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ManagedCrtcContainer {
    pub size: usize,
    pub offset: usize,
    pub crtc: Crtc,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CrtcPropertySpec {
    pub name: &'static str,
    pub flags: u32,
    pub min: u64,
    pub max: u64,
}

/// Kernel/framework operations deliberately kept outside the source translation.
/// Overrides supply DRM object lifetime, user access, locking, and callbacks.
pub trait CrtcUapiIo {
    fn warn(&mut self, _device: DrmId, _message: &'static str) {}
    fn debug(&mut self, _device: DrmId, _message: &str) {}
    fn error(&mut self, _device: DrmId, _message: &'static str) {}
    fn mode_object_add(&mut self, _device: DrmId, object: &mut DrmId, _kind: u32) -> i32 {
        if *object == 0 {
            *object = 1;
        }
        0
    }
    fn mode_object_unregister(&mut self, _device: DrmId, _object: DrmId) {}
    fn modeset_lock_init(&mut self, _crtc: DrmId) {}
    fn modeset_lock_fini(&mut self, _crtc: DrmId) {}
    fn modeset_lock(&mut self, _object: DrmId, _ctx: Option<&mut ModesetAcquireCtx>) -> i32 {
        0
    }
    fn modeset_unlock(&mut self, _object: DrmId) {}
    fn modeset_acquire_init(&mut self, _ctx: &mut ModesetAcquireCtx) {}
    fn lock_all(&mut self, _device: DrmId, _ctx: &mut ModesetAcquireCtx) -> i32 {
        0
    }
    fn unlock_all(&mut self, _device: DrmId, _ctx: &mut ModesetAcquireCtx, _ret: i32) {}
    fn debugfs_crtc_add(&mut self, _crtc: DrmId) {}
    fn debugfs_crtc_remove(&mut self, _crtc: DrmId) {}
    fn crtc_late_register(&mut self, _crtc: DrmId) -> i32 {
        0
    }
    fn crtc_early_unregister(&mut self, _crtc: DrmId) {}
    fn debug_fs_enabled(&mut self) -> bool {
        true
    }
    fn crc_init(&mut self, _crtc: DrmId) -> i32 {
        0
    }
    fn crc_fini(&mut self, _crtc: DrmId) {}
    fn alloc_fence_context(&mut self, _count: u32) -> u64 {
        1
    }
    fn fence_alloc(&mut self) -> bool {
        true
    }
    fn dma_fence_init(&mut self, _fence: &mut DmaFence, _crtc: DrmId, _context: u64, _seqno: u64) {}
    fn set_crtc_state_destroy(&mut self, _crtc: DrmId, _state: &CrtcState) {}
    fn attach_property(&mut self, _object: DrmId, _property: DrmId, _value: u64) {}
    fn add_managed_crtc_cleanup(&mut self, _device: DrmId, _crtc: DrmId) -> i32 {
        0
    }
    fn managed_crtc_alloc(&mut self, _device: DrmId, _size: usize) -> bool {
        true
    }
    fn managed_crtc_free(&mut self, _device: DrmId, _container: &mut ManagedCrtcContainer) {}
    fn uses_atomic_modeset(&mut self, device: &DrmDevice) -> bool {
        device.atomic_modeset
    }
    fn core_check_feature(&mut self, device: &DrmDevice, feature: u32) -> bool {
        device.driver_features & feature != 0
    }
    fn crtc_set_config(&mut self, _set: &ModeSet, _ctx: Option<&mut ModesetAcquireCtx>) -> i32 {
        EOPNOTSUPP
    }
    fn crtc_set_property(&mut self, _crtc: DrmId, _property: DrmId, _value: u64) -> i32 {
        EINVAL
    }
    fn property_set_value(&mut self, _object: DrmId, _property: DrmId, _value: u64) {}
    fn scaling_filter_property_create(
        &mut self,
        _device: DrmId,
        _supported: u32,
    ) -> CrtcResult<DrmId> {
        Err(ENOMEM)
    }
    fn range_property_create(&mut self, _device: DrmId, _spec: CrtcPropertySpec) -> Option<DrmId> {
        None
    }
    fn property_attach(&mut self, _object: DrmId, _property: DrmId, _value: u64) {}
    fn find_crtc(&mut self, device: &DrmDevice, file: &DrmFile, id: DrmId) -> Option<usize> {
        if !self.lease_held(file, id) {
            return None;
        }
        device.crtcs.iter().position(|crtc| crtc.id == id)
    }
    fn lease_held(&mut self, _file: &DrmFile, _object: DrmId) -> bool {
        true
    }
    fn framebuffer_lookup(
        &mut self,
        device: &mut DrmDevice,
        file: &DrmFile,
        id: DrmId,
    ) -> Option<DrmId> {
        if !self.lease_held(file, id) {
            return None;
        }
        let fb = device.framebuffers.iter_mut().find(|fb| fb.id == id)?;
        fb.refs += 1;
        Some(id)
    }
    fn framebuffer_get(&mut self, device: &mut DrmDevice, id: DrmId) {
        if let Some(fb) = device.framebuffers.iter_mut().find(|fb| fb.id == id) {
            fb.refs += 1;
        }
    }
    fn framebuffer_put(&mut self, device: &mut DrmDevice, id: DrmId) {
        if let Some(fb) = device.framebuffers.iter_mut().find(|fb| fb.id == id) {
            fb.refs = fb.refs.saturating_sub(1);
        }
    }
    fn connector_lookup(
        &mut self,
        device: &mut DrmDevice,
        file: &DrmFile,
        id: DrmId,
    ) -> Option<DrmId> {
        if !self.lease_held(file, id) {
            return None;
        }
        let connector = device
            .connectors
            .iter_mut()
            .find(|connector| connector.id == id)?;
        connector.refs += 1;
        Some(id)
    }
    fn connector_put(&mut self, device: &mut DrmDevice, id: DrmId) {
        if let Some(connector) = device
            .connectors
            .iter_mut()
            .find(|connector| connector.id == id)
        {
            connector.refs = connector.refs.saturating_sub(1);
        }
    }
    fn get_user_u32(&mut self, _pointer: UserPtr, _index: usize) -> Result<u32, i32> {
        Err(EFAULT)
    }
    fn mode_create(&mut self, _device: DrmId) -> Option<DisplayMode> {
        Some(DisplayMode::default())
    }
    fn mode_destroy(&mut self, _device: DrmId, _mode: Option<DisplayMode>) {}
    fn mode_convert_umode(
        &mut self,
        _device: DrmId,
        dst: &mut DisplayMode,
        src: &DisplayMode,
    ) -> i32 {
        *dst = src.clone();
        0
    }
    fn mode_convert_to_umode(&mut self, dst: &mut DisplayMode, src: &DisplayMode) {
        *dst = src.clone();
    }
    fn mode_status_name(&mut self, _status: i32) -> &'static str {
        "unknown"
    }
    fn plane_has_format(&mut self, plane: DrmId, format: u32, modifier: u64) -> bool;
    fn mode_get_hv_timing(&mut self, mode: &DisplayMode) -> (i32, i32) {
        (mode.hdisplay as i32, mode.vdisplay as i32)
    }
    fn rotation_90_or_270(&mut self, rotation: u32) -> bool {
        rotation & 0x6 != 0
    }
    fn framebuffer_check_src_coords(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        fb: &Framebuffer,
    ) -> i32 {
        if x < 0
            || y < 0
            || w <= 0
            || h <= 0
            || (x as i64 + w as i64) > ((fb.width as i64) << 16)
            || (y as i64 + h as i64) > ((fb.height as i64) << 16)
        {
            EINVAL
        } else {
            0
        }
    }
}

// upstream: drm_crtc.c drm_crtc_from_index()
pub fn drm_crtc_from_index(device: &DrmDevice, index: i32) -> Option<&Crtc> {
    if index < 0 {
        return None;
    }
    device
        .crtcs
        .iter()
        .find(|crtc| crtc.index == index as usize)
}

// upstream: drm_crtc.c drm_crtc_force_disable()
pub fn drm_crtc_force_disable<I: CrtcUapiIo>(
    io: &mut I,
    device: &mut DrmDevice,
    crtc_id: DrmId,
) -> i32 {
    let Some(crtc) = device.crtcs.iter().find(|crtc| crtc.id == crtc_id) else {
        return ENOENT;
    };
    if io.uses_atomic_modeset(device) {
        io.warn(crtc.device, "force-disable used with atomic modesetting");
    }
    let set = ModeSet {
        crtc: crtc_id,
        ..ModeSet::default()
    };
    drm_mode_set_config_internal(io, device, &set)
}

// upstream: drm_crtc.c drm_crtc_register_all()
pub fn drm_crtc_register_all<I: CrtcUapiIo>(io: &mut I, device: &DrmDevice) -> i32 {
    let crtcs: Vec<(DrmId, bool)> = device
        .crtcs
        .iter()
        .map(|crtc| (crtc.id, crtc.funcs.late_register))
        .collect();
    for (crtc, has_late_register) in crtcs {
        io.debugfs_crtc_add(crtc);
        if has_late_register {
            let ret = io.crtc_late_register(crtc);
            if ret != 0 {
                return ret;
            }
        }
    }
    0
}

// upstream: drm_crtc.c drm_crtc_unregister_all()
pub fn drm_crtc_unregister_all<I: CrtcUapiIo>(io: &mut I, device: &DrmDevice) {
    let crtcs: Vec<(DrmId, bool)> = device
        .crtcs
        .iter()
        .map(|crtc| (crtc.id, crtc.funcs.early_unregister))
        .collect();
    for (crtc, has_early_unregister) in crtcs {
        if has_early_unregister {
            io.crtc_early_unregister(crtc);
        }
        io.debugfs_crtc_remove(crtc);
    }
}

// upstream: drm_crtc.c drm_crtc_crc_init()
fn drm_crtc_crc_init<I: CrtcUapiIo>(io: &mut I, device: &mut DrmDevice, crtc_id: DrmId) -> i32 {
    if !io.debug_fs_enabled() {
        return 0;
    }
    if let Some(crtc) = device.crtcs.iter_mut().find(|crtc| crtc.id == crtc_id) {
        crtc.crc_lock_initialized = true;
        crtc.crc_waitqueue_initialized = true;
    } else {
        return ENOENT;
    }
    let ret = io.crc_init(crtc_id);
    if ret != 0 {
        return ret;
    }
    if let Some(crtc) = device.crtcs.iter_mut().find(|crtc| crtc.id == crtc_id) {
        crtc.crc_source = Some(String::from("auto"));
        0
    } else {
        ENOENT
    }
}

// upstream: drm_crtc.c drm_crtc_crc_fini()
fn drm_crtc_crc_fini<I: CrtcUapiIo>(io: &mut I, device: &mut DrmDevice, crtc_id: DrmId) {
    if !io.debug_fs_enabled() {
        return;
    }
    if let Some(crtc) = device.crtcs.iter_mut().find(|crtc| crtc.id == crtc_id) {
        crtc.crc_source = None;
    }
    io.crc_fini(crtc_id);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CrtcFenceOps;
pub const DRM_CRTC_FENCE_OPS: CrtcFenceOps = CrtcFenceOps;

// upstream: drm_crtc.c fence_to_crtc()
fn fence_to_crtc<'a>(device: &'a DrmDevice, fence: &DmaFence) -> &'a Crtc {
    assert!(
        fence.uses_crtc_ops,
        "fence ops do not match drm_crtc_fence_ops"
    );
    device
        .crtcs
        .iter()
        .find(|crtc| crtc.id == fence.crtc)
        .expect("CRTC fence extern_lock does not point into a registered CRTC")
}

// upstream: drm_crtc.c drm_crtc_fence_get_driver_name()
pub fn drm_crtc_fence_get_driver_name<'a>(device: &'a DrmDevice, fence: &DmaFence) -> &'a str {
    let crtc = fence_to_crtc(device, fence);
    assert_eq!(crtc.device, device.id);
    &device.driver_name
}

// upstream: drm_crtc.c drm_crtc_fence_get_timeline_name()
pub fn drm_crtc_fence_get_timeline_name<'a>(device: &'a DrmDevice, fence: &DmaFence) -> &'a str {
    &fence_to_crtc(device, fence).timeline_name
}

// upstream: drm_crtc.c drm_crtc_create_fence()
pub fn drm_crtc_create_fence<I: CrtcUapiIo>(
    io: &mut I,
    device: &mut DrmDevice,
    crtc_id: DrmId,
) -> Option<DmaFence> {
    if !io.fence_alloc() {
        return None;
    }
    let (context, seqno) = {
        let crtc = device.crtcs.iter_mut().find(|crtc| crtc.id == crtc_id)?;
        crtc.fence_seqno = crtc.fence_seqno.wrapping_add(1);
        (crtc.fence_context, crtc.fence_seqno)
    };
    let mut fence = DmaFence {
        crtc: crtc_id,
        context,
        seqno,
        uses_crtc_ops: true,
    };
    io.dma_fence_init(&mut fence, crtc_id, context, seqno);
    Some(fence)
}

fn crtc_mask(crtc: &Crtc) -> u32 {
    1u32.checked_shl(crtc.index as u32).unwrap_or(0)
}

fn crtc_index(device: &DrmDevice, crtc_id: DrmId) -> Option<usize> {
    device.crtcs.iter().position(|crtc| crtc.id == crtc_id)
}

fn plane_index(device: &DrmDevice, plane_id: DrmId) -> Option<usize> {
    device.planes.iter().position(|plane| plane.id == plane_id)
}

fn framebuffer_index(device: &DrmDevice, fb_id: DrmId) -> Option<usize> {
    device.framebuffers.iter().position(|fb| fb.id == fb_id)
}

// upstream: drm_crtc.c __drm_crtc_init_with_planes()
fn __drm_crtc_init_with_planes<I: CrtcUapiIo>(
    io: &mut I,
    device: &mut DrmDevice,
    mut crtc: Crtc,
    primary: Option<DrmId>,
    cursor: Option<DrmId>,
    funcs: CrtcFuncs,
    name: Option<&str>,
) -> CrtcResult<DrmId> {
    let num_crtc = device.mode_config.num_crtc;
    if let Some(plane_id) = primary {
        if let Some(plane) = device.planes.iter().find(|plane| plane.id == plane_id) {
            if plane.plane_type != DRM_PLANE_TYPE_PRIMARY {
                io.warn(device.id, "primary plane is not DRM_PLANE_TYPE_PRIMARY");
            }
        }
    }
    if let Some(plane_id) = cursor {
        if let Some(plane) = device.planes.iter().find(|plane| plane.id == plane_id) {
            if plane.plane_type != DRM_PLANE_TYPE_CURSOR {
                io.warn(device.id, "cursor plane is not DRM_PLANE_TYPE_CURSOR");
            }
        }
    }

    // CRTC index is used with 32-bit bitmasks.
    if num_crtc >= 32 {
        io.warn(device.id, "mode_config.num_crtc >= 32");
        return Err(EINVAL);
    }
    if io.uses_atomic_modeset(device)
        && (!funcs.atomic_destroy_state || !funcs.atomic_duplicate_state)
    {
        io.warn(
            device.id,
            "atomic modeset CRTC must provide atomic destroy and duplicate state callbacks",
        );
    }

    crtc.device = device.id;
    crtc.funcs = funcs.clone();
    crtc.commit_list_initialized = true;
    crtc.commit_lock_initialized = true;
    io.modeset_lock_init(crtc.id);
    crtc.mutex_initialized = true;
    let ret = io.mode_object_add(device.id, &mut crtc.id, DRM_MODE_OBJECT_CRTC);
    if ret != 0 {
        return Err(ret);
    }

    crtc.name = if let Some(name) = name {
        String::from(name)
    } else {
        format!("crtc-{}", num_crtc)
    };
    if crtc.name.is_empty() {
        io.mode_object_unregister(device.id, crtc.id);
        return Err(ENOMEM);
    }

    crtc.fence_context = io.alloc_fence_context(1);
    crtc.fence_lock_initialized = true;
    crtc.timeline_name = format!("CRTC:{}-{}", crtc.id, crtc.name);
    crtc.primary = primary;
    crtc.cursor = cursor;
    crtc.index = num_crtc;

    if let Some(plane_id) = primary {
        if let Some(index) = plane_index(device, plane_id) {
            if device.planes[index].possible_crtcs == 0 {
                device.planes[index].possible_crtcs = crtc_mask(&crtc);
            }
        }
    }
    if let Some(plane_id) = cursor {
        if let Some(index) = plane_index(device, plane_id) {
            if device.planes[index].possible_crtcs == 0 {
                device.planes[index].possible_crtcs = crtc_mask(&crtc);
            }
        }
    }

    device.crtcs.push(crtc);
    device.mode_config.num_crtc += 1;
    let ret = drm_crtc_crc_init(io, device, device.crtcs.last().expect("inserted CRTC").id);
    if ret != 0 {
        let id = device.crtcs.last().expect("inserted CRTC").id;
        io.mode_object_unregister(device.id, id);
        return Err(ret);
    }

    if io.core_check_feature(device, DRIVER_ATOMIC) {
        let id = device.crtcs.last().expect("inserted CRTC").id;
        let props = device.mode_config.clone();
        io.attach_property(id, props.prop_active, 0);
        io.attach_property(id, props.prop_mode_id, 0);
        io.attach_property(id, props.prop_out_fence_ptr, 0);
        io.attach_property(id, props.prop_vrr_enabled, 0);
    }
    Ok(device.crtcs.last().expect("inserted CRTC").id)
}

// upstream: drm_crtc.c drm_crtc_init_with_planes()
pub fn drm_crtc_init_with_planes<I: CrtcUapiIo>(
    io: &mut I,
    device: &mut DrmDevice,
    crtc: Crtc,
    primary: Option<DrmId>,
    cursor: Option<DrmId>,
    funcs: CrtcFuncs,
    name: Option<&str>,
) -> i32 {
    if !funcs.destroy {
        io.warn(device.id, "non-managed CRTC requires destroy callback");
    }
    match __drm_crtc_init_with_planes(io, device, crtc, primary, cursor, funcs, name) {
        Ok(_) => 0,
        Err(ret) => ret,
    }
}

// upstream: drm_crtc.c drmm_crtc_init_with_planes_cleanup()
fn drmm_crtc_init_with_planes_cleanup<I: CrtcUapiIo>(
    io: &mut I,
    device: &mut DrmDevice,
    crtc_id: DrmId,
) {
    drm_crtc_cleanup(io, device, crtc_id);
}

// upstream: drm_crtc.c __drmm_crtc_init_with_planes()
fn __drmm_crtc_init_with_planes<I: CrtcUapiIo>(
    io: &mut I,
    device: &mut DrmDevice,
    crtc: Crtc,
    primary: Option<DrmId>,
    cursor: Option<DrmId>,
    funcs: CrtcFuncs,
    name: Option<&str>,
) -> i32 {
    if funcs.destroy {
        io.warn(device.id, "managed CRTC must not provide destroy callback");
    }
    let crtc_id = match __drm_crtc_init_with_planes(io, device, crtc, primary, cursor, funcs, name)
    {
        Ok(id) => id,
        Err(ret) => return ret,
    };
    let ret = io.add_managed_crtc_cleanup(device.id, crtc_id);
    if ret != 0 {
        drmm_crtc_init_with_planes_cleanup(io, device, crtc_id);
        return ret;
    }
    0
}

// upstream: drm_crtc.c drmm_crtc_init_with_planes()
pub fn drmm_crtc_init_with_planes<I: CrtcUapiIo>(
    io: &mut I,
    device: &mut DrmDevice,
    crtc: Crtc,
    primary: Option<DrmId>,
    cursor: Option<DrmId>,
    funcs: CrtcFuncs,
    name: Option<&str>,
) -> i32 {
    __drmm_crtc_init_with_planes(io, device, crtc, primary, cursor, funcs, name)
}

// upstream: drm_crtc.c __drmm_crtc_alloc_with_planes()
pub fn __drmm_crtc_alloc_with_planes<I: CrtcUapiIo>(
    io: &mut I,
    device: &mut DrmDevice,
    size: usize,
    offset: usize,
    primary: Option<DrmId>,
    cursor: Option<DrmId>,
    funcs: Option<CrtcFuncs>,
    name: Option<&str>,
) -> CrtcResult<ManagedCrtcContainer> {
    let Some(funcs) = funcs else {
        io.warn(
            device.id,
            "managed CRTC allocation requires funcs without destroy callback",
        );
        return Err(EINVAL);
    };
    if funcs.destroy {
        io.warn(
            device.id,
            "managed CRTC allocation requires funcs without destroy callback",
        );
        return Err(EINVAL);
    }
    if !io.managed_crtc_alloc(device.id, size) {
        return Err(ENOMEM);
    }

    let mut container = ManagedCrtcContainer {
        size,
        offset,
        crtc: Crtc::default(),
    };
    let mut crtc = Crtc::default();
    crtc.id = device.next_object_id;
    device.next_object_id = device.next_object_id.wrapping_add(1).max(1);
    let ret = __drmm_crtc_init_with_planes(io, device, crtc, primary, cursor, funcs, name);
    if ret != 0 {
        io.managed_crtc_free(device.id, &mut container);
        return Err(ret);
    }
    let id = device.crtcs.last().expect("initialized CRTC").id;
    container.crtc = device.crtcs[crtc_index(device, id).expect("registered CRTC")].clone();
    Ok(container)
}

// upstream: drm_crtc.c drm_crtc_cleanup()
pub fn drm_crtc_cleanup<I: CrtcUapiIo>(io: &mut I, device: &mut DrmDevice, crtc_id: DrmId) {
    let Some(index) = crtc_index(device, crtc_id) else {
        return;
    };
    let crtc = device.crtcs[index].clone();

    // The CRTC list is treated as static; runtime removal would require
    // decrementing all subsequent indices.
    drm_crtc_crc_fini(io, device, crtc_id);
    if let Some(crtc) = device.crtcs.get_mut(index) {
        crtc.gamma_store = None;
    }
    io.modeset_lock_fini(crtc_id);
    io.mode_object_unregister(device.id, crtc_id);
    device.crtcs.remove(index);
    device.mode_config.num_crtc = device.mode_config.num_crtc.saturating_sub(1);

    if let Some(state) = crtc.state.as_ref() {
        if !crtc.funcs.atomic_destroy_state {
            io.warn(
                device.id,
                "CRTC state present without atomic_destroy_state callback",
            );
        } else {
            io.set_crtc_state_destroy(crtc_id, state);
        }
    }
    // C memset() clears the entire embedded object after its owned resources
    // and list links have been released.
}

// upstream: drm_crtc.c drm_mode_getcrtc()
pub fn drm_mode_getcrtc<I: CrtcUapiIo>(
    io: &mut I,
    device: &mut DrmDevice,
    data: &mut ModeCrtc,
    file: &DrmFile,
) -> i32 {
    if !io.core_check_feature(device, DRIVER_MODESET) {
        return EOPNOTSUPP;
    }
    let Some(crtc_index) = io.find_crtc(device, file, data.crtc_id) else {
        return ENOENT;
    };
    let Some(primary_id) = device.crtcs[crtc_index].primary else {
        return ENOENT;
    };
    let Some(plane_index) = plane_index(device, primary_id) else {
        return ENOENT;
    };

    data.gamma_size = device.crtcs[crtc_index].gamma_size;
    let _ = io.modeset_lock(primary_id, None);
    let plane = &device.planes[plane_index];
    data.returned_fb_id = if let Some(state) = plane.state.as_ref() {
        state.fb.unwrap_or_default()
    } else {
        plane.fb.unwrap_or(0)
    };
    if let Some(state) = plane.state.as_ref() {
        data.x = (state.src_x >> 16) as i32;
        data.y = (state.src_y >> 16) as i32;
    }
    io.modeset_unlock(primary_id);

    let _ = io.modeset_lock(data.crtc_id, None);
    let crtc = &device.crtcs[crtc_index];
    if let Some(state) = crtc.state.as_ref() {
        if state.enable {
            io.mode_convert_to_umode(&mut data.mode, &state.mode);
            data.mode_valid = true;
        } else {
            data.mode_valid = false;
        }
    } else {
        data.x = crtc.x;
        data.y = crtc.y;
        if crtc.enabled {
            io.mode_convert_to_umode(&mut data.mode, &crtc.mode);
            data.mode_valid = true;
        } else {
            data.mode_valid = false;
        }
    }
    if !file.aspect_ratio_allowed {
        data.mode.flags &= !DRM_MODE_FLAG_PIC_AR_MASK;
    }
    io.modeset_unlock(data.crtc_id);
    0
}

// upstream: drm_crtc.c __drm_mode_set_config_internal()
fn __drm_mode_set_config_internal<I: CrtcUapiIo>(
    io: &mut I,
    device: &mut DrmDevice,
    set: &ModeSet,
    ctx: Option<&mut ModesetAcquireCtx>,
) -> i32 {
    let Some(crtc_index) = crtc_index(device, set.crtc) else {
        return ENOENT;
    };
    if io.uses_atomic_modeset(device) {
        io.warn(device.id, "legacy set_config used with atomic modesetting");
    }

    // ->set_config may disable other CRTCs when connectors are stolen, so hold
    // framebuffer references across the complete CRTC set.
    let primary_planes: Vec<DrmId> = device
        .crtcs
        .iter()
        .filter_map(|crtc| crtc.primary)
        .collect();
    for plane_id in &primary_planes {
        if let Some(index) = plane_index(device, *plane_id) {
            device.planes[index].old_fb = device.planes[index].fb;
        }
    }

    let ret = io.crtc_set_config(set, ctx);
    if ret == 0 {
        if let Some(plane_id) = device.crtcs[crtc_index].primary {
            if let Some(index) = plane_index(device, plane_id) {
                device.planes[index].crtc = if set.fb.is_some() {
                    Some(set.crtc)
                } else {
                    None
                };
                device.planes[index].fb = set.fb;
            }
        }
    }

    for plane_id in primary_planes {
        if let Some(index) = plane_index(device, plane_id) {
            let fb = device.planes[index].fb;
            let old_fb = device.planes[index].old_fb;
            if let Some(fb) = fb {
                io.framebuffer_get(device, fb);
            }
            if let Some(old_fb) = old_fb {
                io.framebuffer_put(device, old_fb);
            }
            if let Some(index) = plane_index(device, plane_id) {
                device.planes[index].old_fb = None;
            }
        }
    }
    ret
}

// upstream: drm_crtc.c drm_mode_set_config_internal()
pub fn drm_mode_set_config_internal<I: CrtcUapiIo>(
    io: &mut I,
    device: &mut DrmDevice,
    set: &ModeSet,
) -> i32 {
    if io.uses_atomic_modeset(device) {
        io.warn(device.id, "legacy set_config used with atomic modesetting");
    }
    __drm_mode_set_config_internal(io, device, set, None)
}

// upstream: drm_crtc.c drm_crtc_check_viewport()
pub fn drm_crtc_check_viewport<I: CrtcUapiIo>(
    io: &mut I,
    device: &DrmDevice,
    crtc_id: DrmId,
    x: i32,
    y: i32,
    mode: &DisplayMode,
    fb_id: DrmId,
) -> i32 {
    let Some(crtc) = device.crtcs.iter().find(|crtc| crtc.id == crtc_id) else {
        return ENOENT;
    };
    let Some(primary_id) = crtc.primary else {
        return ENOENT;
    };
    let Some(plane) = device.planes.iter().find(|plane| plane.id == primary_id) else {
        return ENOENT;
    };
    let Some(fb) = device.framebuffers.iter().find(|fb| fb.id == fb_id) else {
        return ENOENT;
    };
    let (mut hdisplay, mut vdisplay) = io.mode_get_hv_timing(mode);

    if crtc.state.is_some() {
        let rotation = plane
            .state
            .as_ref()
            .expect("CRTC state requires primary plane state")
            .rotation;
        if io.rotation_90_or_270(rotation) {
            core::mem::swap(&mut hdisplay, &mut vdisplay);
        }
    }
    io.framebuffer_check_src_coords(
        x.wrapping_shl(16),
        y.wrapping_shl(16),
        hdisplay.wrapping_shl(16),
        vdisplay.wrapping_shl(16),
        fb,
    )
}

// upstream: drm_crtc.c drm_mode_setcrtc()
#[allow(unused_assignments)]
pub fn drm_mode_setcrtc<I: CrtcUapiIo>(
    io: &mut I,
    device: &mut DrmDevice,
    request: &ModeCrtc,
    file: &DrmFile,
) -> i32 {
    if !io.core_check_feature(device, DRIVER_MODESET) {
        return EOPNOTSUPP;
    }

    // Universal plane source offsets are 16.16 fixed point.
    if ((request.x as u32) & 0xffff_0000) != 0 || ((request.y as u32) & 0xffff_0000) != 0 {
        return ERANGE;
    }
    let Some(crtc_index) = io.find_crtc(device, file, request.crtc_id) else {
        io.debug(device.id, &format!("Unknown CRTC ID {}", request.crtc_id));
        return ENOENT;
    };
    let (crtc_id, primary_id, crtc_name) = {
        let crtc = &device.crtcs[crtc_index];
        (crtc.id, crtc.primary, crtc.name.clone())
    };
    io.debug(device.id, &format!("[CRTC:{}:{}]", crtc_id, crtc_name));
    let Some(plane_id) = primary_id else {
        return ENOENT;
    };

    // Allow disabling with the primary plane leased.
    if request.mode_valid && !io.lease_held(file, plane_id) {
        return EACCES;
    }

    let mut ctx = ModesetAcquireCtx::default();
    io.modeset_acquire_init(&mut ctx);
    let mut ret = io.lock_all(device.id, &mut ctx);
    let mut connector_set: Option<Vec<DrmId>> = None;
    let mut framebuffer: Option<DrmId> = None;
    let mut mode: Option<DisplayMode> = None;
    let mut num_connectors = 0usize;

    if ret == 0 {
        'setcrtc: {
            if request.mode_valid {
                // A mode requires a framebuffer.  -1 reuses the currently bound FB.
                if request.fb_id == -1 {
                    let current_fb = if let Some(index) = plane_index(device, plane_id) {
                        let plane = &device.planes[index];
                        if let Some(state) = plane.state.as_ref() {
                            state.fb
                        } else {
                            plane.fb
                        }
                    } else {
                        None
                    };
                    let Some(old_fb) = current_fb else {
                        io.debug(device.id, "CRTC does not have current framebuffer");
                        ret = EINVAL;
                        break 'setcrtc;
                    };
                    framebuffer = Some(old_fb);
                    // Keep reference accounting symmetric with the lookup path.
                    io.framebuffer_get(device, old_fb);
                } else {
                    let Some(fb) = io.framebuffer_lookup(device, file, request.fb_id as DrmId)
                    else {
                        io.debug(device.id, &format!("Unknown FB ID{}", request.fb_id));
                        ret = ENOENT;
                        break 'setcrtc;
                    };
                    framebuffer = Some(fb);
                }

                let Some(mut created_mode) = io.mode_create(device.id) else {
                    ret = ENOMEM;
                    break 'setcrtc;
                };
                if !file.aspect_ratio_allowed
                    && (request.mode.flags & DRM_MODE_FLAG_PIC_AR_MASK) != DRM_MODE_FLAG_PIC_AR_NONE
                {
                    io.debug(device.id, "Unexpected aspect-ratio flag bits");
                    ret = EINVAL;
                    mode = Some(created_mode);
                    break 'setcrtc;
                }

                ret = io.mode_convert_umode(device.id, &mut created_mode, &request.mode);
                if ret != 0 {
                    let _status_name = io.mode_status_name(created_mode.status);
                    io.debug(
                        device.id,
                        &format!(
                            "Invalid mode ({}, errno {}): {}",
                            _status_name, ret, created_mode.name
                        ),
                    );
                    mode = Some(created_mode);
                    break 'setcrtc;
                }
                mode = Some(created_mode);

                // Legacy drivers without universal planes get the core's
                // placeholder format list; those format checks are skipped.
                let format_default = plane_index(device, plane_id)
                    .map(|index| device.planes[index].format_default)
                    .unwrap_or(false);
                if !format_default {
                    let Some(fb_index) = framebuffer.and_then(|fb| framebuffer_index(device, fb))
                    else {
                        ret = ENOENT;
                        break 'setcrtc;
                    };
                    let fb = &device.framebuffers[fb_index];
                    if !io.plane_has_format(plane_id, fb.format, fb.modifier) {
                        io.debug(
                            device.id,
                            &format!(
                                "Invalid pixel format {:#010x}, modifier {:#x}",
                                fb.format, fb.modifier
                            ),
                        );
                        ret = EINVAL;
                        break 'setcrtc;
                    }
                }

                let Some(fb_id) = framebuffer else {
                    ret = EINVAL;
                    break 'setcrtc;
                };
                ret = drm_crtc_check_viewport(
                    io,
                    device,
                    crtc_id,
                    request.x,
                    request.y,
                    mode.as_ref().expect("mode was created"),
                    fb_id,
                );
                if ret != 0 {
                    break 'setcrtc;
                }
            }

            if request.count_connectors == 0 && mode.is_some() {
                io.debug(device.id, "Count connectors is 0 but mode set");
                ret = EINVAL;
                break 'setcrtc;
            }
            if request.count_connectors > 0 && (mode.is_none() || framebuffer.is_none()) {
                io.debug(
                    device.id,
                    &format!(
                        "Count connectors is {} but no mode or framebuffer set",
                        request.count_connectors
                    ),
                );
                ret = EINVAL;
                break 'setcrtc;
            }

            if request.count_connectors > 0 {
                // Avoid unbounded kernel memory allocation.
                if request.count_connectors > device.mode_config.num_connector {
                    ret = EINVAL;
                    break 'setcrtc;
                }
                let mut connectors = Vec::new();
                if connectors
                    .try_reserve_exact(request.count_connectors)
                    .is_err()
                {
                    ret = ENOMEM;
                    break 'setcrtc;
                }
                connector_set = Some(connectors);

                for index in 0..request.count_connectors {
                    let out_id = match io.get_user_u32(request.set_connectors_ptr, index) {
                        Ok(out_id) => out_id,
                        Err(_) => {
                            ret = EFAULT;
                            break 'setcrtc;
                        }
                    };
                    let Some(connector) = io.connector_lookup(device, file, out_id) else {
                        io.debug(device.id, &format!("Connector id {} unknown", out_id));
                        ret = ENOENT;
                        break 'setcrtc;
                    };
                    let connector_name = device
                        .connectors
                        .iter()
                        .find(|candidate| candidate.id == connector)
                        .map(|candidate| candidate.name.as_str())
                        .unwrap_or("");
                    io.debug(
                        device.id,
                        &format!("[CONNECTOR:{}:{}]", connector, connector_name),
                    );
                    connector_set
                        .as_mut()
                        .expect("connector set allocated")
                        .push(connector);
                    num_connectors += 1;
                }
            }

            let set = ModeSet {
                crtc: crtc_id,
                x: request.x,
                y: request.y,
                mode: mode.clone(),
                connectors: connector_set.clone().unwrap_or_default(),
                fb: framebuffer,
            };
            if io.uses_atomic_modeset(device) {
                ret = io.crtc_set_config(&set, Some(&mut ctx));
            } else {
                ret = __drm_mode_set_config_internal(io, device, &set, Some(&mut ctx));
            }
        }
    }

    if let Some(fb) = framebuffer {
        io.framebuffer_put(device, fb);
    }
    if let Some(connectors) = connector_set.as_ref() {
        for connector in connectors.iter().take(num_connectors) {
            io.connector_put(device, *connector);
        }
    }
    drop(connector_set);
    io.mode_destroy(device.id, mode);

    // Reset lookup-owned locals before the lock-all end path, matching the C
    // cleanup label's retry-safe state.
    framebuffer = None;
    mode = None;
    num_connectors = 0;
    io.unlock_all(device.id, &mut ctx, ret);
    ret
}

// upstream: drm_crtc.c drm_mode_crtc_set_obj_prop()
pub fn drm_mode_crtc_set_obj_prop<I: CrtcUapiIo>(
    io: &mut I,
    device: &mut DrmDevice,
    object: &ModeObject,
    property: &DrmProperty,
    value: u64,
) -> i32 {
    let mut ret = EINVAL;
    let Some(index) = crtc_index(device, object.id) else {
        return ret;
    };
    let crtc = &device.crtcs[index];
    if crtc.funcs.set_property {
        ret = io.crtc_set_property(crtc.id, property.id, value);
    }
    if ret == 0 {
        io.property_set_value(object.id, property.id, value);
    }
    ret
}

// upstream: drm_crtc.c drm_crtc_create_scaling_filter_property()
pub fn drm_crtc_create_scaling_filter_property<I: CrtcUapiIo>(
    io: &mut I,
    device: &mut DrmDevice,
    crtc_id: DrmId,
    supported_filters: u32,
) -> i32 {
    let Some(index) = crtc_index(device, crtc_id) else {
        return ENOENT;
    };
    let property = match io.scaling_filter_property_create(device.id, supported_filters) {
        Ok(property) => property,
        Err(ret) => return ret,
    };
    io.property_attach(crtc_id, property, DRM_SCALING_FILTER_DEFAULT);
    device.crtcs[index].scaling_filter_property = Some(property);
    0
}

// upstream: drm_crtc.c drm_crtc_create_sharpness_strength_property()
pub fn drm_crtc_create_sharpness_strength_property<I: CrtcUapiIo>(
    io: &mut I,
    device: &mut DrmDevice,
    crtc_id: DrmId,
) -> i32 {
    let Some(property) = io.range_property_create(
        device.id,
        CrtcPropertySpec {
            name: "SHARPNESS_STRENGTH",
            flags: 0,
            min: 0,
            max: 255,
        },
    ) else {
        return ENOMEM;
    };
    let Some(index) = crtc_index(device, crtc_id) else {
        return ENOENT;
    };
    device.crtcs[index].sharpness_strength_property = Some(property);
    io.property_attach(crtc_id, property, 0);
    0
}

// upstream: drm_crtc.c drm_crtc_in_clone_mode()
pub fn drm_crtc_in_clone_mode(crtc_state: Option<&CrtcState>) -> bool {
    let Some(crtc_state) = crtc_state else {
        return false;
    };
    crtc_state.encoder_mask.count_ones() > 1
}
