// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
//
// Source-faithful Rust transcription of Linux 7.2.3
// drivers/gpu/drm/i915/gt/intel_engine_cs.c. Header-owned `IntelEngineCs` and
// `IntelGt` records are imported from their separate source-order type bindings;
// surrounding GEM, MMIO and kernel-framework services remain external binding
// points. Preserve
// source ordering, control flow, register accesses, timeout values and error
// ordering when integrating those services.

// Remaining binding boundary: the i915/GEM/GT object types, constants, helpers,
// iterators and kernel macros not represented by the header-layout records.

use core::{
    ffi::{c_char, c_ulong, c_void},
    mem::{offset_of, size_of},
    ops::{Deref, DerefMut},
};

// Engine IDs/classes/masks are owned by `intel_engine_types.h`.
pub use crate::intel_engine_types_upstream::{
    _BCS, _CCS, _VCS, _VECS, ALL_ENGINES, BCS0, BCS1, BCS2, BCS3, BCS4, BCS5, BCS6, BCS7, BCS8,
    CCS0, CCS1, CCS2, CCS3, COMPUTE_CLASS, COPY_ENGINE_CLASS, GSC0, I915_NUM_ENGINES,
    INVALID_ENGINE, IntelEngineId, IntelEngineMask, MAX_ENGINE_CLASS, MAX_ENGINE_INSTANCE,
    OTHER_CLASS, RCS0, RENDER_CLASS, VCS0, VCS1, VCS2, VCS3, VCS4, VCS5, VCS6, VCS7, VECS0, VECS1,
    VECS2, VECS3, VIDEO_DECODE_CLASS, VIDEO_ENHANCEMENT_CLASS, VIRTUAL_ENGINES,
};
use crate::{
    i915_gem_context_upstream::fput,
    i915_gem_object_header_upstream::{
        assert_object_held, i915_gem_object_has_pages, i915_gem_object_lock, i915_gem_object_put,
        i915_gem_object_unpin_map,
    },
    i915_gem_object_types_upstream::DrmI915GemObject,
    i915_gem_object_upstream::i915_gem_object_set_cache_coherency,
    i915_gem_pages_upstream::{i915_gem_object_pin_map, page_pack_bits},
    i915_gem_ww_upstream::{i915_gem_ww_ctx_backoff, i915_gem_ww_ctx_fini, i915_gem_ww_ctx_init},
    i915_request_types_upstream::*,
    i915_request_upstream::i915_request_slab_cache,
    i915_scheduler_types_upstream::*,
    i915_scheduler_upstream::i915_sched_engine_create,
    i915_vma_api_upstream::*,
    intel_breadcrumbs_types_upstream::IntelBreadcrumbs,
    intel_breadcrumbs_upstream::intel_engine_print_breadcrumbs,
    intel_context_types_upstream::{IntelWakerefT, *},
    intel_context_upstream::*,
    intel_engine_regs_upstream,
    intel_execlists_submission_upstream::{
        intel_execlists_dump_active_requests, intel_execlists_show_requests,
        intel_execlists_submission_setup,
    },
    intel_gt_api_upstream::intel_gt_check_and_clear_faults,
    intel_gt_mcr_upstream::*,
    intel_gtt_api_upstream::{
        i915_ggtt_has_aperture, i915_ggtt_require_binder, i915_vm_get, i915_vm_put,
    },
    intel_guc_fwif_types_upstream::{
        MAKE_GUC_ID, engine_class_to_guc_class, guc_policy_max_preempt_timeout_ms,
    },
    intel_guc_submission_types_upstream::{
        intel_guc_dump_active_requests, intel_guc_submission_is_wanted, intel_guc_submission_setup,
    },
    intel_ring_upstream::{i915_gem_object_create_internal, intel_ring_update_space},
    intel_sseu_types_upstream::{
        SseuDevInfo, intel_slicemask_from_xehp_dssmask, intel_sseu_from_device_info,
    },
    intel_timeline_types_upstream::IntelTimeline,
    intel_uc_types_upstream::{intel_uc_uses_guc_submission, intel_uc_wants_gsc_uc},
    intel_uncore_types_upstream::{
        __intel_wait_for_register_fw, IntelUncore, intel_uncore_posting_read_fw,
        intel_uncore_prune_engine_fw_domains, intel_uncore_read, intel_uncore_write,
        intel_uncore_write_fw,
    },
    intel_workarounds_types_upstream::I915WaList,
    intel_workarounds_upstream::{
        intel_engine_apply_whitelist, intel_engine_apply_workarounds, intel_engine_init_ctx_wa,
        intel_engine_init_whitelist, intel_engine_init_workarounds,
    },
    linux::{
        average::{ewma__engine_latency_init, ewma__engine_latency_read},
        i915::{CCS_MASK, HAS_ENGINE, HAS_EXECLISTS, RCS_MASK, VDBOX_MASK, VEBOX_MASK},
        primitives::hweight32,
    },
    linux_config::*,
    linux_heap::kmem_cache_free,
    linux_list::*,
};
pub use crate::{
    intel_engine_api_upstream::*,
    intel_engine_regs_upstream::*,
    intel_engine_types_upstream::{
        I915CtxWorkarounds, IntelEngineCs, IntelEngineExeclists, IntelEngineHeartbeat,
        IntelEngineLegacy, IntelEngineProps, IntelEngineResetOps, IntelEngineStats,
        IntelEngineTlbInv, IntelEngineTlbInvReg, IntelEngineUabi, IntelHwStatusPage,
        intel_engine_supports_stats,
    },
    intel_gt_types_upstream::{
        GtDefaults as IntelGtDefaults, IntelGt, IntelGtCcs, IntelGtInfo, IntelGtMocs,
        IntelGtRequests, IntelGtStats, IntelGtSteering, IntelGtTimelines, IntelGtTlb,
        IntelGtWatchdog,
    },
    intel_wakeref_types_upstream::{IntelWakeref, IntelWakerefOps},
};

// Layout bindings for the source structures in Linux v7.2.3
// drivers/gpu/drm/i915/gt/intel_engine_types.h and intel_gt_types.h.
// Framework-owned allocations remain raw pointers. Opaque by-value kernel
// subobjects below have the exact size/alignment from the x86_64 v7.2.3
// configuration used by this port; fields touched by the GT source are
// represented explicitly.

macro_rules! opaque_c_layout {
    ($name:ident, $bytes:expr,8) => {
        #[repr(C, align(8))]
        #[derive(Clone, Copy)]
        pub struct $name {
            _opaque: [u8; $bytes],
        }
    };
    ($name:ident, $bytes:expr,4) => {
        #[repr(C, align(4))]
        #[derive(Clone, Copy)]
        pub struct $name {
            _opaque: [u8; $bytes],
        }
    };
}

opaque_c_layout!(Kobject, 64, 8);
opaque_c_layout!(I915PerfGt, 40, 8);
opaque_c_layout!(Mutex, 24, 8);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ListHead {
    pub next: *mut ListHead,
    pub prev: *mut ListHead,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LlistNode {
    pub next: *mut LlistNode,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LlistHead {
    pub first: *mut LlistNode,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HlistNode {
    pub next: *mut HlistNode,
    pub pprev: *mut *mut HlistNode,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HlistHead {
    pub first: *mut HlistNode,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RbNode {
    pub parent_color: usize,
    pub right: *mut RbNode,
    pub left: *mut RbNode,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RbRoot {
    pub node: *mut RbNode,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RbRootCached {
    pub root: RbRoot,
    pub leftmost: *mut RbNode,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TimerList {
    pub entry: HlistNode,
    pub expires: c_ulong,
    pub function: Option<unsafe extern "C" fn(*mut TimerList)>,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WorkStruct {
    pub data: c_ulong,
    pub entry: ListHead,
    pub function: Option<unsafe extern "C" fn(*mut WorkStruct)>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DelayedWork {
    pub work: WorkStruct,
    pub timer: TimerList,
    pub workqueue: *mut c_void,
    pub cpu: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AtomicT {
    pub counter: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Seqcount {
    pub sequence: u32,
}

// CONFIG_LOCKDEP and CONFIG_PREEMPT_RT are disabled in the source kernel
// configuration used for this translation, so seqcount_mutex_t is seqcount_t.
pub type SeqcountMutex = Seqcount;

#[repr(C, align(4))]
#[derive(Clone, Copy)]
pub struct Spinlock {
    _opaque: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AtomicNotifierHead {
    pub lock: Spinlock,
    pub head: *mut c_void,
}

// Guard the two source-derived top-level bindings against accidental layout
// drift for the configured x86_64 Linux v7.2.3 structures.
const _: [(); 5496] = [(); size_of::<IntelEngineCs>()];
const _: [(); 5336] = [(); size_of::<IntelGt>()];
const _: [(); 176] = [(); size_of::<SseuDevInfo>()];
const _: [(); 216] = [(); size_of::<IntelGtInfo>()];
const _: [(); 224] = [(); size_of::<IntelEngineExeclists>()];
const _: [(); 144] = [(); size_of::<IntelWakeref>()];
const _: [(); 40] = [(); size_of::<I915WaList>()];
const _: [(); 16] = [(); size_of::<IntelEngineTlbInv>()];
const _: [(); 32] = [(); size_of::<IntelHwStatusPage>()];
const _: [(); 24] = [(); size_of::<I915CtxWorkarounds>()];
const _: [(); 960] = [(); offset_of!(IntelEngineCs, execlists)];
const _: [(); 576] = [(); offset_of!(IntelEngineCs, status_page)];
const _: [(); 5408] = [(); offset_of!(IntelEngineCs, props)];
const _: [(); 4024] = [(); offset_of!(IntelGt, engine)];
const _: [(); 4936] = [(); offset_of!(IntelGt, info)];

#[derive(Clone, Copy, Eq, PartialEq)]
enum TlbInvRegTable {
    None,
    Gen8,
    Gen12,
    Xehp,
    Xelpmp,
}

struct MeasureBreadcrumb {
    rq: I915Request,
    ring: IntelRing,
    cs: [u32; 2048],
}

const HSW_CXT_TOTAL_SIZE: u32 = 17 * PAGE_SIZE as u32;
const DEFAULT_LR_CONTEXT_RENDER_SIZE: u32 = 22 * PAGE_SIZE as u32;
const GEN8_LR_CONTEXT_RENDER_SIZE: u32 = 20 * PAGE_SIZE as u32;
const GEN9_LR_CONTEXT_RENDER_SIZE: u32 = 22 * PAGE_SIZE as u32;
const GEN11_LR_CONTEXT_RENDER_SIZE: u32 = 14 * PAGE_SIZE as u32;
const GEN8_LR_CONTEXT_OTHER_SIZE: u32 = 2 * PAGE_SIZE as u32;
const MAX_MMIO_BASES: usize = 3;
// C stores this enum's values in the header's u8 engine-info class field;
// these aliases preserve the cast required at the Rust ABI boundary.
const RENDER_CLASS_U8: u8 = RENDER_CLASS as u8;
const VIDEO_DECODE_CLASS_U8: u8 = VIDEO_DECODE_CLASS as u8;
const VIDEO_ENHANCEMENT_CLASS_U8: u8 = VIDEO_ENHANCEMENT_CLASS as u8;
const COPY_ENGINE_CLASS_U8: u8 = COPY_ENGINE_CLASS as u8;
const OTHER_CLASS_U8: u8 = OTHER_CLASS as u8;
const COMPUTE_CLASS_U8: u8 = COMPUTE_CLASS as u8;

#[derive(Clone, Copy)]
struct EngineMmioBase {
    graphics_ver: u8,
    base: u32,
}

#[derive(Clone, Copy)]
struct EngineInfo {
    class: u8,
    instance: u8,
    mmio_bases: [EngineMmioBase; MAX_MMIO_BASES],
}

const EMPTY_ENGINE_MMIO_BASE: EngineMmioBase = EngineMmioBase {
    graphics_ver: 0,
    base: 0,
};

static INTEL_ENGINES: [EngineInfo; I915_NUM_ENGINES as usize] = [
    EngineInfo {
        class: RENDER_CLASS as u8,
        instance: 0,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 1,
                base: RENDER_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: COPY_ENGINE_CLASS as u8,
        instance: 0,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 6,
                base: BLT_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: COPY_ENGINE_CLASS as u8,
        instance: 1,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: XEHPC_BCS1_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: COPY_ENGINE_CLASS as u8,
        instance: 2,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: XEHPC_BCS2_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: COPY_ENGINE_CLASS as u8,
        instance: 3,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: XEHPC_BCS3_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: COPY_ENGINE_CLASS as u8,
        instance: 4,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: XEHPC_BCS4_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: COPY_ENGINE_CLASS as u8,
        instance: 5,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: XEHPC_BCS5_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: COPY_ENGINE_CLASS as u8,
        instance: 6,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: XEHPC_BCS6_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: COPY_ENGINE_CLASS as u8,
        instance: 7,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: XEHPC_BCS7_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: COPY_ENGINE_CLASS as u8,
        instance: 8,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: XEHPC_BCS8_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: VIDEO_DECODE_CLASS as u8,
        instance: 0,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 11,
                base: GEN11_BSD_RING_BASE,
            },
            EngineMmioBase {
                graphics_ver: 6,
                base: GEN6_BSD_RING_BASE,
            },
            EngineMmioBase {
                graphics_ver: 4,
                base: BSD_RING_BASE,
            },
        ],
    },
    EngineInfo {
        class: VIDEO_DECODE_CLASS as u8,
        instance: 1,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 11,
                base: GEN11_BSD2_RING_BASE,
            },
            EngineMmioBase {
                graphics_ver: 8,
                base: GEN8_BSD2_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: VIDEO_DECODE_CLASS as u8,
        instance: 2,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 11,
                base: GEN11_BSD3_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: VIDEO_DECODE_CLASS as u8,
        instance: 3,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 11,
                base: GEN11_BSD4_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: VIDEO_DECODE_CLASS as u8,
        instance: 4,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: XEHP_BSD5_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: VIDEO_DECODE_CLASS as u8,
        instance: 5,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: XEHP_BSD6_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: VIDEO_DECODE_CLASS as u8,
        instance: 6,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: XEHP_BSD7_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: VIDEO_DECODE_CLASS as u8,
        instance: 7,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: XEHP_BSD8_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: VIDEO_ENHANCEMENT_CLASS as u8,
        instance: 0,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 11,
                base: GEN11_VEBOX_RING_BASE,
            },
            EngineMmioBase {
                graphics_ver: 7,
                base: VEBOX_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: VIDEO_ENHANCEMENT_CLASS as u8,
        instance: 1,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 11,
                base: GEN11_VEBOX2_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: VIDEO_ENHANCEMENT_CLASS as u8,
        instance: 2,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: XEHP_VEBOX3_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: VIDEO_ENHANCEMENT_CLASS as u8,
        instance: 3,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: XEHP_VEBOX4_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: COMPUTE_CLASS as u8,
        instance: 0,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: GEN12_COMPUTE0_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: COMPUTE_CLASS as u8,
        instance: 1,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: GEN12_COMPUTE1_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: COMPUTE_CLASS as u8,
        instance: 2,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: GEN12_COMPUTE2_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: COMPUTE_CLASS as u8,
        instance: 3,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: GEN12_COMPUTE3_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
    EngineInfo {
        class: OTHER_CLASS as u8,
        instance: OTHER_GSC_INSTANCE as u8,
        mmio_bases: [
            EngineMmioBase {
                graphics_ver: 12,
                base: MTL_GSC_RING_BASE,
            },
            EMPTY_ENGINE_MMIO_BASE,
            EMPTY_ENGINE_MMIO_BASE,
        ],
    },
];

// upstream: intel_engine_cs.c intel_engine_context_size()
pub unsafe fn intel_engine_context_size(gt: *mut IntelGt, class: u8) -> u32 {
    let uncore = (*gt).uncore;
    let mut cxt_size: u32;

    BUILD_BUG_ON!(I915_GTT_PAGE_SIZE != PAGE_SIZE);

    match class {
        COMPUTE_CLASS_U8 | RENDER_CLASS_U8 => match GRAPHICS_VER((*gt).i915) {
            12 | 11 => GEN11_LR_CONTEXT_RENDER_SIZE,
            9 => GEN9_LR_CONTEXT_RENDER_SIZE,
            8 => GEN8_LR_CONTEXT_RENDER_SIZE,
            7 => {
                if IS_HASWELL((*gt).i915) {
                    HSW_CXT_TOTAL_SIZE
                } else {
                    cxt_size = intel_uncore_read(uncore, GEN7_CXT_SIZE);
                    round_up(GEN7_CXT_TOTAL_SIZE(cxt_size) * 64, PAGE_SIZE as u32)
                }
            }
            6 => {
                cxt_size = intel_uncore_read(uncore, CXT_SIZE);
                round_up(GEN6_CXT_TOTAL_SIZE(cxt_size) * 64, PAGE_SIZE as u32)
            }
            5 | 4 => {
                cxt_size = intel_uncore_read(uncore, CXT_SIZE) + 1;
                gt_dbg!(
                    gt,
                    "graphics_ver = %d CXT_SIZE = %d bytes [0x%08x]\n",
                    GRAPHICS_VER((*gt).i915),
                    cxt_size * 64,
                    cxt_size - 1,
                );
                round_up(cxt_size * 64, PAGE_SIZE as u32)
            }
            3 | 2 | 1 => 0,
            ver => {
                MISSING_CASE!(ver);
                DEFAULT_LR_CONTEXT_RENDER_SIZE
            }
        },
        VIDEO_DECODE_CLASS_U8
        | VIDEO_ENHANCEMENT_CLASS_U8
        | COPY_ENGINE_CLASS_U8
        | OTHER_CLASS_U8 => {
            if GRAPHICS_VER((*gt).i915) < 8 {
                0
            } else {
                GEN8_LR_CONTEXT_OTHER_SIZE
            }
        }
        _ => {
            MISSING_CASE!(class);
            if GRAPHICS_VER((*gt).i915) < 8 {
                0
            } else {
                GEN8_LR_CONTEXT_OTHER_SIZE
            }
        }
    }
}

// upstream: intel_engine_cs.c __engine_mmio_base()
unsafe fn __engine_mmio_base(i915: *mut DrmI915Private, bases: *const EngineMmioBase) -> u32 {
    let mut i = 0;

    while i < MAX_MMIO_BASES {
        if GRAPHICS_VER(i915) >= (*bases.add(i)).graphics_ver {
            break;
        }
        i += 1;
    }

    GEM_BUG_ON!(i == MAX_MMIO_BASES);
    GEM_BUG_ON!((*bases.add(i)).base == 0);

    (*bases.add(i)).base
}

// upstream: intel_engine_cs.c __sprint_engine_name()
unsafe fn __sprint_engine_name(engine: *mut IntelEngineCs) {
    GEM_WARN_ON!(
        snprintf!(
            (*engine).name.as_mut_ptr(),
            size_of_val(&(*engine).name),
            "%s'%u",
            intel_engine_class_repr((*engine).class),
            (*engine).instance,
        ) >= size_of_val(&(*engine).name) as i32,
    );
}

// upstream: intel_engine_cs.c intel_engine_set_hwsp_writemask()
pub unsafe fn intel_engine_set_hwsp_writemask(engine: *mut IntelEngineCs, mask: u32) {
    if GRAPHICS_VER((*engine).i915) < 6 && (*engine).class as i32 != RENDER_CLASS {
        return;
    }

    if GRAPHICS_VER((*engine).i915) >= 3 {
        ENGINE_WRITE!(engine, intel_engine_regs_upstream::RING_HWSTAM, mask);
    } else {
        ENGINE_WRITE16!(engine, intel_engine_regs_upstream::RING_HWSTAM, mask as u16);
    }
}

// upstream: intel_engine_cs.c intel_engine_sanitize_mmio()
unsafe fn intel_engine_sanitize_mmio(engine: *mut IntelEngineCs) {
    intel_engine_set_hwsp_writemask(engine, !0u32);
}

// upstream: intel_engine_cs.c nop_irq_handler()
unsafe extern "C" fn nop_irq_handler(engine: *mut IntelEngineCs, iir: u16) {
    GEM_DEBUG_WARN_ON!(iir);
}

// upstream: intel_engine_cs.c get_reset_domain()
unsafe fn get_reset_domain(ver: u8, id: IntelEngineId) -> u32 {
    let reset_domain: u32;

    if ver >= 11 {
        reset_domain = match id {
            RCS0 => GEN11_GRDOM_RENDER,
            BCS0 => GEN11_GRDOM_BLT,
            BCS1 => XEHPC_GRDOM_BLT1,
            BCS2 => XEHPC_GRDOM_BLT2,
            BCS3 => XEHPC_GRDOM_BLT3,
            BCS4 => XEHPC_GRDOM_BLT4,
            BCS5 => XEHPC_GRDOM_BLT5,
            BCS6 => XEHPC_GRDOM_BLT6,
            BCS7 => XEHPC_GRDOM_BLT7,
            BCS8 => XEHPC_GRDOM_BLT8,
            VCS0 => GEN11_GRDOM_MEDIA,
            VCS1 => GEN11_GRDOM_MEDIA2,
            VCS2 => GEN11_GRDOM_MEDIA3,
            VCS3 => GEN11_GRDOM_MEDIA4,
            VCS4 => GEN11_GRDOM_MEDIA5,
            VCS5 => GEN11_GRDOM_MEDIA6,
            VCS6 => GEN11_GRDOM_MEDIA7,
            VCS7 => GEN11_GRDOM_MEDIA8,
            VECS0 => GEN11_GRDOM_VECS,
            VECS1 => GEN11_GRDOM_VECS2,
            VECS2 => GEN11_GRDOM_VECS3,
            VECS3 => GEN11_GRDOM_VECS4,
            CCS0 | CCS1 | CCS2 | CCS3 => GEN11_GRDOM_RENDER,
            GSC0 => GEN12_GRDOM_GSC,
            _ => 0,
        };
        GEM_BUG_ON!(id >= I915_NUM_ENGINES || reset_domain == 0);
    } else {
        reset_domain = match id {
            RCS0 => GEN6_GRDOM_RENDER,
            BCS0 => GEN6_GRDOM_BLT,
            VCS0 => GEN6_GRDOM_MEDIA,
            VCS1 => GEN8_GRDOM_MEDIA2,
            VECS0 => GEN6_GRDOM_VECS,
            _ => 0,
        };
        GEM_BUG_ON!(id >= I915_NUM_ENGINES || reset_domain == 0);
    }

    reset_domain
}

// upstream: intel_engine_cs.c intel_engine_setup()
unsafe fn intel_engine_setup(gt: *mut IntelGt, id: IntelEngineId, logical_instance: u8) -> i32 {
    let info = &INTEL_ENGINES[id as usize];
    let i915 = (*gt).i915;
    let mut engine: *mut IntelEngineCs;
    let guc_class: u8;

    BUILD_BUG_ON!(MAX_ENGINE_CLASS >= BIT!(GEN11_ENGINE_CLASS_WIDTH));
    BUILD_BUG_ON!(MAX_ENGINE_INSTANCE >= BIT!(GEN11_ENGINE_INSTANCE_WIDTH));
    BUILD_BUG_ON!(I915_MAX_VCS as usize > (MAX_ENGINE_INSTANCE as usize + 1));
    BUILD_BUG_ON!(I915_MAX_VECS as usize > (MAX_ENGINE_INSTANCE as usize + 1));

    if GEM_DEBUG_WARN_ON!(id as usize >= ARRAY_SIZE((*gt).engine)) {
        return -EINVAL;
    }
    if GEM_DEBUG_WARN_ON!(info.class as i32 > MAX_ENGINE_CLASS) {
        return -EINVAL;
    }
    if GEM_DEBUG_WARN_ON!(info.instance as i32 > MAX_ENGINE_INSTANCE) {
        return -EINVAL;
    }
    if GEM_DEBUG_WARN_ON!((*gt).engine_class[info.class as usize][info.instance as usize].is_null())
    {
        return -EINVAL;
    }

    engine = kzalloc_obj!(IntelEngineCs);
    if engine.is_null() {
        return -ENOMEM;
    }

    BUILD_BUG_ON!(BITS_PER_TYPE!((*engine).mask) < I915_NUM_ENGINES);

    INIT_LIST_HEAD!(&mut (*engine).pinned_contexts_list);
    (*engine).id = id;
    (*engine).legacy_idx = INVALID_ENGINE;
    (*engine).mask = BIT!(id as usize);
    (*engine).reset_domain = get_reset_domain(GRAPHICS_VER((*gt).i915), id);
    (*engine).i915 = i915;
    (*engine).gt = gt;
    (*engine).uncore = (*gt).uncore;
    guc_class = engine_class_to_guc_class(info.class);
    (*engine).guc_id = MAKE_GUC_ID(guc_class as u32, info.instance as u32);
    (*engine).mmio_base = __engine_mmio_base(i915, info.mmio_bases.as_ptr());

    (*engine).irq_handler = Some(nop_irq_handler);

    (*engine).class = info.class;
    (*engine).instance = info.instance;
    (*engine).logical_mask = BIT!(logical_instance);
    __sprint_engine_name(engine);

    if ((*engine).class as i32 == COMPUTE_CLASS || (*engine).class as i32 == RENDER_CLASS)
        && __ffs(CCS_MASK((*engine).gt) | RCS_MASK((*engine).gt)) as u8 == (*engine).instance
    {
        (*engine).flags |= I915_ENGINE_FIRST_RENDER_COMPUTE;
    }

    if (*engine).class as i32 == RENDER_CLASS || (*engine).class as i32 == COMPUTE_CLASS {
        (*engine).flags |= I915_ENGINE_HAS_RCS_REG_STATE;
        (*engine).flags |= I915_ENGINE_HAS_EU_PRIORITY;
    }

    (*engine).props.heartbeat_interval_ms = CONFIG_DRM_I915_HEARTBEAT_INTERVAL;
    (*engine).props.max_busywait_duration_ns = CONFIG_DRM_I915_MAX_REQUEST_BUSYWAIT;
    (*engine).props.preempt_timeout_ms = CONFIG_DRM_I915_PREEMPT_TIMEOUT;
    (*engine).props.stop_timeout_ms = CONFIG_DRM_I915_STOP_TIMEOUT;
    (*engine).props.timeslice_duration_ms = CONFIG_DRM_I915_TIMESLICE_DURATION;

    if GRAPHICS_VER(i915) == 12 && ((*engine).flags & I915_ENGINE_HAS_RCS_REG_STATE) != 0 {
        (*engine).props.preempt_timeout_ms = CONFIG_DRM_I915_PREEMPT_TIMEOUT_COMPUTE;
    }

    // CLAMP_PROP(field): clamp the configured engine property and report a change.
    macro_rules! clamp_prop {
        ($field:ident, $clamp_fn:ident) => {{
            let clamp = $clamp_fn(engine, (*engine).props.$field);
            if clamp != (*engine).props.$field {
                drm_notice!(
                    &mut (*(*engine).i915).drm,
                    "Warning, clamping %s to %lld to prevent overflow\n",
                    stringify!($field),
                    clamp as i64,
                );
                (*engine).props.$field = clamp;
            }
        }};
    }
    clamp_prop!(heartbeat_interval_ms, intel_clamp_heartbeat_interval_ms);
    clamp_prop!(
        max_busywait_duration_ns,
        intel_clamp_max_busywait_duration_ns
    );
    clamp_prop!(preempt_timeout_ms, intel_clamp_preempt_timeout_ms);
    clamp_prop!(stop_timeout_ms, intel_clamp_stop_timeout_ms);
    clamp_prop!(timeslice_duration_ms, intel_clamp_timeslice_duration_ms);

    (*engine).defaults = (*engine).props;

    (*engine).context_size = intel_engine_context_size(gt, (*engine).class);
    if WARN_ON!((*engine).context_size > BIT!(20)) {
        (*engine).context_size = 0;
    }
    if (*engine).context_size != 0 {
        DRIVER_CAPS!(i915).has_logical_contexts = true;
    }

    ewma__engine_latency_init(&mut (*engine).latency);
    ATOMIC_INIT_NOTIFIER_HEAD!(&mut (*engine).context_status_notifier);

    intel_engine_sanitize_mmio(engine);

    (*gt).engine_class[info.class as usize][info.instance as usize] = engine;
    (*gt).engine[id as usize] = engine;

    0
}

// upstream: intel_engine_cs.c intel_clamp_heartbeat_interval_ms()
pub unsafe fn intel_clamp_heartbeat_interval_ms(
    _engine: *mut IntelEngineCs,
    mut value: u64,
) -> u64 {
    value = core::cmp::min(value, jiffies_to_msecs(MAX_SCHEDULE_TIMEOUT) as u64);
    value
}

// upstream: intel_engine_cs.c intel_clamp_max_busywait_duration_ns()
pub unsafe fn intel_clamp_max_busywait_duration_ns(
    _engine: *mut IntelEngineCs,
    mut value: u64,
) -> u64 {
    value = core::cmp::min(value, jiffies_to_nsecs(2u32));
    value
}

// upstream: intel_engine_cs.c intel_clamp_preempt_timeout_ms()
pub unsafe fn intel_clamp_preempt_timeout_ms(engine: *mut IntelEngineCs, mut value: u64) -> u64 {
    if intel_guc_submission_is_wanted(gt_to_guc((*engine).gt)) {
        value = core::cmp::min(value, guc_policy_max_preempt_timeout_ms() as u64);
    }
    value = core::cmp::min(value, jiffies_to_msecs(MAX_SCHEDULE_TIMEOUT) as u64);
    value
}

// upstream: intel_engine_cs.c intel_clamp_stop_timeout_ms()
pub unsafe fn intel_clamp_stop_timeout_ms(_engine: *mut IntelEngineCs, mut value: u64) -> u64 {
    value = core::cmp::min(value, jiffies_to_msecs(MAX_SCHEDULE_TIMEOUT) as u64);
    value
}

// upstream: intel_engine_cs.c intel_clamp_timeslice_duration_ms()
pub unsafe fn intel_clamp_timeslice_duration_ms(engine: *mut IntelEngineCs, mut value: u64) -> u64 {
    if intel_guc_submission_is_wanted(gt_to_guc((*engine).gt)) {
        value = core::cmp::min(value, guc_policy_max_exec_quantum_ms() as u64);
    }
    value = core::cmp::min(value, jiffies_to_msecs(MAX_SCHEDULE_TIMEOUT) as u64);
    value
}

// upstream: intel_engine_cs.c __setup_engine_capabilities()
unsafe fn __setup_engine_capabilities(engine: *mut IntelEngineCs) {
    let i915 = (*engine).i915;

    if (*engine).class as i32 == VIDEO_DECODE_CLASS {
        if GRAPHICS_VER(i915) >= 11 || (GRAPHICS_VER(i915) >= 9 && (*engine).instance == 0) {
            (*engine).uabi_capabilities |= I915_VIDEO_CLASS_CAPABILITY_HEVC;
        }

        if (GRAPHICS_VER(i915) >= 11
            && ((*(*engine).gt).info.vdbox_sfc_access & BIT!((*engine).instance)) != 0)
            || (GRAPHICS_VER(i915) >= 9 && (*engine).instance == 0)
        {
            (*engine).uabi_capabilities |= I915_VIDEO_AND_ENHANCE_CLASS_CAPABILITY_SFC;
        }
    } else if (*engine).class as i32 == VIDEO_ENHANCEMENT_CLASS {
        if GRAPHICS_VER(i915) >= 9
            && ((*(*engine).gt).info.sfc_mask & BIT!((*engine).instance)) != 0
        {
            (*engine).uabi_capabilities |= I915_VIDEO_AND_ENHANCE_CLASS_CAPABILITY_SFC;
        }
    }
}

// upstream: intel_engine_cs.c intel_setup_engine_capabilities()
unsafe fn intel_setup_engine_capabilities(gt: *mut IntelGt) {
    let mut engine: *mut IntelEngineCs;
    let mut id: IntelEngineId;

    for_each_engine!(engine, id, gt, {
        __setup_engine_capabilities(engine);
    });
}

// upstream: intel_engine_cs.c intel_engines_release()
pub unsafe fn intel_engines_release(gt: *mut IntelGt) {
    let mut engine: *mut IntelEngineCs;
    let mut id: IntelEngineId;

    GEM_BUG_ON!(intel_gt_pm_is_awake(gt));
    if !intel_gt_gpu_reset_clobbers_display(gt) {
        intel_gt_reset_all_engines(gt);
    }

    for_each_engine!(engine, id, gt, {
        if (*engine).release.is_none() {
            continue;
        }

        intel_wakeref_wait_for_idle(&mut (*engine).wakeref);
        GEM_BUG_ON!(intel_engine_pm_is_awake(engine));

        ((*engine).release.unwrap())(engine);
        (*engine).release = None;

        memset(
            &mut (*engine).reset as *mut _ as *mut c_void,
            0,
            size_of_val(&(*engine).reset),
        );
    });

    llist_del_all(&mut (*(*gt).i915).uabi_engines_llist);
}

// upstream: intel_engine_cs.c intel_engine_free_request_pool()
pub unsafe fn intel_engine_free_request_pool(engine: *mut IntelEngineCs) {
    if (*engine).request_pool.is_null() {
        return;
    }

    kmem_cache_free(
        i915_request_slab_cache(),
        (*engine).request_pool.cast::<c_void>(),
    );
}

// upstream: intel_engine_cs.c intel_engines_free()
pub unsafe fn intel_engines_free(gt: *mut IntelGt) {
    let mut engine: *mut IntelEngineCs;
    let mut id: IntelEngineId;

    rcu_barrier();

    for_each_engine!(engine, id, gt, {
        intel_engine_free_request_pool(engine);
        kfree(engine);
        (*gt).engine[id as usize] = core::ptr::null_mut();
    });
}

// upstream: intel_engine_cs.c gen11_vdbox_has_sfc()
unsafe fn gen11_vdbox_has_sfc(
    gt: *mut IntelGt,
    physical_vdbox: u32,
    logical_vdbox: u32,
    vdbox_mask: u16,
) -> bool {
    let i915 = (*gt).i915;

    if ((*gt).info.sfc_mask & BIT!(physical_vdbox / 2)) == 0 {
        false
    } else if MEDIA_VER(i915) >= 12 {
        (physical_vdbox % 2 == 0) || ((BIT!(physical_vdbox - 1) & vdbox_mask) == 0)
    } else if MEDIA_VER(i915) == 11 {
        logical_vdbox % 2 == 0
    } else {
        false
    }
}

// upstream: intel_engine_cs.c engine_mask_apply_media_fuses()
unsafe fn engine_mask_apply_media_fuses(gt: *mut IntelGt) {
    let i915 = (*gt).i915;
    let mut logical_vdbox = 0u32;
    let mut i: usize;
    let mut media_fuse: u32;
    let mut fuse1: u32;
    let mut vdbox_mask: u16;
    let mut vebox_mask: u16;

    if MEDIA_VER((*gt).i915) < 11 {
        return;
    }

    media_fuse = intel_uncore_read((*gt).uncore, GEN11_GT_VEBOX_VDBOX_DISABLE);
    if MEDIA_VER_FULL(i915) < IP_VER(12, 55) {
        media_fuse = !media_fuse;
    }

    vdbox_mask = REG_FIELD_GET(GEN11_GT_VDBOX_DISABLE_MASK, media_fuse) as u16;
    vebox_mask = REG_FIELD_GET(GEN11_GT_VEBOX_DISABLE_MASK, media_fuse) as u16;

    if MEDIA_VER_FULL(i915) >= IP_VER(12, 55) {
        fuse1 = intel_uncore_read((*gt).uncore, HSW_PAVP_FUSE1);
        (*gt).info.sfc_mask = REG_FIELD_GET(XEHP_SFC_ENABLE_MASK, fuse1) as u8;
    } else {
        (*gt).info.sfc_mask = !0;
    }

    for i in 0..I915_MAX_VCS {
        if !HAS_ENGINE(gt, _VCS(i as i32)) {
            vdbox_mask &= !(BIT!(i) as u16);
            continue;
        }

        if (BIT!(i) & vdbox_mask) == 0 {
            (*gt).info.engine_mask &= !BIT!(_VCS(i as i32));
            gt_dbg!(gt, "vcs%u fused off\n", i);
            continue;
        }

        if gen11_vdbox_has_sfc(gt, i as u32, logical_vdbox as u32, vdbox_mask) {
            (*gt).info.vdbox_sfc_access |= BIT!(i) as u8;
        }
        logical_vdbox += 1;
    }
    gt_dbg!(
        gt,
        "vdbox enable: %04x, instances: %04lx\n",
        vdbox_mask,
        VDBOX_MASK(gt)
    );
    GEM_BUG_ON!(vdbox_mask as u32 != VDBOX_MASK(gt));

    for i in 0..I915_MAX_VECS {
        if !HAS_ENGINE(gt, _VECS(i as i32)) {
            vebox_mask &= !(BIT!(i) as u16);
            continue;
        }

        if (BIT!(i) & vebox_mask) == 0 {
            (*gt).info.engine_mask &= !BIT!(_VECS(i as i32));
            gt_dbg!(gt, "vecs%u fused off\n", i);
        }
    }
    gt_dbg!(
        gt,
        "vebox enable: %04x, instances: %04lx\n",
        vebox_mask,
        VEBOX_MASK(gt)
    );
    GEM_BUG_ON!(vebox_mask as u32 != VEBOX_MASK(gt));
}

// upstream: intel_engine_cs.c engine_mask_apply_compute_fuses()
unsafe fn engine_mask_apply_compute_fuses(gt: *mut IntelGt) {
    let i915 = (*gt).i915;
    let info = &mut (*gt).info;
    let ss_per_ccs = info.sseu.max_subslices / I915_MAX_CCS as u8;
    let mut ccs_mask: u64;
    let mut i: usize;

    if GRAPHICS_VER(i915) < 11 {
        return;
    }

    if hweight32(CCS_MASK(gt) as u32) <= 1 {
        return;
    }

    ccs_mask = intel_slicemask_from_xehp_dssmask(info.sseu.compute_subslice_mask, ss_per_ccs as i32)
        as u64;
    for_each_clear_bit!(i, &ccs_mask, I915_MAX_CCS, {
        info.engine_mask &= !BIT!(_CCS(i as i32));
        gt_dbg!(gt, "ccs%u fused off\n", i);
    });
}

// upstream: intel_engine_cs.c init_engine_mask()
unsafe fn init_engine_mask(gt: *mut IntelGt) -> IntelEngineMask {
    let info = &mut (*gt).info;

    GEM_BUG_ON!(info.engine_mask == 0);

    engine_mask_apply_media_fuses(gt);
    engine_mask_apply_compute_fuses(gt);

    if info.engine_mask & BIT!(GSC0 as usize) != 0 && !intel_uc_wants_gsc_uc(&mut (*gt).uc) {
        gt_notice!(gt, "No GSC FW selected, disabling GSC CS and media C6\n");
        info.engine_mask &= !BIT!(GSC0);
    }

    if IS_DG2((*gt).i915) {
        let first_ccs = __ffs(CCS_MASK(gt));
        (*gt).ccs.cslices = CCS_MASK(gt);

        info.engine_mask &= !GENMASK!(CCS3, CCS0);
        info.engine_mask |= BIT!(_CCS(first_ccs as i32));
    }

    info.engine_mask
}

// upstream: intel_engine_cs.c populate_logical_ids()
unsafe fn populate_logical_ids(
    gt: *mut IntelGt,
    logical_ids: *mut u8,
    class: u8,
    map: *const u8,
    num_instances: u8,
) {
    let mut current_logical_id = 0u8;

    for j in 0..num_instances {
        for i in 0..INTEL_ENGINES.len() {
            if !HAS_ENGINE(gt, i) || INTEL_ENGINES[i].class != class {
                continue;
            }

            if INTEL_ENGINES[i].instance == *map.add(j as usize) {
                *logical_ids.add(INTEL_ENGINES[i].instance as usize) = current_logical_id;
                current_logical_id += 1;
                break;
            }
        }
    }
}

// upstream: intel_engine_cs.c setup_logical_ids()
unsafe fn setup_logical_ids(gt: *mut IntelGt, logical_ids: *mut u8, class: u8) {
    if MEDIA_VER((*gt).i915) >= 11 && class as i32 == VIDEO_DECODE_CLASS {
        let map: [u8; 8] = [0, 2, 4, 6, 1, 3, 5, 7];
        populate_logical_ids(gt, logical_ids, class, map.as_ptr(), ARRAY_SIZE(map) as u8);
    } else {
        let mut map: [u8; MAX_ENGINE_INSTANCE as usize + 1] = [0; MAX_ENGINE_INSTANCE as usize + 1];
        for i in 0..MAX_ENGINE_INSTANCE as usize + 1 {
            map[i] = i as u8;
        }
        populate_logical_ids(gt, logical_ids, class, map.as_ptr(), ARRAY_SIZE(map) as u8);
    }
}

// upstream: intel_engine_cs.c intel_engines_init_mmio()
pub unsafe fn intel_engines_init_mmio(gt: *mut IntelGt) -> i32 {
    let i915 = (*gt).i915;
    let engine_mask = init_engine_mask(gt);
    let mut mask: u32 = 0;
    let mut i: usize;
    let mut logical_ids: [u8; MAX_ENGINE_INSTANCE as usize + 1] =
        [0; MAX_ENGINE_INSTANCE as usize + 1];
    let mut err;

    drm_WARN_ON!(&mut (*i915).drm, engine_mask == 0);
    drm_WARN_ON!(
        &mut (*i915).drm,
        engine_mask & GENMASK!(BITS_PER_TYPE!(mask) - 1, I915_NUM_ENGINES) != 0,
    );

    err = (|| {
        for class in 0..MAX_ENGINE_CLASS + 1 {
            setup_logical_ids(gt, logical_ids.as_mut_ptr(), class as u8);

            for i in 0..INTEL_ENGINES.len() {
                let instance = INTEL_ENGINES[i].instance;

                if INTEL_ENGINES[i].class != class as u8 || !HAS_ENGINE(gt, i) {
                    continue;
                }

                let setup_err = intel_engine_setup(gt, i as i32, logical_ids[instance as usize]);
                if setup_err != 0 {
                    return setup_err;
                }

                mask |= BIT!(i) as u32;
            }
        }
        0
    })();
    if err != 0 {
        intel_engines_free(gt);
        return err;
    }

    if drm_WARN_ON!(&mut (*i915).drm, mask != engine_mask) {
        (*gt).info.engine_mask = mask;
    }

    (*gt).info.num_engines = hweight32(mask) as u8;

    intel_gt_check_and_clear_faults(gt);
    intel_setup_engine_capabilities(gt);
    intel_uncore_prune_engine_fw_domains((*gt).uncore, gt);

    0
}

// upstream: intel_engine_cs.c intel_engine_init_execlists()
pub unsafe fn intel_engine_init_execlists(engine: *mut IntelEngineCs) {
    let execlists = &mut (*engine).execlists;

    execlists.port_mask = 1;
    GEM_BUG_ON!(!is_power_of_2(
        crate::intel_engine_api_upstream::execlists_num_ports(execlists) as u32
    ));
    GEM_BUG_ON!(
        crate::intel_engine_api_upstream::execlists_num_ports(execlists)
            > EXECLIST_MAX_PORTS as u32
    );

    memset(
        execlists.pending.as_mut_ptr() as *mut c_void,
        0,
        size_of_val(&execlists.pending),
    );
    execlists.active = memset(
        execlists.inflight.as_mut_ptr() as *mut c_void,
        0,
        size_of_val(&execlists.inflight),
    )
    .cast::<*mut I915Request>() as *const *mut I915Request;
}

// upstream: intel_engine_cs.c cleanup_status_page()
unsafe fn cleanup_status_page(engine: *mut IntelEngineCs) {
    let mut vma: *mut I915Vma;

    intel_engine_set_hwsp_writemask(engine, !0u32);

    vma = fetch_and_zero(&mut (*engine).status_page.vma);
    if vma.is_null() {
        return;
    }

    if !HWS_NEEDS_PHYSICAL((*engine).i915) {
        i915_vma_unpin(vma);
    }

    i915_gem_object_unpin_map((*vma).obj);
    i915_gem_object_put((*vma).obj);
}

// upstream: intel_engine_cs.c pin_ggtt_status_page()
unsafe fn pin_ggtt_status_page(
    engine: *mut IntelEngineCs,
    ww: *mut I915GemWwCtx,
    vma: *mut I915Vma,
) -> i32 {
    let flags;

    if !HAS_LLC((*engine).i915) && i915_ggtt_has_aperture((*(*engine).gt).ggtt) {
        flags = PIN_MAPPABLE;
    } else {
        flags = PIN_HIGH;
    }

    i915_ggtt_pin(vma, ww, 0, flags as u32)
}

// upstream: intel_engine_cs.c init_status_page()
unsafe fn init_status_page(engine: *mut IntelEngineCs) -> i32 {
    let mut obj: *mut DrmI915GemObject;
    let mut ww: I915GemWwCtx = core::mem::zeroed();
    let mut vma: *mut I915Vma;
    let mut vaddr: *mut c_void;
    let mut ret: i32;

    INIT_LIST_HEAD!(&mut (*engine).status_page.timelines);

    obj = i915_gem_object_create_internal((*engine).i915, PAGE_SIZE as u64);
    if IS_ERR(obj) {
        gt_err!((*engine).gt, "Failed to allocate status page\n");
        return PTR_ERR(obj);
    }

    i915_gem_object_set_cache_coherency(obj, I915_CACHE_LLC);

    vma = i915_vma_instance(obj, &mut (*(*(*engine).gt).ggtt).vm, core::ptr::null_mut());
    if IS_ERR(vma) {
        ret = PTR_ERR(vma);
        if ret != 0 {
            i915_gem_object_put(obj);
        }
        return ret;
    }

    i915_gem_ww_ctx_init(&mut ww, true);
    loop {
        ret = i915_gem_object_lock(obj, &mut ww);
        if ret == 0 && !HWS_NEEDS_PHYSICAL((*engine).i915) {
            ret = pin_ggtt_status_page(engine, &mut ww, vma);
        }
        if ret != 0 {
            if ret == -EDEADLK {
                ret = i915_gem_ww_ctx_backoff(&mut ww);
                if ret == 0 {
                    continue;
                }
            }
            i915_gem_ww_ctx_fini(&mut ww);
            if ret != 0 {
                i915_gem_object_put(obj);
            }
            return ret;
        }

        vaddr = i915_gem_object_pin_map(obj, I915_MAP_WB);
        if IS_ERR(vaddr) {
            ret = PTR_ERR(vaddr);
            if ret != 0 {
                i915_vma_unpin(vma);
            }
            if ret == -EDEADLK {
                ret = i915_gem_ww_ctx_backoff(&mut ww);
                if ret == 0 {
                    continue;
                }
            }
            i915_gem_ww_ctx_fini(&mut ww);
            if ret != 0 {
                i915_gem_object_put(obj);
            }
            return ret;
        }

        (*engine).status_page.addr = memset(vaddr, 0, PAGE_SIZE).cast::<u32>();
        (*engine).status_page.vma = vma;
        i915_gem_ww_ctx_fini(&mut ww);
        return 0;
    }
}

// upstream: intel_engine_cs.c intel_engine_init_tlb_invalidation()
unsafe fn intel_engine_init_tlb_invalidation(engine: *mut IntelEngineCs) -> i32 {
    let i915 = (*engine).i915;
    let instance = (*engine).instance;
    let class = (*engine).class;
    let mut table = TlbInvRegTable::None;
    let mut num = 0;
    let mut reg: IntelEngineTlbInvReg = core::mem::zeroed();
    let mut val: u32;

    if (*(*engine).gt).type_ == GT_MEDIA {
        if MEDIA_VER_FULL(i915) == IP_VER(13, 0) {
            table = TlbInvRegTable::Xelpmp;
            num = OTHER_CLASS + 1;
        }
    } else if GRAPHICS_VER_FULL(i915) == IP_VER(12, 74)
        || GRAPHICS_VER_FULL(i915) == IP_VER(12, 71)
        || GRAPHICS_VER_FULL(i915) == IP_VER(12, 70)
        || GRAPHICS_VER_FULL(i915) == IP_VER(12, 55)
    {
        table = TlbInvRegTable::Xehp;
        num = COMPUTE_CLASS + 1;
    } else if GRAPHICS_VER_FULL(i915) == IP_VER(12, 0) || GRAPHICS_VER_FULL(i915) == IP_VER(12, 10)
    {
        table = TlbInvRegTable::Gen12;
        num = COMPUTE_CLASS + 1;
    } else if GRAPHICS_VER(i915) >= 8 && GRAPHICS_VER(i915) <= 11 {
        table = TlbInvRegTable::Gen8;
        num = COPY_ENGINE_CLASS + 1;
    } else if GRAPHICS_VER(i915) < 8 {
        return 0;
    }

    if gt_WARN_ONCE!(
        (*engine).gt,
        num == 0,
        "Platform does not implement TLB invalidation!"
    ) {
        return -ENODEV;
    }

    match table {
        TlbInvRegTable::Gen8 => match class {
            RENDER_CLASS_U8 => reg.reg = GEN8_RTCR,
            VIDEO_DECODE_CLASS_U8 => reg.reg = GEN8_M1TCR,
            VIDEO_ENHANCEMENT_CLASS_U8 => reg.reg = GEN8_VTCR,
            COPY_ENGINE_CLASS_U8 => reg.reg = GEN8_BTCR,
            _ => (),
        },
        TlbInvRegTable::Gen12 => match class {
            RENDER_CLASS_U8 => reg.reg = GEN12_GFX_TLB_INV_CR,
            VIDEO_DECODE_CLASS_U8 => reg.reg = GEN12_VD_TLB_INV_CR,
            VIDEO_ENHANCEMENT_CLASS_U8 => reg.reg = GEN12_VE_TLB_INV_CR,
            COPY_ENGINE_CLASS_U8 => reg.reg = GEN12_BLT_TLB_INV_CR,
            COMPUTE_CLASS_U8 => reg.reg = GEN12_COMPCTX_TLB_INV_CR,
            _ => (),
        },
        TlbInvRegTable::Xehp => match class {
            RENDER_CLASS_U8 => reg.mcr_reg = XEHP_GFX_TLB_INV_CR,
            VIDEO_DECODE_CLASS_U8 => reg.mcr_reg = XEHP_VD_TLB_INV_CR,
            VIDEO_ENHANCEMENT_CLASS_U8 => reg.mcr_reg = XEHP_VE_TLB_INV_CR,
            COPY_ENGINE_CLASS_U8 => reg.mcr_reg = XEHP_BLT_TLB_INV_CR,
            COMPUTE_CLASS_U8 => reg.mcr_reg = XEHP_COMPCTX_TLB_INV_CR,
            _ => (),
        },
        TlbInvRegTable::Xelpmp => match class {
            VIDEO_DECODE_CLASS_U8 => reg.reg = GEN12_VD_TLB_INV_CR,
            VIDEO_ENHANCEMENT_CLASS_U8 => reg.reg = GEN12_VE_TLB_INV_CR,
            OTHER_CLASS_U8 => reg.reg = XELPMP_GSC_TLB_INV_CR,
            _ => (),
        },
        TlbInvRegTable::None => (),
    }

    if gt_WARN_ON_ONCE!(
        (*engine).gt,
        class as i32 >= num || (reg.reg.reg == 0 && reg.mcr_reg.reg == 0),
    ) {
        return -ERANGE;
    }

    if table == TlbInvRegTable::Xelpmp && class as i32 == OTHER_CLASS {
        GEM_WARN_ON!(instance as u32 != OTHER_GSC_INSTANCE);
        val = 1;
    } else if table == TlbInvRegTable::Gen8 && class as i32 == VIDEO_DECODE_CLASS && instance == 1 {
        reg.reg = GEN8_M2TCR;
        val = 0;
    } else {
        val = instance as u32;
    }

    val = BIT!(val);

    (*engine).tlb_inv.mcr = table == TlbInvRegTable::Xehp;
    (*engine).tlb_inv.reg = reg;
    (*engine).tlb_inv.done = val;

    if GRAPHICS_VER(i915) >= 12
        && ((*engine).class as i32 == VIDEO_DECODE_CLASS
            || (*engine).class as i32 == VIDEO_ENHANCEMENT_CLASS
            || (*engine).class as i32 == COMPUTE_CLASS
            || (*engine).class as i32 == OTHER_CLASS)
    {
        (*engine).tlb_inv.request = REG_MASKED_FIELD_ENABLE!(val);
    } else {
        (*engine).tlb_inv.request = val;
    }

    0
}

// upstream: intel_engine_cs.c engine_setup_common()
unsafe fn engine_setup_common(engine: *mut IntelEngineCs) -> i32 {
    let mut err: i32;

    init_llist_head(&mut (*engine).barrier_tasks);

    err = intel_engine_init_tlb_invalidation(engine);
    if err != 0 {
        return err;
    }

    err = init_status_page(engine);
    if err != 0 {
        return err;
    }

    (*engine).breadcrumbs = intel_breadcrumbs_create(engine);
    if (*engine).breadcrumbs.is_null() {
        err = -ENOMEM;
        cleanup_status_page(engine);
        return err;
    }

    (*engine).sched_engine =
        i915_sched_engine_create(crate::intel_engine_api_upstream::ENGINE_PHYSICAL as u32);
    if (*engine).sched_engine.is_null() {
        err = -ENOMEM;
        intel_breadcrumbs_put((*engine).breadcrumbs);
        cleanup_status_page(engine);
        return err;
    }
    (*(*engine).sched_engine).private_data = engine.cast::<c_void>();

    err = intel_engine_init_cmd_parser(engine);
    if err != 0 {
        i915_sched_engine_put((*engine).sched_engine);
        intel_breadcrumbs_put((*engine).breadcrumbs);
        cleanup_status_page(engine);
        return err;
    }

    intel_engine_init_execlists(engine);
    intel_engine_init__pm(engine);
    intel_engine_init_retire(engine);

    (*engine).sseu = intel_sseu_from_device_info(&(*(*engine).gt).info.sseu);

    intel_engine_init_workarounds(engine);
    intel_engine_init_whitelist(engine);
    intel_engine_init_ctx_wa(engine);

    if GRAPHICS_VER((*engine).i915) >= 12 {
        (*engine).flags |= I915_ENGINE_HAS_RELATIVE_MMIO;
    }

    return 0;
}

// upstream: intel_engine_cs.c measure_breadcrumb_dw()
unsafe fn measure_breadcrumb_dw(ce: *mut IntelContext) -> i32 {
    let engine = (*ce).engine;
    let mut frame: *mut MeasureBreadcrumb;
    let mut dw: i32;

    GEM_BUG_ON!((*(*engine).gt).scratch.is_null());

    frame = kzalloc_obj!(MeasureBreadcrumb);
    if frame.is_null() {
        return -ENOMEM;
    }

    (*frame).rq.i915 = (*engine).i915;
    (*frame).rq.engine = engine;
    (*frame).rq.context = ce;
    rcu_assign_pointer!(&mut (*frame).rq.timeline, (*ce).timeline);
    (*frame).rq.hwsp_seqno = (*(*ce).timeline).hwsp_seqno;

    (*frame).ring.vaddr = (*frame).cs.as_mut_ptr().cast::<c_void>();
    (*frame).ring.size = size_of_val(&(*frame).cs) as u32;
    (*frame).ring.wrap = BITS_PER_TYPE!((*frame).ring.size) - ilog2((*frame).ring.size);
    (*frame).ring.effective_size = (*frame).ring.size as u32;
    intel_ring_update_space(&mut (*frame).ring);
    (*frame).rq.ring = &mut (*frame).ring;

    mutex_lock(&mut (*(*ce).timeline).mutex);
    spin_lock_irq(&mut (*(*engine).sched_engine).lock);

    dw = ((*engine).emit_fini_breadcrumb.unwrap())(&mut (*frame).rq, (*frame).cs.as_mut_ptr())
        .offset_from((*frame).cs.as_mut_ptr()) as i32;

    spin_unlock_irq(&mut (*(*engine).sched_engine).lock);
    mutex_unlock(&mut (*(*ce).timeline).mutex);

    GEM_BUG_ON!(dw & 1 != 0);

    kfree(frame);
    dw
}

// upstream: intel_engine_cs.c intel_engine_create_pinned_context()
pub unsafe fn intel_engine_create_pinned_context(
    engine: *mut IntelEngineCs,
    vm: *mut I915AddressSpace,
    ring_size: u32,
    hwsp: u32,
    key: *mut crate::intel_engine_api_upstream::LockClassKey,
    name: *const c_char,
) -> *mut IntelContext {
    let mut ce: *mut IntelContext;
    let mut err: i32;

    ce = intel_context_create(engine);
    if IS_ERR(ce) {
        return ce;
    }

    __set_bit(CONTEXT_BARRIER_BIT, &mut (*ce).flags);
    (*ce).timeline = page_pack_bits(core::ptr::null_mut(), hwsp).cast::<IntelTimeline>();
    (*ce).ring = core::ptr::null_mut();
    (*ce).ring_size = ring_size;

    i915_vm_put((*ce).vm);
    (*ce).vm = i915_vm_get(vm);

    err = intel_context_pin(ce);
    if err != 0 {
        intel_context_put(ce);
        return ERR_PTR(err);
    }

    list_add_tail(
        &mut (*ce).pinned_contexts_link,
        &mut (*engine).pinned_contexts_list,
    );
    lockdep_set_class_and_name(&mut (*(*ce).timeline).mutex, key, name);

    ce
}

// upstream: intel_engine_cs.c intel_engine_destroy_pinned_context()
pub unsafe fn intel_engine_destroy_pinned_context(ce: *mut IntelContext) {
    let engine = (*ce).engine;
    let hwsp = (*engine).status_page.vma;

    GEM_BUG_ON!((*(*ce).timeline).hwsp_ggtt != hwsp);

    mutex_lock(&mut (*(*hwsp).vm).mutex);
    list_del(&mut (*(*ce).timeline).engine_link);
    mutex_unlock(&mut (*(*hwsp).vm).mutex);

    list_del(&mut (*ce).pinned_contexts_link);
    intel_context_unpin(ce);
    intel_context_put(ce);
}

// upstream: intel_engine_cs.c create_ggtt_bind_context()
unsafe fn create_ggtt_bind_context(engine: *mut IntelEngineCs) -> *mut IntelContext {
    static mut KERNEL: crate::intel_engine_api_upstream::LockClassKey =
        unsafe { core::mem::zeroed() };

    intel_engine_create_pinned_context(
        engine,
        (*(*engine).gt).vm,
        SZ_512K as u32,
        crate::intel_engine_api_upstream::I915_GEM_HWS_GGTT_BIND_ADDR as u32,
        &mut KERNEL,
        c"ggtt_bind_context".as_ptr(),
    )
}

// upstream: intel_engine_cs.c create_kernel_context()
unsafe fn create_kernel_context(engine: *mut IntelEngineCs) -> *mut IntelContext {
    static mut KERNEL: crate::intel_engine_api_upstream::LockClassKey =
        unsafe { core::mem::zeroed() };

    intel_engine_create_pinned_context(
        engine,
        (*(*engine).gt).vm,
        SZ_4K as u32,
        crate::intel_engine_api_upstream::I915_GEM_HWS_SEQNO_ADDR as u32,
        &mut KERNEL,
        c"kernel_context".as_ptr(),
    )
}

// upstream: intel_engine_cs.c engine_init_common()
unsafe fn engine_init_common(engine: *mut IntelEngineCs) -> i32 {
    let mut ce: *mut IntelContext;
    let mut bce: *mut IntelContext = core::ptr::null_mut();
    let mut ret: i32;

    ((*engine).set_default_submission.unwrap())(engine);

    ce = create_kernel_context(engine);
    if IS_ERR(ce) {
        return PTR_ERR(ce);
    }

    if i915_ggtt_require_binder((*engine).i915) && (*engine).id == BCS0 {
        bce = create_ggtt_bind_context(engine);
        if IS_ERR(bce) {
            ret = PTR_ERR(bce);
            intel_engine_destroy_pinned_context(ce);
            return ret;
        }
    }

    ret = measure_breadcrumb_dw(ce);
    if ret < 0 {
        if !bce.is_null() {
            intel_engine_destroy_pinned_context(bce);
        }
        intel_engine_destroy_pinned_context(ce);
        return ret;
    }

    (*engine).emit_fini_breadcrumb_dw = ret as u32;
    (*engine).kernel_context = ce;
    (*engine).bind_context = bce;

    0
}

// upstream: intel_engine_cs.c intel_engines_init()
pub unsafe fn intel_engines_init(gt: *mut IntelGt) -> i32 {
    let mut setup: unsafe extern "C" fn(*mut IntelEngineCs) -> i32;
    let mut engine: *mut IntelEngineCs;
    let mut id: IntelEngineId;
    let mut err: i32;

    if unsafe { intel_uc_uses_guc_submission(core::ptr::addr_of_mut!((*gt).uc)) } {
        (*gt).submission_method = INTEL_SUBMISSION_GUC;
        setup = intel_guc_submission_setup;
    } else if unsafe { HAS_EXECLISTS((*gt).i915) } {
        (*gt).submission_method = INTEL_SUBMISSION_ELSP;
        setup = intel_execlists_submission_setup;
    } else {
        (*gt).submission_method = INTEL_SUBMISSION_RING;
        setup = intel_ring_submission_setup;
    }

    for_each_engine!(engine, id, gt, {
        err = engine_setup_common(engine);
        if err != 0 {
            return err;
        }

        err = setup(engine);
        if err != 0 {
            intel_engine_cleanup_common(engine);
            return err;
        }

        GEM_BUG_ON!((*engine).release.is_none());

        err = engine_init_common(engine);
        if err != 0 {
            return err;
        }

        intel_engine_add_user(engine);
    });

    0
}

// upstream: intel_engine_cs.c intel_engine_cleanup_common()
pub unsafe fn intel_engine_cleanup_common(engine: *mut IntelEngineCs) {
    GEM_BUG_ON!(!list_empty(&(*(*engine).sched_engine).requests));

    i915_sched_engine_put((*engine).sched_engine);
    intel_breadcrumbs_put((*engine).breadcrumbs);

    intel_engine_fini_retire(engine);
    intel_engine_cleanup_cmd_parser(engine);

    if !(*engine).default_state.is_null() {
        fput((*engine).default_state.cast());
    }

    if !(*engine).kernel_context.is_null() {
        intel_engine_destroy_pinned_context((*engine).kernel_context);
    }

    if !(*engine).bind_context.is_null() {
        intel_engine_destroy_pinned_context((*engine).bind_context);
    }

    GEM_BUG_ON!(!llist_empty(&(*engine).barrier_tasks));
    cleanup_status_page(engine);

    intel_wa_list_free(&mut (*engine).ctx_wa_list);
    intel_wa_list_free(&mut (*engine).wa_list);
    intel_wa_list_free(&mut (*engine).whitelist);
}

// upstream: intel_engine_cs.c intel_engine_resume()
pub unsafe fn intel_engine_resume(engine: *mut IntelEngineCs) -> i32 {
    intel_engine_apply_workarounds(engine);
    intel_engine_apply_whitelist(engine);
    ((*engine).resume.unwrap())(engine)
}

// upstream: intel_engine_cs.c intel_engine_get_active_head()
pub unsafe fn intel_engine_get_active_head(engine: *const IntelEngineCs) -> u64 {
    let i915 = (*engine).i915;
    let acthd: u64;

    if GRAPHICS_VER(i915) >= 8 {
        acthd = ENGINE_READ64!(
            engine,
            intel_engine_regs_upstream::RING_ACTHD,
            intel_engine_regs_upstream::RING_ACTHD_UDW
        );
    } else if GRAPHICS_VER(i915) >= 4 {
        acthd = ENGINE_READ!(engine, intel_engine_regs_upstream::RING_ACTHD);
    } else {
        acthd = ENGINE_READ!(engine, ACTHD);
    }

    acthd
}

// upstream: intel_engine_cs.c intel_engine_get_last_batch_head()
pub unsafe fn intel_engine_get_last_batch_head(engine: *const IntelEngineCs) -> u64 {
    let bbaddr: u64;

    if GRAPHICS_VER((*engine).i915) >= 8 {
        bbaddr = ENGINE_READ64!(
            engine,
            intel_engine_regs_upstream::RING_BBADDR,
            intel_engine_regs_upstream::RING_BBADDR_UDW
        );
    } else {
        bbaddr = ENGINE_READ!(engine, intel_engine_regs_upstream::RING_BBADDR);
    }

    bbaddr
}

// upstream: intel_engine_cs.c stop_timeout()
unsafe fn stop_timeout(engine: *const IntelEngineCs) -> c_ulong {
    if in_atomic() || irqs_disabled() {
        return 0;
    }

    READ_ONCE!((*engine).props.stop_timeout_ms)
}

// upstream: intel_engine_cs.c __intel_engine_stop_cs()
unsafe fn __intel_engine_stop_cs(
    engine: *mut IntelEngineCs,
    fast_timeout_us: i32,
    slow_timeout_ms: i32,
) -> i32 {
    let uncore = (*engine).uncore;
    let mode = intel_engine_regs_upstream::RING_MI_MODE((*engine).mmio_base);
    let mut err: i32;

    intel_uncore_write_fw(
        uncore,
        mode,
        REG_MASKED_FIELD_ENABLE!(intel_engine_regs_upstream::STOP_RING),
    );

    if intel_engine_reset_needs_wa_22011802037((*engine).gt) {
        intel_uncore_write_fw(
            uncore,
            intel_engine_regs_upstream::RING_MODE_GEN7((*engine).mmio_base),
            REG_MASKED_FIELD_ENABLE!(intel_engine_regs_upstream::GEN12_GFX_PREFETCH_DISABLE),
        );
    }

    err = __intel_wait_for_register_fw(
        (*engine).uncore,
        mode,
        intel_engine_regs_upstream::MODE_IDLE,
        intel_engine_regs_upstream::MODE_IDLE,
        fast_timeout_us as u32,
        slow_timeout_ms as u32,
        core::ptr::null_mut(),
    );

    intel_uncore_posting_read_fw(uncore, mode);
    err
}

// upstream: intel_engine_cs.c intel_engine_stop_cs()
pub unsafe fn intel_engine_stop_cs(engine: *mut IntelEngineCs) -> i32 {
    let mut err = 0;

    if GRAPHICS_VER((*engine).i915) < 3 {
        return -ENODEV;
    }

    ENGINE_TRACE!(engine, "\n");
    if __intel_engine_stop_cs(engine, 1000, stop_timeout(engine) as i32) != 0 {
        ENGINE_TRACE!(
            engine,
            "timed out on intel_engine_regs_upstream::STOP_RING -> IDLE; HEAD:%04x, TAIL:%04x\n",
            ENGINE_READ_FW!(engine, intel_engine_regs_upstream::RING_HEAD)
                & intel_engine_regs_upstream::HEAD_ADDR,
            ENGINE_READ_FW!(engine, intel_engine_regs_upstream::RING_TAIL)
                & intel_engine_regs_upstream::TAIL_ADDR,
        );

        if (ENGINE_READ_FW!(engine, intel_engine_regs_upstream::RING_HEAD)
            & intel_engine_regs_upstream::HEAD_ADDR)
            != (ENGINE_READ_FW!(engine, intel_engine_regs_upstream::RING_TAIL)
                & intel_engine_regs_upstream::TAIL_ADDR)
        {
            err = -ETIMEDOUT;
        }
    }

    err
}

// upstream: intel_engine_cs.c intel_engine_cancel_stop_cs()
pub unsafe fn intel_engine_cancel_stop_cs(engine: *mut IntelEngineCs) {
    ENGINE_TRACE!(engine, "\n");
    ENGINE_WRITE_FW!(
        engine,
        intel_engine_regs_upstream::RING_MI_MODE,
        REG_MASKED_FIELD_DISABLE!(intel_engine_regs_upstream::STOP_RING)
    );
}

// upstream: intel_engine_cs.c __cs_pending_mi_force_wakes()
unsafe fn __cs_pending_mi_force_wakes(engine: *mut IntelEngineCs) -> u32 {
    let reg = match (*engine).id {
        RCS0 | CCS0 | CCS1 | CCS2 | CCS3 => MSG_IDLE_CS,
        BCS0 => MSG_IDLE_BCS,
        VCS0 => MSG_IDLE_VCS0,
        VCS1 => MSG_IDLE_VCS1,
        VCS2 => MSG_IDLE_VCS2,
        VCS3 => MSG_IDLE_VCS3,
        VCS4 => MSG_IDLE_VCS4,
        VCS5 => MSG_IDLE_VCS5,
        VCS6 => MSG_IDLE_VCS6,
        VCS7 => MSG_IDLE_VCS7,
        VECS0 => MSG_IDLE_VECS0,
        VECS1 => MSG_IDLE_VECS1,
        VECS2 => MSG_IDLE_VECS2,
        VECS3 => MSG_IDLE_VECS3,
        _ => I915_REG(0),
    };
    let val: u32;

    if reg.reg == 0 {
        return 0;
    }

    val = intel_uncore_read((*engine).uncore, reg);
    (val & (val >> 16) & MSG_IDLE_FW_MASK) >> MSG_IDLE_FW_SHIFT
}

// upstream: intel_engine_cs.c __gpm_wait_for_fw_complete()
unsafe fn __gpm_wait_for_fw_complete(gt: *mut IntelGt, fw_mask: u32) {
    let mut ret: i32;

    udelay(1);

    ret = __intel_wait_for_register_fw(
        (*gt).uncore,
        GEN9_PWRGT_DOMAIN_STATUS,
        fw_mask,
        fw_mask,
        5000,
        0,
        core::ptr::null_mut(),
    );

    udelay(1);

    if ret != 0 {
        GT_TRACE!(gt, "Failed to complete pending forcewake %d\n", ret);
    }
}

// upstream: intel_engine_cs.c intel_engine_wait_for_pending_mi_fw()
pub unsafe fn intel_engine_wait_for_pending_mi_fw(engine: *mut IntelEngineCs) {
    let fw_pending = __cs_pending_mi_force_wakes(engine);

    if fw_pending != 0 {
        __gpm_wait_for_fw_complete((*engine).gt, fw_pending);
    }
}

// upstream: intel_engine_cs.c intel_engine_get_instdone()
pub unsafe fn intel_engine_get_instdone(
    engine: *const IntelEngineCs,
    instdone: *mut IntelInstdone,
) {
    let i915 = (*engine).i915;
    let uncore = (*engine).uncore;
    let mmio_base = (*engine).mmio_base;
    let mut slice;
    let mut subslice;
    let mut iter;

    memset(instdone as *mut c_void, 0, size_of::<IntelInstdone>());

    if GRAPHICS_VER(i915) >= 8 {
        (*instdone).instdone =
            intel_uncore_read(uncore, intel_engine_regs_upstream::RING_INSTDONE(mmio_base));

        if (*engine).id != RCS0 {
            return;
        }

        (*instdone).slice_common = intel_uncore_read(uncore, GEN7_SC_INSTDONE);
        if GRAPHICS_VER(i915) >= 12 {
            (*instdone).slice_common_extra[0] = intel_uncore_read(uncore, GEN12_SC_INSTDONE_EXTRA);
            (*instdone).slice_common_extra[1] = intel_uncore_read(uncore, GEN12_SC_INSTDONE_EXTRA2);
        }

        crate::for_each_ss_steering!(iter, (*engine).gt, slice, subslice, {
            (*instdone).sampler[slice as usize][subslice as usize] = intel_gt_mcr_read(
                (*engine).gt,
                GEN8_SAMPLER_INSTDONE,
                slice as i32,
                subslice as i32,
            );
            (*instdone).row[slice as usize][subslice as usize] = intel_gt_mcr_read(
                (*engine).gt,
                GEN8_ROW_INSTDONE,
                slice as i32,
                subslice as i32,
            );
        });

        if GRAPHICS_VER_FULL(i915) >= IP_VER(12, 55) {
            crate::for_each_ss_steering!(iter, (*engine).gt, slice, subslice, {
                (*instdone).geom_svg[slice as usize][subslice as usize] = intel_gt_mcr_read(
                    (*engine).gt,
                    XEHPG_INSTDONE_GEOM_SVG,
                    slice as i32,
                    subslice as i32,
                );
            });
        }
    } else if GRAPHICS_VER(i915) >= 7 {
        (*instdone).instdone =
            intel_uncore_read(uncore, intel_engine_regs_upstream::RING_INSTDONE(mmio_base));

        if (*engine).id != RCS0 {
            return;
        }

        (*instdone).slice_common = intel_uncore_read(uncore, GEN7_SC_INSTDONE);
        (*instdone).sampler[0][0] = intel_uncore_read(uncore, GEN7_SAMPLER_INSTDONE);
        (*instdone).row[0][0] = intel_uncore_read(uncore, GEN7_ROW_INSTDONE);
    } else if GRAPHICS_VER(i915) >= 4 {
        (*instdone).instdone =
            intel_uncore_read(uncore, intel_engine_regs_upstream::RING_INSTDONE(mmio_base));
        if (*engine).id == RCS0 {
            (*instdone).slice_common = intel_uncore_read(uncore, GEN4_INSTDONE1);
        }
    } else {
        (*instdone).instdone = intel_uncore_read(uncore, GEN2_INSTDONE);
    }
}

// upstream: intel_engine_cs.c ring_is_idle()
unsafe fn ring_is_idle(engine: *mut IntelEngineCs) -> bool {
    let mut idle = true;

    if I915_SELFTEST_ONLY!((*engine).mmio_base == 0) {
        return true;
    }

    if !intel_engine_pm_get_if_awake(engine) {
        return true;
    }

    if (ENGINE_READ!(engine, intel_engine_regs_upstream::RING_HEAD)
        & intel_engine_regs_upstream::HEAD_ADDR)
        != (ENGINE_READ!(engine, intel_engine_regs_upstream::RING_TAIL)
            & intel_engine_regs_upstream::TAIL_ADDR)
    {
        idle = false;
    }

    if GRAPHICS_VER((*engine).i915) > 2
        && (ENGINE_READ!(engine, intel_engine_regs_upstream::RING_MI_MODE)
            & intel_engine_regs_upstream::MODE_IDLE)
            == 0
    {
        idle = false;
    }

    intel_engine_pm_put(engine);

    idle
}

// upstream: intel_engine_cs.c __intel_engine_flush_submission()
pub unsafe fn __intel_engine_flush_submission(engine: *mut IntelEngineCs, sync: bool) {
    let t = &mut (*(*engine).sched_engine).tasklet;

    if t.callbacks.callback.is_none() {
        return;
    }

    local_bh_disable();
    if tasklet_trylock(t) {
        if __tasklet_is_enabled(t) {
            (t.callbacks.callback.unwrap())(t);
        }
        tasklet_unlock(t);
    }
    local_bh_enable();

    if sync {
        tasklet_unlock_wait(t);
    }
}

// upstream: intel_engine_cs.c intel_engine_is_idle()
pub unsafe fn intel_engine_is_idle(engine: *mut IntelEngineCs) -> bool {
    if intel_gt_is_wedged((*engine).gt) {
        return true;
    }

    if !intel_engine_pm_is_awake(engine) {
        return true;
    }

    intel_synchronize_hardirq((*engine).i915);
    intel_engine_flush_submission(engine);

    if !i915_sched_engine_is_empty((*engine).sched_engine) {
        return false;
    }

    ring_is_idle(engine)
}

// upstream: intel_engine_cs.c intel_engines_are_idle()
pub unsafe fn intel_engines_are_idle(gt: *mut IntelGt) -> bool {
    let mut engine: *mut IntelEngineCs;
    let mut id: IntelEngineId;

    if intel_gt_is_wedged(gt) {
        return true;
    }

    if READ_ONCE!((*gt).awake).is_null() {
        return true;
    }

    for_each_engine!(engine, id, gt, {
        if !intel_engine_is_idle(engine) {
            return false;
        }
    });

    true
}

// upstream: intel_engine_cs.c intel_engine_irq_enable()
pub unsafe fn intel_engine_irq_enable(engine: *mut IntelEngineCs) -> bool {
    if (*engine).irq_enable.is_none() {
        return false;
    }

    spin_lock(&mut *(*(*engine).gt).irq_lock);
    ((*engine).irq_enable.unwrap())(engine);
    spin_unlock(&mut *(*(*engine).gt).irq_lock);

    true
}

// upstream: intel_engine_cs.c intel_engine_irq_disable()
pub unsafe fn intel_engine_irq_disable(engine: *mut IntelEngineCs) {
    if (*engine).irq_disable.is_none() {
        return;
    }

    spin_lock(&mut *(*(*engine).gt).irq_lock);
    ((*engine).irq_disable.unwrap())(engine);
    spin_unlock(&mut *(*(*engine).gt).irq_lock);
}

// upstream: intel_engine_cs.c intel_engines_reset_default_submission()
pub unsafe fn intel_engines_reset_default_submission(gt: *mut IntelGt) {
    let mut engine: *mut IntelEngineCs;
    let mut id: IntelEngineId;

    for_each_engine!(engine, id, gt, {
        if let Some(sanitize) = (*engine).sanitize {
            sanitize(engine);
        }

        if let Some(set_default_submission) = (*engine).set_default_submission {
            set_default_submission(engine);
        }
    });
}

// upstream: intel_engine_cs.c intel_engine_can_store_dword()
pub unsafe fn intel_engine_can_store_dword(engine: *mut IntelEngineCs) -> bool {
    match GRAPHICS_VER((*engine).i915) {
        2 => false,
        3 => !(IS_I915G((*engine).i915) || IS_I915GM((*engine).i915)),
        4 => !IS_I965G((*engine).i915),
        6 => (*engine).class as i32 != VIDEO_DECODE_CLASS,
        _ => true,
    }
}

// upstream: intel_engine_cs.c get_timeline()
unsafe fn get_timeline(rq: *mut I915Request) -> *mut IntelTimeline {
    let mut tl: *mut IntelTimeline;

    rcu_read_lock();
    tl = rcu_dereference!((*rq).timeline);
    if !kref_get_unless_zero(&mut (*tl).kref) {
        tl = core::ptr::null_mut();
    }
    rcu_read_unlock();

    tl
}

// upstream: intel_engine_cs.c print_ring()
unsafe fn print_ring(buf: *mut c_char, sz: i32, rq: *mut I915Request) -> i32 {
    let mut len = 0;

    if !i915_request_signaled(rq) {
        let tl = get_timeline(rq);

        len = scnprintf!(
            buf,
            sz as usize,
            "ring:{start:%08x, hwsp:%08x, seqno:%08x, runtime:%llums}, ",
            i915_ggtt_offset((*(*rq).ring).vma),
            if !tl.is_null() { (*tl).hwsp_offset } else { 0 },
            hwsp_seqno(rq),
            DIV_ROUND_CLOSEST_ULL(
                intel_context_get_total_runtime_ns((*rq).context),
                1000 * 1000,
            ),
        );

        if !tl.is_null() {
            intel_timeline_put(tl);
        }
    }

    len
}

// upstream: intel_engine_cs.c hexdump()
unsafe fn hexdump(m: *mut DrmPrinter, buf: *const c_void, len: usize) {
    let rowsize = 8 * size_of::<u32>();
    let mut prev: *const c_void = core::ptr::null();
    let bytes = buf.cast::<u8>();
    let mut skip = false;
    let mut pos;

    pos = 0;
    while pos < len {
        let mut line = [0 as c_char; 128];

        if !prev.is_null() && memcmp(prev, bytes.add(pos).cast::<c_void>(), rowsize) == 0 {
            if !skip {
                drm_printf!(m, "*\n");
                skip = true;
            }
            pos += rowsize;
            continue;
        }

        WARN_ON_ONCE!(
            hex_dump_to_buffer(
                bytes.add(pos).cast::<c_void>(),
                len - pos,
                rowsize,
                size_of::<u32>(),
                line.as_mut_ptr(),
                size_of_val(&line),
                false,
            ) >= size_of_val(&line),
        );
        drm_printf!(m, "[%04zx] %s\n", pos, line.as_ptr());

        prev = bytes.add(pos).cast::<c_void>();
        skip = false;
        pos += rowsize;
    }
}

// upstream: intel_engine_cs.c repr_timer()
unsafe fn repr_timer(t: *const TimerList) -> *const c_char {
    if READ_ONCE!((*t).expires) == 0 {
        return c"inactive".as_ptr();
    }

    if timer_pending(t) {
        return c"active".as_ptr();
    }

    c"expired".as_ptr()
}

// upstream: intel_engine_cs.c intel_engine_print_registers()
unsafe fn intel_engine_print_registers(engine: *mut IntelEngineCs, m: *mut DrmPrinter) {
    let i915 = (*engine).i915;
    let execlists = &mut (*engine).execlists;
    let mut addr: u64;

    if (*engine).id == RENDER_CLASS && IS_GRAPHICS_VER(i915, 4, 7) {
        drm_printf!(
            m,
            "\tCCID: 0x%08x\n",
            ENGINE_READ!(engine, intel_engine_regs_upstream::CCID)
        );
    }
    if HAS_EXECLISTS(i915) {
        drm_printf!(
            m,
            "\tEL_STAT_HI: 0x%08x\n",
            ENGINE_READ!(engine, intel_engine_regs_upstream::RING_EXECLIST_STATUS_HI),
        );
        drm_printf!(
            m,
            "\tEL_STAT_LO: 0x%08x\n",
            ENGINE_READ!(engine, RING_EXECLIST_STATUS_LO),
        );
    }
    drm_printf!(
        m,
        "\tRING_START: 0x%08x\n",
        ENGINE_READ!(engine, intel_engine_regs_upstream::RING_START)
    );
    drm_printf!(
        m,
        "\tRING_HEAD:  0x%08x\n",
        ENGINE_READ!(engine, intel_engine_regs_upstream::RING_HEAD)
            & intel_engine_regs_upstream::HEAD_ADDR
    );
    drm_printf!(
        m,
        "\tRING_TAIL:  0x%08x\n",
        ENGINE_READ!(engine, intel_engine_regs_upstream::RING_TAIL)
            & intel_engine_regs_upstream::TAIL_ADDR
    );
    drm_printf!(
        m,
        "\tRING_CTL:   0x%08x%s\n",
        ENGINE_READ!(engine, intel_engine_regs_upstream::RING_CTL),
        if ENGINE_READ!(engine, intel_engine_regs_upstream::RING_CTL)
            & (intel_engine_regs_upstream::RING_WAIT
                | intel_engine_regs_upstream::RING_WAIT_SEMAPHORE)
            != 0
        {
            " [waiting]"
        } else {
            ""
        },
    );
    if GRAPHICS_VER((*engine).i915) > 2 {
        drm_printf!(
            m,
            "\tRING_MODE:  0x%08x%s\n",
            ENGINE_READ!(engine, intel_engine_regs_upstream::RING_MI_MODE),
            if ENGINE_READ!(engine, intel_engine_regs_upstream::RING_MI_MODE)
                & intel_engine_regs_upstream::MODE_IDLE
                != 0
            {
                " [idle]"
            } else {
                ""
            },
        );
    }

    if GRAPHICS_VER(i915) >= 6 {
        drm_printf!(
            m,
            "\tRING_IMR:   0x%08x\n",
            ENGINE_READ!(engine, intel_engine_regs_upstream::RING_IMR)
        );
        drm_printf!(
            m,
            "\tRING_ESR:   0x%08x\n",
            ENGINE_READ!(engine, intel_engine_regs_upstream::RING_ESR)
        );
        drm_printf!(
            m,
            "\tRING_EMR:   0x%08x\n",
            ENGINE_READ!(engine, intel_engine_regs_upstream::RING_EMR)
        );
        drm_printf!(
            m,
            "\tRING_EIR:   0x%08x\n",
            ENGINE_READ!(engine, intel_engine_regs_upstream::RING_EIR)
        );
    }

    addr = intel_engine_get_active_head(engine);
    drm_printf!(
        m,
        "\tACTHD:  0x%08x_%08x\n",
        upper_32_bits(addr),
        lower_32_bits(addr)
    );
    addr = intel_engine_get_last_batch_head(engine);
    drm_printf!(
        m,
        "\tBBADDR: 0x%08x_%08x\n",
        upper_32_bits(addr),
        lower_32_bits(addr)
    );
    if GRAPHICS_VER(i915) >= 8 {
        addr = ENGINE_READ64!(
            engine,
            intel_engine_regs_upstream::RING_DMA_FADD,
            intel_engine_regs_upstream::RING_DMA_FADD_UDW
        );
    } else if GRAPHICS_VER(i915) >= 4 {
        addr = ENGINE_READ!(engine, intel_engine_regs_upstream::RING_DMA_FADD);
    } else {
        addr = ENGINE_READ!(engine, DMA_FADD_I8XX);
    }
    drm_printf!(
        m,
        "\tDMA_FADDR: 0x%08x_%08x\n",
        upper_32_bits(addr),
        lower_32_bits(addr)
    );
    if GRAPHICS_VER(i915) >= 4 {
        drm_printf!(
            m,
            "\tIPEIR: 0x%08x\n",
            ENGINE_READ!(engine, intel_engine_regs_upstream::RING_IPEIR)
        );
        drm_printf!(
            m,
            "\tIPEHR: 0x%08x\n",
            ENGINE_READ!(engine, intel_engine_regs_upstream::RING_IPEHR)
        );
    } else {
        drm_printf!(m, "\tIPEIR: 0x%08x\n", ENGINE_READ!(engine, IPEIR));
        drm_printf!(m, "\tIPEHR: 0x%08x\n", ENGINE_READ!(engine, IPEHR));
    }

    if HAS_EXECLISTS(i915) && !crate::intel_engine_api_upstream::intel_engine_uses_guc(engine) {
        let mut port: *const *mut I915Request;
        let mut rq: *mut I915Request;
        let hws = (*engine)
            .status_page
            .addr
            .add(crate::intel_engine_api_upstream::I915_HWS_CSB_BUF0_INDEX as usize);
        let num_entries = execlists.csb_size;
        let mut idx: u32;
        let mut read: u8;
        let mut write: u8;

        drm_printf!(
            m,
            "\tExeclist tasklet queued? %s (%s), preempt? %s, timeslice? %s\n",
            str_yes_no(test_bit(
                TASKLET_STATE_SCHED,
                &(*(*engine).sched_engine).tasklet.state
            )),
            str_enabled_disabled(!atomic_read(&(*(*engine).sched_engine).tasklet.count)),
            repr_timer(&execlists.preempt),
            repr_timer(&execlists.timer),
        );

        read = execlists.csb_head;
        write = READ_ONCE!(*execlists.csb_write) as u8;

        drm_printf!(
            m,
            "\tExeclist status: 0x%08x %08x; CSB read:%d, write:%d, entries:%d\n",
            ENGINE_READ!(engine, RING_EXECLIST_STATUS_LO),
            ENGINE_READ!(engine, intel_engine_regs_upstream::RING_EXECLIST_STATUS_HI),
            read,
            write,
            num_entries,
        );

        if read >= num_entries {
            read = 0;
        }
        if write >= num_entries {
            write = 0;
        }
        if read > write {
            write += num_entries;
        }
        while read < write {
            read += 1;
            idx = read as u32 % num_entries as u32;
            drm_printf!(
                m,
                "\tExeclist CSB[%d]: 0x%08x, context: %d\n",
                idx,
                *hws.add((idx * 2) as usize),
                *hws.add((idx * 2 + 1) as usize),
            );
        }

        i915_sched_engine_active_lock_bh((*engine).sched_engine);
        rcu_read_lock();
        port = execlists.active;
        while {
            rq = *port;
            !rq.is_null()
        } {
            let mut hdr = [0 as c_char; 160];
            let mut len: i32;

            len = scnprintf!(
                hdr.as_mut_ptr(),
                size_of_val(&hdr),
                "\t\tActive[%d]:  ccid:%08x%s%s, ",
                port.offset_from(execlists.active) as i32,
                (*(*rq).context).lrc.ccid,
                if intel_context_is_closed((*rq).context) {
                    "!"
                } else {
                    ""
                },
                if intel_context_is_banned((*rq).context) {
                    "*"
                } else {
                    ""
                },
            );
            len += print_ring(
                hdr.as_mut_ptr().add(len as usize),
                (size_of_val(&hdr) as i32) - len,
                rq,
            );
            scnprintf!(
                hdr.as_mut_ptr().add(len as usize),
                (size_of_val(&hdr) as i32) - len,
                "rq: ",
            );
            i915_request_show(m, rq, hdr.as_ptr(), 0);
            port = port.add(1);
        }

        port = execlists.pending.as_ptr();
        while {
            rq = *port;
            !rq.is_null()
        } {
            let mut hdr = [0 as c_char; 160];
            let mut len: i32;

            len = scnprintf!(
                hdr.as_mut_ptr(),
                size_of_val(&hdr),
                "\t\tPending[%d]: ccid:%08x%s%s, ",
                port.offset_from(execlists.pending.as_ptr()) as i32,
                (*(*rq).context).lrc.ccid,
                if intel_context_is_closed((*rq).context) {
                    "!"
                } else {
                    ""
                },
                if intel_context_is_banned((*rq).context) {
                    "*"
                } else {
                    ""
                },
            );
            len += print_ring(
                hdr.as_mut_ptr().add(len as usize),
                (size_of_val(&hdr) as i32) - len,
                rq,
            );
            scnprintf!(
                hdr.as_mut_ptr().add(len as usize),
                (size_of_val(&hdr) as i32) - len,
                "rq: ",
            );
            i915_request_show(m, rq, hdr.as_ptr(), 0);
            port = port.add(1);
        }
        rcu_read_unlock();
        i915_sched_engine_active_unlock_bh((*engine).sched_engine);
    } else if GRAPHICS_VER(i915) > 6 {
        drm_printf!(
            m,
            "\tPP_DIR_BASE: 0x%08x\n",
            ENGINE_READ!(engine, RING_PP_DIR_BASE)
        );
        drm_printf!(
            m,
            "\tPP_DIR_BASE_READ: 0x%08x\n",
            ENGINE_READ!(engine, RING_PP_DIR_BASE_READ),
        );
        drm_printf!(
            m,
            "\tPP_DIR_DCLV: 0x%08x\n",
            ENGINE_READ!(engine, RING_PP_DIR_DCLV)
        );
    }
}

// upstream: intel_engine_cs.c print_request_ring()
unsafe fn print_request_ring(m: *mut DrmPrinter, rq: *mut I915Request) {
    let vma_res = (*rq).batch_res;
    let mut ring: *mut c_void;
    let mut size: i32;

    drm_printf!(
        m,
        "[head %04x, postfix %04x, tail %04x, batch 0x%08x_%08x]:\n",
        (*rq).head,
        (*rq).postfix,
        (*rq).tail,
        if !vma_res.is_null() {
            upper_32_bits((*vma_res).start)
        } else {
            !0u32
        },
        if !vma_res.is_null() {
            lower_32_bits((*vma_res).start)
        } else {
            !0u32
        },
    );

    size = (*rq).tail as i32 - (*rq).head as i32;
    if (*rq).tail < (*rq).head {
        size += (*(*rq).ring).size as i32;
    }

    ring = kmalloc(size as usize, GFP_ATOMIC);
    if !ring.is_null() {
        let vaddr = (*(*rq).ring).vaddr;
        let mut head = (*rq).head as usize;
        let mut len = 0usize;

        if (*rq).tail < head as u32 {
            len = (*(*rq).ring).size as usize - head;
            memcpy(ring, vaddr.cast::<u8>().add(head).cast::<c_void>(), len);
            head = 0;
        }
        memcpy(
            ring.cast::<u8>().add(len).cast::<c_void>(),
            vaddr.cast::<u8>().add(head).cast::<c_void>(),
            size as usize - len,
        );

        hexdump(m, ring, size as usize);
        kfree(ring);
    }
}

// upstream: intel_engine_cs.c read_ul()
unsafe fn read_ul(p: *mut c_void, x: usize) -> c_ulong {
    *(p.cast::<u8>().add(x) as *const c_ulong)
}

// upstream: intel_engine_cs.c print_properties()
unsafe fn print_properties(engine: *mut IntelEngineCs, m: *mut DrmPrinter) {
    struct Pmap {
        offset: usize,
        name: *const c_char,
    }
    let props = [
        Pmap {
            offset: offset_of!(IntelEngineProps, heartbeat_interval_ms),
            name: c"heartbeat_interval_ms".as_ptr(),
        },
        Pmap {
            offset: offset_of!(IntelEngineProps, max_busywait_duration_ns),
            name: c"max_busywait_duration_ns".as_ptr(),
        },
        Pmap {
            offset: offset_of!(IntelEngineProps, preempt_timeout_ms),
            name: c"preempt_timeout_ms".as_ptr(),
        },
        Pmap {
            offset: offset_of!(IntelEngineProps, stop_timeout_ms),
            name: c"stop_timeout_ms".as_ptr(),
        },
        Pmap {
            offset: offset_of!(IntelEngineProps, timeslice_duration_ms),
            name: c"timeslice_duration_ms".as_ptr(),
        },
        Pmap {
            offset: 0,
            name: core::ptr::null(),
        },
    ];
    let mut p: *const Pmap;

    drm_printf!(m, "\tProperties:\n");
    p = props.as_ptr();
    while !(*p).name.is_null() {
        drm_printf!(
            m,
            "\t\t%s: %lu [default %lu]\n",
            (*p).name,
            read_ul(&(*engine).props as *const _ as *mut c_void, (*p).offset),
            read_ul(&(*engine).defaults as *const _ as *mut c_void, (*p).offset),
        );
        p = p.add(1);
    }
}

// upstream: intel_engine_cs.c engine_dump_request()
unsafe fn engine_dump_request(rq: *mut I915Request, m: *mut DrmPrinter, msg: *const c_char) {
    let tl = get_timeline(rq);

    i915_request_show(m, rq, msg, 0);

    drm_printf!(
        m,
        "\t\tring->start:  0x%08x\n",
        i915_ggtt_offset((*(*rq).ring).vma)
    );
    drm_printf!(m, "\t\tring->head:   0x%08x\n", (*(*rq).ring).head);
    drm_printf!(m, "\t\tring->tail:   0x%08x\n", (*(*rq).ring).tail);
    drm_printf!(m, "\t\tring->emit:   0x%08x\n", (*(*rq).ring).emit);
    drm_printf!(m, "\t\tring->space:  0x%08x\n", (*(*rq).ring).space);

    if !tl.is_null() {
        drm_printf!(m, "\t\tring->hwsp:   0x%08x\n", (*tl).hwsp_offset);
        intel_timeline_put(tl);
    }

    print_request_ring(m, rq);

    if !(*(*rq).context).lrc_reg_state.is_null() {
        drm_printf!(m, "Logical Ring Context:\n");
        hexdump(
            m,
            (*(*rq).context).lrc_reg_state.cast::<c_void>(),
            PAGE_SIZE,
        );
    }
}

// upstream: intel_engine_cs.c intel_engine_dump_active_requests()
pub unsafe fn intel_engine_dump_active_requests(
    requests: *mut ListHead,
    hung_rq: *mut I915Request,
    m: *mut DrmPrinter,
) {
    let mut rq: *mut I915Request;
    let mut msg: *const c_char;
    let mut state: I915RequestState;

    list_for_each_entry!(rq, requests, sched.link, {
        if rq == hung_rq {
            continue;
        }

        state = i915_test_request_state(rq);
        if (state as u32) < (I915_REQUEST_QUEUED as u32) {
            continue;
        }

        if state == I915_REQUEST_ACTIVE {
            msg = c"\t\tactive on engine".as_ptr();
        } else {
            msg = c"\t\tactive in queue".as_ptr();
        }

        engine_dump_request(rq, m, msg);
    });
}

// upstream: intel_engine_cs.c engine_dump_active_requests()
unsafe fn engine_dump_active_requests(engine: *mut IntelEngineCs, m: *mut DrmPrinter) {
    let mut hung_ce: *mut IntelContext = core::ptr::null_mut();
    let mut hung_rq: *mut I915Request = core::ptr::null_mut();

    intel_engine_get_hung_entity(engine, &mut hung_ce, &mut hung_rq);

    drm_printf!(m, "\tRequests:\n");

    if !hung_rq.is_null() {
        engine_dump_request(hung_rq, m, c"\t\thung".as_ptr());
    } else if !hung_ce.is_null() {
        drm_printf!(m, "\t\tGot hung ce but no hung rq!\n");
    }

    if unsafe { intel_uc_uses_guc_submission(core::ptr::addr_of_mut!((*(*engine).gt).uc)) } {
        intel_guc_dump_active_requests(engine, hung_rq, m);
    } else {
        intel_execlists_dump_active_requests(engine, hung_rq, m);
    }

    if !hung_rq.is_null() {
        i915_request_put(hung_rq);
    }
}

// upstream: intel_engine_cs.c intel_engine_dump()
pub unsafe fn intel_engine_dump(
    engine: *mut IntelEngineCs,
    m: *mut DrmPrinter,
    header: *const c_char,
    ap: *mut VaList,
) {
    let error = &mut (*(*engine).i915).gpu_error;
    let mut rq: *mut I915Request;
    let mut wakeref: IntelWakerefT;
    let mut dummy: Ktime;

    if !header.is_null() {
        drm_vprintf(m, header, ap);
    }

    if intel_gt_is_wedged((*engine).gt) {
        drm_printf!(m, "*** WEDGED ***\n");
    }

    drm_printf!(m, "\tAwake? %d\n", atomic_read(&(*engine).wakeref.count));
    drm_printf!(
        m,
        "\tBarriers?: %s\n",
        str_yes_no(!llist_empty(&(*engine).barrier_tasks)),
    );
    drm_printf!(
        m,
        "\tLatency: %luus\n",
        ewma__engine_latency_read(&(*engine).latency)
    );
    if intel_engine_supports_stats(engine) {
        drm_printf!(
            m,
            "\tRuntime: %llums\n",
            ktime_to_ms(intel_engine_get_busy_time(engine, &mut dummy)),
        );
    }
    drm_printf!(
        m,
        "\tForcewake: %x domains, %d active\n",
        (*engine).fw_domain,
        READ_ONCE!((*engine).fw_active),
    );

    rcu_read_lock();
    rq = READ_ONCE!((*engine).heartbeat.systole);
    if !rq.is_null() {
        drm_printf!(
            m,
            "\tHeartbeat: %d ms ago\n",
            jiffies_to_msecs(jiffies() - (*rq).emitted_jiffies),
        );
    }
    rcu_read_unlock();
    drm_printf!(
        m,
        "\tReset count: %d (global %d)\n",
        i915_reset_engine_count(error, engine),
        i915_reset_count(error),
    );
    print_properties(engine, m);

    engine_dump_active_requests(engine, m);

    drm_printf!(m, "\tMMIO base:  0x%08x\n", (*engine).mmio_base);
    wakeref = intel_runtime_pm_get_if_in_use((*(*engine).uncore).rpm);
    if !wakeref.is_null() {
        intel_engine_print_registers(engine, m);
        intel_runtime_pm_put((*(*engine).uncore).rpm, wakeref);
    } else {
        drm_printf!(m, "\tDevice is asleep; skipping register dump\n");
    }

    intel_execlists_show_requests(engine, m, i915_request_show, 8);

    drm_printf!(m, "HWSP:\n");
    hexdump(m, (*engine).status_page.addr as *const c_void, PAGE_SIZE);

    drm_printf!(m, "Idle? %s\n", str_yes_no(intel_engine_is_idle(engine)));
    intel_engine_print_breadcrumbs(engine, m);
}

// upstream: intel_engine_cs.c intel_engine_get_busy_time()
pub unsafe fn intel_engine_get_busy_time(engine: *mut IntelEngineCs, now: *mut Ktime) -> Ktime {
    ((*engine).busyness.unwrap())(engine, now)
}

// upstream: intel_engine_cs.c intel_engine_create_virtual()
pub unsafe fn intel_engine_create_virtual(
    siblings: *mut *mut IntelEngineCs,
    count: u32,
    flags: c_ulong,
) -> *mut IntelContext {
    if count == 0 {
        return ERR_PTR(-EINVAL);
    }

    if count == 1 && flags & crate::intel_engine_api_upstream::FORCE_VIRTUAL == 0 {
        return intel_context_create(*siblings);
    }

    GEM_BUG_ON!((*(*(*siblings)).cops).create_virtual.is_none());
    ((*(*(*siblings)).cops).create_virtual.unwrap())(siblings, count, flags)
}

// upstream: intel_engine_cs.c engine_execlist_find_hung_request()
unsafe fn engine_execlist_find_hung_request(engine: *mut IntelEngineCs) -> *mut I915Request {
    let mut request: *mut I915Request;
    let mut active: *mut I915Request = core::ptr::null_mut();

    GEM_BUG_ON!(unsafe {
        intel_uc_uses_guc_submission(core::ptr::addr_of_mut!((*(*engine).gt).uc))
    });

    lockdep_assert_held(&mut (*(*engine).sched_engine).lock);

    rcu_read_lock();
    request = execlists_active(&mut (*engine).execlists);
    if !request.is_null() {
        let tl = (*(*request).context).timeline;

        list_for_each_entry_from_reverse!(request, &mut (*tl).requests, link, {
            if __i915_request_is_complete(request) {
                break;
            }

            active = request;
        });
    }
    rcu_read_unlock();
    if !active.is_null() {
        return active;
    }

    list_for_each_entry!(
        request,
        &mut (*(*engine).sched_engine).requests,
        sched.link,
        {
            if i915_test_request_state(request) != I915_REQUEST_ACTIVE {
                continue;
            }

            active = request;
            break;
        }
    );

    active
}

// upstream: intel_engine_cs.c intel_engine_get_hung_entity()
pub unsafe fn intel_engine_get_hung_entity(
    engine: *mut IntelEngineCs,
    ce: *mut *mut IntelContext,
    rq: *mut *mut I915Request,
) {
    let mut flags: c_ulong = 0;

    *ce = intel_engine_get_hung_context(engine);
    if !(*ce).is_null() {
        intel_engine_clear_hung_context(engine);

        *rq = intel_context_get_active_request(*ce);
        return;
    }

    if unsafe { intel_uc_uses_guc_submission(core::ptr::addr_of_mut!((*(*engine).gt).uc)) } {
        return;
    }

    spin_lock_irqsave(&mut (*(*engine).sched_engine).lock, &mut flags);
    *rq = engine_execlist_find_hung_request(engine);
    if !(*rq).is_null() {
        *rq = i915_request_get_rcu(*rq);
    }
    spin_unlock_irqrestore(&mut (*(*engine).sched_engine).lock, flags);
}

// upstream: intel_engine_cs.c xehp_enable_ccs_engines()
pub unsafe fn xehp_enable_ccs_engines(engine: *mut IntelEngineCs) {
    if CCS_MASK((*engine).gt) == 0 {
        return;
    }

    intel_uncore_write(
        (*engine).uncore,
        GEN12_RCU_MODE,
        REG_MASKED_FIELD_ENABLE!(GEN12_RCU_MODE_CCS_ENABLE),
    );
}
