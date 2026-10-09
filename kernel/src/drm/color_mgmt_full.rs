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

//! Function-level Rust translation of Linux v7.2.3 `drm_color_mgmt.c`.
//! DRM object/property registration, atomic helpers, locking, diagnostics, and
//! userspace memory access remain explicit trait boundaries.

extern crate alloc;
use alloc::vec::Vec;
use core::{convert::TryFrom, mem::size_of};

const EFAULT: i32 = -14;
const EINVAL: i32 = -22;
const ENODEV: i32 = -19;
const ENOENT: i32 = -2;
const ENOMEM: i32 = -12;
const ENOSYS: i32 = -38;
const EOPNOTSUPP: i32 = -95;
const DRM_MODE_PROP_ENUM: u32 = 1 << 3;
const DRM_COLOR_ENCODING_MAX: u32 = 3;
const DRM_COLOR_RANGE_MAX: u32 = 2;
pub const DRM_COLOR_YCBCR_BT601: u32 = 0;
pub const DRM_COLOR_YCBCR_BT709: u32 = 1;
pub const DRM_COLOR_YCBCR_BT2020: u32 = 2;
pub const DRM_COLOR_YCBCR_FULL_RANGE: u32 = 0;
pub const DRM_COLOR_YCBCR_LIMITED_RANGE: u32 = 1;
pub const DRM_COLOR_LUT_EQUAL_CHANNELS: u32 = 1 << 0;
pub const DRM_COLOR_LUT_NON_DECREASING: u32 = 1 << 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColorLut {
    pub red: u16,
    pub green: u16,
    pub blue: u16,
    pub reserved: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColorLut32 {
    pub red: u32,
    pub green: u32,
    pub blue: u32,
    pub reserved: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColorCtm {
    pub matrix: [u64; 9],
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ModeConfig {
    pub degamma_lut_property: u32,
    pub degamma_lut_size_property: u32,
    pub ctm_property: u32,
    pub gamma_lut_property: u32,
    pub gamma_lut_size_property: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DrmDevice {
    pub id: u32,
    pub modeset: bool,
    pub mode_config: ModeConfig,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DrmFile {
    pub id: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Crtc {
    pub id: u32,
    pub device_id: u32,
    pub gamma_size: i32,
    /// Three adjacent channels, each of `gamma_size` native-endian u16s.
    pub gamma_store: Vec<u16>,
    pub has_gamma_set: bool,
    pub attached_properties: Vec<u32>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CrtcColorState {
    pub color_encoding: u32,
    pub color_range: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Plane {
    pub id: u32,
    pub device_id: u32,
    pub color_encoding_property: Option<u32>,
    pub color_range_property: Option<u32>,
    pub state: Option<CrtcColorState>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DrmModeCrtcLut {
    pub crtc_id: u32,
    pub gamma_size: u32,
    pub red: u64,
    pub green: u64,
    pub blue: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LockContext {
    pub id: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PropEnum {
    pub value: u32,
    pub name: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PropertyBlob {
    pub id: u64,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct AtomicCrtcState {
    pub degamma_lut: Option<PropertyBlob>,
    pub ctm: Option<PropertyBlob>,
    pub gamma_lut: Option<PropertyBlob>,
    pub color_mgmt_changed: bool,
}

#[derive(Clone, Debug, Default)]
pub struct AtomicCommit {
    pub id: u64,
    pub acquire_ctx: Option<u64>,
    pub crtc_state: Option<AtomicCrtcState>,
}

/// Kernel-side property registration / object lookup boundary.
pub trait DrmPropertyOps {
    fn attach_property(&mut self, object_id: u32, property_id: u32, value: u64);
    fn create_enum_property(
        &mut self,
        device_id: u32,
        flags: u32,
        name: &'static str,
        values: &[PropEnum],
    ) -> Option<u32>;
    fn object_has_property(&self, object_id: u32, property_id: u32) -> bool;
    fn find_crtc(&self, device_id: u32, file_id: u32, crtc_id: u32, resolved_id: u32) -> bool;
    fn warn_on(&mut self, condition: bool);
    fn debug_kms(&mut self, message: &'static str);
}

/// Userspace pointer copy boundary. Return the number of bytes not copied,
/// matching `copy_{from,to}_user()` including its partial-copy behavior.
pub trait UserMemoryOps {
    fn copy_from_user(&mut self, user_address: u64, destination: &mut [u8]) -> usize;
    fn copy_to_user(&mut self, user_address: u64, source: &[u8]) -> usize;
}

/// DRM modeset-lock acquisition and release boundary.
pub trait ModesetLockOps {
    fn lock_all_begin(&mut self, device_id: u32, ctx: &mut LockContext) -> i32;
    fn lock_all_end(&mut self, device_id: u32, ctx: &mut LockContext, ret: i32);
    fn lock_crtc_mutex(&mut self, crtc_id: u32);
    fn unlock_crtc_mutex(&mut self, crtc_id: u32);
    fn gamma_set(
        &mut self,
        crtc_id: u32,
        red: &[u16],
        green: &[u16],
        blue: &[u16],
        size: u32,
        ctx: &LockContext,
    ) -> i32;
}

/// Atomic commit / blob reference-management boundary.
pub trait AtomicColorOps {
    fn atomic_commit_alloc(&mut self, device_id: u32) -> Option<AtomicCommit>;
    fn create_property_blob(&mut self, device_id: u32, byte_len: usize)
    -> Result<PropertyBlob, i32>;
    fn atomic_get_crtc_state<'a>(
        &mut self,
        commit: &'a mut AtomicCommit,
        crtc_id: u32,
    ) -> Result<&'a mut AtomicCrtcState, i32>;
    fn property_replace_blob(
        &mut self,
        current: &mut Option<PropertyBlob>,
        replacement: Option<PropertyBlob>,
    ) -> bool;
    fn atomic_commit(&mut self, commit: &mut AtomicCommit) -> i32;
    fn atomic_commit_put(&mut self, commit: AtomicCommit);
    fn property_blob_put(&mut self, blob: Option<PropertyBlob>);
}

/// Hardware LUT programming callback boundary.
pub trait LutWriter {
    fn set_lut(&mut self, crtc_id: u32, index: u32, red: u16, green: u16, blue: u16);
}

#[inline]
fn u16s_to_ne_bytes(values: &[u16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(values.len() * 2);
    for value in values {
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    bytes
}

#[inline]
fn ne_bytes_to_u16s(bytes: &[u8], destination: &mut [u16]) {
    for (word, pair) in destination.iter_mut().zip(bytes.chunks_exact(2)) {
        *word = u16::from_ne_bytes([pair[0], pair[1]]);
    }
}

#[inline]
fn lut16_from_bytes(bytes: &[u8]) -> ColorLut {
    ColorLut {
        red: u16::from_ne_bytes([bytes[0], bytes[1]]),
        green: u16::from_ne_bytes([bytes[2], bytes[3]]),
        blue: u16::from_ne_bytes([bytes[4], bytes[5]]),
        reserved: u16::from_ne_bytes([bytes[6], bytes[7]]),
    }
}

#[inline]
fn lut32_from_bytes(bytes: &[u8]) -> ColorLut32 {
    ColorLut32 {
        red: u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        green: u32::from_ne_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        blue: u32::from_ne_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        reserved: u32::from_ne_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
    }
}

// upstream: drm_color_mgmt.c drm_color_ctm_s31_32_to_qm_n()
pub fn drm_color_ctm_s31_32_to_qm_n<O: DrmPropertyOps>(
    ops: &mut O,
    user_input: u64,
    m: u32,
    n: u32,
) -> u64 {
    let invalid = m > 32 || n > 32 || m + n == 0;
    ops.warn_on(invalid);
    // The C helper's input domain is m,n <= 32 with at least one value bit.
    // Saturate invalid dimensions to keep Rust shifts defined while retaining
    // the source operation exactly for every documented input.
    if invalid {
        return 0;
    }
    let shift = 32 - n;
    let mag = (user_input & !(1u64 << 63)) >> shift;
    let negative = (user_input & (1u64 << 63)) != 0;
    let value_bits = m + n - 1;
    let max_magnitude = 1u128 << value_bits;
    let max_value = if negative {
        max_magnitude
    } else {
        max_magnitude - 1
    };
    let magnitude = (mag as u128).min(max_value);
    let signed = if negative {
        -(magnitude as i128)
    } else {
        magnitude as i128
    };
    signed as u64
}

// upstream: drm_color_mgmt.c drm_crtc_enable_color_mgmt()
pub fn drm_crtc_enable_color_mgmt<O: DrmPropertyOps>(
    ops: &mut O,
    crtc: &Crtc,
    config: &ModeConfig,
    degamma_lut_size: u32,
    has_ctm: bool,
    gamma_lut_size: u32,
) {
    if degamma_lut_size != 0 {
        ops.attach_property(crtc.id, config.degamma_lut_property, 0);
        ops.attach_property(
            crtc.id,
            config.degamma_lut_size_property,
            degamma_lut_size as u64,
        );
    }

    if has_ctm {
        ops.attach_property(crtc.id, config.ctm_property, 0);
    }

    if gamma_lut_size != 0 {
        ops.attach_property(crtc.id, config.gamma_lut_property, 0);
        ops.attach_property(
            crtc.id,
            config.gamma_lut_size_property,
            gamma_lut_size as u64,
        );
    }
}

// upstream: drm_color_mgmt.c drm_mode_crtc_set_gamma_size()
pub fn drm_mode_crtc_set_gamma_size(crtc: &mut Crtc, gamma_size: i32) -> i32 {
    crtc.gamma_size = gamma_size;
    let Some(count) = usize::try_from(gamma_size).ok().and_then(|n| n.checked_mul(3)) else {
        crtc.gamma_size = 0;
        crtc.gamma_store.clear();
        return ENOMEM;
    };
    let mut storage = Vec::new();
    if storage.try_reserve_exact(count).is_err() {
        crtc.gamma_size = 0;
        crtc.gamma_store.clear();
        return ENOMEM;
    }
    storage.resize(count, 0u16);
    crtc.gamma_store = storage;

    let size = gamma_size as usize;
    let (red, rest) = crtc.gamma_store.split_at_mut(size);
    let (green, blue) = rest.split_at_mut(size);
    for i in 0..size {
        red[i] = (i << 8) as u16;
        green[i] = (i << 8) as u16;
        blue[i] = (i << 8) as u16;
    }

    0
}

// upstream: drm_color_mgmt.c drm_crtc_supports_legacy_gamma()
fn drm_crtc_supports_legacy_gamma<O: DrmPropertyOps>(
    ops: &O,
    crtc: &Crtc,
    config: &ModeConfig,
) -> bool {
    if crtc.gamma_size == 0 {
        return false;
    }

    if crtc.has_gamma_set {
        return true;
    }

    ops.object_has_property(crtc.id, config.gamma_lut_property)
        || ops.object_has_property(crtc.id, config.degamma_lut_property)
}

// upstream: drm_color_mgmt.c drm_crtc_legacy_gamma_set()
fn drm_crtc_legacy_gamma_set<O: DrmPropertyOps + AtomicColorOps + ModesetLockOps>(
    ops: &mut O,
    crtc: &Crtc,
    config: &ModeConfig,
    red: &[u16],
    green: &[u16],
    blue: &[u16],
    size: u32,
    ctx: &LockContext,
) -> i32 {
    if crtc.has_gamma_set {
        return ops.gamma_set(crtc.id, red, green, blue, size, ctx);
    }

    let use_gamma_lut = if ops.object_has_property(crtc.id, config.gamma_lut_property) {
        true
    } else if ops.object_has_property(crtc.id, config.degamma_lut_property) {
        false
    } else {
        return ENODEV;
    };

    let Some(mut commit) = ops.atomic_commit_alloc(crtc.device_id) else {
        return ENOMEM;
    };
    let byte_len = match (size as usize).checked_mul(size_of::<ColorLut>()) {
        Some(len) => len,
        None => {
            ops.atomic_commit_put(commit);
            return ENOMEM;
        }
    };
    let mut blob = match ops.create_property_blob(crtc.device_id, byte_len) {
        Ok(blob) => Some(blob),
        Err(err) => {
            ops.atomic_commit_put(commit);
            return err;
        }
    };

    // Prepare GAMMA_LUT with the legacy values.
    if let Some(blob) = blob.as_mut() {
        for i in 0..size as usize {
            let offset = i * size_of::<ColorLut>();
            let values = [red[i], green[i], blue[i], 0];
            for (component, value) in values.iter().enumerate() {
                blob.data[offset + component * 2..offset + component * 2 + 2]
                    .copy_from_slice(&value.to_ne_bytes());
            }
        }
    }

    commit.acquire_ctx = Some(ctx.id);
    let state = match ops.atomic_get_crtc_state(&mut commit, crtc.id) {
        Ok(state) => state,
        Err(err) => {
            ops.atomic_commit_put(commit);
            ops.property_blob_put(blob);
            return err;
        }
    };

    // Set GAMMA_LUT and reset DEGAMMA_LUT and CTM.
    let replaced = ops.property_replace_blob(
        &mut state.degamma_lut,
        if use_gamma_lut { None } else { blob.clone() },
    );
    let replaced_ctm = ops.property_replace_blob(&mut state.ctm, None);
    let replaced_gamma = ops.property_replace_blob(
        &mut state.gamma_lut,
        if use_gamma_lut { blob.clone() } else { None },
    );
    state.color_mgmt_changed |= replaced | replaced_ctm | replaced_gamma;

    let ret = ops.atomic_commit(&mut commit);
    ops.atomic_commit_put(commit);
    ops.property_blob_put(blob);
    ret
}

// upstream: drm_color_mgmt.c drm_mode_gamma_set_ioctl()
pub fn drm_mode_gamma_set_ioctl<O, U>(
    ops: &mut O,
    user_memory: &mut U,
    dev: &DrmDevice,
    data: &DrmModeCrtcLut,
    file_priv: &DrmFile,
    crtc: &mut Crtc,
) -> i32
where
    O: DrmPropertyOps + ModesetLockOps + AtomicColorOps,
    U: UserMemoryOps,
{
    if !dev.modeset {
        return EOPNOTSUPP;
    }

    if !ops.find_crtc(dev.id, file_priv.id, data.crtc_id, crtc.id) || data.crtc_id != crtc.id {
        return ENOENT;
    }

    if !drm_crtc_supports_legacy_gamma(ops, crtc, &dev.mode_config) {
        return ENOSYS;
    }

    // memcpy into gamma store.
    if data.gamma_size != crtc.gamma_size as u32 {
        return EINVAL;
    }

    let mut ctx = LockContext::default();
    let mut ret = ops.lock_all_begin(dev.id, &mut ctx);
    if ret == 0 {
        let size = data.gamma_size as usize * size_of::<u16>();
        let channel_size = data.gamma_size as usize;
        for (address, start) in [(data.red, 0usize), (data.green, channel_size), (data.blue, channel_size * 2)] {
            let end = start + channel_size;
            let mut bytes = u16s_to_ne_bytes(&crtc.gamma_store[start..end]);
            let not_copied = user_memory.copy_from_user(address, &mut bytes);
            ne_bytes_to_u16s(&bytes[..size], &mut crtc.gamma_store[start..end]);
            if not_copied != 0 {
                ret = EFAULT;
                break;
            }
        }

        if ret == 0 {
            let gamma_size = crtc.gamma_size as usize;
            let red = &crtc.gamma_store[..gamma_size];
            let green = &crtc.gamma_store[gamma_size..2 * gamma_size];
            let blue = &crtc.gamma_store[2 * gamma_size..3 * gamma_size];
            ret = drm_crtc_legacy_gamma_set(
                ops,
                crtc,
                &dev.mode_config,
                red,
                green,
                blue,
                crtc.gamma_size as u32,
                &ctx,
            );
        }
    }
    ops.lock_all_end(dev.id, &mut ctx, ret);
    ret
}

// upstream: drm_color_mgmt.c drm_mode_gamma_get_ioctl()
pub fn drm_mode_gamma_get_ioctl<O, U>(
    ops: &mut O,
    user_memory: &mut U,
    dev: &DrmDevice,
    data: &DrmModeCrtcLut,
    file_priv: &DrmFile,
    crtc: &mut Crtc,
) -> i32
where
    O: DrmPropertyOps + ModesetLockOps,
    U: UserMemoryOps,
{
    if !dev.modeset {
        return EOPNOTSUPP;
    }

    if !ops.find_crtc(dev.id, file_priv.id, data.crtc_id, crtc.id) || data.crtc_id != crtc.id {
        return ENOENT;
    }

    // memcpy into gamma store.
    if data.gamma_size != crtc.gamma_size as u32 {
        return EINVAL;
    }

    ops.lock_crtc_mutex(crtc.id);
    let size = data.gamma_size as usize * size_of::<u16>();
    let channel_size = data.gamma_size as usize;
    let mut ret = 0;
    for (address, start) in [(data.red, 0usize), (data.green, channel_size), (data.blue, channel_size * 2)] {
        let end = start + channel_size;
        let bytes = u16s_to_ne_bytes(&crtc.gamma_store[start..end]);
        if user_memory.copy_to_user(address, &bytes[..size]) != 0 {
            ret = EFAULT;
            break;
        }
    }
    ops.unlock_crtc_mutex(crtc.id);
    ret
}

const COLOR_ENCODING_NAME: [&str; DRM_COLOR_ENCODING_MAX as usize] = [
    "ITU-R BT.601 YCbCr",
    "ITU-R BT.709 YCbCr",
    "ITU-R BT.2020 YCbCr",
];
const COLOR_RANGE_NAME: [&str; DRM_COLOR_RANGE_MAX as usize] = [
    "YCbCr full range",
    "YCbCr limited range",
];

// upstream: drm_color_mgmt.c drm_get_color_encoding_name()
pub fn drm_get_color_encoding_name<O: DrmPropertyOps>(ops: &mut O, encoding: u32) -> &'static str {
    if encoding as usize >= COLOR_ENCODING_NAME.len() {
        ops.warn_on(true);
        return "unknown";
    }

    COLOR_ENCODING_NAME[encoding as usize]
}

// upstream: drm_color_mgmt.c drm_get_color_range_name()
pub fn drm_get_color_range_name<O: DrmPropertyOps>(ops: &mut O, range: u32) -> &'static str {
    if range as usize >= COLOR_RANGE_NAME.len() {
        ops.warn_on(true);
        return "unknown";
    }

    COLOR_RANGE_NAME[range as usize]
}

// upstream: drm_color_mgmt.c drm_plane_create_color_properties()
pub fn drm_plane_create_color_properties<O: DrmPropertyOps>(
    ops: &mut O,
    plane: &mut Plane,
    supported_encodings: u32,
    supported_ranges: u32,
    default_encoding: u32,
    default_range: u32,
) -> i32 {
    let max_encoding_bit = 1u32 << DRM_COLOR_ENCODING_MAX;
    if supported_encodings == 0
        || (supported_encodings & 0u32.wrapping_sub(max_encoding_bit)) != 0
        || default_encoding >= DRM_COLOR_ENCODING_MAX
        || (supported_encodings & (1u32 << default_encoding)) == 0
    {
        ops.warn_on(true);
        return EINVAL;
    }

    let max_range_bit = 1u32 << DRM_COLOR_RANGE_MAX;
    if supported_ranges == 0
        || (supported_ranges & 0u32.wrapping_sub(max_range_bit)) != 0
        || default_range >= DRM_COLOR_RANGE_MAX
        || (supported_ranges & (1u32 << default_range)) == 0
    {
        ops.warn_on(true);
        return EINVAL;
    }

    let mut enum_list = [PropEnum::default(); 3];
    let mut len = 0usize;
    for i in 0..DRM_COLOR_ENCODING_MAX {
        if (supported_encodings & (1u32 << i)) == 0 {
            continue;
        }
        enum_list[len].value = i;
        enum_list[len].name = COLOR_ENCODING_NAME[i as usize];
        len += 1;
    }

    let Some(prop) = ops.create_enum_property(
        plane.device_id,
        0,
        "COLOR_ENCODING",
        &enum_list[..len],
    ) else {
        return ENOMEM;
    };
    plane.color_encoding_property = Some(prop);
    ops.attach_property(plane.id, prop, default_encoding as u64);
    if let Some(state) = plane.state.as_mut() {
        state.color_encoding = default_encoding;
    }

    len = 0;
    for i in 0..DRM_COLOR_RANGE_MAX {
        if (supported_ranges & (1u32 << i)) == 0 {
            continue;
        }
        enum_list[len].value = i;
        enum_list[len].name = COLOR_RANGE_NAME[i as usize];
        len += 1;
    }

    let Some(prop) = ops.create_enum_property(
        plane.device_id,
        0,
        "COLOR_RANGE",
        &enum_list[..len],
    ) else {
        return ENOMEM;
    };
    plane.color_range_property = Some(prop);
    ops.attach_property(plane.id, prop, default_range as u64);
    if let Some(state) = plane.state.as_mut() {
        state.color_range = default_range;
    }

    0
}

// upstream: drm_color_mgmt.c drm_color_lut_check()
pub fn drm_color_lut_check<O: DrmPropertyOps>(ops: &mut O, lut: Option<&PropertyBlob>, tests: u32) -> i32 {
    let Some(lut) = lut else {
        return 0;
    };
    if tests == 0 {
        return 0;
    }

    let count = lut.data.len() / size_of::<ColorLut>();
    for i in 0..count {
        let offset = i * size_of::<ColorLut>();
        let entry = lut16_from_bytes(&lut.data[offset..offset + 8]);
        if tests & DRM_COLOR_LUT_EQUAL_CHANNELS != 0
            && (entry.red != entry.blue || entry.red != entry.green)
        {
            ops.debug_kms("All LUT entries must have equal r/g/b\n");
            return EINVAL;
        }

        if i > 0 && tests & DRM_COLOR_LUT_NON_DECREASING != 0 {
            let previous = lut16_from_bytes(&lut.data[offset - 8..offset]);
            if entry.red < previous.red || entry.green < previous.green || entry.blue < previous.blue {
                ops.debug_kms("LUT entries must never decrease.\n");
                return EINVAL;
            }
        }
    }

    0
}

// upstream: drm_color_mgmt.c drm_crtc_load_gamma_888()
pub fn drm_crtc_load_gamma_888<W: LutWriter>(crtc: &Crtc, lut: &[ColorLut], set_gamma: &mut W) {
    for i in 0..256usize {
        set_gamma.set_lut(crtc.id, i as u32, lut[i].red, lut[i].green, lut[i].blue);
    }
}

// upstream: drm_color_mgmt.c drm_crtc_load_gamma_565_from_888()
pub fn drm_crtc_load_gamma_565_from_888<W: LutWriter>(
    crtc: &Crtc,
    lut: &[ColorLut],
    set_gamma: &mut W,
) {
    for i in 0..32usize {
        let r = lut[i * 8 + i / 4].red;
        let g = lut[i * 4 + i / 16].green;
        let b = lut[i * 8 + i / 4].blue;
        set_gamma.set_lut(crtc.id, i as u32, r, g, b);
    }
    // Green has one more bit, so add padding with 0 for red and blue.
    for i in 32..64usize {
        let g = lut[i * 4 + i / 16].green;
        set_gamma.set_lut(crtc.id, i as u32, 0, g, 0);
    }
}

// upstream: drm_color_mgmt.c drm_crtc_load_gamma_555_from_888()
pub fn drm_crtc_load_gamma_555_from_888<W: LutWriter>(
    crtc: &Crtc,
    lut: &[ColorLut],
    set_gamma: &mut W,
) {
    for i in 0..32usize {
        let r = lut[i * 8 + i / 4].red;
        let g = lut[i * 8 + i / 4].green;
        let b = lut[i * 8 + i / 4].blue;
        set_gamma.set_lut(crtc.id, i as u32, r, g, b);
    }
}

// upstream: drm_color_mgmt.c fill_gamma_888()
fn fill_gamma_888<W: LutWriter>(crtc: &Crtc, i: u32, mut r: u16, mut g: u16, mut b: u16, set_gamma: &mut W) {
    r = (r << 8) | r;
    g = (g << 8) | g;
    b = (b << 8) | b;
    set_gamma.set_lut(crtc.id, i, r, g, b);
}

// upstream: drm_color_mgmt.c drm_crtc_fill_gamma_888()
pub fn drm_crtc_fill_gamma_888<W: LutWriter>(crtc: &Crtc, set_gamma: &mut W) {
    for i in 0..256u32 {
        fill_gamma_888(crtc, i, i as u16, i as u16, i as u16, set_gamma);
    }
}

// upstream: drm_color_mgmt.c fill_gamma_565()
fn fill_gamma_565<W: LutWriter>(crtc: &Crtc, i: u32, mut r: u16, mut g: u16, mut b: u16, set_gamma: &mut W) {
    r = (r << 11) | (r << 6) | (r << 1) | (r >> 4);
    g = (g << 10) | (g << 4) | (g >> 2);
    b = (b << 11) | (b << 6) | (b << 1) | (b >> 4);
    set_gamma.set_lut(crtc.id, i, r, g, b);
}

// upstream: drm_color_mgmt.c drm_crtc_fill_gamma_565()
pub fn drm_crtc_fill_gamma_565<W: LutWriter>(crtc: &Crtc, set_gamma: &mut W) {
    for i in 0..32u32 {
        fill_gamma_565(crtc, i, i as u16, i as u16, i as u16, set_gamma);
    }
    // Green has one more bit, so add padding with 0 for red and blue.
    for i in 32..64u32 {
        fill_gamma_565(crtc, i, 0, i as u16, 0, set_gamma);
    }
}

// upstream: drm_color_mgmt.c fill_gamma_555()
fn fill_gamma_555<W: LutWriter>(crtc: &Crtc, i: u32, mut r: u16, mut g: u16, mut b: u16, set_gamma: &mut W) {
    r = (r << 11) | (r << 6) | (r << 1) | (r >> 4);
    g = (g << 11) | (g << 6) | (g << 1) | (g >> 4);
    // Preserve the upstream expression: its final term uses `r`.
    b = (b << 11) | (b << 6) | (b << 1) | (r >> 4);
    set_gamma.set_lut(crtc.id, i, r, g, b);
}

// upstream: drm_color_mgmt.c drm_crtc_fill_gamma_555()
pub fn drm_crtc_fill_gamma_555<W: LutWriter>(crtc: &Crtc, set_gamma: &mut W) {
    for i in 0..32u32 {
        fill_gamma_555(crtc, i, i as u16, i as u16, i as u16, set_gamma);
    }
}

// upstream: drm_color_mgmt.c drm_crtc_load_palette_8()
pub fn drm_crtc_load_palette_8<W: LutWriter>(crtc: &Crtc, lut: &[ColorLut], set_palette: &mut W) {
    for i in 0..256usize {
        set_palette.set_lut(crtc.id, i as u32, lut[i].red, lut[i].green, lut[i].blue);
    }
}

// upstream: drm_color_mgmt.c fill_palette_332()
fn fill_palette_332<W: LutWriter>(crtc: &Crtc, r: u16, g: u16, b: u16, set_palette: &mut W) {
    let i = (r << 5) | (g << 2) | b; // 8-bit palette index

    // Expand R (3-bit) G (3-bit) and B (2-bit) values to 16-bit values.
    let r = (r << 13) | (r << 10) | (r << 7) | (r << 4) | (r << 1) | (r >> 2);
    let g = (g << 13) | (g << 10) | (g << 7) | (g << 4) | (g << 1) | (g >> 2);
    let b = (b << 14) | (b << 12) | (b << 10) | (b << 8) | (b << 6) | (b << 4) | (b << 2) | b;

    set_palette.set_lut(crtc.id, i as u32, r, g, b);
}

// upstream: drm_color_mgmt.c drm_crtc_fill_palette_332()
pub fn drm_crtc_fill_palette_332<W: LutWriter>(crtc: &Crtc, set_palette: &mut W) {
    for r in 0..8u16 {
        for g in 0..8u16 {
            for b in 0..4u16 {
                fill_palette_332(crtc, r, g, b, set_palette);
            }
        }
    }
}

// upstream: drm_color_mgmt.c fill_palette_8()
fn fill_palette_8<W: LutWriter>(crtc: &Crtc, i: u32, set_palette: &mut W) {
    let y = ((i << 8) | i) as u16; // relative luminance
    set_palette.set_lut(crtc.id, i, y, y, y);
}

// upstream: drm_color_mgmt.c drm_crtc_fill_palette_8()
pub fn drm_crtc_fill_palette_8<W: LutWriter>(crtc: &Crtc, set_palette: &mut W) {
    for i in 0..256u32 {
        fill_palette_8(crtc, i, set_palette);
    }
}

// upstream: drm_color_mgmt.c drm_color_lut32_check()
pub fn drm_color_lut32_check<O: DrmPropertyOps>(ops: &mut O, lut: Option<&PropertyBlob>, tests: u32) -> i32 {
    let Some(lut) = lut else {
        return 0;
    };
    if tests == 0 {
        return 0;
    }

    let count = lut.data.len() / size_of::<ColorLut32>();
    for i in 0..count {
        let offset = i * size_of::<ColorLut32>();
        let entry = lut32_from_bytes(&lut.data[offset..offset + 16]);
        if tests & DRM_COLOR_LUT_EQUAL_CHANNELS != 0
            && (entry.red != entry.blue || entry.red != entry.green)
        {
            ops.debug_kms("All LUT entries must have equal r/g/b\n");
            return EINVAL;
        }

        if i > 0 && tests & DRM_COLOR_LUT_NON_DECREASING != 0 {
            let previous = lut32_from_bytes(&lut.data[offset - 16..offset]);
            if entry.red < previous.red || entry.green < previous.green || entry.blue < previous.blue {
                ops.debug_kms("LUT entries must never decrease.\n");
                return EINVAL;
            }
        }
    }

    0
}
