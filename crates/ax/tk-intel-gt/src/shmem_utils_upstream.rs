// SPDX-License-Identifier: MIT
// Copyright © 2020 Intel Corporation.
// Source-order translation of Linux v7.2.3 drivers/gpu/drm/i915/gt/shmem_utils.c.

#![allow(dead_code, non_camel_case_types, non_snake_case, unsafe_code)]

use core::{
    ffi::{c_char, c_int, c_void},
    ptr,
};

use crate::{
    i915_gem_lmem_upstream::i915_gem_object_is_lmem,
    i915_gem_object_api_upstream::i915_gem_object_unpin_map,
    i915_gem_object_types_upstream::{
        DrmI915GemObject, I915_MAP_WB, I915_MAP_WC, Page, intel_bo_to_drm_bo,
    },
    i915_gem_pages_upstream::i915_gem_object_pin_map_unlocked,
    i915_gem_shmem_upstream::{i915_gem_object_is_shmem, shmem_read as shmem_read_impl},
    linux::{
        config::{EOVERFLOW, ERR_PTR, GFP_KERNEL, IS_ERR, PAGE_SHIFT, PAGE_SIZE, PTR_ERR},
        highmem::{kmap_local_page, kunmap_local, mark_page_accessed, put_page},
        iosys_map::IosysMap,
        memory::{kfree, kvmalloc_objs},
        primitives::offset_in_page,
        shmem::{
            AddressSpace, File, file_size, folio_mark_dirty, fput_file, get_file_file,
            mapping_clear_unevictable, mapping_set_unevictable, page_folio, shmem_file_setup,
            shmem_read_mapping_page_gfp,
        },
        vm::{PAGE_KERNEL, VM_MAP_PUT_PAGES, vfree, vmap},
    },
};

const VMA_NORESERVE_BIT: u32 = 21;
const EOVERFLOW_LOCAL: c_int = EOVERFLOW;

#[inline]
unsafe fn file_mapping(file: *mut File) -> *mut AddressSpace {
    unsafe { (*file).f_mapping }
}

#[inline]
unsafe fn object_file(obj: *mut DrmI915GemObject) -> *mut File {
    let base = unsafe { intel_bo_to_drm_bo(obj) };
    unsafe { (*base).filp.cast::<File>() }
}

#[inline]
unsafe fn object_size(obj: *mut DrmI915GemObject) -> usize {
    let base = unsafe { intel_bo_to_drm_bo(obj) };
    unsafe { (*base).size as usize }
}

#[inline]
fn page_aligned_size(size: usize) -> Option<u64> {
    size.checked_add(PAGE_SIZE - 1)
        .map(|aligned| (aligned & !(PAGE_SIZE - 1)) as u64)
}

// upstream: gt/shmem_utils.c shmem_create_from_data()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shmem_create_from_data(
    name: *const c_char,
    data: *mut c_void,
    len: usize,
) -> *mut File {
    let Some(size) = page_aligned_size(len) else {
        return ERR_PTR(-EOVERFLOW_LOCAL);
    };
    let file = unsafe {
        shmem_file_setup(
            name,
            size,
            1usize.checked_shl(VMA_NORESERVE_BIT).unwrap_or(0) as _,
        )
    };
    if IS_ERR(file) {
        return file;
    }
    let err = unsafe { shmem_write(file, 0, data, len) };
    if err != 0 {
        unsafe { fput_file(file) };
        return ERR_PTR(err);
    }
    file
}

// upstream: gt/shmem_utils.c shmem_create_from_object()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shmem_create_from_object(obj: *mut DrmI915GemObject) -> *mut File {
    if unsafe { i915_gem_object_is_shmem(obj) } {
        return unsafe { get_file_file(object_file(obj)) };
    }
    let map_type = if unsafe { i915_gem_object_is_lmem(obj) } {
        I915_MAP_WC
    } else {
        I915_MAP_WB
    };
    let ptr = unsafe { i915_gem_object_pin_map_unlocked(obj, map_type) };
    if IS_ERR(ptr) {
        return ptr.cast::<File>();
    }
    let file = unsafe { shmem_create_from_data(b"\0".as_ptr().cast(), ptr, object_size(obj)) };
    unsafe { i915_gem_object_unpin_map(obj) };
    file
}

// upstream: gt/shmem_utils.c shmem_pin_map()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shmem_pin_map(file: *mut File) -> *mut c_void {
    let mapping = unsafe { file_mapping(file) };
    let page_count = (unsafe { file_size(file) } >> PAGE_SHIFT) as usize;
    if page_count == 0 || page_count > u32::MAX as usize {
        return ptr::null_mut();
    }
    let pages = kvmalloc_objs::<*mut Page, usize>(page_count);
    if pages.is_null() {
        return ptr::null_mut();
    }

    let mut allocated = 0usize;
    while allocated < page_count {
        let page = unsafe { shmem_read_mapping_page_gfp(mapping, allocated as _, GFP_KERNEL) };
        if page.is_null() || IS_ERR(page) {
            for index in 0..allocated {
                unsafe { put_page(pages.add(index).read()) };
            }
            unsafe { kfree(pages) };
            return ptr::null_mut();
        }
        unsafe { pages.add(allocated).write(page) };
        allocated += 1;
    }

    let vaddr = unsafe { vmap(pages, page_count as u32, VM_MAP_PUT_PAGES, PAGE_KERNEL) };
    if vaddr.is_null() {
        for index in 0..page_count {
            unsafe { put_page(pages.add(index).read()) };
        }
        unsafe { kfree(pages) };
        return ptr::null_mut();
    }

    unsafe { mapping_set_unevictable(mapping) };
    vaddr
}

// upstream: gt/shmem_utils.c shmem_unpin_map()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shmem_unpin_map(file: *mut File, ptr_: *mut c_void) {
    unsafe { mapping_clear_unevictable(file_mapping(file)) };
    unsafe { vfree(ptr_) };
}

#[inline]
unsafe fn memcpy_to_iosys_map(map: *mut IosysMap, map_off: usize, src: *const u8, len: usize) {
    if unsafe { (*map).is_iomem } {
        let dst = unsafe { (*map).addr.vaddr_iomem.cast::<u8>().add(map_off) };
        for offset in 0..len {
            let byte = unsafe { src.add(offset).read() };
            unsafe { dst.add(offset).write_volatile(byte) };
        }
    } else {
        let dst = unsafe { (*map).addr.vaddr.cast::<u8>().add(map_off) };
        unsafe { ptr::copy_nonoverlapping(src, dst, len) };
    }
}

// upstream: gt/shmem_utils.c __shmem_rw()
unsafe fn __shmem_rw(
    file: *mut File,
    mut off: i64,
    mut buffer: *mut c_void,
    mut len: usize,
    write: bool,
) -> c_int {
    if !write {
        return unsafe { shmem_read_impl(file.cast(), off as u64, buffer, len) };
    }
    let mut pfn = (off as u64) >> PAGE_SHIFT;
    while len != 0 {
        let page_off = offset_in_page(off as u64) as usize;
        let this = core::cmp::min(PAGE_SIZE - page_off, len);
        let mapping = unsafe { file_mapping(file) };
        let page = unsafe { shmem_read_mapping_page_gfp(mapping, pfn as _, GFP_KERNEL) };
        if IS_ERR(page) {
            return PTR_ERR(page);
        }
        let vaddr = unsafe { kmap_local_page(page) };
        unsafe {
            ptr::copy_nonoverlapping(buffer.cast::<u8>(), vaddr.cast::<u8>().add(page_off), this);
            folio_mark_dirty(page_folio(page));
            mark_page_accessed(page);
            kunmap_local(vaddr);
            put_page(page);
        }
        len -= this;
        buffer = unsafe { buffer.cast::<u8>().add(this).cast() };
        off = 0;
        pfn += 1;
    }
    0
}

// upstream: gt/shmem_utils.c shmem_read_to_iosys_map()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shmem_read_to_iosys_map(
    file: *mut File,
    mut off: i64,
    map: *mut IosysMap,
    mut map_off: usize,
    mut len: usize,
) -> c_int {
    let mut pfn = (off as u64) >> PAGE_SHIFT;
    while len != 0 {
        let page_off = offset_in_page(off as u64) as usize;
        let this = core::cmp::min(PAGE_SIZE - page_off, len);
        let mapping = unsafe { file_mapping(file) };
        let page = unsafe { shmem_read_mapping_page_gfp(mapping, pfn as _, GFP_KERNEL) };
        if IS_ERR(page) {
            return PTR_ERR(page);
        }
        let vaddr = unsafe { kmap_local_page(page) };
        unsafe {
            memcpy_to_iosys_map(map, map_off, vaddr.cast::<u8>().add(page_off), this);
            mark_page_accessed(page);
            kunmap_local(vaddr);
            put_page(page);
        }
        len -= this;
        map_off += this;
        off = 0;
        pfn += 1;
    }
    0
}

// upstream: gt/shmem_utils.c shmem_read()
/// Adapter to the shared native shmem page reader.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shmem_read(
    file: *mut File,
    off: i64,
    dst: *mut c_void,
    len: usize,
) -> c_int {
    unsafe { shmem_read_impl(file.cast(), off as u64, dst, len) }
}

// upstream: gt/shmem_utils.c shmem_write()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shmem_write(
    file: *mut File,
    off: i64,
    src: *mut c_void,
    len: usize,
) -> c_int {
    unsafe { __shmem_rw(file, off, src, len, true) }
}
