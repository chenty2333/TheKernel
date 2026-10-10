// SPDX-License-Identifier: MIT
// Copyright (c) 2008 Intel Corporation.
//! Linux 7.2.3 i915_gpu_error.c capture chain, with the upstream
//! `struct i915_gpu_coredump` layout.
//!
//! Scope: the coredump allocation/free, engine/GT/VMA capture and the GuC
//! capture path (`__i915_gpu_coredump`). The debugfs/sysfs formatter and
//! scatterlist printing (`err_print_*`, `i915_gpu_coredump_copy_to_buffer`)
//! are not translated (COMMON-WIRE rule 5). `CONFIG_DRM_I915_COMPRESS_ERROR`
//! is unset, so the upstream `!COMPRESS` page-copy branch is the one used.
#![allow(unsafe_code, unsafe_op_in_unsafe_fn, non_camel_case_types)]
use core::{
    ffi::{c_char, c_void},
    mem::{offset_of, size_of},
};

use crate::{
    i915_memcpy_upstream::i915_memcpy_from_wc,
    intel_device_info_types_upstream::{IntelDeviceInfo, IntelDriverCaps},
    intel_engine_cs_upstream::ListHead,
    intel_engine_types_upstream::{EXECLIST_MAX_PORTS, I915_MAX_SFC, IntelEngineCs},
    intel_gt_types_upstream::IntelGt,
    intel_guc_capture_upstream::{intel_guc_capture_free_node, intel_guc_capture_get_matching_node},
    linux::{
        list::{INIT_LIST_HEAD, list_add_tail, list_del_init, list_empty},
        memory::{kfree, kmalloc, kmalloc_obj, kref_get, kref_init, kref_put},
        primitives::{jiffies, ktime_get},
    },
    linux_config::{ENOMEM, GFP_KERNEL},
    linux_i915_private::DrmI915Private,
};

use crate::intel_workarounds_upstream::{I915Reg, I915McrReg};
use crate::linux::registers::{GAM_ECOCHK, GFX_MODE};

// Register offsets from drivers/gpu/drm/i915/{gt/intel_gt_regs.h,i915_reg.h}
// that the coredump reads. Same values as the per-file definitions elsewhere.
const fn mmio(offset: u32) -> I915Reg {
    I915Reg { reg: offset }
}
const fn mcr(offset: u32) -> I915McrReg {
    I915McrReg { reg: offset }
}
#[allow(non_snake_case)]
const fn FENCE_REG_GEN6_LO(i: u32) -> I915Reg {
    mmio(0x100000 + i * 8)
}
#[allow(non_snake_case)]
const fn FENCE_REG_965_LO(i: u32) -> I915Reg {
    mmio(0x03000 + i * 8)
}
#[allow(non_snake_case)]
const fn FENCE_REG(i: u32) -> I915Reg {
    mmio(0x2000 + (((i) & 8) << 9) + ((i) & 7) * 4)
}
#[allow(non_snake_case)]
const fn GEN8_GT_IER(which: u32) -> I915Reg {
    mmio(0x4430c + 0x10 * which)
}
#[allow(non_snake_case)]
const fn GEN12_SFC_DONE(n: u32) -> I915Reg {
    mmio(0x1cc000 + n * 0x1000)
}
const GTIER: I915Reg = mmio(0x4401c);
const GEN11_RENDER_COPY_INTR_ENABLE: I915Reg = mmio(0x190030);
const GEN11_VCS_VECS_INTR_ENABLE: I915Reg = mmio(0x190034);
const GEN11_GUC_SG_INTR_ENABLE: I915Reg = mmio(0x190038);
const GEN11_GPM_WGBOXPERF_INTR_ENABLE: I915Reg = mmio(0x19003c);
const GEN11_CRYPTO_RSVD_INTR_ENABLE: I915Reg = mmio(0x190040);
const GEN11_GUNIT_CSME_INTR_ENABLE: I915Reg = mmio(0x190044);
const GEN2_IER: I915Reg = mmio(0x20a0);
const EIR: I915Reg = mmio(0x20b0);
const PGTBL_ER: I915Reg = mmio(0x02024);
const FORCEWAKE_VLV: I915Reg = mmio(0x1300b0);
const FORCEWAKE: I915Reg = mmio(0xa18c);
const FORCEWAKE_MT: I915Reg = mmio(0xa188);
const GAB_CTL: I915Reg = mmio(0x24000);
const ERROR_GEN6: I915Reg = mmio(0x40a0);
const DONE_REG: I915Reg = mmio(0x40b0);
const GAC_ECO_BITS: I915Reg = mmio(0x14090);
const GEN12_AUX_ERR_DBG: I915Reg = mmio(0x43f4);
const GEN12_GAM_DONE: I915Reg = mmio(0xcf68);
const HSW_GTT_CACHE_EN: I915Reg = mmio(0x4024);
const GUCPMTIMESTAMP: I915Reg = mmio(0xc3e8);
const GEN8_FAULT_TLB_DATA0: I915Reg = mmio(0x4b10);
const GEN8_FAULT_TLB_DATA1: I915Reg = mmio(0x4b14);
const GEN12_FAULT_TLB_DATA0: I915Reg = mmio(0xceb8);
const GEN12_FAULT_TLB_DATA1: I915Reg = mmio(0xcebc);
const XEHP_FAULT_TLB_DATA0: I915McrReg = mcr(0xceb8);
const XEHP_FAULT_TLB_DATA1: I915McrReg = mcr(0xcebc);
const GEN8_RING_FAULT_REG: I915Reg = mmio(0x4094);
const GEN12_RING_FAULT_REG: I915Reg = mmio(0xcec4);
const XELPMP_RING_FAULT_REG: I915Reg = mmio(0xcec4);
const XEHP_RING_FAULT_REG: I915McrReg = mcr(0xcec4);
const RENDER_HWS_PGA_GEN7: I915Reg = mmio(0x4080);
const BLT_HWS_PGA_GEN7: I915Reg = mmio(0x4280);
const BSD_HWS_PGA_GEN7: I915Reg = mmio(0x4180);
const VEBOX_HWS_PGA_GEN7: I915Reg = mmio(0x4380);

/// `struct i915_vma_coredump`.
#[repr(C)]
pub struct I915VmaCoredump {
    pub next: *mut I915VmaCoredump,
    pub name: [c_char; 20],
    pub gtt_offset: u64,
    pub gtt_size: u64,
    pub gtt_page_sizes: u32,
    pub unused: i32,
    pub page_list: ListHead,
}

/// `struct i915_request_coredump`.
#[repr(C)]
pub struct I915RequestCoredump {
    pub flags: u64,
    pub pid: i32,
    pub context: u32,
    pub seqno: u32,
    pub head: u32,
    pub tail: u32,
    pub sched_attr: crate::i915_scheduler_types_upstream::I915SchedAttr,
}

/// `struct intel_engine_capture_vma`.
#[repr(C)]
pub struct IntelEngineCaptureVma {
    pub next: *mut IntelEngineCaptureVma,
    pub vma_res: *mut crate::i915_vma_resource_types_upstream::I915VmaResource,
    pub name: [c_char; 16],
    pub lockdep_cookie: bool,
}

/// `struct i915_gem_context_coredump`.
#[repr(C)]
pub struct I915GemContextCoredump {
    pub comm: [c_char; 16],
    pub total_runtime: u64,
    pub avg_runtime: u64,
    pub pid: i32,
    pub active: i32,
    pub guilty: i32,
    pub sched_attr: crate::i915_scheduler_types_upstream::I915SchedAttr,
    pub hwsp_seqno: u32,
}

/// `struct intel_engine_coredump`.
#[repr(C)]
pub struct IntelEngineCoredump {
    pub engine: *const IntelEngineCs,
    pub hung: bool,
    pub simulated: bool,
    pub reset_count: u32,
    pub rq_head: u32,
    pub rq_post: u32,
    pub rq_tail: u32,
    pub ccid: u32,
    pub start: u32,
    pub tail: u32,
    pub head: u32,
    pub ctl: u32,
    pub mode: u32,
    pub hws: u32,
    pub ipeir: u32,
    pub ipehr: u32,
    pub esr: u32,
    pub bbstate: u32,
    pub instpm: u32,
    pub instps: u32,
    pub bbaddr: u64,
    pub acthd: u64,
    pub fault_reg: u32,
    pub faddr: u64,
    pub rc_psmi: u32,
    pub nopid: u32,
    pub excc: u32,
    pub cmd_cctl: u32,
    pub cscmdop: u32,
    pub ctx_sr_ctl: u32,
    pub dma_faddr_hi: u32,
    pub dma_faddr_lo: u32,
    pub instdone: crate::linux::fields::IntelInstdone,
    pub guc_capture: *mut crate::intel_guc_capture_upstream::IntelGucStateCapture,
    pub guc_capture_node: *mut crate::intel_guc_capture_upstream::ParsedOutput,
    pub context: I915GemContextCoredump,
    pub vma: *mut I915VmaCoredump,
    pub execlist: [I915RequestCoredump; EXECLIST_MAX_PORTS],
    pub num_ports: u32,
    pub vm_info: IntelEngineVmInfo,
    pub next: *mut IntelEngineCoredump,
}

#[repr(C)]
pub struct IntelEngineVmInfo {
    pub gfx_mode: u32,
    /// Anonymous C union `{ u64 pdp[4]; u32 pp_dir_base; }`.
    pub pdp: [u64; 4],
}

/// `struct intel_ctb_coredump`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelCtbCoredump {
    pub raw_head: u32,
    pub head: u32,
    pub raw_tail: u32,
    pub tail: u32,
    pub raw_status: u32,
    pub desc_offset: u32,
    pub cmds_offset: u32,
    pub size: u32,
}

/// `struct intel_uc_coredump`.
#[repr(C)]
pub struct IntelUcCoredump {
    pub guc_fw: crate::intel_uc_fw_types_upstream::IntelUcFw,
    pub huc_fw: crate::intel_uc_fw_types_upstream::IntelUcFw,
    pub guc: IntelGucInfoCoredump,
}

/// `struct guc_info` inside `struct intel_uc_coredump`.
#[repr(C)]
pub struct IntelGucInfoCoredump {
    pub ctb: [IntelCtbCoredump; 2],
    pub vma_ctb: *mut I915VmaCoredump,
    pub vma_log: *mut I915VmaCoredump,
    pub hw_state: *mut u32,
    pub timestamp: u32,
    pub last_fence: u16,
    pub is_guc_capture: bool,
}

/// `struct intel_gt_coredump`.
#[repr(C)]
pub struct IntelGtCoredump {
    pub _gt: *const IntelGt,
    pub awake: bool,
    pub simulated: bool,
    pub info: crate::intel_gt_types_upstream::IntelGtInfo,
    pub eir: u32,
    pub pgtbl_er: u32,
    pub gtier: [u32; 6],
    pub ngtier: u32,
    pub forcewake: u32,
    pub error: u32,
    pub fault_data0: u32,
    pub fault_data1: u32,
    pub done_reg: u32,
    pub gac_eco: u32,
    pub gam_ecochk: u32,
    pub gab_ctl: u32,
    pub gfx_mode: u32,
    pub gtt_cache: u32,
    pub aux_err: u32,
    pub gam_done: u32,
    pub clock_frequency: u32,
    pub clock_period_ns: u32,
    pub sfc_done: [u32; I915_MAX_SFC],
    pub nfence: u32,
    pub fence: [u64; crate::intel_gtt_api_upstream::I915_MAX_NUM_FENCES as usize],
    pub engine: *mut IntelEngineCoredump,
    pub uc: *mut IntelUcCoredump,
    pub next: *mut IntelGtCoredump,
}

/// `struct i915_gpu_coredump`.
#[repr(C)]
pub struct I915GpuCoredump {
    pub ref_: crate::intel_context_upstream::Kref,
    pub time: i64,
    pub boottime: i64,
    pub uptime: i64,
    pub capture: u64,
    pub i915: *mut DrmI915Private,
    pub gt: *mut IntelGtCoredump,
    pub error_msg: [c_char; 128],
    pub simulated: bool,
    pub wakelock: bool,
    pub suspended: bool,
    pub iommu: i32,
    pub reset_count: u32,
    pub suspend_count: u32,
    pub device_info: IntelDeviceInfo,
    pub runtime_info: crate::linux::i915::IntelRuntimeInfo,
    pub driver_caps: IntelDriverCaps,
    pub params: crate::linux::i915_private::I915Params,
    /// Display snapshot capture is owned by the display side; TheKernel does
    /// not take a display snapshot, so this stays NULL (upstream
    /// `intel_display_snapshot_capture()` returns NULL when there is no
    /// display to snapshot).
    pub display_snapshot: *mut c_void,
}


/// `kzalloc_obj(*ptr, gfp)`.
unsafe fn zalloc_obj<T>(gfp: u32) -> *mut T {
    crate::linux::memory::kzalloc(size_of::<T>(), gfp).cast()
}

/// Borrow a NUL-terminated kernel string as `&str` (diagnostics only).
unsafe fn cstr<'a>(p: *const c_char) -> &'a str {
    let mut n = 0usize;
    while *p.add(n) != 0 {
        n += 1;
    }
    core::str::from_utf8(core::slice::from_raw_parts(p.cast::<u8>(), n)).unwrap_or("?")
}

/// `strscpy(dst, src, n)`: copy at most n-1 bytes and always NUL-terminate.
unsafe fn strscpy(dst: *mut c_char, src: *const c_char, n: usize) {
    let mut i = 0usize;
    while i + 1 < n && *src.add(i) != 0 {
        *dst.add(i) = *src.add(i);
        i += 1;
    }
    *dst.add(i) = 0;
}

/// `kstrdup(src, ALLOW_FAIL)`; NULL stays NULL.
unsafe fn kstrdup_c(src: *const c_char) -> *mut c_char {
    if src.is_null() {
        return core::ptr::null_mut();
    }
    let mut n = 0usize;
    while *src.add(n) != 0 {
        n += 1;
    }
    let dst = kmalloc(n + 1, ALLOW_FAIL).cast::<c_char>();
    if !dst.is_null() {
        core::ptr::copy_nonoverlapping(src, dst, n + 1);
    }
    dst
}

// --- Page pool (the !CONFIG_DRM_I915_COMPRESS_ERROR `struct i915_vma_compress`).

/// Stand-in for `struct page` used by the captured VMA page lists. `lru` must
/// stay first: `virt_to_page()` relies on `data` being at a fixed offset.
#[repr(C)]
pub struct CapturePage {
    pub lru: ListHead,
    pub data: [u8; 4096],
}

const PAGEVEC_SIZE: usize = 15;
const PAGE_SIZE: usize = 4096;
/// `ALLOW_FAIL` in i915_utils.h: a best-effort allocation.
const ALLOW_FAIL: u32 = GFP_KERNEL | crate::linux::config::__GFP_NOWARN;
/// `__GFP_ZERO`.
const GFP_ZERO: u32 = 1 << 8;

/// Layout of `struct guc_ct_buffer_desc` (first three u32 fields).
#[repr(C)]
struct CtbDescLayout {
    head: u32,
    tail: u32,
    status: u32,
}

/// `struct folio_batch` holding captured pages.
#[repr(C)]
pub struct FolioBatch {
    pub nr: u32,
    pub folios: [*mut CapturePage; PAGEVEC_SIZE],
}

#[repr(C)]
pub struct I915VmaCompress {
    pub pool: FolioBatch,
}

unsafe fn alloc_capture_page(gfp: u32) -> *mut CapturePage {
    kmalloc_obj::<CapturePage>(gfp)
}

unsafe fn virt_to_capture_page(addr: *mut c_void) -> *mut CapturePage {
    addr.cast::<u8>()
        .sub(offset_of!(CapturePage, data))
        .cast::<CapturePage>()
}

/// `pool_fini()`: release every cached page.
unsafe fn pool_fini(fbatch: *mut FolioBatch) {
    while (*fbatch).nr > 0 {
        (*fbatch).nr -= 1;
        kfree((*fbatch).folios[(*fbatch).nr as usize]);
    }
}

/// `pool_refill()`: fill the batch with fresh pages.
unsafe fn pool_refill(fbatch: *mut FolioBatch, gfp: u32) -> i32 {
    while ((*fbatch).nr as usize) < PAGEVEC_SIZE {
        let folio = alloc_capture_page(gfp);
        if folio.is_null() {
            return -(ENOMEM as i32);
        }
        (*fbatch).folios[(*fbatch).nr as usize] = folio;
        (*fbatch).nr += 1;
    }
    0
}

/// `pool_init()`.
unsafe fn pool_init(fbatch: *mut FolioBatch, gfp: u32) -> i32 {
    (*fbatch).nr = 0;
    let err = pool_refill(fbatch, gfp);
    if err != 0 {
        pool_fini(fbatch);
    }
    err
}

/// `pool_alloc()`: a fresh page, or a cached one when memory is short.
unsafe fn pool_alloc(fbatch: *mut FolioBatch, gfp: u32) -> *mut c_void {
    let mut folio = alloc_capture_page(gfp);
    if folio.is_null() && (*fbatch).nr > 0 {
        (*fbatch).nr -= 1;
        folio = (*fbatch).folios[(*fbatch).nr as usize];
    }
    if folio.is_null() {
        core::ptr::null_mut()
    } else {
        core::ptr::addr_of_mut!((*folio).data).cast()
    }
}

/// `pool_free()`.
unsafe fn pool_free(fbatch: *mut FolioBatch, addr: *mut c_void) {
    let folio = virt_to_capture_page(addr);
    if ((*fbatch).nr as usize) < PAGEVEC_SIZE {
        (*fbatch).folios[(*fbatch).nr as usize] = folio;
        (*fbatch).nr += 1;
    } else {
        kfree(folio);
    }
}

unsafe fn compress_init(c: *mut I915VmaCompress) -> bool {
    pool_init(core::ptr::addr_of_mut!((*c).pool), ALLOW_FAIL) == 0
}

unsafe fn compress_start(_c: *mut I915VmaCompress) -> bool {
    true
}

/// Non-compressing `compress_page()`: copy one page into the VMA page list.
/// `cond_resched()` is a scheduling point with no state to translate.
unsafe fn compress_page(
    c: *mut I915VmaCompress,
    src: *mut c_void,
    dst: *mut I915VmaCoredump,
    wc: bool,
) -> i32 {
    let ptr = pool_alloc(core::ptr::addr_of_mut!((*c).pool), ALLOW_FAIL);
    if ptr.is_null() {
        return -(ENOMEM as i32);
    }
    if !(wc && i915_memcpy_from_wc(ptr, src, PAGE_SIZE as u64)) {
        core::ptr::copy_nonoverlapping(src.cast::<u8>(), ptr.cast::<u8>(), PAGE_SIZE);
    }
    let page = virt_to_capture_page(ptr);
    list_add_tail(core::ptr::addr_of_mut!((*page).lru), core::ptr::addr_of_mut!((*dst).page_list));
    0
}

unsafe fn compress_flush(_c: *mut I915VmaCompress, _dst: *mut I915VmaCoredump) -> i32 {
    0
}

unsafe fn compress_finish(_c: *mut I915VmaCompress) {}

unsafe fn compress_fini(c: *mut I915VmaCompress) {
    pool_fini(core::ptr::addr_of_mut!((*c).pool));
}

/// `pool_refill()` before entering the atomic per-engine section.
unsafe fn gt_record_engines_refill(c: *mut I915VmaCompress) {
    let _ = pool_refill(core::ptr::addr_of_mut!((*c).pool), ALLOW_FAIL);
}

// --- VMA coredump lists.

unsafe fn i915_vma_coredump_free(mut vma: *mut I915VmaCoredump) {
    while !vma.is_null() {
        let next = (*vma).next;
        let head: *mut ListHead = core::ptr::addr_of_mut!((*vma).page_list);
        while !list_empty(&*head) {
            let page = (*head).next.cast::<CapturePage>();
            list_del_init(core::ptr::addr_of_mut!((*page).lru));
            kfree(page);
        }
        kfree(vma);
        vma = next;
    }
}

unsafe fn cleanup_params(error: *mut I915GpuCoredump) {
    crate::i915_driver_upstream::i915_params_free(core::ptr::addr_of_mut!((*error).params).cast());
}

unsafe fn cleanup_uc(uc: *mut IntelUcCoredump) {
    kfree((*uc).guc_fw.file_selected.path as *mut c_char);
    kfree((*uc).huc_fw.file_selected.path as *mut c_char);
    kfree((*uc).guc_fw.file_wanted.path as *mut c_char);
    kfree((*uc).huc_fw.file_wanted.path as *mut c_char);
    i915_vma_coredump_free((*uc).guc.vma_log);
    i915_vma_coredump_free((*uc).guc.vma_ctb);
    kfree((*uc).guc.hw_state);
    kfree(uc);
}

unsafe fn cleanup_gt(gt: *mut IntelGtCoredump) {
    while !(*gt).engine.is_null() {
        let ee = (*gt).engine;
        (*gt).engine = (*ee).next;
        i915_vma_coredump_free((*ee).vma);
        intel_guc_capture_free_node(ee);
        kfree(ee);
    }
    if !(*gt).uc.is_null() {
        cleanup_uc((*gt).uc);
    }
    kfree(gt);
}

/// `__i915_gpu_coredump_free()`: the kref release for a coredump.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __i915_gpu_coredump_free(kref: *mut crate::intel_context_upstream::Kref) {
    let error = kref.cast::<u8>().sub(offset_of!(I915GpuCoredump, ref_)).cast::<I915GpuCoredump>();
    while !(*error).gt.is_null() {
        let gt = (*error).gt;
        (*error).gt = (*gt).next;
        cleanup_gt(gt);
    }
    cleanup_params(error);
    kfree(error);
}

// upstream: i915_gpu_error.c i915_vma_coredump_create()
unsafe fn i915_vma_coredump_create(
    gt: *const IntelGt,
    vma_res: *mut crate::i915_vma_resource_types_upstream::I915VmaResource,
    compress: *mut I915VmaCompress,
    name: *const c_char,
) -> *mut I915VmaCoredump {
    if vma_res.is_null() || (*vma_res).bi.pages.is_null() || compress.is_null() {
        return core::ptr::null_mut();
    }
    let dst = kmalloc_obj::<I915VmaCoredump>(ALLOW_FAIL);
    if dst.is_null() {
        return core::ptr::null_mut();
    }
    if !compress_start(compress) {
        kfree(dst);
        return core::ptr::null_mut();
    }

    INIT_LIST_HEAD(core::ptr::addr_of_mut!((*dst).page_list));
    strscpy(core::ptr::addr_of_mut!((*dst).name).cast(), name, 20);
    (*dst).next = core::ptr::null_mut();
    (*dst).gtt_offset = (*vma_res).start;
    (*dst).gtt_size = (*vma_res).node_size;
    (*dst).gtt_page_sizes = (*vma_res).page_sizes_gtt;
    (*dst).unused = 0;

    let mut ret: i32 = -(crate::linux_config::EINVAL as i32);
    // TheKernel's GGTT does not reserve `ggtt->error_capture` (upstream
    // intel_ggtt.c:914), so the `drm_mm_node_allocated()` window branch is
    // unreachable here; the system-page branch below is the one upstream uses
    // in that configuration.
    for_each_sgt_page(vma_res, |page| {
        let s = crate::linux::highmem::kmap_local_page(page);
        crate::i915_gem_clflush_upstream::drm_clflush_virt_range(s, PAGE_SIZE as _);
        ret = compress_page(compress, s, dst, false);
        crate::i915_gem_clflush_upstream::drm_clflush_virt_range(s, PAGE_SIZE as _);
        crate::linux::highmem::kunmap_local(s);
        ret == 0
    });

    if ret != 0 || compress_flush(compress, dst) != 0 {
        let head: *mut ListHead = core::ptr::addr_of_mut!((*dst).page_list);
        while !list_empty(&*head) {
            let page = (*head).prev.cast::<CapturePage>();
            list_del_init(core::ptr::addr_of_mut!((*page).lru));
            pool_free(core::ptr::addr_of_mut!((*compress).pool), (*page).data.as_mut_ptr().cast());
        }
        kfree(dst);
        compress_finish(compress);
        return core::ptr::null_mut();
    }
    compress_finish(compress);
    let _ = gt;
    dst
}

/// `for_each_sgt_page()` over a VMA resource's backing pages; the closure
/// returns false to stop early.
unsafe fn for_each_sgt_page(
    vma_res: *mut crate::i915_vma_resource_types_upstream::I915VmaResource,
    mut f: impl FnMut(*mut crate::i915_gem_object_types_upstream::Page) -> bool,
) {
    let mut it = crate::i915_gem_shmem_upstream::sgt_iter_init((*(*vma_res).bi.pages).sgl);
    loop { let page = crate::i915_gem_shmem_upstream::sgt_iter_next_page(&mut it); if page.is_null() { break; }
        if !f(page) {
            break;
        }
    }
}

unsafe fn create_vma_coredump(
    gt: *const IntelGt,
    vma: *mut crate::i915_vma_types_upstream::I915Vma,
    name: *const c_char,
    compress: *mut I915VmaCompress,
) -> *mut I915VmaCoredump {
    if vma.is_null() {
        return core::ptr::null_mut();
    }
    let vma_res = (*vma).resource;
    let mut cookie = false;
    if crate::i915_vma_resource_upstream::i915_vma_resource_hold(vma_res, &mut cookie) {
        let ret = i915_vma_coredump_create(gt, vma_res, compress, name);
        crate::i915_vma_resource_upstream::i915_vma_resource_unhold(vma_res, cookie);
        return ret;
    }
    core::ptr::null_mut()
}

unsafe fn add_vma(ee: *mut IntelEngineCoredump, vma: *mut I915VmaCoredump) {
    if !vma.is_null() {
        (*vma).next = (*ee).vma;
        (*ee).vma = vma;
    }
}

unsafe fn add_vma_coredump(
    ee: *mut IntelEngineCoredump,
    gt: *const IntelGt,
    vma: *mut crate::i915_vma_types_upstream::I915Vma,
    name: *const c_char,
    compress: *mut I915VmaCompress,
) {
    add_vma(ee, create_vma_coredump(gt, vma, name, compress));
}

// upstream: i915_gpu_error.c record_request()
unsafe fn record_request(
    request: *const crate::i915_request_types_upstream::I915Request,
    erq: *mut I915RequestCoredump,
) {
    (*erq).flags = (*request).fence.flags as u64;
    (*erq).context = (*request).fence.context as u32;
    (*erq).seqno = (*request).fence.seqno as u32;
    (*erq).sched_attr = (*request).sched.attr;
    (*erq).head = (*request).head;
    (*erq).tail = (*request).tail;
    (*erq).pid = 0;
    let ctx = (*request).context;
    if !crate::intel_context_api_upstream::intel_context_is_closed(ctx) {
        let gem_ctx = (*ctx).gem_context;
        if !gem_ctx.is_null() {
            (*erq).pid = crate::linux::kernel_services::pid_nr((*gem_ctx).pid.cast()) as i32;
        }
    }
}

// upstream: i915_gpu_error.c engine_record_execlists()
unsafe fn engine_record_execlists(ee: *mut IntelEngineCoredump) {
    let el = core::ptr::addr_of!((*(*ee).engine).execlists);
    let mut port = (*el).active as *const *const crate::i915_request_types_upstream::I915Request;
    let mut n: usize = 0;
    while !(*port).is_null() {
        record_request(*port, core::ptr::addr_of_mut!((*ee).execlist[n]));
        n += 1;
        port = port.add(1);
    }
    (*ee).num_ports = n as u32;
}

// upstream: i915_gpu_error.c record_context()
unsafe fn record_context(
    e: *mut I915GemContextCoredump,
    ce: *mut crate::intel_context_types_upstream::IntelContext,
) -> bool {
    let ctx = (*ce).gem_context;
    if ctx.is_null() || !crate::linux::memory::kref_get_unless_zero(&mut (*ctx).r#ref) {
        return true;
    }
    // TheKernel has no pid -> task mapping; the comm stays empty and only the
    // pid number (pid_nr) is recorded.
    (*e).pid = crate::linux::kernel_services::pid_nr((*ctx).pid.cast()) as i32;
    (*e).sched_attr = (*ctx).sched;
    (*e).guilty = (*ctx).guilty_count.counter;
    (*e).active = (*ctx).active_count.counter;
    (*e).hwsp_seqno = if !(*ce).timeline.is_null() && !(*(*ce).timeline).hwsp_seqno.is_null() {
        *(*(*ce).timeline).hwsp_seqno
    } else {
        !0u32
    };
    (*e).total_runtime = crate::intel_context_upstream::intel_context_get_total_runtime_ns(ce);
    (*e).avg_runtime = crate::intel_context_upstream::intel_context_get_avg_runtime_ns(ce);
    let simulated = crate::i915_gem_context_upstream::i915_gem_context_no_error_capture(ctx);
    crate::i915_gem_context_upstream::i915_gem_context_put(ctx);
    simulated
}

unsafe fn capture_vma_snapshot(
    next: *mut IntelEngineCaptureVma,
    vma_res: *mut crate::i915_vma_resource_types_upstream::I915VmaResource,
    gfp: u32,
    name: *const c_char,
) -> *mut IntelEngineCaptureVma {
    if vma_res.is_null() {
        return next;
    }
    let c = kmalloc_obj::<IntelEngineCaptureVma>(gfp);
    if c.is_null() {
        return next;
    }
    if !crate::i915_vma_resource_upstream::i915_vma_resource_hold(vma_res, &mut (*c).lockdep_cookie) {
        kfree(c);
        return next;
    }
    strscpy((*c).name.as_mut_ptr(), name, 16);
    (*c).vma_res = crate::i915_vma_upstream::i915_vma_resource_get(vma_res);
    (*c).next = next;
    c
}

unsafe fn capture_vma(
    next: *mut IntelEngineCaptureVma,
    vma: *mut crate::i915_vma_types_upstream::I915Vma,
    name: *const c_char,
    gfp: u32,
) -> *mut IntelEngineCaptureVma {
    if vma.is_null() {
        return next;
    }
    if !crate::i915_vma_api_upstream::i915_vma_is_pinned(vma) {
        axlog::warn!("GEM_WARN_ON: capture_vma on unpinned vma");
        return next;
    }
    capture_vma_snapshot(next, (*vma).resource, gfp, name)
}

unsafe fn capture_user(
    mut capture: *mut IntelEngineCaptureVma,
    rq: *const crate::i915_request_types_upstream::I915Request,
    gfp: u32,
) -> *mut IntelEngineCaptureVma {
    let mut c = (*rq).capture_list;
    while !c.is_null() {
        capture = capture_vma_snapshot(capture, (*c).vma_res, gfp, c"user".as_ptr());
        c = (*c).next;
    }
    capture
}

// upstream: i915_gpu_error.c engine_coredump_add_context()
unsafe fn engine_coredump_add_context(
    ee: *mut IntelEngineCoredump,
    ce: *mut crate::intel_context_types_upstream::IntelContext,
    gfp: u32,
) -> *mut IntelEngineCaptureVma {
    let mut vma: *mut IntelEngineCaptureVma = core::ptr::null_mut();
    (*ee).simulated |= record_context(core::ptr::addr_of_mut!((*ee).context), ce);
    if (*ee).simulated {
        return core::ptr::null_mut();
    }
    vma = capture_vma(vma, (*(*ce).ring).vma, c"ring".as_ptr(), gfp);
    vma = capture_vma(vma, (*ce).state, c"HW context".as_ptr(), gfp);
    vma
}

/// `intel_engine_coredump_alloc()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_engine_coredump_alloc(
    engine: *mut IntelEngineCs,
    gfp: u32,
    dump_flags: u32,
) -> *mut IntelEngineCoredump {
    let ee = kmalloc_obj::<IntelEngineCoredump>(gfp | GFP_ZERO);
    if ee.is_null() {
        return core::ptr::null_mut();
    }
    (*ee).engine = engine;
    if dump_flags & crate::linux::registers::CORE_DUMP_FLAG_IS_GUC_CAPTURE == 0 {
        engine_record_registers(ee);
        engine_record_execlists(ee);
    }
    ee
}

/// `intel_engine_coredump_add_request()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_engine_coredump_add_request(
    ee: *mut IntelEngineCoredump,
    rq: *mut crate::i915_request_types_upstream::I915Request,
    gfp: u32,
) -> *mut IntelEngineCaptureVma {
    let vma = engine_coredump_add_context(ee, (*rq).context, gfp);
    if vma.is_null() {
        return core::ptr::null_mut();
    }
    let vma = capture_vma_snapshot(vma, (*rq).batch_res, gfp, c"batch".as_ptr());
    let vma = capture_user(vma, rq, gfp);
    (*ee).rq_head = (*rq).head;
    (*ee).rq_post = (*rq).postfix;
    (*ee).rq_tail = (*rq).tail;
    vma
}

/// `intel_engine_coredump_add_vma()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_engine_coredump_add_vma(
    ee: *mut IntelEngineCoredump,
    mut capture: *mut IntelEngineCaptureVma,
    compress: *mut I915VmaCompress,
) {
    let engine = (*ee).engine;
    let gt = (*engine).gt;
    while !capture.is_null() {
        let this = capture;
        let vma_res = (*this).vma_res;
        add_vma(
            ee,
            i915_vma_coredump_create(gt, vma_res, compress, (*this).name.as_ptr()),
        );
        crate::i915_vma_resource_upstream::i915_vma_resource_unhold(vma_res, (*this).lockdep_cookie);
        crate::i915_vma_resource_upstream::i915_vma_resource_put(vma_res);
        capture = (*this).next;
        kfree(this);
    }
    add_vma_coredump(ee, gt, (*engine).status_page.vma, c"HW Status".as_ptr(), compress);
    add_vma_coredump(ee, gt, (*engine).wa_ctx.vma, c"WA context".as_ptr(), compress);
}

/// `i915_vma_capture_prepare()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_capture_prepare(_gt: *mut IntelGtCoredump) -> *mut I915VmaCompress {
    let compress = kmalloc_obj::<I915VmaCompress>(ALLOW_FAIL);
    if compress.is_null() {
        return core::ptr::null_mut();
    }
    if !compress_init(compress) {
        kfree(compress);
        return core::ptr::null_mut();
    }
    compress
}

/// `i915_vma_capture_finish()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_capture_finish(
    _gt: *mut IntelGtCoredump,
    compress: *mut I915VmaCompress,
) {
    if compress.is_null() {
        return;
    }
    compress_fini(compress);
    kfree(compress);
}

// --- Register capture.

unsafe fn engine_record_registers(ee: *mut IntelEngineCoredump) {
    use crate::intel_engine_regs_upstream::*;
    let engine = (*ee).engine;
    let i915 = (*engine).i915;
    let ver = crate::linux::i915::GRAPHICS_VER(i915);

    if ver >= 6 {
        (*ee).rc_psmi = ENGINE_READ!(engine, RING_PSMI_CTL);
        if crate::linux::i915::MEDIA_VER(i915) >= 13 && (*(*engine).gt).type_ == crate::intel_gt_types_upstream::GT_MEDIA {
            (*ee).fault_reg = crate::intel_uncore_types_upstream::intel_uncore_read((*engine).uncore, XELPMP_RING_FAULT_REG);
        } else if crate::linux::i915::GRAPHICS_VER_FULL(i915) >= crate::linux::i915::IP_VER(12, 55) {
            (*ee).fault_reg = crate::intel_gt_mcr_upstream::intel_gt_mcr_read_any((*engine).gt, XEHP_RING_FAULT_REG);
        } else if ver >= 12 {
            (*ee).fault_reg = crate::intel_uncore_types_upstream::intel_uncore_read((*engine).uncore, GEN12_RING_FAULT_REG);
        } else if ver >= 8 {
            (*ee).fault_reg = crate::intel_uncore_types_upstream::intel_uncore_read((*engine).uncore, GEN8_RING_FAULT_REG);
        } else {
            // gen6/7 engine fault register: TheKernel targets ADL-N (gen12+) only.
            panic!("intel_engine_coredump: gen6/7 RING_FAULT_REG 不受 TheKernel 支持");
        }
    }

    if ver >= 4 {
        (*ee).esr = ENGINE_READ!(engine, RING_ESR);
        (*ee).faddr = ENGINE_READ!(engine, RING_DMA_FADD) as u64;
        (*ee).ipeir = ENGINE_READ!(engine, RING_IPEIR);
        (*ee).ipehr = ENGINE_READ!(engine, RING_IPEHR);
        (*ee).instps = ENGINE_READ!(engine, RING_INSTPS);
        (*ee).bbaddr = ENGINE_READ!(engine, RING_BBADDR) as u64;
        (*ee).ccid = ENGINE_READ!(engine, CCID);
        if ver >= 8 {
            (*ee).faddr |= (ENGINE_READ!(engine, RING_DMA_FADD_UDW) as u64) << 32;
            (*ee).bbaddr |= (ENGINE_READ!(engine, RING_BBADDR_UDW) as u64) << 32;
        }
        (*ee).bbstate = ENGINE_READ!(engine, RING_BBSTATE);
    } else {
        (*ee).faddr = ENGINE_READ!(engine, DMA_FADD_I8XX) as u64;
        (*ee).ipeir = ENGINE_READ!(engine, IPEIR);
        (*ee).ipehr = ENGINE_READ!(engine, IPEHR);
    }

    if ver >= 11 {
        (*ee).cmd_cctl = ENGINE_READ!(engine, RING_CMD_CCTL);
        (*ee).cscmdop = ENGINE_READ!(engine, RING_CSCMDOP);
        (*ee).ctx_sr_ctl = ENGINE_READ!(engine, RING_CTX_SR_CTL);
        (*ee).dma_faddr_hi = ENGINE_READ!(engine, RING_DMA_FADD_UDW);
        (*ee).dma_faddr_lo = ENGINE_READ!(engine, RING_DMA_FADD);
        (*ee).nopid = ENGINE_READ!(engine, RING_NOPID);
        (*ee).excc = ENGINE_READ!(engine, RING_EXCC);
    }

    crate::intel_engine_cs_upstream::intel_engine_get_instdone(engine, core::ptr::addr_of_mut!((*ee).instdone));

    (*ee).instpm = ENGINE_READ!(engine, RING_INSTPM);
    (*ee).acthd = crate::intel_engine_cs_upstream::intel_engine_get_active_head(engine);
    (*ee).start = ENGINE_READ!(engine, RING_START);
    (*ee).head = ENGINE_READ!(engine, RING_HEAD);
    (*ee).tail = ENGINE_READ!(engine, RING_TAIL);
    (*ee).ctl = ENGINE_READ!(engine, RING_CTL);
    if ver > 2 {
        (*ee).mode = ENGINE_READ!(engine, RING_MI_MODE);
    }

    if !crate::linux::i915::HWS_NEEDS_PHYSICAL(i915) {
        let mmio = if ver == 7 {
            match (*engine).id {
                crate::intel_engine_types_upstream::RCS0 => RENDER_HWS_PGA_GEN7,
                crate::intel_engine_types_upstream::BCS0 => BLT_HWS_PGA_GEN7,
                crate::intel_engine_types_upstream::VCS0 => BSD_HWS_PGA_GEN7,
                crate::intel_engine_types_upstream::VECS0 => VEBOX_HWS_PGA_GEN7,
                // Upstream MISSING_CASE(engine->id) falls through to RCS0.
                _ => RENDER_HWS_PGA_GEN7,
            }
        } else if crate::linux::i915::GRAPHICS_VER((*engine).i915) == 6 {
            RING_HWS_PGA_GEN6((*engine).mmio_base)
        } else {
            RING_HWS_PGA((*engine).mmio_base)
        };
        (*ee).hws = crate::intel_uncore_types_upstream::intel_uncore_read((*engine).uncore, mmio);
    }

    (*ee).reset_count = crate::linux::i915_private::i915_reset_engine_count(
        core::ptr::addr_of!((*i915).gpu_error),
        engine,
    );

    if crate::intel_ggtt_upstream::has_ppgtt(i915) {
        if ver == 6 {
            (*ee).vm_info.gfx_mode = ENGINE_READ!(engine, RING_MODE_GEN7);
            (*ee).vm_info.pdp[0] = ENGINE_READ!(engine, RING_PP_DIR_BASE_READ) as u64;
        } else if ver == 7 {
            (*ee).vm_info.gfx_mode = ENGINE_READ!(engine, RING_MODE_GEN7);
            (*ee).vm_info.pdp[0] = ENGINE_READ!(engine, RING_PP_DIR_BASE) as u64;
        } else if ver >= 8 {
            (*ee).vm_info.gfx_mode = ENGINE_READ!(engine, RING_MODE_GEN7);
            let base = (*engine).mmio_base;
            for i in 0..4u32 {
                let hi = crate::intel_uncore_types_upstream::intel_uncore_read((*engine).uncore, GEN8_RING_PDP_UDW(base, i));
                let lo = crate::intel_uncore_types_upstream::intel_uncore_read((*engine).uncore, GEN8_RING_PDP_LDW(base, i));
                (*ee).vm_info.pdp[i as usize] = ((hi as u64) << 32) | lo as u64;
            }
        }
    }
}

// upstream: i915_gpu_error.c gt_record_fences()
unsafe fn gt_record_fences(gt: *mut IntelGtCoredump) {
    let gt_ = (*gt)._gt;
    let ggtt = (*gt_).ggtt;
    let uncore = (*gt_).uncore;
    let ver = crate::linux::i915::GRAPHICS_VER((*uncore).i915);
    let mut i = 0u32;
    if ver >= 6 {
        while i < (*ggtt).num_fences {
            (*gt).fence[i as usize] = crate::intel_uncore_types_upstream::intel_uncore_read64(uncore, FENCE_REG_GEN6_LO(i));
            i += 1;
        }
    } else if ver >= 4 {
        while i < (*ggtt).num_fences {
            (*gt).fence[i as usize] = crate::intel_uncore_types_upstream::intel_uncore_read64(uncore, FENCE_REG_965_LO(i));
            i += 1;
        }
    } else {
        while i < (*ggtt).num_fences {
            (*gt).fence[i as usize] = crate::intel_uncore_types_upstream::intel_uncore_read(uncore, FENCE_REG(i)) as u64;
            i += 1;
        }
    }
    (*gt).nfence = i;
}

// upstream: i915_gpu_error.c gt_record_global_nonguc_regs()
unsafe fn gt_record_global_nonguc_regs(gt: *mut IntelGtCoredump) {
    use crate::intel_uncore_types_upstream::intel_uncore_read as rd;
    let uncore = (*(*gt)._gt).uncore;
    let i915 = (*uncore).i915;
    if crate::linux::i915::IS_VALLEYVIEW(i915) {
        (*gt).gtier[0] = rd(uncore, GTIER);
        (*gt).ngtier = 1;
    } else if crate::linux::i915::GRAPHICS_VER(i915) >= 11 {
        (*gt).gtier[0] = rd(uncore, GEN11_RENDER_COPY_INTR_ENABLE);
        (*gt).gtier[1] = rd(uncore, GEN11_VCS_VECS_INTR_ENABLE);
        (*gt).gtier[2] = rd(uncore, GEN11_GUC_SG_INTR_ENABLE);
        (*gt).gtier[3] = rd(uncore, GEN11_GPM_WGBOXPERF_INTR_ENABLE);
        (*gt).gtier[4] = rd(uncore, GEN11_CRYPTO_RSVD_INTR_ENABLE);
        (*gt).gtier[5] = rd(uncore, GEN11_GUNIT_CSME_INTR_ENABLE);
        (*gt).ngtier = 6;
    } else if crate::linux::i915::GRAPHICS_VER(i915) >= 8 {
        for i in 0..4u32 {
            (*gt).gtier[i as usize] = rd(uncore, GEN8_GT_IER(i));
        }
        (*gt).ngtier = 4;
    } else if crate::linux::i915::GRAPHICS_VER(i915) >= 5 {
        (*gt).gtier[0] = rd(uncore, GTIER);
        (*gt).ngtier = 1;
    } else {
        (*gt).gtier[0] = rd(uncore, GEN2_IER);
        (*gt).ngtier = 1;
    }
    (*gt).eir = rd(uncore, EIR);
    (*gt).pgtbl_er = rd(uncore, PGTBL_ER);
}

// upstream: i915_gpu_error.c gt_record_global_regs()
unsafe fn gt_record_global_regs(gt: *mut IntelGtCoredump) {
    use crate::intel_uncore_types_upstream::{intel_uncore_read as rd, intel_uncore_read_fw as rdfw};
    let gt_ = (*gt)._gt;
    let uncore = (*gt_).uncore;
    let i915 = (*uncore).i915;
    let ver = crate::linux::i915::GRAPHICS_VER(i915);

    if crate::linux::i915::IS_VALLEYVIEW(i915) {
        (*gt).forcewake = rdfw(uncore, FORCEWAKE_VLV);
    }
    if crate::linux::i915::GRAPHICS_VER_FULL(i915) >= crate::linux::i915::IP_VER(12, 55) {
        (*gt).fault_data0 = crate::intel_gt_mcr_upstream::intel_gt_mcr_read_any(gt_ as *mut IntelGt, XEHP_FAULT_TLB_DATA0);
        (*gt).fault_data1 = crate::intel_gt_mcr_upstream::intel_gt_mcr_read_any(gt_ as *mut IntelGt, XEHP_FAULT_TLB_DATA1);
    } else if ver >= 12 {
        (*gt).fault_data0 = rd(uncore, GEN12_FAULT_TLB_DATA0);
        (*gt).fault_data1 = rd(uncore, GEN12_FAULT_TLB_DATA1);
    } else if ver >= 8 {
        (*gt).fault_data0 = rd(uncore, GEN8_FAULT_TLB_DATA0);
        (*gt).fault_data1 = rd(uncore, GEN8_FAULT_TLB_DATA1);
    }
    if ver == 6 {
        (*gt).forcewake = rdfw(uncore, FORCEWAKE);
        (*gt).gab_ctl = rd(uncore, GAB_CTL);
        (*gt).gfx_mode = rd(uncore, GFX_MODE);
    }
    if ver >= 7 {
        (*gt).forcewake = rdfw(uncore, FORCEWAKE_MT);
    }
    if ver >= 6 && ver < 12 {
        (*gt).error = rd(uncore, ERROR_GEN6);
        (*gt).done_reg = rd(uncore, DONE_REG);
    }
    if crate::linux::i915::IS_GRAPHICS_VER(i915, 6, 7) {
        (*gt).gam_ecochk = rd(uncore, GAM_ECOCHK);
        (*gt).gac_eco = rd(uncore, GAC_ECO_BITS);
    }
    if crate::linux::i915::IS_GRAPHICS_VER(i915, 8, 11) {
        (*gt).gtt_cache = rd(uncore, HSW_GTT_CACHE_EN);
    }
    if ver == 12 {
        (*gt).aux_err = rd(uncore, GEN12_AUX_ERR_DBG);
    }
    if ver >= 12 {
        for i in 0..I915_MAX_SFC {
            // SFC_DONE lives in the VD forcewake domain; only present with the VCS engine.
            if (*gt_).info.sfc_mask & (1 << i) == 0
                || !crate::linux::i915::HAS_ENGINE(gt_ as *mut IntelGt, crate::intel_engine_types_upstream::_VCS(i as i32 * 2))
            {
                continue;
            }
            (*gt).sfc_done[i] = rd(uncore, GEN12_SFC_DONE(i as u32));
        }
        (*gt).gam_done = rd(uncore, GEN12_GAM_DONE);
    }
}

// upstream: i915_gpu_error.c gt_record_info()
unsafe fn gt_record_info(gt: *mut IntelGtCoredump) {
    (*gt).info = (*(*gt)._gt).info;
    (*gt).clock_frequency = (*(*gt)._gt).clock_frequency;
    (*gt).clock_period_ns = (*(*gt)._gt).clock_period_ns;
}

// upstream: i915_gpu_error.c capture_gen()
unsafe fn capture_gen(error: *mut I915GpuCoredump) {
    let i915 = (*error).i915;
    (*error).wakelock = crate::i915_driver_upstream::atomic_read_rpm_wakeref(core::ptr::addr_of_mut!((*i915).runtime_pm).cast()) != 0;
    (*error).suspended = crate::linux::kernel_services::pm_runtime_suspended((*i915).drm.dev);
    (*error).iommu = crate::i915_utils_upstream::i915_vtd_active(i915) as i32;
    (*error).reset_count = crate::gt_header_inline_upstream::i915_reset_count(core::ptr::addr_of!((*i915).gpu_error));
    (*error).suspend_count = (*i915).suspend_count;
    crate::i915_probe_provider_upstream::i915_params_copy(core::ptr::addr_of_mut!((*error).params));
    core::ptr::copy_nonoverlapping(
        crate::linux::i915::INTEL_INFO(i915).cast::<u8>(),
        core::ptr::addr_of_mut!((*error).device_info).cast::<u8>(),
        size_of::<IntelDeviceInfo>(),
    );
    core::ptr::copy_nonoverlapping(
        crate::linux::i915::runtime_info(i915).cast::<u8>(),
        core::ptr::addr_of_mut!((*error).runtime_info).cast::<u8>(),
        size_of::<crate::linux::i915::IntelRuntimeInfo>(),
    );
    // intel_driver_caps immediately follows __runtime (see i915_getparam_upstream).
    core::ptr::copy_nonoverlapping(
        i915.cast::<u8>().add(offset_of!(DrmI915Private, runtime) + size_of::<crate::linux::i915::IntelRuntimeInfo>()),
        core::ptr::addr_of_mut!((*error).driver_caps).cast::<u8>(),
        size_of::<IntelDriverCaps>(),
    );
}

// upstream: i915_gpu_error.c gt_record_guc_ctb()
unsafe fn gt_record_guc_ctb(
    saved: *mut IntelCtbCoredump,
    ctb: *const crate::intel_guc_ct_types_upstream::IntelGucCtBuffer,
    blob_ptr: *const c_void,
) {
    if ctb.is_null() || (*ctb).desc.is_null() {
        return;
    }
    (*saved).raw_status = (*(*ctb).desc.cast::<CtbDescLayout>()).status;
    (*saved).raw_head = (*(*ctb).desc.cast::<CtbDescLayout>()).head;
    (*saved).raw_tail = (*(*ctb).desc.cast::<CtbDescLayout>()).tail;
    (*saved).head = (*ctb).head;
    (*saved).tail = (*ctb).tail;
    (*saved).size = (*ctb).size;
    (*saved).desc_offset = ((*ctb).desc as *const u8).offset_from(blob_ptr.cast()) as u32;
    (*saved).cmds_offset = ((*ctb).cmds as *const u8).offset_from(blob_ptr.cast()) as u32;
}

/// upstream `guc_hw_reg_state[]`: GuC registers useful for debugging hangs.
const GUC_HW_REG_STATE: [(u32, u32); 15] = [
    (0xc0b0, 2), (0xc000, 65), (0xc140, 1), (0xc180, 16), (0xc1dc, 10),
    (0xc300, 79), (0xc4b4, 47), (0xc574, 1), (0xc57c, 1), (0xc584, 11),
    (0xc5c0, 8), (0xc5e4, 1), (0xc5ec, 103), (0xc7c0, 1), (0xc0b0, 2),
];

// upstream: i915_gpu_error.c gt_record_guc_hw_state()
unsafe fn gt_record_guc_hw_state(uncore: *mut crate::intel_uncore_types_upstream::IntelUncore, error_uc: *mut IntelUcCoredump) {
    let total: u32 = GUC_HW_REG_STATE.iter().map(|e| e.1).sum();
    let hw_state = kmalloc(total as usize * size_of::<u32>(), ALLOW_FAIL) as *mut u32;
    if hw_state.is_null() {
        return;
    }
    let mut count = 0usize;
    for &(start, n) in GUC_HW_REG_STATE.iter() {
        for j in 0..n {
            *hw_state.add(count) = crate::intel_uncore_types_upstream::intel_uncore_read(
                uncore,
                mmio(start + j * 4),
            );
            count += 1;
        }
    }
    (*error_uc).guc.hw_state = hw_state;
}

// upstream: i915_gpu_error.c gt_record_uc()
unsafe fn gt_record_uc(gt: *mut IntelGtCoredump, compress: *mut I915VmaCompress) -> *mut IntelUcCoredump {
    let uc = core::ptr::addr_of!((*(*gt)._gt).uc);
    let error_uc = zalloc_obj::<IntelUcCoredump>(ALLOW_FAIL);
    if error_uc.is_null() {
        return core::ptr::null_mut();
    }
    core::ptr::copy_nonoverlapping(core::ptr::addr_of!((*uc).guc.fw), core::ptr::addr_of_mut!((*error_uc).guc_fw), 1);
    core::ptr::copy_nonoverlapping(core::ptr::addr_of!((*uc).huc.fw), core::ptr::addr_of_mut!((*error_uc).huc_fw), 1);
    (*error_uc).guc_fw.file_selected.path = kstrdup_c((*uc).guc.fw.file_selected.path);
    (*error_uc).huc_fw.file_selected.path = kstrdup_c((*uc).huc.fw.file_selected.path);
    (*error_uc).guc_fw.file_wanted.path = kstrdup_c((*uc).guc.fw.file_wanted.path);
    (*error_uc).huc_fw.file_wanted.path = kstrdup_c((*uc).huc.fw.file_wanted.path);

    // Timestamp reference for converting GuC log times to system times.
    (*error_uc).guc.timestamp = crate::intel_uncore_types_upstream::intel_uncore_read(
        (*(*gt)._gt).uncore,
        GUCPMTIMESTAMP,
    );
    (*error_uc).guc.vma_log = create_vma_coredump((*gt)._gt, (*uc).guc.log.vma, c"GuC log buffer".as_ptr(), compress);
    (*error_uc).guc.vma_ctb = create_vma_coredump((*gt)._gt, (*uc).guc.ct.vma, c"GuC CT buffer".as_ptr(), compress);
    (*error_uc).guc.last_fence = (*uc).guc.ct.requests.last_fence;
    let blob = (*uc).guc.ct.ctbs.send.desc as *const c_void;
    gt_record_guc_ctb(core::ptr::addr_of_mut!((*error_uc).guc.ctb[0]), core::ptr::addr_of!((*uc).guc.ct.ctbs.send), blob);
    gt_record_guc_ctb(core::ptr::addr_of_mut!((*error_uc).guc.ctb[1]), core::ptr::addr_of!((*uc).guc.ct.ctbs.recv), blob);
    gt_record_guc_hw_state((*(*gt)._gt).uncore, error_uc);
    error_uc
}

/// `intel_gt_coredump_alloc()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_coredump_alloc(
    gt: *mut IntelGt,
    gfp: u32,
    dump_flags: u32,
) -> *mut IntelGtCoredump {
    let gc = zalloc_obj::<IntelGtCoredump>(gfp);
    if gc.is_null() {
        return core::ptr::null_mut();
    }
    (*gc)._gt = gt;
    (*gc).awake = crate::linux::pm::intel_gt_pm_is_awake(gt);
    gt_record_global_nonguc_regs(gc);
    // GuC captures the global/eng-class registers itself when it triggered the reset.
    if dump_flags & crate::linux::registers::CORE_DUMP_FLAG_IS_GUC_CAPTURE == 0 {
        gt_record_global_regs(gc);
    }
    gt_record_fences(gc);
    gc
}

/// `i915_gpu_coredump_alloc()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gpu_coredump_alloc(i915: *mut DrmI915Private, gfp: u32) -> *mut I915GpuCoredump {
    if !(*i915).params.error_capture {
        return core::ptr::null_mut();
    }
    let error = zalloc_obj::<I915GpuCoredump>(gfp);
    if error.is_null() {
        return core::ptr::null_mut();
    }
    kref_init(core::ptr::addr_of_mut!((*error).ref_));
    (*error).i915 = i915;
    let now = ktime_get();
    (*error).time = now;
    (*error).boottime = now;
    (*error).uptime = now - (*crate::linux::i915::to_gt(i915)).last_init_time;
    (*error).capture = jiffies();
    capture_gen(error);
    error
}

// upstream: i915_gpu_error.c capture_engine()
unsafe fn capture_engine(
    engine: *mut IntelEngineCs,
    compress: *mut I915VmaCompress,
    dump_flags: u32,
) -> *mut IntelEngineCoredump {
    let mut capture: *mut IntelEngineCaptureVma = core::ptr::null_mut();
    let mut ce: *mut crate::intel_context_types_upstream::IntelContext = core::ptr::null_mut();
    let mut rq: *mut crate::i915_request_types_upstream::I915Request = core::ptr::null_mut();

    let ee = intel_engine_coredump_alloc(engine, ALLOW_FAIL, dump_flags);
    if ee.is_null() {
        return core::ptr::null_mut();
    }
    crate::intel_engine_cs_upstream::intel_engine_get_hung_entity(engine, &mut ce, &mut rq);
    if !rq.is_null() && !crate::linux::requests::i915_request_started(rq) {
        axlog::info!(
            "Got hung context on {} with active request not yet started",
            cstr((*engine).name.as_ptr())
        );
    }
    if !rq.is_null() {
        capture = intel_engine_coredump_add_request(ee, rq, crate::linux::config::GFP_ATOMIC);
        crate::gt_header_inline_upstream::i915_request_put(rq);
    } else if !ce.is_null() {
        capture = engine_coredump_add_context(ee, ce, crate::linux::config::GFP_ATOMIC);
    }

    if !capture.is_null() {
        intel_engine_coredump_add_vma(ee, capture, compress);
        if dump_flags & crate::linux::registers::CORE_DUMP_FLAG_IS_GUC_CAPTURE != 0 {
            intel_guc_capture_get_matching_node((*engine).gt, ee, ce);
        }
    } else {
        kfree(ee);
        return core::ptr::null_mut();
    }
    ee
}

// upstream: i915_gpu_error.c gt_record_engines()
unsafe fn gt_record_engines(
    gt: *mut IntelGtCoredump,
    engine_mask: u32,
    compress: *mut I915VmaCompress,
    dump_flags: u32,
) {
    for engine in crate::linux::i915::for_each_engine((*gt)._gt as *mut IntelGt) {
        gt_record_engines_refill(compress);
        let ee = capture_engine(engine as *const IntelEngineCs as *mut IntelEngineCs, compress, dump_flags);
        if ee.is_null() {
            continue;
        }
        (*ee).hung = (*(engine as *const IntelEngineCs)).mask & engine_mask != 0;
        (*gt).simulated |= (*ee).simulated;
        if (*ee).simulated {
            if dump_flags & crate::linux::registers::CORE_DUMP_FLAG_IS_GUC_CAPTURE != 0 {
                intel_guc_capture_free_node(ee);
            }
            kfree(ee);
            continue;
        }
        (*ee).next = (*gt).engine;
        (*gt).engine = ee;
    }
}

// upstream: i915_gpu_error.c __i915_gpu_coredump()
unsafe fn __i915_gpu_coredump(
    gt: *mut IntelGt,
    engine_mask: u32,
    dump_flags: u32,
) -> *mut I915GpuCoredump {
    let i915 = (*gt).i915;
    let first = (*i915).gpu_error.first_error.cast::<I915GpuCoredump>();
    if is_err(first) {
        return first;
    }
    let error = i915_gpu_coredump_alloc(i915, ALLOW_FAIL);
    if error.is_null() {
        return err_ptr(-(ENOMEM as isize));
    }
    (*error).gt = intel_gt_coredump_alloc(gt, ALLOW_FAIL, dump_flags);
    if !(*error).gt.is_null() {
        let compress = i915_vma_capture_prepare((*error).gt);
        if compress.is_null() {
            kfree((*error).gt);
            kfree(error);
            return err_ptr(-(ENOMEM as isize));
        }
        if crate::intel_uc_fw_upstream::has_gt_uc(i915) {
            (*(*error).gt).uc = gt_record_uc((*error).gt, compress);
            if !(*(*error).gt).uc.is_null() {
                if dump_flags & crate::linux::registers::CORE_DUMP_FLAG_IS_GUC_CAPTURE != 0 {
                    (*(*(*error).gt).uc).guc.is_guc_capture = true;
                } else {
                    assert!(!(*(*(*error).gt).uc).guc.is_guc_capture);
                }
            }
        }
        gt_record_info((*error).gt);
        gt_record_engines((*error).gt, engine_mask, compress, dump_flags);
        i915_vma_capture_finish((*error).gt, compress);
        (*error).simulated |= (*(*error).gt).simulated;
    }
    // display_snapshot stays NULL (see the field comment).
    error
}

static CAPTURE_LOCK: spin::Mutex<()> = spin::Mutex::new(());

// upstream: i915_gpu_error.c i915_gpu_coredump()
unsafe fn i915_gpu_coredump(
    gt: *mut IntelGt,
    engine_mask: u32,
    dump_flags: u32,
) -> *mut I915GpuCoredump {
    let guard = CAPTURE_LOCK.lock();
    let dump = __i915_gpu_coredump(gt, engine_mask, dump_flags);
    drop(guard);
    dump
}

unsafe fn i915_gpu_coredump_get(error: *mut I915GpuCoredump) {
    kref_get(core::ptr::addr_of_mut!((*error).ref_));
}

unsafe fn i915_gpu_coredump_put(error: *mut I915GpuCoredump) {
    if error.is_null() || is_err(error) {
        return;
    }
    kref_put(core::ptr::addr_of_mut!((*error).ref_), __i915_gpu_coredump_free_raw);
}

unsafe extern "C" fn __i915_gpu_coredump_free_raw(kref: *mut crate::intel_context_upstream::Kref) {
    __i915_gpu_coredump_free(kref);
}

// upstream: i915_gpu_error.c error_msg()
unsafe fn error_msg(error: *mut I915GpuCoredump) {
    let mut first: *mut IntelEngineCoredump = core::ptr::null_mut();
    let mut hung_classes: u32 = 0;
    let mut gt = (*error).gt;
    while !gt.is_null() {
        let mut cs = (*gt).engine;
        while !cs.is_null() {
            if (*cs).hung {
                hung_classes |= 1 << (*(*cs).engine).uabi_class;
                if first.is_null() {
                    first = cs;
                }
            }
            cs = (*cs).next;
        }
        gt = (*gt).next;
    }
    let ecode = if first.is_null() { 0 } else { (*first).ipehr ^ (*first).instdone.instdone };
    let gfx_ver = crate::linux::i915::GRAPHICS_VER((*error).i915);
    axlog::info!("GPU HANG: ecode {}:{:x}:{:08x}", gfx_ver, hung_classes, ecode);
    if !first.is_null() && (*first).context.pid != 0 {
        axlog::info!("in {} [{}]", cstr((*first).context.comm.as_ptr()), (*first).context.pid);
    }
}

// upstream: i915_gpu_error.c i915_error_state_store()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_error_state_store(error: *mut I915GpuCoredump) {
    if error.is_null() || is_err(error) {
        return;
    }
    let i915 = (*error).i915;
    error_msg(error);
    if (*error).simulated {
        return;
    }
    let cur = (*i915).gpu_error.first_error;
    if !cur.is_null() {
        return;
    }
    (*i915).gpu_error.first_error = error.cast();
    i915_gpu_coredump_get(error);
    axlog::info!("GPU error state saved");
}

// upstream: i915_gpu_error.c i915_capture_error_state()
pub unsafe fn i915_capture_error_state(gt: *mut IntelGt, engine_mask: u32, dump_flags: u32) {
    let error = i915_gpu_coredump(gt, engine_mask, dump_flags);
    if is_err(error) {
        if (*(*gt).i915).gpu_error.first_error.is_null() {
            (*(*gt).i915).gpu_error.first_error = error.cast();
        }
        return;
    }
    i915_error_state_store(error);
    i915_gpu_coredump_put(error);
}

// upstream: i915_gpu_error.c i915_reset_error_state()
pub unsafe fn i915_reset_error_state(i915: *mut DrmI915Private) {
    crate::linux::locks::spin_lock(&mut (*i915).gpu_error.lock);
    let error = (*i915).gpu_error.first_error.cast::<I915GpuCoredump>();
    if error as isize != -(crate::linux_config::ENODEV as isize) {
        (*i915).gpu_error.first_error = core::ptr::null_mut();
    }
    crate::linux::locks::spin_unlock(&mut (*i915).gpu_error.lock);
    if !error.is_null() && !is_err(error) {
        i915_gpu_coredump_put(error);
    }
}

// upstream: i915_gpu_error.c i915_disable_error_state()
pub unsafe fn i915_disable_error_state(i915: *mut DrmI915Private, err: i32) {
    crate::linux::locks::spin_lock(&mut (*i915).gpu_error.lock);
    if (*i915).gpu_error.first_error.is_null() {
        (*i915).gpu_error.first_error = err_ptr::<c_void>(err as isize);
    }
    crate::linux::locks::spin_unlock(&mut (*i915).gpu_error.lock);
}

/// `IS_ERR()` on a coredump pointer: the top 4095 addresses encode errno.
fn is_err<T>(p: *mut T) -> bool {
    (p as usize) >= usize::MAX - 4094
}

fn err_ptr<T>(err: isize) -> *mut T {
    err as *mut T
}
