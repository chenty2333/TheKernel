// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_eld.c` (batch B1b).
//!
//! Source-faithful transcription of Linux 7.2.3. The ELD header inlines from
//! `include/drm/drm_eld.h` are provided as private helpers. The CTA SAD
//! conversion is owned by the EDID batch and is declared `extern "C"` here.


use tk_linux_kapi::config::EINVAL;

use crate::drm_edid_types::CeaSad;

// upstream: include/drm/drm_eld.h
const DRM_ELD_CEA_EDID_VER_MNL: usize = 4;
const DRM_ELD_MNL_SHIFT: u32 = 0;
const DRM_ELD_MNL_MASK: u8 = 0x1f << 0;
const DRM_ELD_SAD_COUNT_CONN_TYPE: usize = 5;
const DRM_ELD_SAD_COUNT_SHIFT: u32 = 4;
const DRM_ELD_SAD_COUNT_MASK: u8 = 0xf << 4;

// upstream: include/drm/drm_eld.h DRM_ELD_CEA_SAD(mnl, sad)
const fn drm_eld_cea_sad(mnl: usize, sad: usize) -> usize {
    20 + mnl + 3 * sad
}

// upstream: include/drm/drm_eld.h drm_eld_mnl()
#[inline]
fn drm_eld_mnl(eld: *const u8) -> usize {
    // SAFETY: the pointer or reference used here was validated or constructed in the lines above.
    ((unsafe { *eld.add(DRM_ELD_CEA_EDID_VER_MNL) } & DRM_ELD_MNL_MASK) >> DRM_ELD_MNL_SHIFT) as usize
}

// upstream: include/drm/drm_eld.h drm_eld_sad_count()
#[inline]
fn drm_eld_sad_count(eld: *const u8) -> i32 {
    // SAFETY: the pointer or reference used here was validated or constructed in the lines above.
    ((unsafe { *eld.add(DRM_ELD_SAD_COUNT_CONN_TYPE) } & DRM_ELD_SAD_COUNT_MASK) >> DRM_ELD_SAD_COUNT_SHIFT)
        as i32
}

// CTA SAD (de)serialization owned by the EDID batch (see pending_stubs/).
unsafe extern "C" {
    fn drm_edid_cta_sad_get(cta_sad: *const CeaSad, sad: *mut u8);
    fn drm_edid_cta_sad_set(cta_sad: *mut CeaSad, sad: *const u8);
}

// upstream: drm_eld.c drm_eld_sad_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_eld_sad_get(eld: *const u8, sad_index: i32, cta_sad: *mut CeaSad) -> i32 {
    if sad_index >= drm_eld_sad_count(eld) {
        return -EINVAL;
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let sad = unsafe { eld.add(drm_eld_cea_sad(drm_eld_mnl(eld), sad_index as usize)) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_edid_cta_sad_set(cta_sad, sad) };
    0
}

// upstream: drm_eld.c drm_eld_sad_set()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_eld_sad_set(eld: *mut u8, sad_index: i32, cta_sad: *const CeaSad) -> i32 {
    if sad_index >= drm_eld_sad_count(eld) {
        return -EINVAL;
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let sad = unsafe { eld.add(drm_eld_cea_sad(drm_eld_mnl(eld), sad_index as usize)) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_edid_cta_sad_get(cta_sad, sad) };
    0
}

