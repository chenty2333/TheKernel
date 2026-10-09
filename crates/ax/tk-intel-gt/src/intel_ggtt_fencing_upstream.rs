// SPDX-License-Identifier: MIT
// Copyright © 2008-2015 Intel Corporation.
//
//! Source-order transcription of Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/intel_ggtt_fencing.c`.
//!
//! The MIT grant is explicit in the source. Fence-register state machines,
//! lock order, lazy MMIO writes, bit-6 detection and page bit-17 repair are
//! kept in upstream order. The canonical owner records are imported below;
//! The complete source-owned `I915FenceReg` and `I915Ggtt` layouts are
//! imported from their MIT header bindings. Remaining API gaps include
//! `i915_active_{init,fini,wait,is_idle}`, `i915_vm_to_ggtt`,
//! `i915_ggtt_has_aperture`, `intel_vgpu_active`,
//! `vgtif_reg_avail_rs_fence_num`, `intel_has_pending_fb_unpin`,
//! `linux::mutex::{mutex_lock_interruptible,mutex_unlock}`,
//! `linux::pm::{intel_runtime_pm_get_if_active,assert_rpm_wakelock_held}`,
//! platform predicates `IS_G33`, `IS_I945G`, `IS_I945GM`, and `IS_PINEVIEW`,
//! plus `linux::{highmem::{kmap_local_page,kunmap_local},
//! page::{page_to_phys,set_page_dirty},scatterlist::sg_table_pages}`. The
//! source's MCH/display register definitions are transcribed locally where
//! used. No stub behavior is substituted for these kernel operations.

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]
#![allow(unsafe_code)]

use core::{
    ffi::{c_ulong, c_void},
    mem::size_of,
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::{
    i915_gem_object_types_upstream::{DrmI915GemObject, Page},
    i915_gem_tiling_upstream::{
        I915_BIT_6_SWIZZLE_9, I915_BIT_6_SWIZZLE_9_10, I915_BIT_6_SWIZZLE_9_10_17,
        I915_BIT_6_SWIZZLE_9_17, I915_BIT_6_SWIZZLE_NONE, I915_BIT_6_SWIZZLE_UNKNOWN,
        I915_TILING_Y,
    },
    i915_vma_api_upstream::*,
    i915_vma_types_upstream::I915Vma,
    intel_context_types_upstream::{I915Active, IntelWakerefT},
    intel_context_upstream::SgTable,
    intel_engine_api_upstream::LockClassKey,
    intel_ggtt_fencing_types_upstream::I915FenceReg,
    intel_gt_types_upstream::IntelGt,
    intel_gtt_api_upstream::{I915Ggtt, i915_ggtt_has_aperture, i915_vm_to_ggtt},
    intel_runtime_pm_upstream::assert_rpm_wakelock_held,
    intel_uncore_types_upstream::{
        IntelRuntimePm, IntelUncore, intel_uncore_posting_read_fw, intel_uncore_read,
        intel_uncore_read16, intel_uncore_rmw, intel_uncore_write, intel_uncore_write_fw,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        bits::{lower_32_bits, upper_32_bits},
        i915::{GRAPHICS_VER, to_gt},
        memory::{atomic_dec, atomic_inc, atomic_read, kfree, kzalloc_objs_flags},
        pm::intel_runtime_pm_put,
    },
    linux_config::{GFP_KERNEL, PAGE_SIZE},
};

const pipelined: bool = false;
const EINVAL: i32 = 22;
const EAGAIN: i32 = 11;
const ENOBUFS: i32 = 105;
const ENOSPC: i32 = 28;
const I965_FENCE_PAGE: u32 = 4096;
const I965_FENCE_PITCH_SHIFT: u32 = 2;
const I965_FENCE_TILING_Y_SHIFT: u32 = 1;
const GEN6_FENCE_PITCH_SHIFT: u32 = 32;
const I830_FENCE_TILING_Y_SHIFT: u32 = 12;
const I830_FENCE_PITCH_SHIFT: u32 = 4;
const I830_FENCE_REG_VALID: u32 = 1;
const I965_FENCE_REG_VALID: u64 = 1;
const DCC_ADDRESSING_MODE_SINGLE_CHANNEL: u32 = 0;
const DCC_ADDRESSING_MODE_DUAL_CHANNEL_ASYMMETRIC: u32 = 1;
const DCC_ADDRESSING_MODE_DUAL_CHANNEL_INTERLEAVED: u32 = 2;
const DCC_ADDRESSING_MODE_MASK: u32 = 3;
const DCC_CHANNEL_XOR_DISABLE: u32 = 1 << 10;
const DCC_CHANNEL_XOR_BIT_17: u32 = 1 << 9;
const DCC2_MODIFIED_ENHANCED_DISABLE: u32 = 1 << 20;
const MAD_DIMM_A_SIZE_MASK: u32 = 0xff;
const MAD_DIMM_B_SIZE_MASK: u32 = 0xff << 8;
const INTEL_I945G: u32 = 7;
const INTEL_I945GM: u32 = 8;
const INTEL_G33: u32 = 9;
const INTEL_PINEVIEW: u32 = 10;
const I915_BIT_6_SWIZZLE_9_11: u32 = 3;
const I915_BIT_6_SWIZZLE_9_10_11: u32 = 4;
const I915_BIT_6_SWIZZLE_9_11_17: u32 = 8;
const I915_BIT_6_SWIZZLE_9_10_11_17: u32 = 9;
const GEM_QUIRK_PIN_SWIZZLED_PAGES: u32 = 1 << 0;
const PAGE_SHIFT: u32 = 12;

const DISP_ARB_CTL: I915RegT = I915RegT { reg: 0x45000 };
const DISP_TILE_SURFACE_SWIZZLING: u32 = 1 << 13;
const TILECTL: I915RegT = I915RegT { reg: 0x101000 };
const TILECTL_SWZCTL: u32 = 1;
const ARB_MODE: I915RegT = I915RegT { reg: 0x4030 };
const ARB_MODE_SWIZZLE_SNB: u32 = 1 << 4;
const ARB_MODE_SWIZZLE_IVB: u32 = 1 << 5;
const GAMTARBMODE: I915RegT = I915RegT { reg: 0x4a08 };
const ARB_MODE_SWIZZLE_BDW: u32 = 1 << 1;
const DCC: I915RegT = I915RegT { reg: 0x10200 };
const DCC2: I915RegT = I915RegT { reg: 0x10204 };
const C0DRB3_BW: I915RegT = I915RegT { reg: 0x10206 };
const C1DRB3_BW: I915RegT = I915RegT { reg: 0x10606 };
const MAD_DIMM_C0: I915RegT = I915RegT { reg: 0x145004 };
const MAD_DIMM_C1: I915RegT = I915RegT { reg: 0x145008 };
const VGTIF_AVAIL_RS_FENCE_NUM: I915RegT = I915RegT { reg: 0x78050 };

static mut I915_FENCE_ACTIVE_MUTEX_KEY: LockClassKey = unsafe { core::mem::zeroed() };
static mut I915_FENCE_ACTIVE_WAIT_KEY: LockClassKey = unsafe { core::mem::zeroed() };

unsafe extern "C" {
    // These are out-of-line Linux/i915 functions. The corresponding
    // `i915_active_*` header macros are expanded below, not declared as C
    // symbols.
    fn __i915_active_init(
        active: *mut I915Active,
        active_cb: Option<unsafe extern "C" fn(*mut I915Active) -> i32>,
        retire_cb: Option<unsafe extern "C" fn(*mut I915Active)>,
        flags: c_ulong,
        mutex_key: *mut LockClassKey,
        wait_key: *mut LockClassKey,
    );
    fn __i915_active_wait(active: *mut I915Active, state: i32) -> i32;
    fn i915_active_fini(active: *mut I915Active);
    fn intel_runtime_pm_get_if_in_use(rpm: *mut IntelRuntimePm) -> IntelWakerefT;
    fn intel_runtime_pm_get_if_active(rpm: *mut IntelRuntimePm) -> IntelWakerefT;
    fn intel_has_pending_fb_unpin(display: *mut c_void) -> bool;
    fn intel_vgpu_active(i915: *mut crate::linux_i915_private::DrmI915Private) -> bool;
    fn mutex_lock_interruptible(mutex: *mut crate::intel_engine_cs_upstream::Mutex) -> i32;
    fn set_page_dirty(page: *mut Page) -> bool;
}

#[inline]
fn reg(offset: u32) -> I915RegT {
    I915RegT { reg: offset }
}

#[inline]
fn fence_reg(index: i32) -> I915RegT {
    reg(0x2000 + (((index as u32) & 8) << 9) + (((index as u32) & 7) * 4))
}

#[inline]
fn fence_reg_965_lo(index: i32) -> I915RegT {
    reg(0x3000 + (index as u32) * 8)
}

#[inline]
fn fence_reg_965_hi(index: i32) -> I915RegT {
    reg(0x3004 + (index as u32) * 8)
}

#[inline]
fn fence_reg_gen6_lo(index: i32) -> I915RegT {
    reg(0x100000 + (index as u32) * 8)
}

#[inline]
fn fence_reg_gen6_hi(index: i32) -> I915RegT {
    reg(0x100004 + (index as u32) * 8)
}

#[inline]
fn is_power_of_2(value: u32) -> bool {
    value.is_power_of_two()
}

#[inline]
fn i915_fence_size_bits(size: u32) -> u32 {
    let size = size >> 20;
    if size == 0 {
        u32::MAX & !0xff
    } else {
        size.trailing_zeros() << 8
    }
}

#[inline]
fn i830_fence_size_bits(size: u32) -> u32 {
    let size = size >> 19;
    if size == 0 {
        u32::MAX & !0xff
    } else {
        size.trailing_zeros() << 8
    }
}

#[inline]
fn reg_masked_field_enable(mask: u32) -> u32 {
    (mask << 16) | mask
}

#[inline]
unsafe fn i915_active_wait(active: &mut I915Active) -> i32 {
    unsafe {
        __i915_active_wait(
            active as *mut I915Active,
            crate::linux::wait::TASK_INTERRUPTIBLE as i32,
        )
    }
}

#[inline]
unsafe fn i915_fence_active_init(active: *mut I915Active) {
    // Equivalent to the header macro's per-call-site lock classes. LOCKDEP is
    // disabled in the selected target, so their opaque keys are not inspected.
    unsafe {
        __i915_active_init(
            active,
            None,
            None,
            0,
            core::ptr::addr_of_mut!(I915_FENCE_ACTIVE_MUTEX_KEY),
            core::ptr::addr_of_mut!(I915_FENCE_ACTIVE_WAIT_KEY),
        )
    }
}

/// `fence_to_i915()` (`intel_ggtt_fencing.c:52`).
// upstream: intel_ggtt_fencing.c fence_to_i915()
unsafe fn fence_to_i915(
    fence: *mut I915FenceReg,
) -> *mut crate::linux_i915_private::DrmI915Private {
    unsafe { (*(*fence).ggtt).vm.i915 }
}

/// `fence_to_uncore()` (`intel_ggtt_fencing.c:57`).
// upstream: intel_ggtt_fencing.c fence_to_uncore()
unsafe fn fence_to_uncore(fence: *mut I915FenceReg) -> *mut IntelUncore {
    unsafe { (*(*(*fence).ggtt).vm.gt).uncore }
}

/// `i965_write_fence_reg()` (`intel_ggtt_fencing.c:62`).
// upstream: intel_ggtt_fencing.c i965_write_fence_reg()
unsafe fn i965_write_fence_reg(fence: *mut I915FenceReg) {
    let graphics_ver = unsafe { GRAPHICS_VER(fence_to_i915(fence)) };
    let (fence_reg_lo, fence_reg_hi, fence_pitch_shift) = if graphics_ver >= 6 {
        (
            fence_reg_gen6_lo(unsafe { (*fence).id }),
            fence_reg_gen6_hi(unsafe { (*fence).id }),
            GEN6_FENCE_PITCH_SHIFT,
        )
    } else {
        (
            fence_reg_965_lo(unsafe { (*fence).id }),
            fence_reg_965_hi(unsafe { (*fence).id }),
            I965_FENCE_PITCH_SHIFT,
        )
    };

    let mut val = 0u64;
    if unsafe { (*fence).tiling } != 0 {
        let stride = unsafe { (*fence).stride };
        GEM_BUG_ON!(!crate::linux::bits::IS_ALIGNED(stride, 128u32));

        val = (unsafe { (*fence).start }
            .wrapping_add(unsafe { (*fence).size })
            .wrapping_sub(I965_FENCE_PAGE)) as u64;
        val <<= 32;
        val |= unsafe { (*fence).start as u64 };
        val |= (stride.wrapping_div(128).wrapping_sub(1) as u64) << fence_pitch_shift;
        if unsafe { (*fence).tiling } == I915_TILING_Y {
            val |= 1u64 << I965_FENCE_TILING_Y_SHIFT;
        }
        val |= I965_FENCE_REG_VALID;
    }

    if !pipelined {
        let uncore = unsafe { fence_to_uncore(fence) };

        // The disabled->high->low write order and posting reads prevent a
        // partially programmed 64-bit fence from becoming visible to HW.
        unsafe {
            intel_uncore_write_fw(uncore, fence_reg_lo, 0);
            intel_uncore_posting_read_fw(uncore, fence_reg_lo);
            intel_uncore_write_fw(uncore, fence_reg_hi, upper_32_bits(val));
            intel_uncore_write_fw(uncore, fence_reg_lo, lower_32_bits(val));
            intel_uncore_posting_read_fw(uncore, fence_reg_lo);
        }
    }
}

/// `i915_write_fence_reg()` (`intel_ggtt_fencing.c:116`).
// upstream: intel_ggtt_fencing.c i915_write_fence_reg()
unsafe fn i915_write_fence_reg(fence: *mut I915FenceReg) {
    let mut val = 0u32;
    if unsafe { (*fence).tiling } != 0 {
        let stride = unsafe { (*fence).stride };
        let tiling = unsafe { (*fence).tiling };
        let is_y_tiled = tiling == I915_TILING_Y;
        let stride = if is_y_tiled
            && unsafe { crate::linux::i915::HAS_128_BYTE_Y_TILING(fence_to_i915(fence)) }
        {
            stride / 128
        } else {
            stride / 512
        };
        GEM_BUG_ON!(!is_power_of_2(stride));

        val = unsafe { (*fence).start };
        if is_y_tiled {
            val |= 1 << I830_FENCE_TILING_Y_SHIFT;
        }
        val |= i915_fence_size_bits(unsafe { (*fence).size });
        val |= stride.trailing_zeros() << I830_FENCE_PITCH_SHIFT;
        val |= I830_FENCE_REG_VALID;
    }

    if !pipelined {
        let uncore = unsafe { fence_to_uncore(fence) };
        let reg = fence_reg(unsafe { (*fence).id });
        unsafe {
            intel_uncore_write_fw(uncore, reg, val);
            intel_uncore_posting_read_fw(uncore, reg);
        }
    }
}

/// `i830_write_fence_reg()` (`intel_ggtt_fencing.c:150`).
// upstream: intel_ggtt_fencing.c i830_write_fence_reg()
unsafe fn i830_write_fence_reg(fence: *mut I915FenceReg) {
    let mut val = 0u32;
    if unsafe { (*fence).tiling } != 0 {
        let stride = unsafe { (*fence).stride };
        val = unsafe { (*fence).start };
        if unsafe { (*fence).tiling } == I915_TILING_Y {
            val |= 1 << I830_FENCE_TILING_Y_SHIFT;
        }
        val |= i830_fence_size_bits(unsafe { (*fence).size });
        val |= (stride / 128).trailing_zeros() << I830_FENCE_PITCH_SHIFT;
        val |= I830_FENCE_REG_VALID;
    }

    if !pipelined {
        let uncore = unsafe { fence_to_uncore(fence) };
        let reg = fence_reg(unsafe { (*fence).id });
        unsafe {
            intel_uncore_write_fw(uncore, reg, val);
            intel_uncore_posting_read_fw(uncore, reg);
        }
    }
}

/// `fence_write()` (`intel_ggtt_fencing.c:175`).
// upstream: intel_ggtt_fencing.c fence_write()
unsafe fn fence_write(fence: *mut I915FenceReg) {
    let i915 = unsafe { fence_to_i915(fence) };
    match unsafe { GRAPHICS_VER(i915) } {
        2 => unsafe { i830_write_fence_reg(fence) },
        3 => unsafe { i915_write_fence_reg(fence) },
        _ => unsafe { i965_write_fence_reg(fence) },
    }
}

/// `gpu_uses_fence_registers()` (`intel_ggtt_fencing.c:198`).
// upstream: intel_ggtt_fencing.c gpu_uses_fence_registers()
unsafe fn gpu_uses_fence_registers(fence: *mut I915FenceReg) -> bool {
    unsafe { GRAPHICS_VER(fence_to_i915(fence)) < 4 }
}

/// `fence_update()` (`intel_ggtt_fencing.c:203`).
// upstream: intel_ggtt_fencing.c fence_update()
unsafe fn fence_update(fence: *mut I915FenceReg, vma: *mut I915Vma) -> i32 {
    let ggtt = unsafe { (*fence).ggtt };
    let uncore = unsafe { fence_to_uncore(fence) };
    let mut ret: i32;

    unsafe { (*fence).tiling = 0 };
    if !vma.is_null() {
        GEM_BUG_ON!(
            crate::linux::fields::i915_gem_object_get_stride(unsafe { (*vma).obj }) == 0
                || crate::linux::fields::i915_gem_object_get_tiling(unsafe { (*vma).obj }) == 0
        );

        if !unsafe { i915_vma_is_map_and_fenceable(vma) } {
            return -EINVAL;
        }

        if unsafe { gpu_uses_fence_registers(fence) } {
            // Implicit unfenced GPU blits must complete before ownership moves.
            ret = unsafe { i915_vma_sync(vma) };
            if ret != 0 {
                return ret;
            }
        }

        GEM_BUG_ON!(unsafe { (*vma).fence_size > (*vma).size as u32 });
        unsafe {
            (*fence).start = crate::linux::i915::i915_ggtt_offset(vma);
            (*fence).size = (*vma).fence_size;
            (*fence).stride = crate::linux::fields::i915_gem_object_get_stride((*vma).obj);
            (*fence).tiling = crate::linux::fields::i915_gem_object_get_tiling((*vma).obj);
        }
    }
    WRITE_ONCE!((*fence).dirty, false);

    // xchg provides the same atomic ownership hand-off as the C helper.
    let old = unsafe {
        (&*ptr::addr_of_mut!((*fence).vma).cast::<AtomicPtr<I915Vma>>())
            .swap(ptr::null_mut(), Ordering::SeqCst)
    };
    if !old.is_null() {
        ret = unsafe { i915_active_wait(&mut (*fence).active) };
        if ret != 0 {
            unsafe { (*fence).vma = old };
            return ret;
        }

        unsafe { i915_vma_flush_writes(old) };
        if old != vma {
            GEM_BUG_ON!(unsafe { (*old).fence != fence });
            unsafe {
                i915_vma_revoke_mmap(old);
                (*old).fence = ptr::null_mut();
            }
        }
        unsafe { crate::linux_list::list_move(&mut (*fence).link, &mut (*ggtt).fence_list) };
    }

    let rpm = unsafe { (*uncore).rpm };
    let wakeref = unsafe { intel_runtime_pm_get_if_in_use(rpm) };
    if wakeref.is_null() {
        GEM_BUG_ON!(!vma.is_null());
        return 0;
    }

    unsafe {
        (*fence).vma = vma;
        fence_write(fence);
    }
    if !vma.is_null() {
        unsafe {
            (*vma).fence = fence;
            crate::linux_list::list_move_tail(&mut (*fence).link, &mut (*ggtt).fence_list);
        }
    }
    unsafe { intel_runtime_pm_put(rpm, wakeref) };
    0
}

/// `i915_vma_revoke_fence()` (`intel_ggtt_fencing.c:294`).
// upstream: intel_ggtt_fencing.c i915_vma_revoke_fence()
pub unsafe fn i915_vma_revoke_fence(vma: *mut I915Vma) {
    let fence = unsafe { (*vma).fence };
    if fence.is_null() {
        return;
    }

    lockdep_assert_held!(unsafe { &(*(*vma).vm).mutex });
    GEM_BUG_ON!(unsafe { (*fence).vma != vma });
    let _ = unsafe { i915_active_wait(&mut (*fence).active) };
    GEM_BUG_ON!(!unsafe { crate::linux::requests::i915_active_is_idle(&mut (*fence).active) });
    GEM_BUG_ON!(atomic_read(unsafe { &(*fence).pin_count }) != 0);

    unsafe {
        (*fence).tiling = 0;
        WRITE_ONCE!((*fence).vma, ptr::null_mut());
        (*vma).fence = ptr::null_mut();
    }

    // `with_intel_runtime_pm_if_active` holds a temporary reference only while
    // the device was active, then drops it on the single loop exit path.
    let rpm = unsafe { (*(*(*(*vma).vm).gt).uncore).rpm };
    let wakeref = unsafe { intel_runtime_pm_get_if_active(rpm) };
    if !wakeref.is_null() {
        unsafe {
            fence_write(fence);
            intel_runtime_pm_put(rpm, wakeref);
        }
    }
}

/// `fence_is_active()` (`intel_ggtt_fencing.c:327`).
// upstream: intel_ggtt_fencing.c fence_is_active()
unsafe fn fence_is_active(fence: *const I915FenceReg) -> bool {
    unsafe { !(*fence).vma.is_null() && i915_vma_is_active((*fence).vma) }
}

/// `fence_find()` (`intel_ggtt_fencing.c:332`).
// upstream: intel_ggtt_fencing.c fence_find()
unsafe fn fence_find(ggtt: *mut I915Ggtt) -> *mut I915FenceReg {
    let display = unsafe { (*(*ggtt).vm.i915).display };
    let mut active: *mut I915FenceReg = ptr::null_mut();
    let (mut fence, mut next): (*mut I915FenceReg, *mut I915FenceReg) =
        (ptr::null_mut(), ptr::null_mut());

    list_for_each_entry_safe!(fence, next, unsafe { &mut (*ggtt).fence_list }, link, {
        GEM_BUG_ON!(unsafe { !(*fence).vma.is_null() && (*(*fence).vma).fence != fence });

        if fence == active {
            active = ERR_PTR!(-EAGAIN);
        }

        let saw_active_twice = !active.is_null() && IS_ERR!(active) && PTR_ERR!(active) == -EAGAIN;
        if !saw_active_twice && unsafe { fence_is_active(fence) } {
            if active.is_null() {
                active = fence;
            }
            unsafe {
                crate::linux_list::list_move_tail(&mut (*fence).link, &mut (*ggtt).fence_list)
            };
            continue;
        }

        if atomic_read(unsafe { &(*fence).pin_count }) != 0 {
            continue;
        }

        return fence;
    });

    // Pending display flips temporarily consume fences even when the LRU is
    // otherwise exhausted; report retry rather than permanent allocation loss.
    if unsafe { intel_has_pending_fb_unpin(display) } {
        return ERR_PTR!(-EAGAIN);
    }
    ERR_PTR!(-ENOBUFS)
}

/// `__i915_vma_pin_fence()` (`intel_ggtt_fencing.c:366`).
// upstream: intel_ggtt_fencing.c __i915_vma_pin_fence()
pub unsafe fn __i915_vma_pin_fence(vma: *mut I915Vma) -> i32 {
    let ggtt = unsafe { i915_vm_to_ggtt((*vma).vm) };
    let mut fence: *mut I915FenceReg;
    let set = if unsafe { crate::linux::fields::i915_gem_object_is_tiled((*vma).obj) } {
        vma
    } else {
        ptr::null_mut()
    };
    let mut err: i32;

    lockdep_assert_held!(unsafe { &(*(*vma).vm).mutex });

    if !unsafe { (*vma).fence }.is_null() {
        fence = unsafe { (*vma).fence };
        GEM_BUG_ON!(unsafe { (*fence).vma != vma });
        atomic_inc(unsafe { &mut (*fence).pin_count });
        if !unsafe { (*fence).dirty } {
            unsafe {
                crate::linux_list::list_move_tail(&mut (*fence).link, &mut (*ggtt).fence_list)
            };
            return 0;
        }
    } else if !set.is_null() {
        fence = unsafe { fence_find(ggtt) };
        if IS_ERR!(fence) {
            return PTR_ERR!(fence);
        }
        GEM_BUG_ON!(atomic_read(unsafe { &(*fence).pin_count }) != 0);
        atomic_inc(unsafe { &mut (*fence).pin_count });
    } else {
        return 0;
    }

    err = unsafe { fence_update(fence, set) };
    if err != 0 {
        atomic_dec(unsafe { &mut (*fence).pin_count });
        return err;
    }

    GEM_BUG_ON!(unsafe { (*fence).vma != set });
    GEM_BUG_ON!(unsafe {
        (*vma).fence
            != if set.is_null() {
                ptr::null_mut()
            } else {
                fence
            }
    });
    if !set.is_null() {
        return 0;
    }

    atomic_dec(unsafe { &mut (*fence).pin_count });
    err
}

/// `i915_vma_pin_fence()` (`intel_ggtt_fencing.c:427`).
// upstream: intel_ggtt_fencing.c i915_vma_pin_fence()
pub unsafe fn i915_vma_pin_fence(vma: *mut I915Vma) -> i32 {
    if unsafe {
        (*vma).fence.is_null() && !crate::linux::fields::i915_gem_object_is_tiled((*vma).obj)
    } {
        return 0;
    }

    assert_rpm_wakelock_held(unsafe { (*(*(*(*vma).vm).gt).uncore).rpm });
    GEM_BUG_ON!(!unsafe { i915_vma_is_ggtt(vma) });

    let ret = unsafe { mutex_lock_interruptible(&mut (*(*vma).vm).mutex) };
    if ret != 0 {
        return ret;
    }
    let ret = unsafe { __i915_vma_pin_fence(vma) };
    unsafe { crate::linux::mutex::mutex_unlock(&mut (*(*vma).vm).mutex) };
    ret
}

/// `i915_reserve_fence()` (`intel_ggtt_fencing.c:458`).
// upstream: intel_ggtt_fencing.c i915_reserve_fence()
pub unsafe fn i915_reserve_fence(ggtt: *mut I915Ggtt) -> *mut I915FenceReg {
    let mut count = 0;
    let mut fence: *mut I915FenceReg = ptr::null_mut();
    lockdep_assert_held!(unsafe { &(*ggtt).vm.mutex });

    list_for_each_entry!(fence, unsafe { &mut (*ggtt).fence_list }, link, {
        count += (atomic_read(unsafe { &(*fence).pin_count }) == 0) as i32;
    });
    if count <= 1 {
        return ERR_PTR!(-ENOSPC);
    }

    fence = unsafe { fence_find(ggtt) };
    if IS_ERR!(fence) {
        return fence;
    }

    if !unsafe { (*fence).vma.is_null() } {
        let ret = unsafe { fence_update(fence, ptr::null_mut()) };
        if ret != 0 {
            return ERR_PTR!(ret);
        }
    }

    unsafe { crate::linux_list::list_del(&mut (*fence).link) };
    fence
}

/// `i915_unreserve_fence()` (`intel_ggtt_fencing.c:495`).
// upstream: intel_ggtt_fencing.c i915_unreserve_fence()
pub unsafe fn i915_unreserve_fence(fence: *mut I915FenceReg) {
    let ggtt = unsafe { (*fence).ggtt };
    lockdep_assert_held!(unsafe { &(*ggtt).vm.mutex });
    unsafe { crate::linux_list::list_add(&mut (*fence).link, &mut (*ggtt).fence_list) };
}

/// `intel_ggtt_restore_fences()` (`intel_ggtt_fencing.c:512`).
// upstream: intel_ggtt_fencing.c intel_ggtt_restore_fences()
pub unsafe fn intel_ggtt_restore_fences(ggtt: *mut I915Ggtt) {
    for i in 0..unsafe { (*ggtt).num_fences } {
        unsafe { fence_write((*ggtt).fence_regs.add(i as usize)) };
    }
}

/// `detect_bit_6_swizzle()` (`intel_ggtt_fencing.c:575`).
// upstream: intel_ggtt_fencing.c detect_bit_6_swizzle()
unsafe fn detect_bit_6_swizzle(ggtt: *mut I915Ggtt) {
    let uncore = unsafe { (*(*ggtt).vm.gt).uncore };
    let i915 = unsafe { (*ggtt).vm.i915 };
    let mut swizzle_x = I915_BIT_6_SWIZZLE_UNKNOWN;
    let mut swizzle_y = I915_BIT_6_SWIZZLE_UNKNOWN;

    if unsafe { GRAPHICS_VER(i915) >= 8 || crate::linux::i915::IS_VALLEYVIEW(i915) } {
        // BDW+, VLV and CHV do not use GPU-side bit-6 swizzling.
        swizzle_x = I915_BIT_6_SWIZZLE_NONE;
        swizzle_y = I915_BIT_6_SWIZZLE_NONE;
    } else if unsafe { GRAPHICS_VER(i915) >= 6 } {
        if unsafe { (*i915).preserve_bios_swizzle } {
            if unsafe { intel_uncore_read(uncore, DISP_ARB_CTL) & DISP_TILE_SURFACE_SWIZZLING != 0 }
            {
                swizzle_x = I915_BIT_6_SWIZZLE_9_10;
                swizzle_y = I915_BIT_6_SWIZZLE_9;
            } else {
                swizzle_x = I915_BIT_6_SWIZZLE_NONE;
                swizzle_y = I915_BIT_6_SWIZZLE_NONE;
            }
        } else {
            let mut dimm_c0 = unsafe { intel_uncore_read(uncore, MAD_DIMM_C0) };
            let mut dimm_c1 = unsafe { intel_uncore_read(uncore, MAD_DIMM_C1) };
            dimm_c0 &= MAD_DIMM_A_SIZE_MASK | MAD_DIMM_B_SIZE_MASK;
            dimm_c1 &= MAD_DIMM_A_SIZE_MASK | MAD_DIMM_B_SIZE_MASK;
            if dimm_c0 == dimm_c1 {
                swizzle_x = I915_BIT_6_SWIZZLE_9_10;
                swizzle_y = I915_BIT_6_SWIZZLE_9;
            } else {
                swizzle_x = I915_BIT_6_SWIZZLE_NONE;
                swizzle_y = I915_BIT_6_SWIZZLE_NONE;
            }
        }
    } else if unsafe { GRAPHICS_VER(i915) == 5 } {
        swizzle_x = I915_BIT_6_SWIZZLE_9_10;
        swizzle_y = I915_BIT_6_SWIZZLE_9;
    } else if unsafe { GRAPHICS_VER(i915) == 2 } {
        swizzle_x = I915_BIT_6_SWIZZLE_NONE;
        swizzle_y = I915_BIT_6_SWIZZLE_NONE;
    } else if unsafe {
        crate::linux::i915::IS_G4X(i915)
            || crate::linux::i915::IS_I965G(i915)
            || crate::linux::i915::IS_PLATFORM(i915, INTEL_G33)
    } {
        if unsafe {
            intel_uncore_read16(uncore, C0DRB3_BW) == intel_uncore_read16(uncore, C1DRB3_BW)
        } {
            swizzle_x = I915_BIT_6_SWIZZLE_9_10;
            swizzle_y = I915_BIT_6_SWIZZLE_9;
        }
    } else {
        let dcc = unsafe { intel_uncore_read(uncore, DCC) };
        match dcc & DCC_ADDRESSING_MODE_MASK {
            DCC_ADDRESSING_MODE_SINGLE_CHANNEL | DCC_ADDRESSING_MODE_DUAL_CHANNEL_ASYMMETRIC => {
                swizzle_x = I915_BIT_6_SWIZZLE_NONE;
                swizzle_y = I915_BIT_6_SWIZZLE_NONE;
            }
            DCC_ADDRESSING_MODE_DUAL_CHANNEL_INTERLEAVED => {
                if dcc & DCC_CHANNEL_XOR_DISABLE != 0 {
                    swizzle_x = I915_BIT_6_SWIZZLE_9_10;
                    swizzle_y = I915_BIT_6_SWIZZLE_9;
                } else if dcc & DCC_CHANNEL_XOR_BIT_17 == 0 {
                    swizzle_x = I915_BIT_6_SWIZZLE_9_10_11;
                    swizzle_y = I915_BIT_6_SWIZZLE_9_11;
                } else {
                    swizzle_x = I915_BIT_6_SWIZZLE_9_10_17;
                    swizzle_y = I915_BIT_6_SWIZZLE_9_17;
                }
            }
            _ => {}
        }

        if unsafe { GRAPHICS_VER(i915) == 4 }
            && unsafe { intel_uncore_read(uncore, DCC2) & DCC2_MODIFIED_ENHANCED_DISABLE == 0 }
        {
            swizzle_x = I915_BIT_6_SWIZZLE_UNKNOWN;
            swizzle_y = I915_BIT_6_SWIZZLE_UNKNOWN;
        }

        if dcc == u32::MAX {
            drm_err!(
                &(*i915).drm,
                "Couldn't read from MCHBAR. Disabling tiling.\n"
            );
            swizzle_x = I915_BIT_6_SWIZZLE_UNKNOWN;
            swizzle_y = I915_BIT_6_SWIZZLE_UNKNOWN;
        }
    }

    if swizzle_x == I915_BIT_6_SWIZZLE_UNKNOWN || swizzle_y == I915_BIT_6_SWIZZLE_UNKNOWN {
        // Unknown mappings may depend on bit 17, so pinning swizzled pages is
        // required while userspace receives the historical compatibility lie.
        unsafe { (*i915).gem_quirks |= GEM_QUIRK_PIN_SWIZZLED_PAGES as c_ulong };
        swizzle_x = I915_BIT_6_SWIZZLE_NONE;
        swizzle_y = I915_BIT_6_SWIZZLE_NONE;
    }

    let gt = unsafe { to_gt(i915) };
    unsafe {
        (*(*gt).ggtt).bit_6_swizzle_x = swizzle_x;
        (*(*gt).ggtt).bit_6_swizzle_y = swizzle_y;
    }
}

/// `swizzle_page()` (`intel_ggtt_fencing.c:750`).
// upstream: intel_ggtt_fencing.c swizzle_page()
unsafe fn swizzle_page(page: *mut Page) {
    let mut temp = [0u8; 64];
    let vaddr = unsafe { crate::linux::highmem::kmap_local_page(page).cast::<u8>() };
    for i in (0..PAGE_SIZE).step_by(128) {
        unsafe {
            ptr::copy_nonoverlapping(vaddr.add(i), temp.as_mut_ptr(), 64);
            ptr::copy_nonoverlapping(vaddr.add(i + 64), vaddr.add(i), 64);
            ptr::copy_nonoverlapping(temp.as_ptr(), vaddr.add(i + 64), 64);
        }
    }
    unsafe { crate::linux::highmem::kunmap_local(vaddr.cast::<c_void>()) };
}

/// `i915_gem_object_do_bit_17_swizzle()` (`intel_ggtt_fencing.c:773`).
// upstream: intel_ggtt_fencing.c i915_gem_object_do_bit_17_swizzle()
pub unsafe fn i915_gem_object_do_bit_17_swizzle(obj: *mut DrmI915GemObject, pages: *mut SgTable) {
    let bitmap = unsafe { (*obj).bit_17 };
    if bitmap.is_null() {
        return;
    }

    let mut i = 0usize;
    for page in unsafe { crate::linux::scatterlist::sg_table_pages(pages) } {
        let new_bit_17 = unsafe { crate::linux::page::page_to_phys(page) >> 17 };
        let word_bits = size_of::<c_ulong>() * 8;
        let old =
            unsafe { *bitmap.add(i / word_bits) } & (1usize << (i % word_bits)) as c_ulong != 0;
        if (new_bit_17 & 1 != 0) != old {
            unsafe {
                swizzle_page(page);
                set_page_dirty(page);
            }
        }
        i += 1;
    }
}

/// `i915_gem_object_save_bit_17_swizzle()` (`intel_ggtt_fencing.c:809`).
// upstream: intel_ggtt_fencing.c i915_gem_object_save_bit_17_swizzle()
pub unsafe fn i915_gem_object_save_bit_17_swizzle(obj: *mut DrmI915GemObject, pages: *mut SgTable) {
    let gem_obj = unsafe { crate::i915_gem_object_types_upstream::intel_bo_to_drm_bo(obj) };
    let page_count = (unsafe { (*gem_obj).size } >> PAGE_SHIFT) as usize;
    let mut bitmap = unsafe { (*obj).bit_17 };
    if bitmap.is_null() {
        let bits_per_word = size_of::<c_ulong>() * 8;
        bitmap = kzalloc_objs_flags::<c_ulong, _>(page_count.div_ceil(bits_per_word), GFP_KERNEL);
        if bitmap.is_null() {
            drm_err!(
                (*gem_obj).dev,
                "Failed to allocate memory for bit 17 record\n"
            );
            return;
        }
        unsafe { (*obj).bit_17 = bitmap };
    }

    let word_bits = size_of::<c_ulong>() * 8;
    let mut i = 0usize;
    for page in unsafe { crate::linux::scatterlist::sg_table_pages(pages) } {
        let word = unsafe { &mut *bitmap.add(i / word_bits) };
        if unsafe { crate::linux::page::page_to_phys(page) } & (1 << 17) != 0 {
            *word |= (1usize << (i % word_bits)) as c_ulong;
        } else {
            *word &= !((1usize << (i % word_bits)) as c_ulong);
        }
        i += 1;
    }
}

/// `intel_ggtt_init_fences()` (`intel_ggtt_fencing.c:842`).
// upstream: intel_ggtt_fencing.c intel_ggtt_init_fences()
pub unsafe fn intel_ggtt_init_fences(ggtt: *mut I915Ggtt) {
    let i915 = unsafe { (*ggtt).vm.i915 };
    let uncore = unsafe { (*(*ggtt).vm.gt).uncore };
    unsafe {
        crate::linux_list::INIT_LIST_HEAD(&mut (*ggtt).fence_list);
        crate::linux_list::INIT_LIST_HEAD(&mut (*ggtt).userfault_list);
        detect_bit_6_swizzle(ggtt);
    }

    let mut num_fences;
    if !unsafe { i915_ggtt_has_aperture(ggtt) } {
        num_fences = 0;
    } else if unsafe { GRAPHICS_VER(i915) >= 7 }
        && !unsafe {
            crate::linux::i915::IS_VALLEYVIEW(i915) || crate::linux::i915::IS_CHERRYVIEW(i915)
        }
    {
        num_fences = 32;
    } else if unsafe { GRAPHICS_VER(i915) >= 4 }
        || unsafe {
            crate::linux::i915::IS_PLATFORM(i915, INTEL_I945G)
                || crate::linux::i915::IS_PLATFORM(i915, INTEL_I945GM)
                || crate::linux::i915::IS_PLATFORM(i915, INTEL_G33)
                || crate::linux::i915::IS_PLATFORM(i915, INTEL_PINEVIEW)
        }
    {
        num_fences = 16;
    } else {
        num_fences = 8;
    }

    if unsafe { intel_vgpu_active(i915) } {
        num_fences = unsafe { intel_uncore_read(uncore, VGTIF_AVAIL_RS_FENCE_NUM) as i32 };
    }

    unsafe {
        (*ggtt).fence_regs = kzalloc_objs_flags::<I915FenceReg, _>(num_fences, GFP_KERNEL);
    }
    if unsafe { (*ggtt).fence_regs.is_null() } {
        num_fences = 0;
    }

    for i in 0..num_fences {
        let fence = unsafe { (*ggtt).fence_regs.add(i as usize) };
        unsafe {
            i915_fence_active_init(core::ptr::addr_of_mut!((*fence).active));
            (*fence).ggtt = ggtt;
            (*fence).id = i;
            crate::linux_list::list_add_tail(&mut (*fence).link, &mut (*ggtt).fence_list);
        }
    }
    unsafe {
        (*ggtt).num_fences = num_fences as u32;
        intel_ggtt_restore_fences(ggtt);
    }
}

/// `intel_ggtt_fini_fences()` (`intel_ggtt_fencing.c:887`).
// upstream: intel_ggtt_fencing.c intel_ggtt_fini_fences()
pub unsafe fn intel_ggtt_fini_fences(ggtt: *mut I915Ggtt) {
    for i in 0..unsafe { (*ggtt).num_fences } {
        let fence = unsafe { (*ggtt).fence_regs.add(i as usize) };
        unsafe { i915_active_fini(&mut (*fence).active) };
    }
    unsafe { kfree((*ggtt).fence_regs) };
}

/// `intel_gt_init_swizzling()` (`intel_ggtt_fencing.c:900`).
// upstream: intel_ggtt_fencing.c intel_gt_init_swizzling()
pub unsafe fn intel_gt_init_swizzling(gt: *mut IntelGt) {
    let i915 = unsafe { (*gt).i915 };
    let uncore = unsafe { (*gt).uncore };
    let ggtt = unsafe { (*gt).ggtt };

    if unsafe { GRAPHICS_VER(i915) < 5 || (*ggtt).bit_6_swizzle_x == I915_BIT_6_SWIZZLE_NONE } {
        return;
    }

    unsafe { intel_uncore_rmw(uncore, DISP_ARB_CTL, 0, DISP_TILE_SURFACE_SWIZZLING) };
    if unsafe { GRAPHICS_VER(i915) == 5 } {
        return;
    }

    unsafe { intel_uncore_rmw(uncore, TILECTL, 0, TILECTL_SWZCTL) };
    match unsafe { GRAPHICS_VER(i915) } {
        6 => unsafe {
            intel_uncore_write(
                uncore,
                ARB_MODE,
                reg_masked_field_enable(ARB_MODE_SWIZZLE_SNB),
            )
        },
        7 => unsafe {
            intel_uncore_write(
                uncore,
                ARB_MODE,
                reg_masked_field_enable(ARB_MODE_SWIZZLE_IVB),
            )
        },
        8 => unsafe {
            intel_uncore_write(
                uncore,
                GAMTARBMODE,
                reg_masked_field_enable(ARB_MODE_SWIZZLE_BDW),
            )
        },
        version => MISSING_CASE!(version),
    }
}
