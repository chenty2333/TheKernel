// SPDX-License-Identifier: MIT
// Copyright © 2026 Intel Corporation and TheKernel contributors.
// Linux v7.2.3 source: drivers/gpu/drm/i915/i915_drv.h, intel_device_info.h,
// intel_step.h, gt/intel_gt.h, i915_vma.h and i915_reg_defs.h.

#![allow(unsafe_code)]

use core::{
    ffi::{c_ulong, c_void},
    marker::PhantomData,
    mem::{offset_of, size_of},
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{
    i915_request_types_upstream::I915Request,
    i915_vma_types_upstream::I915Vma,
    intel_context_types_upstream::IntelContext,
    intel_engine_cs_upstream::{I915_NUM_ENGINES, IntelEngineCs, IntelEngineExeclists, IntelGt},
    intel_ring_types_upstream::IntelRing,
    intel_timeline_types_upstream::IntelTimeline,
    intel_workarounds_upstream::{I915McrReg, I915Reg, I915WaList},
    linux_i915_private::DrmI915Private,
};

/// Kernel-side PCI owner callback for Linux `INTEL_REVID(i915)`.
///
/// The GT crate deliberately does not mirror `struct pci_dev`; the kernel
/// adapter reads the PCI config-space revision from the actual owning device.
/// Return 0 and write the revision on success, or return a negative errno.
pub type I915PciRevisionReader = unsafe extern "C" fn(*mut DrmI915Private, *mut u8) -> i32;

static I915_PCI_REVISION_READER: AtomicUsize = AtomicUsize::new(0);

/// Install or clear the PCI revision reader supplied by the kernel device
/// owner. Clearing it makes future reads fail closed with `-ENODEV`.
pub fn install_pci_revision_reader(reader: Option<I915PciRevisionReader>) {
    let address = reader.map_or(0, |read| read as *const () as usize);
    I915_PCI_REVISION_READER.store(address, Ordering::Release);
}

/// Read the actual PCI config-space revision corresponding to Linux
/// `INTEL_REVID(i915)`. It never substitutes a graphics/media stepping.
pub unsafe fn i915_pci_revision(i915: *mut DrmI915Private) -> Result<u8, i32> {
    assert!(!i915.is_null());
    let address = I915_PCI_REVISION_READER.load(Ordering::Acquire);
    if address == 0 {
        return Err(-crate::linux::config::ENODEV);
    }

    // Function pointers are pointer-width values on TheKernel's x86_64-only
    // target. A zero address is rejected above before reconstituting one.
    let read: I915PciRevisionReader = unsafe { core::mem::transmute(address) };
    let mut revision = 0;
    let result = unsafe { read(i915, &mut revision) };
    if result == 0 {
        Ok(revision)
    } else {
        Err(result)
    }
}

/// Iterator for Linux's `for_each_engine(gt, engine)` idiom. The `gt` pointer
/// and its engine array must remain alive and immutable for the iterator's
/// lifetime; iteration yields shared engine references.
pub struct IntelEngineIterator<'a> {
    gt: *const IntelGt,
    index: usize,
    _lifetime: PhantomData<&'a IntelGt>,
}

/// Construct an iterator over populated slots in the Linux engine-ID array.
/// The caller guarantees `gt` remains valid for the inferred lifetime.
pub unsafe fn for_each_engine<'a>(gt: *mut IntelGt) -> IntelEngineIterator<'a> {
    IntelEngineIterator {
        gt,
        index: 0,
        _lifetime: PhantomData,
    }
}

/// Linux `HAS_ENGINE(gt, id)` over the exact engine-mask word.
#[allow(non_snake_case)]
pub unsafe fn HAS_ENGINE<I: TryInto<usize>>(gt: *mut IntelGt, id: I) -> bool {
    assert!(!gt.is_null());
    let id = id.try_into().ok().expect("negative/out-of-range engine id");
    assert!(id < u32::BITS as usize);
    unsafe { (*gt).info.engine_mask & (1u32 << id) != 0 }
}

/// Linux `ENGINE_INSTANCES_MASK(gt, first, count)` over the exact GT mask.
#[allow(non_snake_case)]
pub unsafe fn ENGINE_INSTANCES_MASK(gt: *mut IntelGt, first: i32, count: usize) -> u32 {
    assert!(!gt.is_null() && first >= 0 && count > 0);
    let first = first as u32;
    assert!(first + count as u32 <= u32::BITS);
    let mask = if count == u32::BITS as usize {
        u32::MAX
    } else {
        ((1u32 << count) - 1) << first
    };
    (unsafe { (*gt).info.engine_mask } & mask) >> first
}

#[allow(non_snake_case)]
pub unsafe fn RCS_MASK(gt: *mut IntelGt) -> u32 {
    unsafe {
        ENGINE_INSTANCES_MASK(
            gt,
            crate::intel_engine_cs_upstream::RCS0,
            crate::linux_config::I915_MAX_RCS,
        )
    }
}

#[allow(non_snake_case)]
pub unsafe fn BCS_MASK(gt: *mut IntelGt) -> u32 {
    unsafe {
        ENGINE_INSTANCES_MASK(
            gt,
            crate::intel_engine_cs_upstream::BCS0,
            crate::linux_config::I915_MAX_BCS,
        )
    }
}

#[allow(non_snake_case)]
pub unsafe fn VDBOX_MASK(gt: *mut IntelGt) -> u32 {
    unsafe {
        ENGINE_INSTANCES_MASK(
            gt,
            crate::intel_engine_cs_upstream::VCS0,
            crate::linux_config::I915_MAX_VCS,
        )
    }
}

#[allow(non_snake_case)]
pub unsafe fn VEBOX_MASK(gt: *mut IntelGt) -> u32 {
    unsafe {
        ENGINE_INSTANCES_MASK(
            gt,
            crate::intel_engine_cs_upstream::VECS0,
            crate::linux_config::I915_MAX_VECS,
        )
    }
}

#[allow(non_snake_case)]
pub unsafe fn CCS_MASK(gt: *mut IntelGt) -> u32 {
    unsafe {
        ENGINE_INSTANCES_MASK(
            gt,
            crate::intel_engine_cs_upstream::CCS0,
            crate::linux_config::I915_MAX_CCS,
        )
    }
}

/// Source inline helper: `port_mask + 1` is the active Execlists port count.
pub unsafe fn execlists_num_ports(execlists: *const IntelEngineExeclists) -> u32 {
    assert!(!execlists.is_null());
    unsafe { (*execlists).port_mask + 1 }
}

const I915_WEDGED_ON_INIT: u32 = 61;
const I915_WEDGED_ON_FINI: u32 = 62;
const I915_WEDGED: u32 = 63;

/// `intel_gt_has_unrecoverable_error()` from gt/intel_gt.h. The x86_64
/// target uses `BITS_PER_LONG == 64` for these reset-state bit numbers.
pub unsafe fn intel_gt_has_unrecoverable_error(gt: *const IntelGt) -> bool {
    assert!(!gt.is_null());
    let flags = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*gt).reset.flags)) };
    let mask = (1u64 << I915_WEDGED_ON_INIT) | (1u64 << I915_WEDGED_ON_FINI);
    (flags & mask) != 0
}

/// `intel_gt_is_wedged()` from gt/intel_gt.h, including its invariant check.
pub unsafe fn intel_gt_is_wedged(gt: *const IntelGt) -> bool {
    assert!(!gt.is_null());
    let flags = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*gt).reset.flags)) };
    let unrecoverable = (1u64 << I915_WEDGED_ON_INIT) | (1u64 << I915_WEDGED_ON_FINI);
    let wedged = 1u64 << I915_WEDGED;
    let has_unrecoverable = flags & unrecoverable != 0;
    let is_wedged = flags & wedged != 0;
    assert!(!has_unrecoverable || is_wedged);
    is_wedged
}

/// `intel_engine_uses_guc()` from gt/intel_engine.h.
pub unsafe fn intel_engine_uses_guc(engine: *const IntelEngineCs) -> bool {
    assert!(!engine.is_null());
    let gt = unsafe { (*engine).gt };
    assert!(!gt.is_null());
    unsafe { (*gt).submission_method >= crate::linux::registers::INTEL_SUBMISSION_GUC }
}

/// `intel_engine_set_hung_context()` from gt/intel_engine.h.
pub unsafe fn intel_engine_set_hung_context(
    engine: *mut IntelEngineCs,
    context: *mut IntelContext,
) {
    assert!(!engine.is_null());
    unsafe { (*engine).hung_ce = context.cast() };
}

/// `intel_engine_reset_needs_wa_22011802037()` from gt/intel_reset.c.
pub unsafe fn intel_engine_reset_needs_wa_22011802037(gt: *mut IntelGt) -> bool {
    assert!(!gt.is_null());
    let i915 = unsafe { (*gt).i915.cast::<c_void>().cast_const() };
    if unsafe { GRAPHICS_VER(i915) } < 11 {
        return false;
    }
    if unsafe { IS_GFX_GT_IP_STEP(gt, IP_VER(12, 70), STEP_A0, STEP_B0) } {
        return true;
    }
    if unsafe { GRAPHICS_VER_FULL(i915) } >= IP_VER(12, 70) {
        return false;
    }
    true
}

/// `intel_gt_clock_interval_to_ns()` from gt/intel_gt_clock_utils.c.
/// Uses a full-width product, matching `mul_u64_u32_div(count, 1e9, freq)`.
pub unsafe fn intel_gt_clock_interval_to_ns(gt: *const IntelGt, count: u64) -> u64 {
    assert!(!gt.is_null());
    let frequency = unsafe { (*gt).clock_frequency };
    assert_ne!(frequency, 0, "GEM_BUG_ON: GT clock frequency is unset");
    ((count as u128 * 1_000_000_000u128) / frequency as u128) as u64
}

/// `to_i915()` from i915_drv.h; `drm` is the first member of the private
/// object in the target layout.
#[inline]
pub unsafe fn to_i915(dev: *mut c_void) -> *mut DrmI915Private {
    assert!(!dev.is_null());
    dev.cast()
}

/// `i915_gem_get_pat_index()` from i915_gem.c.
pub unsafe fn i915_gem_get_pat_index(i915: *const DrmI915Private, level: u32) -> u32 {
    assert!(!i915.is_null());
    if level >= 4 {
        return 0;
    }
    let info = unsafe { (*i915).info.cast::<IntelDeviceInfoOverlay>() };
    assert!(!info.is_null());
    unsafe { (*info).cachelevel_to_pat[level as usize] }
}

/// N305/Gen12.55 MCR locking path from intel_gt_mcr.c. The later hardware
/// semaphore/forcewake path is deliberately refused until an owned uncore
/// MMIO backend is available; a spinlock alone is not equivalent there.
pub unsafe fn intel_gt_mcr_lock(gt: *mut IntelGt, flags: &mut c_ulong) {
    assert!(!gt.is_null());
    let i915 = unsafe { (*gt).i915.cast::<c_void>().cast_const() };
    if unsafe { GRAPHICS_VER_FULL(i915) } >= IP_VER(12, 70) {
        panic!("MCR hardware semaphore backend is unavailable on IP 12.70+");
    }
    unsafe { crate::linux_locks::spin_lock_irqsave(&mut (*gt).mcr_lock, flags) };
}

/// Release the N305/Gen12.55 software MCR lock. IP 12.70+ never reaches this
/// adapter because its lock acquisition fails closed above.
pub unsafe fn intel_gt_mcr_unlock(gt: *mut IntelGt, flags: c_ulong) {
    assert!(!gt.is_null());
    unsafe { crate::linux_locks::spin_unlock_irqrestore(&mut (*gt).mcr_lock, flags) };
}

/// `intel_ring_wrap()` from gt/intel_ring.h.
pub unsafe fn intel_ring_wrap(ring: *const IntelRing, pos: u32) -> u32 {
    assert!(!ring.is_null());
    pos & (unsafe { (*ring).size } - 1)
}

/// `intel_ring_offset_valid()` from gt/intel_ring.h.
pub unsafe fn intel_ring_offset_valid(ring: *const IntelRing, pos: u32) -> bool {
    assert!(!ring.is_null());
    let size = unsafe { (*ring).size };
    pos & size.wrapping_neg() == 0 && pos & 7 == 0
}

/// `assert_ring_tail_valid()` from gt/intel_ring.h.
pub unsafe fn assert_ring_tail_valid(ring: *const IntelRing, tail: u32) {
    assert!(!ring.is_null());
    let head = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*ring).head)) };
    assert!(unsafe { intel_ring_offset_valid(ring, tail) });
    let cacheline = crate::guc_submission::CACHELINE_BYTES as u32;
    assert!(tail / cacheline != head / cacheline || tail >= head);
}

/// `intel_ring_set_tail()` from gt/intel_ring.h.
pub unsafe fn intel_ring_set_tail(ring: *mut IntelRing, tail: u32) -> u32 {
    assert!(!ring.is_null());
    unsafe { assert_ring_tail_valid(ring, tail) };
    unsafe { core::ptr::addr_of_mut!((*ring).tail).write_volatile(tail) };
    tail
}

/// `intel_ring_direction()` from gt/intel_ring.h.
pub unsafe fn intel_ring_direction(ring: *const IntelRing, next: u32, prev: u32) -> i32 {
    assert!(!ring.is_null());
    (next
        .wrapping_sub(prev)
        .wrapping_shl(unsafe { (*ring).wrap })) as i32
}

/// `intel_ring_offset()` from gt/intel_ring.h.
pub unsafe fn intel_ring_offset(request: *const I915Request, addr: *const c_void) -> u32 {
    assert!(!request.is_null() && !addr.is_null());
    let ring = unsafe { (*request).ring };
    assert!(!ring.is_null());
    let base = unsafe { (*ring).vaddr as usize };
    let address = addr as usize;
    let offset = address.wrapping_sub(base) as u32;
    assert!(offset <= unsafe { (*ring).size });
    unsafe { intel_ring_wrap(ring, offset) }
}

/// `engine_class_to_guc_class()` from gt/uc/intel_guc_fwif.h.
pub fn engine_class_to_guc_class(class: u8) -> u8 {
    crate::guc_submission::engine_class_to_guc_class(class)
        .unwrap_or_else(|_| panic!("GEM_BUG_ON: invalid i915 engine class {class}"))
}

/// `guc_class_to_engine_class()` from gt/uc/intel_guc_fwif.h.
pub fn guc_class_to_engine_class(class: u8) -> u8 {
    crate::guc_ads::guc_class_to_engine_class(class)
        .unwrap_or_else(|| panic!("GEM_BUG_ON: invalid GuC engine class {class}"))
}

/// `guc_policy_max_exec_quantum_ms()` from gt/uc/intel_guc_fwif.h.
pub const fn guc_policy_max_exec_quantum_ms() -> u32 {
    100_000
}

/// Source inline helper from gt/intel_workarounds.h.
pub unsafe fn intel_wa_list_free(wal: *mut I915WaList) {
    assert!(!wal.is_null());
    let list = unsafe { (*wal).list };
    unsafe { crate::linux_memory::kfree(list) };
    unsafe { core::ptr::write_bytes(wal.cast::<u8>(), 0, size_of::<I915WaList>()) };
}

/// `intel_timeline_put()` from gt/intel_timeline.h.
pub unsafe fn intel_timeline_put(tl: *mut IntelTimeline) {
    assert!(!tl.is_null());
    if crate::linux_memory::refcount_dec_and_test(unsafe { &mut (*tl).kref.refcount }) {
        unsafe { crate::intel_timeline_upstream::__intel_timeline_free(&mut (*tl).kref) };
    }
}

impl<'a> Iterator for IntelEngineIterator<'a> {
    type Item = &'a IntelEngineCs;

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < I915_NUM_ENGINES as usize {
            let index = self.index;
            self.index += 1;
            // SAFETY: the iterator's lifetime is bounded by the caller's GT
            // lifetime, and the source GT engine array has this fixed length.
            let engine = unsafe { (*self.gt).engine[index] };
            if !engine.is_null() {
                // SAFETY: populated engine pointers remain valid for the GT
                // lifetime; this iterator yields only shared references.
                return Some(unsafe { &*engine });
            }
        }
        None
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IntelIpVersion {
    pub ver: u8,
    pub rel: u8,
    pub step: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IntelIp {
    pub ip: IntelIpVersion,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IntelStepInfo {
    pub graphics_step: u8,
    pub media_step: u8,
}

/// Only the runtime fields consumed by the imported i915 macros are named;
/// surrounding storage retains the exact Linux v7.2.3 `intel_runtime_info`
/// offsets and total size for the wt-dev x86_64 configuration.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IntelRuntimeInfo {
    pub graphics: IntelIp,
    pub media: IntelIp,
    _align_platform_mask: [u8; 2],
    pub platform_mask: [u32; 2],
    pub device_id: u16,
    pub step: IntelStepInfo,
    pub page_sizes: u32,
    pub ppgtt_type: i32,
    pub ppgtt_size: u32,
    pub has_pooled_eu: bool,
    _tail: [u8; 3],
}

/// `HAS_POOLED_EU(i915)` from intel_device_info.h.
#[inline]
pub unsafe fn HAS_POOLED_EU(i915: *const DrmI915Private) -> bool {
    assert!(!i915.is_null());
    let info = unsafe { (*i915).info.cast::<IntelDeviceInfoOverlay>() };
    assert!(!info.is_null());
    unsafe { (*info).runtime.has_pooled_eu }
}

/// `intel_device_info` layout from the wt-dev Linux 7.2.3 compile oracle.
/// `is_dgfx` is bit 2 in the first byte of `DEV_INFO_FOR_EACH_FLAG`.
#[repr(C, align(8))]
pub struct IntelDeviceInfoOverlay {
    _prefix: [u8; 16],
    pub gt: u8,
    _pad_gt: [u8; 3],
    pub platform_engine_mask: u32,
    pub memory_regions: u32,
    pub flags: [u8; 5],
    _pad: [u8; 3],
    pub runtime: IntelRuntimeInfo,
    pub cachelevel_to_pat: [u32; 4],
    pub max_pat_index: u32,
}
const _: [(); 96] = [(); size_of::<IntelDeviceInfoOverlay>()];
const _: [(); 16] = [(); offset_of!(IntelDeviceInfoOverlay, gt)];
const _: [(); 20] = [(); offset_of!(IntelDeviceInfoOverlay, platform_engine_mask)];
const _: [(); 24] = [(); offset_of!(IntelDeviceInfoOverlay, memory_regions)];
const _: [(); 28] = [(); offset_of!(IntelDeviceInfoOverlay, flags)];
const _: [(); 72] = [(); offset_of!(IntelDeviceInfoOverlay, cachelevel_to_pat)];
const _: [(); 88] = [(); offset_of!(IntelDeviceInfoOverlay, max_pat_index)];

/// `INTEL_INFO(i915)` source accessor for the configured device-info overlay.
pub unsafe fn INTEL_INFO<P: I915PrivatePtr>(i915: P) -> *const IntelDeviceInfoOverlay {
    (*(i915.as_i915_private().cast::<DrmI915Private>()))
        .info
        .cast::<IntelDeviceInfoOverlay>()
}

/// Source `HAS_L3_CCS_READ(i915)` from the device-info flag bitfield.
#[allow(non_snake_case)]
pub unsafe fn HAS_L3_CCS_READ<P: I915PrivatePtr>(i915: P) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    !info.is_null() && (unsafe { (*info).flags[2] } & 1) != 0
}

/// `tuning_thread_rr_after_dep` is flag index 34 in DEV_INFO_FOR_EACH_FLAG.
pub unsafe fn tuning_thread_rr_after_dep<P: I915PrivatePtr>(i915: P) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    !info.is_null() && (unsafe { (*info).flags[4] } & (1 << 2)) != 0
}

/// Source `HWS_NEEDS_PHYSICAL(i915)` bit from `DEV_INFO_FOR_EACH_FLAG`.
#[allow(non_snake_case)]
pub unsafe fn HWS_NEEDS_PHYSICAL<P: I915PrivatePtr>(i915: P) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    !info.is_null() && (unsafe { (*info).flags[4] } & (1 << 4)) != 0
}

/// Source `HAS_64BIT_RELOC(i915)` from `i915_drv.h` and the
/// `has_64bit_reloc` device-info flag (flag index 3 in Linux 7.2.3).
#[allow(non_snake_case)]
pub unsafe fn HAS_64BIT_RELOC<P: I915PrivatePtr>(i915: P) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    !info.is_null() && (unsafe { (*info).flags[0] } & (1 << 3)) != 0
}

/// `HAS_FLAT_CCS(i915)` from i915_drv.h; source flag bit 9.
#[allow(non_snake_case)]
pub unsafe fn HAS_FLAT_CCS<P: I915PrivatePtr>(i915: P) -> bool {
    let info = INTEL_INFO(i915);
    !info.is_null() && ((*info).flags[1] & (1 << 1)) != 0
}

/// `HAS_3D_PIPELINE(i915)` from i915_drv.h, device-info flag bit 8.
#[allow(non_snake_case)]
pub unsafe fn HAS_3D_PIPELINE<P: I915PrivatePtr>(i915: P) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    !info.is_null() && (unsafe { (*info).flags[1] } & 1) != 0
}

/// Prefix overlay for `drm_i915_private.__runtime`. Offset and runtime size
/// were obtained from the source kernel's x86_64 v7.2.3 compile configuration.
#[repr(C)]
pub struct DrmI915RuntimeOverlay {
    _prefix: [u8; 1656],
    pub runtime: IntelRuntimeInfo,
}

const _: [(); 36] = [(); size_of::<IntelRuntimeInfo>()];
const _: [(); 1656] = [(); offset_of!(DrmI915RuntimeOverlay, runtime)];
const _: [(); 3] = [(); offset_of!(IntelRuntimeInfo, media)];
const _: [(); 8] = [(); offset_of!(IntelRuntimeInfo, platform_mask)];
const _: [(); 16] = [(); offset_of!(IntelRuntimeInfo, device_id)];
const _: [(); 18] = [(); offset_of!(IntelRuntimeInfo, step)];

/// C i915 version macros are called with either the driver's private object
/// or an opaque pointer already typed by a source adapter. Accept both while
/// keeping one layout-checked runtime-info access path.
pub trait I915PrivatePtr: Copy {
    fn as_i915_private(self) -> *const c_void;
}

impl I915PrivatePtr for *const c_void {
    fn as_i915_private(self) -> *const c_void {
        self
    }
}
impl I915PrivatePtr for *mut c_void {
    fn as_i915_private(self) -> *const c_void {
        self.cast_const()
    }
}
impl I915PrivatePtr for *const DrmI915Private {
    fn as_i915_private(self) -> *const c_void {
        self.cast()
    }
}
impl I915PrivatePtr for *mut DrmI915Private {
    fn as_i915_private(self) -> *const c_void {
        self.cast()
    }
}

#[inline]
unsafe fn runtime_info<P: I915PrivatePtr>(i915: P) -> *const IntelRuntimeInfo {
    core::ptr::addr_of!((*i915.as_i915_private().cast::<DrmI915RuntimeOverlay>()).runtime)
}

#[allow(non_snake_case)]
pub const fn IP_VER(ver: u16, rel: u16) -> u16 {
    (ver << 8) | rel
}

#[allow(non_snake_case)]
pub unsafe fn GRAPHICS_VER<P: I915PrivatePtr>(i915: P) -> u8 {
    (*runtime_info(i915)).graphics.ip.ver
}

#[allow(non_snake_case)]
pub unsafe fn GRAPHICS_VER_FULL<P: I915PrivatePtr>(i915: P) -> u16 {
    let ip = (*runtime_info(i915)).graphics.ip;
    IP_VER(ip.ver as u16, ip.rel as u16)
}

#[allow(non_snake_case)]
pub unsafe fn graphics_ver<P: I915PrivatePtr>(i915: P) -> u8 {
    GRAPHICS_VER(i915)
}

#[allow(non_snake_case)]
pub unsafe fn graphics_ver_full<P: I915PrivatePtr>(i915: P) -> u16 {
    GRAPHICS_VER_FULL(i915)
}

#[allow(non_snake_case)]
pub unsafe fn MEDIA_VER<P: I915PrivatePtr>(i915: P) -> u8 {
    (*runtime_info(i915)).media.ip.ver
}

#[allow(non_snake_case)]
pub unsafe fn MEDIA_VER_FULL<P: I915PrivatePtr>(i915: P) -> u16 {
    let ip = (*runtime_info(i915)).media.ip;
    IP_VER(ip.ver as u16, ip.rel as u16)
}

#[allow(non_snake_case)]
pub unsafe fn IS_GRAPHICS_VER<P: I915PrivatePtr>(i915: P, from: u8, until: u8) -> bool {
    let ver = GRAPHICS_VER(i915);
    ver >= from && ver <= until
}

#[allow(non_snake_case)]
pub unsafe fn IS_GRAPHICS_STEP<P: I915PrivatePtr>(i915: P, since: u8, until: u8) -> bool {
    let step = (*runtime_info(i915)).step.graphics_step;
    step >= since && step < until
}

#[allow(non_snake_case)]
pub unsafe fn IS_MEDIA_STEP<P: I915PrivatePtr>(i915: P, since: u8, until: u8) -> bool {
    let step = (*runtime_info(i915)).step.media_step;
    step >= since && step < until
}

const PLATFORM_MASK_BITS: u32 = 32 - 4;
const SUBPLATFORM_MASK: u32 = 0xf;
const INTEL_I915G: u32 = 5;
const INTEL_I915GM: u32 = 6;
const INTEL_I965G: u32 = 11;
const INTEL_I965GM: u32 = 12;
const INTEL_G45: u32 = 13;
const INTEL_IVYBRIDGE: u32 = 17;
const INTEL_VALLEYVIEW: u32 = 18;
const INTEL_HASWELL: u32 = 19;
const INTEL_BROADWELL: u32 = 20;
const INTEL_CHERRYVIEW: u32 = 21;
const INTEL_SKYLAKE: u32 = 22;
const INTEL_BROXTON: u32 = 23;
const INTEL_KABYLAKE: u32 = 24;
const INTEL_GEMINILAKE: u32 = 25;
const INTEL_COFFEELAKE: u32 = 26;
const INTEL_COMETLAKE: u32 = 27;
const INTEL_ICELAKE: u32 = 28;
const INTEL_ELKHARTLAKE: u32 = 29;
const INTEL_JASPERLAKE: u32 = 30;
pub const INTEL_TIGERLAKE: u32 = 31;
const INTEL_ROCKETLAKE: u32 = 32;
const INTEL_DG1: u32 = 33;
const INTEL_ALDERLAKE_S: u32 = 34;
const INTEL_ALDERLAKE_P: u32 = 35;
const INTEL_DG2: u32 = 36;
const INTEL_METEORLAKE: u32 = 37;
const INTEL_PINEVIEW: u32 = 10;

/// Source semantics of `HAS_EXECLISTS(i915)`/`HAS_LOGICAL_RING_CONTEXTS`: the
/// device-info bit is flag index 19 (byte 2, bit 3) in Linux v7.2.3.
#[allow(non_snake_case)]
pub unsafe fn HAS_EXECLISTS<P: I915PrivatePtr>(i915: P) -> bool {
    let info = (*(i915.as_i915_private().cast::<DrmI915Private>())).info;
    !info.is_null() && ((*info.cast::<IntelDeviceInfoOverlay>()).flags[2] & (1 << 3)) != 0
}

/// `HAS_LLC(i915)` from i915_drv.h; has_llc is the 19th source flag bit.
#[allow(non_snake_case)]
pub unsafe fn HAS_LLC<P: I915PrivatePtr>(i915: P) -> bool {
    let info = (*(i915.as_i915_private().cast::<DrmI915Private>())).info;
    !info.is_null() && ((*info.cast::<IntelDeviceInfoOverlay>()).flags[2] & (1 << 2)) != 0
}

/// `HAS_SNOOP(i915)` from i915_drv.h; has_snoop is source flag bit 32.
#[allow(non_snake_case)]
pub unsafe fn HAS_SNOOP<P: I915PrivatePtr>(i915: P) -> bool {
    let info = (*(i915.as_i915_private().cast::<DrmI915Private>())).info;
    !info.is_null() && ((*info.cast::<IntelDeviceInfoOverlay>()).flags[4] & 1) != 0
}

/// `HAS_LMEM(i915)` from i915_drv.h, using INTEL_REGION_LMEM_0 (bit 1).
#[allow(non_snake_case)]
pub unsafe fn HAS_LMEM<P: I915PrivatePtr>(i915: P) -> bool {
    let info = (*(i915.as_i915_private().cast::<DrmI915Private>())).info;
    !info.is_null() && ((*info.cast::<IntelDeviceInfoOverlay>()).memory_regions & (1 << 1)) != 0
}

/// `HAS_WT(i915)` from i915_drv.h; WT capability is the measured eDRAM size.
#[allow(non_snake_case)]
pub unsafe fn HAS_WT<P: I915PrivatePtr>(i915: P) -> bool {
    (*(i915.as_i915_private().cast::<DrmI915Private>())).edram_size_mb != 0
}

/// `to_gt(i915)` from gt/intel_gt.h: GT0 is the primary GT in this ABI.
pub unsafe fn to_gt<P: I915PrivatePtr>(i915: P) -> *mut IntelGt {
    (*(i915.as_i915_private().cast::<DrmI915Private>())).gt[0]
}

#[allow(non_snake_case)]
pub unsafe fn IS_PLATFORM<P: I915PrivatePtr>(i915: P, platform: u32) -> bool {
    let runtime = &*runtime_info(i915);
    let mask_index = platform / PLATFORM_MASK_BITS;
    let mask_bit = platform % PLATFORM_MASK_BITS + 4;
    runtime.platform_mask[mask_index as usize] & (1 << mask_bit) != 0
}

#[allow(non_snake_case)]
pub unsafe fn IS_SUBPLATFORM<P: I915PrivatePtr>(i915: P, platform: u32, subplatform: u32) -> bool {
    if subplatform >= 4 || !IS_PLATFORM(i915, platform) {
        return false;
    }
    let runtime = &*runtime_info(i915);
    let mask_index = platform / PLATFORM_MASK_BITS;
    runtime.platform_mask[mask_index as usize] & (1 << subplatform) != 0
}

macro_rules! platform_predicates {
    ($($name:ident = $platform:ident),+ $(,)?) => {$ (
        #[allow(non_snake_case)]
        pub unsafe fn $name<P: I915PrivatePtr>(i915: P) -> bool {
            IS_PLATFORM(i915, $platform)
        }
    )+};
}

platform_predicates! {
    IS_I915G = INTEL_I915G,
    IS_I915GM = INTEL_I915GM,
    IS_I965G = INTEL_I965G,
    IS_I965GM = INTEL_I965GM,
    IS_G4X = INTEL_G45,
    IS_IVYBRIDGE = INTEL_IVYBRIDGE,
    IS_VALLEYVIEW = INTEL_VALLEYVIEW,
    IS_HASWELL = INTEL_HASWELL,
    IS_BROADWELL = INTEL_BROADWELL,
    IS_CHERRYVIEW = INTEL_CHERRYVIEW,
    IS_SKYLAKE = INTEL_SKYLAKE,
    IS_BROXTON = INTEL_BROXTON,
    IS_KABYLAKE = INTEL_KABYLAKE,
    IS_GEMINILAKE = INTEL_GEMINILAKE,
    IS_COFFEELAKE = INTEL_COFFEELAKE,
    IS_COMETLAKE = INTEL_COMETLAKE,
    IS_ICELAKE = INTEL_ICELAKE,
    IS_ELKHARTLAKE = INTEL_ELKHARTLAKE,
    IS_JASPERLAKE = INTEL_JASPERLAKE,
    IS_TIGERLAKE = INTEL_TIGERLAKE,
    IS_ROCKETLAKE = INTEL_ROCKETLAKE,
    IS_DG1 = INTEL_DG1,
    IS_ALDERLAKE_S = INTEL_ALDERLAKE_S,
    IS_ALDERLAKE_P = INTEL_ALDERLAKE_P,
    IS_DG2 = INTEL_DG2,
    IS_METEORLAKE = INTEL_METEORLAKE,
}

#[allow(non_snake_case)]
pub unsafe fn IS_DG2_G10<P: I915PrivatePtr>(i915: P) -> bool {
    IS_SUBPLATFORM(i915, INTEL_DG2, 0)
}

#[allow(non_snake_case)]
pub unsafe fn IS_DG2_G11<P: I915PrivatePtr>(i915: P) -> bool {
    IS_SUBPLATFORM(i915, INTEL_DG2, 1)
}

/// `HAS_GUC_TLB_INVALIDATION()` from i915_drv.h.
#[allow(non_snake_case)]
pub unsafe fn HAS_GUC_TLB_INVALIDATION<P: I915PrivatePtr>(i915: P) -> bool {
    let i915 = i915.as_i915_private().cast::<DrmI915Private>();
    assert!(!i915.is_null());
    let info = unsafe { (*i915).info.cast::<IntelDeviceInfoOverlay>() };
    assert!(!info.is_null());
    // DEV_INFO_FOR_EACH_FLAG places this flag at bit 15, after the three
    // platform flags and twelve alphabetically ordered feature flags.
    unsafe { (*info).flags[1] & (1 << 7) != 0 }
}

/// `IS_DGFX()` from i915_drv.h, reading the asserted `is_dgfx` bit from the
/// source `intel_device_info` object.
#[allow(non_snake_case)]
pub unsafe fn IS_DGFX<P: I915PrivatePtr>(i915: P) -> bool {
    let i915 = i915.as_i915_private().cast::<DrmI915Private>();
    assert!(!i915.is_null());
    let info = unsafe { (*i915).info.cast::<IntelDeviceInfoOverlay>() };
    assert!(!info.is_null());
    unsafe { (*info).flags[0] & (1 << 2) != 0 }
}

/// `IS_MOBILE()` from i915_drv.h; the first `DEV_INFO_FOR_EACH_FLAG` bit is
/// `is_mobile` in the Linux x86_64 `intel_device_info` layout.
#[allow(non_snake_case)]
pub unsafe fn IS_MOBILE<P: I915PrivatePtr>(i915: P) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    !info.is_null() && unsafe { (*info).flags[0] & 1 != 0 }
}

/// `IS_PINEVIEW()` platform test used by the i915 FSB clock helper.
#[allow(non_snake_case)]
pub unsafe fn IS_PINEVIEW<P: I915PrivatePtr>(i915: P) -> bool {
    unsafe { IS_PLATFORM(i915, INTEL_PINEVIEW) }
}

/// `HAS_GT_UC(i915)` from i915_drv.h (DEV_INFO_FOR_EACH_FLAG bit 11).
#[allow(non_snake_case)]
pub unsafe fn HAS_GT_UC<P: I915PrivatePtr>(i915: P) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    !info.is_null() && unsafe { (*info).flags[1] & (1 << 3) != 0 }
}

/// `HAS_GUC_DEPRIVILEGE(i915)` from i915_drv.h (flag bit 14).
#[allow(non_snake_case)]
pub unsafe fn HAS_GUC_DEPRIVILEGE<P: I915PrivatePtr>(i915: P) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    !info.is_null() && unsafe { (*info).flags[1] & (1 << 6) != 0 }
}

/// `HAS_128_BYTE_Y_TILING()` from i915_drv.h.
#[allow(non_snake_case)]
pub unsafe fn HAS_128_BYTE_Y_TILING<P: I915PrivatePtr>(i915: P) -> bool {
    !IS_I915G(i915) && !IS_I915GM(i915)
}

#[allow(non_snake_case)]
pub unsafe fn IS_GEN9_LP<P: I915PrivatePtr>(i915: P) -> bool {
    IS_BROXTON(i915) || IS_GEMINILAKE(i915)
}

pub const STEP_NONE: u8 = 0;
pub const STEP_A0: u8 = 1;
pub const STEP_B0: u8 = 5;
pub const STEP_C0: u8 = 9;
pub const STEP_F0: u8 = 21;
pub const STEP_H0: u8 = 29;
pub const STEP_FOREVER: u8 = 42;

#[allow(non_snake_case)]
pub unsafe fn IS_GFX_GT_IP_RANGE(gt: *mut IntelGt, from: u16, until: u16) -> bool {
    (*gt).type_ != 2
        && GRAPHICS_VER_FULL((*gt).i915.cast::<c_void>().cast_const()) >= from
        && GRAPHICS_VER_FULL((*gt).i915.cast::<c_void>().cast_const()) <= until
}

#[allow(non_snake_case)]
pub unsafe fn IS_MEDIA_GT_IP_RANGE(gt: *mut IntelGt, from: u16, until: u16) -> bool {
    !gt.is_null()
        && (*gt).type_ == 2
        && MEDIA_VER_FULL((*gt).i915.cast::<c_void>().cast_const()) >= from
        && MEDIA_VER_FULL((*gt).i915.cast::<c_void>().cast_const()) <= until
}

#[allow(non_snake_case)]
pub unsafe fn IS_GFX_GT_IP_STEP(gt: *mut IntelGt, ip: u16, since: u8, until: u8) -> bool {
    IS_GFX_GT_IP_RANGE(gt, ip, ip)
        && IS_GRAPHICS_STEP((*gt).i915.cast::<c_void>().cast_const(), since, until)
}

pub trait I915MmioRegister {
    fn offset(self) -> u32;
}

impl I915MmioRegister for I915Reg {
    fn offset(self) -> u32 {
        self.reg
    }
}

impl I915MmioRegister for I915McrReg {
    fn offset(self) -> u32 {
        self.reg
    }
}

#[inline]
pub unsafe fn intel_engine_is_virtual(
    engine: *const crate::intel_engine_cs_upstream::IntelEngineCs,
) -> bool {
    !engine.is_null() && (*engine).flags & crate::linux_config::I915_ENGINE_IS_VIRTUAL != 0
}

#[inline]
pub fn i915_mmio_reg_offset<R: I915MmioRegister>(register: R) -> u32 {
    register.offset()
}

/// `drm_mm_node.start` follows its `unsigned long color` at byte offset 8 on
/// the source x86_64 ABI. `i915_vma_is_ggtt()` checks bit 13 of vma flags.
#[inline]
pub unsafe fn i915_ggtt_offset(vma: *const I915Vma) -> u32 {
    let flags = (*vma).flags.counter as u32;
    assert_ne!(flags & (1 << 13), 0, "i915_ggtt_offset requires a GGTT VMA");
    let start = core::ptr::read_unaligned(vma.cast::<u8>().add(8).cast::<u64>());
    assert_eq!(start >> 32, 0, "GGTT offset exceeds u32");
    let end = start.checked_add((*vma).size).expect("GGTT range overflow");
    assert_eq!(end.saturating_sub(1) >> 32, 0, "GGTT range exceeds u32");
    start as u32
}

/// `guc_to_gt()` / `gt_to_guc()` from gt/intel_gt.h. The offset is the sum of
/// `offsetof(intel_gt, uc)` and `offsetof(intel_uc, guc)` from Linux v7.2.3.
pub const GT_GUC_OFFSET: usize = 624;

#[inline]
pub unsafe fn gt_to_guc(gt: *mut IntelGt) -> *mut crate::intel_guc_types_upstream::IntelGuc {
    gt.cast::<u8>().add(GT_GUC_OFFSET).cast()
}

#[inline]
pub unsafe fn guc_to_gt(guc: *mut crate::intel_guc_types_upstream::IntelGuc) -> *mut IntelGt {
    guc.cast::<u8>().sub(GT_GUC_OFFSET).cast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct RuntimeFixture {
        _prefix: [u8; 1656],
        runtime: IntelRuntimeInfo,
    }

    #[test]
    fn runtime_version_and_platform_masks_follow_i915_layout() {
        let mut fixture = RuntimeFixture {
            _prefix: [0; 1656],
            runtime: IntelRuntimeInfo::default(),
        };
        fixture.runtime.graphics.ip = IntelIpVersion {
            ver: 12,
            rel: 55,
            step: 0,
        };
        fixture.runtime.media.ip = IntelIpVersion {
            ver: 12,
            rel: 0,
            step: 0,
        };
        fixture.runtime.step.graphics_step = STEP_A0;
        let dg2_index = INTEL_DG2 / PLATFORM_MASK_BITS;
        let dg2_bit = INTEL_DG2 % PLATFORM_MASK_BITS + 4;
        fixture.runtime.platform_mask[dg2_index as usize] = (1 << dg2_bit) | 1;

        let i915 = (&mut fixture as *mut RuntimeFixture).cast::<c_void>();
        unsafe {
            assert_eq!(GRAPHICS_VER(i915), 12);
            assert_eq!(GRAPHICS_VER_FULL(i915), IP_VER(12, 55));
            assert!(IS_DG2(i915));
            assert!(IS_DG2_G10(i915));
            assert!(IS_GRAPHICS_STEP(i915, STEP_A0, STEP_B0));
        }
    }
}
