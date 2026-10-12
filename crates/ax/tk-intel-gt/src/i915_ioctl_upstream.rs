// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// i915_ioctls[] from Linux v7.2.3 drivers/gpu/drm/i915/i915_driver.c, in table order.
// The handler is recorded by its C symbol name; the DRM ioctl dispatcher
// installed by the kernel resolves it. The complete MIT grant is retained in
// ../LICENSE-MIT.

#![allow(dead_code)]

use core::ffi::{c_int, c_void};

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

/// Whether this ioctl has a real upstream implementation which can be called
/// by the kernel's file adapter. The table records the complete Linux ABI,
/// while this deliberately narrow list contains only handlers whose current
/// object, memory and execution owners are wired by this kernel.
pub fn supports_kernel_ioctl(cmd: u32) -> bool {
    let Some(nr) = kernel_ioctl_nr(cmd) else {
        return false;
    };
    let Some(desc) = I915_IOCTLS.iter().find(|desc| desc.nr == nr) else {
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
    let drm_dev = dev.cast::<crate::linux::gem::DrmDevice>();
    let drm_file = file.cast::<crate::linux::gem::DrmFile>();
    let i915_file = file.cast::<crate::i915_request_types_upstream::DrmFile>();
    let domain_dev = dev.cast::<crate::i915_gem_domain_upstream::DrmDevice>();
    let domain_file = file.cast::<crate::i915_gem_domain_upstream::DrmFile>();
    Some(unsafe {
        match nr {
            6 => crate::i915_getparam_upstream::i915_getparam_ioctl(dev, data, file),
            23 => crate::i915_gem_busy_upstream::i915_gem_busy_ioctl(dev, data, drm_file),
            27 => crate::i915_gem_create_upstream::i915_gem_create_ioctl(drm_dev, data, i915_file),
            28 => crate::i915_gem_core_upstream::i915_gem_pread_ioctl(dev, data, file),
            29 => crate::i915_gem_core_upstream::i915_gem_pwrite_ioctl(dev, data, file),
            31 => crate::i915_gem_domain_upstream::i915_gem_set_domain_ioctl(domain_dev, data, domain_file),
            32 => crate::i915_gem_core_upstream::i915_gem_sw_finish_ioctl(dev, data, file),
            33 => crate::i915_gem_tiling_upstream::i915_gem_set_tiling_ioctl(drm_dev, data, drm_file),
            34 => crate::i915_gem_tiling_upstream::i915_gem_get_tiling_ioctl(drm_dev, data, drm_file),
            35 => crate::i915_gem_core_upstream::i915_gem_get_aperture_ioctl(dev, data, file),
            38 => crate::i915_gem_core_upstream::i915_gem_madvise_ioctl(dev, data, file),
            44 => crate::i915_gem_wait_upstream::i915_gem_wait_ioctl(dev, data, file),
            46 => crate::i915_gem_context_upstream::i915_gem_context_destroy_ioctl(drm_dev, data, drm_file),
            47 => crate::i915_gem_domain_upstream::i915_gem_set_caching_ioctl(domain_dev, data, domain_file),
            48 => crate::i915_gem_domain_upstream::i915_gem_get_caching_ioctl(domain_dev, data, domain_file),
            49 => i915_reg_read_ioctl(dev, data),
            50 => crate::i915_gem_context_upstream::i915_gem_context_reset_stats_ioctl(drm_dev, data, drm_file),
            52 => crate::i915_gem_context_upstream::i915_gem_context_getparam_ioctl(drm_dev, data, drm_file),
            53 => crate::i915_gem_context_upstream::i915_gem_context_setparam_ioctl(drm_dev, data, drm_file),
            57 => crate::i915_query_upstream::i915_query_ioctl(dev, data, file),
            58 => crate::i915_gem_context_upstream::i915_gem_vm_create_ioctl(dev, data, drm_file),
            59 => crate::i915_gem_context_upstream::i915_gem_vm_destroy_ioctl(dev, data, drm_file),
            60 => crate::i915_gem_create_upstream::i915_gem_create_ext_ioctl(drm_dev, data, i915_file),
            0x24 => crate::i915_gem_mman_upstream::i915_gem_mmap_offset_ioctl(drm_dev, data, drm_file),
            0x29 => crate::i915_gem_execbuffer_upstream::i915_gem_execbuffer2_ioctl(drm_dev, data, drm_file),
            0x2d => crate::i915_gem_context_upstream::i915_gem_context_create_ioctl(drm_dev, data, drm_file),
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

unsafe fn i915_reg_read_ioctl(dev: *mut c_void, data: *mut c_void) -> c_int {
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
