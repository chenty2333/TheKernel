// SPDX-License-Identifier: LicenseRef-Intel-Drm-Framebuffer-MIT
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

//! Function-level translation of Linux v7.2.3 `drm_framebuffer.c`.
//! Framework operations (IDR, GEM handles, copy-from-user, locks and workqueues)
//! are represented by explicit state transitions and injectable operation results.

#![no_std]
extern crate alloc;

use alloc::{format, string::String, vec, vec::Vec};

pub type KResult<T> = Result<T, i32>;
pub type FbId = u32;
pub type ObjectId = u32;
pub type GemObjectId = u64;

pub const EFAULT: i32 = -14;
pub const EINVAL: i32 = -22;
pub const ENODEV: i32 = -19;
pub const ENOENT: i32 = -2;
pub const ENOMEM: i32 = -12;
pub const ENOSPC: i32 = -28;
pub const ENOSYS: i32 = -38;
pub const ERANGE: i32 = -34;
pub const EOPNOTSUPP: i32 = -95;
pub const EDEADLK: i32 = -35;

pub const DRM_MODE_FB_INTERLACED: u32 = 1 << 0;
pub const DRM_MODE_FB_MODIFIERS: u32 = 1 << 1;
pub const DRM_MODE_FB_DIRTY_ANNOTATE_COPY: u32 = 1 << 0;
pub const DRM_MODE_FB_DIRTY_ANNOTATE_FILL: u32 = 1 << 1;
pub const DRM_MODE_FB_DIRTY_FLAGS: u32 = DRM_MODE_FB_DIRTY_ANNOTATE_COPY | DRM_MODE_FB_DIRTY_ANNOTATE_FILL;
pub const DRM_MODE_FB_DIRTY_MAX_CLIPS: i32 = 256;
pub const DRM_FORMAT_INVALID: u32 = 0;
pub const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
pub const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
pub const DRM_FORMAT_RGB565: u32 = u32::from_le_bytes(*b"RG16");
pub const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
pub const DRM_FORMAT_MOD_SAMSUNG_64_32_TILE: u64 = 0x0400_0000_0000_0001;
pub const DRM_DRIVER_MODESET: u32 = 1 << 0;
pub const DRM_MODE_OBJECT_FB: u32 = 0xfbfb_fbfb;
pub const CAP_SYS_ADMIN: u32 = 21;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FormatInfo {
    pub format: u32,
    pub num_planes: usize,
    pub depth: u32,
    /// Number of bytes in a minimum-pitch format block for each plane.
    pub char_per_block: [u8; 4],
    pub block_width: [u8; 4],
    pub hsub: u8,
    pub vsub: u8,
}

impl FormatInfo {
    fn plane_width(&self, width: u32, plane: usize) -> u32 {
        if plane == 0 || self.hsub <= 1 { width } else { width.div_ceil(self.hsub as u32) }
    }
    fn plane_height(&self, height: u32, plane: usize) -> u32 {
        if plane == 0 || self.vsub <= 1 { height } else { height.div_ceil(self.vsub as u32) }
    }
    fn min_pitch(&self, plane: usize, width: u32) -> u64 {
        let block = self.block_width[plane].max(1) as u64;
        let bytes = self.char_per_block[plane] as u64;
        (u64::from(width).div_ceil(block)).saturating_mul(bytes)
    }
    fn bpp(&self, plane: usize) -> u32 { u32::from(self.char_per_block[plane]) * 8 }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeFbCmd {
    pub fb_id: FbId, pub width: u32, pub height: u32, pub pitch: u32,
    pub bpp: u32, pub depth: u32, pub handle: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeFbCmd2 {
    pub fb_id: FbId, pub width: u32, pub height: u32, pub pixel_format: u32,
    pub flags: u32, pub handles: [u32; 4], pub pitches: [u32; 4],
    pub offsets: [u32; 4], pub modifier: [u64; 4],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeCloseFb { pub fb_id: FbId, pub pad: u32 }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeFbDirtyCmd { pub fb_id: FbId, pub flags: u32, pub color: u32, pub num_clips: i32 }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClipRect { pub x1: u16, pub y1: u16, pub x2: u16, pub y2: u16 }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FbFuncs { pub create_handle: bool, pub dirty: bool }

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Framebuffer {
    pub id: FbId, pub width: u32, pub height: u32, pub pixel_format: u32,
    pub format: FormatInfo, pub flags: u32, pub modifier: u64,
    pub pitches: [u32; 4], pub offsets: [u32; 4],
    pub obj: [Option<GemObjectId>; 4], pub funcs: FbFuncs,
    pub refcount: u32, pub handle_ref: [bool; 4], pub listed: bool,
    pub destroyed: bool, pub comm: String, pub device_id: u64, pub in_filp_list: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plane { pub id: ObjectId, pub name: String, pub fb: Option<FbId>, pub crtc: Option<ObjectId> }
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Crtc { pub id: ObjectId, pub name: String, pub primary: Option<ObjectId>, pub active: bool }
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Connector { pub id: ObjectId, pub crtc: Option<ObjectId> }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GemHandle { pub handle: u32, pub object: GemObjectId }

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrmFile {
    pub fbs: Vec<FbId>, pub gem_handles: Vec<GemHandle>, pub next_handle: u32,
    pub is_current_master: bool, pub has_sys_admin: bool, pub leased_fbs: Option<Vec<FbId>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub device_id: u64, pub current_comm: String, pub modeset: bool, pub min_width: u32, pub max_width: u32,
    pub min_height: u32, pub max_height: u32, pub fb_modifiers_not_supported: bool,
    pub prefer_host_byte_order: bool, pub formats: Vec<FormatInfo>,
    pub framebuffers: Vec<Framebuffer>, pub planes: Vec<Plane>,
    pub crtcs: Vec<Crtc>, pub connectors: Vec<Connector>, pub atomic_modeset: bool,
    pub next_fb_id: FbId, pub next_fb_id_error: Option<i32>,
    pub fb_create_error: Option<i32>, pub create_handle_error: Option<i32>,
    pub dirty_error: i32, pub dirty_calls: Vec<(FbId, u32, u32, Vec<ClipRect>, i32)>,
    pub debug: Vec<String>, pub warnings: Vec<String>, pub gem_objects: Vec<GemObjectId>,
    pub gem_handle_refs: Vec<(GemObjectId, u32)>,
    pub atomic_alloc_error: Option<i32>, pub atomic_lock_deadlocks: u32,
    pub atomic_commit_error: i32, pub atomic_plane_state_error: Option<i32>,
    pub atomic_connector_error: i32, pub atomic_commit_count: u32,
    pub lock_held: bool, pub fb_lock_held: bool, pub registered_ids: Vec<FbId>,
    pub debugfs_registered: bool, pub debugfs_enabled: bool, pub num_fb: u32,
}

impl Default for Device {
    fn default() -> Self {
        Self {
            device_id: 1, current_comm: String::new(), modeset: true, min_width: 1, max_width: u32::MAX,
            min_height: 1, max_height: u32::MAX, fb_modifiers_not_supported: false,
            prefer_host_byte_order: false, formats: Vec::new(), framebuffers: Vec::new(),
            planes: Vec::new(), crtcs: Vec::new(), connectors: Vec::new(), atomic_modeset: true,
            next_fb_id: 1, next_fb_id_error: None, fb_create_error: None,
            create_handle_error: None, dirty_error: 0, dirty_calls: Vec::new(),
            debug: Vec::new(), warnings: Vec::new(), gem_objects: Vec::new(), gem_handle_refs: Vec::new(),
            atomic_alloc_error: None, atomic_lock_deadlocks: 0, atomic_commit_error: 0,
            atomic_plane_state_error: None, atomic_connector_error: 0,
            atomic_commit_count: 0, lock_held: false, fb_lock_held: false,
            registered_ids: Vec::new(), debugfs_registered: false, debugfs_enabled: false, num_fb: 0,
        }
    }
}

impl Device {
    fn fb_pos(&self, id: FbId) -> Option<usize> { self.framebuffers.iter().position(|fb| fb.id == id) }
    fn fb(&self, id: FbId) -> Option<&Framebuffer> { self.fb_pos(id).map(|i| &self.framebuffers[i]) }
    fn fb_mut(&mut self, id: FbId) -> Option<&mut Framebuffer> { self.fb_pos(id).map(|i| &mut self.framebuffers[i]) }
    fn find_format(&self, code: u32) -> Option<FormatInfo> { self.formats.iter().copied().find(|f| f.format == code) }
    fn log(&mut self, msg: String) { self.debug.push(msg); }
    fn warn(&mut self, msg: String) { self.warnings.push(msg); }
}

fn check_modeset(dev: &Device, unsupported: i32) -> KResult<()> {
    if dev.modeset { Ok(()) } else { Err(unsupported) }
}

fn object_from_handle(file: &DrmFile, handle: u32) -> Option<GemObjectId> {
    file.gem_handles.iter().find(|entry| entry.handle == handle).map(|entry| entry.object)
}

fn add_gem_handle_ref(dev: &mut Device, object: GemObjectId) {
    if let Some((_, refs)) = dev.gem_handle_refs.iter_mut().find(|(id, _)| *id == object) { *refs = refs.saturating_add(1); }
    else { dev.gem_handle_refs.push((object, 1)); }
    if !dev.gem_objects.contains(&object) { dev.gem_objects.push(object); }
}

fn drop_gem_handle_ref(dev: &mut Device, object: GemObjectId) {
    if let Some(index) = dev.gem_handle_refs.iter().position(|(id, _)| *id == object) {
        if dev.gem_handle_refs[index].1 > 1 { dev.gem_handle_refs[index].1 -= 1; }
        else { dev.gem_handle_refs.remove(index); dev.gem_objects.retain(|id| *id != object); }
    }
}

fn create_gem_handle(dev: &mut Device, file: &mut DrmFile, object: GemObjectId) -> KResult<u32> {
    if let Some(err) = dev.create_handle_error { return Err(err); }
    let handle = file.next_handle.max(1);
    file.next_handle = handle.wrapping_add(1).max(1);
    file.gem_handles.push(GemHandle { handle, object });
    add_gem_handle_ref(dev, object);
    Ok(handle)
}

fn delete_gem_handle(dev: &mut Device, file: &mut DrmFile, handle: u32) {
    if let Some(index) = file.gem_handles.iter().position(|entry| entry.handle == handle) {
        let object = file.gem_handles.remove(index).object;
        drop_gem_handle_ref(dev, object);
    }
}

fn framebuffer_put(dev: &mut Device, fb_id: FbId) {
    let Some(pos) = dev.fb_pos(fb_id) else { return; };
    if dev.framebuffers[pos].refcount > 0 { dev.framebuffers[pos].refcount -= 1; }
    if dev.framebuffers[pos].refcount == 0 { drm_framebuffer_free(dev, fb_id); }
}

fn lookup_fb(dev: &mut Device, id: FbId) -> bool {
    if let Some(fb) = dev.fb_mut(id) { fb.refcount = fb.refcount.saturating_add(1); true } else { false }
}

fn register_fb(dev: &mut Device, fb: &mut Framebuffer) -> KResult<()> {
    if let Some(err) = dev.next_fb_id_error.take() { return Err(err); }
    if fb.id == 0 { fb.id = dev.next_fb_id; dev.next_fb_id = dev.next_fb_id.wrapping_add(1).max(1); }
    fb.refcount = fb.refcount.max(1);
    fb.listed = true;
    fb.in_filp_list = false;
    dev.registered_ids.push(fb.id);
    dev.framebuffers.push(fb.clone());
    Ok(())
}

fn framebuffer_from_cmd(dev: &Device, info: FormatInfo, r: &ModeFbCmd2) -> Framebuffer {
    let mut objects = [None; 4];
    // Driver-created objects are looked up from this ioctl's handles by the caller.
    let _ = (&dev, &mut objects);
    Framebuffer { id: 0, width: r.width, height: r.height, pixel_format: r.pixel_format,
        format: info, flags: r.flags, modifier: r.modifier[0], pitches: r.pitches,
        offsets: r.offsets, obj: objects, funcs: FbFuncs { create_handle: true, dirty: false },
        refcount: 1, handle_ref: [false; 4], listed: false, destroyed: false, comm: String::new(),
        device_id: dev.device_id, in_filp_list: false }
}

fn format_plane_width(info: &FormatInfo, width: u32, plane: usize) -> u32 { info.plane_width(width, plane) }
fn format_plane_height(info: &FormatInfo, height: u32, plane: usize) -> u32 { info.plane_height(height, plane) }
fn format_min_pitch(info: &FormatInfo, plane: usize, width: u32) -> u64 { info.min_pitch(plane, width) }

// upstream: drm_framebuffer.c drm_framebuffer_check_src_coords()
pub fn drm_framebuffer_check_src_coords(src_x: u32, src_y: u32, src_w: u32, src_h: u32,
                                        fb: &Framebuffer) -> KResult<()> {
    let fb_width = fb.width.wrapping_shl(16);
    let fb_height = fb.height.wrapping_shl(16);
    if src_w > fb_width || src_x > fb_width.wrapping_sub(src_w) ||
       src_h > fb_height || src_y > fb_height.wrapping_sub(src_h) {
        return Err(ENOSPC);
    }
    Ok(())
}

fn legacy_format(dev: &Device, bpp: u32, depth: u32) -> u32 {
    let code = match (bpp, depth) {
        (32, 24) => DRM_FORMAT_XRGB8888,
        (32, 32) => DRM_FORMAT_ARGB8888,
        (16, 16) => DRM_FORMAT_RGB565,
        _ => DRM_FORMAT_INVALID,
    };
    if dev.find_format(code).is_some() { code } else { DRM_FORMAT_INVALID }
}

// upstream: drm_framebuffer.c drm_mode_addfb()
pub fn drm_mode_addfb(dev: &mut Device, request: &mut ModeFbCmd, file: &mut DrmFile) -> KResult<()> {
    check_modeset(dev, EOPNOTSUPP)?;
    let pixel_format = legacy_format(dev, request.bpp, request.depth);
    if pixel_format == DRM_FORMAT_INVALID {
        dev.log(format!("bad {{bpp:{}, depth:{}}}", request.bpp, request.depth));
        return Err(EINVAL);
    }
    let mut r = ModeFbCmd2 { fb_id: request.fb_id, width: request.width, height: request.height,
        pixel_format, pitches: [request.pitch, 0, 0, 0], handles: [request.handle, 0, 0, 0], ..ModeFbCmd2::default() };
    drm_mode_addfb2(dev, &mut r, file)?;
    request.fb_id = r.fb_id;
    Ok(())
}

// upstream: drm_framebuffer.c drm_mode_addfb_ioctl()
pub fn drm_mode_addfb_ioctl(dev: &mut Device, data: &mut ModeFbCmd, file: &mut DrmFile) -> KResult<()> {
    drm_mode_addfb(dev, data, file)
}

// upstream: drm_framebuffer.c framebuffer_check()
fn framebuffer_check(dev: &mut Device, info: &FormatInfo, r: &ModeFbCmd2) -> KResult<()> {
    if r.width == 0 { dev.log(format!("bad framebuffer width {}", r.width)); return Err(EINVAL); }
    if r.height == 0 { dev.log(format!("bad framebuffer height {}", r.height)); return Err(EINVAL); }

    for i in 0..info.num_planes {
        let width = format_plane_width(info, r.width, i);
        let height = format_plane_height(info, r.height, i);
        let block_size = info.char_per_block[i];
        let min_pitch = format_min_pitch(info, i, width);
        if block_size == 0 && r.modifier[i] == DRM_FORMAT_MOD_LINEAR {
            dev.log(format!("Format requires non-linear modifier for plane {}", i)); return Err(EINVAL);
        }
        if r.handles[i] == 0 { dev.log(format!("no buffer object handle for plane {}", i)); return Err(EINVAL); }
        if min_pitch > u64::from(u32::MAX) { return Err(ERANGE); }
        if u64::from(height) * u64::from(r.pitches[i]) + u64::from(r.offsets[i]) > u64::from(u32::MAX) {
            return Err(ERANGE);
        }
        if block_size != 0 && u64::from(r.pitches[i]) < min_pitch {
            dev.log(format!("bad pitch {} for plane {}", r.pitches[i], i)); return Err(EINVAL);
        }
        if r.modifier[i] != 0 && r.flags & DRM_MODE_FB_MODIFIERS == 0 {
            dev.log(format!("bad fb modifier {} for plane {}", r.modifier[i], i)); return Err(EINVAL);
        }
        if r.flags & DRM_MODE_FB_MODIFIERS != 0 && r.modifier[i] != r.modifier[0] {
            dev.log(format!("bad fb modifier {} for plane {}", r.modifier[i], i)); return Err(EINVAL);
        }
        if r.modifier[i] == DRM_FORMAT_MOD_SAMSUNG_64_32_TILE &&
           (r.pixel_format != DRM_FORMAT_NV12 || width % 128 != 0 || height % 32 != 0 || r.pitches[i] % 128 != 0) {
            dev.log(format!("bad modifier data for plane {}", i)); return Err(EINVAL);
        }
    }

    for i in info.num_planes..4 {
        if r.modifier[i] != 0 { dev.log(format!("non-zero modifier for unused plane {}", i)); return Err(EINVAL); }
        // Before FB_MODIFIERS userspace did not reliably clear the remaining fields.
        if r.flags & DRM_MODE_FB_MODIFIERS == 0 { continue; }
        if r.handles[i] != 0 { dev.log(format!("buffer object handle for unused plane {}", i)); return Err(EINVAL); }
        if r.pitches[i] != 0 { dev.log(format!("non-zero pitch for unused plane {}", i)); return Err(EINVAL); }
        if r.offsets[i] != 0 { dev.log(format!("non-zero offset for unused plane {}", i)); return Err(EINVAL); }
    }
    Ok(())
}

// upstream: drm_framebuffer.c drm_internal_framebuffer_create()
pub fn drm_internal_framebuffer_create(dev: &mut Device, r: &ModeFbCmd2, file: &DrmFile) -> KResult<FbId> {
    if r.flags & !(DRM_MODE_FB_INTERLACED | DRM_MODE_FB_MODIFIERS) != 0 {
        dev.log(format!("bad framebuffer flags 0x{:08x}", r.flags)); return Err(EINVAL);
    }
    if r.width < dev.min_width || r.width > dev.max_width {
        dev.log(format!("bad framebuffer width {}, should be >= {} && <= {}", r.width, dev.min_width, dev.max_width)); return Err(EINVAL);
    }
    if r.height < dev.min_height || r.height > dev.max_height {
        dev.log(format!("bad framebuffer height {}, should be >= {} && <= {}", r.height, dev.min_height, dev.max_height)); return Err(EINVAL);
    }
    if r.flags & DRM_MODE_FB_MODIFIERS != 0 && dev.fb_modifiers_not_supported {
        dev.log("driver does not support fb modifiers".into()); return Err(EINVAL);
    }
    let Some(info) = dev.find_format(r.pixel_format) else {
        dev.log(format!("bad framebuffer format 0x{:08x}", r.pixel_format)); return Err(EINVAL);
    };
    framebuffer_check(dev, &info, r)?;
    if let Some(err) = dev.fb_create_error.take() {
        dev.log("could not create framebuffer".into()); return Err(err);
    }
    let mut fb = framebuffer_from_cmd(dev, info, r);
    for i in 0..info.num_planes { fb.obj[i] = object_from_handle(file, r.handles[i]); }
    let id = fb.id;
    let comm = dev.current_comm.clone();
    drm_framebuffer_init(dev, &mut fb, &comm)?;
    Ok(if id == 0 { dev.framebuffers.last().map(|f| f.id).unwrap_or(0) } else { id })
}

// upstream: drm_framebuffer.c drm_mode_addfb2()
pub fn drm_mode_addfb2(dev: &mut Device, r: &mut ModeFbCmd2, file: &mut DrmFile) -> KResult<()> {
    check_modeset(dev, EOPNOTSUPP)?;
    let fb_id = drm_internal_framebuffer_create(dev, r, file)?;
    r.fb_id = fb_id;
    dev.log(format!("[FB:{}]", fb_id));
    // Transfer the initial reference to the per-file list until close or RMFB.
    file.fbs.push(fb_id);
    if let Some(fb) = dev.fb_mut(fb_id) { fb.in_filp_list = true; }
    Ok(())
}

// upstream: drm_framebuffer.c drm_mode_addfb2_ioctl()
pub fn drm_mode_addfb2_ioctl(dev: &mut Device, data: &mut ModeFbCmd2, file: &mut DrmFile,
                             big_endian: bool) -> KResult<()> {
    if big_endian && !dev.prefer_host_byte_order {
        dev.log("addfb2 broken on bigendian".into()); return Err(EOPNOTSUPP);
    }
    drm_mode_addfb2(dev, data, file)
}

// upstream: drm_framebuffer.c drm_mode_rmfb_work_fn()
pub fn drm_mode_rmfb_work_fn(dev: &mut Device, work_fbs: &mut Vec<FbId>) {
    while !work_fbs.is_empty() {
        let fb_id = work_fbs.remove(0);
        if let Some(fb) = dev.fb(fb_id) {
            dev.log(format!("Removing [FB:{}] from all active usage due to RMFB ioctl", fb.id));
        }
        drm_framebuffer_remove(dev, Some(fb_id));
    }
}

// upstream: drm_framebuffer.c drm_mode_closefb()
pub fn drm_mode_closefb(dev: &mut Device, fb_id: FbId, file: &mut DrmFile) -> KResult<()> {
    let Some(index) = file.fbs.iter().position(|id| *id == fb_id) else { return Err(ENOENT); };
    file.fbs.remove(index);
    if let Some(fb) = dev.fb_mut(fb_id) { fb.in_filp_list = false; }
    // Drop the reference stored in the per-file framebuffer list.
    framebuffer_put(dev, fb_id);
    Ok(())
}

// upstream: drm_framebuffer.c drm_mode_rmfb()
pub fn drm_mode_rmfb(dev: &mut Device, fb_id: FbId, file: &mut DrmFile) -> KResult<()> {
    check_modeset(dev, EOPNOTSUPP)?;
    if !lookup_fb(dev, fb_id) { return Err(ENOENT); }
    if let Err(err) = drm_mode_closefb(dev, fb_id, file) {
        framebuffer_put(dev, fb_id);
        return Err(err);
    }
    // The lookup reference is the only one when the framebuffer is inactive.
    if dev.fb(fb_id).map(|fb| fb.refcount > 1).unwrap_or(false) {
        let mut work_fbs = vec![fb_id];
        drm_mode_rmfb_work_fn(dev, &mut work_fbs);
    } else {
        framebuffer_put(dev, fb_id);
    }
    Ok(())
}

// upstream: drm_framebuffer.c drm_mode_rmfb_ioctl()
pub fn drm_mode_rmfb_ioctl(dev: &mut Device, fb_id: &mut FbId, file: &mut DrmFile) -> KResult<()> {
    drm_mode_rmfb(dev, *fb_id, file)
}

// upstream: drm_framebuffer.c drm_mode_closefb_ioctl()
pub fn drm_mode_closefb_ioctl(dev: &mut Device, request: &ModeCloseFb, file: &mut DrmFile) -> KResult<()> {
    check_modeset(dev, EOPNOTSUPP)?;
    if request.pad != 0 { return Err(EINVAL); }
    if !lookup_fb(dev, request.fb_id) { return Err(ENOENT); }
    let ret = drm_mode_closefb(dev, request.fb_id, file);
    framebuffer_put(dev, request.fb_id);
    ret
}

fn create_handle_for_fb(dev: &mut Device, fb: &Framebuffer, file: &mut DrmFile) -> KResult<u32> {
    if let Some(err) = dev.create_handle_error { return Err(err); }
    let object = fb.obj[0].unwrap_or(u64::from(fb.id));
    create_gem_handle(dev, file, object)
}

// upstream: drm_framebuffer.c drm_mode_getfb()
pub fn drm_mode_getfb(dev: &mut Device, request: &mut ModeFbCmd, file: &mut DrmFile) -> KResult<()> {
    check_modeset(dev, EOPNOTSUPP)?;
    let id = request.fb_id;
    if !lookup_fb(dev, id) { return Err(ENOENT); }
    let result = (|| {
        let fb = dev.fb(id).ok_or(ENOENT)?.clone();
        if fb.format.num_planes > 1 { return Err(EINVAL); }
        if !fb.funcs.create_handle { return Err(ENODEV); }
        request.height = fb.height;
        request.width = fb.width;
        request.depth = fb.format.depth;
        request.bpp = fb.format.bpp(0);
        request.pitch = fb.pitches[0];
        // GET_FB is unprivileged, but a buffer handle is withheld from non-masters.
        if !file.is_current_master && !file.has_sys_admin { request.handle = 0; return Ok(()); }
        request.handle = create_handle_for_fb(dev, &fb, file)?;
        Ok(())
    })();
    framebuffer_put(dev, id);
    result
}

// upstream: drm_framebuffer.c drm_mode_getfb2_ioctl()
pub fn drm_mode_getfb2_ioctl(dev: &mut Device, request: &mut ModeFbCmd2, file: &mut DrmFile) -> KResult<()> {
    check_modeset(dev, EINVAL)?;
    let id = request.fb_id;
    if !lookup_fb(dev, id) { return Err(ENOENT); }
    let fb = match dev.fb(id) {
        Some(fb) => fb.clone(),
        None => { framebuffer_put(dev, id); return Err(ENOENT); }
    };
    let mut ret: KResult<()> = Ok(());
    if fb.obj[0].is_none() && (fb.format.num_planes > 1 || !fb.funcs.create_handle) { ret = Err(ENODEV); }
    if ret.is_ok() {
        request.height = fb.height;
        request.width = fb.width;
        request.pixel_format = fb.format.format;
        request.flags = if dev.fb_modifiers_not_supported { 0 } else { DRM_MODE_FB_MODIFIERS };
        request.handles = [0; 4]; request.pitches = [0; 4];
        request.offsets = [0; 4]; request.modifier = [0; 4];
        for i in 0..fb.format.num_planes {
            request.pitches[i] = fb.pitches[i];
            request.offsets[i] = fb.offsets[i];
            if !dev.fb_modifiers_not_supported { request.modifier[i] = fb.modifier; }
        }
        if !file.is_current_master && !file.has_sys_admin {
            // Match GET_FB by preserving zero handles for unprivileged clients.
        } else {
            for i in 0..fb.format.num_planes {
                if let Some(previous) = (0..i).find(|j| fb.obj[i] == fb.obj[*j]) {
                    request.handles[i] = request.handles[previous];
                }
                if request.handles[i] != 0 { continue; }
                let one = if let Some(object) = fb.obj[i] {
                    create_gem_handle(dev, file, object)
                } else {
                    if i > 0 { dev.warn("WARN_ON(i > 0) in GET_FB2".into()); }
                    create_handle_for_fb(dev, &fb, file)
                };
                match one { Ok(handle) => request.handles[i] = handle, Err(err) => { ret = Err(err); break; } }
            }
        }
    }
    if ret.is_err() {
        // Delete any new handles on failure and clear aliases of the deleted handle.
        for i in 0..4 {
            let handle = request.handles[i];
            if handle != 0 { delete_gem_handle(dev, file, handle); }
            for j in (i + 1)..4 { if request.handles[j] == handle { request.handles[j] = 0; } }
        }
    }
    framebuffer_put(dev, id);
    ret
}

// upstream: drm_framebuffer.c drm_mode_dirtyfb_ioctl()
pub fn drm_mode_dirtyfb_ioctl(dev: &mut Device, request: &ModeFbDirtyCmd, file: &DrmFile,
                              clips_pointer_present: bool, user_clips: Option<&[ClipRect]>,
                              copy_from_user_failed: bool) -> KResult<()> {
    check_modeset(dev, EOPNOTSUPP)?;
    let id = request.fb_id;
    if !lookup_fb(dev, id) { return Err(ENOENT); }
    let result = (|| {
        let num_clips = request.num_clips;
        if (num_clips == 0) != !clips_pointer_present { return Err(EINVAL); }
        let flags = DRM_MODE_FB_DIRTY_FLAGS & request.flags;
        if flags & DRM_MODE_FB_DIRTY_ANNOTATE_COPY != 0 && num_clips % 2 != 0 { return Err(EINVAL); }
        let mut clips = Vec::new();
        if num_clips != 0 && clips_pointer_present {
            if num_clips < 0 || num_clips > DRM_MODE_FB_DIRTY_MAX_CLIPS { return Err(EINVAL); }
            if copy_from_user_failed { return Err(EFAULT); }
            let Some(user) = user_clips else { return Err(EFAULT); };
            if user.len() < num_clips as usize { return Err(EFAULT); }
            clips.extend_from_slice(&user[..num_clips as usize]);
        }
        let fb = dev.fb(id).ok_or(ENOENT)?;
        if !fb.funcs.dirty { return Err(ENOSYS); }
        if dev.dirty_error != 0 { return Err(dev.dirty_error); }
        dev.dirty_calls.push((id, flags, request.color, clips, num_clips));
        let _ = file;
        Ok(())
    })();
    framebuffer_put(dev, id);
    result
}

// upstream: drm_framebuffer.c drm_fb_release()
pub fn drm_fb_release(dev: &mut Device, file: &mut DrmFile) {
    // File teardown excludes other access to this list, so no list mutex is taken.
    let owned = core::mem::take(&mut file.fbs);
    let mut deferred = Vec::new();
    for fb_id in owned {
        let refs = dev.fb(fb_id).map(|fb| fb.refcount).unwrap_or(0);
        if refs > 1 {
            deferred.push(fb_id);
        } else {
            if let Some(fb) = dev.fb_mut(fb_id) { fb.in_filp_list = false; }
            // Drops this file's fbs reference without taking modeset locks.
            framebuffer_put(dev, fb_id);
        }
    }
    if !deferred.is_empty() {
        for id in &deferred { if let Some(fb) = dev.fb_mut(*id) { fb.in_filp_list = false; } }
        drm_mode_rmfb_work_fn(dev, &mut deferred);
    }
}

// upstream: drm_framebuffer.c drm_framebuffer_free()
pub fn drm_framebuffer_free(dev: &mut Device, fb_id: FbId) {
    let Some(snapshot) = dev.fb(fb_id).cloned() else { return; };
    if snapshot.in_filp_list { dev.warn(format!("WARN_ON: [FB:{}] still on a file framebuffer list", fb_id)); }
    // The mode-object IDR is weak and may already have been unregistered.
    dev.registered_ids.retain(|id| *id != fb_id);
    // A framebuffer's destroy callback normally invokes drm_framebuffer_cleanup().
    drm_framebuffer_cleanup(dev, fb_id);
    if let Some(fb) = dev.fb_mut(fb_id) { fb.destroyed = true; }
    dev.framebuffers.retain(|fb| fb.id != fb_id);
}

// upstream: drm_framebuffer.c drm_framebuffer_init()
pub fn drm_framebuffer_init(dev: &mut Device, fb: &mut Framebuffer, comm: &str) -> KResult<()> {
    if fb.device_id != dev.device_id || fb.format.num_planes == 0 { return Err(EINVAL); }
    for i in 0..fb.format.num_planes {
        if fb.handle_ref[i] {
            dev.warn(format!("WARN_ON_ONCE: framebuffer handle-ref flag already set for plane {}", i));
            if let Some(object) = fb.obj[i] { drop_gem_handle_ref(dev, object); }
            fb.handle_ref[i] = false;
        }
        if let Some(object) = fb.obj[i] {
            // Preserve a userspace-visible GEM handle if one exists at init time.
            if dev.gem_objects.contains(&object) {
                add_gem_handle_ref(dev, object);
                fb.handle_ref[i] = true;
            }
        }
    }
    fb.in_filp_list = false;
    fb.comm = String::from(comm);
    if let Err(err) = register_fb(dev, fb) {
        for i in 0..fb.format.num_planes {
            if fb.handle_ref[i] {
                if let Some(object) = fb.obj[i] { drop_gem_handle_ref(dev, object); }
                fb.handle_ref[i] = false;
            }
        }
        return Err(err);
    }
    // Publish on mode_config.fb_list only after all invariant fields are set.
    dev.fb_lock_held = true;
    dev.num_fb = dev.num_fb.saturating_add(1);
    dev.fb_lock_held = false;
    Ok(())
}

// upstream: drm_framebuffer.c drm_framebuffer_lookup()
pub fn drm_framebuffer_lookup(dev: &mut Device, file: &DrmFile, id: FbId) -> Option<FbId> {
    if let Some(leased) = &file.leased_fbs { if !leased.contains(&id) { return None; } }
    if !dev.registered_ids.contains(&id) || !lookup_fb(dev, id) { return None; }
    Some(id)
}

// upstream: drm_framebuffer.c drm_framebuffer_unregister_private()
pub fn drm_framebuffer_unregister_private(dev: &mut Device, fb_id: Option<FbId>) {
    let Some(id) = fb_id else { return; };
    // Mark the private fb as reaped and drop its mode-object lookup reference.
    dev.registered_ids.retain(|registered| *registered != id);
}

// upstream: drm_framebuffer.c drm_framebuffer_cleanup()
pub fn drm_framebuffer_cleanup(dev: &mut Device, fb_id: FbId) {
    let Some(snapshot) = dev.fb(fb_id).cloned() else { return; };
    for i in 0..snapshot.format.num_planes {
        if snapshot.handle_ref[i] {
            if let Some(object) = snapshot.obj[i] { drop_gem_handle_ref(dev, object); }
            if let Some(fb) = dev.fb_mut(fb_id) { fb.handle_ref[i] = false; }
        }
    }
    if snapshot.listed {
        dev.fb_lock_held = true;
        if let Some(fb) = dev.fb_mut(fb_id) { fb.listed = false; }
        dev.num_fb = dev.num_fb.saturating_sub(1);
        dev.fb_lock_held = false;
    }
}

// upstream: drm_framebuffer.c atomic_remove_fb()
fn atomic_remove_fb(dev: &mut Device, fb_id: FbId) -> KResult<()> {
    if let Some(err) = dev.atomic_alloc_error.take() { return Err(err); }
    let mut disable_crtcs = false;
    loop {
        // DRM acquires an all-modeset context and allocates a fresh atomic state.
        let result = loop {
            let mut staged_planes: Vec<ObjectId> = Vec::new();
            let mut staged_crtcs: Vec<ObjectId> = Vec::new();
            let mut staged_connectors: Vec<ObjectId> = Vec::new();
            let mut plane_mask = 0u64;
            let mut ret = 0;
            if dev.atomic_lock_deadlocks != 0 {
                dev.atomic_lock_deadlocks -= 1;
                ret = EDEADLK;
            } else {
                dev.lock_held = true;
                let planes = dev.planes.clone();
                for plane in &planes {
                    if plane.fb != Some(fb_id) { continue; }
                    dev.log(format!("Disabling [PLANE:{}:{}] because [FB:{}] is removed", plane.id, plane.name, fb_id));
                    if let Some(err) = dev.atomic_plane_state_error { ret = err; break; }
                    if disable_crtcs {
                        if let Some(crtc_id) = plane.crtc {
                            if dev.crtcs.iter().any(|crtc| crtc.id == crtc_id && crtc.primary == Some(plane.id)) {
                                dev.log(format!("Disabling [CRTC:{}] because [FB:{}] is removed", crtc_id, fb_id));
                                if dev.atomic_connector_error != 0 { ret = dev.atomic_connector_error; break; }
                                staged_crtcs.push(crtc_id);
                                staged_connectors.extend(dev.connectors.iter()
                                    .filter(|connector| connector.crtc == Some(crtc_id)).map(|connector| connector.id));
                            }
                        }
                    }
                    staged_planes.push(plane.id);
                    plane_mask |= 1u64.checked_shl((plane.id & 63) as u32).unwrap_or(0);
                }
                // drm_atomic_set_crtc_for_connector() runs for each newly affected connector.
                if ret == 0 && !staged_connectors.is_empty() && dev.atomic_connector_error != 0 {
                    ret = dev.atomic_connector_error;
                }
                if ret == 0 && plane_mask != 0 {
                    dev.atomic_commit_count = dev.atomic_commit_count.saturating_add(1);
                    ret = dev.atomic_commit_error;
                    if ret == 0 {
                        let mut removed_fb_refs = 0usize;
                        for plane_id in &staged_planes {
                            if let Some(plane) = dev.planes.iter_mut().find(|plane| plane.id == *plane_id) {
                                if plane.fb == Some(fb_id) { removed_fb_refs += 1; }
                                plane.fb = None;
                                plane.crtc = None;
                            }
                        }
                        for crtc_id in &staged_crtcs {
                            if let Some(crtc) = dev.crtcs.iter_mut().find(|crtc| crtc.id == *crtc_id) { crtc.active = false; }
                        }
                        for connector_id in &staged_connectors {
                            if let Some(connector) = dev.connectors.iter_mut().find(|connector| connector.id == *connector_id) {
                                connector.crtc = None;
                            }
                        }
                        for _ in 0..removed_fb_refs { framebuffer_put(dev, fb_id); }
                    }
                }
                dev.lock_held = false;
            }
            if ret == EDEADLK {
                // drm_atomic_commit_clear(), drm_modeset_backoff(), and retry.
                dev.lock_held = false;
                continue;
            }
            break if ret == 0 { Ok(()) } else { Err(ret) };
        };
        if result == Err(EINVAL) && !disable_crtcs {
            disable_crtcs = true;
            continue;
        }
        dev.lock_held = false;
        return result;
    }
}

// upstream: drm_framebuffer.c legacy_remove_fb()
fn legacy_remove_fb(dev: &mut Device, fb_id: FbId) {
    dev.lock_held = true;
    let crtcs = dev.crtcs.clone();
    for crtc in crtcs {
        let primary_uses_fb = crtc.primary.and_then(|id| dev.planes.iter().find(|p| p.id == id))
            .map(|plane| plane.fb == Some(fb_id)).unwrap_or(false);
        if primary_uses_fb {
            dev.log(format!("Disabling [CRTC:{}:{}] because [FB:{}] is removed", crtc.id, crtc.name, fb_id));
            if let Some(live) = dev.crtcs.iter_mut().find(|candidate| candidate.id == crtc.id) { live.active = false; }
            if let Some(primary) = crtc.primary {
                if let Some(plane) = dev.planes.iter_mut().find(|candidate| candidate.id == primary) {
                    plane.fb = None;
                    plane.crtc = None;
                    framebuffer_put(dev, fb_id);
                }
            }
        }
    }
    let planes = dev.planes.clone();
    for snapshot in planes {
        if snapshot.fb == Some(fb_id) {
            dev.log(format!("Disabling [PLANE:{}:{}] because [FB:{}] is removed", snapshot.id, snapshot.name, fb_id));
            if let Some(plane) = dev.planes.iter_mut().find(|plane| plane.id == snapshot.id) {
                plane.fb = None;
                plane.crtc = None;
                framebuffer_put(dev, fb_id);
            }
        }
    }
    dev.lock_held = false;
}

// upstream: drm_framebuffer.c drm_framebuffer_remove()
pub fn drm_framebuffer_remove(dev: &mut Device, fb_id: Option<FbId>) {
    let Some(id) = fb_id else { return; };
    let Some(snapshot) = dev.fb(id).cloned() else { return; };
    if snapshot.in_filp_list { dev.warn(format!("WARN_ON: [FB:{}] is still on a per-file list", id)); }
    // A last-reference cleanup cannot race back to a new reference and needs no modeset locks.
    if snapshot.refcount > 1 {
        if dev.atomic_modeset {
            if let Err(ret) = atomic_remove_fb(dev, id) {
                dev.warn(format!("atomic remove_fb failed with {}", ret));
            }
        } else {
            legacy_remove_fb(dev, id);
        }
    }
    framebuffer_put(dev, id);
}

// upstream: drm_framebuffer.c drm_framebuffer_print_info()
pub fn drm_framebuffer_print_info(dev: &Device, indent: usize, fb_id: FbId) -> String {
    let Some(fb) = dev.fb(fb_id) else { return String::new(); };
    let mut out = String::new();
    let pad = " ".repeat(indent);
    out.push_str(&format!("{}allocated by = {}\n", pad, fb.comm));
    out.push_str(&format!("{}refcount={}\n", pad, fb.refcount));
    out.push_str(&format!("{}format=0x{:08x}\n", pad, fb.format.format));
    out.push_str(&format!("{}modifier=0x{:016x}\n", pad, fb.modifier));
    out.push_str(&format!("{}size={}x{}\n", pad, fb.width, fb.height));
    out.push_str(&format!("{}layers:\n", pad));
    for i in 0..fb.format.num_planes {
        let layer_pad = " ".repeat(indent + 1);
        out.push_str(&format!("{}size[{}]={}x{}\n", layer_pad, i,
            format_plane_width(&fb.format, fb.width, i), format_plane_height(&fb.format, fb.height, i)));
        out.push_str(&format!("{}pitch[{}]={}\n", layer_pad, i, fb.pitches[i]));
        out.push_str(&format!("{}offset[{}]={}\n", layer_pad, i, fb.offsets[i]));
        out.push_str(&format!("{}obj[{}]:{}\n", layer_pad, i, if fb.obj[i].is_some() { "" } else { "(null)" }));
        if let Some(object) = fb.obj[i] {
            out.push_str(&format!("{}gem object {}\n", " ".repeat(indent + 2), object));
        }
    }
    out
}

// upstream: drm_framebuffer.c drm_framebuffer_info()
pub fn drm_framebuffer_info(dev: &mut Device) -> String {
    let ids: Vec<FbId> = dev.framebuffers.iter().filter(|fb| fb.listed).map(|fb| fb.id).collect();
    dev.fb_lock_held = true;
    let mut output = String::new();
    for id in ids {
        output.push_str(&format!("framebuffer[{}]:\n", id));
        output.push_str(&drm_framebuffer_print_info(dev, 1, id));
    }
    dev.fb_lock_held = false;
    output
}

// upstream: drm_framebuffer.c drm_framebuffer_debugfs_init()
pub fn drm_framebuffer_debugfs_init(dev: &mut Device) {
    if dev.debugfs_enabled { dev.debugfs_registered = true; }
}
