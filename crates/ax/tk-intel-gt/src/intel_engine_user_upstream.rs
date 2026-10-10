// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//
//! Source-order translation of Linux 7.2.3
//! `drivers/gpu/drm/i915/gt/intel_engine_user.c`.

#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use core::{
    ffi::{c_char, c_int, c_void},
    mem::{offset_of, size_of},
    ptr,
};

use crate::{
    intel_engine_cs_upstream::{
        BCS0, CCS0, IntelEngineCs, IntelEngineId, ListHead, RCS0, RbNode, RbRoot, VCS0, VECS0,
    },
    intel_engine_types_upstream::{
        I915_ENGINE_HAS_PREEMPTION, I915_ENGINE_HAS_SEMAPHORES, I915_ENGINE_SUPPORTS_STATS,
        I915_MAX_CCS, I915_MAX_VCS, I915_MAX_VECS, INTEL_ENGINE_CS_MAX_NAME, INVALID_ENGINE,
    },
    intel_gt_types_upstream::IntelGt,
    linux::{
        i915::IntelRuntimeInfo,
        list::{INIT_LIST_HEAD, list_add, llist_add, llist_del_all},
    },
    linux_i915_private::DrmI915Private,
};

const I915_NO_UABI_CLASS: u16 = u16::MAX;

const I915_ENGINE_CLASS_RENDER: u16 = 0;
const I915_ENGINE_CLASS_COPY: u16 = 1;
const I915_ENGINE_CLASS_VIDEO: u16 = 2;
const I915_ENGINE_CLASS_VIDEO_ENHANCE: u16 = 3;
const I915_ENGINE_CLASS_COMPUTE: u16 = 4;
const UABI_CLASSES: [u16; 6] = [
    I915_ENGINE_CLASS_RENDER,
    I915_ENGINE_CLASS_VIDEO,
    I915_ENGINE_CLASS_VIDEO_ENHANCE,
    I915_ENGINE_CLASS_COPY,
    I915_NO_UABI_CLASS,
    I915_ENGINE_CLASS_COMPUTE,
];

const I915_SCHEDULER_CAP_ENABLED: u32 = 1 << 0;
const I915_SCHEDULER_CAP_PRIORITY: u32 = 1 << 1;
const I915_SCHEDULER_CAP_PREEMPTION: u32 = 1 << 2;
const I915_SCHEDULER_CAP_SEMAPHORES: u32 = 1 << 3;
const I915_SCHEDULER_CAP_ENGINE_BUSY_STATS: u32 = 1 << 4;
const I915_SCHEDULER_CAP_STATIC_PRIORITY_MAP: u32 = 1 << 5;

const I915_UABI_ENGINE_CLASS_COUNT: usize = 5;
const I915_UABI_ENGINE_CLASS_COUNT_OFFSET: usize = 2376;

#[repr(C)]
#[derive(Clone, Copy)]
union I915UabiEngines {
    llist: crate::intel_engine_cs_upstream::LlistHead,
    list: ListHead,
    tree: RbRoot,
}

/// `drm_i915_private` fields from `i915_drv.h` needed before/after engine
/// registration. The class-count offset is the existing source-derived
/// overlay asserted against `DrmI915Private.wq` in `i915_drm_client_upstream`;
/// the source union immediately precedes it and has `list_head`'s 16-byte size.
#[repr(C)]
struct I915EngineUabiView {
    _prefix: [u8; I915_UABI_ENGINE_CLASS_COUNT_OFFSET - size_of::<I915UabiEngines>()],
    engines: I915UabiEngines,
    engine_uabi_class_count: [u32; I915_UABI_ENGINE_CLASS_COUNT],
}

#[repr(C)]
struct IntelDriverCapsView {
    scheduler: u32,
}

const _: [(); 16] = [(); size_of::<I915UabiEngines>()];
const _: [(); 2360] = [(); offset_of!(I915EngineUabiView, engines)];
const _: [(); I915_UABI_ENGINE_CLASS_COUNT_OFFSET] =
    [(); offset_of!(I915EngineUabiView, engine_uabi_class_count)];
const _: [(); 36] = [(); size_of::<IntelRuntimeInfo>()];
const _: [(); 0] = [(); offset_of!(IntelDriverCapsView, scheduler)];

unsafe extern "C" {
    fn list_sort(
        priv_: *mut c_void,
        head: *mut ListHead,
        cmp: unsafe extern "C" fn(*mut c_void, *const ListHead, *const ListHead) -> c_int,
    );
    // `rb_insert_color()` is the out-of-line provider from Linux lib/rbtree.c.
    fn rb_insert_color(node: *mut RbNode, root: *mut RbRoot);
}

#[inline]
unsafe fn engine_uabi_view(i915: *mut DrmI915Private) -> *mut I915EngineUabiView {
    i915.cast()
}

#[inline]
pub(crate) unsafe fn engine_uabi_tree(i915: *mut DrmI915Private) -> *mut RbRoot {
    ptr::addr_of_mut!((*engine_uabi_view(i915)).engines.tree)
}

/// Read the header-owned legacy UABI engine count by class.
pub(crate) unsafe fn engine_uabi_class_count(
    i915: *mut DrmI915Private,
    class: usize,
) -> u32 {
    if class >= I915_UABI_ENGINE_CLASS_COUNT {
        return 0;
    }
    unsafe { (*engine_uabi_view(i915)).engine_uabi_class_count[class] }
}

#[inline]
pub(crate) unsafe fn engine_uabi_llist(
    i915: *mut DrmI915Private,
) -> *mut crate::intel_engine_cs_upstream::LlistHead {
    ptr::addr_of_mut!((*engine_uabi_view(i915)).engines.llist)
}

#[inline]
unsafe fn i915_scheduler_caps(i915: *mut DrmI915Private) -> *mut u32 {
    // `intel_driver_caps caps` immediately follows `intel_runtime_info __runtime`
    // in i915_drv.h; both its prefix and the runtime size are source-owned.
    let caps = i915
        .cast::<u8>()
        .add(offset_of!(DrmI915Private, runtime) + size_of::<IntelRuntimeInfo>())
        .cast::<IntelDriverCapsView>();
    ptr::addr_of_mut!((*caps).scheduler)
}

#[inline]
unsafe fn engine_from_uabi(node: *const u8) -> *mut IntelEngineCs {
    node.cast_mut()
        .sub(offset_of!(IntelEngineCs, uabi))
        .cast::<IntelEngineCs>()
}

// upstream: intel_engine_user.c intel_engine_lookup_user()
pub unsafe fn intel_engine_lookup_user(
    i915: *mut DrmI915Private,
    class: u8,
    instance: u8,
) -> *mut IntelEngineCs {
    let mut p = (*engine_uabi_tree(i915)).node;

    while !p.is_null() {
        let it = engine_from_uabi(p.cast());

        if (class as u16) < (*it).uabi_class {
            p = (*p).left;
        } else if (class as u16) > (*it).uabi_class || (instance as u16) > (*it).uabi_instance {
            p = (*p).right;
        } else if (instance as u16) < (*it).uabi_instance {
            p = (*p).left;
        } else {
            return it;
        }
    }

    ptr::null_mut()
}

// upstream: intel_engine_user.c intel_engine_add_user()
pub unsafe fn intel_engine_add_user(engine: *mut IntelEngineCs) {
    let i915 = (*engine).i915;
    let view = engine_uabi_view(i915);
    let _ = llist_add(
        ptr::addr_of_mut!((*engine).uabi.uabi_llist),
        ptr::addr_of_mut!((*view).engines.llist),
    );
}

// upstream: intel_engine_user.c engine_cmp()
unsafe extern "C" fn engine_cmp(
    _priv: *mut c_void,
    A: *const ListHead,
    B: *const ListHead,
) -> c_int {
    let a = engine_from_uabi(A.cast());
    let b = engine_from_uabi(B.cast());

    if UABI_CLASSES[(*a).class as usize] < UABI_CLASSES[(*b).class as usize] {
        return -1;
    }
    if UABI_CLASSES[(*a).class as usize] > UABI_CLASSES[(*b).class as usize] {
        return 1;
    }

    if (*a).instance < (*b).instance {
        return -1;
    }
    if (*a).instance > (*b).instance {
        return 1;
    }

    0
}

// upstream: intel_engine_user.c get_engines()
unsafe fn get_engines(
    i915: *mut DrmI915Private,
) -> *mut crate::intel_engine_cs_upstream::LlistNode {
    llist_del_all(ptr::addr_of_mut!((*engine_uabi_view(i915)).engines.llist))
}

// upstream: intel_engine_user.c sort_engines()
unsafe fn sort_engines(i915: *mut DrmI915Private, engines: *mut ListHead) {
    let mut pos = get_engines(i915);

    while !pos.is_null() {
        let next = (*pos).next;
        let engine = engine_from_uabi(pos.cast());
        list_add(ptr::addr_of_mut!((*engine).uabi.uabi_list), engines);
        pos = next;
    }

    list_sort(ptr::null_mut(), engines, engine_cmp);
}

// upstream: intel_engine_user.c set_scheduler_caps()
unsafe fn set_scheduler_caps(i915: *mut DrmI915Private) {
    const MAP: [(u8, u8); 3] = [
        (
            I915_ENGINE_HAS_PREEMPTION.trailing_zeros() as u8,
            I915_SCHEDULER_CAP_PREEMPTION.trailing_zeros() as u8,
        ),
        (
            I915_ENGINE_HAS_SEMAPHORES.trailing_zeros() as u8,
            I915_SCHEDULER_CAP_SEMAPHORES.trailing_zeros() as u8,
        ),
        (
            I915_ENGINE_SUPPORTS_STATS.trailing_zeros() as u8,
            I915_SCHEDULER_CAP_ENGINE_BUSY_STATS.trailing_zeros() as u8,
        ),
    ];
    let mut enabled = 0u32;
    let mut disabled = 0u32;
    let mut node = crate::intel_engine_api_upstream::rb_first_uabi_engine(engine_uabi_tree(i915));

    while !node.is_null() {
        let engine = engine_from_uabi(node.cast());

        if (*(*engine).sched_engine).schedule.is_some() {
            enabled |= I915_SCHEDULER_CAP_ENABLED | I915_SCHEDULER_CAP_PRIORITY;
        } else {
            disabled |= I915_SCHEDULER_CAP_ENABLED | I915_SCHEDULER_CAP_PRIORITY;
        }

        if crate::intel_uc_types_upstream::intel_uc_uses_guc_submission(ptr::addr_of_mut!(
            (*(*engine).gt).uc
        )) {
            enabled |= I915_SCHEDULER_CAP_STATIC_PRIORITY_MAP;
        }

        for (engine_bit, sched_bit) in MAP {
            if (*engine).flags & (1u32 << engine_bit) != 0 {
                enabled |= 1u32 << sched_bit;
            } else {
                disabled |= 1u32 << sched_bit;
            }
        }

        node = crate::linux::rbtree::rb_next(node);
    }

    let caps = i915_scheduler_caps(i915);
    *caps = enabled & !disabled;
    if *caps & I915_SCHEDULER_CAP_ENABLED == 0 {
        *caps = 0;
    }
}

// upstream: intel_engine_user.c intel_engine_class_repr()
pub fn intel_engine_class_repr(class: u8) -> *const c_char {
    static UABI_NAMES: [&[u8]; 6] = [
        b"rcs\0", b"vcs\0", b"vecs\0", b"bcs\0", b"other\0", b"ccs\0",
    ];

    if (class as usize) >= UABI_NAMES.len() {
        return b"xxx\0".as_ptr().cast();
    }

    UABI_NAMES[class as usize].as_ptr().cast()
}

struct LegacyRing {
    gt: *mut IntelGt,
    class: u8,
    instance: u8,
}

// upstream: intel_engine_user.c legacy_ring_idx()
unsafe fn legacy_ring_idx(ring: *const LegacyRing) -> IntelEngineId {
    #[derive(Clone, Copy)]
    struct RingMap {
        base: u8,
        max: u8,
    }
    const MAP: [RingMap; 6] = [
        RingMap {
            base: RCS0 as u8,
            max: 1,
        },
        RingMap {
            base: VCS0 as u8,
            max: I915_MAX_VCS as u8,
        },
        RingMap {
            base: VECS0 as u8,
            max: I915_MAX_VECS as u8,
        },
        RingMap {
            base: BCS0 as u8,
            max: 1,
        },
        RingMap { base: 0, max: 0 },
        RingMap {
            base: CCS0 as u8,
            max: I915_MAX_CCS as u8,
        },
    ];

    if GEM_DEBUG_WARN_ON!((*ring).class as usize >= MAP.len()) {
        return INVALID_ENGINE;
    }

    if GEM_DEBUG_WARN_ON!((*ring).instance >= MAP[(*ring).class as usize].max) {
        return INVALID_ENGINE;
    }

    MAP[(*ring).class as usize].base as IntelEngineId + (*ring).instance as IntelEngineId
}

// upstream: intel_engine_user.c add_legacy_ring()
unsafe fn add_legacy_ring(ring: *mut LegacyRing, engine: *mut IntelEngineCs) {
    if (*engine).gt != (*ring).gt || (*engine).class != (*ring).class {
        (*ring).gt = (*engine).gt;
        (*ring).class = (*engine).class;
        (*ring).instance = 0;
    }

    (*engine).legacy_idx = legacy_ring_idx(ring);
    if (*engine).legacy_idx != INVALID_ENGINE {
        (*ring).instance = (*ring).instance.wrapping_add(1);
    }
}

// upstream: intel_engine_user.c engine_rename()
unsafe fn engine_rename(engine: *mut IntelEngineCs, name: *const c_char, instance: u16) {
    let mut old = [0 as c_char; INTEL_ENGINE_CS_MAX_NAME];
    ptr::copy_nonoverlapping(
        (*engine).name.as_ptr(),
        old.as_mut_ptr(),
        size_of::<[c_char; INTEL_ENGINE_CS_MAX_NAME]>(),
    );
    scnprintf!(
        (*engine).name.as_mut_ptr(),
        size_of::<[c_char; INTEL_ENGINE_CS_MAX_NAME]>(),
        "%s%u",
        name,
        instance
    );
    drm_dbg!(
        ptr::addr_of_mut!((*(*engine).i915).drm),
        "renamed %s to %s\n",
        old,
        (*engine).name
    );
}

// upstream: intel_engine_user.c intel_engines_driver_register()
pub unsafe fn intel_engines_driver_register(i915: *mut DrmI915Private) {
    let mut other_instance = 0u16;
    let mut ring = LegacyRing {
        gt: ptr::null_mut(),
        class: 0,
        instance: 0,
    };
    let mut engines = ListHead {
        next: ptr::null_mut(),
        prev: ptr::null_mut(),
    };
    INIT_LIST_HEAD(ptr::addr_of_mut!(engines));
    sort_engines(i915, ptr::addr_of_mut!(engines));

    let view = engine_uabi_view(i915);
    let mut prev: *mut RbNode = ptr::null_mut();
    let mut p = ptr::addr_of_mut!((*view).engines.tree.node);
    let mut it = engines.next;

    while !ptr::eq(it, ptr::addr_of_mut!(engines)) {
        let next = (*it).next;
        let engine = engine_from_uabi(it.cast());

        if crate::intel_gt_api_upstream::intel_gt_has_unrecoverable_error((*engine).gt) {
            it = next;
            continue;
        }

        GEM_BUG_ON!((*engine).class as usize >= 6);
        let uabi_class = UABI_CLASSES[(*engine).class as usize];
        (*engine).uabi_class = uabi_class;
        let name_instance = if uabi_class == I915_NO_UABI_CLASS {
            let current = other_instance;
            other_instance = other_instance.wrapping_add(1);
            current
        } else {
            GEM_BUG_ON!(uabi_class as usize >= I915_UABI_ENGINE_CLASS_COUNT);
            let count = ptr::addr_of_mut!((*view).engine_uabi_class_count[uabi_class as usize]);
            let current = *count;
            *count = current.wrapping_add(1);
            current as u16
        };

        (*engine).uabi_instance = name_instance;
        engine_rename(
            engine,
            intel_engine_class_repr((*engine).class),
            name_instance,
        );

        if (*engine).uabi_class == I915_NO_UABI_CLASS {
            it = next;
            continue;
        }

        let node = ptr::addr_of_mut!((*engine).uabi.uabi_node);
        // `rb_link_node()` is the header inline from include/linux/rbtree.h.
        (*node).parent_color = prev as usize;
        (*node).left = ptr::null_mut();
        (*node).right = ptr::null_mut();
        *p = node;
        rb_insert_color(
            node,
            ptr::addr_of_mut!((*view).engines.tree),
        );

        GEM_BUG_ON!(
            intel_engine_lookup_user(
                i915,
                (*engine).uabi_class as u8,
                (*engine).uabi_instance as u8
            ) != engine
        );

        add_legacy_ring(&mut ring, engine);

        prev = ptr::addr_of_mut!((*engine).uabi.uabi_node);
        p = ptr::addr_of_mut!((*prev).right);
        it = next;
    }

    if crate::linux_config::CONFIG_DRM_I915_SELFTEST
        && crate::linux_config::CONFIG_DRM_I915_DEBUG_GEM
    {
        let mut errors = 0;

        for class in 0..I915_UABI_ENGINE_CLASS_COUNT as i32 {
            let count = (*view).engine_uabi_class_count[class as usize] as i32;
            for inst in 0..count {
                let engine = intel_engine_lookup_user(i915, class as u8, inst as u8);
                if engine.is_null() {
                    pr_err!(
                        "UABI engine not found for { class:%d, instance:%d }\n",
                        class,
                        inst
                    );
                    errors += 1;
                    continue;
                }

                if (*engine).uabi_class as i32 != class || (*engine).uabi_instance as i32 != inst {
                    pr_err!(
                        "Wrong UABI engine:%s { class:%d, instance:%d } found for { class:%d, \
                         instance:%d }\n",
                        (*engine).name,
                        (*engine).uabi_class,
                        (*engine).uabi_instance,
                        class,
                        inst
                    );
                    errors += 1;
                    continue;
                }
            }
        }

        let isolation = intel_engines_has_context_isolation(i915);
        let mut node =
            crate::intel_engine_api_upstream::rb_first_uabi_engine(engine_uabi_tree(i915));
        while !node.is_null() {
            let engine = engine_from_uabi(node.cast());
            let bit = 1u32 << (*engine).uabi_class;
            let expected = if (*engine).default_state.is_null() {
                0
            } else {
                bit
            };

            if isolation & bit != expected {
                pr_err!(
                    "mismatching default context state for class %d on engine %s\n",
                    (*engine).uabi_class,
                    (*engine).name
                );
                errors += 1;
            }
            node = crate::linux::rbtree::rb_next(node);
        }

        if errors != 0 {
            drm_warn!(
                ptr::addr_of_mut!((*i915).drm),
                "Invalid UABI engine mapping found"
            );
            (*engine_uabi_tree(i915)).node = ptr::null_mut();
        }
    }

    set_scheduler_caps(i915);
}

// upstream: intel_engine_user.c intel_engines_has_context_isolation()
pub unsafe fn intel_engines_has_context_isolation(i915: *mut DrmI915Private) -> u32 {
    let mut which = 0u32;
    let mut node = crate::intel_engine_api_upstream::rb_first_uabi_engine(engine_uabi_tree(i915));

    while !node.is_null() {
        let engine = engine_from_uabi(node.cast());
        if !(*engine).default_state.is_null() {
            which |= 1u32 << (*engine).uabi_class;
        }
        node = crate::linux::rbtree::rb_next(node);
    }

    which
}
