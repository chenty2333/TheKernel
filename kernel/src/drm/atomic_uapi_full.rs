// SPDX-License-Identifier: MIT
/*
 * Copyright (C) 2014 Red Hat
 * Copyright (C) 2014 Intel Corp.
 * Copyright (C) 2018 Intel Corp.
 * Copyright (c) 2020, The Linux Foundation. All rights reserved.
 *
 * Permission is hereby granted, free of charge, to any person obtaining a
 * copy of this software and associated documentation files (the "Software"),
 * to deal in the Software without restriction, including without limitation
 * the rights to use, copy, modify, merge, publish, distribute, sublicense,
 * and/or sell copies of the Software, and to permit persons to whom the
 * Software is furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
 * THE COPYRIGHT HOLDER(S) OR AUTHOR(S) BE LIABLE FOR ANY CLAIM, DAMAGES OR
 * OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE,
 * ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
 * DEALINGS IN THE SOFTWARE.
 *
 * Authors:
 * Rob Clark <robdclark@gmail.com>
 * Daniel Vetter <daniel.vetter@ffwll.ch>
 */

//! Linux DRM atomic UAPI property marshalling and ioctl logic.
//!
//! DRM core object lookup, locking, userspace access, references, blobs,
//! framebuffers, fences, events, and commit execution are explicit `AtomicUapiIo`
//! hooks. The property-to-state mapping and validation order remain here.

extern crate alloc;
use alloc::{collections::BTreeMap, format, string::String, vec::Vec};

pub type DeviceId = u64;
pub type ObjectId = u32;
pub type PropertyId = u32;
pub type BlobId = u32;
pub type FramebufferId = u32;
pub type FileId = u64;
pub type UserPtr = u64;
pub type FenceId = u64;
pub type SyncFileId = u64;
pub type AcquireContext = u64;
pub type KResult<T> = Result<T, i32>;

pub const EACCES: i32 = -13;
pub const EDEADLK: i32 = -35;
pub const EFAULT: i32 = -14;
pub const EINVAL: i32 = -22;
pub const ENOENT: i32 = -2;
pub const ENOMEM: i32 = -12;
pub const EOPNOTSUPP: i32 = -95;
pub const DRM_LINK_STATUS_GOOD: u64 = 0;
pub const DRM_MODE_CONTENT_PROTECTION_ENABLED: u64 = 1;
pub const DRM_MODE_DPMS_ON: i32 = 0;
pub const DRM_MODE_DPMS_OFF: i32 = 3;
pub const DRM_EVENT_FLIP_COMPLETE: u32 = 0x02;
pub const DRM_EVENT_VBLANK_SIZE: u32 = 32;
pub const DRM_MODE_ATOMIC_TEST_ONLY: u32 = 1 << 0;
pub const DRM_MODE_ATOMIC_NONBLOCK: u32 = 1 << 1;
pub const DRM_MODE_ATOMIC_ALLOW_MODESET: u32 = 1 << 2;
pub const DRM_MODE_PAGE_FLIP_EVENT: u32 = 1 << 3;
pub const DRM_MODE_PAGE_FLIP_ASYNC: u32 = 1 << 4;
pub const DRM_MODE_ATOMIC_FLAGS: u32 = DRM_MODE_ATOMIC_TEST_ONLY
    | DRM_MODE_ATOMIC_NONBLOCK
    | DRM_MODE_ATOMIC_ALLOW_MODESET
    | DRM_MODE_PAGE_FLIP_EVENT
    | DRM_MODE_PAGE_FLIP_ASYNC;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    Connector,
    Crtc,
    Plane(PlaneKind),
    Colorop,
    Other(u32),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaneKind {
    Primary,
    Cursor,
    Other(u32),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColoropKind {
    Lut1d,
    Ctm3x4,
    Lut3d,
    Other(u32),
}

/// Standard and object-specific properties recognized by the DRM atomic UAPI.
/// Driver-private properties are passed to the object's atomic property hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyKey {
    Active, ModeId, VrrEnabled, DegammaLut, DegammaLutSize, Ctm, GammaLut, GammaLutSize, BackgroundColor,
    OutFencePtr, SharpnessStrength, FbId, InFenceFd, CrtcId, CrtcX, CrtcY, CrtcW, CrtcH,
    SrcX, SrcY, SrcW, SrcH, Alpha, BlendMode, Rotation, Zpos,
    ColorEncoding, ColorRange, ColorPipeline, FbDamageClips, ScalingFilter,
    HotspotX, HotspotY, ColoropType, Bypass, Lut1dInterpolation,
    Curve1dType, Multiplier, Lut3dInterpolation, Size, Data,
    Dpms, TvSelectSubconnector, TvSubconnector, TvLeftMargin, TvRightMargin,
    TvTopMargin, TvBottomMargin, LegacyTvMode, TvMode, TvBrightness,
    TvContrast, TvFlickerReduction, TvOverscan, TvSaturation, TvHue,
    LinkStatus, HdrOutputMetadata, AspectRatio, ContentType, ScalingMode,
    ContentProtection, HdcpContentType, WritebackFbId, WritebackOutFencePtr,
    MaxBpc, PrivacyScreenSwState, BroadcastRgb, Colorspace, Driver(u32),
}
#[derive(Clone, Debug)]
pub struct PropertyRef {
    pub id: PropertyId,
    pub name: String,
    pub key: PropertyKey,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayMode {
    pub name: String,
    pub status: i32,
    pub opaque: Vec<u8>,
}
#[derive(Clone, Debug, Default)]
pub struct TvMargins {
    pub left: u64, pub right: u64, pub top: u64, pub bottom: u64,
}
#[derive(Clone, Debug, Default)]
pub struct TvState {
    pub select_subconnector: u64, pub subconnector: u64, pub margins: TvMargins,
    pub legacy_mode: u64, pub mode: u64, pub brightness: u64, pub contrast: u64,
    pub flicker_reduction: u64, pub overscan: u64, pub saturation: u64, pub hue: u64,
}
#[derive(Clone, Debug, Default)]
pub struct HdmiState { pub broadcast_rgb: u64 }
#[derive(Clone, Debug, Default)]
pub struct WritebackJob { pub out_fence: Option<FenceId> }
#[derive(Clone, Debug, Default)]
pub struct CrtcState {
    pub crtc: ObjectId, pub mode: Option<DisplayMode>, pub mode_blob: Option<BlobId>,
    pub enable: bool, pub active: bool, pub vrr_enabled: u64,
    pub degamma_lut: Option<BlobId>, pub ctm: Option<BlobId>,
    pub gamma_lut: Option<BlobId>, pub background_color: u64,
    pub scaling_filter: u64, pub sharpness_strength: u64,
    pub color_mgmt_changed: bool, pub plane_mask: u64, pub connector_mask: u64,
    pub event: Option<PendingVblankEvent>, pub async_flip: bool,
}
#[derive(Clone, Debug, Default)]
pub struct PlaneState {
    pub plane: ObjectId, pub crtc: Option<ObjectId>, pub fb: Option<FramebufferId>,
    pub fence: Option<FenceId>, pub crtc_x: i64, pub crtc_y: i64,
    pub crtc_w: u64, pub crtc_h: u64, pub src_x: u64, pub src_y: u64,
    pub src_w: u64, pub src_h: u64, pub alpha: u64, pub pixel_blend_mode: u64,
    pub rotation: u64, pub zpos: u64, pub color_encoding: u64, pub color_range: u64,
    pub color_pipeline: Option<ObjectId>, pub fb_damage_clips: Option<BlobId>,
    pub scaling_filter: u64, pub hotspot_x: u64, pub hotspot_y: u64,
    pub color_mgmt_changed: bool,
}
#[derive(Clone, Debug, Default)]
pub struct ConnectorState {
    pub connector: ObjectId, pub crtc: Option<ObjectId>, pub tv: TvState,
    pub link_status: u64, pub hdr_output_metadata: Option<BlobId>,
    pub picture_aspect_ratio: u64, pub content_type: u64, pub scaling_mode: u64,
    pub content_protection: u64, pub hdcp_content_type: u64, pub colorspace: u64,
    pub max_requested_bpc: u64, pub privacy_screen_sw_state: u64,
    pub hdmi: HdmiState, pub writeback_job: Option<WritebackJob>,
}
#[derive(Clone, Debug, Default)]
pub struct ColoropState {
    pub colorop: ObjectId, pub bypass: u64, pub lut1d_interpolation: u64,
    pub curve_1d_type: u64, pub multiplier: u64, pub lut3d_interpolation: u64,
    pub data: Option<BlobId>,
}
#[derive(Clone, Debug, Default)]
pub struct PendingVblankEvent {
    pub id: u64, pub event_type: u32, pub length: u32, pub crtc: ObjectId, pub user_data: u64,
    pub fence: Option<FenceId>, pub file_priv: Option<FileId>,
}
#[derive(Clone, Debug, Default)]
pub struct AtomicCommit {
    pub dev: DeviceId, pub acquire_ctx: Option<AcquireContext>,
    pub allow_modeset: bool, pub plane_color_pipeline: bool,
    pub crtcs: BTreeMap<ObjectId, CrtcState>, pub new_crtc_order: Vec<ObjectId>,
    pub planes: BTreeMap<ObjectId, PlaneState>, pub connectors: BTreeMap<ObjectId, ConnectorState>,
    pub new_connector_order: Vec<ObjectId>, pub colorops: BTreeMap<ObjectId, ColoropState>,
    pub crtc_out_fence_ptrs: BTreeMap<ObjectId, Option<UserPtr>>,
    pub connector_out_fence_ptrs: BTreeMap<ObjectId, Option<UserPtr>>,
}
#[derive(Clone, Debug)]
pub struct AtomicModeArgs {
    pub flags: u32, pub reserved: u32, pub user_data: u64,
    pub count_objs: u32, pub objs_ptr: UserPtr, pub count_props_ptr: UserPtr,
    pub props_ptr: UserPtr, pub prop_values_ptr: UserPtr,
}
#[derive(Clone, Debug, Default)]
pub struct OutFenceState {
    pub out_fence_ptr: Option<UserPtr>, pub sync_file: Option<SyncFileId>, pub fd: i32,
}
#[derive(Clone, Copy, Debug)]
pub struct BlobConstraints {
    pub expected_size: Option<usize>, pub num_elem: Option<usize>, pub elem_size: Option<usize>,
}
#[derive(Clone, Copy, Debug)]
pub struct ObjectRef { pub id: ObjectId, pub kind: ObjectKind }

/// Boundary to DRM core facilities. Implementations own object references,
/// locks, state acquisition, user memory, handle/blob/fence lifetimes, and the
/// final commit. Property dispatch and state-field mutation stay in this file.
pub trait AtomicUapiIo {
    fn object_info(&self, object: ObjectId) -> Option<(DeviceId, ObjectKind, String)>;
    fn object_has_properties(&self, object: ObjectId) -> bool;
    fn property_lookup(&self, object: ObjectId, property: PropertyId) -> Option<PropertyRef>;
    fn property_change_valid_get(&mut self, property: PropertyId, value: u64) -> Option<u64>;
    fn property_change_valid_put(&mut self, property: PropertyId, reference: u64);
    fn file_atomic_enabled(&self, file: FileId) -> bool;
    fn file_plane_color_pipeline(&self, file: FileId) -> bool;
    fn async_page_flip_supported(&self, dev: DeviceId) -> bool;
    fn object_find(&mut self, dev: DeviceId, file: FileId, id: ObjectId) -> Option<ObjectRef>;
    fn object_put(&mut self, object: ObjectRef);
    fn ensure_crtc_state(&mut self, commit: &mut AtomicCommit, crtc: ObjectId) -> KResult<()>;
    fn ensure_plane_state(&mut self, commit: &mut AtomicCommit, plane: ObjectId) -> KResult<()>;
    fn ensure_connector_state(&mut self, commit: &mut AtomicCommit, connector: ObjectId) -> KResult<()>;
    fn ensure_colorop_state(&mut self, commit: &mut AtomicCommit, colorop: ObjectId) -> KResult<()>;
    fn current_crtc_state(&mut self, crtc: ObjectId) -> KResult<CrtcState>;
    fn current_plane_state(&mut self, plane: ObjectId) -> KResult<PlaneState>;
    fn current_connector_state(&mut self, connector: ObjectId) -> KResult<ConnectorState>;
    fn current_colorop_state(&mut self, colorop: ObjectId) -> KResult<ColoropState>;
    fn crtc_self_refresh_active(&self, crtc: ObjectId) -> bool;
    fn colorop_plane(&self, colorop: ObjectId) -> Option<ObjectId>;
    fn crtc_id_from_user(&mut self, dev: DeviceId, file: FileId, id: u64) -> Option<ObjectId>;
    fn colorop_id_from_user(&mut self, dev: DeviceId, file: FileId, id: u64) -> Option<ObjectId>;
    fn plane_kind(&self, plane: ObjectId) -> PlaneKind;
    fn colorop_kind_size(&self, colorop: ObjectId) -> (ColoropKind, usize);
    fn color_lut_element_size(&self) -> usize;
    fn color_lut32_element_size(&self) -> usize;
    fn color_ctm_element_size(&self) -> usize;
    fn color_ctm3x4_element_size(&self) -> usize;
    fn mode_rect_element_size(&self) -> usize;
    fn hdr_output_metadata_element_size(&self) -> usize;
    fn framebuffer_put(&mut self, fb: FramebufferId);
    fn plane_async_check(&mut self, plane: ObjectId, commit: &mut AtomicCommit, active: bool) -> Option<KResult<()>>;
    fn plane_mask(&self, plane: ObjectId) -> u64;
    fn connector_mask(&self, connector: ObjectId) -> u64;
    fn debug(&mut self, category: &'static str, message: &str);
    fn blob_put(&mut self, blob: BlobId);
    fn blob_get(&mut self, blob: BlobId) -> Option<BlobId>;
    fn blob_lookup(&mut self, dev: DeviceId, id: u64) -> Option<BlobId>;
    fn blob_bytes(&mut self, blob: BlobId) -> Option<Vec<u8>>;
    fn replace_blob_from_id(
        &mut self, dev: DeviceId, slot: &mut Option<BlobId>, id: u64,
        constraints: BlobConstraints,
    ) -> KResult<bool>;
    fn mode_info_size(&self) -> usize;
    fn mode_to_uapi(&mut self, mode: &DisplayMode) -> Vec<u8>;
    fn mode_from_uapi(&mut self, dev: DeviceId, bytes: &[u8]) -> KResult<DisplayMode>;
    fn create_mode_blob(&mut self, dev: DeviceId, bytes: &[u8]) -> KResult<BlobId>;
    fn crtc_effectively_active(&self, state: &CrtcState) -> bool;
    fn immutable_property_value(&mut self, object: ObjectId, property: PropertyKey) -> KResult<u64>;
    fn driver_set_property(&mut self, commit: &mut AtomicCommit, object: ObjectId, state: &mut PropertyStateMut<'_>, property: &PropertyRef, value: u64) -> Option<KResult<()>>;
    fn driver_get_property(&mut self, object: ObjectId, state: &PropertyStateRef<'_>, property: &PropertyRef) -> Option<KResult<u64>>;
    fn framebuffer_lookup(&mut self, dev: DeviceId, file: FileId, id: u64) -> Option<FramebufferId>;
    fn framebuffer_assign(&mut self, slot: &mut Option<FramebufferId>, fb: Option<FramebufferId>);
    fn writeback_set_fb(&mut self, state: &mut ConnectorState, fb: Option<FramebufferId>) -> KResult<()>;
    fn sync_file_get_fence(&mut self, fd: u64) -> Option<FenceId>;
    fn user_put_i32(&mut self, pointer: UserPtr, value: i32) -> KResult<()>;
    fn connector_get(&mut self, connector: ObjectId);
    fn connector_put(&mut self, connector: ObjectId);
    fn connection_mutex_lock(&mut self, ctx: AcquireContext) -> KResult<()>;
    fn connector_dpms(&self, connector: ObjectId) -> i32;
    fn set_connector_dpms(&mut self, connector: ObjectId, mode: i32);
    fn add_affected_connectors(&mut self, commit: &mut AtomicCommit, crtc: ObjectId) -> KResult<()>;
    fn atomic_commit(&mut self, commit: &mut AtomicCommit) -> KResult<()>;
    fn object_lock_is_held(&mut self, object: ObjectId) -> bool;
    fn connection_lock_is_held(&mut self) -> bool;
    fn allocate_event(&mut self) -> Option<PendingVblankEvent>;
    fn event_free_unreserved(&mut self, event: PendingVblankEvent);
    fn event_reserve(&mut self, dev: DeviceId, file: FileId, event: &mut PendingVblankEvent) -> KResult<()>;
    fn event_cancel_free(&mut self, dev: DeviceId, event: PendingVblankEvent);
    fn crtc_create_fence(&mut self, crtc: ObjectId) -> Option<FenceId>;
    fn writeback_out_fence(&mut self, connector: ObjectId) -> Option<FenceId>;
    fn get_unused_fd_cloexec(&mut self) -> i32;
    fn sync_file_create(&mut self, fence: FenceId) -> Option<SyncFileId>;
    fn sync_file_install(&mut self, fd: i32, sync_file: SyncFileId);
    fn sync_file_put(&mut self, sync_file: SyncFileId);
    fn put_unused_fd(&mut self, fd: i32);
    fn dma_fence_put(&mut self, fence: FenceId);
    fn new_commit(&mut self, dev: DeviceId) -> Option<AtomicCommit>;
    fn modeset_acquire_init(&mut self) -> AcquireContext;
    fn atomic_commit_clear(&mut self, commit: &mut AtomicCommit);
    fn atomic_commit_put(&mut self, commit: AtomicCommit);
    fn modeset_backoff(&mut self, ctx: AcquireContext) -> KResult<()>;
    fn modeset_drop_locks(&mut self, ctx: AcquireContext);
    fn modeset_acquire_fini(&mut self, ctx: AcquireContext);
    fn read_user_u32(&mut self, pointer: UserPtr, index: usize) -> KResult<u32>;
    fn read_user_u64(&mut self, pointer: UserPtr, index: usize) -> KResult<u64>;
    fn core_has_atomic(&self, dev: DeviceId) -> bool;
    fn atomic_check_only(&mut self, commit: &mut AtomicCommit) -> KResult<()>;
    fn atomic_nonblocking_commit(&mut self, commit: &mut AtomicCommit) -> KResult<()>;
}

/// Borrowed state variant for driver property callbacks.
pub enum PropertyStateRef<'a> {
    Crtc(&'a CrtcState), Plane(&'a PlaneState), Connector(&'a ConnectorState), Colorop(&'a ColoropState),
}
pub enum PropertyStateMut<'a> {
    Crtc(&'a mut CrtcState), Plane(&'a mut PlaneState), Connector(&'a mut ConnectorState), Colorop(&'a mut ColoropState),
}

fn object_device<I: AtomicUapiIo>(io: &I, object: ObjectId) -> DeviceId {
    io.object_info(object).map(|x| x.0).unwrap_or_default()
}
fn blob_constraints(expected_size: Option<usize>, num_elem: Option<usize>, elem_size: Option<usize>) -> BlobConstraints {
    BlobConstraints { expected_size, num_elem, elem_size }
}

// upstream: drm_atomic_uapi.c drm_atomic_set_mode_for_crtc()
pub fn drm_atomic_set_mode_for_crtc<I: AtomicUapiIo>(io: &mut I, state: &mut CrtcState, mode: Option<&DisplayMode>) -> KResult<()> {
    let crtc = state.crtc;
    let dev = object_device(io, crtc);
    if let Some(mode) = mode {
        if state.mode.as_ref() == Some(mode) { return Ok(()); }
    }
    if let Some(old) = state.mode_blob.take() { io.blob_put(old); }
    if let Some(mode) = mode {
        let bytes = io.mode_to_uapi(mode);
        let blob = io.create_mode_blob(dev, &bytes)?;
        state.mode = Some(mode.clone());
        state.mode_blob = Some(blob);
        state.enable = true;
        io.debug("atomic", &format!("Set [MODE:{}] for CRTC {crtc} state", mode.name));
    } else {
        state.mode = None;
        state.enable = false;
        io.debug("atomic", &format!("Set [NOMODE] for CRTC {crtc} state"));
    }
    Ok(())
}

// upstream: drm_atomic_uapi.c drm_atomic_set_mode_prop_for_crtc()
pub fn drm_atomic_set_mode_prop_for_crtc<I: AtomicUapiIo>(io: &mut I, state: &mut CrtcState, blob: Option<BlobId>) -> KResult<()> {
    let crtc = state.crtc;
    let dev = object_device(io, crtc);
    if blob == state.mode_blob { return Ok(()); }
    if let Some(old) = state.mode_blob.take() { io.blob_put(old); }
    state.mode = None;
    if let Some(blob) = blob {
        let bytes = io.blob_bytes(blob).ok_or(EINVAL)?;
        if bytes.len() != io.mode_info_size() {
            io.debug("atomic", &format!("[CRTC:{crtc}] bad mode blob length: {}", bytes.len()));
            return Err(EINVAL);
        }
        let mode = match io.mode_from_uapi(dev, &bytes) {
            Ok(mode) => mode,
            Err(err) => {
                io.debug("atomic", &format!("[CRTC:{crtc}] invalid mode: {err}"));
                return Err(EINVAL);
            }
        };
        let blob = io.blob_get(blob).ok_or(EINVAL)?;
        state.mode = Some(mode.clone());
        state.mode_blob = Some(blob);
        state.enable = true;
        io.debug("atomic", &format!("Set [MODE:{}] for CRTC {crtc} state", mode.name));
    } else {
        state.enable = false;
        io.debug("atomic", &format!("Set [NOMODE] for CRTC {crtc} state"));
    }
    Ok(())
}

// upstream: drm_atomic_uapi.c drm_atomic_set_crtc_for_plane()
pub fn drm_atomic_set_crtc_for_plane<I: AtomicUapiIo>(io: &mut I, commit: &mut AtomicCommit, state: &mut PlaneState, crtc: Option<ObjectId>) -> KResult<()> {
    let plane = state.plane;
    if state.crtc == crtc { return Ok(()); }
    if let Some(old_crtc) = state.crtc {
        io.ensure_crtc_state(commit, old_crtc)?;
        if let Some(crtc_state) = commit.crtcs.get_mut(&old_crtc) {
            crtc_state.plane_mask &= !io.plane_mask(plane);
        }
    }
    state.crtc = crtc;
    if let Some(crtc) = crtc {
        io.ensure_crtc_state(commit, crtc)?;
        let crtc_state = commit.crtcs.get_mut(&crtc).ok_or(EINVAL)?;
        crtc_state.plane_mask |= io.plane_mask(plane);
        io.debug("atomic", &format!("Link PLANE:{plane} state to CRTC:{crtc}"));
    } else {
        io.debug("atomic", &format!("Link PLANE:{plane} state to NOCRTC"));
    }
    Ok(())
}

// upstream: drm_atomic_uapi.c drm_atomic_set_fb_for_plane()
pub fn drm_atomic_set_fb_for_plane<I: AtomicUapiIo>(io: &mut I, state: &mut PlaneState, fb: Option<FramebufferId>) {
    let plane = state.plane;
    if let Some(fb) = fb { io.debug("atomic", &format!("Set FB:{fb} for PLANE:{plane} state")); }
    else { io.debug("atomic", &format!("Set NOFB for PLANE:{plane} state")); }
    io.framebuffer_assign(&mut state.fb, fb);
}

// upstream: drm_atomic_uapi.c drm_atomic_set_colorop_for_plane()
pub fn drm_atomic_set_colorop_for_plane<I: AtomicUapiIo>(io: &mut I, state: &mut PlaneState, colorop: Option<ObjectId>) -> bool {
    let plane = state.plane;
    if state.color_pipeline == colorop { return false; }
    if let Some(colorop) = colorop { io.debug("atomic", &format!("Set COLOROP:{colorop} for PLANE:{plane} state")); }
    else { io.debug("atomic", &format!("Set NOCOLOROP for PLANE:{plane} state")); }
    state.color_pipeline = colorop;
    true
}

// upstream: drm_atomic_uapi.c drm_atomic_set_crtc_for_connector()
pub fn drm_atomic_set_crtc_for_connector<I: AtomicUapiIo>(io: &mut I, commit: &mut AtomicCommit, state: &mut ConnectorState, crtc: Option<ObjectId>) -> KResult<()> {
    let connector = state.connector;
    if state.crtc == crtc { return Ok(()); }
    if let Some(old_crtc) = state.crtc {
        if let Some(crtc_state) = commit.crtcs.get_mut(&old_crtc) {
            crtc_state.connector_mask &= !io.connector_mask(connector);
        }
        io.connector_put(connector);
        state.crtc = None;
    }
    if let Some(crtc) = crtc {
        io.ensure_crtc_state(commit, crtc)?;
        let crtc_state = commit.crtcs.get_mut(&crtc).ok_or(EINVAL)?;
        crtc_state.connector_mask |= io.connector_mask(connector);
        io.connector_get(connector);
        state.crtc = Some(crtc);
        io.debug("atomic", &format!("Link CONNECTOR:{connector} state to CRTC:{crtc}"));
    } else {
        io.debug("atomic", &format!("Link CONNECTOR:{connector} state to NOCRTC"));
    }
    Ok(())
}

// upstream: drm_atomic_uapi.c set_out_fence_for_crtc()
pub fn set_out_fence_for_crtc(commit: &mut AtomicCommit, crtc: ObjectId, fence_ptr: Option<UserPtr>) {
    commit.crtc_out_fence_ptrs.insert(crtc, fence_ptr);
}

// upstream: drm_atomic_uapi.c get_out_fence_for_crtc()
pub fn get_out_fence_for_crtc(commit: &mut AtomicCommit, crtc: ObjectId) -> Option<UserPtr> {
    commit.crtc_out_fence_ptrs.insert(crtc, None).flatten()
}

// upstream: drm_atomic_uapi.c set_out_fence_for_connector()
pub fn set_out_fence_for_connector<I: AtomicUapiIo>(io: &mut I, commit: &mut AtomicCommit, connector: ObjectId, fence_ptr: Option<UserPtr>) -> KResult<()> {
    let Some(fence_ptr) = fence_ptr else { return Ok(()); };
    io.user_put_i32(fence_ptr, -1).map_err(|_| EFAULT)?;
    commit.connector_out_fence_ptrs.insert(connector, Some(fence_ptr));
    Ok(())
}

// upstream: drm_atomic_uapi.c get_out_fence_for_connector()
pub fn get_out_fence_for_connector(commit: &mut AtomicCommit, connector: ObjectId) -> Option<UserPtr> {
    commit.connector_out_fence_ptrs.insert(connector, None).flatten()
}

// upstream: drm_atomic_uapi.c drm_atomic_crtc_set_property()
pub fn drm_atomic_crtc_set_property<I: AtomicUapiIo>(io: &mut I, commit: &mut AtomicCommit, crtc: ObjectId, state: &mut CrtcState, property: &PropertyRef, val: u64) -> KResult<()> {
    let dev = object_device(io, crtc);
    let mut replaced = false;
    match property.key {
        PropertyKey::Active => state.active = val != 0,
        PropertyKey::ModeId => {
            let mode = io.blob_lookup(dev, val);
            let result = drm_atomic_set_mode_prop_for_crtc(io, state, mode);
            if let Some(mode) = mode { io.blob_put(mode); }
            return result;
        }
        PropertyKey::VrrEnabled => state.vrr_enabled = val,
        PropertyKey::DegammaLut => {
            let elem_size = io.color_lut_element_size();
            let lut_size = io.immutable_property_value(crtc, PropertyKey::DegammaLutSize)? as usize;
            let result = io.replace_blob_from_id(dev, &mut state.degamma_lut, val,
                blob_constraints(Some(elem_size.saturating_mul(lut_size)), None, Some(elem_size)));
            if let Ok(changed) = result { state.color_mgmt_changed |= changed; }
            return result.map(|_| ());
        }
        PropertyKey::Ctm => {
            let result = io.replace_blob_from_id(dev, &mut state.ctm, val,
                blob_constraints(None, Some(io.color_ctm_element_size()), None));
            if let Ok(changed) = result { state.color_mgmt_changed |= changed; }
            return result.map(|_| ());
        }
        PropertyKey::GammaLut => {
            let elem_size = io.color_lut_element_size();
            let lut_size = io.immutable_property_value(crtc, PropertyKey::GammaLutSize)? as usize;
            let result = io.replace_blob_from_id(dev, &mut state.gamma_lut, val,
                blob_constraints(Some(elem_size.saturating_mul(lut_size)), None, Some(elem_size)));
            if let Ok(changed) = result { state.color_mgmt_changed |= changed; }
            return result.map(|_| ());
        }
        PropertyKey::BackgroundColor => state.background_color = val,
        PropertyKey::OutFencePtr => {
            if val == 0 { return Ok(()); }
            let pointer = val;
            io.user_put_i32(pointer, -1).map_err(|_| EFAULT)?;
            set_out_fence_for_crtc(commit, crtc, Some(pointer));
        }
        PropertyKey::ScalingFilter => state.scaling_filter = val,
        PropertyKey::SharpnessStrength => state.sharpness_strength = val,
        PropertyKey::Driver(_) => {
            if let Some(result) = io.driver_set_property(commit, crtc, &mut PropertyStateMut::Crtc(state), property, val) { return result; }
            return unknown_property(io, "CRTC", crtc, property);
        }
        _ => {
            if let Some(result) = io.driver_set_property(commit, crtc, &mut PropertyStateMut::Crtc(state), property, val) { return result; }
            return unknown_property(io, "CRTC", crtc, property);
        }
    }
    let _ = &mut replaced;
    Ok(())
}

// upstream: drm_atomic_uapi.c drm_atomic_crtc_get_property()
pub fn drm_atomic_crtc_get_property<I: AtomicUapiIo>(io: &mut I, crtc: ObjectId, state: &CrtcState, property: &PropertyRef) -> KResult<u64> {
    match property.key {
        PropertyKey::Active => Ok(io.crtc_effectively_active(state) as u64),
        PropertyKey::ModeId => Ok(state.mode_blob.unwrap_or(0) as u64),
        PropertyKey::VrrEnabled => Ok(state.vrr_enabled),
        PropertyKey::DegammaLut => Ok(state.degamma_lut.unwrap_or(0) as u64),
        PropertyKey::Ctm => Ok(state.ctm.unwrap_or(0) as u64),
        PropertyKey::GammaLut => Ok(state.gamma_lut.unwrap_or(0) as u64),
        PropertyKey::BackgroundColor => Ok(state.background_color),
        PropertyKey::OutFencePtr => Ok(0),
        PropertyKey::ScalingFilter => Ok(state.scaling_filter),
        PropertyKey::SharpnessStrength => Ok(state.sharpness_strength),
        PropertyKey::Driver(_) => {
            if let Some(result) = io.driver_get_property(crtc, &PropertyStateRef::Crtc(state), property) { return result; }
            unknown_property(io, "CRTC", crtc, property)
        }
        _ => {
            if let Some(result) = io.driver_get_property(crtc, &PropertyStateRef::Crtc(state), property) { return result; }
            unknown_property(io, "CRTC", crtc, property)
        }
    }
}

// upstream: drm_atomic_uapi.c drm_atomic_plane_set_property()
pub fn drm_atomic_plane_set_property<I: AtomicUapiIo>(io: &mut I, commit: &mut AtomicCommit, plane: ObjectId, state: &mut PlaneState, file: FileId, property: &PropertyRef, val: u64) -> KResult<()> {
    let dev = object_device(io, plane);
    let mut replaced = false;
    match property.key {
        PropertyKey::FbId => {
            let fb = io.framebuffer_lookup(dev, file, val);
            drm_atomic_set_fb_for_plane(io, state, fb);
            if let Some(fb) = fb { io.framebuffer_put(fb); }
        }
        PropertyKey::InFenceFd => {
            if state.fence.is_some() { return Err(EINVAL); }
            if val as i64 == -1 { return Ok(()); }
            state.fence = io.sync_file_get_fence(val).or_else(|| None);
            if state.fence.is_none() { return Err(EINVAL); }
        }
        PropertyKey::CrtcId => {
            let crtc = io.crtc_id_from_user(dev, file, val);
            if val != 0 && crtc.is_none() {
                io.debug("atomic", &format!("[PROP:{}:{}] cannot find CRTC with ID {val}", property.id, property.name));
                return Err(EACCES);
            }
            return drm_atomic_set_crtc_for_plane(io, commit, state, crtc);
        }
        PropertyKey::CrtcX => state.crtc_x = val as i64,
        PropertyKey::CrtcY => state.crtc_y = val as i64,
        PropertyKey::CrtcW => state.crtc_w = val,
        PropertyKey::CrtcH => state.crtc_h = val,
        PropertyKey::SrcX => state.src_x = val,
        PropertyKey::SrcY => state.src_y = val,
        PropertyKey::SrcW => state.src_w = val,
        PropertyKey::SrcH => state.src_h = val,
        PropertyKey::Alpha => state.alpha = val,
        PropertyKey::BlendMode => state.pixel_blend_mode = val,
        PropertyKey::Rotation => {
            let rotation_mask = val & 0x0f;
            if rotation_mask == 0 || !rotation_mask.is_power_of_two() {
                io.debug("atomic", &format!("[PLANE:{plane}] bad rotation bitmask: {val:#x}"));
                return Err(EINVAL);
            }
            state.rotation = val;
        }
        PropertyKey::Zpos => state.zpos = val,
        PropertyKey::ColorEncoding => state.color_encoding = val,
        PropertyKey::ColorRange => state.color_range = val,
        PropertyKey::ColorPipeline => {
            let colorop = io.colorop_id_from_user(dev, file, val);
            if val != 0 && colorop.is_none() { return Err(EACCES); }
            state.color_mgmt_changed |= drm_atomic_set_colorop_for_plane(io, state, colorop);
        }
        PropertyKey::FbDamageClips => return io.replace_blob_from_id(dev, &mut state.fb_damage_clips, val,
            blob_constraints(None, None, Some(io.mode_rect_element_size()))).map(|_| ()),
        PropertyKey::ScalingFilter => state.scaling_filter = val,
        PropertyKey::HotspotX | PropertyKey::HotspotY => {
            if let Some(result) = io.driver_set_property(commit, plane, &mut PropertyStateMut::Plane(state), property, val) { return result; }
            if io.plane_kind(plane) != PlaneKind::Cursor {
                io.debug("atomic", &format!("[PLANE:{plane}] is not a cursor plane: {val:#x}"));
                return Err(EINVAL);
            }
            if property.key == PropertyKey::HotspotX { state.hotspot_x = val; }
            else { state.hotspot_y = val; }
        }
        PropertyKey::Driver(_) => {
            if let Some(result) = io.driver_set_property(commit, plane, &mut PropertyStateMut::Plane(state), property, val) { return result; }
            return unknown_property(io, "PLANE", plane, property);
        }
        _ => {
            if let Some(result) = io.driver_set_property(commit, plane, &mut PropertyStateMut::Plane(state), property, val) { return result; }
            return unknown_property(io, "PLANE", plane, property);
        }
    }
    let _ = &mut replaced;
    Ok(())
}

// upstream: drm_atomic_uapi.c drm_atomic_plane_get_property()
pub fn drm_atomic_plane_get_property<I: AtomicUapiIo>(io: &mut I, plane: ObjectId, state: &PlaneState, property: &PropertyRef) -> KResult<u64> {
    match property.key {
        PropertyKey::FbId => Ok(state.fb.unwrap_or(0) as u64),
        PropertyKey::InFenceFd => Ok(u64::MAX),
        PropertyKey::CrtcId => Ok(state.crtc.unwrap_or(0) as u64),
        PropertyKey::CrtcX => Ok(state.crtc_x as u64),
        PropertyKey::CrtcY => Ok(state.crtc_y as u64),
        PropertyKey::CrtcW => Ok(state.crtc_w),
        PropertyKey::CrtcH => Ok(state.crtc_h),
        PropertyKey::SrcX => Ok(state.src_x),
        PropertyKey::SrcY => Ok(state.src_y),
        PropertyKey::SrcW => Ok(state.src_w),
        PropertyKey::SrcH => Ok(state.src_h),
        PropertyKey::Alpha => Ok(state.alpha),
        PropertyKey::BlendMode => Ok(state.pixel_blend_mode),
        PropertyKey::Rotation => Ok(state.rotation),
        PropertyKey::Zpos => Ok(state.zpos),
        PropertyKey::ColorEncoding => Ok(state.color_encoding),
        PropertyKey::ColorRange => Ok(state.color_range),
        PropertyKey::ColorPipeline => Ok(state.color_pipeline.unwrap_or(0) as u64),
        PropertyKey::FbDamageClips => Ok(state.fb_damage_clips.unwrap_or(0) as u64),
        PropertyKey::ScalingFilter => Ok(state.scaling_filter),
        PropertyKey::HotspotX | PropertyKey::HotspotY => {
            if let Some(result) = io.driver_get_property(plane, &PropertyStateRef::Plane(state), property) { return result; }
            if property.key == PropertyKey::HotspotX { Ok(state.hotspot_x) } else { Ok(state.hotspot_y) }
        }
        PropertyKey::Driver(_) => {
            if let Some(result) = io.driver_get_property(plane, &PropertyStateRef::Plane(state), property) { return result; }
            unknown_property(io, "PLANE", plane, property)
        }
        _ => {
            if let Some(result) = io.driver_get_property(plane, &PropertyStateRef::Plane(state), property) { return result; }
            unknown_property(io, "PLANE", plane, property)
        }
    }
}

// upstream: drm_atomic_uapi.c drm_atomic_color_set_data_property()
pub fn drm_atomic_color_set_data_property<I: AtomicUapiIo>(io: &mut I, colorop: ObjectId, state: &mut ColoropState, val: u64, replaced: &mut bool) -> KResult<()> {
    let dev = object_device(io, colorop);
    let (kind, size) = io.colorop_kind_size(colorop);
    let expected = match kind {
        ColoropKind::Lut1d => size.saturating_mul(io.color_lut32_element_size()),
        ColoropKind::Ctm3x4 => io.color_ctm3x4_element_size(),
        ColoropKind::Lut3d => size.saturating_mul(size).saturating_mul(size).saturating_mul(io.color_lut32_element_size()),
        ColoropKind::Other(_) => return Err(EINVAL),
    };
    *replaced = io.replace_blob_from_id(dev, &mut state.data, val,
        blob_constraints(None, Some(expected), None))?;
    Ok(())
}

// upstream: drm_atomic_uapi.c drm_atomic_colorop_set_property()
pub fn drm_atomic_colorop_set_property<I: AtomicUapiIo>(io: &mut I, colorop: ObjectId, state: &mut ColoropState, property: &PropertyRef, val: u64, replaced: &mut bool) -> KResult<()> {
    match property.key {
        PropertyKey::Bypass => { if state.bypass != val { state.bypass = val; *replaced = true; } }
        PropertyKey::Lut1dInterpolation => { if state.lut1d_interpolation != val { state.lut1d_interpolation = val; *replaced = true; } }
        PropertyKey::Curve1dType => { if state.curve_1d_type != val { state.curve_1d_type = val; *replaced = true; } }
        PropertyKey::Multiplier => { if state.multiplier != val { state.multiplier = val; *replaced = true; } }
        PropertyKey::Lut3dInterpolation => { if state.lut3d_interpolation != val { state.lut3d_interpolation = val; *replaced = true; } }
        PropertyKey::Data => return drm_atomic_color_set_data_property(io, colorop, state, val, replaced),
        _ => return unknown_property(io, "COLOROP", colorop, property),
    }
    Ok(())
}

// upstream: drm_atomic_uapi.c drm_atomic_colorop_get_property()
pub fn drm_atomic_colorop_get_property<I: AtomicUapiIo>(io: &mut I, colorop: ObjectId, state: &ColoropState, property: &PropertyRef) -> KResult<u64> {
    let (kind, size) = io.colorop_kind_size(colorop);
    match property.key {
        PropertyKey::ColoropType => Ok(match kind { ColoropKind::Lut1d => 1, ColoropKind::Ctm3x4 => 2, ColoropKind::Lut3d => 3, ColoropKind::Other(v) => v as u64 }),
        PropertyKey::Bypass => Ok(state.bypass),
        PropertyKey::Lut1dInterpolation => Ok(state.lut1d_interpolation),
        PropertyKey::Curve1dType => Ok(state.curve_1d_type),
        PropertyKey::Multiplier => Ok(state.multiplier),
        PropertyKey::Size => Ok(size as u64),
        PropertyKey::Lut3dInterpolation => Ok(state.lut3d_interpolation),
        PropertyKey::Data => Ok(state.data.unwrap_or(0) as u64),
        _ => unknown_property(io, "COLOROP", colorop, property),
    }
}

// upstream: drm_atomic_uapi.c drm_atomic_set_writeback_fb_for_connector()
pub fn drm_atomic_set_writeback_fb_for_connector<I: AtomicUapiIo>(io: &mut I, state: &mut ConnectorState, fb: Option<FramebufferId>) -> KResult<()> {
    io.writeback_set_fb(state, fb)?;
    if let Some(fb) = fb { io.debug("atomic", &format!("Set FB:{fb} for connector state")); }
    else { io.debug("atomic", "Set NOFB for connector state"); }
    Ok(())
}

// upstream: drm_atomic_uapi.c drm_atomic_connector_set_property()
pub fn drm_atomic_connector_set_property<I: AtomicUapiIo>(io: &mut I, commit: &mut AtomicCommit, connector: ObjectId, state: &mut ConnectorState, file: FileId, property: &PropertyRef, val: u64) -> KResult<()> {
    let dev = object_device(io, connector);
    let mut replaced = false;
    match property.key {
        PropertyKey::CrtcId => {
            let crtc = io.crtc_id_from_user(dev, file, val);
            if val != 0 && crtc.is_none() {
                io.debug("atomic", &format!("[PROP:{}:{}] cannot find CRTC with ID {val}", property.id, property.name));
                return Err(EACCES);
            }
            return drm_atomic_set_crtc_for_connector(io, commit, state, crtc);
        }
        PropertyKey::Dpms => {
            io.debug("atomic", &format!("legacy [PROP:{}:{}] can only be set via legacy uAPI", property.id, property.name));
            return Err(EINVAL);
        }
        PropertyKey::TvSelectSubconnector => state.tv.select_subconnector = val,
        PropertyKey::TvSubconnector => state.tv.subconnector = val,
        PropertyKey::TvLeftMargin => state.tv.margins.left = val,
        PropertyKey::TvRightMargin => state.tv.margins.right = val,
        PropertyKey::TvTopMargin => state.tv.margins.top = val,
        PropertyKey::TvBottomMargin => state.tv.margins.bottom = val,
        PropertyKey::LegacyTvMode => state.tv.legacy_mode = val,
        PropertyKey::TvMode => state.tv.mode = val,
        PropertyKey::TvBrightness => state.tv.brightness = val,
        PropertyKey::TvContrast => state.tv.contrast = val,
        PropertyKey::TvFlickerReduction => state.tv.flicker_reduction = val,
        PropertyKey::TvOverscan => state.tv.overscan = val,
        PropertyKey::TvSaturation => state.tv.saturation = val,
        PropertyKey::TvHue => state.tv.hue = val,
        PropertyKey::LinkStatus => {
            if state.link_status != DRM_LINK_STATUS_GOOD { state.link_status = val; }
        }
        PropertyKey::HdrOutputMetadata => return io.replace_blob_from_id(dev, &mut state.hdr_output_metadata, val,
            blob_constraints(None, Some(io.hdr_output_metadata_element_size()), None)).map(|_| ()),
        PropertyKey::AspectRatio => state.picture_aspect_ratio = val,
        PropertyKey::ContentType => state.content_type = val,
        PropertyKey::ScalingMode => state.scaling_mode = val,
        PropertyKey::ContentProtection => {
            if val == DRM_MODE_CONTENT_PROTECTION_ENABLED {
                io.debug("kms", "only drivers can set CP Enabled");
                return Err(EINVAL);
            }
            state.content_protection = val;
        }
        PropertyKey::HdcpContentType => state.hdcp_content_type = val,
        PropertyKey::Driver(_) => {
            if let Some(result) = io.driver_set_property(commit, connector, &mut PropertyStateMut::Connector(state), property, val) { return result; }
            return unknown_property(io, "CONNECTOR", connector, property);
        }
        PropertyKey::WritebackFbId => {
            let fb = io.framebuffer_lookup(dev, file, val);
            let result = drm_atomic_set_writeback_fb_for_connector(io, state, fb);
            if let Some(fb) = fb { io.framebuffer_put(fb); }
            return result;
        }
        PropertyKey::WritebackOutFencePtr => {
            return set_out_fence_for_connector(io, commit, connector, (val != 0).then_some(val));
        }
        PropertyKey::MaxBpc => state.max_requested_bpc = val,
        PropertyKey::PrivacyScreenSwState => state.privacy_screen_sw_state = val,
        PropertyKey::BroadcastRgb => state.hdmi.broadcast_rgb = val,
        PropertyKey::Colorspace => state.colorspace = val,
        _ => {
            if let Some(result) = io.driver_set_property(commit, connector, &mut PropertyStateMut::Connector(state), property, val) { return result; }
            return unknown_property(io, "CONNECTOR", connector, property);
        }
    }
    let _ = &mut replaced;
    Ok(())
}

// upstream: drm_atomic_uapi.c drm_atomic_connector_get_property()
pub fn drm_atomic_connector_get_property<I: AtomicUapiIo>(io: &mut I, connector: ObjectId, state: &ConnectorState, property: &PropertyRef) -> KResult<u64> {
    match property.key {
        PropertyKey::CrtcId => Ok(state.crtc.unwrap_or(0) as u64),
        PropertyKey::Dpms => {
            if state.crtc.is_some_and(|crtc| io.crtc_self_refresh_active(crtc)) { Ok(DRM_MODE_DPMS_ON as u64) }
            else { Ok(io.connector_dpms(connector) as u64) }
        }
        PropertyKey::TvSelectSubconnector => Ok(state.tv.select_subconnector),
        PropertyKey::TvSubconnector => Ok(state.tv.subconnector),
        PropertyKey::TvLeftMargin => Ok(state.tv.margins.left),
        PropertyKey::TvRightMargin => Ok(state.tv.margins.right),
        PropertyKey::TvTopMargin => Ok(state.tv.margins.top),
        PropertyKey::TvBottomMargin => Ok(state.tv.margins.bottom),
        PropertyKey::LegacyTvMode => Ok(state.tv.legacy_mode),
        PropertyKey::TvMode => Ok(state.tv.mode),
        PropertyKey::TvBrightness => Ok(state.tv.brightness),
        PropertyKey::TvContrast => Ok(state.tv.contrast),
        PropertyKey::TvFlickerReduction => Ok(state.tv.flicker_reduction),
        PropertyKey::TvOverscan => Ok(state.tv.overscan),
        PropertyKey::TvSaturation => Ok(state.tv.saturation),
        PropertyKey::TvHue => Ok(state.tv.hue),
        PropertyKey::LinkStatus => Ok(state.link_status),
        PropertyKey::AspectRatio => Ok(state.picture_aspect_ratio),
        PropertyKey::ContentType => Ok(state.content_type),
        PropertyKey::Colorspace => Ok(state.colorspace),
        PropertyKey::ScalingMode => Ok(state.scaling_mode),
        PropertyKey::HdrOutputMetadata => Ok(state.hdr_output_metadata.unwrap_or(0) as u64),
        PropertyKey::ContentProtection => Ok(state.content_protection),
        PropertyKey::HdcpContentType => Ok(state.hdcp_content_type),
        PropertyKey::WritebackFbId | PropertyKey::WritebackOutFencePtr => Ok(0),
        PropertyKey::MaxBpc => Ok(state.max_requested_bpc),
        PropertyKey::PrivacyScreenSwState => Ok(state.privacy_screen_sw_state),
        PropertyKey::BroadcastRgb => Ok(state.hdmi.broadcast_rgb),
        PropertyKey::Driver(_) => {
            if let Some(result) = io.driver_get_property(connector, &PropertyStateRef::Connector(state), property) { return result; }
            unknown_property(io, "CONNECTOR", connector, property)
        }
        _ => {
            if let Some(result) = io.driver_get_property(connector, &PropertyStateRef::Connector(state), property) { return result; }
            unknown_property(io, "CONNECTOR", connector, property)
        }
    }
}

// upstream: drm_atomic_uapi.c drm_atomic_get_property()
pub fn drm_atomic_get_property<I: AtomicUapiIo>(io: &mut I, object: ObjectId, property: &PropertyRef) -> KResult<u64> {
    let (_, kind, _) = io.object_info(object).ok_or(EINVAL)?;
    match kind {
        ObjectKind::Connector => {
            if !io.connection_lock_is_held() { io.debug("warn", "connection mutex is not locked"); }
            let state = io.current_connector_state(object)?;
            drm_atomic_connector_get_property(io, object, &state, property)
        }
        ObjectKind::Crtc => {
            if !io.object_lock_is_held(object) { io.debug("warn", "CRTC mutex is not locked"); }
            let state = io.current_crtc_state(object)?;
            drm_atomic_crtc_get_property(io, object, &state, property)
        }
        ObjectKind::Plane(_) => {
            if !io.object_lock_is_held(object) { io.debug("warn", "plane mutex is not locked"); }
            let state = io.current_plane_state(object)?;
            drm_atomic_plane_get_property(io, object, &state, property)
        }
        ObjectKind::Colorop => {
            if let Some(plane) = io.colorop_plane(object) {
                if !io.object_lock_is_held(plane) { io.debug("warn", "colorop plane mutex is not locked"); }
            }
            let state = io.current_colorop_state(object)?;
            drm_atomic_colorop_get_property(io, object, &state, property)
        }
        ObjectKind::Other(_) => {
            io.debug("atomic", &format!("[OBJECT:{object}] has no properties"));
            Err(EINVAL)
        }
    }
}

// upstream: drm_atomic_uapi.c create_vblank_event()
pub fn create_vblank_event<I: AtomicUapiIo>(io: &mut I, crtc: ObjectId, user_data: u64) -> Option<PendingVblankEvent> {
    let mut event = io.allocate_event()?;
    event.event_type = DRM_EVENT_FLIP_COMPLETE;
    event.length = DRM_EVENT_VBLANK_SIZE;
    event.crtc = crtc;
    event.user_data = user_data;
    Some(event)
}

// upstream: drm_atomic_uapi.c drm_atomic_connector_commit_dpms()
pub fn drm_atomic_connector_commit_dpms<I: AtomicUapiIo>(io: &mut I, commit: &mut AtomicCommit, connector: ObjectId, mode: i32) -> KResult<()> {
    let ctx = commit.acquire_ctx.ok_or(EINVAL)?;
    let old_mode = io.connector_dpms(connector);
    io.connection_mutex_lock(ctx)?;
    let requested = if mode != DRM_MODE_DPMS_ON { DRM_MODE_DPMS_OFF } else { DRM_MODE_DPMS_ON };
    if old_mode == requested { return Ok(()); }
    io.set_connector_dpms(connector, requested);
    let current = match io.current_connector_state(connector) {
        Ok(state) => state,
        Err(err) => { io.set_connector_dpms(connector, old_mode); return Err(err); }
    };
    let Some(crtc) = current.crtc else { return Ok(()); };
    let result = (|| {
        io.add_affected_connectors(commit, crtc)?;
        io.ensure_crtc_state(commit, crtc)?;
        let mut active = false;
        for id in commit.new_connector_order.clone() {
            let Some(new_state) = commit.connectors.get(&id) else { continue; };
            if new_state.crtc != Some(crtc) { continue; }
            if io.connector_dpms(id) == DRM_MODE_DPMS_ON { active = true; break; }
        }
        commit.crtcs.get_mut(&crtc).ok_or(EINVAL)?.active = active;
        io.atomic_commit(commit)
    })();
    if result.is_err() { io.set_connector_dpms(connector, old_mode); }
    result
}

// upstream: drm_atomic_uapi.c drm_atomic_check_prop_changes()
pub fn drm_atomic_check_prop_changes<I: AtomicUapiIo>(io: &mut I, ret: KResult<u64>, old_val: u64, prop_value: u64, prop: &PropertyRef) -> KResult<()> {
    if ret.is_err() || old_val != prop_value {
        io.debug("atomic", &format!("[PROP:{}:{}] No prop can be changed during async flip", prop.id, prop.name));
        return Err(EINVAL);
    }
    Ok(())
}

fn unknown_property<I: AtomicUapiIo, T>(io: &mut I, class: &str, object: ObjectId, property: &PropertyRef) -> KResult<T> {
    io.debug("atomic", &format!("[{class}:{object}] unknown property [PROP:{}:{}]", property.id, property.name));
    Err(EINVAL)
}

// upstream: drm_atomic_uapi.c drm_atomic_set_property()
pub fn drm_atomic_set_property<I: AtomicUapiIo>(io: &mut I, commit: &mut AtomicCommit, file: FileId, object: ObjectId, property: &PropertyRef, value: u64, async_flip: bool) -> KResult<()> {
    let reference = io.property_change_valid_get(property.id, value).ok_or(EINVAL)?;
    let result = (|| {
        let (_, kind, _) = io.object_info(object).ok_or(EINVAL)?;
        match kind {
            ObjectKind::Connector => {
                io.ensure_connector_state(commit, object)?;
                if async_flip {
                    let state = commit.connectors.get(&object).ok_or(EINVAL)?;
                    let old = drm_atomic_connector_get_property(io, object, state, property);
                    let old_value = old.as_ref().copied().unwrap_or(0);
                    return drm_atomic_check_prop_changes(io, old, old_value, value, property);
                }
                let mut state = commit.connectors.remove(&object).ok_or(EINVAL)?;
                let result = drm_atomic_connector_set_property(io, commit, object, &mut state, file, property, value);
                commit.connectors.insert(object, state);
                result
            }
            ObjectKind::Crtc => {
                io.ensure_crtc_state(commit, object)?;
                if async_flip {
                    let state = commit.crtcs.get(&object).ok_or(EINVAL)?;
                    let old = drm_atomic_crtc_get_property(io, object, state, property);
                    let old_value = old.as_ref().copied().unwrap_or(0);
                    return drm_atomic_check_prop_changes(io, old, old_value, value, property);
                }
                let mut state = commit.crtcs.remove(&object).ok_or(EINVAL)?;
                let result = drm_atomic_crtc_set_property(io, commit, object, &mut state, property, value);
                commit.crtcs.insert(object, state);
                result
            }
            ObjectKind::Plane(plane_kind) => {
                io.ensure_plane_state(commit, object)?;
                if async_flip {
                    let state = commit.planes.get(&object).ok_or(EINVAL)?;
                    let old = drm_atomic_plane_get_property(io, object, state, property);
                    let old_value = old.as_ref().copied().unwrap_or(0);
                    let check = drm_atomic_check_prop_changes(io, old, old_value, value, property);
                    if let Err(err) = check {
                        if !matches!(property.key, PropertyKey::FbId | PropertyKey::InFenceFd | PropertyKey::FbDamageClips) {
                            return Err(err);
                        }
                        if plane_kind != PlaneKind::Primary {
                            match io.plane_async_check(object, commit, true) {
                                Some(Ok(())) => {},
                                Some(Err(async_err)) => {
                                    io.debug("atomic", &format!("[PLANE:{object}] does not support async flips"));
                                    return Err(async_err);
                                }
                                None => {
                                    io.debug("atomic", &format!("[PLANE:{object}] does not support async flips"));
                                    return Err(err);
                                }
                            }
                        }
                    }
                }
                let mut state = commit.planes.remove(&object).ok_or(EINVAL)?;
                let result = drm_atomic_plane_set_property(io, commit, object, &mut state, file, property, value);
                commit.planes.insert(object, state);
                result
            }
            ObjectKind::Colorop => {
                io.ensure_colorop_state(commit, object)?;
                let mut replaced = false;
                drm_atomic_colorop_set_property(io, object, commit.colorops.get_mut(&object).ok_or(EINVAL)?, property, value, &mut replaced)?;
                if !replaced { return Ok(()); }
                let plane = io.colorop_plane(object).ok_or(EINVAL)?;
                io.ensure_plane_state(commit, plane)?;
                commit.planes.get_mut(&plane).ok_or(EINVAL)?.color_mgmt_changed |= replaced;
                Ok(())
            }
            ObjectKind::Other(_) => {
                io.debug("atomic", &format!("[OBJECT:{object}] has no properties"));
                Err(EINVAL)
            }
        }
    })();
    io.property_change_valid_put(property.id, reference);
    result
}

// upstream: drm_atomic_uapi.c setup_out_fence()
pub fn setup_out_fence<I: AtomicUapiIo>(io: &mut I, fence_state: &mut OutFenceState, fence: FenceId) -> KResult<()> {
    fence_state.fd = io.get_unused_fd_cloexec();
    if fence_state.fd < 0 { return Err(fence_state.fd); }
    io.user_put_i32(fence_state.out_fence_ptr.ok_or(EFAULT)?, fence_state.fd).map_err(|_| EFAULT)?;
    fence_state.sync_file = Some(io.sync_file_create(fence).ok_or(ENOMEM)?);
    Ok(())
}

// upstream: drm_atomic_uapi.c prepare_signaling()
pub fn prepare_signaling<I: AtomicUapiIo>(io: &mut I, commit: &mut AtomicCommit, arg: &AtomicModeArgs, file: Option<FileId>, fence_state: &mut Option<Vec<OutFenceState>>, num_fences: &mut usize) -> KResult<()> {
    if arg.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0 { return Ok(()); }
    let mut crtc_count = 0usize;
    *num_fences = 0;
    for crtc in commit.new_crtc_order.clone() {
        let fence_ptr = get_out_fence_for_crtc(commit, crtc);
        if arg.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 || fence_ptr.is_some() {
            let event = create_vblank_event(io, crtc, arg.user_data).ok_or(ENOMEM)?;
            commit.crtcs.get_mut(&crtc).ok_or(EINVAL)?.event = Some(event);
        }
        if arg.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
            let Some(file) = file else { crtc_count += 1; continue; };
            let state = commit.crtcs.get_mut(&crtc).ok_or(EINVAL)?;
            let event = state.event.as_mut().ok_or(EINVAL)?;
            if let Err(err) = io.event_reserve(commit.dev, file, event) {
                let event = state.event.take().expect("event just reserved");
                io.event_free_unreserved(event);
                return Err(err);
            }
        }
        if let Some(pointer) = fence_ptr {
            if fence_state.is_none() { *fence_state = Some(Vec::new()); }
            let fences = fence_state.as_mut().expect("created above");
            fences.try_reserve(1).map_err(|_| ENOMEM)?;
            fences.push(OutFenceState { out_fence_ptr: Some(pointer), ..OutFenceState::default() });
            let fence = io.crtc_create_fence(crtc).ok_or(ENOMEM)?;
            *num_fences += 1;
            let setup_result = setup_out_fence(io, fences.get_mut(*num_fences - 1).ok_or(EINVAL)?, fence);
            if let Err(err) = setup_result { io.dma_fence_put(fence); return Err(err); }
            commit.crtcs.get_mut(&crtc).and_then(|state| state.event.as_mut()).ok_or(EINVAL)?.fence = Some(fence);
        }
        crtc_count += 1;
    }
    for connector in commit.new_connector_order.clone() {
        let has_job = commit.connectors.get(&connector).is_some_and(|state| state.writeback_job.is_some());
        if !has_job { continue; }
        let Some(pointer) = get_out_fence_for_connector(commit, connector) else { continue; };
        if fence_state.is_none() { *fence_state = Some(Vec::new()); }
        let fences = fence_state.as_mut().expect("created above");
        fences.try_reserve(1).map_err(|_| ENOMEM)?;
        fences.push(OutFenceState { out_fence_ptr: Some(pointer), ..OutFenceState::default() });
        let fence = io.writeback_out_fence(connector).ok_or(ENOMEM)?;
        *num_fences += 1;
        let setup_result = setup_out_fence(io, fences.get_mut(*num_fences - 1).ok_or(EINVAL)?, fence);
        if let Err(err) = setup_result { io.dma_fence_put(fence); return Err(err); }
        commit.connectors.get_mut(&connector).and_then(|state| state.writeback_job.as_mut()).ok_or(EINVAL)?.out_fence = Some(fence);
    }
    if crtc_count == 0 && arg.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
        io.debug("atomic", "need at least one CRTC for DRM_MODE_PAGE_FLIP_EVENT");
        return Err(EINVAL);
    }
    Ok(())
}

// upstream: drm_atomic_uapi.c complete_signaling()
pub fn complete_signaling<I: AtomicUapiIo>(io: &mut I, dev: DeviceId, commit: &mut AtomicCommit, fence_state: Option<Vec<OutFenceState>>, num_fences: usize, install_fds: bool) {
    if install_fds {
        if let Some(fences) = fence_state {
            for fence in fences.into_iter().take(num_fences) {
                if let Some(sync_file) = fence.sync_file { io.sync_file_install(fence.fd, sync_file); }
            }
        }
        return;
    }
    for crtc in commit.new_crtc_order.clone() {
        let event = commit.crtcs.get_mut(&crtc).and_then(|state| state.event.as_mut());
        if event.is_some_and(|event| event.fence.is_some() || event.file_priv.is_some()) {
            if let Some(event) = commit.crtcs.get_mut(&crtc).and_then(|state| state.event.take()) {
                io.event_cancel_free(dev, event);
            }
        }
    }
    let Some(fences) = fence_state else { return; };
    for fence in fences.into_iter().take(num_fences) {
        if let Some(sync_file) = fence.sync_file { io.sync_file_put(sync_file); }
        if fence.fd >= 0 { io.put_unused_fd(fence.fd); }
        if let Some(pointer) = fence.out_fence_ptr {
            if io.user_put_i32(pointer, -1).is_err() { io.debug("atomic", "Couldn't clear out_fence_ptr"); }
        }
    }
}

// upstream: drm_atomic_uapi.c set_async_flip()
pub fn set_async_flip(commit: &mut AtomicCommit) {
    for crtc in commit.new_crtc_order.clone() {
        if let Some(state) = commit.crtcs.get_mut(&crtc) { state.async_flip = true; }
    }
}

// upstream: drm_atomic_uapi.c drm_mode_atomic_ioctl()
pub fn drm_mode_atomic_ioctl<I: AtomicUapiIo>(io: &mut I, dev: DeviceId, arg: &AtomicModeArgs, file: FileId) -> KResult<()> {
    if !io.core_has_atomic(dev) { return Err(EOPNOTSUPP); }
    if !io.file_atomic_enabled(file) {
        io.debug("atomic", "commit failed: atomic cap not enabled");
        return Err(EINVAL);
    }
    if arg.flags & !DRM_MODE_ATOMIC_FLAGS != 0 {
        io.debug("atomic", "commit failed: invalid flag");
        return Err(EINVAL);
    }
    if arg.reserved != 0 {
        io.debug("atomic", "commit failed: reserved field set");
        return Err(EINVAL);
    }
    let async_flip = arg.flags & DRM_MODE_PAGE_FLIP_ASYNC != 0;
    if async_flip && !io.async_page_flip_supported(dev) {
        io.debug("atomic", "commit failed: DRM_MODE_PAGE_FLIP_ASYNC not supported");
        return Err(EINVAL);
    }
    if arg.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0 && arg.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
        io.debug("atomic", "commit failed: page-flip event requested with test-only commit");
        return Err(EINVAL);
    }
    let mut commit = io.new_commit(dev).ok_or(ENOMEM)?;
    let ctx = io.modeset_acquire_init();
    commit.acquire_ctx = Some(ctx);
    commit.allow_modeset = arg.flags & DRM_MODE_ATOMIC_ALLOW_MODESET != 0;
    commit.plane_color_pipeline = io.file_plane_color_pipeline(file);
    loop {
        let mut copied_objs = 0usize;
        let mut copied_props = 0usize;
        let mut fence_state = None;
        let mut num_fences = 0usize;
        let parse_result = (|| {
            for _ in 0..arg.count_objs {
                let obj_id = io.read_user_u32(arg.objs_ptr, copied_objs)?;
                let object = match io.object_find(dev, file, obj_id) {
                    Some(object) => object,
                    None => {
                        io.debug("atomic", &format!("cannot find object ID {obj_id}"));
                        return Err(ENOENT);
                    }
                };
                if !io.object_has_properties(object.id) {
                    io.debug("atomic", &format!("[OBJECT:{}] has no properties", object.id));
                    io.object_put(object);
                    return Err(ENOENT);
                }
                let count_props = match io.read_user_u32(arg.count_props_ptr, copied_objs) {
                    Ok(value) => value,
                    Err(err) => { io.object_put(object); return Err(err); }
                };
                copied_objs += 1;
                let object_result = (|| {
                    for _ in 0..count_props {
                        let prop_id = io.read_user_u32(arg.props_ptr, copied_props)?;
                        let property = match io.property_lookup(object.id, prop_id) {
                            Some(property) => property,
                            None => {
                                io.debug("atomic", &format!("[OBJECT:{}] cannot find property ID {prop_id}", object.id));
                                return Err(ENOENT);
                            }
                        };
                        let prop_value = io.read_user_u64(arg.prop_values_ptr, copied_props)?;
                        drm_atomic_set_property(io, &mut commit, file, object.id, &property, prop_value, async_flip)?;
                        copied_props += 1;
                    }
                    Ok(())
                })();
                io.object_put(object);
                object_result?;
            }
            Ok(())
        })();
        let mut ret = parse_result;
        if ret.is_ok() {
            match prepare_signaling(io, &mut commit, arg, Some(file), &mut fence_state, &mut num_fences) {
                Ok(()) => {},
                Err(err) => ret = Err(err),
            }
        }
        if ret.is_ok() {
            if async_flip { set_async_flip(&mut commit); }
            ret = if arg.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0 {
                io.atomic_check_only(&mut commit)
            } else if arg.flags & DRM_MODE_ATOMIC_NONBLOCK != 0 {
                io.atomic_nonblocking_commit(&mut commit)
            } else {
                io.atomic_commit(&mut commit)
            };
        }
        complete_signaling(io, dev, &mut commit, fence_state, num_fences, ret.is_ok());
        if ret == Err(EDEADLK) {
            io.atomic_commit_clear(&mut commit);
            match io.modeset_backoff(ctx) {
                Ok(()) => continue,
                Err(err) => ret = Err(err),
            }
        }
        io.atomic_commit_put(commit);
        io.modeset_drop_locks(ctx);
        io.modeset_acquire_fini(ctx);
        return ret;
    }
}
