// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// i915_ioctls[] from Linux v7.2.3 drivers/gpu/drm/i915/i915_driver.c, in table order.
// The handler is recorded by its C symbol name; the DRM ioctl dispatcher
// installed by the kernel resolves it. The complete MIT grant is retained in
// ../LICENSE-MIT.

#![allow(dead_code)]

use core::ffi::{CStr, c_char, c_int, c_void};
use core::ptr;

use tk_drm_upstream::{
    drm_device_types::DrmDevice, drm_file_types::DrmFile, drm_ioctl_types::DrmIoctlDesc,
};

/// `drm_ioctl_t`: the `.func` calling convention of `drm_ioctl_kernel()`.
pub type DrmIoctlFn = unsafe extern "C" fn(*mut DrmDevice, *mut c_void, *mut DrmFile) -> i32;

const fn ioctl_func(func: DrmIoctlFn) -> *mut c_void {
    func as *mut c_void
}

/// `DRM_IOCTL_DEF_DRV(ioctl, func, flags)` with `.name = #ioctl`.
const fn ioctl_entry(cmd: u32, func: *mut c_void, flags: u32, name: &'static CStr) -> DrmIoctlDesc {
    DrmIoctlDesc { cmd, flags, func, name: name.as_ptr().cast_mut() }
}

/// Designated-initializer gap in `i915_ioctls[]`: zeroed, as in C.
const fn ioctl_hole() -> DrmIoctlDesc {
    DrmIoctlDesc { cmd: 0, flags: 0, func: ptr::null_mut(), name: ptr::null_mut() as *mut c_char }
}

/// `static const struct drm_ioctl_desc i915_ioctls[]` (i915_driver.c, Linux
/// v7.2.3), in `DRM_I915_*` index order. The array has `max index + 1` = 61
/// slots, as `ARRAY_SIZE()` gives in C; `DRM_IOCTL_DEF_DRV` fills each slot
/// `[DRM_IOCTL_NR(cmd) - DRM_COMMAND_BASE]`, and unlisted slots stay zero.
/// `cmd` is the x86_64 `DRM_IOCTL_I915_*` value (`DRM_IOW`/`DRM_IOWR` of
/// `DRM_COMMAND_BASE + DRM_I915_*`). Flags use the canonical DRM_* bits.
pub static I915_IOCTLS: [DrmIoctlDesc; 61] = [
    ioctl_entry(0x40446440, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x7, c"I915_INIT"), // index 0
    ioctl_entry(0x00006441, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x1, c"I915_FLUSH"), // index 1
    ioctl_entry(0x00006442, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x1, c"I915_FLIP"), // index 2
    ioctl_entry(0x40206443, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x1, c"I915_BATCHBUFFER"), // index 3
    ioctl_entry(0xc0086444, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x1, c"I915_IRQ_EMIT"), // index 4
    ioctl_entry(0x40046445, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x1, c"I915_IRQ_WAIT"), // index 5
    ioctl_entry(0xc0106446, ioctl_func(crate::i915_getparam_upstream::i915_getparam_ioctl), 0x20, c"I915_GETPARAM"), // index 6
    ioctl_entry(0x40086447, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x7, c"I915_SETPARAM"), // index 7
    ioctl_entry(0xc0186448, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x1, c"I915_ALLOC"), // index 8
    ioctl_entry(0x40086449, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x1, c"I915_FREE"), // index 9
    ioctl_entry(0x400c644a, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x7, c"I915_INIT_HEAP"), // index 10
    ioctl_entry(0x4020644b, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x1, c"I915_CMDBUFFER"), // index 11
    ioctl_entry(0x4004644c, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x7, c"I915_DESTROY_HEAP"), // index 12
    ioctl_entry(0x4004644d, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x7, c"I915_SET_VBLANK_PIPE"), // index 13
    ioctl_entry(0x8004644e, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x1, c"I915_GET_VBLANK_PIPE"), // index 14
    ioctl_entry(0xc00c644f, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x1, c"I915_VBLANK_SWAP"), // index 15
    ioctl_hole(), // 0x50 unused
    ioctl_entry(0x40106451, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x7, c"I915_HWS_ADDR"), // index 17
    ioctl_hole(), // 0x52 unused
    ioctl_entry(0x40106453, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x7, c"I915_GEM_INIT"), // index 19
    ioctl_entry(0x40286454, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_invalid_op), 0x1, c"I915_GEM_EXECBUFFER"), // index 20
    ioctl_entry(0xc0186455, ioctl_func(crate::i915_driver_upstream::i915_gem_reject_pin_ioctl), 0x5, c"I915_GEM_PIN"), // index 21
    ioctl_entry(0x40086456, ioctl_func(crate::i915_driver_upstream::i915_gem_reject_pin_ioctl), 0x5, c"I915_GEM_UNPIN"), // index 22
    ioctl_entry(0xc0086457, ioctl_func(crate::i915_gem_busy_upstream::i915_gem_busy_ioctl), 0x20, c"I915_GEM_BUSY"), // index 23
    ioctl_entry(0x00006458, ioctl_func(crate::i915_gem_throttle_upstream::i915_gem_throttle_ioctl), 0x20, c"I915_GEM_THROTTLE"), // index 24
    ioctl_entry(0x00006459, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x7, c"I915_GEM_ENTERVT"), // index 25
    ioctl_entry(0x0000645a, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x7, c"I915_GEM_LEAVEVT"), // index 26
    ioctl_entry(0xc010645b, ioctl_func(crate::i915_gem_create_upstream::i915_gem_create_ioctl), 0x20, c"I915_GEM_CREATE"), // index 27
    ioctl_entry(0x4020645c, ioctl_func(crate::i915_gem_core_upstream::i915_gem_pread_ioctl), 0x20, c"I915_GEM_PREAD"), // index 28
    ioctl_entry(0x4020645d, ioctl_func(crate::i915_gem_core_upstream::i915_gem_pwrite_ioctl), 0x20, c"I915_GEM_PWRITE"), // index 29
    ioctl_entry(0xc028645e, ioctl_func(crate::i915_gem_mman_upstream::i915_gem_mmap_ioctl), 0x20, c"I915_GEM_MMAP"), // index 30
    ioctl_entry(0x400c645f, ioctl_func(crate::i915_gem_domain_upstream::i915_gem_set_domain_ioctl), 0x20, c"I915_GEM_SET_DOMAIN"), // index 31
    ioctl_entry(0x40046460, ioctl_func(crate::i915_gem_core_upstream::i915_gem_sw_finish_ioctl), 0x20, c"I915_GEM_SW_FINISH"), // index 32
    ioctl_entry(0xc0106461, ioctl_func(crate::i915_gem_tiling_upstream::i915_gem_set_tiling_ioctl), 0x20, c"I915_GEM_SET_TILING"), // index 33
    ioctl_entry(0xc0106462, ioctl_func(crate::i915_gem_tiling_upstream::i915_gem_get_tiling_ioctl), 0x20, c"I915_GEM_GET_TILING"), // index 34
    ioctl_entry(0x80106463, ioctl_func(crate::i915_gem_core_upstream::i915_gem_get_aperture_ioctl), 0x20, c"I915_GEM_GET_APERTURE"), // index 35
    ioctl_entry(0xc0206464, ioctl_func(crate::i915_gem_mman_upstream::i915_gem_mmap_offset_ioctl), 0x20, c"I915_GEM_MMAP_OFFSET"), // index 36
    ioctl_entry(0xc0086465, ioctl_func(crate::intel_crtc_upstream::intel_crtc_get_pipe_from_crtc_id_ioctl), 0x0, c"I915_GET_PIPE_FROM_CRTC_ID"), // index 37
    ioctl_entry(0xc00c6466, ioctl_func(crate::i915_gem_core_upstream::i915_gem_madvise_ioctl), 0x20, c"I915_GEM_MADVISE"), // index 38
    // TODO(i915-port): intel_overlay_put_image_ioctl is not translated yet; func stays NULL, which the DRM core rejects with -EINVAL.
    ioctl_entry(0x402c6467, ptr::null_mut(), 0x2, c"I915_OVERLAY_PUT_IMAGE"), // index 39
    // TODO(i915-port): intel_overlay_attrs_ioctl is not translated yet; func stays NULL, which the DRM core rejects with -EINVAL.
    ioctl_entry(0xc02c6468, ptr::null_mut(), 0x2, c"I915_OVERLAY_ATTRS"), // index 40
    ioctl_entry(0xc0406469, ioctl_func(crate::i915_gem_execbuffer_upstream::i915_gem_execbuffer2_ioctl), 0x20, c"I915_GEM_EXECBUFFER2_WR"), // index 41
    ioctl_entry(0xc014646a, ioctl_func(tk_drm_upstream::drm_ioctl_upstream::drm_noop), 0x2, c"I915_GET_SPRITE_COLORKEY"), // index 42
    ioctl_entry(0xc014646b, ioctl_func(crate::intel_sprite_uapi_upstream::intel_sprite_set_colorkey_ioctl), 0x2, c"I915_SET_SPRITE_COLORKEY"), // index 43
    ioctl_entry(0xc010646c, ioctl_func(crate::i915_gem_wait_upstream::i915_gem_wait_ioctl), 0x20, c"I915_GEM_WAIT"), // index 44
    ioctl_entry(0xc010646d, ioctl_func(crate::i915_gem_context_upstream::i915_gem_context_create_ioctl), 0x20, c"I915_GEM_CONTEXT_CREATE_EXT"), // index 45
    ioctl_entry(0x4008646e, ioctl_func(crate::i915_gem_context_upstream::i915_gem_context_destroy_ioctl), 0x20, c"I915_GEM_CONTEXT_DESTROY"), // index 46
    ioctl_entry(0x4008646f, ioctl_func(crate::i915_gem_domain_upstream::i915_gem_set_caching_ioctl), 0x20, c"I915_GEM_SET_CACHING"), // index 47
    ioctl_entry(0xc0086470, ioctl_func(crate::i915_gem_domain_upstream::i915_gem_get_caching_ioctl), 0x20, c"I915_GEM_GET_CACHING"), // index 48
    ioctl_entry(0xc0106471, ioctl_func(crate::i915_ioctl_upstream::i915_reg_read_ioctl), 0x20, c"I915_REG_READ"), // index 49
    ioctl_entry(0xc0186472, ioctl_func(crate::i915_gem_context_upstream::i915_gem_context_reset_stats_ioctl), 0x20, c"I915_GET_RESET_STATS"), // index 50
    ioctl_entry(0xc0186473, ioctl_func(crate::i915_gem_userptr_upstream::i915_gem_userptr_ioctl), 0x20, c"I915_GEM_USERPTR"), // index 51
    ioctl_entry(0xc0186474, ioctl_func(crate::i915_gem_context_upstream::i915_gem_context_getparam_ioctl), 0x20, c"I915_GEM_CONTEXT_GETPARAM"), // index 52
    ioctl_entry(0xc0186475, ioctl_func(crate::i915_gem_context_upstream::i915_gem_context_setparam_ioctl), 0x20, c"I915_GEM_CONTEXT_SETPARAM"), // index 53
    // TODO(i915-port): i915_perf_open_ioctl is not translated yet; func stays NULL, which the DRM core rejects with -EINVAL.
    ioctl_entry(0x40106476, ptr::null_mut(), 0x20, c"I915_PERF_OPEN"), // index 54
    // TODO(i915-port): i915_perf_add_config_ioctl is not translated yet; func stays NULL, which the DRM core rejects with -EINVAL.
    ioctl_entry(0x40486477, ptr::null_mut(), 0x20, c"I915_PERF_ADD_CONFIG"), // index 55
    // TODO(i915-port): i915_perf_remove_config_ioctl is not translated yet; func stays NULL, which the DRM core rejects with -EINVAL.
    ioctl_entry(0x40086478, ptr::null_mut(), 0x20, c"I915_PERF_REMOVE_CONFIG"), // index 56
    ioctl_entry(0xc0106479, ioctl_func(crate::i915_query_upstream::i915_query_ioctl), 0x20, c"I915_QUERY"), // index 57
    ioctl_entry(0xc010647a, ioctl_func(crate::i915_gem_context_upstream::i915_gem_vm_create_ioctl), 0x20, c"I915_GEM_VM_CREATE"), // index 58
    ioctl_entry(0x4010647b, ioctl_func(crate::i915_gem_context_upstream::i915_gem_vm_destroy_ioctl), 0x20, c"I915_GEM_VM_DESTROY"), // index 59
    ioctl_entry(0xc018647c, ioctl_func(crate::i915_gem_create_upstream::i915_gem_create_ext_ioctl), 0x20, c"I915_GEM_CREATE_EXT"), // index 60
];

/// Bridge-only view of one `DRM_IOCTL_DEF_DRV(I915_*, func, flags)` entry, used
/// by the kernel bridge until it dispatches through the DRM core. Superseded by
/// the canonical `I915_IOCTLS` above.
#[derive(Clone, Copy)]
pub struct BridgeIoctlDesc {
    /// `DRM_I915_*` command number (drm_ioctl index is `nr`).
    pub nr: u32,
    /// C symbol of the `.func` handler.
    pub func: &'static str,
    /// `DRM_AUTH` = 1, `DRM_MASTER` = 2, `DRM_ROOT_ONLY` = 4, `DRM_RENDER_ALLOW` = 32.
    pub flags: u32,
}

pub const I915_BRIDGE_IOCTLS: &[BridgeIoctlDesc] = &[
    BridgeIoctlDesc { nr: 0, func: "drm_noop", flags: 7 }, // I915_INIT
    BridgeIoctlDesc { nr: 1, func: "drm_noop", flags: 1 }, // I915_FLUSH
    BridgeIoctlDesc { nr: 2, func: "drm_noop", flags: 1 }, // I915_FLIP
    BridgeIoctlDesc { nr: 3, func: "drm_noop", flags: 1 }, // I915_BATCHBUFFER
    BridgeIoctlDesc { nr: 4, func: "drm_noop", flags: 1 }, // I915_IRQ_EMIT
    BridgeIoctlDesc { nr: 5, func: "drm_noop", flags: 1 }, // I915_IRQ_WAIT
    BridgeIoctlDesc { nr: 6, func: "i915_getparam_ioctl", flags: 32 }, // I915_GETPARAM
    BridgeIoctlDesc { nr: 7, func: "drm_noop", flags: 7 }, // I915_SETPARAM
    BridgeIoctlDesc { nr: 8, func: "drm_noop", flags: 1 }, // I915_ALLOC
    BridgeIoctlDesc { nr: 9, func: "drm_noop", flags: 1 }, // I915_FREE
    BridgeIoctlDesc { nr: 10, func: "drm_noop", flags: 7 }, // I915_INIT_HEAP
    BridgeIoctlDesc { nr: 11, func: "drm_noop", flags: 1 }, // I915_CMDBUFFER
    BridgeIoctlDesc { nr: 12, func: "drm_noop", flags: 7 }, // I915_DESTROY_HEAP
    BridgeIoctlDesc { nr: 13, func: "drm_noop", flags: 7 }, // I915_SET_VBLANK_PIPE
    BridgeIoctlDesc { nr: 14, func: "drm_noop", flags: 1 }, // I915_GET_VBLANK_PIPE
    BridgeIoctlDesc { nr: 15, func: "drm_noop", flags: 1 }, // I915_VBLANK_SWAP
    BridgeIoctlDesc { nr: 17, func: "drm_noop", flags: 7 }, // I915_HWS_ADDR
    BridgeIoctlDesc { nr: 19, func: "drm_noop", flags: 7 }, // I915_GEM_INIT
    BridgeIoctlDesc { nr: 20, func: "drm_invalid_op", flags: 1 }, // I915_GEM_EXECBUFFER
    BridgeIoctlDesc { nr: 21, func: "i915_gem_reject_pin_ioctl", flags: 5 }, // I915_GEM_PIN
    BridgeIoctlDesc { nr: 22, func: "i915_gem_reject_pin_ioctl", flags: 5 }, // I915_GEM_UNPIN
    BridgeIoctlDesc { nr: 23, func: "i915_gem_busy_ioctl", flags: 32 }, // I915_GEM_BUSY
    BridgeIoctlDesc { nr: 47, func: "i915_gem_set_caching_ioctl", flags: 32 }, // I915_GEM_SET_CACHING
    BridgeIoctlDesc { nr: 48, func: "i915_gem_get_caching_ioctl", flags: 32 }, // I915_GEM_GET_CACHING
    BridgeIoctlDesc { nr: 24, func: "i915_gem_throttle_ioctl", flags: 32 }, // I915_GEM_THROTTLE
    BridgeIoctlDesc { nr: 25, func: "drm_noop", flags: 7 }, // I915_GEM_ENTERVT
    BridgeIoctlDesc { nr: 26, func: "drm_noop", flags: 7 }, // I915_GEM_LEAVEVT
    BridgeIoctlDesc { nr: 27, func: "i915_gem_create_ioctl", flags: 32 }, // I915_GEM_CREATE
    BridgeIoctlDesc { nr: 60, func: "i915_gem_create_ext_ioctl", flags: 32 }, // I915_GEM_CREATE_EXT
    BridgeIoctlDesc { nr: 28, func: "i915_gem_pread_ioctl", flags: 32 }, // I915_GEM_PREAD
    BridgeIoctlDesc { nr: 29, func: "i915_gem_pwrite_ioctl", flags: 32 }, // I915_GEM_PWRITE
    BridgeIoctlDesc { nr: 30, func: "i915_gem_mmap_ioctl", flags: 32 }, // I915_GEM_MMAP
    BridgeIoctlDesc { nr: 31, func: "i915_gem_set_domain_ioctl", flags: 32 }, // I915_GEM_SET_DOMAIN
    BridgeIoctlDesc { nr: 32, func: "i915_gem_sw_finish_ioctl", flags: 32 }, // I915_GEM_SW_FINISH
    BridgeIoctlDesc { nr: 33, func: "i915_gem_set_tiling_ioctl", flags: 32 }, // I915_GEM_SET_TILING
    BridgeIoctlDesc { nr: 34, func: "i915_gem_get_tiling_ioctl", flags: 32 }, // I915_GEM_GET_TILING
    BridgeIoctlDesc { nr: 35, func: "i915_gem_get_aperture_ioctl", flags: 32 }, // I915_GEM_GET_APERTURE
    BridgeIoctlDesc { nr: 37, func: "intel_crtc_get_pipe_from_crtc_id_ioctl", flags: 0 }, // I915_GET_PIPE_FROM_CRTC_ID
    BridgeIoctlDesc { nr: 38, func: "i915_gem_madvise_ioctl", flags: 32 }, // I915_GEM_MADVISE
    BridgeIoctlDesc { nr: 39, func: "intel_overlay_put_image_ioctl", flags: 2 }, // I915_OVERLAY_PUT_IMAGE
    BridgeIoctlDesc { nr: 40, func: "intel_overlay_attrs_ioctl", flags: 2 }, // I915_OVERLAY_ATTRS
    BridgeIoctlDesc { nr: 43, func: "intel_sprite_set_colorkey_ioctl", flags: 2 }, // I915_SET_SPRITE_COLORKEY
    BridgeIoctlDesc { nr: 42, func: "drm_noop", flags: 2 }, // I915_GET_SPRITE_COLORKEY
    BridgeIoctlDesc { nr: 44, func: "i915_gem_wait_ioctl", flags: 32 }, // I915_GEM_WAIT
    BridgeIoctlDesc { nr: 46, func: "i915_gem_context_destroy_ioctl", flags: 32 }, // I915_GEM_CONTEXT_DESTROY
    BridgeIoctlDesc { nr: 49, func: "i915_reg_read_ioctl", flags: 32 }, // I915_REG_READ
    BridgeIoctlDesc { nr: 50, func: "i915_gem_context_reset_stats_ioctl", flags: 32 }, // I915_GET_RESET_STATS
    BridgeIoctlDesc { nr: 51, func: "i915_gem_userptr_ioctl", flags: 32 }, // I915_GEM_USERPTR
    BridgeIoctlDesc { nr: 52, func: "i915_gem_context_getparam_ioctl", flags: 32 }, // I915_GEM_CONTEXT_GETPARAM
    BridgeIoctlDesc { nr: 53, func: "i915_gem_context_setparam_ioctl", flags: 32 }, // I915_GEM_CONTEXT_SETPARAM
    BridgeIoctlDesc { nr: 54, func: "i915_perf_open_ioctl", flags: 32 }, // I915_PERF_OPEN
    BridgeIoctlDesc { nr: 55, func: "i915_perf_add_config_ioctl", flags: 32 }, // I915_PERF_ADD_CONFIG
    BridgeIoctlDesc { nr: 56, func: "i915_perf_remove_config_ioctl", flags: 32 }, // I915_PERF_REMOVE_CONFIG
    BridgeIoctlDesc { nr: 57, func: "i915_query_ioctl", flags: 32 }, // I915_QUERY
    BridgeIoctlDesc { nr: 58, func: "i915_gem_vm_create_ioctl", flags: 32 }, // I915_GEM_VM_CREATE
    BridgeIoctlDesc { nr: 59, func: "i915_gem_vm_destroy_ioctl", flags: 32 }, // I915_GEM_VM_DESTROY
    // Aliases defined in i915_drm.h: EXECBUFFER2_WR = EXECBUFFER2 (0x29),
    // MMAP_OFFSET = MMAP_GTT (0x24), CONTEXT_CREATE_EXT = CONTEXT_CREATE (0x2d).
    BridgeIoctlDesc { nr: 0x29, func: "i915_gem_execbuffer2_ioctl", flags: 32 }, // I915_GEM_EXECBUFFER2_WR
    BridgeIoctlDesc { nr: 0x24, func: "i915_gem_mmap_offset_ioctl", flags: 32 }, // I915_GEM_MMAP_OFFSET
    BridgeIoctlDesc { nr: 0x2d, func: "i915_gem_context_create_ioctl", flags: 32 }, // I915_GEM_CONTEXT_CREATE_EXT
];

/// Whether this ioctl has a real upstream implementation which can be called
/// by the kernel's file adapter. The table records the complete Linux ABI,
/// while this deliberately narrow list contains only handlers whose current
/// object, memory and execution owners are wired by this kernel.
pub fn supports_kernel_ioctl(cmd: u32) -> bool {
    let Some(nr) = kernel_ioctl_nr(cmd) else {
        return false;
    };
    let Some(desc) = I915_BRIDGE_IOCTLS.iter().find(|desc| desc.nr == nr) else {
        return false;
    };
    let Some((size, direction)) = kernel_ioctl_shape(nr) else {
        return false;
    };
    // Accept only the canonical DRM_IOWR command. Besides direction and type,
    // exact size is critical because the bridge copies this many bytes before
    // handing the buffer to source-translated handlers.
    let expected = (direction << 30) | ((size as u32) << 16) | (u32::from(b'd') << 8) | (nr + 0x40);
    let execbuffer2_write = nr == 0x29
        && cmd
            == ((1u32 << 30)
                | ((size as u32) << 16)
                | (u32::from(b'd') << 8)
                | (nr + 0x40));
    if cmd != expected && !execbuffer2_write {
        return false;
    }
    // DRM_RENDER_ALLOW; i915 GEM allocation/query/submit do not require
    // primary-node master or legacy authentication.
    desc.flags & 32 != 0
        && matches!(
            nr,
            6 | 23 | 27 | 28 | 29 | 31 | 32 | 33 | 34 | 35 | 38 | 44 | 46 | 47 | 48 | 49
                | 50 | 52 | 53 | 57 | 58 | 59 | 60 | 0x24 | 0x29 | 0x2d
        )
}

#[inline]
fn kernel_ioctl_nr(cmd: u32) -> Option<u32> {
    (((cmd >> 8) & 0xff) == u32::from(b'd'))
        .then(|| cmd & 0xff)
        .and_then(|number| number.checked_sub(0x40))
}

/// Exact (IOC direction, x86_64 record size) for each kernel-routed command.
/// Directions are Linux `_IOC_WRITE=1`, `_IOC_READ=2`, `_IOC_READ|WRITE=3`.
fn kernel_ioctl_shape(nr: u32) -> Option<(usize, u32)> {
    Some(match nr {
        6 => (16, 3),       // drm_i915_getparam
        23 => (8, 3),       // drm_i915_gem_busy
        27 => (16, 3),      // drm_i915_gem_create
        28 | 29 => (32, 1), // drm_i915_gem_pread / pwrite
        31 => (12, 1),      // drm_i915_gem_set_domain
        32 => (4, 1),       // drm_i915_gem_sw_finish
        33 | 34 => (16, 3), // drm_i915_gem_set_tiling / get_tiling
        35 => (16, 2),      // drm_i915_gem_get_aperture
        38 => (12, 3),      // drm_i915_gem_madvise
        44 => (16, 3),      // drm_i915_gem_wait
        46 => (8, 1),       // drm_i915_gem_context_destroy
        47 => (8, 1),       // drm_i915_gem_set_caching
        48 => (8, 3),       // drm_i915_gem_get_caching
        49 => (16, 3),      // drm_i915_reg_read
        50 => (24, 3),      // drm_i915_reset_stats
        52 | 53 => (24, 3), // drm_i915_gem_context_param
        57 => (16, 3),      // drm_i915_query
        58 => (16, 3),     // drm_i915_gem_vm_control (create)
        59 => (16, 1),     // drm_i915_gem_vm_control (destroy)
        60 => (24, 3),      // drm_i915_gem_create_ext
        0x24 => (32, 3),    // drm_i915_gem_mmap_offset
        0x29 => (64, 3),    // drm_i915_gem_execbuffer2_wr
        0x2d => (16, 3),    // drm_i915_gem_context_create_ext
        _ => return None,
    })
}

/// Dispatch a supported i915 command to its source-translated Linux handler.
/// `dev`, `file` and `data` retain the exact DRM objects used by the core
/// provider; all handlers still perform their own UAPI and user-copy checks.
/// Unsupported ABI entries intentionally return `None`, never synthetic
/// success.
///
/// # Safety
/// Pointers must be the live DRM device/file and current task's ioctl buffer.
pub unsafe fn dispatch_kernel_ioctl(
    dev: *mut c_void,
    file: *mut c_void,
    cmd: u32,
    data: *mut c_void,
) -> Option<c_int> {
    if !supports_kernel_ioctl(cmd) {
        return None;
    }
    // `drm_ioctl()` receives the full ioctl command number; i915's descriptor
    // table is indexed relative to DRM_COMMAND_BASE (0x40).
    let nr = kernel_ioctl_nr(cmd).expect("supported i915 ioctl has command-base number");
    let cdev = dev.cast::<DrmDevice>();
    let cfile = file.cast::<DrmFile>();
    Some(unsafe {
        match nr {
            6 => crate::i915_getparam_upstream::i915_getparam_ioctl(cdev, data, cfile),
            23 => crate::i915_gem_busy_upstream::i915_gem_busy_ioctl(cdev, data, cfile),
            27 => crate::i915_gem_create_upstream::i915_gem_create_ioctl(cdev, data, cfile),
            28 => crate::i915_gem_core_upstream::i915_gem_pread_ioctl(cdev, data, cfile),
            29 => crate::i915_gem_core_upstream::i915_gem_pwrite_ioctl(cdev, data, cfile),
            31 => crate::i915_gem_domain_upstream::i915_gem_set_domain_ioctl(cdev, data, cfile),
            32 => crate::i915_gem_core_upstream::i915_gem_sw_finish_ioctl(cdev, data, cfile),
            33 => crate::i915_gem_tiling_upstream::i915_gem_set_tiling_ioctl(cdev, data, cfile),
            34 => crate::i915_gem_tiling_upstream::i915_gem_get_tiling_ioctl(cdev, data, cfile),
            35 => crate::i915_gem_core_upstream::i915_gem_get_aperture_ioctl(cdev, data, cfile),
            38 => crate::i915_gem_core_upstream::i915_gem_madvise_ioctl(cdev, data, cfile),
            44 => crate::i915_gem_wait_upstream::i915_gem_wait_ioctl(cdev, data, cfile),
            46 => crate::i915_gem_context_upstream::i915_gem_context_destroy_ioctl(cdev, data, cfile),
            47 => crate::i915_gem_domain_upstream::i915_gem_set_caching_ioctl(cdev, data, cfile),
            48 => crate::i915_gem_domain_upstream::i915_gem_get_caching_ioctl(cdev, data, cfile),
            49 => i915_reg_read_ioctl(cdev, data, cfile),
            50 => crate::i915_gem_context_upstream::i915_gem_context_reset_stats_ioctl(cdev, data, cfile),
            52 => crate::i915_gem_context_upstream::i915_gem_context_getparam_ioctl(cdev, data, cfile),
            53 => crate::i915_gem_context_upstream::i915_gem_context_setparam_ioctl(cdev, data, cfile),
            57 => crate::i915_query_upstream::i915_query_ioctl(cdev, data, cfile),
            58 => crate::i915_gem_context_upstream::i915_gem_vm_create_ioctl(cdev, data, cfile),
            59 => crate::i915_gem_context_upstream::i915_gem_vm_destroy_ioctl(cdev, data, cfile),
            60 => crate::i915_gem_create_upstream::i915_gem_create_ext_ioctl(cdev, data, cfile),
            0x24 => crate::i915_gem_mman_upstream::i915_gem_mmap_offset_ioctl(cdev, data, cfile),
            0x29 => crate::i915_gem_execbuffer_upstream::i915_gem_execbuffer2_ioctl(cdev, data, cfile),
            0x2d => crate::i915_gem_context_upstream::i915_gem_context_create_ioctl(cdev, data, cfile),
            _ => return None,
        }
    })
}

/// Source i915_ioctl.c REG_READ whitelist: only the render timestamp, with
/// optional 8B-WA split-read flag. Never admit arbitrary MMIO from userspace.
fn timestamp_read_flags(graphics_ver: u8, offset: u64) -> Option<bool> {
    let timestamp = crate::intel_engine_regs_upstream::RING_TIMESTAMP(
        crate::linux::registers::RENDER_RING_BASE,
    ).reg as u64;
    if !(4..=12).contains(&graphics_ver) || offset & !7 != timestamp {
        return None;
    }
    match offset & 7 { 0 => Some(false), 1 => Some(true), _ => None }
}

pub unsafe extern "C" fn i915_reg_read_ioctl(
    dev: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    data: *mut c_void,
    _file: *mut tk_drm_upstream::drm_file_types::DrmFile,
) -> i32 {
    let dev = dev.cast::<c_void>();
    use crate::intel_uncore_types_upstream::{IntelUncore, intel_uncore_read64, intel_uncore_read64_2x32};
    use crate::intel_runtime_pm_upstream::{intel_runtime_pm_get, intel_runtime_pm_put};
    use crate::intel_engine_regs_upstream::{RING_TIMESTAMP, RING_TIMESTAMP_UDW};
    use crate::linux::registers::RENDER_RING_BASE;
    #[repr(C)]
    struct RegRead { offset: u64, value: u64 }
    if dev.is_null() || data.is_null() { return -22; }
    let i915 = unsafe { crate::linux::i915::to_i915(dev) };
    let reg = unsafe { &mut *data.cast::<RegRead>() };
    let Some(split) = timestamp_read_flags(unsafe { crate::linux::i915::GRAPHICS_VER(i915) } as u8, reg.offset) else {
        return -22;
    };
    let uncore = unsafe { core::ptr::addr_of_mut!((*i915).uncore).cast::<IntelUncore>() };
    let rpm = unsafe { core::ptr::addr_of_mut!((*i915).runtime_pm).cast() };
    let wakeref = unsafe { intel_runtime_pm_get(rpm) };
    reg.value = unsafe {
        if split {
            intel_uncore_read64_2x32(uncore, RING_TIMESTAMP(RENDER_RING_BASE), RING_TIMESTAMP_UDW(RENDER_RING_BASE))
        } else {
            intel_uncore_read64(uncore, RING_TIMESTAMP(RENDER_RING_BASE))
        }
    };
    unsafe { intel_runtime_pm_put(rpm, wakeref) };
    0
}

#[cfg(test)]
mod bridge_tests {
    use super::*;
    fn cmd(nr: u32, size: u32, direction: u32) -> u32 {
        (direction << 30) | (size << 16) | (u32::from(b'd') << 8) | (nr + 0x40)
    }
    #[test]
    fn timestamp_and_vm_uapi_shapes_are_exact() {
        for (nr, size, dir) in [(49, 16, 3), (58, 16, 3), (59, 16, 1), (0x24, 32, 3)] {
            assert!(supports_kernel_ioctl(cmd(nr, size, dir)));
            assert!(!supports_kernel_ioctl(cmd(nr, size - 8, dir)));
        }
    }
    #[test]
    fn reg_read_never_exposes_arbitrary_mmio_or_unknown_flags() {
        assert_eq!(timestamp_read_flags(12, 0x2358), Some(false));
        assert_eq!(timestamp_read_flags(12, 0x2359), Some(true));
        for offset in [0, 0x235a, 0x235f, 0x2360, 0x1_0000_2358] {
            assert_eq!(timestamp_read_flags(12, offset), None);
        }
        assert_eq!(timestamp_read_flags(3, 0x2358), None);
        assert_eq!(timestamp_read_flags(13, 0x2358), None);
    }
}

#[cfg(test)]
mod canonical_table_tests {
    use super::*;
    use tk_drm_upstream::drm_ioctl_types::{DRM_AUTH, DRM_MASTER, DRM_RENDER_ALLOW, DRM_ROOT_ONLY};
    use tk_drm_upstream::drm_ioctl_upstream::{drm_invalid_op, drm_noop};

    /// `ARRAY_SIZE(i915_ioctls)`: highest `DRM_I915_*` index (`GEM_CREATE_EXT`,
    /// 0x3c) plus one. `DRM_I915_*` values from Linux v7.2.3 i915_drm.h.
    const LINUX_NUM_IOCTLS: usize = 61;
    const LINUX_ENTRIES: usize = 59;

    fn name_of(desc: &DrmIoctlDesc) -> &'static str {
        assert!(!desc.name.is_null());
        unsafe { core::ffi::CStr::from_ptr(desc.name) }.to_str().unwrap()
    }

    fn entry_for(name: &str) -> &'static DrmIoctlDesc {
        I915_IOCTLS
            .iter()
            .find(|desc| !desc.name.is_null() && name_of(desc) == name)
            .unwrap_or_else(|| panic!("{name} missing from I915_IOCTLS"))
    }

    #[test]
    fn table_has_linux_count_and_holes_are_zero() {
        assert_eq!(I915_IOCTLS.len(), LINUX_NUM_IOCTLS);
        let populated = I915_IOCTLS.iter().filter(|desc| !desc.name.is_null()).count();
        assert_eq!(populated, LINUX_ENTRIES);
        // DRM_I915_* 0x10 and 0x12 are not defined, so their slots stay zeroed.
        for index in [0x10usize, 0x12] {
            let desc = &I915_IOCTLS[index];
            assert_eq!((desc.cmd, desc.flags), (0, 0));
            assert!(desc.func.is_null() && desc.name.is_null());
        }
    }

    #[test]
    fn cmd_numbers_match_linux_uapi() {
        // DRM_IOCTL_I915_* as computed by Linux's DRM_IOW/DRM_IOWR macros.
        assert_eq!(entry_for("I915_INIT").cmd, 0x4044_6440);
        assert_eq!(entry_for("I915_GETPARAM").cmd, 0xc010_6446);
        assert_eq!(entry_for("I915_GEM_EXECBUFFER2_WR").cmd, 0xc040_6469);
        assert_eq!(entry_for("I915_GEM_CREATE_EXT").cmd, 0xc018_647c);
        assert_eq!(entry_for("I915_GEM_VM_DESTROY").cmd, 0x4010_647b);
    }

    #[test]
    fn each_entry_sits_at_its_drm_command_index() {
        for (index, desc) in I915_IOCTLS.iter().enumerate() {
            if desc.name.is_null() {
                continue;
            }
            assert_eq!(((desc.cmd & 0xff) - 0x40) as usize, index, "{}", name_of(desc));
        }
    }

    #[test]
    fn flags_and_handlers_match_linux_table() {
        // DRM_IOCTL_DEF_DRV(I915_GETPARAM, i915_getparam_ioctl, DRM_RENDER_ALLOW)
        let getparam = entry_for("I915_GETPARAM");
        assert_eq!(getparam.flags, DRM_RENDER_ALLOW);
        assert_eq!(getparam.func, ioctl_func(crate::i915_getparam_upstream::i915_getparam_ioctl));
        // DRM_IOCTL_DEF_DRV(I915_GEM_EXECBUFFER2_WR, ..., DRM_RENDER_ALLOW)
        let execbuffer2 = entry_for("I915_GEM_EXECBUFFER2_WR");
        assert_eq!(execbuffer2.flags, DRM_RENDER_ALLOW);
        assert_eq!(
            execbuffer2.func,
            ioctl_func(crate::i915_gem_execbuffer_upstream::i915_gem_execbuffer2_ioctl)
        );
        // DRM_IOCTL_DEF_DRV(I915_GEM_CREATE_EXT, ..., DRM_RENDER_ALLOW)
        let create_ext = entry_for("I915_GEM_CREATE_EXT");
        assert_eq!(create_ext.flags, DRM_RENDER_ALLOW);
        assert_eq!(
            create_ext.func,
            ioctl_func(crate::i915_gem_create_upstream::i915_gem_create_ext_ioctl)
        );
        // DRM_AUTH|DRM_MASTER|DRM_ROOT_ONLY, handler drm_noop
        let init = entry_for("I915_INIT");
        assert_eq!(init.flags, DRM_AUTH | DRM_MASTER | DRM_ROOT_ONLY);
        assert_eq!(init.func, ioctl_func(drm_noop));
        // I915_GEM_EXECBUFFER (legacy) is drm_invalid_op with DRM_AUTH.
        let legacy = entry_for("I915_GEM_EXECBUFFER");
        assert_eq!((legacy.flags, legacy.func), (DRM_AUTH, ioctl_func(drm_invalid_op)));
        // Handlers not yet ported stay NULL, so the DRM core returns -EINVAL.
        assert!(entry_for("I915_PERF_OPEN").func.is_null());
    }
}
