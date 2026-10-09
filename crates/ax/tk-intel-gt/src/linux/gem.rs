// SPDX-License-Identifier: MIT
// Copyright © 2026 TheKernel contributors.
// Linux v7.2.3 DRM/i915 GEM shared layout records.

use core::{
    ffi::{c_char, c_int, c_long, c_ulong, c_void},
    mem::{align_of, offset_of, size_of},
};

use crate::{
    i915_gem_context_types_upstream::I915GemContext,
    intel_context_types_upstream::File,
    linux_i915_private::Inode,
    i915_gem_ww_upstream::WwAcquireCtx,
    intel_context_upstream::{DrmGemObjectBaseLayout, DrmVmaOffsetNode},
    intel_engine_cs_upstream::ListHead,
    linux_i915_private::DrmVmaOffsetManager,
    linux::ww_mutex::{
        WwMutex, ww_mutex_lock, ww_mutex_lock_slow, ww_mutex_trylock, ww_mutex_unlock,
    },
};

use crate::intel_context_upstream::DmaFence;

/// The C type name used by DRM/i915 call sites for the 480-byte base union.
/// Accessed `drm_gem_object` fields use the Linux 7.2.3 x86_64 record layout;
/// opaque storage preserves the larger `ttm_buffer_object` union size.
pub type DrmGemObject = DrmGemObjectBaseLayout;

/// `drm_gem_is_imported()` from `include/drm/drm_gem.h`.
#[inline]
pub unsafe fn drm_gem_is_imported(obj: *const DrmGemObject) -> bool {
    !unsafe { (*obj).import_attach }.is_null()
}

/// `drm_vma_node_start()` from `include/drm/drm_vma_manager.h`.
#[cfg(feature = "upstream-gt")]
#[inline]
pub unsafe fn drm_vma_node_start(node: *const DrmVmaOffsetNode) -> c_ulong {
    unsafe { (*node).vm_node.start as c_ulong }
}

/// `drm_vma_node_offset_addr()` from `include/drm/drm_vma_manager.h`.
#[cfg(feature = "upstream-gt")]
#[inline]
pub unsafe fn drm_vma_node_offset_addr(node: *const DrmVmaOffsetNode) -> u64 {
    unsafe { ((*node).vm_node.start as u64) << crate::linux_config::PAGE_SHIFT }
}

/// `drm_vma_node_reset()` from `include/drm/drm_vma_manager.h` for the
/// configured LOCKDEP=n, PREEMPT_RT=n target. In this configuration the
/// unlocked `rwlock_t` initializer and `RB_ROOT` are all-zero representations.
/// The source precondition applies: `node` must not currently be allocated in
/// a VMA-offset manager.
#[cfg(feature = "upstream-gt")]
#[inline]
pub unsafe fn drm_vma_node_reset(node: *mut DrmVmaOffsetNode) {
    unsafe { core::ptr::write_bytes(node, 0, 1) };
}

/// Linux `drm_vma_node_unmap()` header helper. Preserve private COW mappings
/// when invalidating the DRM object's shared mapping (`even_cows = 1`).
#[cfg(feature = "upstream-gt")]
pub unsafe fn drm_vma_node_unmap(node: *mut DrmVmaOffsetNode, mapping: *mut c_void) {
    let mm_node = unsafe { &(*node).vm_node };
    if !mm_node.mm.is_null() {
        unsafe {
            unmap_mapping_range(
                mapping,
                drm_vma_node_offset_addr(node) as c_long,
                (mm_node.size << crate::linux_config::PAGE_SHIFT) as c_long,
                1,
            );
        }
    }
}

/// Linux v7.2.3 `struct dma_resv` for CONFIG_PREEMPT_RT=n and LOCKDEP=n.
#[repr(C)]
pub struct DmaResv {
    pub lock: WwMutex,
    pub fences: *mut c_void,
}

unsafe extern "C" {
    pub fn get_file_active(file: *mut *mut File) -> *mut File;
    pub fn anon_inode_getfile(
        name: *const c_char,
        fops: *const FileOperations,
        private_data: *mut c_void,
        flags: c_int,
    ) -> *mut File;
    pub fn drm_dev_get(dev: *mut c_void) -> *mut c_void;
    pub fn drm_dev_put(dev: *mut c_void);
    // Exported DRM VMA-offset manager APIs from drm_vma_manager.c. The node
    // and manager records are the kernel's native C objects; these declarations
    // bind the existing DRM implementation rather than reimplementing it.
    pub fn drm_vma_offset_add(
        manager: *mut DrmVmaOffsetManager,
        node: *mut DrmVmaOffsetNode,
        pages: c_ulong,
    ) -> c_int;
    pub fn drm_vma_offset_remove(manager: *mut DrmVmaOffsetManager, node: *mut DrmVmaOffsetNode);
    pub fn drm_vma_node_allow_once(node: *mut DrmVmaOffsetNode, file: *mut DrmFile) -> c_int;
    pub fn drm_vma_offset_lookup_locked(
        manager: *mut DrmVmaOffsetManager,
        start: c_ulong,
        pages: c_ulong,
    ) -> *mut DrmVmaOffsetNode;
    pub fn drm_vma_node_is_allowed(node: *mut DrmVmaOffsetNode, file: *mut DrmFile) -> bool;
    fn unmap_mapping_range(
        mapping: *mut c_void,
        holebegin: c_long,
        holelen: c_long,
        even_cows: i32,
    );
    #[link_name = "dma_resv_fini"]
    fn __dma_resv_fini(resv: *mut DmaResv);
    #[link_name = "dma_resv_get_singleton"]
    fn __dma_resv_get_singleton(
        resv: *mut DmaResv,
        usage: i32,
        fence: *mut *mut DmaFence,
    ) -> i32;
    #[link_name = "dma_resv_wait_timeout"]
    fn __dma_resv_wait_timeout(
        resv: *mut DmaResv,
        usage: i32,
        intr: bool,
        timeout: c_long,
    ) -> c_long;
}

/// Linux reservation-object API from `include/linux/dma-resv.h`.
pub unsafe fn dma_resv_fini(resv: *mut DmaResv) {
    unsafe { __dma_resv_fini(resv) };
}

/// Linux reservation-object API from `include/linux/dma-resv.h`.
pub unsafe fn dma_resv_get_singleton(
    resv: *mut DmaResv,
    usage: u32,
    fence: *mut *mut DmaFence,
) -> i32 {
    unsafe { __dma_resv_get_singleton(resv, usage as i32, fence) }
}

/// Linux reservation-object API from `include/linux/dma-resv.h`.
pub unsafe fn dma_resv_wait_timeout(
    resv: *mut DmaResv,
    usage: u32,
    intr: bool,
    timeout: c_long,
) -> c_long {
    unsafe { __dma_resv_wait_timeout(resv, usage as i32, intr, timeout) }
}

const _: [(); 40] = [(); size_of::<DmaResv>()];
const _: [(); 0] = [(); offset_of!(DmaResv, lock)];
const _: [(); 32] = [(); offset_of!(DmaResv, fences)];

/// `dma_resv_lock()` source-order API. The current task model has no POSIX
/// signal delivery; interruptible callers otherwise use the same wait path.
pub unsafe fn dma_resv_lock(
    resv: *mut c_void,
    ctx: *mut WwAcquireCtx,
    _intr: bool,
    try_only: bool,
) -> i32 {
    let lock = unsafe { core::ptr::addr_of_mut!((*resv.cast::<DmaResv>()).lock) };
    if try_only {
        if unsafe { ww_mutex_trylock(lock, ctx) } {
            0
        } else {
            -crate::linux_config::EBUSY
        }
    } else {
        unsafe { ww_mutex_lock(lock, ctx) }
    }
}

/// `dma_resv_lock_slow()` after the WW caller has dropped all other locks.
pub unsafe fn dma_resv_lock_slow(resv: *mut c_void, ctx: *mut WwAcquireCtx) {
    let lock = unsafe { core::ptr::addr_of_mut!((*resv.cast::<DmaResv>()).lock) };
    unsafe { ww_mutex_lock_slow(lock, ctx) };
}

/// Interruptible source API. Signals are not part of the active task model.
pub unsafe fn dma_resv_lock_slow_interruptible(resv: *mut c_void, ctx: *mut WwAcquireCtx) -> i32 {
    unsafe { dma_resv_lock_slow(resv, ctx) };
    0
}

/// `dma_resv_unlock()` releases the underlying WW mutex.
pub unsafe fn dma_resv_unlock(resv: *mut c_void) {
    let lock = unsafe { core::ptr::addr_of_mut!((*resv.cast::<DmaResv>()).lock) };
    unsafe { ww_mutex_unlock(lock) };
}

/// The `struct drm_device` member used by `drm_dev_to_dev()`.
///
/// Linux v7.2.3 places `dev` after `if_version` and the embedded `kref`;
/// both fields occupy eight bytes in the configured x86_64 ABI.
#[repr(C)]
pub struct DrmDevice {
    _if_version: i32,
    _pad: u32,
    _ref: crate::intel_context_upstream::Kref,
    pub dev: *mut core::ffi::c_void,
}

#[inline]
pub unsafe fn drm_device_device(drm: *mut core::ffi::c_void) -> *mut core::ffi::c_void {
    unsafe { (*drm.cast::<DrmDevice>()).dev }
}

/// Opaque TTM arm of `drm_i915_gem_object.base`; target-configured x86_64
/// storage is 480 bytes and the i915 owner does not access TTM-private fields.
#[repr(C, align(8))]
pub struct TtmBufferObjectLayout {
    /// The DRM GEM base is the leading TTM member; the remaining TTM-private
    /// payload is not accessed by this GT/GEM translation.
    pub base: DrmGemObjectBaseLayout,
}
const _: [(); 480] = [(); size_of::<TtmBufferObjectLayout>()];
const _: [(); 8] = [(); align_of::<TtmBufferObjectLayout>()];

/// `i915_gem_to_ttm()` from `gem/i915_gem_ttm.h`: the TTM BO is the
/// alternate arm of the GEM object's base union, at the same address.
#[cfg(feature = "upstream-gt")]
#[inline]
pub unsafe fn i915_gem_to_ttm(obj: *mut crate::i915_gem_object_types_upstream::DrmI915GemObject) -> *mut TtmBufferObjectLayout {
    unsafe { core::ptr::addr_of_mut!((*obj).base.__do_not_access).cast() }
}

/// Source-layout prefix through `drm_file::driver_priv`, the member used by
/// GEM handle close paths. Later DRM file fields are not accessed here.
#[repr(C)]
pub struct DrmFile {
    _before_minor: [u8; 72],
    pub minor: *mut DrmMinor,
    _before_driver_priv: [u8; 56],
    pub driver_priv: *mut core::ffi::c_void,
    _tail: [u8; 224],
}

#[repr(C)]
pub struct DrmMinor {
    _index: i32,
    _type: i32,
    _kdev: *mut core::ffi::c_void,
    pub dev: *mut core::ffi::c_void,
}

const _: [(); 72] = [(); offset_of!(DrmFile, minor)];
const _: [(); 136] = [(); offset_of!(DrmFile, driver_priv)];
const _: [(); 368] = [(); size_of::<DrmFile>()];
const _: [(); 16] = [(); offset_of!(DrmMinor, dev)];

/// Linux 7.2.3 `file_operations` record for the configured x86_64 build.
/// Unused entries preserve the source table positions; `release` is the
/// callback consumed by the singleton i915 mmap file.
#[repr(C)]
pub struct FileOperations {
    pub owner: *mut core::ffi::c_void,
    fop_flags: u32,
    _flags_pad: u32,
    _before_release: [*const core::ffi::c_void; 13],
    pub release: Option<unsafe extern "C" fn(*mut Inode, *mut File) -> i32>,
    _after_release: [*const core::ffi::c_void; 18],
}

unsafe impl Sync for FileOperations {}
const _: [(); 120] = [(); offset_of!(FileOperations, release)];
const _: [(); 272] = [(); size_of::<FileOperations>()];

/// `struct i915_lut_handle` from `gem/i915_gem_object_types.h`.
#[repr(C)]
pub struct I915LutHandle {
    pub obj_link: ListHead,
    pub ctx: *mut I915GemContext,
    pub handle: u32,
}

const _: [(); 480] = [(); size_of::<DrmGemObject>()];
const _: [(); 8] = [(); align_of::<DrmGemObject>()];
const _: [(); 8] = [(); offset_of!(DrmGemObject, dev)];
const _: [(); 24] = [(); size_of::<DrmDevice>()];
const _: [(); 16] = [(); offset_of!(DrmDevice, dev)];

const _: [(); 32] = [(); size_of::<I915LutHandle>()];
const _: [(); 8] = [(); align_of::<I915LutHandle>()];
const _: [(); 0] = [(); offset_of!(I915LutHandle, obj_link)];
const _: [(); 16] = [(); offset_of!(I915LutHandle, ctx)];
const _: [(); 24] = [(); offset_of!(I915LutHandle, handle)];
