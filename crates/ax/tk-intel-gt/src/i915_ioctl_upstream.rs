// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// i915_ioctls[] from Linux v7.2.3 drivers/gpu/drm/i915/i915_driver.c, in table order.
// The handler is recorded by its C symbol name; the DRM ioctl dispatcher
// installed by the kernel resolves it. The complete MIT grant is retained in
// ../LICENSE-MIT.

#![allow(dead_code)]

/// One `DRM_IOCTL_DEF_DRV(I915_*, func, flags)` entry.
#[derive(Clone, Copy)]
pub struct DrmIoctlDesc {
    /// `DRM_I915_*` command number (drm_ioctl index is `nr`).
    pub nr: u32,
    /// C symbol of the `.func` handler.
    pub func: &'static str,
    /// `DRM_AUTH` = 1, `DRM_MASTER` = 2, `DRM_ROOT_ONLY` = 4, `DRM_RENDER_ALLOW` = 32.
    pub flags: u32,
}

pub const I915_IOCTLS: &[DrmIoctlDesc] = &[
    DrmIoctlDesc { nr: 0, func: "drm_noop", flags: 7 }, // I915_INIT
    DrmIoctlDesc { nr: 1, func: "drm_noop", flags: 1 }, // I915_FLUSH
    DrmIoctlDesc { nr: 2, func: "drm_noop", flags: 1 }, // I915_FLIP
    DrmIoctlDesc { nr: 3, func: "drm_noop", flags: 1 }, // I915_BATCHBUFFER
    DrmIoctlDesc { nr: 4, func: "drm_noop", flags: 1 }, // I915_IRQ_EMIT
    DrmIoctlDesc { nr: 5, func: "drm_noop", flags: 1 }, // I915_IRQ_WAIT
    DrmIoctlDesc { nr: 6, func: "i915_getparam_ioctl", flags: 32 }, // I915_GETPARAM
    DrmIoctlDesc { nr: 7, func: "drm_noop", flags: 7 }, // I915_SETPARAM
    DrmIoctlDesc { nr: 8, func: "drm_noop", flags: 1 }, // I915_ALLOC
    DrmIoctlDesc { nr: 9, func: "drm_noop", flags: 1 }, // I915_FREE
    DrmIoctlDesc { nr: 10, func: "drm_noop", flags: 7 }, // I915_INIT_HEAP
    DrmIoctlDesc { nr: 11, func: "drm_noop", flags: 1 }, // I915_CMDBUFFER
    DrmIoctlDesc { nr: 12, func: "drm_noop", flags: 7 }, // I915_DESTROY_HEAP
    DrmIoctlDesc { nr: 13, func: "drm_noop", flags: 7 }, // I915_SET_VBLANK_PIPE
    DrmIoctlDesc { nr: 14, func: "drm_noop", flags: 1 }, // I915_GET_VBLANK_PIPE
    DrmIoctlDesc { nr: 15, func: "drm_noop", flags: 1 }, // I915_VBLANK_SWAP
    DrmIoctlDesc { nr: 17, func: "drm_noop", flags: 7 }, // I915_HWS_ADDR
    DrmIoctlDesc { nr: 19, func: "drm_noop", flags: 7 }, // I915_GEM_INIT
    DrmIoctlDesc { nr: 20, func: "drm_invalid_op", flags: 1 }, // I915_GEM_EXECBUFFER
    DrmIoctlDesc { nr: 21, func: "i915_gem_reject_pin_ioctl", flags: 5 }, // I915_GEM_PIN
    DrmIoctlDesc { nr: 22, func: "i915_gem_reject_pin_ioctl", flags: 5 }, // I915_GEM_UNPIN
    DrmIoctlDesc { nr: 23, func: "i915_gem_busy_ioctl", flags: 32 }, // I915_GEM_BUSY
    DrmIoctlDesc { nr: 47, func: "i915_gem_set_caching_ioctl", flags: 32 }, // I915_GEM_SET_CACHING
    DrmIoctlDesc { nr: 48, func: "i915_gem_get_caching_ioctl", flags: 32 }, // I915_GEM_GET_CACHING
    DrmIoctlDesc { nr: 24, func: "i915_gem_throttle_ioctl", flags: 32 }, // I915_GEM_THROTTLE
    DrmIoctlDesc { nr: 25, func: "drm_noop", flags: 7 }, // I915_GEM_ENTERVT
    DrmIoctlDesc { nr: 26, func: "drm_noop", flags: 7 }, // I915_GEM_LEAVEVT
    DrmIoctlDesc { nr: 27, func: "i915_gem_create_ioctl", flags: 32 }, // I915_GEM_CREATE
    DrmIoctlDesc { nr: 60, func: "i915_gem_create_ext_ioctl", flags: 32 }, // I915_GEM_CREATE_EXT
    DrmIoctlDesc { nr: 28, func: "i915_gem_pread_ioctl", flags: 32 }, // I915_GEM_PREAD
    DrmIoctlDesc { nr: 29, func: "i915_gem_pwrite_ioctl", flags: 32 }, // I915_GEM_PWRITE
    DrmIoctlDesc { nr: 30, func: "i915_gem_mmap_ioctl", flags: 32 }, // I915_GEM_MMAP
    DrmIoctlDesc { nr: 31, func: "i915_gem_set_domain_ioctl", flags: 32 }, // I915_GEM_SET_DOMAIN
    DrmIoctlDesc { nr: 32, func: "i915_gem_sw_finish_ioctl", flags: 32 }, // I915_GEM_SW_FINISH
    DrmIoctlDesc { nr: 33, func: "i915_gem_set_tiling_ioctl", flags: 32 }, // I915_GEM_SET_TILING
    DrmIoctlDesc { nr: 34, func: "i915_gem_get_tiling_ioctl", flags: 32 }, // I915_GEM_GET_TILING
    DrmIoctlDesc { nr: 35, func: "i915_gem_get_aperture_ioctl", flags: 32 }, // I915_GEM_GET_APERTURE
    DrmIoctlDesc { nr: 37, func: "intel_crtc_get_pipe_from_crtc_id_ioctl", flags: 0 }, // I915_GET_PIPE_FROM_CRTC_ID
    DrmIoctlDesc { nr: 38, func: "i915_gem_madvise_ioctl", flags: 32 }, // I915_GEM_MADVISE
    DrmIoctlDesc { nr: 39, func: "intel_overlay_put_image_ioctl", flags: 2 }, // I915_OVERLAY_PUT_IMAGE
    DrmIoctlDesc { nr: 40, func: "intel_overlay_attrs_ioctl", flags: 2 }, // I915_OVERLAY_ATTRS
    DrmIoctlDesc { nr: 43, func: "intel_sprite_set_colorkey_ioctl", flags: 2 }, // I915_SET_SPRITE_COLORKEY
    DrmIoctlDesc { nr: 42, func: "drm_noop", flags: 2 }, // I915_GET_SPRITE_COLORKEY
    DrmIoctlDesc { nr: 44, func: "i915_gem_wait_ioctl", flags: 32 }, // I915_GEM_WAIT
    DrmIoctlDesc { nr: 46, func: "i915_gem_context_destroy_ioctl", flags: 32 }, // I915_GEM_CONTEXT_DESTROY
    DrmIoctlDesc { nr: 49, func: "i915_reg_read_ioctl", flags: 32 }, // I915_REG_READ
    DrmIoctlDesc { nr: 50, func: "i915_gem_context_reset_stats_ioctl", flags: 32 }, // I915_GET_RESET_STATS
    DrmIoctlDesc { nr: 51, func: "i915_gem_userptr_ioctl", flags: 32 }, // I915_GEM_USERPTR
    DrmIoctlDesc { nr: 52, func: "i915_gem_context_getparam_ioctl", flags: 32 }, // I915_GEM_CONTEXT_GETPARAM
    DrmIoctlDesc { nr: 53, func: "i915_gem_context_setparam_ioctl", flags: 32 }, // I915_GEM_CONTEXT_SETPARAM
    DrmIoctlDesc { nr: 54, func: "i915_perf_open_ioctl", flags: 32 }, // I915_PERF_OPEN
    DrmIoctlDesc { nr: 55, func: "i915_perf_add_config_ioctl", flags: 32 }, // I915_PERF_ADD_CONFIG
    DrmIoctlDesc { nr: 56, func: "i915_perf_remove_config_ioctl", flags: 32 }, // I915_PERF_REMOVE_CONFIG
    DrmIoctlDesc { nr: 57, func: "i915_query_ioctl", flags: 32 }, // I915_QUERY
    DrmIoctlDesc { nr: 58, func: "i915_gem_vm_create_ioctl", flags: 32 }, // I915_GEM_VM_CREATE
    DrmIoctlDesc { nr: 59, func: "i915_gem_vm_destroy_ioctl", flags: 32 }, // I915_GEM_VM_DESTROY
    // Aliases defined in i915_drm.h: EXECBUFFER2_WR = EXECBUFFER2 (0x29),
    // MMAP_OFFSET = MMAP_GTT (0x24), CONTEXT_CREATE_EXT = CONTEXT_CREATE (0x2d).
    DrmIoctlDesc { nr: 0x29, func: "i915_gem_execbuffer2_ioctl", flags: 32 }, // I915_GEM_EXECBUFFER2_WR
    DrmIoctlDesc { nr: 0x24, func: "i915_gem_mmap_offset_ioctl", flags: 32 }, // I915_GEM_MMAP_OFFSET
    DrmIoctlDesc { nr: 0x2d, func: "i915_gem_context_create_ioctl", flags: 32 }, // I915_GEM_CONTEXT_CREATE_EXT
];
