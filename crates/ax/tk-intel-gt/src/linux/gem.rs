// SPDX-License-Identifier: MIT
// Copyright © 2026 TheKernel contributors.
// Linux v7.2.3 DRM/i915 GEM shared layout records.

use core::{
    ffi::{c_char, c_int, c_long, c_ulong, c_void},
    mem::{align_of, offset_of, size_of},
    sync::atomic::{AtomicU32, Ordering},
};

use crate::{
    i915_gem_context_types_upstream::I915GemContext,
    i915_gem_ww_upstream::WwAcquireCtx,
    intel_context_types_upstream::File,
    intel_context_upstream::{DmaFence, DrmGemObjectBaseLayout, DrmVmaOffsetNode, Kref},
    intel_engine_cs_upstream::{ListHead, Spinlock},
    linux::ww_mutex::{
        WwMutex, ww_mutex_lock, ww_mutex_lock_slow, ww_mutex_trylock, ww_mutex_unlock,
    },
    linux_i915_private::{DrmVmaOffsetManager, Inode},
};

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
#[inline]
pub unsafe fn drm_vma_node_start(node: *const DrmVmaOffsetNode) -> c_ulong {
    unsafe { (*node).vm_node.start as c_ulong }
}

/// `drm_vma_node_offset_addr()` from `include/drm/drm_vma_manager.h`.
#[inline]
pub unsafe fn drm_vma_node_offset_addr(node: *const DrmVmaOffsetNode) -> u64 {
    unsafe { ((*node).vm_node.start as u64) << crate::linux_config::PAGE_SHIFT }
}

/// `drm_vma_node_reset()` from `include/drm/drm_vma_manager.h` for the
/// configured LOCKDEP=n, PREEMPT_RT=n target. In this configuration the
/// unlocked `rwlock_t` initializer and `RB_ROOT` are all-zero representations.
/// The source precondition applies: `node` must not currently be allocated in
/// a VMA-offset manager.
#[inline]
pub unsafe fn drm_vma_node_reset(node: *mut DrmVmaOffsetNode) {
    unsafe { core::ptr::write_bytes(node, 0, 1) };
}

/// Linux `drm_vma_node_unmap()` header helper. Preserve private COW mappings
/// when invalidating the DRM object's shared mapping (`even_cows = 1`).
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
    fn __dma_resv_get_singleton(resv: *mut DmaResv, usage: i32, fence: *mut *mut DmaFence) -> i32;
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

/// Cross-crate native DRM helper: acquire a Linux reservation without a WW
/// context when publishing a native KMS fence. The helper's caller must pair
/// this with `thekernel_dma_resv_unlock` on every success/error path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thekernel_dma_resv_lock(resv: *mut c_void) -> i32 {
    unsafe { dma_resv_lock(resv, core::ptr::null_mut(), false, false) }
}

/// Try-acquire helper used by native cross-layer transactions. Multi-object
/// callers always release all acquired reservations on contention instead of
/// sleeping while holding a prefix with a conflicting i915 WW lock order.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thekernel_dma_resv_trylock(resv: *mut c_void) -> i32 {
    unsafe { dma_resv_lock(resv, core::ptr::null_mut(), false, true) }
}

/// Cross-crate pair for `thekernel_dma_resv_lock`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thekernel_dma_resv_unlock(resv: *mut c_void) {
    unsafe { dma_resv_unlock(resv) };
}

/// `struct drm_device` (tk-drm-upstream mirror); `dev` is at offset 8.
pub use tk_drm_upstream::drm_device_types::DrmDevice;

#[inline]
pub unsafe fn drm_device_device(drm: *mut core::ffi::c_void) -> *mut core::ffi::c_void {
    unsafe { (*drm.cast::<DrmDevice>()).dev }
}

/// Target x86_64 prefix of `drm_gem_object`, embedded at offset zero in TTM.
#[repr(C, align(8))]
pub struct TtmGemObjectPrefix {
    pub refcount: Kref,
    _refcount_padding: [u8; 4],
    pub dev: *mut core::ffi::c_void,
    pub filp: *mut core::ffi::c_void,
    pub vma_node: DrmVmaOffsetNode,
    pub size: u64,
    _name_and_padding: [u8; 8],
    pub dma_buf: *mut core::ffi::c_void,
    pub import_attach: *mut core::ffi::c_void,
    pub resv: *mut core::ffi::c_void,
    pub _resv: DmaResv,
    _gpuva: [u8; 40],
    pub funcs: *const core::ffi::c_void,
    _lru_node: [u8; 16],
    _lru: *mut core::ffi::c_void,
}
const _: [(); 368] = [(); size_of::<TtmGemObjectPrefix>()];

/// Linux v7.2.3 TTM buffer-object layout, with the unused tail opaque.
#[repr(C, align(8))]
pub struct TtmBufferObjectLayout {
    pub base: TtmGemObjectPrefix,
    pub bdev: *mut core::ffi::c_void,
    _type: u32,
    _page_alignment: u32,
    _destroy: *const core::ffi::c_void,
    _kref: Kref,
    _kref_padding: u32,
    pub resource: *mut TtmResource,
    _tail: [u8; 72],
}
const _: [(); 480] = [(); size_of::<TtmBufferObjectLayout>()];
const _: [(); 8] = [(); align_of::<TtmBufferObjectLayout>()];
const _: [(); 368] = [(); offset_of!(TtmBufferObjectLayout, bdev)];
const _: [(); 400] = [(); offset_of!(TtmBufferObjectLayout, resource)];

/// TTM resource prefix used by the i915 mappable-memory predicate.
#[repr(C, align(8))]
pub struct TtmResource {
    _start: usize,
    pub size: usize,
    pub mem_type: u32,
    _tail: [u8; 68],
}
const _: [(); 88] = [(); size_of::<TtmResource>()];

#[inline]
pub unsafe fn i915_ttm_resource_visible_size(res: *mut TtmResource) -> usize {
    // i915_ttm_buddy_resource::used_visible_size follows the 88-byte base,
    // list_head, and flags. Its offset is bound from Linux's target layout.
    unsafe { res.cast::<u8>().add(112).cast::<usize>().read() }
}

/// `ttm_device::dev_mapping` from the configured Linux 7.2.3 x86_64 layout.
/// The enclosing TTM device has many opaque subrecords; this is the only
/// member used by i915's mmap-offset invalidation path.
#[inline]
pub unsafe fn ttm_device_dev_mapping(bdev: *mut core::ffi::c_void) -> *mut core::ffi::c_void {
    const DEV_MAPPING_OFFSET: usize = 1656;
    unsafe {
        bdev.cast::<u8>()
            .add(DEV_MAPPING_OFFSET)
            .cast::<*mut core::ffi::c_void>()
            .read()
    }
}

/// `i915_gem_to_ttm()` from `gem/i915_gem_ttm.h`: the TTM BO is the
/// alternate arm of the GEM object's base union, at the same address.
#[inline]
pub unsafe fn i915_gem_to_ttm(
    obj: *mut crate::i915_gem_object_types_upstream::DrmI915GemObject,
) -> *mut TtmBufferObjectLayout {
    unsafe { core::ptr::addr_of_mut!((*obj).base.__do_not_access).cast() }
}

/// Linux 7.2.3 `struct drm_file` prefix through `driver_priv`.
///
/// Keep this canonical view shared by i915 paths that inspect DRM object
/// handles, per-file driver data, or the backing `struct file`.
#[repr(C)]
pub struct DrmFile {
    _before_minor: [u8; 72],
    pub minor: *mut DrmMinor,
    pub object_idr: [u8; 24],
    pub table_lock: Spinlock,
    _syncobj_xa_alignment: u32,
    pub(crate) _syncobj_xa: [u64; 2],
    pub filp: *mut crate::linux::shmem::File,
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

/// Serialize access to the canonical Linux `drm_file.object_idr` while a
/// caller updates the GEM handle table.
///
/// # Safety
/// `file` must be a live, initialized `DrmFile` held for the lock lifetime.
pub unsafe fn drm_file_table_lock(file: *mut DrmFile) {
    unsafe { crate::linux::locks::spin_lock(&mut (*file).table_lock) };
}

/// Unlock the canonical Linux `drm_file.object_idr` table lock.
///
/// # Safety
/// `file` must be live and its `table_lock` must be held by the current CPU.
pub unsafe fn drm_file_table_unlock(file: *mut DrmFile) {
    unsafe { crate::linux::locks::spin_unlock(&mut (*file).table_lock) };
}

/// Add the object reference and per-object handle count for one IDR handle.
/// All caller-side IDR modifications must use `drm_file_table_lock` and the
/// backend's IDR serialization gate.
///
/// # Safety
/// `object` must be a live DRM GEM object with an owned reference.
pub unsafe fn drm_gem_handle_get(object: *mut DrmGemObject) -> Result<(), i32> {
    if object.is_null() {
        return Err(-22);
    }
    let count =
        unsafe { AtomicU32::from_ptr(object.cast::<u8>().add(size_of::<Kref>()).cast::<u32>()) };
    if count
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        })
        .is_err()
    {
        return Err(-24);
    }
    unsafe { crate::linux_memory::kref_get(&mut (*object).refcount) };
    Ok(())
}

/// Drop the object reference and handle count previously acquired by
/// `drm_gem_handle_get`.
///
/// # Safety
/// `object` must have a live GEM reference corresponding to one handle.
pub unsafe fn drm_gem_handle_put(object: *mut DrmGemObject) {
    unsafe extern "C" fn release(refcount: *mut Kref) {
        unsafe { crate::linux::drm_core::drm_gem_object_free(refcount.cast()) };
    }
    let count =
        unsafe { AtomicU32::from_ptr(object.cast::<u8>().add(size_of::<Kref>()).cast::<u32>()) };
    count
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_sub(1)
        })
        .expect("GEM handle count underflow");
    unsafe { crate::linux_memory::kref_put(&mut (*object).refcount, release) };
}

#[repr(C)]
struct DrmGemObjectFuncsPrefix {
    free: *const c_void,
    open: *const c_void,
    close: *const c_void,
}

/// Run the optional object-specific DRM open hook after publishing a handle.
///
/// # Safety
/// `object` and `file` must be live Linux DRM objects; the caller must already
/// have installed the handle and retained its object reference.
pub unsafe fn drm_gem_object_open(object: *mut DrmGemObject, file: *mut DrmFile) -> c_int {
    let funcs = unsafe { (*object).funcs.cast::<DrmGemObjectFuncsPrefix>() };
    if funcs.is_null() || unsafe { (*funcs).open.is_null() } {
        return 0;
    }
    let open: unsafe extern "C" fn(*mut DrmGemObject, *mut DrmFile) -> c_int =
        unsafe { core::mem::transmute((*funcs).open) };
    unsafe { open(object, file) }
}

/// Run the optional object-specific DRM close hook before dropping a handle.
///
/// # Safety
/// `object` and `file` must be live Linux DRM objects; the caller must already
/// have removed this handle from the file's IDR while retaining its object ref.
pub unsafe fn drm_gem_object_close(object: *mut DrmGemObject, file: *mut DrmFile) {
    let funcs = unsafe { (*object).funcs.cast::<DrmGemObjectFuncsPrefix>() };
    if funcs.is_null() || unsafe { (*funcs).close.is_null() } {
        return;
    }
    let close: unsafe extern "C" fn(*mut DrmGemObject, *mut DrmFile) =
        unsafe { core::mem::transmute((*funcs).close) };
    unsafe { close(object, file) };
}


const _: [(); 72] = [(); offset_of!(DrmFile, minor)];
const _: [(); 80] = [(); offset_of!(DrmFile, object_idr)];
const _: [(); 104] = [(); offset_of!(DrmFile, table_lock)];
const _: [(); 112] = [(); offset_of!(DrmFile, _syncobj_xa)];
const _: [(); 128] = [(); offset_of!(DrmFile, filp)];
const _: [(); 136] = [(); offset_of!(DrmFile, driver_priv)];
const _: [(); 368] = [(); size_of::<DrmFile>()];
const _: [(); 16] = [(); offset_of!(DrmMinor, dev)];

/// Linux 7.2.3 `file_operations` record for the configured x86_64 build.
/// Unused entries preserve the source table positions; `release` is the
/// callback consumed by the singleton i915 mmap file.
#[repr(C)]
pub struct FileOperations {
    pub owner: *mut core::ffi::c_void,
    pub(crate) fop_flags: u32,
    pub(crate) _flags_pad: u32,
    pub(crate) _before_release: [*const core::ffi::c_void; 13],
    pub release: Option<unsafe extern "C" fn(*mut Inode, *mut File) -> i32>,
    pub(crate) _after_release: [*const core::ffi::c_void; 18],
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
const _: [(); 1560] = [(); size_of::<DrmDevice>()];
const _: [(); 8] = [(); offset_of!(DrmDevice, dev)];

const _: [(); 32] = [(); size_of::<I915LutHandle>()];
const _: [(); 8] = [(); align_of::<I915LutHandle>()];
const _: [(); 0] = [(); offset_of!(I915LutHandle, obj_link)];
const _: [(); 16] = [(); offset_of!(I915LutHandle, ctx)];
const _: [(); 24] = [(); offset_of!(I915LutHandle, handle)];

// upstream: drm_vma_manager.h drm_vma_offset_lock_lookup()
pub unsafe fn drm_vma_offset_lock_lookup(manager: *mut DrmVmaOffsetManager) {
    unsafe { crate::linux::locks::read_lock(manager.cast()) };
}
// upstream: drm_vma_manager.h drm_vma_offset_unlock_lookup()
pub unsafe fn drm_vma_offset_unlock_lookup(manager: *mut DrmVmaOffsetManager) {
    unsafe { crate::linux::locks::read_unlock(manager.cast()) };
}
// Both Linux 7.2.3 oracles build with CONFIG_TRANSPARENT_HUGEPAGE=y, so
// drm_device carries huge_mnt (tk_drm_upstream::drm_device_types). i915 never
// creates a huge shmem mount here, so the mount stays NULL as in a Linux build
// that has not called drm_gem_huge_mnt_create().
// upstream: drm_gem.h drm_gem_get_huge_mnt()
pub fn drm_gem_get_huge_mnt<T, U>(_dev: *mut T) -> *mut U {
    core::ptr::null_mut()
}
// upstream: drm_gem.h drm_gem_huge_mnt_create()
pub fn drm_gem_huge_mnt_create<T>(_dev: *mut T, _value: *const c_char) -> c_int {
    0
}
