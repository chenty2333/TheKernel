// SPDX-License-Identifier: MIT
// Copyright © 2008-2012 Intel Corporation.
//
// Source-order Rust translation of Linux v7.2.3
// drivers/gpu/drm/i915/gem/i915_gem_stolen.c. The full MIT grant is in
// ../LICENSE-MIT. Resource allocation, PCI, DMA, DRM-MM, and MMIO operations
// remain calls to their actual LinuxKPI/source owners.
#![allow(
    unsafe_code,
    unsafe_op_in_unsafe_fn,
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code
)]

use core::{
    ffi::{c_char, c_int, c_ulong, c_void},
    mem::{offset_of, size_of},
    ptr,
};

use crate::{
    i915_gem_object_api_upstream::{
        i915_gem_object_pin_pages, i915_gem_object_trylock, i915_gem_object_unlock,
    },
    i915_gem_object_types_upstream::{
        DrmI915GemObject, DrmI915GemObjectOps, I915_BO_ALLOC_CONTIGUOUS, I915_BO_ALLOC_GPU_ONLY,
    },
    i915_gem_object_upstream::i915_gem_object_set_cache_coherency,
    i915_gem_pages_upstream::__i915_gem_object_set_pages,
    i915_gem_region_upstream::{
        IntelMemoryRegionOps, i915_gem_object_create_region, i915_gem_object_init_memory_region,
        i915_gem_object_release_memory_region,
    },
    i915_gem_userptr_upstream::drm_gem_private_object_init,
    i915_utils_upstream::i915_vtd_active,
    intel_context_upstream::{DrmMmNode, SgTable},
    intel_gt_mcr_upstream::intel_gt_mcr_read_any,
    intel_uncore_types_upstream::{
        IntelUncore, intel_uncore_read, intel_uncore_read16, intel_uncore_read64,
        intel_uncore_read64_2x32,
    },
    intel_workarounds_types_upstream::{I915McrRegT, I915RegT},
    linux::{
        gem_memory::{DrmMm, IntelMemoryRegion, Resource, drm_mm_node_allocated},
        i915::{
            GRAPHICS_VER, GRAPHICS_VER_FULL, HAS_LLC, HAS_LMEM, IP_VER, IS_BROXTON, IS_CHERRYVIEW,
            IS_DG1, IS_G4X, IS_GEMINILAKE, IS_GRAPHICS_STEP, IS_METEORLAKE, IS_PINEVIEW,
            IS_PLATFORM, IS_VALLEYVIEW, MEDIA_VER_FULL, STEP_A0, STEP_B0, to_gt, to_i915,
        },
        i915_private::DrmI915Private,
        memory::{kfree, kzalloc_obj},
        mutex::{mutex_init, mutex_lock, mutex_unlock},
    },
    linux_config::{self, EBUSY, EINVAL, ENODEV, ENOMEM, ENOSPC, ENXIO, GFP_KERNEL, PAGE_SIZE},
};

const I915_GEM_STOLEN_BIAS: u64 = 128 * 1024;
const DRM_MM_INSERT_BEST: c_int = 0;
const I915_GEM_DOMAIN_CPU: u16 = 1;
const I915_GEM_DOMAIN_GTT: u16 = 0x40;
const I915_CACHE_NONE: u32 = 0;
const I915_CACHE_LLC: u32 = 1;
const I915_GTT_PAGE_SIZE_4K: u64 = 1 << 12;
const I915_GTT_PAGE_SIZE_64K: u64 = 1 << 16;
const I915_BO_INVALID_OFFSET: u64 = u64::MAX;
const GEN6_STOLEN_RESERVED: I915RegT = I915RegT { reg: 0x1082c0 };
const CTG_STOLEN_RESERVED: I915RegT = I915RegT { reg: 0x10034 };
const ELK_STOLEN_RESERVED: I915RegT = I915RegT { reg: 0x10048 };
const GGC: I915RegT = I915RegT { reg: 0x108040 };
const GEN6_DSMBASE: I915RegT = I915RegT { reg: 0x1080c0 };
const MTL_GSCPSMI_BASEADDR_LSB: I915RegT = I915RegT { reg: 0x880c };
const MTL_GSCPSMI_BASEADDR_MSB: I915RegT = I915RegT { reg: 0x8810 };
const XEHP_TILE0_ADDR_RANGE: I915McrRegT = I915McrRegT { reg: 0x4900 };
const GEN6_STOLEN_RESERVED_ADDR_MASK: u32 = 0xfff << 20;
const GEN7_STOLEN_RESERVED_ADDR_MASK: u32 = 0x3fff << 18;
const GEN6_STOLEN_RESERVED_SIZE_MASK: u32 = 3 << 4;
const GEN6_STOLEN_RESERVED_1M: u32 = 0 << 4;
const GEN6_STOLEN_RESERVED_512K: u32 = 1 << 4;
const GEN6_STOLEN_RESERVED_256K: u32 = 2 << 4;
const GEN6_STOLEN_RESERVED_128K: u32 = 3 << 4;
const GEN7_STOLEN_RESERVED_SIZE_MASK: u32 = 1 << 5;
const GEN7_STOLEN_RESERVED_1M: u32 = 0 << 5;
const GEN7_STOLEN_RESERVED_256K: u32 = 1 << 5;
const GEN8_STOLEN_RESERVED_SIZE_MASK: u32 = 3 << 7;
const GEN8_STOLEN_RESERVED_1M: u32 = 0 << 7;
const GEN8_STOLEN_RESERVED_2M: u32 = 1 << 7;
const GEN8_STOLEN_RESERVED_4M: u32 = 2 << 7;
const GEN8_STOLEN_RESERVED_8M: u32 = 3 << 7;
const GEN6_STOLEN_RESERVED_ENABLE: u32 = 1;
const GEN11_STOLEN_RESERVED_ADDR_MASK: u64 = 0xffff_ffff_fff0_0000;
const G4X_STOLEN_RESERVED_ADDR1_MASK: u32 = 0xffff << 16;
const G4X_STOLEN_RESERVED_ADDR2_MASK: u32 = 0xfff << 4;
const G4X_STOLEN_RESERVED_ENABLE: u32 = 1;
const GEN11_BDSM_MASK: u64 = 0xffff_ffff_fff0_0000;
const GGMS_MASK: u16 = 3 << 6;
const GMS_MASK: u16 = 0xff << 8;
const GEN12_LMEM_BAR: usize = 2;
const XEHP_TILE_LMEM_RANGE_SHIFT: u32 = 8;
const I915_VMA_GLOBAL_BIND: u32 = 1 << 10;
const POISON_INUSE: u8 = 0x5a;
const POISON_FREE: u8 = 0x6b;
const SZ_1M: u64 = 1024 * 1024;
const SZ_8M: u64 = 8 * 1024 * 1024;
const SZ_256M: u64 = 256 * 1024 * 1024;
const SZ_1G: u64 = 1024 * 1024 * 1024;
const IORESOURCE_MEM: c_ulong = 0x200;
const DSM_OFFSET: usize = 1704;
const PCI_DEV_OFFSET: usize = 200;
const PCI_RESOURCE_OFFSET: usize = 968;

/// `drm_i915_private::dsm` source-layout view. The configured Linux 7.2.3
/// x86_64 C layout places this at byte 1704; the main i915 overlay leaves it
/// opaque because it is not needed by other owners.
#[repr(C)]
struct I915Dsm {
    stolen: Resource,
    reserved: Resource,
    usable_size: u64,
}
const _: [(); 136] = [(); size_of::<I915Dsm>()];
const _: [(); 0] = [(); offset_of!(I915Dsm, stolen)];
const _: [(); 64] = [(); offset_of!(I915Dsm, reserved)];
const _: [(); 128] = [(); offset_of!(I915Dsm, usable_size)];

#[repr(C)]
struct PciDevResources {
    _before_resources: [u8; PCI_RESOURCE_OFFSET],
    resource: [Resource; GEN12_LMEM_BAR + 1],
}
const _: [(); PCI_RESOURCE_OFFSET] = [(); offset_of!(PciDevResources, resource)];

#[repr(C)]
struct IntelStolenNode {
    i915: *mut DrmI915Private,
    node: DrmMmNode,
}

#[repr(C)]
pub struct IntelDisplayStolenInterface {
    insert_node_in_range:
        Option<unsafe extern "C" fn(*mut IntelStolenNode, u64, u32, u64, u64) -> c_int>,
    insert_node: Option<unsafe extern "C" fn(*mut IntelStolenNode, u64, u32) -> c_int>,
    remove_node: Option<unsafe extern "C" fn(*mut IntelStolenNode)>,
    initialized:
        Option<unsafe extern "C" fn(*mut crate::linux::i915_private::DrmDevicePrefix) -> bool>,
    node_allocated: Option<unsafe extern "C" fn(*const IntelStolenNode) -> bool>,
    node_offset: Option<unsafe extern "C" fn(*const IntelStolenNode) -> u64>,
    area_address:
        Option<unsafe extern "C" fn(*mut crate::linux::i915_private::DrmDevicePrefix) -> u64>,
    area_size:
        Option<unsafe extern "C" fn(*mut crate::linux::i915_private::DrmDevicePrefix) -> u64>,
    node_address: Option<unsafe extern "C" fn(*const IntelStolenNode) -> u64>,
    node_size: Option<unsafe extern "C" fn(*const IntelStolenNode) -> u64>,
    node_alloc: Option<
        unsafe extern "C" fn(
            *mut crate::linux::i915_private::DrmDevicePrefix,
        ) -> *mut IntelStolenNode,
    >,
    node_free: Option<unsafe extern "C" fn(*const IntelStolenNode)>,
}

#[inline]
unsafe fn dsm(i915: *mut DrmI915Private) -> *mut I915Dsm {
    unsafe { i915.cast::<u8>().add(DSM_OFFSET).cast() }
}
#[inline]
unsafe fn drm_mm_initialized(mm: *const DrmMm) -> bool {
    unsafe { !(*mm).hole_stack.next.is_null() }
}
#[inline]
fn resource_size(res: *const Resource) -> u64 {
    unsafe { (*res).end.wrapping_sub((*res).start).wrapping_add(1) }
}
#[inline]
fn make_resource(start: u64, size: u64) -> Resource {
    Resource {
        start,
        end: start.wrapping_add(size).wrapping_sub(1),
        name: ptr::null(),
        flags: IORESOURCE_MEM,
        desc: 0,
        parent: ptr::null_mut(),
        sibling: ptr::null_mut(),
        child: ptr::null_mut(),
    }
}
#[inline]
unsafe fn copy_resource(source: *const Resource) -> Resource {
    unsafe {
        Resource {
            start: (*source).start,
            end: (*source).end,
            name: (*source).name,
            flags: (*source).flags,
            desc: (*source).desc,
            parent: (*source).parent,
            sibling: (*source).sibling,
            child: (*source).child,
        }
    }
}
#[inline]
fn range_overflows(offset: u64, size: u64, max: u64) -> bool {
    offset.checked_add(size).map_or(true, |end| end > max)
}
#[inline]
fn resource_contains(parent: *const Resource, child: *const Resource) -> bool {
    unsafe { (*child).start >= (*parent).start && (*child).end <= (*parent).end }
}
#[inline]
unsafe fn has_lmembar_smem_stolen(i915: *mut DrmI915Private) -> bool {
    unsafe { !HAS_LMEM(i915) && GRAPHICS_VER_FULL(i915) >= IP_VER(12, 70) }
}
#[inline]
unsafe fn pci_dev_from_device(dev: *mut c_void) -> *mut PciDevResources {
    unsafe { dev.cast::<u8>().sub(PCI_DEV_OFFSET).cast() }
}
#[inline]
unsafe fn pci_resource(pdev: *mut PciDevResources, bar: usize) -> *mut Resource {
    assert!(bar < GEN12_LMEM_BAR + 1);
    unsafe { ptr::addr_of_mut!((*pdev).resource[bar]) }
}
#[inline]
unsafe fn pci_resource_start(pdev: *mut PciDevResources, bar: usize) -> u64 {
    unsafe { (*pci_resource(pdev, bar)).start }
}
#[inline]
unsafe fn pci_resource_len(pdev: *mut PciDevResources, bar: usize) -> u64 {
    let r = unsafe { pci_resource(pdev, bar) };
    unsafe {
        if (*r).start == 0 && (*r).end == 0 {
            0
        } else {
            resource_size(r)
        }
    }
}
#[inline]
unsafe fn is_gm45(i915: *mut DrmI915Private) -> bool {
    unsafe { IS_PLATFORM(i915, 14) }
}
#[inline]
unsafe fn is_g33(i915: *mut DrmI915Private) -> bool {
    unsafe { IS_PLATFORM(i915, 9) }
}

unsafe extern "C" {
    fn drm_mm_insert_node_in_range(
        mm: *mut DrmMm,
        node: *mut DrmMmNode,
        size: u64,
        alignment: u64,
        color: c_ulong,
        start: u64,
        end: u64,
        mode: c_int,
    ) -> c_int;
    fn drm_mm_remove_node(node: *mut DrmMmNode);
    fn drm_mm_reserve_node(mm: *mut DrmMm, node: *mut DrmMmNode) -> c_int;
    fn drm_mm_init(mm: *mut DrmMm, start: u64, size: u64);
    fn drm_mm_takedown(mm: *mut DrmMm);
    fn sg_alloc_table(sgt: *mut SgTable, nents: u32, gfp: u32) -> c_int;
    fn sg_free_table(sgt: *mut SgTable);
    fn intel_vgpu_active(i915: *mut DrmI915Private) -> bool;
    fn i915_pci_resource_valid(pdev: *mut c_void, bar: c_int) -> bool;
    fn i915_direct_stolen_access(i915: *mut DrmI915Private) -> bool;
    fn devm_request_mem_region(
        dev: *mut c_void,
        start: u64,
        n: u64,
        name: *const c_char,
    ) -> *mut Resource;
    static intel_graphics_stolen_res: Resource;
}

// upstream: i915_gem_stolen.c __i915_gem_stolen_insert_node_in_range()
unsafe fn __i915_gem_stolen_insert_node_in_range(
    i915: *mut DrmI915Private,
    node: *mut DrmMmNode,
    size: u64,
    alignment: u32,
    start: u64,
    end: u64,
) -> c_int {
    let mm = unsafe { ptr::addr_of_mut!((*i915).mm.stolen) };
    if !unsafe { drm_mm_initialized(mm) } {
        return -ENODEV;
    }
    let start = if unsafe { GRAPHICS_VER(i915) >= 8 && start < 4096 } {
        4096
    } else {
        start
    };
    unsafe { mutex_lock(&mut (*i915).mm.stolen_lock) };
    let ret = unsafe {
        drm_mm_insert_node_in_range(
            mm,
            node,
            size,
            alignment as u64,
            0,
            start,
            end,
            DRM_MM_INSERT_BEST,
        )
    };
    unsafe { mutex_unlock(&mut (*i915).mm.stolen_lock) };
    ret
}

// upstream: i915_gem_stolen.c i915_gem_stolen_insert_node_in_range()
unsafe extern "C" fn i915_gem_stolen_insert_node_in_range(
    node: *mut IntelStolenNode,
    size: u64,
    alignment: u32,
    start: u64,
    end: u64,
) -> c_int {
    unsafe {
        __i915_gem_stolen_insert_node_in_range(
            (*node).i915,
            &mut (*node).node,
            size,
            alignment,
            start,
            end,
        )
    }
}

// upstream: i915_gem_stolen.c __i915_gem_stolen_insert_node()
unsafe fn __i915_gem_stolen_insert_node(
    i915: *mut DrmI915Private,
    node: *mut DrmMmNode,
    size: u64,
    alignment: u32,
) -> c_int {
    unsafe {
        __i915_gem_stolen_insert_node_in_range(
            i915,
            node,
            size,
            alignment,
            I915_GEM_STOLEN_BIAS,
            u64::MAX,
        )
    }
}

// upstream: i915_gem_stolen.c i915_gem_stolen_insert_node()
unsafe extern "C" fn i915_gem_stolen_insert_node(
    node: *mut IntelStolenNode,
    size: u64,
    alignment: u32,
) -> c_int {
    unsafe { __i915_gem_stolen_insert_node((*node).i915, &mut (*node).node, size, alignment) }
}

// upstream: i915_gem_stolen.c __i915_gem_stolen_remove_node()
unsafe fn __i915_gem_stolen_remove_node(i915: *mut DrmI915Private, node: *mut DrmMmNode) {
    unsafe {
        mutex_lock(&mut (*i915).mm.stolen_lock);
        drm_mm_remove_node(node);
        mutex_unlock(&mut (*i915).mm.stolen_lock);
    }
}

// upstream: i915_gem_stolen.c i915_gem_stolen_remove_node()
unsafe extern "C" fn i915_gem_stolen_remove_node(node: *mut IntelStolenNode) {
    unsafe { __i915_gem_stolen_remove_node((*node).i915, &mut (*node).node) }
}

// upstream: i915_gem_stolen.c valid_stolen_size()
unsafe fn valid_stolen_size(i915: *mut DrmI915Private, dsm: *const Resource) -> bool {
    unsafe { ((*dsm).start != 0 || has_lmembar_smem_stolen(i915)) && (*dsm).end > (*dsm).start }
}

// upstream: i915_gem_stolen.c adjust_stolen()
unsafe fn adjust_stolen(i915: *mut DrmI915Private, dsm: *mut Resource) -> c_int {
    let ggtt = unsafe { (*to_gt(i915)).ggtt };
    let uncore = unsafe { (*(*ggtt).vm.gt).uncore };
    if !unsafe { valid_stolen_size(i915, dsm) } {
        return -EINVAL;
    }
    if unsafe { GRAPHICS_VER(i915) <= 4 && !is_g33(i915) && !IS_PINEVIEW(i915) && !IS_G4X(i915) } {
        let original = unsafe { copy_resource(dsm) };
        let mut low = unsafe { copy_resource(&original) };
        let mut high = unsafe { copy_resource(&original) };
        let mut gtt_start = unsafe { intel_uncore_read(uncore, I915RegT { reg: 0x2020 }) as u64 };
        if unsafe { GRAPHICS_VER(i915) == 4 } {
            gtt_start = (gtt_start & 0xffff_f000) | ((gtt_start & 0x0000_00f0) << 28);
        } else {
            gtt_start &= 0xffff_f000;
        }
        let gtt_size =
            unsafe { crate::intel_gtt_api_upstream::ggtt_total_entries(ggtt) }.wrapping_mul(4);
        let gtt = make_resource(gtt_start, gtt_size);
        if gtt.start >= low.start && gtt.start < low.end {
            low.end = gtt.start;
        }
        if gtt.end > high.start && gtt.end <= high.end {
            high.start = gtt.end;
        }
        unsafe {
            if resource_size(&low) > resource_size(&high) {
                *dsm = copy_resource(&low);
            } else {
                *dsm = copy_resource(&high);
            }
        }
        if low.start != high.start || low.end != high.end {
            drm_dbg!(
                &(*i915).drm,
                "GTT within stolen memory at %llx..%llx\n",
                gtt.start,
                gtt.end
            );
            drm_dbg!(
                &(*i915).drm,
                "Stolen memory adjusted to %llx..%llx\n",
                unsafe { (*dsm).start },
                unsafe { (*dsm).end }
            );
        }
    }
    if !unsafe { valid_stolen_size(i915, dsm) } {
        return -EINVAL;
    }
    0
}

// upstream: i915_gem_stolen.c request_smem_stolen()
unsafe fn request_smem_stolen(i915: *mut DrmI915Private, dsm: *mut Resource) -> c_int {
    if unsafe { HAS_LMEM(i915) || has_lmembar_smem_stolen(i915) } {
        return 0;
    }
    let dev = unsafe { (*i915).drm.dev };
    let size = resource_size(dsm);
    let name = b"Graphics Stolen Memory\0".as_ptr().cast();
    let mut r = unsafe { devm_request_mem_region(dev, (*dsm).start, size, name) };
    if r.is_null() {
        r = unsafe {
            devm_request_mem_region(
                dev,
                (*dsm).start.wrapping_add(1),
                size.wrapping_sub(2),
                name,
            )
        };
        if r.is_null() && unsafe { GRAPHICS_VER(i915) != 3 } {
            drm_err!(
                &(*i915).drm,
                "conflict detected with stolen region %llx..%llx\n",
                unsafe { (*dsm).start },
                unsafe { (*dsm).end }
            );
            return -EBUSY;
        }
    }
    0
}

// upstream: i915_gem_stolen.c i915_gem_cleanup_stolen()
unsafe fn i915_gem_cleanup_stolen(i915: *mut DrmI915Private) {
    let mm = unsafe { ptr::addr_of_mut!((*i915).mm.stolen) };
    if !unsafe { drm_mm_initialized(mm) } {
        return;
    }
    unsafe {
        drm_mm_takedown(mm);
    }
}

#[inline]
fn missing_case(value: u32) {
    axlog::warn!("i915: unhandled stolen-memory register value {:#x}", value);
}

// upstream: i915_gem_stolen.c g4x_get_stolen_reserved()
unsafe fn g4x_get_stolen_reserved(
    i915: *mut DrmI915Private,
    uncore: *mut IntelUncore,
    base: *mut u64,
    size: *mut u64,
) {
    let gm45 = unsafe { is_gm45(i915) };
    let reg = if gm45 {
        CTG_STOLEN_RESERVED
    } else {
        ELK_STOLEN_RESERVED
    };
    let value = unsafe { intel_uncore_read(uncore, reg) };
    let stolen_top = unsafe { (*dsm(i915)).stolen.end.wrapping_add(1) };
    drm_dbg!(
        &(*i915).drm,
        "%s_STOLEN_RESERVED = %08x\n",
        if gm45 { "CTG" } else { "ELK" },
        value
    );
    if value & G4X_STOLEN_RESERVED_ENABLE == 0 {
        return;
    }
    drm_WARN_ON!(&(*i915).drm, unsafe { GRAPHICS_VER(i915) == 5 });
    if value & G4X_STOLEN_RESERVED_ADDR2_MASK == 0 {
        return;
    }
    unsafe {
        *base = ((value & G4X_STOLEN_RESERVED_ADDR2_MASK) as u64) << 16;
        drm_WARN_ON!(
            &(*i915).drm,
            value & G4X_STOLEN_RESERVED_ADDR1_MASK < *base as u32
        );
        *size = stolen_top.wrapping_sub(*base);
    }
}

// upstream: i915_gem_stolen.c gen6_get_stolen_reserved()
unsafe fn gen6_get_stolen_reserved(
    i915: *mut DrmI915Private,
    uncore: *mut IntelUncore,
    base: *mut u64,
    size: *mut u64,
) {
    let value = unsafe { intel_uncore_read(uncore, GEN6_STOLEN_RESERVED) };
    drm_dbg!(&(*i915).drm, "GEN6_STOLEN_RESERVED = %08x\n", value);
    if value & GEN6_STOLEN_RESERVED_ENABLE == 0 {
        return;
    }
    unsafe {
        *base = (value & GEN6_STOLEN_RESERVED_ADDR_MASK) as u64;
    }
    unsafe {
        *size = match value & GEN6_STOLEN_RESERVED_SIZE_MASK {
            GEN6_STOLEN_RESERVED_1M => SZ_1M,
            GEN6_STOLEN_RESERVED_512K => SZ_1M / 2,
            GEN6_STOLEN_RESERVED_256K => SZ_1M / 4,
            GEN6_STOLEN_RESERVED_128K => SZ_1M / 8,
            other => {
                missing_case(other);
                SZ_1M
            }
        };
    }
}

// upstream: i915_gem_stolen.c vlv_get_stolen_reserved()
unsafe fn vlv_get_stolen_reserved(
    i915: *mut DrmI915Private,
    uncore: *mut IntelUncore,
    base: *mut u64,
    size: *mut u64,
) {
    let value = unsafe { intel_uncore_read(uncore, GEN6_STOLEN_RESERVED) };
    let top = unsafe { (*dsm(i915)).stolen.end.wrapping_add(1) };
    drm_dbg!(&(*i915).drm, "GEN6_STOLEN_RESERVED = %08x\n", value);
    if value & GEN6_STOLEN_RESERVED_ENABLE == 0 {
        return;
    }
    match value & GEN7_STOLEN_RESERVED_SIZE_MASK {
        GEN7_STOLEN_RESERVED_1M => unsafe { *size = SZ_1M },
        other => {
            missing_case(other);
            unsafe {
                *size = SZ_1M;
            }
        }
    }
    // VLV leaves ADDR_MASK zero; HW derives base as DSM top minus size.
    unsafe {
        *base = top.wrapping_sub(*size);
    }
}

// upstream: i915_gem_stolen.c gen7_get_stolen_reserved()
unsafe fn gen7_get_stolen_reserved(
    i915: *mut DrmI915Private,
    uncore: *mut IntelUncore,
    base: *mut u64,
    size: *mut u64,
) {
    let value = unsafe { intel_uncore_read(uncore, GEN6_STOLEN_RESERVED) };
    drm_dbg!(&(*i915).drm, "GEN6_STOLEN_RESERVED = %08x\n", value);
    if value & GEN6_STOLEN_RESERVED_ENABLE == 0 {
        return;
    }
    unsafe {
        *base = (value & GEN7_STOLEN_RESERVED_ADDR_MASK) as u64;
    }
    unsafe {
        *size = match value & GEN7_STOLEN_RESERVED_SIZE_MASK {
            GEN7_STOLEN_RESERVED_1M => SZ_1M,
            GEN7_STOLEN_RESERVED_256K => SZ_1M / 4,
            other => {
                missing_case(other);
                SZ_1M
            }
        };
    }
}

// upstream: i915_gem_stolen.c chv_get_stolen_reserved()
unsafe fn chv_get_stolen_reserved(
    i915: *mut DrmI915Private,
    uncore: *mut IntelUncore,
    base: *mut u64,
    size: *mut u64,
) {
    let value = unsafe { intel_uncore_read(uncore, GEN6_STOLEN_RESERVED) };
    drm_dbg!(&(*i915).drm, "GEN6_STOLEN_RESERVED = %08x\n", value);
    if value & GEN6_STOLEN_RESERVED_ENABLE == 0 {
        return;
    }
    unsafe {
        *base = (value & GEN6_STOLEN_RESERVED_ADDR_MASK) as u64;
    }
    unsafe {
        *size = match value & GEN8_STOLEN_RESERVED_SIZE_MASK {
            GEN8_STOLEN_RESERVED_1M => SZ_1M,
            GEN8_STOLEN_RESERVED_2M => 2 * SZ_1M,
            GEN8_STOLEN_RESERVED_4M => 4 * SZ_1M,
            GEN8_STOLEN_RESERVED_8M => 8 * SZ_1M,
            other => {
                missing_case(other);
                8 * SZ_1M
            }
        };
    }
}

// upstream: i915_gem_stolen.c bdw_get_stolen_reserved()
unsafe fn bdw_get_stolen_reserved(
    i915: *mut DrmI915Private,
    uncore: *mut IntelUncore,
    base: *mut u64,
    size: *mut u64,
) {
    let value = unsafe { intel_uncore_read(uncore, GEN6_STOLEN_RESERVED) };
    let top = unsafe { (*dsm(i915)).stolen.end.wrapping_add(1) };
    drm_dbg!(&(*i915).drm, "GEN6_STOLEN_RESERVED = %08x\n", value);
    if value & GEN6_STOLEN_RESERVED_ENABLE == 0 || value & GEN6_STOLEN_RESERVED_ADDR_MASK == 0 {
        return;
    }
    unsafe {
        *base = (value & GEN6_STOLEN_RESERVED_ADDR_MASK) as u64;
        *size = top.wrapping_sub(*base);
    }
}

// upstream: i915_gem_stolen.c icl_get_stolen_reserved()
unsafe fn icl_get_stolen_reserved(
    i915: *mut DrmI915Private,
    uncore: *mut IntelUncore,
    base: *mut u64,
    size: *mut u64,
) {
    let value = unsafe { intel_uncore_read64(uncore, GEN6_STOLEN_RESERVED) };
    drm_dbg!(&(*i915).drm, "GEN6_STOLEN_RESERVED = 0x%016llx\n", value);
    if unsafe { MEDIA_VER_FULL(i915) == IP_VER(13, 0) } {
        let gsc_base = unsafe {
            intel_uncore_read64_2x32(uncore, MTL_GSCPSMI_BASEADDR_LSB, MTL_GSCPSMI_BASEADDR_MSB)
        };
        let stolen = unsafe { &(*dsm(i915)).stolen };
        if gsc_base >= stolen.start && gsc_base < stolen.end {
            unsafe {
                *base = gsc_base;
                *size = stolen.end - gsc_base;
            }
            return;
        }
    }
    let reserve = match value as u32 & GEN8_STOLEN_RESERVED_SIZE_MASK {
        GEN8_STOLEN_RESERVED_1M => SZ_1M,
        GEN8_STOLEN_RESERVED_2M => 2 * SZ_1M,
        GEN8_STOLEN_RESERVED_4M => 4 * SZ_1M,
        GEN8_STOLEN_RESERVED_8M => 8 * SZ_1M,
        other => {
            missing_case(other);
            8 * SZ_1M
        }
    };
    unsafe {
        *size = reserve;
        if has_lmembar_smem_stolen(i915) {
            *base = (*base).wrapping_sub(*size);
        } else {
            *base = value & GEN11_STOLEN_RESERVED_ADDR_MASK;
        }
    }
}

// upstream: i915_gem_stolen.c init_reserved_stolen()
unsafe fn init_reserved_stolen(i915: *mut DrmI915Private) -> c_int {
    let uncore = unsafe { (*to_gt(i915)).uncore };
    let stolen_top = unsafe { (*dsm(i915)).stolen.end.wrapping_add(1) };
    let mut base = stolen_top;
    let mut size = 0;
    let ver = unsafe { GRAPHICS_VER(i915) };
    unsafe {
        if ver >= 11 {
            icl_get_stolen_reserved(i915, uncore, &mut base, &mut size);
        } else if ver >= 8 {
            if IS_CHERRYVIEW(i915) || IS_BROXTON(i915) || IS_GEMINILAKE(i915) {
                chv_get_stolen_reserved(i915, uncore, &mut base, &mut size);
            } else {
                bdw_get_stolen_reserved(i915, uncore, &mut base, &mut size);
            }
        } else if ver >= 7 {
            if IS_VALLEYVIEW(i915) {
                vlv_get_stolen_reserved(i915, uncore, &mut base, &mut size);
            } else {
                gen7_get_stolen_reserved(i915, uncore, &mut base, &mut size);
            }
        } else if ver >= 6 {
            gen6_get_stolen_reserved(i915, uncore, &mut base, &mut size);
        } else if ver >= 5 || IS_G4X(i915) {
            g4x_get_stolen_reserved(i915, uncore, &mut base, &mut size);
        }
    }
    let dsm = unsafe { dsm(i915) };
    if base == stolen_top {
        unsafe {
            (*dsm).reserved = make_resource(base, 0);
        }
        return 0;
    }
    if base == 0 {
        drm_err!(
            &(*i915).drm,
            "inconsistent stolen reservation base=%llx, size=%llx; ignoring\n",
            base,
            size
        );
        unsafe {
            (*dsm).reserved = make_resource(base, 0);
        }
        return -EINVAL;
    }
    unsafe {
        (*dsm).reserved = make_resource(base, size);
    }
    if !unsafe { resource_contains(&(*dsm).stolen, &(*dsm).reserved) } {
        drm_err!(
            &(*i915).drm,
            "Stolen reserved area %llx..%llx outside stolen memory %llx..%llx\n",
            unsafe { (*dsm).reserved.start },
            unsafe { (*dsm).reserved.end },
            unsafe { (*dsm).stolen.start },
            unsafe { (*dsm).stolen.end }
        );
        unsafe {
            (*dsm).reserved = make_resource(base, 0);
        }
        return -EINVAL;
    }
    0
}

// upstream: i915_gem_stolen.c i915_gem_init_stolen()
unsafe fn i915_gem_init_stolen(mem: *mut IntelMemoryRegion) -> c_int {
    let i915 = unsafe { (*mem).i915 };
    unsafe {
        mutex_init(&mut (*i915).mm.stolen_lock);
    }
    if unsafe { intel_vgpu_active(i915) } {
        drm_notice!(
            &(*i915).drm,
            "iGVT-g active, disabling use of stolen memory\n"
        );
        return -ENOSPC;
    }
    if unsafe { i915_vtd_active(i915) && GRAPHICS_VER(i915) < 8 } {
        drm_notice!(
            &(*i915).drm,
            "DMAR active, disabling use of stolen memory\n"
        );
        return -ENOSPC;
    }
    if unsafe {
        adjust_stolen(i915, &mut (*mem).region) != 0
            || request_smem_stolen(i915, &mut (*mem).region) != 0
    } {
        return -ENOSPC;
    }
    let dsm = unsafe { dsm(i915) };
    unsafe {
        (*dsm).stolen = copy_resource(&(*mem).region);
    }
    if unsafe { init_reserved_stolen(i915) != 0 } {
        return -ENOSPC;
    }
    unsafe {
        (*mem).region.end = (*dsm).reserved.start.wrapping_sub(1);
        (*mem).io = make_resource(
            (*mem).io.start,
            core::cmp::min(resource_size(&(*mem).io), resource_size(&(*mem).region)),
        );
        (*dsm).usable_size = resource_size(&(*mem).region);
    }
    drm_dbg!(
        &(*i915).drm,
        "Memory reserved for graphics device: %lluK, usable: %lluK\n",
        unsafe { resource_size(&(*dsm).stolen) >> 10 },
        unsafe { (*dsm).usable_size >> 10 }
    );
    if unsafe { (*dsm).usable_size == 0 } {
        return -ENOSPC;
    }
    unsafe {
        drm_mm_init(&mut (*i915).mm.stolen, 0, (*dsm).usable_size);
    }
    // MTL A0 cannot safely expose the top-end stolen LMEM range to userspace.
    if unsafe { IS_METEORLAKE(i915) && IS_GRAPHICS_STEP(i915, STEP_A0, STEP_B0) } {
        unsafe {
            (*dsm).usable_size = 0;
        }
    }
    0
}

// upstream: i915_gem_stolen.c dbg_poison()
unsafe fn dbg_poison(
    _ggtt: *mut crate::intel_gtt_api_upstream::I915Ggtt,
    _addr: u64,
    _size: u64,
    _x: u8,
) {
    #[cfg(CONFIG_DRM_I915_DEBUG_GEM)]
    unsafe {
        let ggtt = _ggtt;
        let mut addr = _addr;
        let mut size = _size;
        let x = _x;
        if !drm_mm_node_allocated(&(*ggtt).error_capture)
            || (*ggtt).vm.bind_async_flags & I915_VMA_GLOBAL_BIND != 0
        {
            return;
        }
        assert!(size % PAGE_SIZE as u64 == 0);
        mutex_lock(&mut (*ggtt).error_mutex);
        while size != 0 {
            ((*ggtt).vm.insert_page.unwrap())(
                &mut (*ggtt).vm,
                addr,
                (*ggtt).error_capture.start,
                crate::i915_gem_object_types_upstream::I915_CACHE_NONE as u32,
                0,
            );
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            let s = io_mapping_map_wc(&mut (*ggtt).iomap, (*ggtt).error_capture.start);
            for byte in 0..PAGE_SIZE {
                s.cast::<u8>().add(byte).write_volatile(x);
            }
            addr += PAGE_SIZE as u64;
            size -= PAGE_SIZE as u64;
        }
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        ((*ggtt).vm.clear_range.unwrap())(
            &mut (*ggtt).vm,
            (*ggtt).error_capture.start,
            PAGE_SIZE as u64,
        );
        mutex_unlock(&mut (*ggtt).error_mutex);
    }
}

#[inline]
fn err_ptr(err: c_int) -> *mut IntelMemoryRegion {
    (err as isize) as *mut IntelMemoryRegion
}
#[inline]
fn is_err<T>(ptr: *const T) -> bool {
    (ptr as isize) < 0 && (ptr as isize) >= -4095
}
#[inline]
unsafe fn gem_obj_i915(obj: *mut DrmI915GemObject) -> *mut DrmI915Private {
    let base = obj.cast::<crate::intel_context_upstream::DrmGemObjectBaseLayout>();
    unsafe { to_i915((*base).dev) }
}

// upstream: i915_gem_stolen.c i915_pages_create_for_stolen()
unsafe fn i915_pages_create_for_stolen(
    dev: *mut crate::linux::i915_private::DrmDevicePrefix,
    offset: u64,
    size: u64,
) -> *mut SgTable {
    let i915 = unsafe { to_i915(dev.cast::<c_void>()) };
    let dsm = unsafe { dsm(i915) };
    assert!(!range_overflows(
        offset,
        size,
        resource_size(&(*dsm).stolen)
    ));
    let st = kzalloc_obj::<SgTable>();
    if st.is_null() {
        return (-ENOMEM as isize) as *mut SgTable;
    }
    if unsafe { sg_alloc_table(st, 1, GFP_KERNEL) } != 0 {
        unsafe {
            kfree(st);
        }
        return (-ENOMEM as isize) as *mut SgTable;
    }
    unsafe {
        let sg = (*st).sgl;
        (*sg).offset = 0;
        (*sg).length = size as u32;
        (*sg).dma_address = (*dsm).stolen.start.wrapping_add(offset);
        (*sg).dma_length = size as u32;
    }
    st
}

// upstream: i915_gem_stolen.c i915_gem_object_get_pages_stolen()
unsafe extern "C" fn i915_gem_object_get_pages_stolen(obj: *mut DrmI915GemObject) -> c_int {
    let i915 = unsafe { gem_obj_i915(obj) };
    let node = unsafe { (*obj).backing.stolen };
    let base = obj.cast::<crate::intel_context_upstream::DrmGemObjectBaseLayout>();
    let pages =
        unsafe { i915_pages_create_for_stolen((*base).dev.cast(), (*node).start, (*node).size) };
    if (pages as isize) < 0 {
        return pages as isize as c_int;
    }
    unsafe {
        let sg = (*pages).sgl;
        dbg_poison(
            (*to_gt(i915)).ggtt,
            (*sg).dma_address,
            (*sg).dma_length as u64,
            POISON_INUSE,
        );
        __i915_gem_object_set_pages(obj, pages);
    }
    0
}

// upstream: i915_gem_stolen.c i915_gem_object_put_pages_stolen()
unsafe extern "C" fn i915_gem_object_put_pages_stolen(
    obj: *mut DrmI915GemObject,
    pages: *mut SgTable,
) {
    let i915 = unsafe { gem_obj_i915(obj) };
    unsafe {
        let sg = (*pages).sgl;
        dbg_poison(
            (*to_gt(i915)).ggtt,
            (*sg).dma_address,
            (*sg).dma_length as u64,
            POISON_FREE,
        );
        sg_free_table(pages);
        kfree(pages);
    }
}

// upstream: i915_gem_stolen.c i915_gem_object_release_stolen()
unsafe extern "C" fn i915_gem_object_release_stolen(obj: *mut DrmI915GemObject) {
    let i915 = unsafe { gem_obj_i915(obj) };
    let stolen = unsafe { core::ptr::replace(&mut (*obj).backing.stolen, ptr::null_mut()) };
    assert!(
        !stolen.is_null(),
        "stolen GEM object lost its allocation node"
    );
    unsafe {
        __i915_gem_stolen_remove_node(i915, stolen);
        kfree(stolen);
        i915_gem_object_release_memory_region(obj);
    }
}

static i915_gem_object_stolen_ops: DrmI915GemObjectOps = DrmI915GemObjectOps {
    flags: 0,
    get_pages: Some(i915_gem_object_get_pages_stolen),
    put_pages: Some(i915_gem_object_put_pages_stolen),
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
    release: Some(i915_gem_object_release_stolen),
    mmap_ops: ptr::null(),
    name: b"i915_gem_object_stolen\0".as_ptr().cast(),
};

// upstream: i915_gem_stolen.c __i915_gem_object_create_stolen()
unsafe fn __i915_gem_object_create_stolen(
    mem: *mut IntelMemoryRegion,
    obj: *mut DrmI915GemObject,
    stolen: *mut DrmMmNode,
) -> c_int {
    static mut LOCK_CLASS: crate::linux::fields::LockClassKey =
        crate::linux::fields::LockClassKey {};
    let i915 = unsafe { (*mem).i915 };
    unsafe {
        drm_gem_private_object_init(
            ptr::addr_of_mut!((*i915).drm).cast::<c_void>(),
            obj.cast(),
            (*stolen).size as usize,
        );
        crate::i915_gem_object_upstream::i915_gem_object_init(
            obj,
            &i915_gem_object_stolen_ops,
            ptr::addr_of_mut!(LOCK_CLASS),
            I915_BO_ALLOC_CONTIGUOUS as u32,
        );
        (*obj).backing.stolen = stolen;
        (*obj).read_domains = (I915_GEM_DOMAIN_CPU | I915_GEM_DOMAIN_GTT) as u16;
        let cache_level = if HAS_LLC(i915) {
            I915_CACHE_LLC
        } else {
            I915_CACHE_NONE
        };
        i915_gem_object_set_cache_coherency(obj, cache_level);
        if !i915_gem_object_trylock(obj, ptr::null_mut()) {
            return -EBUSY;
        }
        i915_gem_object_init_memory_region(obj, mem);
        let err = i915_gem_object_pin_pages(obj);
        if err != 0 {
            i915_gem_object_release_memory_region(obj);
        }
        i915_gem_object_unlock(obj);
        err
    }
}

// upstream: i915_gem_stolen.c _i915_gem_object_stolen_init()
unsafe extern "C" fn _i915_gem_object_stolen_init(
    mem: *mut IntelMemoryRegion,
    obj: *mut DrmI915GemObject,
    offset: u64,
    size: u64,
    _page_size: u64,
    flags: u32,
) -> c_int {
    let i915 = unsafe { (*mem).i915 };
    let mm = unsafe { ptr::addr_of_mut!((*i915).mm.stolen) };
    if !unsafe { drm_mm_initialized(mm) } {
        return -ENODEV;
    }
    if size == 0 {
        return -EINVAL;
    }
    if unsafe {
        (*mem).r#type == crate::linux::gem_memory::INTEL_MEMORY_STOLEN_LOCAL
            && resource_size(&(*mem).io) == 0
            && flags & I915_BO_ALLOC_GPU_ONLY as u32 == 0
    } {
        return -ENOSPC;
    }
    let stolen = kzalloc_obj::<DrmMmNode>();
    if stolen.is_null() {
        return -ENOMEM;
    }
    let ret = if offset != I915_BO_INVALID_OFFSET {
        drm_dbg!(
            &(*i915).drm,
            "creating preallocated stolen object: offset=%llx, size=%llx\n",
            offset,
            size
        );
        unsafe {
            (*stolen).start = offset;
            (*stolen).size = size;
            mutex_lock(&mut (*i915).mm.stolen_lock);
        }
        let ret = unsafe { drm_mm_reserve_node(mm, stolen) };
        unsafe {
            mutex_unlock(&mut (*i915).mm.stolen_lock);
        }
        ret
    } else {
        unsafe { __i915_gem_stolen_insert_node(i915, stolen, size, (*mem).min_page_size as u32) }
    };
    if ret != 0 {
        unsafe {
            kfree(stolen);
        }
        return ret;
    }
    let ret = unsafe { __i915_gem_object_create_stolen(mem, obj, stolen) };
    if ret != 0 {
        unsafe {
            __i915_gem_stolen_remove_node(i915, stolen);
            kfree(stolen);
        }
    }
    ret
}

// upstream: i915_gem_stolen.c i915_gem_object_create_stolen()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_object_create_stolen(
    i915: *mut DrmI915Private,
    size: u64,
) -> *mut DrmI915GemObject {
    unsafe { i915_gem_object_create_region((*i915).mm.stolen_region, size, 0, 0) }
}

// upstream: i915_gem_stolen.c init_stolen_smem()
unsafe extern "C" fn init_stolen_smem(mem: *mut IntelMemoryRegion) -> c_int {
    let err = unsafe { i915_gem_init_stolen(mem) };
    if err != 0 {
        drm_dbg!(&(*(*mem).i915).drm, "Skip stolen region: failed to setup\n");
    }
    0
}

// upstream: i915_gem_stolen.c release_stolen_smem()
unsafe extern "C" fn release_stolen_smem(mem: *mut IntelMemoryRegion) -> c_int {
    unsafe {
        i915_gem_cleanup_stolen((*mem).i915);
    }
    0
}

static i915_region_stolen_smem_ops: IntelMemoryRegionOps = IntelMemoryRegionOps {
    init: Some(init_stolen_smem),
    release: Some(release_stolen_smem),
    init_object: Some(_i915_gem_object_stolen_init),
};

// These source-inline accessors delegate to the LinuxKPI WC mapping owner,
// which preserves PAT1 cache mode and waits for the shared kernel TLB owner.
unsafe fn io_mapping_init_wc(
    mapping: *mut crate::linux::gem_memory::IoMapping,
    base: u64,
    size: u64,
) -> bool {
    unsafe { crate::linux::iomapping::io_mapping_init_wc(mapping, base, size as usize) }
}
unsafe fn io_mapping_fini(mapping: *mut crate::linux::gem_memory::IoMapping) {
    unsafe { crate::linux::iomapping::io_mapping_fini(mapping) };
}
unsafe fn io_mapping_map_wc(
    mapping: *mut crate::linux::gem_memory::IoMapping,
    offset: u64,
) -> *mut c_void {
    unsafe { crate::linux::iomapping::io_mapping_map_wc(mapping, offset as usize).cast() }
}

// upstream: i915_gem_stolen.c init_stolen_lmem()
unsafe extern "C" fn init_stolen_lmem(mem: *mut IntelMemoryRegion) -> c_int {
    if resource_size(unsafe { ptr::addr_of!((*mem).region) }) == 0 {
        return 0;
    }
    let err = unsafe { i915_gem_init_stolen(mem) };
    if err != 0 {
        drm_dbg!(&(*(*mem).i915).drm, "Skip stolen region: failed to setup\n");
        return 0;
    }
    if unsafe {
        resource_size(&(*mem).io) != 0
            && !io_mapping_init_wc(
                &mut (*mem).iomap,
                (*mem).io.start,
                resource_size(&(*mem).io),
            )
    } {
        unsafe {
            i915_gem_cleanup_stolen((*mem).i915);
        }
        return err;
    }
    0
}

// upstream: i915_gem_stolen.c release_stolen_lmem()
unsafe extern "C" fn release_stolen_lmem(mem: *mut IntelMemoryRegion) -> c_int {
    unsafe {
        if resource_size(&(*mem).io) != 0 {
            io_mapping_fini(&mut (*mem).iomap);
        }
        i915_gem_cleanup_stolen((*mem).i915);
    }
    0
}

static i915_region_stolen_lmem_ops: IntelMemoryRegionOps = IntelMemoryRegionOps {
    init: Some(init_stolen_lmem),
    release: Some(release_stolen_lmem),
    init_object: Some(_i915_gem_object_stolen_init),
};

// upstream: i915_gem_stolen.c mtl_get_gms_size()
unsafe fn mtl_get_gms_size(uncore: *mut IntelUncore) -> c_int {
    let ggc = unsafe { intel_uncore_read16(uncore, GGC) };
    if ggc & GGMS_MASK != GGMS_MASK {
        return -linux_config::EIO;
    }
    let gms = ((ggc & GMS_MASK) >> 8) as u32;
    match gms {
        0x00..=0x04 => (gms * 32) as c_int,
        0xf0..=0xfe => ((gms - 0xf0 + 1) * 4) as c_int,
        other => {
            missing_case(other);
            -linux_config::EIO
        }
    }
}

// upstream: i915_gem_stolen.c i915_gem_stolen_lmem_setup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_stolen_lmem_setup(
    i915: *mut DrmI915Private,
    type_: u16,
    instance: u16,
) -> *mut IntelMemoryRegion {
    let gt = unsafe { to_gt(i915) };
    let uncore = unsafe { (*gt).uncore };
    let pdev = unsafe { pci_dev_from_device((*i915).drm.dev) };
    if WARN_ON_ONCE!(instance != 0) {
        return err_ptr(-ENODEV);
    }
    if !unsafe { i915_pci_resource_valid(pdev.cast(), GEN12_LMEM_BAR as c_int) } {
        return err_ptr(-ENXIO);
    }
    let lmem_size = if unsafe { has_lmembar_smem_stolen(i915) || IS_DG1(i915) } {
        unsafe { pci_resource_len(pdev, GEN12_LMEM_BAR) }
    } else {
        let range = unsafe { intel_gt_mcr_read_any(gt, XEHP_TILE0_ADDR_RANGE) } & 0xffff;
        ((range >> XEHP_TILE_LMEM_RANGE_SHIFT) as u64).wrapping_mul(SZ_1G)
    };
    let (dsm_base, dsm_size) = if unsafe { has_lmembar_smem_stolen(i915) } {
        let gms_size = unsafe { mtl_get_gms_size(uncore) };
        if gms_size < 0 {
            drm_err!(&(*i915).drm, "invalid MTL GGC register setting\n");
            return err_ptr(gms_size);
        }
        GEM_BUG_ON!(unsafe { pci_resource_len(pdev, GEN12_LMEM_BAR) != SZ_256M });
        GEM_BUG_ON!((SZ_8M + gms_size as u64 * (SZ_1M / 1)) > lmem_size);
        (SZ_8M, gms_size as u64 * SZ_1M)
    } else {
        let base = unsafe { intel_uncore_read64(uncore, GEN6_DSMBASE) } & GEN11_BDSM_MASK;
        if lmem_size < base {
            drm_dbg!(
                &(*i915).drm,
                "Disabling stolen memory support due to OOB placement\n"
            );
            return ptr::null_mut();
        }
        (base, (lmem_size - base) & !(SZ_1M - 1))
    };
    let (io_start, io_size) = if unsafe { i915_direct_stolen_access(i915) } {
        drm_dbg!(&(*i915).drm, "Using direct DSM access\n");
        (
            unsafe { intel_uncore_read64(uncore, GEN6_DSMBASE) } & GEN11_BDSM_MASK,
            dsm_size,
        )
    } else if unsafe { pci_resource_len(pdev, GEN12_LMEM_BAR) < lmem_size } {
        (0, 0)
    } else {
        (
            unsafe { pci_resource_start(pdev, GEN12_LMEM_BAR) }.wrapping_add(dsm_base),
            dsm_size,
        )
    };
    let info = unsafe {
        (*i915)
            .info
            .cast::<crate::linux::i915::IntelDeviceInfoOverlay>()
    };
    let min_page_size = if !info.is_null() && unsafe { (*info).flags[0] & (1 << 4) != 0 } {
        I915_GTT_PAGE_SIZE_64K
    } else {
        I915_GTT_PAGE_SIZE_4K
    };
    let mem = unsafe {
        crate::intel_memory_region_upstream::intel_memory_region_create(
            i915,
            dsm_base,
            dsm_size,
            min_page_size,
            io_start,
            io_size,
            type_,
            instance,
            &i915_region_stolen_lmem_ops,
        )
    };
    if is_err(mem) {
        return mem;
    }
    unsafe {
        crate::intel_memory_region_upstream::intel_memory_region_set_name(
            mem,
            b"stolen-local\0".as_ptr().cast(),
            &[],
        );
        (*mem).private = true;
    }
    mem
}

// upstream: i915_gem_stolen.c i915_gem_stolen_smem_setup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_stolen_smem_setup(
    i915: *mut DrmI915Private,
    type_: u16,
    instance: u16,
) -> *mut IntelMemoryRegion {
    let stolen = unsafe { &intel_graphics_stolen_res };
    let mem = unsafe {
        crate::intel_memory_region_upstream::intel_memory_region_create(
            i915,
            stolen.start,
            resource_size(stolen),
            PAGE_SIZE as u64,
            0,
            0,
            type_,
            instance,
            &i915_region_stolen_smem_ops,
        )
    };
    if is_err(mem) {
        return mem;
    }
    unsafe {
        crate::intel_memory_region_upstream::intel_memory_region_set_name(
            mem,
            b"stolen-system\0".as_ptr().cast(),
            &[],
        );
        (*mem).private = true;
    }
    mem
}

// upstream: i915_gem_stolen.c i915_gem_object_is_stolen()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_object_is_stolen(obj: *const DrmI915GemObject) -> bool {
    unsafe { (*obj).ops == ptr::addr_of!(i915_gem_object_stolen_ops) }
}

// upstream: i915_gem_stolen.c i915_gem_stolen_initialized()
unsafe extern "C" fn i915_gem_stolen_initialized(
    drm: *mut crate::linux::i915_private::DrmDevicePrefix,
) -> bool {
    let i915 = unsafe { to_i915(drm.cast::<c_void>()) };
    unsafe { drm_mm_initialized(&(*i915).mm.stolen) }
}

// upstream: i915_gem_stolen.c i915_gem_stolen_area_address()
unsafe extern "C" fn i915_gem_stolen_area_address(
    drm: *mut crate::linux::i915_private::DrmDevicePrefix,
) -> u64 {
    let i915 = unsafe { to_i915(drm.cast::<c_void>()) };
    unsafe { (*dsm(i915)).stolen.start }
}

// upstream: i915_gem_stolen.c i915_gem_stolen_area_size()
unsafe extern "C" fn i915_gem_stolen_area_size(
    drm: *mut crate::linux::i915_private::DrmDevicePrefix,
) -> u64 {
    let i915 = unsafe { to_i915(drm.cast::<c_void>()) };
    unsafe { resource_size(&(*dsm(i915)).stolen) }
}

// upstream: i915_gem_stolen.c i915_gem_stolen_node_offset()
unsafe extern "C" fn i915_gem_stolen_node_offset(node: *const IntelStolenNode) -> u64 {
    unsafe { (*node).node.start }
}

// upstream: i915_gem_stolen.c i915_gem_stolen_node_address()
unsafe extern "C" fn i915_gem_stolen_node_address(node: *const IntelStolenNode) -> u64 {
    unsafe {
        (*dsm((*node).i915))
            .stolen
            .start
            .wrapping_add(i915_gem_stolen_node_offset(node))
    }
}

// upstream: i915_gem_stolen.c i915_gem_stolen_node_allocated()
unsafe extern "C" fn i915_gem_stolen_node_allocated(node: *const IntelStolenNode) -> bool {
    unsafe { drm_mm_node_allocated(&(*node).node) }
}

// upstream: i915_gem_stolen.c i915_gem_stolen_node_size()
unsafe extern "C" fn i915_gem_stolen_node_size(node: *const IntelStolenNode) -> u64 {
    unsafe { (*node).node.size }
}

// upstream: i915_gem_stolen.c i915_gem_stolen_node_alloc()
unsafe extern "C" fn i915_gem_stolen_node_alloc(
    drm: *mut crate::linux::i915_private::DrmDevicePrefix,
) -> *mut IntelStolenNode {
    let i915 = unsafe { to_i915(drm.cast::<c_void>()) };
    let node = kzalloc_obj::<IntelStolenNode>();
    if !node.is_null() {
        unsafe {
            (*node).i915 = i915;
        }
    }
    node
}

// upstream: i915_gem_stolen.c i915_gem_stolen_node_free()
unsafe extern "C" fn i915_gem_stolen_node_free(node: *const IntelStolenNode) {
    unsafe {
        kfree(node as *mut IntelStolenNode);
    }
}

#[unsafe(no_mangle)]
pub static i915_display_stolen_interface: IntelDisplayStolenInterface =
    IntelDisplayStolenInterface {
        insert_node_in_range: Some(i915_gem_stolen_insert_node_in_range),
        insert_node: Some(i915_gem_stolen_insert_node),
        remove_node: Some(i915_gem_stolen_remove_node),
        initialized: Some(i915_gem_stolen_initialized),
        node_allocated: Some(i915_gem_stolen_node_allocated),
        node_offset: Some(i915_gem_stolen_node_offset),
        area_address: Some(i915_gem_stolen_area_address),
        area_size: Some(i915_gem_stolen_area_size),
        node_address: Some(i915_gem_stolen_node_address),
        node_size: Some(i915_gem_stolen_node_size),
        node_alloc: Some(i915_gem_stolen_node_alloc),
        node_free: Some(i915_gem_stolen_node_free),
    };
