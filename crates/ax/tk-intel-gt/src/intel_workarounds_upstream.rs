// SPDX-License-Identifier: MIT
// Copyright © 2014-2018 Intel Corporation
// Source-faithful Rust transcription of Linux 7.2.3
// drivers/gpu/drm/i915/gt/intel_workarounds.c. This intentionally remains
// bounded to the translated Gen12+ subset; upstream MMIO/GT operations are
// binding points and must not be replaced by no-op compatibility shims.

// Header-owned types come from intel_workarounds_types_upstream; the C source
// provides these construction helpers only.
use core::ffi::c_void;

pub use crate::intel_workarounds_types_upstream::{
    I915McrRegT as I915McrReg, I915RegT as I915Reg, I915Wa, I915WaList, I915WaReg,
};
use crate::{
    i915_gem_object_header_upstream::{i915_gem_object_lock, i915_gem_object_unpin_map},
    i915_gem_pages_upstream::i915_gem_object_pin_map,
    i915_gem_ww_upstream::{
        I915GemWwCtx, i915_gem_ww_ctx_backoff, i915_gem_ww_ctx_fini, i915_gem_ww_ctx_init,
    },
    i915_request_types_upstream::I915Request,
    i915_request_upstream::{i915_request_add, i915_request_create, i915_request_wait},
    i915_vma_api_upstream::{
        i915_vma_is_ggtt, i915_vma_move_to_active, i915_vma_pin_ww, i915_vma_put, i915_vma_unpin,
    },
    i915_vma_types_upstream::I915Vma,
    intel_context_types_upstream::IntelContext,
    intel_engine_regs_upstream::{
        BLIT_CCTL, BLIT_CCTL_MOCS, CMD_CCTL_MOCS_OVERRIDE, ECOSKPD, L3_GENERAL_PRIO_CREDITS,
        L3_HIGH_PRIO_CREDITS, VDBOX_CGCTL3F1C, VDBOX_CGCTL3F10, XEHP_CCS_MODE_CSLICE,
        XEHP_CCS_MODE_CSLICE_MASK,
    },
    intel_engine_types_upstream::{
        COMPUTE_CLASS, COPY_ENGINE_CLASS, I915_MAX_CCS, IntelEngineCs, RENDER_CLASS,
        VIDEO_DECODE_CLASS,
    },
    intel_gt_mcr_upstream::{
        intel_gt_mcr_multicast_write_fw, intel_gt_mcr_read_any_fw, intel_gt_mcr_report_steering,
    },
    intel_gt_types_upstream::{IntelGt, IntelMmioRange as I915MmioRange},
    intel_gtt_api_upstream::__vm_create_scratch_for_read,
    intel_ring_upstream::intel_ring_begin,
    intel_sseu_types_upstream::{
        intel_slicemask_from_xehp_dssmask, intel_sseu_find_first_xehp_dss,
        intel_sseu_get_hsw_subslices,
    },
    intel_uncore_types_upstream::*,
    linux::{
        i915::{CCS_MASK, HAS_L3_CCS_READ, tuning_thread_rr_after_dep},
        memory::kmemdup_array,
        primitives::hweight8,
    },
    linux_config::*,
    linux_list::*,
};

impl I915Wa {
    unsafe fn reg(&self) -> I915Reg {
        unsafe { self.reg.reg }
    }

    unsafe fn mcr_reg(&self) -> I915McrReg {
        unsafe { self.reg.mcr_reg }
    }

    fn normal(reg: I915Reg, clr: u32, set: u32, read: u32, masked: bool) -> Self {
        Self {
            reg: I915WaReg { reg },
            clr,
            set,
            read,
            flags: masked as u32,
        }
    }

    fn multicast(reg: I915McrReg, clr: u32, set: u32, read: u32, masked: bool) -> Self {
        Self {
            reg: I915WaReg { mcr_reg: reg },
            clr,
            set,
            read,
            flags: (masked as u32) | 2,
        }
    }
}

// The source file is 3,131 lines and contains legacy platform paths outside
// this transcription's current coverage. Gen12 and later context/GT paths,
// their selectors, and shared workaround-list operations are represented here.

const WA_LIST_CHUNK: usize = 1 << 4;

// upstream: intel_workarounds.c wa_init_start()
unsafe fn wa_init_start(
    wal: *mut I915WaList,
    gt: *mut IntelGt,
    name: *const i8,
    engine_name: *const i8,
) {
    (*wal).gt = gt;
    (*wal).name = name;
    (*wal).engine_name = engine_name;
}

// upstream: intel_workarounds.c wa_init_finish()
unsafe fn wa_init_finish(wal: *mut I915WaList) {
    if (*wal).count as usize % WA_LIST_CHUNK != 0 {
        let list = kmemdup_array(
            (*wal).list.cast::<c_void>(),
            (*wal).count as usize,
            core::mem::size_of::<I915Wa>(),
            GFP_KERNEL,
        );
        if !list.is_null() {
            kfree((*wal).list);
            (*wal).list = list.cast::<I915Wa>();
        }
    }
    if (*wal).count == 0 {
        return;
    }
    gt_dbg!(
        (*wal).gt,
        "Initialized {} {} workarounds on {}\n",
        (*wal).wa_count,
        (*wal).name,
        (*wal).engine_name,
    );
}

// upstream: intel_workarounds.c wal_get_fw_for_rmw()
unsafe fn wal_get_fw_for_rmw(uncore: *mut IntelUncore, wal: *const I915WaList) -> ForcewakeDomains {
    let mut fw = 0;
    for i in 0..(*wal).count {
        let wa = &*(*wal).list.add(i as usize);
        fw |= intel_uncore_forcewake_for_reg(
            uncore,
            wa.reg(),
            crate::intel_uncore_types_upstream::FW_REG_READ
                | crate::intel_uncore_types_upstream::FW_REG_WRITE,
        );
    }
    fw
}

// upstream: intel_workarounds.c _wa_add()
unsafe fn _wa_add(wal: *mut I915WaList, wa: *const I915Wa) {
    let addr = i915_mmio_reg_offset((*wa).reg());
    let i915 = (*(*wal).gt).i915;
    let (mut start, mut end) = (0usize, (*wal).count as usize);
    if (*wal).count as usize % WA_LIST_CHUNK == 0 {
        let list = kmalloc_objs::<I915Wa, usize>(
            ((*wal).count as usize + WA_LIST_CHUNK) & !(WA_LIST_CHUNK - 1),
        );
        if list.is_null() {
            drm_err!(i915, "No space for workaround init!\n");
            return;
        }
        if !(*wal).list.is_null() {
            memcpy(
                list,
                (*wal).list,
                core::mem::size_of::<I915Wa>() * (*wal).count as usize,
            );
            kfree((*wal).list);
        }
        (*wal).list = list.cast::<I915Wa>();
    }
    while start < end {
        let mid = start + (end - start) / 2;
        let item = &mut *(*wal).list.add(mid);
        let current = i915_mmio_reg_offset(item.reg());
        if current < addr {
            start = mid + 1;
        } else if current > addr {
            end = mid;
        } else {
            if (item.clr | (*wa).clr) != 0 && ((*wa).clr & !item.clr) == 0 {
                drm_err!(
                    i915,
                    "Discarding overwritten w/a for reg {:04x} (clear: {:08x}, set: {:08x})\n",
                    current,
                    item.clr,
                    item.set,
                );
                item.set &= !(*wa).clr;
            }
            (*wal).wa_count += 1;
            item.set |= (*wa).set;
            item.clr |= (*wa).clr;
            item.read |= (*wa).read;
            return;
        }
    }
    (*wal).wa_count += 1;
    let mut index = (*wal).count as usize;
    (*wal).count += 1;
    *(*wal).list.add(index) = *wa;
    while index > 0 {
        if i915_mmio_reg_offset((*(*wal).list.add(index - 1)).reg())
            == i915_mmio_reg_offset((*(*wal).list.add(index)).reg())
        {
            GEM_BUG_ON!(true);
        }
        if i915_mmio_reg_offset((*(*wal).list.add(index)).reg())
            > i915_mmio_reg_offset((*(*wal).list.add(index - 1)).reg())
        {
            break;
        }
        core::ptr::swap((*wal).list.add(index), (*wal).list.add(index - 1));
        index -= 1;
    }
}

// upstream: intel_workarounds.c wa_add()
unsafe fn wa_add(
    wal: *mut I915WaList,
    reg: I915Reg,
    clear: u32,
    set: u32,
    read_mask: u32,
    masked_reg: bool,
) {
    let wa = I915Wa::normal(reg, clear, set, read_mask, masked_reg);
    _wa_add(wal, &wa);
}
// upstream: intel_workarounds.c wa_mcr_add()
unsafe fn wa_mcr_add(
    wal: *mut I915WaList,
    reg: I915McrReg,
    clear: u32,
    set: u32,
    read_mask: u32,
    masked_reg: bool,
) {
    let wa = I915Wa::multicast(reg, clear, set, read_mask, masked_reg);
    _wa_add(wal, &wa);
}
// upstream: intel_workarounds.c wa_write_clr_set()
unsafe fn wa_write_clr_set(wal: *mut I915WaList, reg: I915Reg, clear: u32, set: u32) {
    wa_add(wal, reg, clear, set, clear | set, false);
}
// upstream: intel_workarounds.c wa_mcr_write_clr_set()
unsafe fn wa_mcr_write_clr_set(wal: *mut I915WaList, reg: I915McrReg, clear: u32, set: u32) {
    wa_mcr_add(wal, reg, clear, set, clear | set, false);
}
// upstream: intel_workarounds.c wa_write()
unsafe fn wa_write(wal: *mut I915WaList, reg: I915Reg, set: u32) {
    wa_write_clr_set(wal, reg, !0, set);
}
// upstream: intel_workarounds.c wa_write_or()
unsafe fn wa_write_or(wal: *mut I915WaList, reg: I915Reg, set: u32) {
    wa_write_clr_set(wal, reg, set, set);
}
// upstream: intel_workarounds.c wa_mcr_write_or()
unsafe fn wa_mcr_write_or(wal: *mut I915WaList, reg: I915McrReg, set: u32) {
    wa_mcr_write_clr_set(wal, reg, set, set);
}
// upstream: intel_workarounds.c wa_write_clr()
unsafe fn wa_write_clr(wal: *mut I915WaList, reg: I915Reg, clr: u32) {
    wa_write_clr_set(wal, reg, clr, 0);
}
// upstream: intel_workarounds.c wa_mcr_write_clr()
unsafe fn wa_mcr_write_clr(wal: *mut I915WaList, reg: I915McrReg, clr: u32) {
    wa_mcr_write_clr_set(wal, reg, clr, 0);
}
// upstream: intel_workarounds.c wa_masked_en()
unsafe fn wa_masked_en(wal: *mut I915WaList, reg: I915Reg, val: u32) {
    wa_add(wal, reg, 0, REG_MASKED_FIELD_ENABLE!(val), val, true);
}
// upstream: intel_workarounds.c wa_mcr_masked_en()
unsafe fn wa_mcr_masked_en(wal: *mut I915WaList, reg: I915McrReg, val: u32) {
    wa_mcr_add(wal, reg, 0, REG_MASKED_FIELD_ENABLE!(val), val, true);
}
// upstream: intel_workarounds.c wa_masked_dis()
unsafe fn wa_masked_dis(wal: *mut I915WaList, reg: I915Reg, val: u32) {
    wa_add(wal, reg, 0, REG_MASKED_FIELD_DISABLE!(val), val, true);
}
// upstream: intel_workarounds.c wa_mcr_masked_dis()
unsafe fn wa_mcr_masked_dis(wal: *mut I915WaList, reg: I915McrReg, val: u32) {
    wa_mcr_add(wal, reg, 0, REG_MASKED_FIELD_DISABLE!(val), val, true);
}
// upstream: intel_workarounds.c wa_masked_field_set()
unsafe fn wa_masked_field_set(wal: *mut I915WaList, reg: I915Reg, mask: u32, val: u32) {
    wa_add(wal, reg, 0, REG_MASKED_FIELD!(mask, val), mask, true);
}
// upstream: intel_workarounds.c wa_mcr_masked_field_set()
unsafe fn wa_mcr_masked_field_set(wal: *mut I915WaList, reg: I915McrReg, mask: u32, val: u32) {
    wa_mcr_add(wal, reg, 0, REG_MASKED_FIELD!(mask, val), mask, true);
}

// upstream: intel_workarounds.c gen6_ctx_workarounds_init()
unsafe fn gen6_ctx_workarounds_init(_engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    wa_masked_en(wal, INSTPM, INSTPM_FORCE_ORDERING);
    wa_masked_dis(wal, CACHE_MODE_0, RC_OP_FLUSH_ENABLE);
}
// upstream: intel_workarounds.c gen7_ctx_workarounds_init()
unsafe fn gen7_ctx_workarounds_init(_engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    wa_masked_en(wal, INSTPM, INSTPM_FORCE_ORDERING);
    wa_masked_dis(wal, CACHE_MODE_0_GEN7, RC_OP_FLUSH_ENABLE);
    wa_masked_en(wal, CACHE_MODE_1, PIXEL_SUBSPAN_COLLECT_OPT_DISABLE);
}
// upstream: intel_workarounds.c gen8_ctx_workarounds_init()
unsafe fn gen8_ctx_workarounds_init(_engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    wa_masked_en(wal, INSTPM, INSTPM_FORCE_ORDERING);
    wa_masked_en(wal, RING_MI_MODE(RENDER_RING_BASE), ASYNC_FLIP_PERF_DISABLE);
    wa_mcr_masked_en(wal, GEN8_ROW_CHICKEN, PARTIAL_INSTRUCTION_SHOOTDOWN_DISABLE);
    wa_masked_en(
        wal,
        HDC_CHICKEN0,
        HDC_DONOT_FETCH_MEM_WHEN_MASKED | HDC_FORCE_NON_COHERENT,
    );
    wa_masked_dis(wal, CACHE_MODE_0_GEN7, HIZ_RAW_STALL_OPT_DISABLE);
    wa_masked_en(wal, CACHE_MODE_1, GEN8_4x4_STC_OPTIMIZATION_DISABLE);
    wa_masked_field_set(
        wal,
        GEN7_GT_MODE,
        GEN6_WIZ_HASHING_MASK,
        GEN6_WIZ_HASHING_16x4,
    );
}
// upstream: intel_workarounds.c bdw_ctx_workarounds_init()
unsafe fn bdw_ctx_workarounds_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    gen8_ctx_workarounds_init(engine, wal);
    wa_mcr_masked_en(wal, GEN8_ROW_CHICKEN, STALL_DOP_GATING_DISABLE);
    wa_mcr_masked_en(wal, GEN8_ROW_CHICKEN2, DOP_CLOCK_GATING_DISABLE);
    wa_mcr_masked_en(wal, GEN8_HALF_SLICE_CHICKEN3, GEN8_SAMPLER_POWER_BYPASS_DIS);
    let disable_slm = if (*INTEL_INFO((*engine).i915)).gt == 3 {
        HDC_FENCE_DEST_SLM_DISABLE
    } else {
        0
    };
    wa_masked_en(
        wal,
        HDC_CHICKEN0,
        HDC_FORCE_CONTEXT_SAVE_RESTORE_NON_COHERENT | disable_slm,
    );
}
// upstream: intel_workarounds.c chv_ctx_workarounds_init()
unsafe fn chv_ctx_workarounds_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    gen8_ctx_workarounds_init(engine, wal);
    wa_mcr_masked_en(wal, GEN8_ROW_CHICKEN, STALL_DOP_GATING_DISABLE);
    wa_masked_en(wal, HIZ_CHICKEN, CHV_HZ_8X8_MODE_IN_1X);
}
// upstream: intel_workarounds.c gen9_ctx_workarounds_init()
unsafe fn gen9_ctx_workarounds_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    let i915 = (*engine).i915;
    if HAS_LLC(i915) {
        wa_masked_en(
            wal,
            COMMON_SLICE_CHICKEN2,
            GEN9_PBE_COMPRESSED_HASH_SELECTION,
        );
        wa_mcr_masked_en(
            wal,
            GEN9_HALF_SLICE_CHICKEN7,
            GEN9_SAMPLER_HASH_COMPRESSED_READ_ADDR,
        );
    }
    wa_mcr_masked_en(
        wal,
        GEN8_ROW_CHICKEN,
        FLOW_CONTROL_ENABLE | PARTIAL_INSTRUCTION_SHOOTDOWN_DISABLE,
    );
    wa_mcr_masked_en(
        wal,
        GEN9_HALF_SLICE_CHICKEN7,
        GEN9_ENABLE_YV12_BUGFIX | GEN9_ENABLE_GPGPU_PREEMPTION,
    );
    wa_masked_en(
        wal,
        CACHE_MODE_1,
        GEN8_4x4_STC_OPTIMIZATION_DISABLE | GEN9_PARTIAL_RESOLVE_IN_VC_DISABLE,
    );
    wa_mcr_masked_dis(wal, GEN9_HALF_SLICE_CHICKEN5, GEN9_CCS_TLB_PREFETCH_ENABLE);
    wa_masked_en(
        wal,
        HDC_CHICKEN0,
        HDC_FORCE_CONTEXT_SAVE_RESTORE_NON_COHERENT | HDC_FORCE_CSR_NON_COHERENT_OVR_DISABLE,
    );
    wa_masked_en(wal, HDC_CHICKEN0, HDC_FORCE_NON_COHERENT);
    if IS_SKYLAKE(i915) || IS_KABYLAKE(i915) || IS_COFFEELAKE(i915) || IS_COMETLAKE(i915) {
        wa_mcr_masked_en(wal, GEN8_HALF_SLICE_CHICKEN3, GEN8_SAMPLER_POWER_BYPASS_DIS);
    }
    wa_mcr_masked_en(wal, HALF_SLICE_CHICKEN2, GEN8_ST_PO_DISABLE);
    wa_masked_dis(wal, GEN8_CS_CHICKEN1, GEN9_PREEMPT_3D_OBJECT_LEVEL);
    wa_masked_field_set(
        wal,
        GEN8_CS_CHICKEN1,
        GEN9_PREEMPT_GPGPU_LEVEL_MASK,
        GEN9_PREEMPT_GPGPU_COMMAND_LEVEL,
    );
    if IS_GEN9_LP(i915) {
        wa_masked_en(wal, GEN9_WM_CHICKEN3, GEN9_FACTOR_IN_CLR_VAL_HIZ);
    }
}
// upstream: intel_workarounds.c skl_tune_iz_hashing()
unsafe fn skl_tune_iz_hashing(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    let gt = (*engine).gt;
    let mut vals = [0u8; 3];
    for i in 0..3 {
        let eu = (*gt).info.sseu.subslice_7eu[i];
        if !is_power_of_2(eu) {
            continue;
        }
        let ss = ffs(eu as u32) - 1;
        vals[i] = (3 - ss) as u8;
    }
    if vals == [0, 0, 0] {
        return;
    }
    wa_masked_field_set(
        wal,
        GEN7_GT_MODE,
        GEN9_IZ_HASHING_MASK(2) | GEN9_IZ_HASHING_MASK(1) | GEN9_IZ_HASHING_MASK(0),
        GEN9_IZ_HASHING(2, vals[2] as u32)
            | GEN9_IZ_HASHING(1, vals[1] as u32)
            | GEN9_IZ_HASHING(0, vals[0] as u32),
    );
}
// upstream: intel_workarounds.c skl_ctx_workarounds_init()
unsafe fn skl_ctx_workarounds_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    gen9_ctx_workarounds_init(engine, wal);
    skl_tune_iz_hashing(engine, wal);
}
// upstream: intel_workarounds.c bxt_ctx_workarounds_init()
unsafe fn bxt_ctx_workarounds_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    gen9_ctx_workarounds_init(engine, wal);
    wa_mcr_masked_en(wal, GEN8_ROW_CHICKEN, STALL_DOP_GATING_DISABLE);
    wa_masked_en(
        wal,
        COMMON_SLICE_CHICKEN2,
        GEN8_SBE_DISABLE_REPLAY_BUF_OPTIMIZATION,
    );
}
// upstream: intel_workarounds.c kbl_ctx_workarounds_init()
unsafe fn kbl_ctx_workarounds_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    let i915 = (*engine).i915;
    gen9_ctx_workarounds_init(engine, wal);
    if IS_KABYLAKE(i915) && IS_GRAPHICS_STEP(i915, STEP_C0, STEP_FOREVER) {
        wa_masked_en(
            wal,
            COMMON_SLICE_CHICKEN2,
            GEN8_SBE_DISABLE_REPLAY_BUF_OPTIMIZATION,
        );
    }
    wa_mcr_masked_en(
        wal,
        GEN8_HALF_SLICE_CHICKEN1,
        GEN7_SBE_SS_CACHE_DISPATCH_PORT_SHARING_DISABLE,
    );
}
// upstream: intel_workarounds.c glk_ctx_workarounds_init()
unsafe fn glk_ctx_workarounds_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    gen9_ctx_workarounds_init(engine, wal);
    wa_masked_en(
        wal,
        COMMON_SLICE_CHICKEN2,
        GEN8_SBE_DISABLE_REPLAY_BUF_OPTIMIZATION,
    );
}
// upstream: intel_workarounds.c cfl_ctx_workarounds_init()
unsafe fn cfl_ctx_workarounds_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    gen9_ctx_workarounds_init(engine, wal);
    wa_masked_en(
        wal,
        COMMON_SLICE_CHICKEN2,
        GEN8_SBE_DISABLE_REPLAY_BUF_OPTIMIZATION,
    );
    wa_mcr_masked_en(
        wal,
        GEN8_HALF_SLICE_CHICKEN1,
        GEN7_SBE_SS_CACHE_DISPATCH_PORT_SHARING_DISABLE,
    );
}
// upstream: intel_workarounds.c icl_ctx_workarounds_init()
unsafe fn icl_ctx_workarounds_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    let i915 = (*engine).i915;
    wa_write(wal, GEN8_L3CNTLREG, GEN8_ERRDETBCTRL);
    wa_mcr_masked_en(wal, ICL_HDC_MODE, HDC_FORCE_NON_COHERENT);
    wa_mcr_add(
        wal,
        GEN10_CACHE_MODE_SS,
        0,
        REG_MASKED_FIELD_ENABLE!(FLOAT_BLEND_OPTIMIZATION_ENABLE),
        0,
        true,
    );
    wa_masked_field_set(
        wal,
        GEN8_CS_CHICKEN1,
        GEN9_PREEMPT_GPGPU_LEVEL_MASK,
        GEN9_PREEMPT_GPGPU_THREAD_GROUP_LEVEL,
    );
    wa_mcr_masked_en(wal, GEN10_SAMPLER_MODE, GEN11_SAMPLER_ENABLE_HEADLESS_MSG);
    wa_write(wal, IVB_FBC_RT_BASE, 0xffffffff & !ILK_FBC_RT_VALID);
    wa_write_clr_set(wal, IVB_FBC_RT_BASE_UPPER, 0, 0xffffffff);
    wa_mcr_masked_en(wal, GEN9_ROW_CHICKEN4, GEN11_DIS_PICK_2ND_EU);
    if IS_JASPERLAKE(i915) || IS_ELKHARTLAKE(i915) {
        wa_masked_en(wal, CACHE_MODE_0_GEN7, DISABLE_REPACKING_FOR_COMPRESSION);
    }
}

// upstream: intel_workarounds.c dg2_ctx_gt_tuning_init()
unsafe fn dg2_ctx_gt_tuning_init(_engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    wa_mcr_masked_en(wal, CHICKEN_RASTER_2, TBIMR_FAST_CLIP);
    wa_mcr_write_clr_set(
        wal,
        XEHP_L3SQCREG5,
        L3_PWM_TIMER_INIT_VAL_MASK,
        REG_FIELD_PREP(L3_PWM_TIMER_INIT_VAL_MASK, 0x7f),
    );
    wa_mcr_write_clr_set(
        wal,
        XEHP_FF_MODE2,
        FF_MODE2_TDS_TIMER_MASK,
        FF_MODE2_TDS_TIMER_128,
    );
}
// upstream: intel_workarounds.c gen12_ctx_workarounds_init()
unsafe fn gen12_ctx_workarounds_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    let i915 = (*engine).i915;
    wa_masked_en(
        wal,
        GEN11_COMMON_SLICE_CHICKEN3,
        GEN12_DISABLE_CPS_AWARE_COLOR_PIPE,
    );
    wa_masked_field_set(
        wal,
        GEN8_CS_CHICKEN1,
        GEN9_PREEMPT_GPGPU_LEVEL_MASK,
        GEN9_PREEMPT_GPGPU_THREAD_GROUP_LEVEL,
    );
    wa_add(
        wal,
        GEN12_FF_MODE2,
        !0,
        FF_MODE2_TDS_TIMER_128 | FF_MODE2_GS_TIMER_224,
        0,
        false,
    );
    if !IS_DG1(i915) {
        wa_masked_en(wal, HIZ_CHICKEN, HZ_DEPTH_TEST_LE_GE_OPT_DISABLE);
        wa_masked_en(wal, COMMON_SLICE_CHICKEN4, DISABLE_TDC_LOAD_BALANCING_CALC);
    }
    wa_mcr_write_or(wal, GEN8_WM_CHICKEN2, WAIT_ON_DEPTH_STALL_DONE_DISABLE);
}
// upstream: intel_workarounds.c dg1_ctx_workarounds_init()
unsafe fn dg1_ctx_workarounds_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    gen12_ctx_workarounds_init(engine, wal);
    wa_masked_dis(
        wal,
        GEN11_COMMON_SLICE_CHICKEN3,
        DG1_FLOAT_POINT_BLEND_OPT_STRICT_MODE_EN,
    );
    wa_masked_en(
        wal,
        HIZ_CHICKEN,
        DG1_HZ_READ_SUPPRESSION_OPTIMIZATION_DISABLE,
    );
}
// upstream: intel_workarounds.c dg2_ctx_workarounds_init()
unsafe fn dg2_ctx_workarounds_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    dg2_ctx_gt_tuning_init(engine, wal);
    wa_mcr_masked_en(
        wal,
        XEHP_SLICE_COMMON_ECO_CHICKEN1,
        MSC_MSAA_REODER_BUF_BYPASS_DISABLE,
    );
    wa_masked_field_set(wal, VF_PREEMPTION, PREEMPTION_VERTEX_COUNT, 0x4000);
    wa_mcr_masked_en(wal, XEHP_PSS_MODE2, SCOREBOARD_STALL_FLUSH_CONTROL);
    wa_masked_en(wal, CACHE_MODE_1, MSAA_OPTIMIZATION_REDUC_DISABLE);
    wa_mcr_masked_en(wal, XEHP_PSS_CHICKEN, FD_END_COLLECT);
}
// upstream: intel_workarounds.c xelpg_ctx_gt_tuning_init()
unsafe fn xelpg_ctx_gt_tuning_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    let gt = (*engine).gt;
    dg2_ctx_gt_tuning_init(engine, wal);
    if !(IS_GFX_GT_IP_STEP(gt, IP_VER(12, 70), STEP_A0, STEP_B0)
        || IS_GFX_GT_IP_STEP(gt, IP_VER(12, 71), STEP_A0, STEP_B0))
    {
        wa_add(wal, DRAW_WATERMARK, VERT_WM_VAL, 0x3ff, 0, false);
    }
}
// upstream: intel_workarounds.c xelpg_ctx_workarounds_init()
unsafe fn xelpg_ctx_workarounds_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    let gt = (*engine).gt;
    xelpg_ctx_gt_tuning_init(engine, wal);
    if IS_GFX_GT_IP_STEP(gt, IP_VER(12, 70), STEP_A0, STEP_B0)
        || IS_GFX_GT_IP_STEP(gt, IP_VER(12, 71), STEP_A0, STEP_B0)
    {
        wa_masked_field_set(wal, VF_PREEMPTION, PREEMPTION_VERTEX_COUNT, 0x4000);
        wa_mcr_masked_en(
            wal,
            XEHP_SLICE_COMMON_ECO_CHICKEN1,
            MSC_MSAA_REODER_BUF_BYPASS_DISABLE,
        );
        wa_mcr_masked_en(wal, VFLSKPD, VF_PREFETCH_TLB_DIS);
        wa_mcr_masked_en(wal, XEHP_PSS_MODE2, SCOREBOARD_STALL_FLUSH_CONTROL);
    }
    wa_masked_en(wal, CACHE_MODE_1, MSAA_OPTIMIZATION_REDUC_DISABLE);
    wa_mcr_masked_en(wal, XEHP_PSS_CHICKEN, FD_END_COLLECT);
}
// upstream: intel_workarounds.c fakewa_disable_nestedbb_mode()
unsafe fn fakewa_disable_nestedbb_mode(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    wa_masked_dis(wal, RING_MI_MODE((*engine).mmio_base), TGL_NESTED_BB_EN);
}
// upstream: intel_workarounds.c gen12_ctx_gt_mocs_init()
unsafe fn gen12_ctx_gt_mocs_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    if (*engine).class as i32 == COPY_ENGINE_CLASS {
        let mocs = (*(*engine).gt).mocs.uc_index;
        wa_write_clr_set(
            wal,
            BLIT_CCTL((*engine).mmio_base),
            BLIT_CCTL_MASK,
            BLIT_CCTL_MOCS(mocs as u32, mocs as u32),
        );
    }
}
// upstream: intel_workarounds.c gen12_ctx_gt_fake_wa_init()
unsafe fn gen12_ctx_gt_fake_wa_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    if GRAPHICS_VER_FULL((*engine).i915) >= IP_VER(12, 55) {
        fakewa_disable_nestedbb_mode(engine, wal);
    }
    gen12_ctx_gt_mocs_init(engine, wal);
}

// upstream: intel_workarounds.c __intel_engine_init_ctx_wa()
unsafe fn __intel_engine_init_ctx_wa(
    engine: *mut IntelEngineCs,
    wal: *mut I915WaList,
    name: *const i8,
) {
    let i915 = (*engine).i915;
    wa_init_start(wal, (*engine).gt, name, (*engine).name.as_ptr());
    if GRAPHICS_VER(i915) >= 12 {
        gen12_ctx_gt_fake_wa_init(engine, wal);
    }
    if (*engine).class as i32 != RENDER_CLASS {
        wa_init_finish(wal);
        return;
    }
    if IS_GFX_GT_IP_RANGE((*engine).gt, IP_VER(12, 70), IP_VER(12, 74)) {
        xelpg_ctx_workarounds_init(engine, wal);
    } else if IS_DG2(i915) {
        dg2_ctx_workarounds_init(engine, wal);
    } else if IS_DG1(i915) {
        dg1_ctx_workarounds_init(engine, wal);
    } else if GRAPHICS_VER(i915) == 12 {
        gen12_ctx_workarounds_init(engine, wal);
    } else if GRAPHICS_VER(i915) == 11 {
        icl_ctx_workarounds_init(engine, wal);
    } else if IS_COFFEELAKE(i915) || IS_COMETLAKE(i915) {
        cfl_ctx_workarounds_init(engine, wal);
    } else if IS_GEMINILAKE(i915) {
        glk_ctx_workarounds_init(engine, wal);
    } else if IS_KABYLAKE(i915) {
        kbl_ctx_workarounds_init(engine, wal);
    } else if IS_BROXTON(i915) {
        bxt_ctx_workarounds_init(engine, wal);
    } else if IS_SKYLAKE(i915) {
        skl_ctx_workarounds_init(engine, wal);
    } else if IS_CHERRYVIEW(i915) {
        chv_ctx_workarounds_init(engine, wal);
    } else if IS_BROADWELL(i915) {
        bdw_ctx_workarounds_init(engine, wal);
    } else if GRAPHICS_VER(i915) == 7 {
        gen7_ctx_workarounds_init(engine, wal);
    } else if GRAPHICS_VER(i915) == 6 {
        gen6_ctx_workarounds_init(engine, wal);
    } else if GRAPHICS_VER(i915) >= 8 {
        MISSING_CASE(GRAPHICS_VER(i915));
    }
    wa_init_finish(wal);
}
// upstream: intel_workarounds.c intel_engine_init_ctx_wa()
pub(crate) unsafe fn intel_engine_init_ctx_wa(engine: *mut IntelEngineCs) {
    __intel_engine_init_ctx_wa(engine, &mut (*engine).ctx_wa_list, c"context".as_ptr());
}

// upstream: intel_workarounds.c intel_engine_emit_ctx_wa()
unsafe fn intel_engine_emit_ctx_wa(rq: *mut I915Request) -> i32 {
    let wal = &mut (*(*rq).engine).ctx_wa_list;
    let uncore = (*(*rq).engine).uncore;
    if wal.count == 0 {
        return 0;
    }
    let mut ret = ((*(*rq).engine).emit_flush.unwrap())(rq, EMIT_BARRIER);
    if ret != 0 {
        return ret;
    }
    let mesh = (IS_GFX_GT_IP_RANGE((*(*rq).engine).gt, IP_VER(12, 70), IP_VER(12, 74))
        || IS_DG2((*rq).i915))
        && (*(*rq).engine).class as i32 == RENDER_CLASS;
    let mut cs = intel_ring_begin(rq, wal.count * 2 + if mesh { 6 } else { 2 });
    if IS_ERR(cs) {
        return PTR_ERR(cs);
    }
    let fw = wal_get_fw_for_rmw(uncore, wal);
    let mut flags = 0;
    intel_gt_mcr_lock(wal.gt, &mut flags);
    spin_lock(&mut (*uncore).lock);
    intel_uncore_forcewake_get__locked(uncore, fw);
    *cs = MI_LOAD_REGISTER_IMM(wal.count as u32);
    cs = cs.add(1);
    for i in 0..wal.count {
        let wa = &*wal.list.add(i as usize);
        let val = if wa.masked_reg() || (wa.clr | wa.set) == U32_MAX {
            wa.set
        } else {
            let mut value = if wa.is_mcr() {
                intel_gt_mcr_read_any_fw(wal.gt, wa.mcr_reg())
            } else {
                intel_uncore_read_fw(uncore, wa.reg())
            };
            value &= !wa.clr;
            value |= wa.set;
            value
        };
        *cs = i915_mmio_reg_offset(wa.reg());
        cs = cs.add(1);
        *cs = val;
        cs = cs.add(1);
    }
    *cs = MI_NOOP;
    cs = cs.add(1);
    if mesh {
        *cs = CMD_3DSTATE_MESH_CONTROL;
        cs = cs.add(1);
        *cs = 0;
        cs = cs.add(1);
        *cs = 0;
        cs = cs.add(1);
        *cs = MI_NOOP;
        cs = cs.add(1);
    }
    intel_uncore_forcewake_put__locked(uncore, fw);
    spin_unlock(&mut (*uncore).lock);
    intel_gt_mcr_unlock(wal.gt, flags);
    intel_ring_advance(rq, cs);
    ret = ((*(*rq).engine).emit_flush.unwrap())(rq, EMIT_BARRIER);
    if ret != 0 {
        return ret;
    }
    0
}

// upstream: intel_workarounds.c gen4_gt_workarounds_init()
unsafe fn gen4_gt_workarounds_init(_gt: *mut IntelGt, wal: *mut I915WaList) {
    wa_masked_dis(wal, CACHE_MODE_0, RC_OP_FLUSH_ENABLE);
}
// upstream: intel_workarounds.c g4x_gt_workarounds_init()
unsafe fn g4x_gt_workarounds_init(gt: *mut IntelGt, wal: *mut I915WaList) {
    gen4_gt_workarounds_init(gt, wal);
    wa_masked_en(wal, CACHE_MODE_0, CM0_PIPELINED_RENDER_FLUSH_DISABLE);
}
// upstream: intel_workarounds.c ilk_gt_workarounds_init()
unsafe fn ilk_gt_workarounds_init(gt: *mut IntelGt, wal: *mut I915WaList) {
    g4x_gt_workarounds_init(gt, wal);
    wa_masked_en(wal, _3D_CHICKEN2, _3D_CHICKEN2_WM_READ_PIPELINED);
}
// upstream: intel_workarounds.c snb_gt_workarounds_init()
unsafe fn snb_gt_workarounds_init(_gt: *mut IntelGt, _wal: *mut I915WaList) {}
// upstream: intel_workarounds.c ivb_gt_workarounds_init()
unsafe fn ivb_gt_workarounds_init(_gt: *mut IntelGt, wal: *mut I915WaList) {
    wa_masked_dis(
        wal,
        GEN7_COMMON_SLICE_CHICKEN1,
        GEN7_CSC1_RHWO_OPT_DISABLE_IN_RCC,
    );
    wa_write(wal, GEN7_L3CNTLREG1, GEN7_WA_FOR_GEN7_L3_CONTROL);
    wa_write(wal, GEN7_L3_CHICKEN_MODE_REGISTER, GEN7_WA_L3_CHICKEN_MODE);
    wa_write_clr(wal, GEN7_L3SQCREG4, L3SQ_URB_READ_CAM_MATCH_DISABLE);
}
// upstream: intel_workarounds.c vlv_gt_workarounds_init()
unsafe fn vlv_gt_workarounds_init(_gt: *mut IntelGt, wal: *mut I915WaList) {
    wa_write_clr(wal, GEN7_L3SQCREG4, L3SQ_URB_READ_CAM_MATCH_DISABLE);
    wa_write(wal, GEN7_L3SQCREG1, VLV_B0_WA_L3SQCREG1_VALUE);
}
// upstream: intel_workarounds.c hsw_gt_workarounds_init()
unsafe fn hsw_gt_workarounds_init(_gt: *mut IntelGt, wal: *mut I915WaList) {
    wa_write(wal, HSW_SCRATCH1, HSW_SCRATCH1_L3_DATA_ATOMICS_DISABLE);
    wa_add(
        wal,
        HSW_ROW_CHICKEN3,
        0,
        REG_MASKED_FIELD_ENABLE!(HSW_ROW_CHICKEN3_L3_GLOBAL_ATOMICS_DISABLE),
        0,
        true,
    );
    wa_write_clr(wal, GEN7_FF_THREAD_MODE, GEN7_FF_VS_REF_CNT_FFME);
}
// upstream: intel_workarounds.c gen9_wa_init_mcr()
unsafe fn gen9_wa_init_mcr(i915: *mut DrmI915Private, wal: *mut I915WaList) {
    let sseu = &(*to_gt(i915)).info.sseu;
    GEM_BUG_ON!(GRAPHICS_VER(i915) != 9);
    let slice = ffs(sseu.slice_mask as u32) - 1;
    GEM_BUG_ON!(slice as usize >= ARRAY_SIZE(sseu.subslice_mask.hsw));
    let subslices = intel_sseu_get_hsw_subslices(sseu, slice as u8);
    GEM_BUG_ON!(subslices == 0);
    let subslice = ffs(subslices) - 1;
    let mcr = GEN8_MCR_SLICE(slice as u32) | GEN8_MCR_SUBSLICE(subslice as u32);
    let mcr_mask = GEN8_MCR_SLICE_MASK | GEN8_MCR_SUBSLICE_MASK;
    drm_dbg!(
        i915,
        "MCR slice:{}/subslice:{} = {:x}\n",
        slice,
        subslice,
        mcr,
    );
    wa_write_clr_set(wal, GEN8_MCR_SELECTOR, mcr_mask, mcr);
}
// upstream: intel_workarounds.c gen9_gt_workarounds_init()
unsafe fn gen9_gt_workarounds_init(gt: *mut IntelGt, wal: *mut I915WaList) {
    let i915 = (*gt).i915;
    gen9_wa_init_mcr(i915, wal);
    if !IS_COFFEELAKE(i915) && !IS_COMETLAKE(i915) {
        wa_write_or(wal, GAM_ECOCHK, ECOCHK_DIS_TLB);
    }
    if HAS_LLC(i915) {
        wa_write_or(wal, MMCD_MISC_CTRL, MMCD_PCLA | MMCD_HOTSPOT_EN);
    }
    wa_write_or(wal, GAM_ECOCHK, BDW_DISABLE_HDC_INVALIDATION);
}
// upstream: intel_workarounds.c skl_gt_workarounds_init()
unsafe fn skl_gt_workarounds_init(gt: *mut IntelGt, wal: *mut I915WaList) {
    gen9_gt_workarounds_init(gt, wal);
    wa_write_or(wal, GEN7_UCGCTL4, GEN8_EU_GAUNIT_CLOCK_GATE_DISABLE);
    if IS_SKYLAKE((*gt).i915) && IS_GRAPHICS_STEP((*gt).i915, STEP_A0, STEP_H0) {
        wa_write_or(
            wal,
            GEN9_GAMT_ECO_REG_RW_IA,
            GAMT_ECO_ENABLE_IN_PLACE_DECOMPRESS,
        );
    }
}
// upstream: intel_workarounds.c kbl_gt_workarounds_init()
unsafe fn kbl_gt_workarounds_init(gt: *mut IntelGt, wal: *mut I915WaList) {
    gen9_gt_workarounds_init(gt, wal);
    if IS_KABYLAKE((*gt).i915) && IS_GRAPHICS_STEP((*gt).i915, 0, STEP_C0) {
        wa_write_or(
            wal,
            GAMT_CHKN_BIT_REG,
            GAMT_CHKN_DISABLE_DYNAMIC_CREDIT_SHARING,
        );
    }
    wa_write_or(wal, GEN7_UCGCTL4, GEN8_EU_GAUNIT_CLOCK_GATE_DISABLE);
    wa_write_or(
        wal,
        GEN9_GAMT_ECO_REG_RW_IA,
        GAMT_ECO_ENABLE_IN_PLACE_DECOMPRESS,
    );
}
// upstream: intel_workarounds.c glk_gt_workarounds_init()
unsafe fn glk_gt_workarounds_init(gt: *mut IntelGt, wal: *mut I915WaList) {
    gen9_gt_workarounds_init(gt, wal);
}
// upstream: intel_workarounds.c cfl_gt_workarounds_init()
unsafe fn cfl_gt_workarounds_init(gt: *mut IntelGt, wal: *mut I915WaList) {
    gen9_gt_workarounds_init(gt, wal);
    wa_write_or(wal, GEN7_UCGCTL4, GEN8_EU_GAUNIT_CLOCK_GATE_DISABLE);
    wa_write_or(
        wal,
        GEN9_GAMT_ECO_REG_RW_IA,
        GAMT_ECO_ENABLE_IN_PLACE_DECOMPRESS,
    );
}

// upstream: intel_workarounds.c __set_mcr_steering()
unsafe fn __set_mcr_steering(wal: *mut I915WaList, reg: I915Reg, slice: u32, subslice: u32) {
    wa_write_clr_set(
        wal,
        reg,
        GEN11_MCR_SLICE_MASK | GEN11_MCR_SUBSLICE_MASK,
        GEN11_MCR_SLICE(slice) | GEN11_MCR_SUBSLICE(subslice),
    );
}
// upstream: intel_workarounds.c debug_dump_steering()
unsafe fn debug_dump_steering(gt: *mut IntelGt) {
    let mut printer = drm_dbg_printer(
        &mut (*(*gt).i915).drm,
        DRM_UT_DRIVER,
        c"MCR Steering:".as_ptr(),
    );
    if drm_debug_enabled(DRM_UT_DRIVER) {
        intel_gt_mcr_report_steering(&mut printer, gt, false);
    }
}
// upstream: intel_workarounds.c __add_mcr_wa()
unsafe fn __add_mcr_wa(gt: *mut IntelGt, wal: *mut I915WaList, slice: u32, subslice: u32) {
    let i915 = (*gt).i915;
    __set_mcr_steering(wal, GEN8_MCR_SELECTOR, slice, subslice);
    (*gt).default_steering.groupid = slice as u8;
    (*gt).default_steering.instanceid = subslice as u8;
    debug_dump_steering(gt);
}
// upstream: intel_workarounds.c icl_wa_init_mcr()
unsafe fn icl_wa_init_mcr(gt: *mut IntelGt, wal: *mut I915WaList) {
    let sseu = &(*gt).info.sseu;
    GEM_BUG_ON!(GRAPHICS_VER((*gt).i915) < 11);
    GEM_BUG_ON!(hweight8(sseu.slice_mask) > 1);
    let subslice = __ffs(intel_sseu_get_hsw_subslices(sseu, 0u8));
    if ((*gt).info.l3bank_mask & BIT!(subslice)) != 0 {
        (*gt).steering_table[L3BANK] = core::ptr::null_mut();
    }
    __add_mcr_wa(gt, wal, 0, subslice as u32);
}
// upstream: intel_workarounds.c xehp_init_mcr()
unsafe fn xehp_init_mcr(gt: *mut IntelGt, wal: *mut I915WaList) {
    let sseu = &(*gt).info.sseu;
    let mut lncf_mask = 0u64;
    let mut slice_mask =
        intel_slicemask_from_xehp_dssmask(sseu.subslice_mask, GEN_DSS_PER_GSLICE as i32) as u64;
    for i in for_each_set_bit((*gt).info.mslice_mask, GEN12_MAX_MSLICES) {
        lncf_mask |= 3 << (i * 2);
    }
    if (slice_mask & lncf_mask) != 0 {
        slice_mask &= lncf_mask;
        (*gt).steering_table[LNCF] = core::ptr::null_mut();
    }
    if (slice_mask & (*gt).info.mslice_mask) != 0 {
        slice_mask &= (*gt).info.mslice_mask;
        (*gt).steering_table[MSLICE] = core::ptr::null_mut();
    }
    let slice = __ffs(slice_mask) as u32;
    let subslice = intel_sseu_find_first_xehp_dss(sseu, GEN_DSS_PER_GSLICE as i32, slice as i32)
        as u32
        % GEN_DSS_PER_GSLICE;
    __add_mcr_wa(gt, wal, slice, subslice);
    __set_mcr_steering(wal, MCFG_MCR_SELECTOR, 0, 2);
    __set_mcr_steering(wal, SF_MCR_SELECTOR, 0, 2);
    if IS_DG2((*gt).i915) {
        __set_mcr_steering(wal, GAM_MCR_SELECTOR, 1, 0);
    }
}

// upstream: intel_workarounds.c icl_gt_workarounds_init()
unsafe fn icl_gt_workarounds_init(gt: *mut IntelGt, wal: *mut I915WaList) {
    let i915 = (*gt).i915;
    icl_wa_init_mcr(gt, wal);
    wa_write_clr_set(
        wal,
        GEN11_GACB_PERF_CTRL,
        GEN11_HASH_CTRL_MASK,
        GEN11_HASH_CTRL_BIT0 | GEN11_HASH_CTRL_BIT4,
    );
    wa_write_or(
        wal,
        GEN11_LSN_UNSLCVC,
        GEN11_LSN_UNSLCVC_GAFS_HALF_SF_MAXALLOC | GEN11_LSN_UNSLCVC_GAFS_HALF_CL2_MAXALLOC,
    );
    wa_write_or(
        wal,
        GEN8_GAMW_ECO_DEV_RW_IA,
        GAMW_ECO_DEV_CTX_RELOAD_DISABLE,
    );
    wa_write_or(wal, GAMT_CHKN_BIT_REG, GAMT_CHKN_DISABLE_L3_COH_PIPE);
    wa_write_or(
        wal,
        UNSLICE_UNIT_LEVEL_CLKGATE,
        VSUNIT_CLKGATE_DIS | HSUNIT_CLKGATE_DIS,
    );
    wa_write_or(wal, UNSLICE_UNIT_LEVEL_CLKGATE2, PSDUNIT_CLKGATE_DIS);
    wa_mcr_write_or(wal, GEN11_SUBSLICE_UNIT_LEVEL_CLKGATE, GWUNIT_CLKGATE_DIS);
    if IS_ICELAKE(i915)
        || ((IS_JASPERLAKE(i915) || IS_ELKHARTLAKE(i915))
            && IS_GRAPHICS_STEP(i915, STEP_A0, STEP_B0))
    {
        wa_write_or(
            wal,
            GEN11_SLICE_UNIT_LEVEL_CLKGATE,
            L3_CLKGATE_DIS | L3_CR2X_CLKGATE_DIS,
        );
    }
    wa_mcr_write_clr(wal, GEN10_DFR_RATIO_EN_AND_CHICKEN, DFR_DISABLE);
}
// upstream: intel_workarounds.c wa_14011060649()
unsafe fn wa_14011060649(gt: *mut IntelGt, wal: *mut I915WaList) {
    for engine in for_each_engine(gt) {
        if engine.class as i32 != VIDEO_DECODE_CLASS || engine.instance % 2 != 0 {
            continue;
        }
        wa_write_or(wal, VDBOX_CGCTL3F10(engine.mmio_base), IECPUNIT_CLKGATE_DIS);
    }
}
// upstream: intel_workarounds.c gen12_gt_workarounds_init()
unsafe fn gen12_gt_workarounds_init(gt: *mut IntelGt, wal: *mut I915WaList) {
    icl_wa_init_mcr(gt, wal);
    wa_14011060649(gt, wal);
    wa_mcr_write_or(wal, GEN10_DFR_RATIO_EN_AND_CHICKEN, DFR_DISABLE);
    wa_add(
        wal,
        GEN7_MISCCPCTL,
        GEN12_DOP_CLOCK_GATE_RENDER_ENABLE,
        0,
        0,
        false,
    );
}
// upstream: intel_workarounds.c dg1_gt_workarounds_init()
unsafe fn dg1_gt_workarounds_init(gt: *mut IntelGt, wal: *mut I915WaList) {
    gen12_gt_workarounds_init(gt, wal);
    wa_mcr_write_or(wal, SUBSLICE_UNIT_LEVEL_CLKGATE2, CPSSUNIT_CLKGATE_DIS);
    wa_write_or(wal, UNSLICE_UNIT_LEVEL_CLKGATE2, VSUNIT_CLKGATE_DIS_TGL);
}
// upstream: intel_workarounds.c dg2_gt_workarounds_init()
unsafe fn dg2_gt_workarounds_init(gt: *mut IntelGt, wal: *mut I915WaList) {
    xehp_init_mcr(gt, wal);
    wa_14011060649(gt, wal);
    if IS_DG2_G10((*gt).i915) {
        wa_write_or(wal, UNSLICE_UNIT_LEVEL_CLKGATE, CG3DDISCFEG_CLKGATE_DIS);
        wa_mcr_write_or(
            wal,
            GEN11_SUBSLICE_UNIT_LEVEL_CLKGATE,
            DSS_ROUTER_CLKGATE_DIS,
        );
    }
    wa_mcr_write_clr(wal, SARB_CHICKEN1, COMP_CKN_IN);
    wa_add(
        wal,
        GEN7_MISCCPCTL,
        GEN12_DOP_CLOCK_GATE_RENDER_ENABLE,
        0,
        0,
        false,
    );
    wa_mcr_write_or(wal, RENDER_MOD_CTRL, FORCE_MISS_FTLB);
    wa_mcr_write_or(wal, COMP_MOD_CTRL, FORCE_MISS_FTLB);
    wa_mcr_write_or(wal, XEHP_VDBX_MOD_CTRL, FORCE_MISS_FTLB);
    wa_mcr_write_or(wal, XEHP_VEBX_MOD_CTRL, FORCE_MISS_FTLB);
    wa_mcr_write_or(
        wal,
        XEHP_GAMCNTRL_CTRL,
        INVALIDATION_BROADCAST_MODE_DIS | GLOBAL_INVALIDATION_MODE,
    );
    wa_mcr_write_or(wal, XEHP_L3NODEARBCFG, XEHP_LNESPARE);
}
// upstream: intel_workarounds.c xelpg_gt_workarounds_init()
unsafe fn xelpg_gt_workarounds_init(gt: *mut IntelGt, wal: *mut I915WaList) {
    wa_mcr_write_or(wal, RENDER_MOD_CTRL, FORCE_MISS_FTLB);
    wa_mcr_write_or(wal, COMP_MOD_CTRL, FORCE_MISS_FTLB);
    wa_write_or(wal, GEN12_SQCNT1, GEN12_STRICT_RAR_ENABLE);
    if IS_GFX_GT_IP_STEP(gt, IP_VER(12, 70), STEP_A0, STEP_B0)
        || IS_GFX_GT_IP_STEP(gt, IP_VER(12, 71), STEP_A0, STEP_B0)
    {
        wa_mcr_write_clr(wal, SARB_CHICKEN1, COMP_CKN_IN);
        wa_write_clr(wal, GEN7_MISCCPCTL, GEN12_DOP_CLOCK_GATE_RENDER_ENABLE);
    }
    debug_dump_steering(gt);
}
// upstream: intel_workarounds.c wa_16021867713()
unsafe fn wa_16021867713(gt: *mut IntelGt, wal: *mut I915WaList) {
    for engine in for_each_engine(gt) {
        if engine.class as i32 == VIDEO_DECODE_CLASS {
            wa_write_or(wal, VDBOX_CGCTL3F1C(engine.mmio_base), MFXPIPE_CLKGATE_DIS);
        }
    }
}
// upstream: intel_workarounds.c xelpmp_gt_workarounds_init()
unsafe fn xelpmp_gt_workarounds_init(gt: *mut IntelGt, wal: *mut I915WaList) {
    wa_16021867713(gt, wal);
    wa_write_or(wal, XELPMP_GSC_MOD_CTRL, FORCE_MISS_FTLB);
    wa_write_or(wal, XELPMP_VDBX_MOD_CTRL, FORCE_MISS_FTLB);
    wa_write_or(wal, GEN12_SQCNT1, GEN12_STRICT_RAR_ENABLE);
    debug_dump_steering(gt);
}
// upstream: intel_workarounds.c gt_tuning_settings()
unsafe fn gt_tuning_settings(gt: *mut IntelGt, wal: *mut I915WaList) {
    if IS_GFX_GT_IP_RANGE(gt, IP_VER(12, 70), IP_VER(12, 74)) || IS_DG2((*gt).i915) {
        wa_mcr_write_or(wal, XEHP_L3SCQREG7, BLEND_FILL_CACHING_OPT_DIS);
        wa_mcr_write_or(wal, XEHP_SQCM, EN_32B_ACCESS);
    }
}
// upstream: intel_workarounds.c gt_init_workarounds()
unsafe fn gt_init_workarounds(gt: *mut IntelGt, wal: *mut I915WaList) {
    let i915 = (*gt).i915;
    gt_tuning_settings(gt, wal);
    if (*gt).type_ == GT_MEDIA {
        if MEDIA_VER_FULL(i915) == IP_VER(13, 0) {
            xelpmp_gt_workarounds_init(gt, wal);
        } else {
            MISSING_CASE(MEDIA_VER_FULL(i915));
        }
        return;
    }
    if IS_GFX_GT_IP_RANGE(gt, IP_VER(12, 70), IP_VER(12, 74)) {
        xelpg_gt_workarounds_init(gt, wal);
    } else if IS_DG2(i915) {
        dg2_gt_workarounds_init(gt, wal);
    } else if IS_DG1(i915) {
        dg1_gt_workarounds_init(gt, wal);
    } else if GRAPHICS_VER(i915) == 12 {
        gen12_gt_workarounds_init(gt, wal);
    } else if GRAPHICS_VER(i915) == 11 {
        icl_gt_workarounds_init(gt, wal);
    } else if IS_COFFEELAKE(i915) || IS_COMETLAKE(i915) {
        cfl_gt_workarounds_init(gt, wal);
    } else if IS_GEMINILAKE(i915) {
        glk_gt_workarounds_init(gt, wal);
    } else if IS_KABYLAKE(i915) {
        kbl_gt_workarounds_init(gt, wal);
    } else if IS_BROXTON(i915) {
        gen9_gt_workarounds_init(gt, wal);
    } else if IS_SKYLAKE(i915) {
        skl_gt_workarounds_init(gt, wal);
    } else if IS_HASWELL(i915) {
        hsw_gt_workarounds_init(gt, wal);
    } else if IS_VALLEYVIEW(i915) {
        vlv_gt_workarounds_init(gt, wal);
    } else if IS_IVYBRIDGE(i915) {
        ivb_gt_workarounds_init(gt, wal);
    } else if GRAPHICS_VER(i915) == 6 {
        snb_gt_workarounds_init(gt, wal);
    } else if GRAPHICS_VER(i915) == 5 {
        ilk_gt_workarounds_init(gt, wal);
    } else if IS_G4X(i915) {
        g4x_gt_workarounds_init(gt, wal);
    } else if GRAPHICS_VER(i915) == 4 {
        gen4_gt_workarounds_init(gt, wal);
    } else if GRAPHICS_VER(i915) > 8 {
        MISSING_CASE(GRAPHICS_VER(i915));
    }
}
// upstream: intel_workarounds.c intel_gt_init_workarounds()
unsafe fn intel_gt_init_workarounds(gt: *mut IntelGt) {
    let wal = &mut (*gt).wa_list;
    wa_init_start(wal, gt, c"GT".as_ptr(), c"global".as_ptr());
    gt_init_workarounds(gt, wal);
    wa_init_finish(wal);
}

// upstream: intel_workarounds.c wa_verify()
unsafe fn wa_verify(
    gt: *mut IntelGt,
    wa: *const I915Wa,
    cur: u32,
    name: *const i8,
    from: *const i8,
) -> bool {
    if ((cur ^ (*wa).set) & (*wa).read) != 0 {
        gt_err!(
            gt,
            "{} workaround lost on {}! (reg[{:x}]=0x{:x}, relevant bits were 0x{:x} vs expected \
             0x{:x})\n",
            name,
            from,
            i915_mmio_reg_offset((*wa).reg()),
            cur,
            cur & (*wa).read,
            (*wa).set & (*wa).read,
        );
        return false;
    }
    true
}
// upstream: intel_workarounds.c wa_list_apply()
unsafe fn wa_list_apply(wal: *const I915WaList) {
    let gt = (*wal).gt;
    let uncore = (*gt).uncore;
    if (*wal).count == 0 {
        return;
    }
    let fw = wal_get_fw_for_rmw(uncore, wal);
    let mut flags = 0;
    intel_gt_mcr_lock(gt, &mut flags);
    spin_lock(&mut (*uncore).lock);
    intel_uncore_forcewake_get__locked(uncore, fw);
    for i in 0..(*wal).count {
        let wa = &mut *(*wal).list.add(i as usize);
        let old = if wa.clr != 0 {
            if wa.is_mcr() {
                intel_gt_mcr_read_any_fw(gt, wa.mcr_reg())
            } else {
                intel_uncore_read_fw(uncore, wa.reg())
            }
        } else {
            0
        };
        let val = (old & !wa.clr) | wa.set;
        if val != old || wa.clr == 0 {
            if wa.is_mcr() {
                intel_gt_mcr_multicast_write_fw(gt, wa.mcr_reg(), val);
            } else {
                intel_uncore_write_fw(uncore, wa.reg(), val);
            }
        }
        if IS_ENABLED!(CONFIG_DRM_I915_DEBUG_GEM) {
            let value = if wa.is_mcr() {
                intel_gt_mcr_read_any_fw(gt, wa.mcr_reg())
            } else {
                intel_uncore_read_fw(uncore, wa.reg())
            };
            wa_verify(gt, wa, value, (*wal).name, c"application".as_ptr());
        }
    }
    intel_uncore_forcewake_put__locked(uncore, fw);
    spin_unlock(&mut (*uncore).lock);
    intel_gt_mcr_unlock(gt, flags);
}
// upstream: intel_workarounds.c intel_gt_apply_workarounds()
unsafe fn intel_gt_apply_workarounds(gt: *mut IntelGt) {
    wa_list_apply(&(*gt).wa_list);
}
// upstream: intel_workarounds.c wa_list_verify()
unsafe fn wa_list_verify(gt: *mut IntelGt, wal: *const I915WaList, from: *const i8) -> bool {
    let uncore = (*gt).uncore;
    let fw = wal_get_fw_for_rmw(uncore, wal);
    let mut flags = 0;
    let mut ok = true;
    intel_gt_mcr_lock(gt, &mut flags);
    spin_lock(&mut (*uncore).lock);
    intel_uncore_forcewake_get__locked(uncore, fw);
    for i in 0..(*wal).count {
        let wa = &*(*wal).list.add(i as usize);
        let val = if wa.is_mcr() {
            intel_gt_mcr_read_any_fw(gt, wa.mcr_reg())
        } else {
            intel_uncore_read_fw(uncore, wa.reg())
        };
        ok &= wa_verify((*wal).gt, wa, val, (*wal).name, from);
    }
    intel_uncore_forcewake_put__locked(uncore, fw);
    spin_unlock(&mut (*uncore).lock);
    intel_gt_mcr_unlock(gt, flags);
    ok
}
// upstream: intel_workarounds.c intel_gt_verify_workarounds()
unsafe fn intel_gt_verify_workarounds(gt: *mut IntelGt, from: *const i8) -> bool {
    wa_list_verify(gt, &(*gt).wa_list, from)
}

// upstream: intel_workarounds.c is_nonpriv_flags_valid()
unsafe fn is_nonpriv_flags_valid(flags: u32) -> bool {
    if flags & !RING_FORCE_TO_NONPRIV_MASK_VALID != 0 {
        return false;
    }
    if flags & RING_FORCE_TO_NONPRIV_ACCESS_MASK == RING_FORCE_TO_NONPRIV_ACCESS_INVALID {
        return false;
    }
    true
}
// upstream: intel_workarounds.c whitelist_reg_ext()
unsafe fn whitelist_reg_ext(wal: *mut I915WaList, mut reg: I915Reg, flags: u32) {
    if GEM_DEBUG_WARN_ON!((*wal).count >= RING_MAX_NONPRIV_SLOTS) {
        return;
    }
    if GEM_DEBUG_WARN_ON!(!is_nonpriv_flags_valid(flags)) {
        return;
    }
    reg.reg |= flags;
    let wa = I915Wa::normal(reg, 0, 0, 0, false);
    _wa_add(wal, &wa);
}
// upstream: intel_workarounds.c whitelist_mcr_reg_ext()
unsafe fn whitelist_mcr_reg_ext(wal: *mut I915WaList, mut reg: I915McrReg, flags: u32) {
    if GEM_DEBUG_WARN_ON!((*wal).count >= RING_MAX_NONPRIV_SLOTS) {
        return;
    }
    if GEM_DEBUG_WARN_ON!(!is_nonpriv_flags_valid(flags)) {
        return;
    }
    reg.reg |= flags;
    let wa = I915Wa::multicast(reg, 0, 0, 0, false);
    _wa_add(wal, &wa);
}
// upstream: intel_workarounds.c whitelist_reg()
unsafe fn whitelist_reg(wal: *mut I915WaList, reg: I915Reg) {
    whitelist_reg_ext(wal, reg, RING_FORCE_TO_NONPRIV_ACCESS_RW);
}
// upstream: intel_workarounds.c whitelist_mcr_reg()
unsafe fn whitelist_mcr_reg(wal: *mut I915WaList, reg: I915McrReg) {
    whitelist_mcr_reg_ext(wal, reg, RING_FORCE_TO_NONPRIV_ACCESS_RW);
}

// upstream: intel_workarounds.c gen9_whitelist_build()
unsafe fn gen9_whitelist_build(w: *mut I915WaList) {
    whitelist_reg(w, GEN9_CTX_PREEMPT_REG);
    whitelist_reg(w, GEN8_CS_CHICKEN1);
    whitelist_reg(w, GEN8_HDC_CHICKEN1);
    whitelist_reg(w, COMMON_SLICE_CHICKEN2);
}
// upstream: intel_workarounds.c skl_whitelist_build()
unsafe fn skl_whitelist_build(engine: *mut IntelEngineCs) {
    let w = &mut (*engine).whitelist;
    if (*engine).class as i32 != RENDER_CLASS {
        return;
    }
    gen9_whitelist_build(w);
    whitelist_mcr_reg(w, GEN8_L3SQCREG4);
}
// upstream: intel_workarounds.c bxt_whitelist_build()
unsafe fn bxt_whitelist_build(engine: *mut IntelEngineCs) {
    if (*engine).class as i32 == RENDER_CLASS {
        gen9_whitelist_build(&mut (*engine).whitelist);
    }
}
// upstream: intel_workarounds.c kbl_whitelist_build()
unsafe fn kbl_whitelist_build(engine: *mut IntelEngineCs) {
    let w = &mut (*engine).whitelist;
    if (*engine).class as i32 != RENDER_CLASS {
        return;
    }
    gen9_whitelist_build(w);
    whitelist_mcr_reg(w, GEN8_L3SQCREG4);
}
// upstream: intel_workarounds.c glk_whitelist_build()
unsafe fn glk_whitelist_build(engine: *mut IntelEngineCs) {
    let w = &mut (*engine).whitelist;
    if (*engine).class as i32 != RENDER_CLASS {
        return;
    }
    gen9_whitelist_build(w);
    whitelist_reg(w, GEN9_SLICE_COMMON_ECO_CHICKEN1);
}
// upstream: intel_workarounds.c cfl_whitelist_build()
unsafe fn cfl_whitelist_build(engine: *mut IntelEngineCs) {
    let w = &mut (*engine).whitelist;
    if (*engine).class as i32 != RENDER_CLASS {
        return;
    }
    gen9_whitelist_build(w);
    whitelist_reg_ext(
        w,
        PS_INVOCATION_COUNT,
        RING_FORCE_TO_NONPRIV_ACCESS_RD | RING_FORCE_TO_NONPRIV_RANGE_4,
    );
}
// upstream: intel_workarounds.c allow_read_ctx_timestamp()
unsafe fn allow_read_ctx_timestamp(engine: *mut IntelEngineCs) {
    let w = &mut (*engine).whitelist;
    if (*engine).class as i32 != RENDER_CLASS {
        whitelist_reg_ext(
            w,
            RING_CTX_TIMESTAMP((*engine).mmio_base),
            RING_FORCE_TO_NONPRIV_ACCESS_RD,
        );
    }
}
// upstream: intel_workarounds.c cml_whitelist_build()
unsafe fn cml_whitelist_build(engine: *mut IntelEngineCs) {
    allow_read_ctx_timestamp(engine);
    cfl_whitelist_build(engine);
}
// upstream: intel_workarounds.c icl_whitelist_build()
unsafe fn icl_whitelist_build(engine: *mut IntelEngineCs) {
    let w = &mut (*engine).whitelist;
    allow_read_ctx_timestamp(engine);
    if (*engine).class as i32 == RENDER_CLASS {
        whitelist_mcr_reg(w, GEN9_HALF_SLICE_CHICKEN7);
        whitelist_mcr_reg(w, GEN10_SAMPLER_MODE);
        whitelist_reg(w, GEN9_SLICE_COMMON_ECO_CHICKEN1);
        whitelist_reg_ext(
            w,
            PS_INVOCATION_COUNT,
            RING_FORCE_TO_NONPRIV_ACCESS_RD | RING_FORCE_TO_NONPRIV_RANGE_4,
        );
    } else if (*engine).class as i32 == VIDEO_DECODE_CLASS {
        whitelist_reg_ext(
            w,
            _MMIO(0x2000 + (*engine).mmio_base),
            RING_FORCE_TO_NONPRIV_ACCESS_RD,
        );
        whitelist_reg_ext(
            w,
            _MMIO(0x2014 + (*engine).mmio_base),
            RING_FORCE_TO_NONPRIV_ACCESS_RD,
        );
        whitelist_reg_ext(
            w,
            _MMIO(0x23b0 + (*engine).mmio_base),
            RING_FORCE_TO_NONPRIV_ACCESS_RD,
        );
    }
}

// upstream: intel_workarounds.c tgl_whitelist_build()
unsafe fn tgl_whitelist_build(engine: *mut IntelEngineCs) {
    let w = &mut (*engine).whitelist;
    allow_read_ctx_timestamp(engine);
    if (*engine).class as i32 == RENDER_CLASS {
        whitelist_reg_ext(
            w,
            PS_INVOCATION_COUNT,
            RING_FORCE_TO_NONPRIV_ACCESS_RD | RING_FORCE_TO_NONPRIV_RANGE_4,
        );
        whitelist_reg(w, GEN7_COMMON_SLICE_CHICKEN1);
        whitelist_reg(w, HIZ_CHICKEN);
        whitelist_reg(w, GEN11_COMMON_SLICE_CHICKEN3);
    }
}
// upstream: intel_workarounds.c dg2_whitelist_build()
unsafe fn dg2_whitelist_build(engine: *mut IntelEngineCs) {
    let w = &mut (*engine).whitelist;
    if (*engine).class as i32 == RENDER_CLASS {
        whitelist_mcr_reg(w, XEHP_COMMON_SLICE_CHICKEN3);
        whitelist_reg(w, GEN7_COMMON_SLICE_CHICKEN1);
    }
}
// upstream: intel_workarounds.c xelpg_whitelist_build()
unsafe fn xelpg_whitelist_build(engine: *mut IntelEngineCs) {
    dg2_whitelist_build(engine);
}
// upstream: intel_workarounds.c intel_engine_init_whitelist()
pub(crate) unsafe fn intel_engine_init_whitelist(engine: *mut IntelEngineCs) {
    let i915 = (*engine).i915;
    let w = &mut (*engine).whitelist;
    wa_init_start(
        w,
        (*engine).gt,
        c"whitelist".as_ptr(),
        (*engine).name.as_ptr(),
    );
    if (*(*engine).gt).type_ == GT_MEDIA { /* none yet */
    } else if IS_GFX_GT_IP_RANGE((*engine).gt, IP_VER(12, 70), IP_VER(12, 74)) {
        xelpg_whitelist_build(engine);
    } else if IS_DG2(i915) {
        dg2_whitelist_build(engine);
    } else if GRAPHICS_VER(i915) == 12 {
        tgl_whitelist_build(engine);
    } else if GRAPHICS_VER(i915) == 11 {
        icl_whitelist_build(engine);
    } else if IS_COMETLAKE(i915) {
        cml_whitelist_build(engine);
    } else if IS_COFFEELAKE(i915) {
        cfl_whitelist_build(engine);
    } else if IS_GEMINILAKE(i915) {
        glk_whitelist_build(engine);
    } else if IS_KABYLAKE(i915) {
        kbl_whitelist_build(engine);
    } else if IS_BROXTON(i915) {
        bxt_whitelist_build(engine);
    } else if IS_SKYLAKE(i915) {
        skl_whitelist_build(engine);
    } else if GRAPHICS_VER(i915) > 8 {
        MISSING_CASE(GRAPHICS_VER(i915));
    }
    wa_init_finish(w);
}
// upstream: intel_workarounds.c intel_engine_apply_whitelist()
pub(crate) unsafe fn intel_engine_apply_whitelist(engine: *mut IntelEngineCs) {
    let wal = &(*engine).whitelist;
    if wal.count == 0 {
        return;
    }
    for i in 0..wal.count {
        intel_uncore_write(
            (*engine).uncore,
            RING_FORCE_TO_NONPRIV((*engine).mmio_base, i),
            i915_mmio_reg_offset((*wal.list.add(i as usize)).reg()),
        );
    }
    for i in wal.count..RING_MAX_NONPRIV_SLOTS {
        intel_uncore_write(
            (*engine).uncore,
            RING_FORCE_TO_NONPRIV((*engine).mmio_base, i),
            i915_mmio_reg_offset(RING_NOPID((*engine).mmio_base)),
        );
    }
}

// upstream: intel_workarounds.c engine_fake_wa_init()
unsafe fn engine_fake_wa_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    if GRAPHICS_VER((*engine).i915) >= 12 {
        let mut mocs_w = (*(*engine).gt).mocs.uc_index;
        let mut mocs_r = mocs_w;
        if HAS_L3_CCS_READ((*engine).i915) && (*engine).class as i32 == COMPUTE_CLASS {
            mocs_r = (*(*engine).gt).mocs.wb_index;
            drm_WARN_ON!((*engine).i915, mocs_r == 0);
        }
        wa_masked_field_set(
            wal,
            RING_CMD_CCTL((*engine).mmio_base),
            CMD_CCTL_MOCS_MASK,
            CMD_CCTL_MOCS_OVERRIDE(mocs_w as u32, mocs_r as u32),
        );
    }
}
// upstream: intel_workarounds.c rcs_engine_wa_init()
unsafe fn rcs_engine_wa_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    let i915 = (*engine).i915;
    let gt = (*engine).gt;
    if IS_GFX_GT_IP_STEP(gt, IP_VER(12, 70), STEP_A0, STEP_B0)
        || IS_GFX_GT_IP_STEP(gt, IP_VER(12, 71), STEP_A0, STEP_B0)
    {
        wa_mcr_masked_en(wal, GEN10_CACHE_MODE_SS, ENABLE_EU_COUNT_FOR_TDL_FLUSH);
    }
    if IS_GFX_GT_IP_STEP(gt, IP_VER(12, 70), STEP_A0, STEP_B0)
        || IS_GFX_GT_IP_STEP(gt, IP_VER(12, 71), STEP_A0, STEP_B0)
        || IS_DG2(i915)
    {
        wa_mcr_masked_en(wal, GEN10_SAMPLER_MODE, SC_DISABLE_POWER_OPTIMIZATION_EBB);
    }
    if IS_GFX_GT_IP_STEP(gt, IP_VER(12, 70), STEP_A0, STEP_B0) || IS_DG2(i915) {
        wa_mcr_masked_en(wal, GEN8_ROW_CHICKEN2, GEN12_DISABLE_READ_SUPPRESSION);
    }
    if IS_DG2(i915) {
        wa_mcr_masked_dis(
            wal,
            XEHP_HDC_CHICKEN0,
            LSC_L1_FLUSH_CTL_3D_DATAPORT_FLUSH_EVENTS_MASK,
        );
    }
    if IS_GFX_GT_IP_RANGE(gt, IP_VER(12, 70), IP_VER(12, 71)) || IS_DG2(i915) {
        wa_mcr_add(
            wal,
            XEHP_HDC_CHICKEN0,
            0,
            REG_MASKED_FIELD_ENABLE!(DIS_ATOMIC_CHAINING_TYPED_WRITES),
            0,
            true,
        );
    }
    if IS_DG2(i915)
        || IS_ALDERLAKE_P(i915)
        || IS_ALDERLAKE_S(i915)
        || IS_DG1(i915)
        || IS_ROCKETLAKE(i915)
        || IS_TIGERLAKE(i915)
    {
        wa_masked_en(wal, GEN9_CS_DEBUG_MODE1, FF_DOP_CLOCK_GATE_DISABLE);
    }
    if IS_ALDERLAKE_P(i915)
        || IS_ALDERLAKE_S(i915)
        || IS_DG1(i915)
        || IS_ROCKETLAKE(i915)
        || IS_TIGERLAKE(i915)
    {
        wa_mcr_masked_en(wal, GEN8_ROW_CHICKEN2, GEN12_DISABLE_EARLY_READ);
        wa_write_or(
            wal,
            GEN7_FF_THREAD_MODE,
            GEN12_FF_TESSELATION_DOP_GATE_DISABLE,
        );
        wa_mcr_masked_en(wal, GEN10_SAMPLER_MODE, ENABLE_SMALLPL);
    }
    if IS_ALDERLAKE_P(i915) || IS_ALDERLAKE_S(i915) || IS_ROCKETLAKE(i915) || IS_TIGERLAKE(i915) {
        wa_mcr_masked_en(wal, GEN8_ROW_CHICKEN2, GEN12_PUSH_CONST_DEREF_HOLD_DIS);
        wa_mcr_masked_en(wal, GEN9_ROW_CHICKEN4, GEN12_DISABLE_TDL_PUSH);
    }
    if IS_ROCKETLAKE(i915) || IS_TIGERLAKE(i915) || IS_ALDERLAKE_P(i915) {
        wa_masked_en(
            wal,
            RING_PSMI_CTL(RENDER_RING_BASE),
            GEN12_WAIT_FOR_EVENT_POWER_DOWN_DISABLE | GEN8_RC_SEMA_IDLE_MSG_DISABLE,
        );
    }
    if GRAPHICS_VER(i915) == 11 {
        wa_masked_en(wal, _3D_CHICKEN3, _3D_CHICKEN3_AA_LINE_QUALITY_FIX_ENABLE);
        wa_write_or(wal, GEN8_GARBCNTL, GEN11_ARBITRATION_PRIO_ORDER_MASK);
        wa_write_clr_set(
            wal,
            GEN8_GARBCNTL,
            GEN11_HASH_CTRL_EXCL_MASK,
            GEN11_HASH_CTRL_EXCL_BIT0,
        );
        wa_write_clr_set(
            wal,
            GEN11_GLBLINVL,
            GEN11_BANK_HASH_ADDR_EXCL_MASK,
            GEN11_BANK_HASH_ADDR_EXCL_BIT0,
        );
        wa_mcr_write_or(wal, GEN8_L3SQCREG4, GEN11_LQSC_CLEAN_EVICT_DISABLE);
        wa_write_or(wal, GEN7_SARCHKMD, GEN7_DISABLE_SAMPLER_PREFETCH);
        wa_mcr_write_clr_set(
            wal,
            GEN11_SCRATCH2,
            GEN11_COHERENT_PARTIAL_WRITE_MERGE_ENABLE,
            0,
        );
        wa_masked_en(wal, GEN9_CSFE_CHICKEN1_RCS, GEN11_ENABLE_32_PLANE_MODE);
        wa_write_or(
            wal,
            GEN7_FF_THREAD_MODE,
            GEN12_FF_TESSELATION_DOP_GATE_DISABLE,
        );
        wa_masked_en(wal, GEN9_CS_DEBUG_MODE1, FF_DOP_CLOCK_GATE_DISABLE);
    }
    if GRAPHICS_VER(i915) >= 9 {
        wa_masked_en(
            wal,
            GEN7_FF_SLICE_CS_CHICKEN1,
            GEN9_FFSC_PERCTX_PREEMPT_CTRL,
        );
    }
    if IS_SKYLAKE(i915) || IS_KABYLAKE(i915) || IS_COFFEELAKE(i915) || IS_COMETLAKE(i915) {
        wa_write_or(wal, GEN8_GARBCNTL, GEN9_GAPS_TSV_CREDIT_DISABLE);
    }
    if IS_BROXTON(i915) {
        wa_masked_en(
            wal,
            FF_SLICE_CS_CHICKEN2,
            GEN9_POOLED_EU_LOAD_BALANCING_FIX_DISABLE,
        );
    }
    if GRAPHICS_VER(i915) == 9 {
        wa_masked_en(
            wal,
            GEN9_CSFE_CHICKEN1_RCS,
            GEN9_PREEMPT_GPGPU_SYNC_SWITCH_DISABLE,
        );
        wa_mcr_write_or(wal, BDW_SCRATCH1, GEN9_LBS_SLA_RETRY_TIMER_DECREMENT_ENABLE);
        if IS_GEN9_LP(i915) {
            wa_mcr_write_clr_set(
                wal,
                GEN8_L3SQCREG1,
                L3_PRIO_CREDITS_MASK,
                L3_GENERAL_PRIO_CREDITS(62) | L3_HIGH_PRIO_CREDITS(2),
            );
        }
        wa_mcr_write_or(wal, GEN8_L3SQCREG4, GEN8_LQSC_FLUSH_COHERENT_LINES);
        wa_write_clr_set(
            wal,
            GEN9_SCRATCH_LNCF1,
            GEN9_LNCF_NONIA_COHERENT_ATOMICS_ENABLE,
            0,
        );
        wa_mcr_write_clr_set(
            wal,
            GEN8_L3SQCREG4,
            GEN8_LQSQ_NONIA_COHERENT_ATOMICS_ENABLE,
            0,
        );
        wa_mcr_write_clr_set(wal, GEN9_SCRATCH1, EVICTION_PERF_FIX_ENABLE, 0);
    }
    if IS_HASWELL(i915) {
        wa_masked_en(wal, HSW_HALF_SLICE_CHICKEN3, HSW_SAMPLE_C_PERFORMANCE);
        wa_masked_dis(wal, CACHE_MODE_0_GEN7, HIZ_RAW_STALL_OPT_DISABLE);
    }
    if IS_VALLEYVIEW(i915) {
        wa_masked_en(wal, _3D_CHICKEN3, _3D_CHICKEN_SF_DISABLE_OBJEND_CULL);
        wa_write_clr_set(
            wal,
            GEN7_FF_THREAD_MODE,
            GEN7_FF_SCHED_MASK,
            GEN7_FF_TS_SCHED_HW | GEN7_FF_VS_SCHED_HW | GEN7_FF_DS_SCHED_HW,
        );
        wa_masked_en(
            wal,
            GEN7_HALF_SLICE_CHICKEN1,
            GEN7_MAX_PS_THREAD_DEP | GEN7_PSD_SINGLE_PORT_DISPATCH_ENABLE,
        );
    }
    if IS_IVYBRIDGE(i915) {
        wa_masked_en(wal, _3D_CHICKEN3, _3D_CHICKEN_SF_DISABLE_OBJEND_CULL);
        if false {
            wa_masked_dis(wal, CACHE_MODE_0_GEN7, HIZ_RAW_STALL_OPT_DISABLE);
        }
        wa_write_clr_set(
            wal,
            GEN7_FF_THREAD_MODE,
            GEN7_FF_SCHED_MASK,
            GEN7_FF_TS_SCHED_HW | GEN7_FF_VS_SCHED_HW | GEN7_FF_DS_SCHED_HW,
        );
        if (*INTEL_INFO(i915)).gt == 1 {
            wa_masked_en(
                wal,
                GEN7_HALF_SLICE_CHICKEN1,
                GEN7_PSD_SINGLE_PORT_DISPATCH_ENABLE,
            );
        }
    }
    if GRAPHICS_VER(i915) == 7 {
        wa_masked_en(
            wal,
            RING_MODE_GEN7(RENDER_RING_BASE),
            GFX_TLB_INVALIDATE_EXPLICIT | GFX_REPLAY_MODE,
        );
        wa_masked_field_set(
            wal,
            GEN7_GT_MODE,
            GEN6_WIZ_HASHING_MASK,
            GEN6_WIZ_HASHING_16x4,
        );
    }
    if IS_GRAPHICS_VER(i915, 6, 7) {
        wa_masked_en(wal, RING_MI_MODE(RENDER_RING_BASE), ASYNC_FLIP_PERF_DISABLE);
    }
    if GRAPHICS_VER(i915) == 6 {
        wa_masked_en(wal, GFX_MODE, GFX_TLB_INVALIDATE_EXPLICIT);
        wa_masked_en(wal, _3D_CHICKEN, _3D_CHICKEN_HIZ_PLANE_DISABLE_MSAA_4X_SNB);
        wa_masked_en(
            wal,
            _3D_CHICKEN3,
            _3D_CHICKEN3_SF_DISABLE_FASTCLIP_CULL | _3D_CHICKEN3_SF_DISABLE_PIPELINED_ATTR_FETCH,
        );
        wa_masked_field_set(
            wal,
            GEN6_GT_MODE,
            GEN6_WIZ_HASHING_MASK,
            GEN6_WIZ_HASHING_16x4,
        );
        wa_masked_dis(wal, CACHE_MODE_0, CM0_STC_EVICT_DISABLE_LRA_SNB);
    }
    if IS_GRAPHICS_VER(i915, 4, 6) {
        wa_add(
            wal,
            RING_MI_MODE(RENDER_RING_BASE),
            0,
            REG_MASKED_FIELD_ENABLE!(VS_TIMER_DISPATCH),
            if IS_I965G(i915) { 0 } else { VS_TIMER_DISPATCH },
            true,
        );
    }
    if GRAPHICS_VER(i915) == 4 {
        wa_add(
            wal,
            ECOSKPD(RENDER_RING_BASE),
            0,
            REG_MASKED_FIELD_ENABLE!(ECO_CONSTANT_BUFFER_SR_DISABLE),
            0,
            true,
        );
    }
}

// Source helpers from intel_gt.h and intel_gt_ccs_mode.c, kept in this
// source-order unit to avoid a placeholder layer.
unsafe fn needs_fastcolor_blt_wabb(engine: *mut IntelEngineCs) -> bool {
    IS_GFX_GT_IP_RANGE((*engine).gt, IP_VER(12, 55), IP_VER(12, 71))
        && (*engine).class as i32 == COPY_ENGINE_CLASS
        && (*engine).instance == 0
}

unsafe fn intel_gt_apply_ccs_mode(gt: *mut IntelGt) -> u32 {
    if !IS_DG2((*gt).i915) {
        return 0;
    }

    let first_ccs = __ffs(CCS_MASK(gt)) as u32;
    let mut mode = 0u32;
    for cslice in 0..I915_MAX_CCS {
        let target = if (*gt).ccs.cslices & BIT!(cslice) != 0 {
            first_ccs
        } else {
            XEHP_CCS_MODE_CSLICE_MASK
        };
        mode |= XEHP_CCS_MODE_CSLICE(cslice as u32, target);
    }
    mode
}

// upstream: intel_workarounds.c xcs_engine_wa_init()
unsafe fn xcs_engine_wa_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    let i915 = (*engine).i915;
    if IS_KABYLAKE(i915) && IS_GRAPHICS_STEP(i915, STEP_A0, STEP_F0) {
        wa_write(wal, RING_SEMA_WAIT_POLL((*engine).mmio_base), 1);
    }
    if needs_fastcolor_blt_wabb(engine) {
        wa_masked_field_set(
            wal,
            ECOSKPD((*engine).mmio_base),
            XEHP_BLITTER_SCHEDULING_MODE_MASK,
            XEHP_BLITTER_ROUND_ROBIN_MODE,
        );
    }
}
// upstream: intel_workarounds.c ccs_engine_wa_init()
unsafe fn ccs_engine_wa_init(_engine: *mut IntelEngineCs, _wal: *mut I915WaList) { /* Upstream boilerplate: no CCS-specific WAs yet. */
}
// upstream: intel_workarounds.c add_render_compute_tuning_settings()
unsafe fn add_render_compute_tuning_settings(gt: *mut IntelGt, wal: *mut I915WaList) {
    let i915 = (*gt).i915;
    if IS_GFX_GT_IP_RANGE(gt, IP_VER(12, 70), IP_VER(12, 74)) || IS_DG2(i915) {
        wa_mcr_write_clr_set(wal, RT_CTRL, STACKID_CTRL, STACKID_CTRL_512);
    }
    if tuning_thread_rr_after_dep(i915) {
        wa_mcr_masked_field_set(
            wal,
            GEN9_ROW_CHICKEN4,
            THREAD_EX_ARB_MODE,
            THREAD_EX_ARB_MODE_RR_AFTER_DEP,
        );
    }
    if GRAPHICS_VER(i915) == 12 && GRAPHICS_VER_FULL(i915) < IP_VER(12, 55) {
        wa_write_clr(wal, GEN8_GARBCNTL, GEN12_BUS_HASH_CTL_BIT_EXC);
    }
}
// upstream: intel_workarounds.c ccs_engine_wa_mode()
unsafe fn ccs_engine_wa_mode(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    let gt = (*engine).gt;
    if !IS_DG2((*gt).i915) {
        return;
    }
    wa_masked_en(wal, GEN12_RCU_MODE, XEHP_RCU_MODE_FIXED_SLICE_CCS_MODE);
    let mode = intel_gt_apply_ccs_mode(gt);
    wa_masked_en(wal, XEHP_CCS_MODE, mode);
}
// upstream: intel_workarounds.c general_render_compute_wa_init()
unsafe fn general_render_compute_wa_init(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    let i915 = (*engine).i915;
    let gt = (*engine).gt;
    add_render_compute_tuning_settings(gt, wal);
    if GRAPHICS_VER(i915) >= 11 {
        wa_mcr_masked_en(
            wal,
            GEN10_SAMPLER_MODE,
            GEN11_INDIRECT_STATE_BASE_ADDR_OVERRIDE,
        );
    }
    if IS_GFX_GT_IP_STEP(gt, IP_VER(12, 70), STEP_B0, STEP_FOREVER)
        || IS_GFX_GT_IP_STEP(gt, IP_VER(12, 71), STEP_B0, STEP_FOREVER)
        || IS_GFX_GT_IP_RANGE(gt, IP_VER(12, 74), IP_VER(12, 74))
    {
        wa_mcr_masked_en(wal, GEN9_ROW_CHICKEN3, MTL_DISABLE_FIX_FOR_EOT_FLUSH);
        wa_mcr_masked_en(wal, GEN8_ROW_CHICKEN2, XELPG_DISABLE_TDL_SVHS_GATING);
    }
    if IS_GFX_GT_IP_STEP(gt, IP_VER(12, 70), STEP_A0, STEP_B0)
        || IS_GFX_GT_IP_STEP(gt, IP_VER(12, 71), STEP_A0, STEP_B0)
    {
        wa_mcr_masked_en(wal, GEN10_SAMPLER_MODE, MTL_DISABLE_SAMPLER_SC_OOO);
    }
    if IS_GFX_GT_IP_STEP(gt, IP_VER(12, 71), STEP_A0, STEP_B0) {
        wa_mcr_masked_en(wal, GEN10_CACHE_MODE_SS, DISABLE_PREFETCH_INTO_IC);
    }
    if IS_GFX_GT_IP_STEP(gt, IP_VER(12, 70), STEP_A0, STEP_B0)
        || IS_GFX_GT_IP_STEP(gt, IP_VER(12, 71), STEP_A0, STEP_B0)
        || IS_DG2(i915)
    {
        wa_mcr_write_or(
            wal,
            LSC_CHICKEN_BIT_0_UDW,
            DISABLE_128B_EVICTION_COMMAND_UDW,
        );
        wa_masked_en(wal, VFG_PREEMPTION_CHICKEN, POLYGON_TRIFAN_LINELOOP_DISABLE);
        wa_mcr_write_or(wal, LSC_CHICKEN_BIT_0, DISABLE_D8_D16_COASLESCE);
    }
    if IS_DG2(i915) {
        wa_mcr_masked_en(wal, GEN9_ROW_CHICKEN4, XEHP_DIS_BBL_SYSPIPE);
        wa_mcr_write_or(wal, LSC_CHICKEN_BIT_0_UDW, DIS_CHAIN_2XSIMD8);
        wa_mcr_write_or(wal, LSC_CHICKEN_BIT_0_UDW, UGM_FRAGMENT_THRESHOLD_TO_3);
    }
    if IS_DG2_G11(i915) {
        wa_mcr_write_clr_set(
            wal,
            LSC_CHICKEN_BIT_0_UDW,
            MAXREQS_PER_BANK,
            REG_FIELD_PREP(MAXREQS_PER_BANK, 2),
        );
        wa_mcr_write_or(wal, LSC_CHICKEN_BIT_0, FORCE_1_SUB_MESSAGE_PER_FRAGMENT);
        wa_mcr_add(
            wal,
            GEN10_CACHE_MODE_SS,
            0,
            REG_MASKED_FIELD_ENABLE!(ENABLE_PREFETCH_INTO_IC),
            0,
            true,
        );
    }
}
// upstream: intel_workarounds.c engine_init_workarounds()
unsafe fn engine_init_workarounds(engine: *mut IntelEngineCs, wal: *mut I915WaList) {
    if GRAPHICS_VER((*engine).i915) < 4 {
        return;
    }
    engine_fake_wa_init(engine, wal);
    if (*engine).flags & I915_ENGINE_FIRST_RENDER_COMPUTE != 0 {
        general_render_compute_wa_init(engine, wal);
        ccs_engine_wa_mode(engine, wal);
    }
    if (*engine).class as i32 == COMPUTE_CLASS {
        ccs_engine_wa_init(engine, wal);
    } else if (*engine).class as i32 == RENDER_CLASS {
        rcs_engine_wa_init(engine, wal);
    } else {
        xcs_engine_wa_init(engine, wal);
    }
}
// upstream: intel_workarounds.c intel_engine_init_workarounds()
pub(crate) unsafe fn intel_engine_init_workarounds(engine: *mut IntelEngineCs) {
    let wal = &mut (*engine).wa_list;
    wa_init_start(
        wal,
        (*engine).gt,
        c"engine".as_ptr(),
        (*engine).name.as_ptr(),
    );
    engine_init_workarounds(engine, wal);
    wa_init_finish(wal);
}
// upstream: intel_workarounds.c intel_engine_apply_workarounds()
pub(crate) unsafe fn intel_engine_apply_workarounds(engine: *mut IntelEngineCs) {
    wa_list_apply(&(*engine).wa_list);
}

const MCR_RANGES_GEN8: &[I915MmioRange] = &[
    I915MmioRange {
        start: 0x5500,
        end: 0x55ff,
    },
    I915MmioRange {
        start: 0x7000,
        end: 0x7fff,
    },
    I915MmioRange {
        start: 0x9400,
        end: 0x97ff,
    },
    I915MmioRange {
        start: 0xb000,
        end: 0xb3ff,
    },
    I915MmioRange {
        start: 0xe000,
        end: 0xe7ff,
    },
];
const MCR_RANGES_GEN12: &[I915MmioRange] = &[
    I915MmioRange {
        start: 0x8150,
        end: 0x815f,
    },
    I915MmioRange {
        start: 0x9520,
        end: 0x955f,
    },
    I915MmioRange {
        start: 0xb100,
        end: 0xb3ff,
    },
    I915MmioRange {
        start: 0xde80,
        end: 0xe8ff,
    },
    I915MmioRange {
        start: 0x24a00,
        end: 0x24a7f,
    },
];
const MCR_RANGES_XEHP: &[I915MmioRange] = &[
    I915MmioRange {
        start: 0x4000,
        end: 0x4aff,
    },
    I915MmioRange {
        start: 0x5200,
        end: 0x52ff,
    },
    I915MmioRange {
        start: 0x5400,
        end: 0x7fff,
    },
    I915MmioRange {
        start: 0x8140,
        end: 0x815f,
    },
    I915MmioRange {
        start: 0x8c80,
        end: 0x8dff,
    },
    I915MmioRange {
        start: 0x94d0,
        end: 0x955f,
    },
    I915MmioRange {
        start: 0x9680,
        end: 0x96ff,
    },
    I915MmioRange {
        start: 0xb000,
        end: 0xb3ff,
    },
    I915MmioRange {
        start: 0xc800,
        end: 0xcfff,
    },
    I915MmioRange {
        start: 0xd800,
        end: 0xd8ff,
    },
    I915MmioRange {
        start: 0xdc00,
        end: 0xffff,
    },
    I915MmioRange {
        start: 0x17000,
        end: 0x17fff,
    },
    I915MmioRange {
        start: 0x24a00,
        end: 0x24a7f,
    },
];
// upstream: intel_workarounds.c mcr_range()
unsafe fn mcr_range(i915: *mut DrmI915Private, offset: u32) -> bool {
    let ranges = if GRAPHICS_VER_FULL(i915) >= IP_VER(12, 55) {
        MCR_RANGES_XEHP
    } else if GRAPHICS_VER(i915) >= 12 {
        MCR_RANGES_GEN12
    } else if GRAPHICS_VER(i915) >= 8 {
        MCR_RANGES_GEN8
    } else {
        return false;
    };
    ranges
        .iter()
        .any(|range| offset >= range.start && offset <= range.end)
}
// upstream: intel_workarounds.c wa_list_srm()
unsafe fn wa_list_srm(rq: *mut I915Request, wal: *const I915WaList, vma: *mut I915Vma) -> i32 {
    let i915 = (*rq).i915;
    let mut count = 0;
    let mut srm = MI_STORE_REGISTER_MEM | MI_SRM_LRM_GLOBAL_GTT;
    if GRAPHICS_VER(i915) >= 8 {
        srm += 1;
    }
    for i in 0..(*wal).count {
        if !mcr_range(
            i915,
            i915_mmio_reg_offset((*(*wal).list.add(i as usize)).reg()),
        ) {
            count += 1;
        }
    }
    let mut cs = intel_ring_begin(rq, 4 * count);
    if IS_ERR(cs) {
        return PTR_ERR(cs);
    }
    for i in 0..(*wal).count {
        let offset = i915_mmio_reg_offset((*(*wal).list.add(i as usize)).reg());
        if mcr_range(i915, offset) {
            continue;
        }
        *cs = srm;
        cs = cs.add(1);
        *cs = offset;
        cs = cs.add(1);
        *cs = i915_ggtt_offset(vma) + core::mem::size_of::<u32>() as u32 * i as u32;
        cs = cs.add(1);
        *cs = 0;
        cs = cs.add(1);
    }
    intel_ring_advance(rq, cs);
    0
}
// upstream: intel_workarounds.c engine_wa_list_verify()
unsafe fn engine_wa_list_verify(
    ce: *mut IntelContext,
    wal: *const I915WaList,
    from: *const i8,
) -> i32 {
    if (*wal).count == 0 {
        return 0;
    }
    let engine = (*ce).engine;
    let vma = __vm_create_scratch_for_read(
        &mut (*(*(*engine).gt).ggtt).vm,
        (*wal).count as u64 * core::mem::size_of::<u32>() as u64,
    );
    if IS_ERR(vma) {
        return PTR_ERR(vma);
    }
    intel_engine_pm_get(engine);
    let mut ww: I915GemWwCtx = core::mem::zeroed();
    i915_gem_ww_ctx_init(&mut ww, false);
    let mut err;
    'retry: loop {
        err = i915_gem_object_lock((*vma).obj, &mut ww);
        if err == 0 {
            err = intel_context_pin_ww(ce, &mut ww);
        }
        if err != 0 {
            break;
        }
        err = i915_vma_pin_ww(
            vma,
            &mut ww,
            0,
            0,
            if i915_vma_is_ggtt(vma) {
                PIN_GLOBAL
            } else {
                PIN_USER
            },
        );
        if err != 0 {
            intel_context_unpin(ce);
            break;
        }
        let rq = i915_request_create(ce);
        if IS_ERR(rq) {
            err = PTR_ERR(rq);
            i915_vma_unpin(vma);
            intel_context_unpin(ce);
            break;
        }
        err = i915_vma_move_to_active(vma, rq, EXEC_OBJECT_WRITE as u32);
        if err == 0 {
            err = wa_list_srm(rq, wal, vma);
        }
        i915_request_get(rq);
        if err != 0 {
            i915_request_set_error_once(rq, err);
        }
        i915_request_add(rq);
        if err == 0 && i915_request_wait(rq, 0, (HZ / 5) as i64) < 0 {
            err = -ETIME;
        }
        if err == 0 {
            let results = i915_gem_object_pin_map((*vma).obj, I915_MAP_WB);
            if IS_ERR(results) {
                err = PTR_ERR(results);
            } else {
                for i in 0..(*wal).count {
                    let wa = &*(*wal).list.add(i as usize);
                    if !mcr_range((*rq).i915, i915_mmio_reg_offset(wa.reg()))
                        && !wa_verify(
                            (*wal).gt,
                            wa,
                            *results.cast::<u32>().add(i as usize),
                            (*wal).name,
                            from,
                        )
                    {
                        err = -ENXIO;
                    }
                }
                i915_gem_object_unpin_map((*vma).obj);
            }
        }
        i915_request_put(rq);
        i915_vma_unpin(vma);
        intel_context_unpin(ce);
        if err == -EDEADLK {
            err = i915_gem_ww_ctx_backoff(&mut ww);
            if err == 0 {
                continue 'retry;
            }
        }
        break;
    }
    i915_gem_ww_ctx_fini(&mut ww);
    intel_engine_pm_put(engine);
    i915_vma_put(vma);
    err
}
// upstream: intel_workarounds.c intel_engine_verify_workarounds()
unsafe fn intel_engine_verify_workarounds(engine: *mut IntelEngineCs, from: *const i8) -> i32 {
    engine_wa_list_verify((*engine).kernel_context, &(*engine).wa_list, from)
}
