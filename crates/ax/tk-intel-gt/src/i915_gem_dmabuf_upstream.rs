// SPDX-License-Identifier: MIT
// Copyright 2012 Red Hat Inc.
//
//! Linux v7.2.3 `drivers/gpu/drm/i915/gem/i915_gem_dmabuf.c` translation.
//! DMA-BUF objects, file references, and attachment lifecycle are owned by the
//! shared `tk-drm-upstream` core. This driver supplies only its exporter
//! callbacks; DMA mappings remain requester-scoped LinuxKPI/VT-d operations.
//! The source's trailing selftest includes are absent for CONFIG_DRM_I915_SELFTEST=n.

#![allow(
    unsafe_code,
    unsafe_op_in_unsafe_fn,
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals
)]

use core::ffi::{c_char, c_int, c_ulong, c_void};

use tk_drm_upstream::{
    drm_prime_upstream::{
        DmaBuf, DmaBufAttachment, DmaBufExportInfo, DmaBufOps, drm_gem_dmabuf_export,
        drm_gem_dmabuf_release, drm_gem_prime_mmap, drm_gem_unmap_dma_buf,
    },
    iosys_map_types::{IosysMap, iosys_map_set_vaddr},
};

use crate::{
    i915_gem_domain_upstream::{
        i915_gem_object_set_to_cpu_domain, i915_gem_object_set_to_gtt_domain,
    },
    i915_gem_object_api_upstream::{
        i915_gem_object_get, i915_gem_object_lock, i915_gem_object_pin_pages, i915_gem_object_put,
        i915_gem_object_unpin_map, i915_gem_object_unpin_pages,
    },
    i915_gem_object_header_upstream::{
        assert_object_held, i915_gem_object_flush_map, i915_gem_object_size_2big,
    },
    i915_gem_object_types_upstream::{
        DrmI915GemObject, DrmI915GemObjectOps, I915_BO_ALLOC_USER, I915_MAP_WB, intel_bo_to_drm_bo,
    },
    i915_gem_object_upstream::{
        i915_gem_object_alloc, i915_gem_object_can_bypass_llc, i915_gem_object_can_migrate,
        i915_gem_object_init, i915_gem_object_migrate,
    },
    i915_gem_pages_upstream::{
        __i915_gem_object_set_pages, Scatterlist, i915_gem_object_pin_map, sg_alloc_table, sg_next,
        sg_page,
    },
    i915_gem_userptr_upstream::{drm_gem_private_object_init, sg_free_table},
    i915_gem_ww_upstream::{
        I915GemWwCtx, i915_gem_ww_ctx_backoff, i915_gem_ww_ctx_fini, i915_gem_ww_ctx_init,
    },
    intel_context_types_upstream::File,
    intel_context_upstream::SgTable,
    intel_engine_cs_upstream::{ListHead, Spinlock},
    linux::{
        fields::LockClassKey,
        gem::{DmaResv, DrmDevice, DrmGemObject, get_file_active},
        gem_memory::INTEL_REGION_SMEM,
        i915::{HAS_LLC, HAS_LMEM, IS_DG1, to_i915},
        memory::{kfree, kmalloc_obj},
        mm::VmAreaStruct,
    },
    linux_config::{
        E2BIG, EDEADLK, EINVAL, ENODEV, ENOMEM, EOPNOTSUPP, ERR_PTR, GFP_KERNEL, IS_ERR, PTR_ERR,
    },
};

const DMA_BIDIRECTIONAL: c_int = 0;
const DMA_TO_DEVICE: c_int = 1;
const DMA_ATTR_SKIP_CPU_SYNC: c_ulong = 1 << 5;
const I915_GEM_DOMAIN_GTT: u32 = 0x0000_0040;
const SG_PAGE_LINK_MASK: usize = 3;
const KBUILD_MODNAME_I915: &[u8] = b"i915\0";

/// Linux `struct file` field used to locate its operations vector.
#[repr(C)]
struct FileFopsPrefix {
    _before_f_op: [u8; 8],
    f_op: *const FileOperationsMmapView,
}
const _: [(); 8] = [(); core::mem::offset_of!(FileFopsPrefix, f_op)];

/// `file_operations` prefix / mmap tail for Linux v7.2.3 with CONFIG_MMU=y.
/// The unrelated file-operation slots preserve the source offsets exactly.
#[repr(C)]
struct FileOperationsMmapView {
    owner: *mut c_void,
    fop_flags: u32,
    _flags_pad: u32,
    _llseek: *const c_void,
    _read: *const c_void,
    _write: *const c_void,
    _read_iter: *const c_void,
    _write_iter: *const c_void,
    _iopoll: *const c_void,
    _iterate_shared: *const c_void,
    _poll: *const c_void,
    _unlocked_ioctl: *const c_void,
    _compat_ioctl: *const c_void,
    mmap: Option<unsafe extern "C" fn(*mut File, *mut VmAreaStruct) -> c_int>,
    _after_mmap: [*const c_void; 20],
    mmap_prepare: *const c_void,
}
const _: [(); 96] = [(); core::mem::offset_of!(FileOperationsMmapView, mmap)];
const _: [(); 264] = [(); core::mem::offset_of!(FileOperationsMmapView, mmap_prepare)];

unsafe extern "C" {
    fn dma_map_sgtable(
        dev: *mut c_void,
        sgt: *mut SgTable,
        direction: c_int,
        attrs: c_ulong,
    ) -> c_int;
    fn dma_buf_attach(dmabuf: *mut DmaBuf, dev: *mut c_void) -> *mut DmaBufAttachment;
    fn dma_buf_detach(dmabuf: *mut DmaBuf, attach: *mut DmaBufAttachment);
    fn dma_buf_map_attachment(
        attach: *mut DmaBufAttachment,
        direction: c_int,
    ) -> *mut SgTable;
    fn dma_buf_unmap_attachment(
        attach: *mut DmaBufAttachment,
        sgt: *mut SgTable,
        direction: c_int,
    );
    fn dma_buf_put(dmabuf: *mut DmaBuf);
    fn compat_vma_mmap(file: *mut File, vma: *mut VmAreaStruct) -> c_int;
    fn i915_gem_object_wait_migration(obj: *mut DrmI915GemObject, flags: u32) -> c_int;
    fn wbinvd_on_all_cpus();
}

#[cfg(CONFIG_DRM_I915_SELFTEST)]
static mut force_different_devices: bool = false;

// The source's to_intel_bo() conversion: the exported priv points at the
// drm_gem_object base, which is at offset zero in drm_i915_gem_object.
unsafe fn dma_buf_to_obj(buf: *mut DmaBuf) -> *mut DrmI915GemObject {
    unsafe { (*buf).r#priv.cast() }
}

// Linux `get_dma_buf()` is a header inline around `get_file()`. The DMA-BUF
// itself owns a live file reference, so `get_file_active()` acquires the same
// reference and makes an invariant violation fail closed.
unsafe fn get_dma_buf(buf: *mut DmaBuf) {
    let file = unsafe { core::ptr::addr_of_mut!((*buf).file) };
    GEM_BUG_ON!(unsafe { get_file_active(file) }.is_null());
}

// Linux `vfs_mmap()` is a header inline in v7.2.3. Preserve its selection of
// compat_vma_mmap when mmap_prepare is installed; otherwise call mmap itself.
unsafe fn vfs_mmap(file: *mut File, vma: *mut VmAreaStruct) -> c_int {
    let file_ops = unsafe { (*file.cast::<FileFopsPrefix>()).f_op };
    if unsafe { !(*file_ops).mmap_prepare.is_null() } {
        unsafe { compat_vma_mmap(file, vma) }
    } else {
        unsafe { ((*file_ops).mmap.unwrap())(file, vma) }
    }
}

// upstream: i915_gem_dmabuf.c i915_gem_map_dma_buf()
unsafe extern "C" fn i915_gem_map_dma_buf(
    attach: *mut DmaBufAttachment,
    direction: c_int,
) -> *mut SgTable {
    let obj = unsafe { dma_buf_to_obj((*attach).dmabuf) };
    let pages = unsafe { (*obj).mm.pages };
    let sgt = kmalloc_obj::<SgTable>(GFP_KERNEL);
    if sgt.is_null() {
        return ERR_PTR(-ENOMEM);
    }

    let mut ret = unsafe { sg_alloc_table(sgt, (*pages).orig_nents, GFP_KERNEL) };
    if ret != 0 {
        unsafe { kfree(sgt) };
        return ERR_PTR(ret);
    }

    let mut dst = unsafe { (*sgt).sgl };
    let mut src = unsafe { (*pages).sgl };
    for _ in 0..unsafe { (*pages).orig_nents } {
        let page = unsafe { sg_page(src) };
        unsafe {
            (*dst).page_link = ((*dst).page_link & SG_PAGE_LINK_MASK) | page as usize;
            (*dst).offset = 0;
            (*dst).length = (*src).length;
        }
        src = unsafe { sg_next(src) };
        dst = unsafe { sg_next(dst) };
    }

    ret = unsafe { dma_map_sgtable((*attach).dev, sgt, direction, DMA_ATTR_SKIP_CPU_SYNC) };
    if ret != 0 {
        unsafe {
            sg_free_table(sgt);
            kfree(sgt);
        }
        return ERR_PTR(ret);
    }

    sgt
}

// upstream: i915_gem_dmabuf.c i915_gem_dmabuf_vmap()
unsafe extern "C" fn i915_gem_dmabuf_vmap(dma_buf: *mut DmaBuf, map: *mut c_void) -> c_int {
    let map = map.cast::<IosysMap>();
    let obj = unsafe { dma_buf_to_obj(dma_buf) };
    let vaddr = unsafe { i915_gem_object_pin_map(obj, I915_MAP_WB) };
    if IS_ERR(vaddr) {
        return PTR_ERR(vaddr);
    }
    unsafe { iosys_map_set_vaddr(map, vaddr) };
    0
}

// upstream: i915_gem_dmabuf.c i915_gem_dmabuf_vunmap()
unsafe extern "C" fn i915_gem_dmabuf_vunmap(dma_buf: *mut DmaBuf, _map: *mut c_void) {
    let obj = unsafe { dma_buf_to_obj(dma_buf) };
    unsafe {
        i915_gem_object_flush_map(obj);
        i915_gem_object_unpin_map(obj);
    }
}

// upstream: i915_gem_dmabuf.c i915_gem_dmabuf_mmap()
unsafe extern "C" fn i915_gem_dmabuf_mmap(dma_buf: *mut DmaBuf, vma: *mut c_void) -> c_int {
    let vma = vma.cast::<VmAreaStruct>();
    let obj = unsafe { dma_buf_to_obj(dma_buf) };
    let base = unsafe { intel_bo_to_drm_bo(obj) };
    let i915 = unsafe { to_i915((*base).dev.cast()) };
    let vma_size = unsafe { (*vma).vm_end - (*vma).vm_start } as u64;
    if unsafe { (*base).size } < vma_size as usize {
        return -EINVAL;
    }

    if unsafe { HAS_LMEM(i915) } {
        return unsafe { drm_gem_prime_mmap(base.cast(), vma.cast()) };
    }

    let filp = unsafe { (*base).filp.cast::<File>() };
    if filp.is_null() {
        return -ENODEV;
    }
    let ret = unsafe { vfs_mmap(filp, vma) };
    if ret != 0 {
        return ret;
    }
    unsafe { crate::linux::mm::vma_set_file(vma, filp) };
    0
}

// upstream: i915_gem_dmabuf.c i915_gem_begin_cpu_access()
unsafe extern "C" fn i915_gem_begin_cpu_access(
    dma_buf: *mut DmaBuf,
    direction: c_int,
) -> c_int {
    let obj = unsafe { dma_buf_to_obj(dma_buf) };
    let write = direction == DMA_BIDIRECTIONAL || direction == DMA_TO_DEVICE;
    let mut ww = I915GemWwCtx::default();
    unsafe { i915_gem_ww_ctx_init(&mut ww, true) };

    let mut err;
    'retry: loop {
        err = unsafe { i915_gem_object_lock(obj, &mut ww) };
        if err == -EDEADLK {
            err = unsafe { i915_gem_ww_ctx_backoff(&mut ww) };
            if err == 0 {
                continue 'retry;
            }
            break 'retry;
        }
        if err != 0 {
            break 'retry;
        }

        err = unsafe { i915_gem_object_pin_pages(obj) };
        if err == -EDEADLK {
            err = unsafe { i915_gem_ww_ctx_backoff(&mut ww) };
            if err == 0 {
                continue 'retry;
            }
            break 'retry;
        }
        if err != 0 {
            break 'retry;
        }

        err = unsafe { i915_gem_object_set_to_cpu_domain(obj, write) };
        unsafe { i915_gem_object_unpin_pages(obj) };
        if err == -EDEADLK {
            err = unsafe { i915_gem_ww_ctx_backoff(&mut ww) };
            if err == 0 {
                continue 'retry;
            }
        }
        break 'retry;
    }

    unsafe { i915_gem_ww_ctx_fini(&mut ww) };
    err
}

// upstream: i915_gem_dmabuf.c i915_gem_end_cpu_access()
unsafe extern "C" fn i915_gem_end_cpu_access(
    dma_buf: *mut DmaBuf,
    _direction: c_int,
) -> c_int {
    let obj = unsafe { dma_buf_to_obj(dma_buf) };
    let mut ww = I915GemWwCtx::default();
    unsafe { i915_gem_ww_ctx_init(&mut ww, true) };

    let mut err;
    'retry: loop {
        err = unsafe { i915_gem_object_lock(obj, &mut ww) };
        if err == -EDEADLK {
            err = unsafe { i915_gem_ww_ctx_backoff(&mut ww) };
            if err == 0 {
                continue 'retry;
            }
            break 'retry;
        }
        if err != 0 {
            break 'retry;
        }

        err = unsafe { i915_gem_object_pin_pages(obj) };
        if err == -EDEADLK {
            err = unsafe { i915_gem_ww_ctx_backoff(&mut ww) };
            if err == 0 {
                continue 'retry;
            }
            break 'retry;
        }
        if err != 0 {
            break 'retry;
        }

        err = unsafe { i915_gem_object_set_to_gtt_domain(obj, false) };
        unsafe { i915_gem_object_unpin_pages(obj) };
        if err == -EDEADLK {
            err = unsafe { i915_gem_ww_ctx_backoff(&mut ww) };
            if err == 0 {
                continue 'retry;
            }
        }
        break 'retry;
    }

    unsafe { i915_gem_ww_ctx_fini(&mut ww) };
    err
}

// upstream: i915_gem_dmabuf.c i915_gem_dmabuf_attach()
unsafe extern "C" fn i915_gem_dmabuf_attach(
    dma_buf: *mut DmaBuf,
    _attach: *mut DmaBufAttachment,
) -> c_int {
    let obj = unsafe { dma_buf_to_obj(dma_buf) };
    if !unsafe { i915_gem_object_can_migrate(obj, INTEL_REGION_SMEM) } {
        return -EOPNOTSUPP;
    }

    let mut ww = I915GemWwCtx::default();
    unsafe { i915_gem_ww_ctx_init(&mut ww, true) };
    let mut err = -EDEADLK;
    'ww: loop {
        err = unsafe { i915_gem_object_lock(obj, &mut ww) };
        if err != 0 {
            if err == -EDEADLK {
                err = unsafe { i915_gem_ww_ctx_backoff(&mut ww) };
                if err == 0 {
                    err = -EDEADLK;
                    continue 'ww;
                }
            }
            break 'ww;
        }

        err = unsafe { i915_gem_object_migrate(obj, &mut ww, INTEL_REGION_SMEM) };
        if err != 0 {
            if err == -EDEADLK {
                err = unsafe { i915_gem_ww_ctx_backoff(&mut ww) };
                if err == 0 {
                    err = -EDEADLK;
                    continue 'ww;
                }
            }
            break 'ww;
        }

        err = unsafe { i915_gem_object_wait_migration(obj, 0) };
        if err != 0 {
            if err == -EDEADLK {
                err = unsafe { i915_gem_ww_ctx_backoff(&mut ww) };
                if err == 0 {
                    err = -EDEADLK;
                    continue 'ww;
                }
            }
            break 'ww;
        }

        err = unsafe { i915_gem_object_pin_pages(obj) };
        if err == -EDEADLK {
            err = unsafe { i915_gem_ww_ctx_backoff(&mut ww) };
            if err == 0 {
                err = -EDEADLK;
                continue 'ww;
            }
        }
        break 'ww;
    }
    unsafe { i915_gem_ww_ctx_fini(&mut ww) };
    err
}

// upstream: i915_gem_dmabuf.c i915_gem_dmabuf_detach()
unsafe extern "C" fn i915_gem_dmabuf_detach(dma_buf: *mut DmaBuf, _attach: *mut DmaBufAttachment) {
    let obj = unsafe { dma_buf_to_obj(dma_buf) };
    unsafe { i915_gem_object_unpin_pages(obj) };
}

static I915_DMABUF_OPS: DmaBufOps = DmaBufOps {
    attach: Some(i915_gem_dmabuf_attach),
    detach: Some(i915_gem_dmabuf_detach),
    pin: None,
    unpin: None,
    map_dma_buf: Some(i915_gem_map_dma_buf),
    unmap_dma_buf: Some(drm_gem_unmap_dma_buf),
    release: Some(drm_gem_dmabuf_release),
    begin_cpu_access: Some(i915_gem_begin_cpu_access),
    end_cpu_access: Some(i915_gem_end_cpu_access),
    mmap: Some(i915_gem_dmabuf_mmap),
    vmap: Some(i915_gem_dmabuf_vmap),
    vunmap: Some(i915_gem_dmabuf_vunmap),
};

// upstream: i915_gem_dmabuf.c i915_gem_prime_export()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_prime_export(
    gem_obj: *mut DrmGemObject,
    flags: c_int,
) -> *mut DmaBuf {
    let obj = gem_obj.cast::<DrmI915GemObject>();
    let mut info = DmaBufExportInfo {
        exp_name: KBUILD_MODNAME_I915.as_ptr().cast(),
        // TheKernel's i915 is built into the kernel, so THIS_MODULE is NULL.
        owner: core::ptr::null_mut(),
        ops: &I915_DMABUF_OPS,
        size: unsafe { (*gem_obj).size as usize },
        flags,
        resv: unsafe { (&(*obj).base.base).resv.cast() },
        r#priv: gem_obj.cast(),
    };

    if let Some(export) = unsafe { (*(*obj).ops).dmabuf_export } {
        let ret = unsafe { export(obj) };
        if ret != 0 {
            return ERR_PTR(ret);
        }
    }

    let dev = unsafe { (*gem_obj).dev.cast::<DrmDevice>() };
    unsafe { drm_gem_dmabuf_export(dev.cast(), &mut info) }
}

// upstream: i915_gem_dmabuf.c i915_gem_object_get_pages_dmabuf()
unsafe extern "C" fn i915_gem_object_get_pages_dmabuf(obj: *mut DrmI915GemObject) -> c_int {
    let base = unsafe { intel_bo_to_drm_bo(obj) };
    let i915 = unsafe { to_i915((*base).dev.cast()) };
    unsafe { assert_object_held(obj) };

    let attach = unsafe { (*base).import_attach.cast::<DmaBufAttachment>() };
    let sgt = unsafe { dma_buf_map_attachment(attach, DMA_BIDIRECTIONAL) };
    if IS_ERR(sgt) {
        return PTR_ERR(sgt);
    }

    if unsafe { i915_gem_object_can_bypass_llc(obj) }
        || (!unsafe { HAS_LLC(i915) } && !unsafe { IS_DG1(i915) })
    {
        unsafe { wbinvd_on_all_cpus() };
    }

    unsafe { __i915_gem_object_set_pages(obj, sgt) };
    0
}

// upstream: i915_gem_dmabuf.c i915_gem_object_put_pages_dmabuf()
unsafe extern "C" fn i915_gem_object_put_pages_dmabuf(
    obj: *mut DrmI915GemObject,
    sgt: *mut SgTable,
) {
    let base = unsafe { intel_bo_to_drm_bo(obj) };
    let attach = unsafe { (*base).import_attach.cast::<DmaBufAttachment>() };
    unsafe { dma_buf_unmap_attachment(attach, sgt, DMA_BIDIRECTIONAL) };
}

static I915_GEM_OBJECT_DMABUF_OPS: DrmI915GemObjectOps = DrmI915GemObjectOps {
    flags: 0,
    get_pages: Some(i915_gem_object_get_pages_dmabuf),
    put_pages: Some(i915_gem_object_put_pages_dmabuf),
    truncate: None,
    shrink: None,
    pread: None,
    pwrite: None,
    mmap_offset: None,
    unmap_virtual: None,
    dmabuf_export: None,
    adjust_lru: None,
    delayed_free: None,
    migrate: None,
    release: None,
    mmap_ops: core::ptr::null(),
    name: b"i915_gem_object_dmabuf\0".as_ptr().cast::<c_char>(),
};

// upstream: i915_gem_dmabuf.c i915_gem_prime_import()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_prime_import(
    dev: *mut DrmDevice,
    dma_buf: *mut c_void,
) -> *mut DrmGemObject {
    let dma_buf = dma_buf.cast::<DmaBuf>();
    let is_i915_export = unsafe { (*dma_buf).ops == &I915_DMABUF_OPS };
    if is_i915_export {
        let obj = unsafe { dma_buf_to_obj(dma_buf) };
        let base = unsafe { intel_bo_to_drm_bo(obj) };
        if unsafe { (*base).dev == dev.cast() } && !I915_SELFTEST_ONLY!(force_different_devices) {
            let obj = unsafe { i915_gem_object_get(obj) };
            return unsafe { intel_bo_to_drm_bo(obj) };
        }
    }

    // The shared DMA-BUF core owns file lifetime and attachment dispatch, but
    // this i915 integration has no cross-device memory-sharing contract yet.
    // Preserve its same-device fast path and fail closed before attaching any
    // foreign or different-device exporter; selftests retain their deliberate
    // force-different-devices coverage.
    if !is_i915_export || !I915_SELFTEST_ONLY!(force_different_devices) {
        return ERR_PTR(-ENODEV);
    }

    let size = unsafe { (*dma_buf).size as u64 };
    if i915_gem_object_size_2big(size) {
        return ERR_PTR(-E2BIG);
    }

    let attach = unsafe { dma_buf_attach(dma_buf, (*dev).dev) };
    if IS_ERR(attach) {
        return attach.cast();
    }

    // The DMA-BUF retains a file reference until dma_buf_put; acquiring this
    // active reference is the Linux header-inline get_dma_buf operation.
    unsafe { get_dma_buf(dma_buf) };

    let obj = unsafe { i915_gem_object_alloc() };
    if obj.is_null() {
        unsafe {
            dma_buf_detach(dma_buf, attach);
            dma_buf_put(dma_buf);
        }
        return ERR_PTR(-ENOMEM);
    }

    let base = unsafe { intel_bo_to_drm_bo(obj) };
    unsafe {
        drm_gem_private_object_init(dev.cast(), base, size as usize);
        i915_gem_object_init(
            obj,
            &I915_GEM_OBJECT_DMABUF_OPS,
            core::ptr::addr_of_mut!(I915_GEM_DMABUF_LOCK_CLASS),
            I915_BO_ALLOC_USER as u32,
        );
        (*base).import_attach = attach.cast();
        (*base).resv = (*dma_buf).resv.cast();
        (*obj).read_domains = I915_GEM_DOMAIN_GTT as u16;
        (*obj).write_domain = 0;
    }

    base
}

static mut I915_GEM_DMABUF_LOCK_CLASS: LockClassKey = LockClassKey {};
