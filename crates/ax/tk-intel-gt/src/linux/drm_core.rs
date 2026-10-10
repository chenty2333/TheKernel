// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! LinuxKPI entry points for DRM core objects (GEM handles, PRIME/dma-buf,
//! sync_file, syncobj, DRM device references) whose objects and tables are
//! owned by the kernel's DRM implementation.
//!
//! Every symbol forwards to one installed `DrmCoreProvider`, so the object
//! identity seen by i915 is the kernel's. With no provider installed the calls
//! fail closed: pointer results are NULL or `ERR_PTR(-ENODEV)`, integer results
//! are `-ENODEV`, `drm_dev_enter()` reports the device as unplugged, and calls
//! with no result panic. Installation and the provider's callbacks are listed
//! in `PROVIDERS.md`.

#![allow(unsafe_code)]

use core::{
ffi::{c_char, c_int, c_void},
sync::atomic::{AtomicPtr, Ordering},
};

use crate::linux_config::{ENODEV, EINVAL};

/// Kernel-side DRM core operations. Pointers are the kernel's own objects.
#[repr(C)]
pub struct DrmCoreProvider {
pub dev_enter: unsafe extern "C" fn(dev: *mut c_void, idx: *mut c_int) -> bool,
pub dev_exit: unsafe extern "C" fn(idx: c_int),
pub dev_get: unsafe extern "C" fn(dev: *mut c_void) -> *mut c_void,
pub dev_put: unsafe extern "C" fn(dev: *mut c_void),
pub is_current_master: unsafe extern "C" fn(file: *mut c_void) -> bool,
pub print_memory_stats:
    unsafe extern "C" fn(printer: *mut c_void, stats: *const c_void, supported: c_int, region: *const c_char),
pub gem_free_mmap_offset: unsafe extern "C" fn(obj: *mut c_void),
pub gem_handle_create: unsafe extern "C" fn(file: *mut c_void, obj: *mut c_void, handle: *mut u32) -> c_int,
pub gem_object_free: unsafe extern "C" fn(refcount: *mut c_void),
pub gem_private_object_init: unsafe extern "C" fn(dev: *mut c_void, obj: *mut c_void, size: usize),
pub gem_dmabuf_export: unsafe extern "C" fn(dev: *mut c_void, info: *mut c_void) -> *mut c_void,
pub gem_dmabuf_release: unsafe extern "C" fn(dmabuf: *mut c_void),
pub gem_prime_mmap: unsafe extern "C" fn(obj: *mut c_void, vma: *mut c_void) -> c_int,
pub gem_unmap_dma_buf: unsafe extern "C" fn(attach: *mut c_void, sgt: *mut c_void, direction: c_int),
pub prime_gem_destroy: unsafe extern "C" fn(obj: *mut c_void, sg: *mut c_void),
pub dma_buf_attach: unsafe extern "C" fn(dmabuf: *mut c_void, dev: *mut c_void) -> *mut c_void,
pub dma_buf_detach: unsafe extern "C" fn(dmabuf: *mut c_void, attach: *mut c_void),
pub dma_buf_map_attachment: unsafe extern "C" fn(attach: *mut c_void, direction: c_int) -> *mut c_void,
pub dma_buf_unmap_attachment: unsafe extern "C" fn(attach: *mut c_void, sgt: *mut c_void, direction: c_int),
pub dma_buf_put: unsafe extern "C" fn(dmabuf: *mut c_void),
pub sync_file_create: unsafe extern "C" fn(fence: *mut c_void) -> *mut c_void,
pub sync_file_get_fence: unsafe extern "C" fn(fd: c_int) -> *mut c_void,
pub syncobj_find: unsafe extern "C" fn(file: *mut c_void, handle: u32) -> *mut c_void,
pub syncobj_create: unsafe extern "C" fn(out: *mut *mut c_void, flags: u32, fence: *mut c_void) -> c_int,
pub syncobj_add_point: unsafe extern "C" fn(syncobj: *mut c_void, chain: *mut c_void, fence: *mut c_void, point: u64),
pub syncobj_replace_fence: unsafe extern "C" fn(syncobj: *mut c_void, fence: *mut c_void),
pub syncobj_fence_get: unsafe extern "C" fn(syncobj: *mut c_void) -> *mut c_void,
pub syncobj_put: unsafe extern "C" fn(syncobj: *mut c_void),
}

static DRM_CORE_PROVIDER: AtomicPtr<DrmCoreProvider> = AtomicPtr::new(core::ptr::null_mut());

/// Install the kernel's DRM core provider. It can be installed once.
pub fn install_drm_core_provider(provider: &'static DrmCoreProvider) -> Result<(), &'static str> {
DRM_CORE_PROVIDER
    .compare_exchange(
        core::ptr::null_mut(),
        core::ptr::from_ref(provider).cast_mut(),
        Ordering::AcqRel,
        Ordering::Acquire,
    )
    .map(|_| ())
    .map_err(|_| "DRM core provider already installed")
}

fn provider() -> Option<&'static DrmCoreProvider> {
let provider = DRM_CORE_PROVIDER.load(Ordering::Acquire);
// SAFETY: only `install_drm_core_provider` stores a non-null `'static` reference.
unsafe { provider.as_ref() }
}

/// The provider, or a panic naming the operation that has no result to fail with.
fn required(operation: &str) -> &'static DrmCoreProvider {
provider().unwrap_or_else(|| panic!("{operation}: DRM core provider not installed"))
}

#[inline]
fn err_ptr(errno: c_int) -> *mut c_void {
(errno as isize) as *mut c_void
}

/// Linux `drm_dev_enter()`: false when no provider can admit the device.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_enter(dev: *mut c_void, idx: *mut c_int) -> bool {
match provider() {
    Some(p) => unsafe { (p.dev_enter)(dev, idx) },
    None => false,
}
}

/// Linux `drm_dev_exit()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_exit(idx: c_int) {
unsafe { (required("drm_dev_exit").dev_exit)(idx) };
}

/// Linux `drm_dev_get()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_get(dev: *mut c_void) -> *mut c_void {
match provider() {
    Some(p) => unsafe { (p.dev_get)(dev) },
    None => core::ptr::null_mut(),
}
}

/// Linux `drm_dev_put()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dev_put(dev: *mut c_void) {
unsafe { (required("drm_dev_put").dev_put)(dev) };
}

/// Linux `drm_is_current_master()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_is_current_master(file: *mut c_void) -> bool {
match provider() {
    Some(p) => unsafe { (p.is_current_master)(file) },
    None => false,
}
}

/// Linux `drm_print_memory_stats()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_print_memory_stats(
printer: *mut c_void,
stats: *const c_void,
supported_status: c_int,
region: *const c_char,
) {
unsafe { (required("drm_print_memory_stats").print_memory_stats)(printer, stats, supported_status, region) };
}

/// Linux `drm_gem_free_mmap_offset()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_free_mmap_offset(obj: *mut c_void) {
unsafe { (required("drm_gem_free_mmap_offset").gem_free_mmap_offset)(obj) };
}

/// Linux `drm_gem_handle_create()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_handle_create(file: *mut c_void, obj: *mut c_void, handle: *mut u32) -> c_int {
match provider() {
    Some(p) => unsafe { (p.gem_handle_create)(file, obj, handle) },
    None => -ENODEV,
}
}

/// Linux `drm_gem_object_free()`: final-reference release of a GEM object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_object_free(refcount: *mut c_void) {
unsafe { (required("drm_gem_object_free").gem_object_free)(refcount) };
}

/// Linux `drm_gem_private_object_init()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_private_object_init(dev: *mut c_void, obj: *mut c_void, size: usize) {
unsafe { (required("drm_gem_private_object_init").gem_private_object_init)(dev, obj, size) };
}

/// Linux `drm_gem_dmabuf_export()`: an error pointer when no provider exists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_dmabuf_export(dev: *mut c_void, info: *mut c_void) -> *mut c_void {
match provider() {
    Some(p) => unsafe { (p.gem_dmabuf_export)(dev, info) },
    None => err_ptr(-ENODEV),
}
}

/// Linux `drm_gem_dmabuf_release()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_dmabuf_release(dmabuf: *mut c_void) {
unsafe { (required("drm_gem_dmabuf_release").gem_dmabuf_release)(dmabuf) };
}

/// Linux `drm_gem_prime_mmap()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_prime_mmap(obj: *mut c_void, vma: *mut c_void) -> c_int {
match provider() {
    Some(p) => unsafe { (p.gem_prime_mmap)(obj, vma) },
    None => -ENODEV,
}
}

/// Linux `drm_gem_unmap_dma_buf()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_unmap_dma_buf(attach: *mut c_void, sgt: *mut c_void, direction: c_int) {
unsafe { (required("drm_gem_unmap_dma_buf").gem_unmap_dma_buf)(attach, sgt, direction) };
}

/// Linux `drm_prime_gem_destroy()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_prime_gem_destroy(obj: *mut c_void, sg: *mut c_void) {
unsafe { (required("drm_prime_gem_destroy").prime_gem_destroy)(obj, sg) };
}

/// Linux `dma_buf_attach()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_buf_attach(dmabuf: *mut c_void, dev: *mut c_void) -> *mut c_void {
match provider() {
    Some(p) => unsafe { (p.dma_buf_attach)(dmabuf, dev) },
    None => err_ptr(-ENODEV),
}
}

/// Linux `dma_buf_detach()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_buf_detach(dmabuf: *mut c_void, attach: *mut c_void) {
unsafe { (required("dma_buf_detach").dma_buf_detach)(dmabuf, attach) };
}

/// Linux `dma_buf_map_attachment()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_buf_map_attachment(attach: *mut c_void, direction: c_int) -> *mut c_void {
match provider() {
    Some(p) => unsafe { (p.dma_buf_map_attachment)(attach, direction) },
    None => err_ptr(-ENODEV),
}
}

/// Linux `dma_buf_unmap_attachment()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_buf_unmap_attachment(attach: *mut c_void, sgt: *mut c_void, direction: c_int) {
unsafe { (required("dma_buf_unmap_attachment").dma_buf_unmap_attachment)(attach, sgt, direction) };
}

/// Linux `dma_buf_put()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_buf_put(dmabuf: *mut c_void) {
unsafe { (required("dma_buf_put").dma_buf_put)(dmabuf) };
}

/// Linux `sync_file_create()`: NULL when no provider exists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sync_file_create(fence: *mut c_void) -> *mut c_void {
match provider() {
    Some(p) => unsafe { (p.sync_file_create)(fence) },
    None => core::ptr::null_mut(),
}
}

/// Linux `sync_file_get_fence()`: NULL when no provider exists or `fd` is not a sync file.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sync_file_get_fence(fd: c_int) -> *mut c_void {
match provider() {
    Some(p) => unsafe { (p.sync_file_get_fence)(fd) },
    None => core::ptr::null_mut(),
}
}

/// Linux `drm_syncobj_find()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_find(file: *mut c_void, handle: u32) -> *mut c_void {
match provider() {
    Some(p) => unsafe { (p.syncobj_find)(file, handle) },
    None => core::ptr::null_mut(),
}
}

/// Linux `drm_syncobj_create()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_create(out: *mut *mut c_void, flags: u32, fence: *mut c_void) -> c_int {
match provider() {
    Some(p) => unsafe { (p.syncobj_create)(out, flags, fence) },
    None => -ENODEV,
}
}

/// Linux `drm_syncobj_add_point()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_add_point(syncobj: *mut c_void, chain: *mut c_void, fence: *mut c_void, point: u64) {
unsafe { (required("drm_syncobj_add_point").syncobj_add_point)(syncobj, chain, fence, point) };
}

/// Linux `drm_syncobj_replace_fence()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_replace_fence(syncobj: *mut c_void, fence: *mut c_void) {
unsafe { (required("drm_syncobj_replace_fence").syncobj_replace_fence)(syncobj, fence) };
}

/// Linux `drm_syncobj_fence_get()`: NULL when no provider exists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_fence_get(syncobj: *mut c_void) -> *mut c_void {
match provider() {
    Some(p) => unsafe { (p.syncobj_fence_get)(syncobj) },
    None => core::ptr::null_mut(),
}
}

/// Linux `drm_syncobj_put()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_put(syncobj: *mut c_void) {
unsafe { (required("drm_syncobj_put").syncobj_put)(syncobj) };
}

/// Linux `shmem_read_mapping_page_gfp()` (mm/shmem.c) C entry point; the Rust
/// implementation in `shmem.rs` keeps its name.
#[unsafe(export_name = "shmem_read_mapping_page_gfp")]
pub unsafe extern "C" fn c_shmem_read_mapping_page_gfp(
mapping: *mut crate::linux::shmem::AddressSpace,
index: core::ffi::c_ulong,
gfp: u32,
) -> *mut crate::i915_gem_object_types_upstream::Page {
if mapping.is_null() {
    return err_ptr(-EINVAL).cast();
}
unsafe { crate::linux::shmem::shmem_read_mapping_page_gfp(mapping, index, gfp) }
}
