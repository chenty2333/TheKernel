// SPDX-License-Identifier: MIT
// Copyright © 2008-2018 Intel Corporation.
//
// Source-order translation of Linux v7.2.3
// drivers/gpu/drm/i915/gt/intel_reset.c.  Hardware sequencing is retained;
// lower Linux/DRM services not owned by this crate remain explicit externs.
#![allow(non_snake_case, non_camel_case_types, unsafe_op_in_unsafe_fn)]

use core::{
    ffi::{CStr, c_char, c_int, c_long, c_ulong, c_void},
    mem::offset_of,
};

use crate::{
    i915_gem_context_types_upstream::{I915GemContext, UCONTEXT_BANNABLE, UCONTEXT_RECOVERABLE},
    i915_request_types_upstream::I915Request,
    intel_context_upstream::DmaFence,
    intel_engine_cs_upstream::{
        ALL_ENGINES, AtomicT, DelayedWork, IntelEngineCs, IntelEngineMask, WorkStruct,
    },
    intel_engine_regs_upstream::*,
    intel_gt_api_upstream::{
        intel_gt_has_unrecoverable_error, intel_gt_init_hw, intel_gt_is_wedged,
    },
    intel_gt_types_upstream::IntelGt,
    intel_reset_types_upstream::{
        I915_RESET_BACKOFF, I915_RESET_ENGINE, I915_WEDGED, I915_WEDGED_ON_FINI,
        I915_WEDGED_ON_INIT,
    },
    intel_uc_types_upstream::intel_uc_uses_guc_submission,
    intel_uncore_types_upstream::{
        __intel_wait_for_register_fw, FORCEWAKE_ALL, intel_uncore_forcewake_get,
        intel_uncore_forcewake_put, intel_uncore_posting_read_fw, intel_uncore_read,
        intel_uncore_read_fw, intel_uncore_rmw, intel_uncore_rmw_fw, intel_uncore_write_fw,
    },
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        bits::{clear_bit, clear_bit_unlock, set_bit, test_and_set_bit, test_bit},
        i915::{
            GRAPHICS_VER, GRAPHICS_VER_FULL, HAS_ENGINE, INTEL_INFO, IP_VER, IS_DG2, MEDIA_VER_FULL,
        },
        locks::{spin_lock_irqsave_raw, spin_unlock_irqrestore_raw},
        memory::{atomic_add, atomic_inc, atomic_read, kref_get_unless_zero},
        pm::{intel_runtime_pm_get, intel_runtime_pm_put},
        primitives::{jiffies, udelay},
        rcu::{cond_synchronize_rcu, rcu_dereference, rcu_read_lock, rcu_read_unlock},
        srcu::{
            cleanup_srcu_struct, init_srcu_struct, srcu_read_lock, srcu_read_unlock,
            synchronize_srcu_expedited,
        },
        wait::{init_waitqueue_head, msleep, wake_up_all},
        workqueue::{INIT_DELAYED_WORK, INIT_WORK_C, cancel_delayed_work_sync, queue_delayed_work},
    },
    linux_config::{EAGAIN, EBUSY, EINTR, EIO, ENODEV, ETIMEDOUT, HZ, MAX_SCHEDULE_TIMEOUT},
    linux_i915_private::DrmI915Private,
};

const RESET_MAX_RETRIES: u32 = 3;
const VIDEO_DECODE_CLASS_U8: u8 = crate::intel_engine_types_upstream::VIDEO_DECODE_CLASS as u8;
const VIDEO_ENHANCEMENT_CLASS_U8: u8 =
    crate::intel_engine_types_upstream::VIDEO_ENHANCEMENT_CLASS as u8;
const GDRST: I915RegT = I915RegT { reg: 0x941c };
const ILK_GDSR: I915RegT = I915RegT { reg: 0x12ca4 };
const I915_GDRST: u32 = 0xc0;
const I915_GTT_VIEW_PARTIAL: u32 = 12;
const GRDOM_RESET_ENABLE: u8 = 1 << 0;
const GRDOM_RESET_STATUS: u8 = 1 << 1;
const GRDOM_RENDER: u8 = 1 << 2;
const GRDOM_MEDIA: u8 = 3 << 2;
const ILK_GRDOM_RENDER: u32 = 1 << 1;
const ILK_GRDOM_MEDIA: u32 = 3 << 1;
const GEN6_GRDOM_FULL: u32 = 1;
const GEN9_GRDOM_GUC: u32 = 1 << 5;
const GEN11_GRDOM_GUC: u32 = 1 << 3;
const GEN11_GRDOM_FULL: u32 = 1;
const GSC0: usize = crate::intel_engine_types_upstream::GSC0 as usize;
const I915_CLIENT_SCORE_HANG_FAST: i32 = 1;
const I915_CLIENT_SCORE_CONTEXT_BAN: i32 = 3;
const I915_CLIENT_FAST_HANG_JIFFIES: u64 = 60 * HZ as u64;
const CONTEXT_FAST_HANG_JIFFIES: u64 = 120 * HZ as u64;
const I915_ERROR_CAPTURE: u64 = crate::linux::registers::I915_ERROR_CAPTURE as u64;
const CORE_DUMP_FLAG_NONE: u32 = 0;
const TAINT_WARN: u32 = 9;
const KOBJ_CHANGE: i32 = 1;
const DRM_WEDGE_RECOVERY_REBIND: u32 = 1 << 1;
const DRM_WEDGE_RECOVERY_BUS_RESET: u32 = 1 << 2;

/// The accessed prefix of Linux `drm_i915_file_private` (source header layout).
#[repr(C)]
struct I915FilePrivateView {
    _i915: *mut DrmI915Private,
    _prefix: [u8; 88],
    _bsd_engine: u32,
    ban_score: AtomicT,
    hang_timestamp: c_ulong,
    _client: *mut c_void,
}

#[repr(C)]
pub struct IntelWedgeMe {
    work: DelayedWork,
    gt: *mut IntelGt,
    name: *const c_char,
}

/// Target-derived DRM node prefix: `drm_device.primary` follows its leading
/// refcount, bus devices, managed resources, driver and `dev_private` fields.
#[repr(C)]
struct DrmPrimaryView {
    _prefix: [u8; 72],
    primary: *mut DrmMinorView,
}

#[repr(C)]
struct DrmMinorView {
    _index: i32,
    _type: i32,
    kdev: *mut c_void,
}

#[repr(C)]
struct IntelSfcLockData {
    lock_reg: I915RegT,
    ack_reg: I915RegT,
    usage_reg: I915RegT,
    lock_bit: u32,
    ack_bit: u32,
    usage_bit: u32,
    reset_bit: u32,
}

const _: [(); 120] = [(); core::mem::size_of::<I915FilePrivateView>()];
const _: [(); 104] = [(); offset_of!(I915FilePrivateView, hang_timestamp)];
const _: [(); 72] = [(); offset_of!(DrmPrimaryView, primary)];
const _: [(); 8] = [(); offset_of!(DrmMinorView, kdev)];

#[inline]
fn reg(addr: u32) -> I915RegT {
    I915RegT { reg: addr }
}

#[inline]
unsafe fn engines(
    gt: *mut IntelGt,
    mask: IntelEngineMask,
) -> impl Iterator<Item = *mut IntelEngineCs> {
    unsafe { (*gt).engine }
        .into_iter()
        .filter(move |&engine| !engine.is_null() && unsafe { (*engine).mask & mask != 0 })
}

#[inline]
fn time_before(left: u64, right: u64) -> bool {
    (left.wrapping_sub(right) as i64) < 0
}

#[inline]
fn wait_for_atomic(mut condition: impl FnMut() -> bool, timeout_us: u32) -> c_int {
    if crate::linux::wait::wait_until(timeout_us as u64 * 1_000, &mut condition, false) {
        -ETIMEDOUT
    } else {
        0
    }
}

#[inline]
unsafe fn is_mock_gt(gt: *const IntelGt) -> bool {
    crate::linux::config::CONFIG_DRM_I915_SELFTEST
        && unsafe { (*gt).awake as usize == (-(ENODEV as isize)) as usize }
}

#[inline]
fn c_name(ctx: *mut I915GemContext) -> *const c_char {
    unsafe { (*ctx).name.as_ptr() }
}

#[inline]
unsafe fn drm_primary_kobj(i915: *mut DrmI915Private) -> *mut c_void {
    let drm = core::ptr::addr_of_mut!((*i915).drm).cast::<DrmPrimaryView>();
    let minor = unsafe { (*drm).primary };
    if minor.is_null() {
        core::ptr::null_mut()
    } else {
        unsafe { (*minor).kdev }
    }
}

#[inline]
unsafe fn log_c_string(message: *const c_char) -> alloc::string::String {
    if message.is_null() {
        return alloc::string::String::new();
    }
    unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned()
}

unsafe extern "C" {
    fn to_pci_dev(dev: *mut c_void) -> *mut c_void;
    fn pci_read_config_byte(pdev: *mut c_void, where_: u32, value: *mut u8) -> i32;
    fn pci_write_config_byte(pdev: *mut c_void, where_: u32, value: u8) -> i32;
    fn i915_ggtt_enable_hw(i915: *mut DrmI915Private) -> i32;
    fn intel_irq_suspend(i915: *mut DrmI915Private);
    fn intel_irq_resume(i915: *mut DrmI915Private);
    fn intel_overlay_reset(display: *mut c_void);
    fn intel_display_reset_test(display: *mut c_void) -> bool;
    fn intel_display_reset_supported(display: *mut c_void) -> bool;
    fn intel_display_reset_prepare(display: *mut c_void);
    fn intel_display_reset_finish(display: *mut c_void, reset: bool);
    fn add_taint_for_CI(i915: *mut DrmI915Private, flag: u32);
    fn clear_and_wake_up_bit(bit: c_int, word: *mut c_ulong);
    fn unmap_mapping_range(mapping: *mut c_void, start: c_long, length: c_long, even_cows: i32);
    fn dma_fence_default_wait(fence: *mut DmaFence, intr: bool, timeout: c_long) -> c_long;
    fn signal_pending_state(state: i32, task: *mut c_void) -> bool;
    fn kobject_uevent_env(kobj: *mut c_void, action: i32, envp: *const *const c_char) -> i32;
    fn drm_dev_wedged_event(drm: *mut c_void, recovery: u32, data: *mut c_void);
}

// upstream: intel_reset.c client_mark_guilty()
unsafe fn client_mark_guilty(ctx: *mut I915GemContext, banned: bool) {
    let file_priv = unsafe { (*ctx).file_priv };
    if file_priv.is_null() || (file_priv as isize) < 0 {
        return;
    }
    let view = file_priv.cast::<I915FilePrivateView>();
    let mut score = if banned {
        I915_CLIENT_SCORE_CONTEXT_BAN
    } else {
        0
    };
    let hang = unsafe {
        core::ptr::addr_of_mut!((*view).hang_timestamp).cast::<core::sync::atomic::AtomicU64>()
    };
    let now = jiffies();
    let prev = unsafe { &*hang }.swap(now, core::sync::atomic::Ordering::AcqRel);
    if time_before(now, prev.wrapping_add(I915_CLIENT_FAST_HANG_JIFFIES)) {
        score += I915_CLIENT_SCORE_HANG_FAST;
    }
    if score != 0 {
        unsafe { atomic_add(score, &mut (*view).ban_score) };
        axlog::debug!(
            "i915 client {:?}: gained {} ban score, now {}",
            unsafe { log_c_string(c_name(ctx)) },
            score,
            atomic_read(unsafe { &(*view).ban_score })
        );
    }
}

// upstream: intel_reset.c mark_guilty()
unsafe fn mark_guilty(rq: *mut I915Request) -> bool {
    let ce = unsafe { (*rq).context };
    if crate::intel_context_api_upstream::intel_context_is_closed(ce) {
        return true;
    }
    rcu_read_lock();
    let ctx = unsafe { rcu_dereference(core::ptr::addr_of!((*ce).gem_context)) };
    let ctx = if ctx.is_null() || !unsafe { kref_get_unless_zero(&mut (*ctx).r#ref) } {
        core::ptr::null_mut()
    } else {
        ctx
    };
    rcu_read_unlock();
    if ctx.is_null() {
        return crate::intel_context_api_upstream::intel_context_is_banned(ce);
    }
    unsafe { atomic_inc(&mut (*ctx).guilty_count) };
    if !crate::linux::bits::test_bit(UCONTEXT_BANNABLE, unsafe { &(*ctx).user_flags }) {
        unsafe { crate::i915_gem_context_upstream::i915_gem_context_put(ctx) };
        return false;
    }
    axlog::warn!("context reset due to GPU hang: {}", unsafe {
        log_c_string(c_name(ctx))
    });
    let prev = unsafe { (*ctx).hang_timestamp[0] as u64 };
    unsafe {
        (*ctx).hang_timestamp[0] = (*ctx).hang_timestamp[1];
        (*ctx).hang_timestamp[1] = jiffies() as c_ulong;
    }
    let now = jiffies();
    let mut banned =
        !crate::linux::bits::test_bit(UCONTEXT_RECOVERABLE, unsafe { &(*ctx).user_flags });
    if time_before(now, prev.wrapping_add(CONTEXT_FAST_HANG_JIFFIES)) {
        banned = true;
    }
    if banned {
        axlog::debug!("context guilty and banned: {}", unsafe {
            log_c_string(c_name(ctx))
        });
    }
    unsafe { client_mark_guilty(ctx, banned) };
    unsafe { crate::i915_gem_context_upstream::i915_gem_context_put(ctx) };
    banned
}

// upstream: intel_reset.c mark_innocent()
unsafe fn mark_innocent(rq: *mut I915Request) {
    let ce = unsafe { (*rq).context };
    rcu_read_lock();
    let ctx = unsafe { rcu_dereference(core::ptr::addr_of!((*ce).gem_context)) };
    if !ctx.is_null() {
        unsafe { atomic_inc(&mut (*ctx).active_count) };
    }
    rcu_read_unlock();
}

// upstream: intel_reset.c __i915_request_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __i915_request_reset(rq: *mut I915Request, guilty: bool) {
    assert!(!crate::linux::requests::__i915_request_is_complete(rq));
    rcu_read_lock();
    let banned = if guilty {
        unsafe { crate::i915_request_upstream::i915_request_set_error_once(rq, -EIO) };
        if !crate::linux::requests::i915_request_signaled(rq) {
            unsafe { crate::i915_request_upstream::__i915_request_skip(rq) };
        }
        unsafe { mark_guilty(rq) }
    } else {
        unsafe { crate::i915_request_upstream::i915_request_set_error_once(rq, -EAGAIN) };
        unsafe { mark_innocent(rq) };
        false
    };
    rcu_read_unlock();
    if banned {
        unsafe { crate::intel_context_upstream::intel_context_ban((*rq).context, rq) };
    }
}

// upstream: intel_reset.c i915_in_reset()
unsafe fn i915_in_reset(pdev: *mut c_void) -> bool {
    let mut status = 0;
    unsafe { pci_read_config_byte(pdev, I915_GDRST, &mut status) };
    status & GRDOM_RESET_STATUS != 0
}

// upstream: intel_reset.c i915_do_reset()
unsafe fn i915_do_reset(gt: *mut IntelGt, _engine_mask: IntelEngineMask, _retry: u32) -> c_int {
    let pdev = unsafe { to_pci_dev((*(*gt).i915).drm.dev) };
    unsafe { pci_write_config_byte(pdev, I915_GDRST, GRDOM_RESET_ENABLE) };
    udelay(50);
    let mut err = wait_for_atomic(|| unsafe { i915_in_reset(pdev) }, 50_000);
    unsafe { pci_write_config_byte(pdev, I915_GDRST, 0) };
    udelay(50);
    if err == 0 {
        err = wait_for_atomic(|| unsafe { !i915_in_reset(pdev) }, 50_000);
    }
    err
}

// upstream: intel_reset.c g4x_reset_complete()
unsafe fn g4x_reset_complete(pdev: *mut c_void) -> bool {
    let mut status = 0;
    unsafe { pci_read_config_byte(pdev, I915_GDRST, &mut status) };
    status & GRDOM_RESET_ENABLE == 0
}

// upstream: intel_reset.c g33_do_reset()
unsafe fn g33_do_reset(gt: *mut IntelGt, _engine_mask: IntelEngineMask, _retry: u32) -> c_int {
    let pdev = unsafe { to_pci_dev((*(*gt).i915).drm.dev) };
    unsafe { pci_write_config_byte(pdev, I915_GDRST, GRDOM_RESET_ENABLE) };
    wait_for_atomic(|| unsafe { g4x_reset_complete(pdev) }, 50_000)
}

// upstream: intel_reset.c g4x_do_reset()
unsafe fn g4x_do_reset(gt: *mut IntelGt, _engine_mask: IntelEngineMask, _retry: u32) -> c_int {
    let pdev = unsafe { to_pci_dev((*(*gt).i915).drm.dev) };
    let uncore = unsafe { (*gt).uncore };
    let vdec = reg(0x12400);
    unsafe { intel_uncore_rmw_fw(uncore, vdec, 0, 1 << 4) };
    unsafe { intel_uncore_posting_read_fw(uncore, vdec) };
    unsafe { pci_write_config_byte(pdev, I915_GDRST, GRDOM_MEDIA | GRDOM_RESET_ENABLE) };
    let mut ret = wait_for_atomic(|| unsafe { g4x_reset_complete(pdev) }, 50_000);
    if ret == 0 {
        unsafe { pci_write_config_byte(pdev, I915_GDRST, GRDOM_RENDER | GRDOM_RESET_ENABLE) };
        ret = wait_for_atomic(|| unsafe { g4x_reset_complete(pdev) }, 50_000);
    }
    unsafe { pci_write_config_byte(pdev, I915_GDRST, 0) };
    unsafe { intel_uncore_rmw_fw(uncore, vdec, 1 << 4, 0) };
    unsafe { intel_uncore_posting_read_fw(uncore, vdec) };
    ret
}

// upstream: intel_reset.c ilk_do_reset()
unsafe fn ilk_do_reset(gt: *mut IntelGt, _engine_mask: IntelEngineMask, _retry: u32) -> c_int {
    let uncore = unsafe { (*gt).uncore };
    unsafe {
        intel_uncore_write_fw(
            uncore,
            ILK_GDSR,
            ILK_GRDOM_RENDER | GRDOM_RESET_ENABLE as u32,
        )
    };
    let mut ret = unsafe {
        __intel_wait_for_register_fw(
            uncore,
            ILK_GDSR,
            GRDOM_RESET_ENABLE as u32,
            0,
            5000,
            0,
            core::ptr::null_mut(),
        )
    };
    if ret == 0 {
        unsafe {
            intel_uncore_write_fw(
                uncore,
                ILK_GDSR,
                ILK_GRDOM_MEDIA | GRDOM_RESET_ENABLE as u32,
            )
        };
        ret = unsafe {
            __intel_wait_for_register_fw(
                uncore,
                ILK_GDSR,
                GRDOM_RESET_ENABLE as u32,
                0,
                5000,
                0,
                core::ptr::null_mut(),
            )
        };
    }
    unsafe { intel_uncore_write_fw(uncore, ILK_GDSR, 0) };
    unsafe { intel_uncore_posting_read_fw(uncore, ILK_GDSR) };
    ret
}

// upstream: intel_reset.c gen6_hw_domain_reset()
unsafe fn gen6_hw_domain_reset(gt: *mut IntelGt, mask: u32) -> c_int {
    let uncore = unsafe { (*gt).uncore };
    let loops = if GRAPHICS_VER_FULL(unsafe { (*gt).i915 }) < IP_VER(12, 70) {
        2
    } else {
        1
    };
    let mut ret = 0;
    for _ in 0..loops {
        unsafe { intel_uncore_write_fw(uncore, GDRST, mask) };
        ret = unsafe {
            __intel_wait_for_register_fw(uncore, GDRST, mask, 0, 2000, 0, core::ptr::null_mut())
        };
        if ret != 0 {
            break;
        }
    }
    udelay(50);
    ret
}

// upstream: intel_reset.c __gen6_reset_engines()
unsafe fn __gen6_reset_engines(
    gt: *mut IntelGt,
    engine_mask: IntelEngineMask,
    _retry: u32,
) -> c_int {
    let hw_mask = if engine_mask == ALL_ENGINES {
        GEN6_GRDOM_FULL
    } else {
        engines(gt, engine_mask).fold(0, |mask, engine| mask | unsafe { (*engine).reset_domain })
    };
    unsafe { gen6_hw_domain_reset(gt, hw_mask) }
}

// upstream: intel_reset.c gen6_reset_engines()
unsafe fn gen6_reset_engines(gt: *mut IntelGt, engine_mask: IntelEngineMask, retry: u32) -> c_int {
    let mut flags = 0;
    unsafe { spin_lock_irqsave_raw(core::ptr::addr_of_mut!((*(*gt).uncore).lock), &mut flags) };
    let ret = unsafe { __gen6_reset_engines(gt, engine_mask, retry) };
    unsafe { spin_unlock_irqrestore_raw(core::ptr::addr_of_mut!((*(*gt).uncore).lock), flags) };
    ret
}

// upstream: intel_reset.c find_sfc_paired_vecs_engine()
unsafe fn find_sfc_paired_vecs_engine(engine: *mut IntelEngineCs) -> *mut IntelEngineCs {
    let vecs_id =
        crate::intel_engine_types_upstream::_VECS((unsafe { (*engine).instance } as i32) / 2)
            as usize;
    unsafe { (*(*engine).gt).engine[vecs_id] }
}

// upstream: intel_reset.c get_sfc_forced_lock_data()
unsafe fn get_sfc_forced_lock_data(engine: *mut IntelEngineCs) -> IntelSfcLockData {
    let base = unsafe { (*engine).mmio_base };
    if unsafe { (*engine).class } == VIDEO_ENHANCEMENT_CLASS_U8 {
        IntelSfcLockData {
            lock_reg: GEN11_VECS_SFC_FORCED_LOCK(base),
            ack_reg: GEN11_VECS_SFC_LOCK_ACK(base),
            usage_reg: GEN11_VECS_SFC_USAGE(base),
            lock_bit: GEN11_VECS_SFC_FORCED_LOCK_BIT,
            ack_bit: GEN11_VECS_SFC_LOCK_ACK_BIT,
            usage_bit: GEN11_VECS_SFC_USAGE_BIT,
            reset_bit: (1 << 17) << unsafe { (*engine).instance },
        }
    } else {
        IntelSfcLockData {
            lock_reg: GEN11_VCS_SFC_FORCED_LOCK(base),
            ack_reg: GEN11_VCS_SFC_LOCK_STATUS(base),
            usage_reg: GEN11_VCS_SFC_LOCK_STATUS(base),
            lock_bit: GEN11_VCS_SFC_FORCED_LOCK_BIT,
            ack_bit: GEN11_VCS_SFC_LOCK_ACK_BIT,
            usage_bit: GEN11_VCS_SFC_USAGE_BIT,
            reset_bit: (1 << 17) << (unsafe { (*engine).instance } >> 1),
        }
    }
}

// upstream: intel_reset.c gen11_lock_sfc()
unsafe fn gen11_lock_sfc(
    engine: *mut IntelEngineCs,
    reset_mask: &mut u32,
    unlock_mask: &mut u32,
) -> c_int {
    let class = unsafe { (*engine).class };
    let access = unsafe { (*(*engine).gt).info.vdbox_sfc_access };
    if class == VIDEO_DECODE_CLASS_U8 && access & (1 << unsafe { (*engine).instance }) == 0 {
        return 0;
    }
    if class != VIDEO_DECODE_CLASS_U8 && class != VIDEO_ENHANCEMENT_CLASS_U8 {
        return 0;
    }
    let mut data = unsafe { get_sfc_forced_lock_data(engine) };
    let mut lock_to_other = false;
    if unsafe { intel_uncore_read_fw((*engine).uncore, data.usage_reg) } & data.usage_bit == 0 {
        if class != VIDEO_DECODE_CLASS_U8 || GRAPHICS_VER(unsafe { (*engine).i915 }) != 12 {
            return 0;
        }
        if unsafe {
            intel_uncore_read_fw(
                (*engine).uncore,
                GEN12_HCP_SFC_LOCK_STATUS((*engine).mmio_base),
            )
        } & GEN12_HCP_SFC_USAGE_BIT
            == 0
        {
            return 0;
        }
        let paired = unsafe { find_sfc_paired_vecs_engine(engine) };
        data = unsafe { get_sfc_forced_lock_data(paired) };
        lock_to_other = true;
        *unlock_mask |= unsafe { (*paired).mask };
    } else {
        *unlock_mask |= unsafe { (*engine).mask };
    }
    unsafe { intel_uncore_rmw_fw((*engine).uncore, data.lock_reg, 0, data.lock_bit) };
    let ret = unsafe {
        __intel_wait_for_register_fw(
            (*engine).uncore,
            data.ack_reg,
            data.ack_bit,
            data.ack_bit,
            1000,
            0,
            core::ptr::null_mut(),
        )
    };
    let obtained =
        unsafe { intel_uncore_read_fw((*engine).uncore, data.usage_reg) } & data.usage_bit != 0;
    if obtained == lock_to_other {
        return 0;
    }
    if ret != 0 {
        return ret;
    }
    *reset_mask |= data.reset_bit;
    0
}

// upstream: intel_reset.c gen11_unlock_sfc()
unsafe fn gen11_unlock_sfc(engine: *mut IntelEngineCs) {
    let class = unsafe { (*engine).class };
    let access = unsafe { (*(*engine).gt).info.vdbox_sfc_access };
    if class != VIDEO_DECODE_CLASS_U8 && class != VIDEO_ENHANCEMENT_CLASS_U8 {
        return;
    }
    if class == VIDEO_DECODE_CLASS_U8 && access & (1 << unsafe { (*engine).instance }) == 0 {
        return;
    }
    let data = unsafe { get_sfc_forced_lock_data(engine) };
    unsafe { intel_uncore_rmw_fw((*engine).uncore, data.lock_reg, data.lock_bit, 0) };
}

// upstream: intel_reset.c __gen11_reset_engines()
unsafe fn __gen11_reset_engines(
    gt: *mut IntelGt,
    engine_mask: IntelEngineMask,
    retry: u32,
) -> c_int {
    let mut reset_mask = if engine_mask == ALL_ENGINES {
        GEN11_GRDOM_FULL
    } else {
        0
    };
    let mut unlock_mask = 0;
    let mut ret = 0;
    if engine_mask != ALL_ENGINES {
        for engine in engines(gt, engine_mask) {
            reset_mask |= unsafe { (*engine).reset_domain };
            ret = unsafe { gen11_lock_sfc(engine, &mut reset_mask, &mut unlock_mask) };
            if ret != 0 {
                break;
            }
        }
    }
    if ret == 0 {
        ret = unsafe { gen6_hw_domain_reset(gt, reset_mask) };
    }
    for engine in engines(gt, unlock_mask) {
        unsafe { gen11_unlock_sfc(engine) };
    }
    let _ = retry;
    ret
}

// upstream: intel_reset.c gen8_engine_reset_prepare()
unsafe fn gen8_engine_reset_prepare(engine: *mut IntelEngineCs) -> c_int {
    let reg = RING_RESET_CTL(unsafe { (*engine).mmio_base });
    let old = unsafe { intel_uncore_read_fw((*engine).uncore, reg) };
    let (request, mask, ack) = if old & RESET_CTL_CAT_ERROR != 0 {
        (RESET_CTL_CAT_ERROR, RESET_CTL_CAT_ERROR, 0)
    } else if old & RESET_CTL_READY_TO_RESET == 0 {
        (
            RESET_CTL_REQUEST_RESET,
            RESET_CTL_READY_TO_RESET,
            RESET_CTL_READY_TO_RESET,
        )
    } else {
        return 0;
    };
    unsafe { intel_uncore_write_fw((*engine).uncore, reg, (request << 16) | request) };
    unsafe {
        __intel_wait_for_register_fw(
            (*engine).uncore,
            reg,
            mask,
            ack,
            700,
            0,
            core::ptr::null_mut(),
        )
    }
}

// upstream: intel_reset.c gen8_engine_reset_cancel()
unsafe fn gen8_engine_reset_cancel(engine: *mut IntelEngineCs) {
    unsafe {
        intel_uncore_write_fw(
            (*engine).uncore,
            RING_RESET_CTL((*engine).mmio_base),
            RESET_CTL_REQUEST_RESET << 16,
        )
    };
}

// upstream: intel_reset.c gen8_reset_engines()
unsafe fn gen8_reset_engines(gt: *mut IntelGt, engine_mask: IntelEngineMask, retry: u32) -> c_int {
    let mut flags = 0;
    unsafe { spin_lock_irqsave_raw(core::ptr::addr_of_mut!((*(*gt).uncore).lock), &mut flags) };
    let mut ret = 0;
    let reset_non_ready = retry >= 1;
    for engine in engines(gt, engine_mask) {
        ret = unsafe { gen8_engine_reset_prepare(engine) };
        if ret != 0 && !reset_non_ready {
            break;
        }
    }
    if ret == 0 || reset_non_ready {
        if IS_DG2(unsafe { (*gt).i915 }) && engine_mask == ALL_ENGINES {
            let _ = unsafe { __gen11_reset_engines(gt, (*gt).info.engine_mask, 0) };
        }
        ret = if GRAPHICS_VER(unsafe { (*gt).i915 }) >= 11 {
            unsafe { __gen11_reset_engines(gt, engine_mask, retry) }
        } else {
            unsafe { __gen6_reset_engines(gt, engine_mask, retry) }
        };
    }
    for engine in engines(gt, engine_mask) {
        unsafe { gen8_engine_reset_cancel(engine) };
    }
    unsafe { spin_unlock_irqrestore_raw(core::ptr::addr_of_mut!((*(*gt).uncore).lock), flags) };
    ret
}

// upstream: intel_reset.c mock_reset()
unsafe fn mock_reset(_gt: *mut IntelGt, _mask: IntelEngineMask, _retry: u32) -> c_int {
    0
}

type ResetFunc = unsafe fn(*mut IntelGt, IntelEngineMask, u32) -> c_int;

// upstream: intel_reset.c intel_get_gpu_reset()
unsafe fn intel_get_gpu_reset(gt: *const IntelGt) -> Option<ResetFunc> {
    if unsafe { is_mock_gt(gt) } {
        Some(mock_reset)
    } else if GRAPHICS_VER(unsafe { (*gt).i915 }) >= 8 {
        Some(gen8_reset_engines)
    } else if GRAPHICS_VER(unsafe { (*gt).i915 }) >= 6 {
        Some(gen6_reset_engines)
    } else if GRAPHICS_VER(unsafe { (*gt).i915 }) >= 5 {
        Some(ilk_do_reset)
    } else if unsafe { crate::linux::i915::IS_G4X((*gt).i915) } {
        Some(g4x_do_reset)
    } else if unsafe {
        crate::linux::i915::IS_PLATFORM((*gt).i915, 9)
            || crate::linux::i915::IS_PLATFORM((*gt).i915, 10)
    } {
        Some(g33_do_reset)
    } else if GRAPHICS_VER(unsafe { (*gt).i915 }) >= 3 {
        Some(i915_do_reset)
    } else {
        None
    }
}

// upstream: intel_reset.c __reset_guc()
unsafe fn __reset_guc(gt: *mut IntelGt) -> c_int {
    unsafe {
        gen6_hw_domain_reset(
            gt,
            if GRAPHICS_VER((*gt).i915) >= 11 {
                GEN11_GRDOM_GUC
            } else {
                GEN9_GRDOM_GUC
            },
        )
    }
}

// upstream: intel_reset.c needs_wa_14015076503()
unsafe fn needs_wa_14015076503(gt: *mut IntelGt, engine_mask: IntelEngineMask) -> bool {
    if MEDIA_VER_FULL(unsafe { (*gt).i915 }) != IP_VER(13, 0) || !HAS_ENGINE(gt, GSC0 as u32) {
        return false;
    }
    if engine_mask & (1 << GSC0) == 0 {
        return false;
    }
    unsafe { intel_uncore_read((*gt).uncore, reg(0x116c40)) & (1 << 9) != 0 }
}

// upstream: intel_reset.c wa_14015076503_start()
unsafe fn wa_14015076503_start(
    gt: *mut IntelGt,
    mut engine_mask: IntelEngineMask,
    first: bool,
) -> IntelEngineMask {
    if !unsafe { needs_wa_14015076503(gt, engine_mask) } {
        return engine_mask;
    }
    if engine_mask == ALL_ENGINES
        && first
        && unsafe { crate::intel_engine_cs_upstream::intel_engine_is_idle((*gt).engine[GSC0]) }
    {
        let _ = unsafe { __reset_guc(gt) };
        engine_mask = unsafe { (*gt).info.engine_mask & !(1 << GSC0) };
    } else {
        unsafe { intel_uncore_rmw((*gt).uncore, reg(0x117c4c), 0, 1) };
        unsafe { intel_uncore_rmw((*gt).uncore, reg(0x117c04), 1 << 4, 1 << 2) };
        msleep(200);
    }
    engine_mask
}

// upstream: intel_reset.c wa_14015076503_end()
unsafe fn wa_14015076503_end(gt: *mut IntelGt, engine_mask: IntelEngineMask) {
    if unsafe { needs_wa_14015076503(gt, engine_mask) } {
        unsafe { intel_uncore_rmw((*gt).uncore, reg(0x117c4c), 1, 0) };
    }
}

// upstream: intel_reset.c __intel_gt_reset()
unsafe fn __intel_gt_reset(gt: *mut IntelGt, engine_mask: IntelEngineMask) -> c_int {
    let retries = if engine_mask == ALL_ENGINES {
        RESET_MAX_RETRIES
    } else {
        1
    };
    let Some(reset) = (unsafe { intel_get_gpu_reset(gt) }) else {
        return -ENODEV;
    };
    unsafe { intel_uncore_forcewake_get((*gt).uncore, FORCEWAKE_ALL) };
    let mut ret = -ETIMEDOUT;
    for retry in 0..retries {
        if ret != -ETIMEDOUT {
            break;
        }
        let reset_mask = unsafe { wa_14015076503_start(gt, engine_mask, retry == 0) };
        ret = unsafe { reset(gt, reset_mask, retry) };
        unsafe { wa_14015076503_end(gt, reset_mask) };
    }
    unsafe { intel_uncore_forcewake_put((*gt).uncore, FORCEWAKE_ALL) };
    ret
}

// upstream: intel_reset.c intel_has_gpu_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_has_gpu_reset(gt: *const IntelGt) -> bool {
    if unsafe { (*(*gt).i915).params.reset } == 0 {
        return false;
    }
    unsafe { intel_get_gpu_reset(gt).is_some() }
}

// upstream: intel_reset.c intel_has_reset_engine()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_has_reset_engine(gt: *const IntelGt) -> bool {
    if unsafe { (*(*gt).i915).params.reset } < 2 {
        return false;
    }
    unsafe { (*INTEL_INFO((*gt).i915)).flags[0] & (1 << 6) != 0 }
}

// upstream: intel_reset.c intel_reset_guc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_reset_guc(gt: *mut IntelGt) -> c_int {
    assert!(unsafe { crate::linux::i915::HAS_GT_UC((*gt).i915) });
    unsafe { intel_uncore_forcewake_get((*gt).uncore, FORCEWAKE_ALL) };
    let ret = unsafe { __reset_guc(gt) };
    unsafe { intel_uncore_forcewake_put((*gt).uncore, FORCEWAKE_ALL) };
    ret
}

// upstream: intel_reset.c reset_prepare_engine()
unsafe fn reset_prepare_engine(engine: *mut IntelEngineCs) {
    unsafe { intel_uncore_forcewake_get((*engine).uncore, FORCEWAKE_ALL) };
    if let Some(prepare) = unsafe { (*engine).reset.prepare } {
        unsafe { prepare(engine) };
    }
}

// upstream: intel_reset.c revoke_mmaps()
unsafe fn revoke_mmaps(gt: *mut IntelGt) {
    let ggtt = unsafe { (*gt).ggtt };
    if ggtt.is_null() {
        return;
    }
    let anon = unsafe { (*(*(*gt).i915).drm.anon_inode).i_mapping };
    for index in 0..unsafe { (*ggtt).num_fences as usize } {
        let fence = unsafe { (*ggtt).fence_regs.add(index) };
        let vma = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*fence).vma)) };
        if vma.is_null()
            || !crate::i915_vma_api_upstream::i915_vma_has_userfault(vma)
            || unsafe { (*vma).mmo.is_null() }
        {
            continue;
        }
        assert!(unsafe { (*vma).fence == fence });
        let partial_offset = if unsafe { (*vma).gtt_view.r#type } == I915_GTT_VIEW_PARTIAL {
            unsafe { (*vma).gtt_view.info.partial.offset }
        } else {
            0
        };
        let node = unsafe { core::ptr::addr_of!((*(*vma).mmo).vma_node) };
        let start = crate::linux::gem::drm_vma_node_offset_addr(node)
            .wrapping_add(partial_offset << crate::linux_config::PAGE_SHIFT);
        unsafe { unmap_mapping_range(anon, start as c_long, (*vma).size as c_long, 1) };
    }
}

// upstream: intel_reset.c reset_prepare()
unsafe fn reset_prepare(gt: *mut IntelGt) -> IntelEngineMask {
    if unsafe { intel_uc_uses_guc_submission(core::ptr::addr_of_mut!((*gt).uc)) } {
        unsafe {
            crate::intel_uc_upstream::intel_uc_reset_prepare(core::ptr::addr_of_mut!((*gt).uc))
        };
    }
    let mut awake = 0;
    for engine in engines(gt, ALL_ENGINES) {
        if crate::linux::pm::intel_engine_pm_get_if_awake(engine) {
            awake |= unsafe { (*engine).mask };
        }
        unsafe { reset_prepare_engine(engine) };
    }
    awake
}

// upstream: intel_reset.c gt_revoke()
unsafe fn gt_revoke(gt: *mut IntelGt) {
    unsafe { revoke_mmaps(gt) };
}

// upstream: intel_reset.c gt_reset()
unsafe fn gt_reset(gt: *mut IntelGt, stalled_mask: IntelEngineMask) -> c_int {
    let err = unsafe { i915_ggtt_enable_hw((*gt).i915) };
    if err != 0 {
        return err;
    }
    crate::linux::irq::local_bh_disable();
    for engine in engines(gt, ALL_ENGINES) {
        unsafe {
            crate::intel_engine_api_upstream::__intel_engine_reset(
                engine,
                (*engine).mask & stalled_mask != 0,
            )
        };
    }
    crate::linux::irq::local_bh_enable();
    unsafe {
        crate::intel_uc_upstream::intel_uc_reset(core::ptr::addr_of_mut!((*gt).uc), ALL_ENGINES)
    };
    unsafe { crate::intel_ggtt_fencing_upstream::intel_ggtt_restore_fences((*gt).ggtt) };
    err
}

// upstream: intel_reset.c reset_finish_engine()
unsafe fn reset_finish_engine(engine: *mut IntelEngineCs) {
    if let Some(finish) = unsafe { (*engine).reset.finish } {
        unsafe { finish(engine) };
    }
    unsafe { intel_uncore_forcewake_put((*engine).uncore, FORCEWAKE_ALL) };
    crate::linux::irq::intel_engine_signal_breadcrumbs(engine);
}

// upstream: intel_reset.c reset_finish()
unsafe fn reset_finish(gt: *mut IntelGt, awake: IntelEngineMask) {
    for engine in engines(gt, ALL_ENGINES) {
        unsafe { reset_finish_engine(engine) };
        if awake & unsafe { (*engine).mask } != 0 {
            crate::linux::pm::intel_engine_pm_put(engine);
        }
    }
    unsafe { crate::intel_uc_upstream::intel_uc_reset_finish(core::ptr::addr_of_mut!((*gt).uc)) };
}

// upstream: intel_reset.c nop_submit_request()
unsafe extern "C" fn nop_submit_request(request: *mut I915Request) {
    let request = unsafe { crate::i915_request_upstream::i915_request_mark_eio(request) };
    if !request.is_null() {
        unsafe { crate::i915_request_upstream::i915_request_submit(request) };
        crate::linux::irq::intel_engine_signal_breadcrumbs(unsafe { (*request).engine });
        crate::linux::requests::i915_request_put(request);
    }
}

// upstream: intel_reset.c __intel_gt_set_wedged()
unsafe fn __intel_gt_set_wedged(gt: *mut IntelGt) {
    if test_bit(I915_WEDGED, unsafe { &(*gt).reset.flags }) {
        return;
    }
    let awake = unsafe { reset_prepare(gt) };
    if !unsafe { intel_gt_gpu_reset_clobbers_display(gt) }
        && !unsafe { intel_display_reset_test((*(*gt).i915).display) }
    {
        let _ = unsafe { intel_gt_reset_all_engines(gt) };
    }
    for engine in engines(gt, ALL_ENGINES) {
        unsafe { (*engine).submit_request = Some(nop_submit_request) };
    }
    let state = crate::linux::rcu::get_state_synchronize_rcu();
    cond_synchronize_rcu(state);
    set_bit(I915_WEDGED, unsafe { &mut (*gt).reset.flags });
    crate::linux::irq::local_bh_disable();
    for engine in engines(gt, ALL_ENGINES) {
        if let Some(cancel) = unsafe { (*engine).reset.cancel } {
            unsafe { cancel(engine) };
        }
    }
    unsafe {
        crate::intel_uc_upstream::intel_uc_cancel_requests(core::ptr::addr_of_mut!((*gt).uc))
    };
    crate::linux::irq::local_bh_enable();
    unsafe { reset_finish(gt, awake) };
}

// upstream: intel_reset.c set_wedged_work()
unsafe extern "C" fn set_wedged_work(work: *mut WorkStruct) {
    let gt = unsafe {
        work.cast::<u8>()
            .sub(offset_of!(IntelGt, wedge))
            .cast::<IntelGt>()
    };
    let rpm = unsafe { (*(*gt).uncore).rpm };
    let wakeref = intel_runtime_pm_get(rpm);
    unsafe { __intel_gt_set_wedged(gt) };
    intel_runtime_pm_put(rpm, wakeref);
}

// upstream: intel_reset.c intel_gt_set_wedged()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_set_wedged(gt: *mut IntelGt) {
    if test_bit(I915_WEDGED, unsafe { &(*gt).reset.flags }) {
        return;
    }
    let rpm = unsafe { (*(*gt).uncore).rpm };
    let wakeref = intel_runtime_pm_get(rpm);
    unsafe { crate::linux::mutex::mutex_lock(&mut (*gt).reset.mutex) };
    unsafe { __intel_gt_set_wedged(gt) };
    unsafe { crate::linux::mutex::mutex_unlock(&mut (*gt).reset.mutex) };
    intel_runtime_pm_put(rpm, wakeref);
}

// upstream: intel_reset.c __intel_gt_unset_wedged()
unsafe fn __intel_gt_unset_wedged(gt: *mut IntelGt) -> bool {
    if !test_bit(I915_WEDGED, unsafe { &(*gt).reset.flags }) {
        return true;
    }
    if unsafe { intel_gt_has_unrecoverable_error(gt) } {
        return false;
    }
    let timelines = core::ptr::addr_of_mut!((*gt).timelines);
    crate::linux::locks::spin_lock(unsafe { &mut (*timelines).lock });
    let head = core::ptr::addr_of_mut!((*timelines).active_list);
    let mut link = unsafe { (*head).next };
    while link != head {
        let next = unsafe { (*link).next };
        let timeline = unsafe {
            link.cast::<u8>()
                .sub(offset_of!(
                    crate::intel_timeline_types_upstream::IntelTimeline,
                    link
                ))
                .cast::<crate::intel_timeline_types_upstream::IntelTimeline>()
        };
        let fence = unsafe {
            crate::i915_active_upstream::i915_active_fence_get(core::ptr::addr_of_mut!(
                (*timeline).last_request
            ))
        };
        if !fence.is_null() {
            crate::linux::locks::spin_unlock(unsafe { &mut (*timelines).lock });
            unsafe { dma_fence_default_wait(fence, false, MAX_SCHEDULE_TIMEOUT as c_long) };
            crate::linux::requests::dma_fence_put(fence);
            crate::linux::locks::spin_lock(unsafe { &mut (*timelines).lock });
            link = unsafe { (*head).next };
        } else {
            link = next;
        }
    }
    crate::linux::locks::spin_unlock(unsafe { &mut (*timelines).lock });
    let mut ok = !unsafe { crate::linux::i915::HAS_EXECLISTS((*gt).i915) };
    if !unsafe { intel_gt_gpu_reset_clobbers_display(gt) } {
        ok = unsafe { intel_gt_reset_all_engines(gt) } == 0;
    }
    if !ok {
        unsafe { add_taint_for_CI((*gt).i915, TAINT_WARN) };
        return false;
    }
    unsafe { crate::intel_engine_cs_upstream::intel_engines_reset_default_submission(gt) };
    crate::linux::primitives::mb();
    clear_bit(I915_WEDGED, unsafe { &mut (*gt).reset.flags });
    true
}

// upstream: intel_reset.c intel_gt_unset_wedged()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_unset_wedged(gt: *mut IntelGt) -> bool {
    unsafe { crate::linux::mutex::mutex_lock(&mut (*gt).reset.mutex) };
    let result = unsafe { __intel_gt_unset_wedged(gt) };
    unsafe { crate::linux::mutex::mutex_unlock(&mut (*gt).reset.mutex) };
    result
}

// upstream: intel_reset.c do_reset()
unsafe fn do_reset(gt: *mut IntelGt, stalled_mask: IntelEngineMask) -> c_int {
    let mut err = unsafe { intel_gt_reset_all_engines(gt) };
    for retry in 0..RESET_MAX_RETRIES {
        if err == 0 {
            break;
        }
        msleep(10 * (retry + 1));
        err = unsafe { intel_gt_reset_all_engines(gt) };
    }
    if err != 0 {
        return err;
    }
    unsafe { gt_reset(gt, stalled_mask) }
}

// upstream: intel_reset.c resume()
unsafe fn resume(gt: *mut IntelGt) -> c_int {
    for engine in engines(gt, ALL_ENGINES) {
        let ret = unsafe { crate::intel_engine_cs_upstream::intel_engine_resume(engine) };
        if ret != 0 {
            return ret;
        }
    }
    0
}

// upstream: intel_reset.c intel_gt_gpu_reset_clobbers_display()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_gpu_reset_clobbers_display(gt: *mut IntelGt) -> bool {
    let info = unsafe { crate::linux::i915::INTEL_INFO((*gt).i915) };
    unsafe { (*info).flags[0] & (1 << 5) != 0 }
}

// upstream: intel_reset.c intel_gt_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_reset(
    gt: *mut IntelGt,
    stalled_mask: IntelEngineMask,
    reason: *const c_char,
) {
    assert!(test_bit(I915_RESET_BACKOFF, unsafe { &(*gt).reset.flags }));
    unsafe { gt_revoke(gt) };
    unsafe { crate::linux::mutex::mutex_lock(&mut (*gt).reset.mutex) };
    if !unsafe { __intel_gt_unset_wedged(gt) } {
        unsafe { crate::linux::mutex::mutex_unlock(&mut (*gt).reset.mutex) };
        return;
    }
    if !reason.is_null() {
        axlog::info!("GPU reset requested: {}", unsafe { log_c_string(reason) });
    }
    let i915 = unsafe { (*gt).i915 };
    unsafe { atomic_inc(&mut (*i915).gpu_error.reset_count) };
    let awake = unsafe { reset_prepare(gt) };
    if !unsafe { intel_has_gpu_reset(gt) } {
        unsafe { __intel_gt_set_wedged(gt) };
        unsafe { reset_finish(gt, awake) };
        unsafe { crate::linux::mutex::mutex_unlock(&mut (*gt).reset.mutex) };
        return;
    }
    let clobbers_display = unsafe { intel_gt_gpu_reset_clobbers_display(gt) };
    if clobbers_display {
        unsafe { intel_irq_suspend(i915) };
    }
    if unsafe { do_reset(gt, stalled_mask) } != 0 {
        unsafe { add_taint_for_CI(i915, TAINT_WARN) };
        unsafe { __intel_gt_set_wedged(gt) };
        unsafe { reset_finish(gt, awake) };
        unsafe { crate::linux::mutex::mutex_unlock(&mut (*gt).reset.mutex) };
        return;
    }
    if clobbers_display {
        unsafe { intel_irq_resume(i915) };
    }
    unsafe { intel_overlay_reset((*i915).display) };
    if !unsafe { intel_uc_uses_guc_submission(core::ptr::addr_of_mut!((*gt).uc)) } {
        unsafe {
            crate::intel_uc_upstream::intel_uc_reset_prepare(core::ptr::addr_of_mut!((*gt).uc))
        };
    }
    let ret = unsafe { intel_gt_init_hw(gt) };
    let ret = if ret != 0 { ret } else { resume(gt) };
    if ret != 0 {
        unsafe { add_taint_for_CI(i915, TAINT_WARN) };
        unsafe { __intel_gt_set_wedged(gt) };
    }
    unsafe { reset_finish(gt, awake) };
    unsafe { crate::linux::mutex::mutex_unlock(&mut (*gt).reset.mutex) };
}

// upstream: intel_reset.c intel_gt_reset_all_engines()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_reset_all_engines(gt: *mut IntelGt) -> c_int {
    unsafe { __intel_gt_reset(gt, ALL_ENGINES) }
}

// upstream: intel_reset.c intel_gt_reset_engine()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_reset_engine(engine: *mut IntelEngineCs) -> c_int {
    unsafe { __intel_gt_reset((*engine).gt, (*engine).mask) }
}

// upstream: intel_reset.c __intel_engine_reset_bh()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __intel_engine_reset_bh(
    engine: *mut IntelEngineCs,
    msg: *const c_char,
) -> c_int {
    let gt = unsafe { (*engine).gt };
    let reset_bit = I915_RESET_ENGINE + unsafe { (*engine).id as u32 };
    assert!(test_bit(reset_bit, unsafe { &(*gt).reset.flags }));
    if crate::linux::i915::intel_engine_uses_guc(engine) {
        return -ENODEV;
    }
    if !crate::linux::pm::intel_engine_pm_get_if_awake(engine) {
        return 0;
    }
    unsafe { reset_prepare_engine(engine) };
    if !msg.is_null() {
        axlog::info!("engine reset: {}", unsafe { log_c_string(msg) });
    }
    crate::linux::i915_private::i915_increase_reset_engine_count(
        unsafe { &mut (*(*engine).i915).gpu_error },
        engine,
    );
    let mut ret = unsafe { intel_gt_reset_engine(engine) };
    if ret == 0 {
        unsafe { crate::intel_engine_api_upstream::__intel_engine_reset(engine, true) };
        ret = unsafe { crate::intel_engine_cs_upstream::intel_engine_resume(engine) };
    }
    unsafe { crate::intel_engine_cs_upstream::intel_engine_cancel_stop_cs(engine) };
    unsafe { reset_finish_engine(engine) };
    crate::linux::pm::intel_engine_pm_put_async(engine);
    ret
}

// upstream: intel_reset.c intel_engine_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_engine_reset(
    engine: *mut IntelEngineCs,
    msg: *const c_char,
) -> c_int {
    crate::linux::irq::local_bh_disable();
    let ret = unsafe { __intel_engine_reset_bh(engine, msg) };
    crate::linux::irq::local_bh_enable();
    ret
}

// upstream: intel_reset.c intel_gt_reset_global()
unsafe fn intel_gt_reset_global(
    gt: *mut IntelGt,
    engine_mask: IntelEngineMask,
    reason: *const c_char,
) {
    let mut wedge = core::mem::MaybeUninit::<IntelWedgeMe>::zeroed();
    let wedge = wedge.as_mut_ptr();
    unsafe {
        __intel_init_wedge(
            wedge,
            gt,
            60 * HZ as c_long,
            c"intel_gt_reset_global".as_ptr(),
        )
    };
    let i915 = unsafe { (*gt).i915 };
    let kobj = unsafe { drm_primary_kobj(i915) };
    assert!(
        !kobj.is_null(),
        "reset uevent requires the DRM primary kobject"
    );
    let error_event = [c"I915_ERROR=1".as_ptr(), core::ptr::null()];
    let reset_event = [c"I915_RESET=1".as_ptr(), core::ptr::null()];
    let reset_done_event = [c"I915_ERROR=0".as_ptr(), core::ptr::null()];
    unsafe { kobject_uevent_env(kobj, KOBJ_CHANGE, error_event.as_ptr()) };
    axlog::debug!("resetting chip, engines={engine_mask:#x}");
    unsafe { kobject_uevent_env(kobj, KOBJ_CHANGE, reset_event.as_ptr()) };
    let display = unsafe { (*i915).display };
    let need_display_reset = unsafe { intel_display_reset_supported(display) }
        && unsafe { intel_gt_gpu_reset_clobbers_display(gt) }
        && unsafe { intel_has_gpu_reset(gt) };
    let reset_display = unsafe { intel_display_reset_test(display) } || need_display_reset;
    if reset_display {
        unsafe { intel_display_reset_prepare(display) };
    }
    unsafe { intel_gt_reset(gt, engine_mask, reason) };
    if reset_display {
        unsafe { intel_display_reset_finish(display, !need_display_reset) };
    }
    if !test_bit(I915_WEDGED, unsafe { &(*gt).reset.flags }) {
        unsafe { kobject_uevent_env(kobj, KOBJ_CHANGE, reset_done_event.as_ptr()) };
    } else {
        unsafe {
            drm_dev_wedged_event(
                core::ptr::addr_of_mut!((*i915).drm).cast(),
                DRM_WEDGE_RECOVERY_REBIND | DRM_WEDGE_RECOVERY_BUS_RESET,
                core::ptr::null_mut(),
            )
        };
    }
    unsafe { __intel_fini_wedge(wedge) };
}

// upstream: intel_reset.c intel_gt_handle_error()
// Rust cannot define a stable C-variadic body. Owning Rust callers format the
// message through `intel_gt_handle_error_format()` and pass this fixed-prefix
// C ABI entry point, preserving the source's vscnprintf buffer limit.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_handle_error(
    gt: *mut IntelGt,
    mut engine_mask: IntelEngineMask,
    flags: c_ulong,
    msg: *const c_char,
) {
    let i915 = unsafe { (*gt).i915 };
    let rpm = unsafe { (*(*gt).uncore).rpm };
    let wakeref = intel_runtime_pm_get(rpm);
    engine_mask &= unsafe { (*gt).info.engine_mask };
    if flags & I915_ERROR_CAPTURE != 0 {
        unsafe {
            crate::i915_gpu_error_upstream::i915_capture_error_state(
                gt,
                engine_mask,
                CORE_DUMP_FLAG_NONE,
            )
        };
        unsafe { crate::intel_gt_upstream::intel_gt_clear_error_registers(gt, engine_mask) };
    }
    if !unsafe { intel_display_reset_test((*i915).display) }
        && !unsafe { intel_uc_uses_guc_submission(core::ptr::addr_of_mut!((*gt).uc)) }
        && unsafe { intel_has_reset_engine(gt) }
        && !unsafe { intel_gt_is_wedged(gt) }
    {
        crate::linux::irq::local_bh_disable();
        for engine in engines(gt, engine_mask) {
            let bit = I915_RESET_ENGINE + unsafe { (*engine).id as u32 };
            if bit >= I915_WEDGED_ON_INIT
                || test_and_set_bit(bit, unsafe { &mut (*gt).reset.flags })
            {
                continue;
            }
            if unsafe { __intel_engine_reset_bh(engine, msg) } == 0 {
                engine_mask &= !unsafe { (*engine).mask };
            }
            unsafe {
                clear_and_wake_up_bit(bit as c_int, core::ptr::addr_of_mut!((*gt).reset.flags))
            };
        }
        crate::linux::irq::local_bh_enable();
    }
    if engine_mask == 0 {
        intel_runtime_pm_put(rpm, wakeref);
        return;
    }
    if test_and_set_bit(I915_RESET_BACKOFF, unsafe { &mut (*gt).reset.flags }) {
        while test_bit(I915_RESET_BACKOFF, unsafe { &(*gt).reset.flags }) {
            axtask::yield_now();
        }
        intel_runtime_pm_put(rpm, wakeref);
        return;
    }
    let state = crate::linux::rcu::get_state_synchronize_rcu();
    cond_synchronize_rcu(state);
    if !unsafe { intel_uc_uses_guc_submission(core::ptr::addr_of_mut!((*gt).uc)) } {
        for engine in engines(gt, ALL_ENGINES) {
            let bit = I915_RESET_ENGINE + unsafe { (*engine).id as u32 };
            while test_and_set_bit(bit, unsafe { &mut (*gt).reset.flags }) {
                axtask::yield_now();
            }
        }
    }
    unsafe { synchronize_srcu_expedited(core::ptr::addr_of_mut!((*gt).reset.backoff_srcu)) };
    unsafe { intel_gt_reset_global(gt, engine_mask, msg) };
    if !unsafe { intel_uc_uses_guc_submission(core::ptr::addr_of_mut!((*gt).uc)) } {
        for engine in engines(gt, ALL_ENGINES) {
            clear_bit_unlock(I915_RESET_ENGINE + unsafe { (*engine).id as u32 }, unsafe {
                &mut (*gt).reset.flags
            });
        }
    }
    clear_bit_unlock(I915_RESET_BACKOFF, unsafe { &mut (*gt).reset.flags });
    crate::linux::primitives::mb();
    wake_up_all(unsafe { &mut (*gt).reset.queue });
    intel_runtime_pm_put(rpm, wakeref);
}

/// Adapter for source `intel_gt_handle_error(gt, mask, flags, fmt, ...)`.
/// The source formats into an 80-byte stack buffer using `vscnprintf`, so the
/// adapter uses the shared C formatter and copies no more than 79 data bytes.
pub unsafe fn intel_gt_handle_error_format(
    gt: *mut IntelGt,
    engine_mask: IntelEngineMask,
    flags: c_ulong,
    fmt: *const c_char,
    args: &[&dyn crate::linux::print::CFormatArg],
) {
    let format = if fmt.is_null() {
        alloc::string::String::new()
    } else {
        unsafe { CStr::from_ptr(fmt) }
            .to_string_lossy()
            .into_owned()
    };
    let formatted = crate::linux::print::format_message(&format, args);
    let mut message = [0 as c_char; 80];
    for (destination, source) in message.iter_mut().take(79).zip(formatted.as_bytes()) {
        *destination = *source as c_char;
    }
    unsafe { intel_gt_handle_error(gt, engine_mask, flags, message.as_ptr()) };
}

// upstream: intel_reset.c _intel_gt_reset_lock()
unsafe fn _intel_gt_reset_lock(gt: *mut IntelGt, srcu: *mut c_int, retry: bool) -> c_int {
    // `might_lock()` compiles away for this source target's CONFIG_LOCKDEP=n.
    if retry {
        crate::linux::wait::might_sleep();
    }
    rcu_read_lock();
    while test_bit(I915_RESET_BACKOFF, unsafe { &(*gt).reset.flags }) {
        rcu_read_unlock();
        if !retry {
            return -EBUSY;
        }
        while test_bit(I915_RESET_BACKOFF, unsafe { &(*gt).reset.flags }) {
            if unsafe { signal_pending_state(1, current_task_ptr()) } {
                return -EINTR;
            }
            axtask::yield_now();
        }
        rcu_read_lock();
    }
    unsafe { *srcu = srcu_read_lock(core::ptr::addr_of_mut!((*gt).reset.backoff_srcu)) };
    rcu_read_unlock();
    0
}

// upstream: intel_reset.c intel_gt_reset_trylock()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_reset_trylock(gt: *mut IntelGt, srcu: *mut c_int) -> c_int {
    unsafe { _intel_gt_reset_lock(gt, srcu, false) }
}

// upstream: intel_reset.c intel_gt_reset_lock_interruptible()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_reset_lock_interruptible(
    gt: *mut IntelGt,
    srcu: *mut c_int,
) -> c_int {
    unsafe { _intel_gt_reset_lock(gt, srcu, true) }
}

// upstream: intel_reset.c intel_gt_reset_unlock()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_reset_unlock(gt: *mut IntelGt, tag: c_int) {
    unsafe { srcu_read_unlock(core::ptr::addr_of_mut!((*gt).reset.backoff_srcu), tag) };
}

// upstream: intel_reset.c intel_gt_terminally_wedged()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_terminally_wedged(gt: *mut IntelGt) -> c_int {
    crate::linux::wait::might_sleep();
    if !unsafe { intel_gt_is_wedged(gt) } {
        return 0;
    }
    if unsafe { intel_gt_has_unrecoverable_error(gt) } {
        return -EIO;
    }
    while test_bit(I915_RESET_BACKOFF, unsafe { &(*gt).reset.flags }) {
        if unsafe { signal_pending_state(1, current_task_ptr()) } {
            return -EINTR;
        }
        axtask::yield_now();
    }
    if unsafe { intel_gt_is_wedged(gt) } {
        -EIO
    } else {
        0
    }
}

// upstream: intel_reset.c intel_gt_set_wedged_on_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_set_wedged_on_init(gt: *mut IntelGt) {
    assert!(
        I915_RESET_ENGINE + crate::intel_engine_types_upstream::I915_NUM_ENGINES as u32
            <= I915_WEDGED_ON_INIT
    );
    unsafe { intel_gt_set_wedged(gt) };
    unsafe { crate::i915_gpu_error_upstream::i915_disable_error_state((*gt).i915, -ENODEV) };
    set_bit(I915_WEDGED_ON_INIT, unsafe { &mut (*gt).reset.flags });
    unsafe { add_taint_for_CI((*gt).i915, TAINT_WARN) };
}

// upstream: intel_reset.c intel_gt_set_wedged_on_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_set_wedged_on_fini(gt: *mut IntelGt) {
    unsafe { intel_gt_set_wedged(gt) };
    unsafe { crate::i915_gpu_error_upstream::i915_disable_error_state((*gt).i915, -ENODEV) };
    set_bit(I915_WEDGED_ON_FINI, unsafe { &mut (*gt).reset.flags });
    let _ = unsafe {
        crate::intel_gt_requests_upstream::intel_gt_retire_requests_timeout(
            gt,
            0,
            core::ptr::null_mut(),
        )
    };
}

// upstream: intel_reset.c intel_gt_init_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_init_reset(gt: *mut IntelGt) {
    init_waitqueue_head(unsafe { &mut (*gt).reset.queue });
    unsafe { crate::linux::mutex::mutex_init(&mut (*gt).reset.mutex) };
    let _ = unsafe { init_srcu_struct(core::ptr::addr_of_mut!((*gt).reset.backoff_srcu)) };
    unsafe { INIT_WORK_C(&mut (*gt).wedge, set_wedged_work) };
    unsafe {
        crate::i915_gem_shrinker_upstream::i915_gem_shrinker_taints_mutex(
            (*gt).i915,
            core::ptr::addr_of_mut!((*gt).reset.mutex),
        )
    };
    set_bit(I915_WEDGED, unsafe { &mut (*gt).reset.flags });
}

// upstream: intel_reset.c intel_gt_fini_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_fini_reset(gt: *mut IntelGt) {
    unsafe { cleanup_srcu_struct(core::ptr::addr_of_mut!((*gt).reset.backoff_srcu)) };
}

// upstream: intel_reset.c intel_wedge_me()
unsafe extern "C" fn intel_wedge_me(work: *mut WorkStruct) {
    let delayed = unsafe {
        work.cast::<u8>()
            .sub(offset_of!(DelayedWork, work))
            .cast::<DelayedWork>()
    };
    let wedge = unsafe {
        delayed
            .cast::<u8>()
            .sub(offset_of!(IntelWedgeMe, work))
            .cast::<IntelWedgeMe>()
    };
    let gt = unsafe { (*wedge).gt };
    axlog::error!("{} timed out; cancelling all in-flight rendering", unsafe {
        log_c_string((*wedge).name)
    });
    unsafe { set_wedged_work(core::ptr::addr_of_mut!((*gt).wedge)) };
}

// upstream: intel_reset.c __intel_init_wedge()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __intel_init_wedge(
    w: *mut IntelWedgeMe,
    gt: *mut IntelGt,
    timeout: c_long,
    name: *const c_char,
) {
    unsafe {
        (*w).gt = gt;
        (*w).name = name;
    }
    unsafe {
        INIT_DELAYED_WORK(&mut (*w).work, |work| {
            let ptr = (work as *mut WorkStruct)
                .cast::<u8>()
                .sub(offset_of!(DelayedWork, work))
                .cast::<DelayedWork>();
            intel_wedge_me(core::ptr::addr_of_mut!((*ptr).work));
        })
    };
    let queue = unsafe { (*(*gt).i915).unordered_wq };
    let _ = unsafe {
        queue_delayed_work(
            queue,
            core::ptr::addr_of_mut!((*w).work),
            timeout.max(0) as u64,
        )
    };
}

// upstream: intel_reset.c __intel_fini_wedge()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __intel_fini_wedge(w: *mut IntelWedgeMe) {
    unsafe { cancel_delayed_work_sync(core::ptr::addr_of_mut!((*w).work)) };
    // The configured kernel disables `CONFIG_DEBUG_OBJECTS_WORK`, for which
    // Linux's `destroy_delayed_work_on_stack()` is an empty inline.
    unsafe { (*w).gt = core::ptr::null_mut() };
}

// upstream: intel_reset.c intel_engine_reset_needs_wa_22011802037()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_engine_reset_needs_wa_22011802037(gt: *mut IntelGt) -> bool {
    if GRAPHICS_VER(unsafe { (*gt).i915 }) < 11 {
        return false;
    }
    if crate::linux::i915::IS_GFX_GT_IP_STEP(
        gt,
        IP_VER(12, 70),
        crate::linux::i915::STEP_A0,
        crate::linux::i915::STEP_B0,
    ) {
        return true;
    }
    GRAPHICS_VER_FULL(unsafe { (*gt).i915 }) < IP_VER(12, 70)
}

#[inline]
fn current_task_ptr() -> *mut c_void {
    axhal::percpu::current_task_ptr::<()>().cast_mut().cast()
}
