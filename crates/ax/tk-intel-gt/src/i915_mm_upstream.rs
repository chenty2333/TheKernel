// SPDX-License-Identifier: MIT
// Copyright © 2014 Intel Corporation.
//! Linux 7.2.3 drivers/gpu/drm/i915/i915_mm.c, all x86_64 remap functions.
//! Native page-table operations are LinuxKPI adapters, not copied Linux MM.
#![allow(unsafe_code, unsafe_op_in_unsafe_fn)]
use core::ffi::{c_int, c_ulong, c_void};

use crate::{
    i915_gem_pages_upstream::{Scatterlist, sg_next, sg_page},
    linux::{
        gem_memory::{IoMapping, PgProt},
        mm::{VM_DONTDUMP, VM_DONTEXPAND, VM_PFNMAP, VmAreaStruct},
        mm_native::{MmStruct, map_pfn, zap_range},
        page::page_to_pfn,
    },
    linux_config::{EINVAL, PAGE_SHIFT, PAGE_SIZE},
};
struct SgtIter {
    sgp: *mut Scatterlist,
    dma: u64,
    pfn: c_ulong,
    curr: c_ulong,
    max: c_ulong,
}
struct RemapPfn {
    mm: *mut MmStruct,
    pfn: c_ulong,
    prot: PgProt,
    sgt: SgtIter,
    iobase: u64,
}
const EXPECTED_FLAGS: c_ulong = VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP;
// Original adapter of the i915 scatterlist iterator header contract.
unsafe fn sgt_iter(sg: *mut Scatterlist, dma: bool) -> SgtIter {
    if sg.is_null() {
        return SgtIter {
            sgp: sg,
            dma: 0,
            pfn: 0,
            curr: 0,
            max: 0,
        };
    }
    SgtIter {
        sgp: sg,
        dma: (*sg).dma_address,
        pfn: page_to_pfn(sg_page(sg)),
        curr: if dma { 0 } else { (*sg).offset as c_ulong },
        max: if dma {
            (*sg).dma_length as c_ulong
        } else {
            ((*sg).offset + (*sg).length) as c_ulong
        },
    }
}
// upstream: i915_mm.c sgt_pfn()
unsafe fn sgt_pfn(r: &RemapPfn) -> c_ulong {
    if r.iobase != u64::MAX {
        ((r.sgt.dma + r.sgt.curr + r.iobase) >> PAGE_SHIFT) as c_ulong
    } else {
        r.sgt.pfn + (r.sgt.curr >> PAGE_SHIFT)
    }
}
// LinuxKPI apply_to_page_range: execute the source per-PTE callback, then map
// the resulting PFN through axmm. Each callback updates the insertion count
// before a later failure can require the source unwind.
unsafe fn apply_range(
    r: &mut RemapPfn,
    addr: c_ulong,
    size: c_ulong,
    callback: unsafe fn(&mut RemapPfn, c_ulong) -> c_int,
) -> c_int {
    let mut at = addr;
    let Some(end) = addr.checked_add(size) else {
        return -EINVAL;
    };
    while at < end {
        let ret = callback(r, at);
        if ret != 0 {
            return ret;
        }
        at += PAGE_SIZE as c_ulong;
    }
    0
}
// upstream: i915_mm.c remap_sg()
unsafe fn remap_sg(r: &mut RemapPfn, addr: c_ulong) -> c_int {
    if GEM_WARN_ON!(r.sgt.sgp.is_null()) {
        return -EINVAL;
    }
    let ret = map_pfn(r.mm, addr as usize, sgt_pfn(r) as usize, r.prot.pgprot);
    if ret != 0 {
        return ret;
    }
    r.pfn += 1;
    r.sgt.curr += PAGE_SIZE as c_ulong;
    if r.sgt.curr >= r.sgt.max {
        r.sgt = sgt_iter(sg_next(r.sgt.sgp), r.iobase != u64::MAX);
    }
    0
}
// upstream: i915_mm.c remap_pfn()
unsafe fn remap_pfn(r: &mut RemapPfn, addr: c_ulong) -> c_int {
    let ret = map_pfn(r.mm, addr as usize, r.pfn as usize, r.prot.pgprot);
    if ret != 0 {
        return ret;
    }
    r.pfn += 1;
    0
}
// upstream: i915_mm.c remap_io_mapping()
pub unsafe fn remap_io_mapping(
    vma: *mut VmAreaStruct,
    addr: c_ulong,
    pfn: c_ulong,
    size: c_ulong,
    iomap: *mut IoMapping,
) -> c_int {
    GEM_BUG_ON!((*vma).vm_flags & EXPECTED_FLAGS != EXPECTED_FLAGS);
    let mut r = RemapPfn {
        mm: (*vma).vm_mm.cast(),
        pfn,
        prot: PgProt {
            pgprot: ((*iomap).prot.pgprot & 0x98) | ((*vma).vm_page_prot.pgprot & !0x98),
        },
        sgt: sgt_iter(core::ptr::null_mut(), false),
        iobase: 0,
    };
    let err = apply_range(&mut r, addr, size, remap_pfn);
    if err != 0 {
        zap_range(r.mm, addr as usize, ((r.pfn - pfn) << PAGE_SHIFT) as usize);
        return err;
    }
    0
}
// upstream: i915_mm.c remap_io_sg()
pub unsafe fn remap_io_sg(
    vma: *mut VmAreaStruct,
    addr: c_ulong,
    size: c_ulong,
    sgl: *mut Scatterlist,
    mut offset: c_ulong,
    iobase: u64,
) -> c_int {
    let mut r = RemapPfn {
        mm: (*vma).vm_mm.cast(),
        pfn: 0,
        prot: (*vma).vm_page_prot,
        sgt: sgt_iter(sgl, iobase != u64::MAX),
        iobase,
    };
    GEM_BUG_ON!((*vma).vm_flags & EXPECTED_FLAGS != EXPECTED_FLAGS);
    while offset >= r.sgt.max >> PAGE_SHIFT {
        offset -= r.sgt.max >> PAGE_SHIFT;
        r.sgt = sgt_iter(sg_next(r.sgt.sgp), iobase != u64::MAX);
        if r.sgt.sgp.is_null() {
            return -EINVAL;
        }
    }
    r.sgt.curr = offset << PAGE_SHIFT;
    if iobase == u64::MAX {
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst); /* x86 flush_cache_range is an architectural no-op */
    }
    let err = apply_range(&mut r, addr, size, remap_sg);
    if err != 0 {
        zap_range(r.mm, addr as usize, (r.pfn << PAGE_SHIFT) as usize);
        return err;
    }
    0
}
