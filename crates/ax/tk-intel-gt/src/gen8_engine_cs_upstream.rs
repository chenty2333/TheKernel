// SPDX-License-Identifier: MIT
// Copyright © 2014 Intel Corporation
// Source-faithful Rust transcription of Linux 7.2.3
// drivers/gpu/drm/i915/gt/gen8_engine_cs.c.

use crate::{
    i915_request_types_upstream::{I915Request, i915_request_timeline},
    intel_engine_types_upstream::{IntelEngineCs, RCS0, BCS0, VCS0, VCS2, VECS0, CCS0, VIDEO_DECODE_CLASS, COPY_ENGINE_CLASS, COMPUTE_CLASS},

    intel_gt_types_upstream::IntelGt,
    intel_lrc_types_upstream::*,
    intel_lrc_upstream::lrc_indirect_bb,
    intel_ring_upstream::intel_ring_begin,
    intel_ring_types_upstream::IntelRing,

    intel_engine_types_upstream::{intel_engine_has_semaphores, intel_engine_uses_wa_hold_switchout},
    linux::requests::{intel_ring_advance, i915_request_has_initial_breadcrumb, i915_request_signaled},
    linux::bits::{set_bit, test_bit},
    intel_workarounds_types_upstream::I915RegT,
    intel_engine_regs_upstream::RING_PREDICATE_RESULT,
    intel_uc_types_upstream::intel_uc_uses_guc_submission,
    linux::i915::{graphics_ver, graphics_ver_full, IS_DG2, IS_KABYLAKE, IS_GRAPHICS_STEP, HAS_FLAT_CCS, HAS_3D_PIPELINE, i915_ggtt_offset, intel_ring_offset, intel_ring_direction, assert_ring_tail_valid, i915_mmio_reg_offset, IS_GFX_GT_IP_RANGE},
    linux::primitives::offset_in_page,
    linux::registers::*,
    linux_config::*,
};

// Upstream header-inline helpers translated from gen8_engine_cs.h.
unsafe extern "C" {
}

pub(crate) unsafe fn gen8_emit_pipe_control(cs: *mut u32, flags: u32, addr: i32) -> *mut u32 {
    unsafe { core::ptr::write_bytes(cs, 0, 6) };
    unsafe {
        *cs = GFX_OP_PIPE_CONTROL(6);
        *cs.add(1) = flags;
        *cs.add(2) = addr as u32;
        cs.add(6)
    }
}

pub(crate) unsafe fn gen12_emit_pipe_control(
    cs: *mut u32,
    group0: u32,
    group1: u32,
    addr: i32,
) -> *mut u32 {
    unsafe { core::ptr::write_bytes(cs, 0, 6) };
    unsafe {
        *cs = GFX_OP_PIPE_CONTROL(6) | group0;
        *cs.add(1) = group1;
        *cs.add(2) = addr as u32;
        cs.add(6)
    }
}

unsafe fn __gen8_emit_write_rcs(
    mut cs: *mut u32,
    value: u32,
    offset: u32,
    flags0: u32,
    flags1: u32,
) -> *mut u32 {
    unsafe {
        *cs = GFX_OP_PIPE_CONTROL(6) | flags0;
        cs = cs.add(1);
        *cs = flags1 | PIPE_CONTROL_QW_WRITE;
        cs = cs.add(1);
        *cs = offset;
        cs = cs.add(1);
        *cs = 0;
        cs = cs.add(1);
        *cs = value;
        cs = cs.add(1);
        *cs = 0;
        cs.add(1)
    }
}

pub(crate) unsafe fn gen8_emit_ggtt_write_rcs(
    cs: *mut u32,
    value: u32,
    offset: u32,
    flags: u32,
) -> *mut u32 {
    GEM_BUG_ON!(!IS_ALIGNED!(offset, 8));
    unsafe { __gen8_emit_write_rcs(cs, value, offset, 0, flags | PIPE_CONTROL_GLOBAL_GTT_IVB) }
}

pub(crate) unsafe fn gen12_emit_ggtt_write_rcs(
    cs: *mut u32,
    value: u32,
    offset: u32,
    flags0: u32,
    flags1: u32,
) -> *mut u32 {
    GEM_BUG_ON!(!IS_ALIGNED!(offset, 8));
    unsafe {
        __gen8_emit_write_rcs(
            cs,
            value,
            offset,
            flags0,
            flags1 | PIPE_CONTROL_GLOBAL_GTT_IVB,
        )
    }
}

unsafe fn __gen8_emit_flush_dw(cs: *mut u32, value: u32, offset: u32, flags: u32) -> *mut u32 {
    unsafe {
        *cs = (MI_FLUSH_DW + 1) | flags;
        *cs.add(1) = offset;
        *cs.add(2) = 0;
        *cs.add(3) = value;
        cs.add(4)
    }
}

pub(crate) unsafe fn gen8_emit_ggtt_write(
    cs: *mut u32,
    value: u32,
    offset: u32,
    flags: u32,
) -> *mut u32 {
    GEM_BUG_ON!(offset & (1 << 5) != 0);
    GEM_BUG_ON!(!IS_ALIGNED!(offset, 8));
    unsafe {
        __gen8_emit_flush_dw(
            cs,
            value,
            offset | MI_FLUSH_DW_USE_GTT,
            flags | MI_FLUSH_DW_OP_STOREDW,
        )
    }
}

#[inline]
unsafe fn ptr_err<T>(p: *mut T) -> i32 { p as isize as i32 }

// upstream: gen8_engine_cs.c gen8_emit_flush_rcs()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen8_emit_flush_rcs(rq: *mut I915Request, mode: u32) -> i32 {
    let mut vf_flush_wa = false;
    let mut dc_flush_wa = false;
    let mut flags = 0;
    let mut len = 6;
    flags |= PIPE_CONTROL_CS_STALL;
    if mode & EMIT_FLUSH != 0 {
        flags |= PIPE_CONTROL_RENDER_TARGET_CACHE_FLUSH | PIPE_CONTROL_DEPTH_CACHE_FLUSH;
        flags |= PIPE_CONTROL_DC_FLUSH_ENABLE | PIPE_CONTROL_FLUSH_ENABLE;
    }
    if mode & EMIT_INVALIDATE != 0 {
        flags |= PIPE_CONTROL_TLB_INVALIDATE | PIPE_CONTROL_INSTRUCTION_CACHE_INVALIDATE;
        flags |= PIPE_CONTROL_TEXTURE_CACHE_INVALIDATE | PIPE_CONTROL_VF_CACHE_INVALIDATE;
        flags |= PIPE_CONTROL_CONST_CACHE_INVALIDATE | PIPE_CONTROL_STATE_CACHE_INVALIDATE;
        flags |= PIPE_CONTROL_QW_WRITE | PIPE_CONTROL_STORE_DATA_INDEX;
        if graphics_ver((*rq).i915) == 9 { vf_flush_wa = true; }
        if IS_KABYLAKE((*rq).i915) && IS_GRAPHICS_STEP((*rq).i915, 0, STEP_C0) { dc_flush_wa = true; }
    }
    if vf_flush_wa { len += 6; }
    if dc_flush_wa { len += 12; }
    let mut cs = intel_ring_begin(rq, len);
    if IS_ERR(cs) { return ptr_err(cs); }
    if vf_flush_wa { cs = gen8_emit_pipe_control(cs, 0, 0); }
    if dc_flush_wa { cs = gen8_emit_pipe_control(cs, PIPE_CONTROL_DC_FLUSH_ENABLE, 0); }
    cs = gen8_emit_pipe_control(cs, flags, crate::intel_lrc_types_upstream::LRC_PPHWSP_SCRATCH_ADDR as i32);
    if dc_flush_wa { cs = gen8_emit_pipe_control(cs, PIPE_CONTROL_CS_STALL, 0); }
    intel_ring_advance(rq, cs);
    0
}

// upstream: gen8_engine_cs.c gen8_emit_flush_xcs()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen8_emit_flush_xcs(rq: *mut I915Request, mode: u32) -> i32 {
    let mut cs = intel_ring_begin(rq, 4);
    if IS_ERR(cs) { return ptr_err(cs); }
    let mut cmd = MI_FLUSH_DW + 1;
    cmd |= MI_FLUSH_DW_STORE_INDEX | MI_FLUSH_DW_OP_STOREDW;
    if mode & EMIT_INVALIDATE != 0 {
        cmd |= MI_INVALIDATE_TLB;
        if (*(*rq).engine).class == VIDEO_DECODE_CLASS as u8 { cmd |= MI_INVALIDATE_BSD; }
    }
    *cs = cmd; cs = cs.add(1);
    *cs = crate::intel_lrc_types_upstream::LRC_PPHWSP_SCRATCH_ADDR as u32; cs = cs.add(1);
    *cs = 0; cs = cs.add(1);
    *cs = 0; cs = cs.add(1);
    intel_ring_advance(rq, cs);
    0
}

// upstream: gen8_engine_cs.c gen11_emit_flush_rcs()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen11_emit_flush_rcs(rq: *mut I915Request, mode: u32) -> i32 {
    if mode & EMIT_FLUSH != 0 {
        let flags = PIPE_CONTROL_CS_STALL | PIPE_CONTROL_TILE_CACHE_FLUSH
            | PIPE_CONTROL_RENDER_TARGET_CACHE_FLUSH | PIPE_CONTROL_DEPTH_CACHE_FLUSH
            | PIPE_CONTROL_DC_FLUSH_ENABLE | PIPE_CONTROL_FLUSH_ENABLE
            | PIPE_CONTROL_QW_WRITE | PIPE_CONTROL_STORE_DATA_INDEX;
        let mut cs = intel_ring_begin(rq, 6);
        if IS_ERR(cs) { return ptr_err(cs); }
        cs = gen8_emit_pipe_control(cs, flags, crate::intel_lrc_types_upstream::LRC_PPHWSP_SCRATCH_ADDR as i32);
        intel_ring_advance(rq, cs);
    }
    if mode & EMIT_INVALIDATE != 0 {
        let flags = PIPE_CONTROL_CS_STALL | PIPE_CONTROL_COMMAND_CACHE_INVALIDATE
            | PIPE_CONTROL_TLB_INVALIDATE | PIPE_CONTROL_INSTRUCTION_CACHE_INVALIDATE
            | PIPE_CONTROL_TEXTURE_CACHE_INVALIDATE | PIPE_CONTROL_VF_CACHE_INVALIDATE
            | PIPE_CONTROL_CONST_CACHE_INVALIDATE | PIPE_CONTROL_STATE_CACHE_INVALIDATE
            | PIPE_CONTROL_QW_WRITE | PIPE_CONTROL_STORE_DATA_INDEX;
        let mut cs = intel_ring_begin(rq, 6);
        if IS_ERR(cs) { return ptr_err(cs); }
        cs = gen8_emit_pipe_control(cs, flags, crate::intel_lrc_types_upstream::LRC_PPHWSP_SCRATCH_ADDR as i32);
        intel_ring_advance(rq, cs);
    }
    0
}

// upstream: gen8_engine_cs.c preparser_disable()
unsafe fn preparser_disable(state: bool) -> u32 { MI_ARB_CHECK | (1 << 8) | state as u32 }

// upstream: gen8_engine_cs.c gen12_get_aux_inv_reg()
unsafe fn gen12_get_aux_inv_reg(engine: *mut IntelEngineCs) -> I915RegT {
    match (*engine).id as i32 {
        RCS0 => GEN12_CCS_AUX_INV, BCS0 => GEN12_BCS0_AUX_INV,
        VCS0 => GEN12_VD0_AUX_INV, VCS2 => GEN12_VD2_AUX_INV,
        VECS0 => GEN12_VE0_AUX_INV, CCS0 => GEN12_CCS0_AUX_INV,
        _ => INVALID_MMIO_REG,
    }
}

// upstream: gen8_engine_cs.c gen12_needs_ccs_aux_inv()
unsafe fn gen12_needs_ccs_aux_inv(engine: *mut IntelEngineCs) -> bool {
    let reg = gen12_get_aux_inv_reg(engine);
    i915_mmio_reg_valid(reg) && !HAS_FLAT_CCS((*engine).i915)
}

// upstream: gen8_engine_cs.c gen12_emit_aux_table_inv()
pub unsafe fn gen12_emit_aux_table_inv(engine: *mut IntelEngineCs, mut cs: *mut u32) -> *mut u32 {
    let inv_reg = gen12_get_aux_inv_reg(engine);
    let gsi_offset = (*(*(*engine).gt).uncore).gsi_offset;
    if !gen12_needs_ccs_aux_inv(engine) { return cs; }
    *cs = MI_LOAD_REGISTER_IMM(1) | MI_LRI_MMIO_REMAP_EN; cs=cs.add(1);
    *cs = i915_mmio_reg_offset(inv_reg) + gsi_offset; cs=cs.add(1);
    *cs = AUX_INV; cs=cs.add(1);
    *cs = MI_SEMAPHORE_WAIT_TOKEN | MI_SEMAPHORE_REGISTER_POLL | MI_SEMAPHORE_POLL | MI_SEMAPHORE_SAD_EQ_SDD; cs=cs.add(1);
    *cs = 0; cs=cs.add(1);
    *cs = i915_mmio_reg_offset(inv_reg) + gsi_offset; cs=cs.add(1);
    *cs = 0; cs=cs.add(1); *cs = 0; cs=cs.add(1);
    cs
}

// upstream: gen8_engine_cs.c mtl_dummy_pipe_control()
unsafe fn mtl_dummy_pipe_control(rq: *mut I915Request) -> i32 {
    if IS_GFX_GT_IP_RANGE((*(*rq).engine).gt, IP_VER(12, 70), IP_VER(12, 74)) || IS_DG2((*rq).i915) {
        let mut cs = intel_ring_begin(rq, 6);
        if IS_ERR(cs) { return ptr_err(cs); }
        cs = gen12_emit_pipe_control(cs, 0, PIPE_CONTROL_DEPTH_CACHE_FLUSH, crate::intel_lrc_types_upstream::LRC_PPHWSP_SCRATCH_ADDR as i32);
        intel_ring_advance(rq, cs);
    }
    0
}

// upstream: gen8_engine_cs.c gen12_emit_flush_rcs()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen12_emit_flush_rcs(rq: *mut I915Request, mode: u32) -> i32 {
    let engine = (*rq).engine;
    if mode & EMIT_FLUSH != 0 || gen12_needs_ccs_aux_inv(engine) {
        let mut bit_group_0 = PIPE_CONTROL0_HDC_PIPELINE_FLUSH;
        let mut bit_group_1 = 0;
        let err = mtl_dummy_pipe_control(rq); if err != 0 { return err; }
        if graphics_ver_full((*rq).i915) >= IP_VER(12,70) { bit_group_0 |= PIPE_CONTROL_CCS_FLUSH; }
        if mode & EMIT_FLUSH != 0 && graphics_ver_full((*rq).i915) < IP_VER(12,70) { bit_group_1 |= PIPE_CONTROL_FLUSH_L3; }
        bit_group_1 |= PIPE_CONTROL_TILE_CACHE_FLUSH | PIPE_CONTROL_RENDER_TARGET_CACHE_FLUSH;
        bit_group_1 |= PIPE_CONTROL_DEPTH_CACHE_FLUSH | PIPE_CONTROL_DEPTH_STALL;
        bit_group_1 |= PIPE_CONTROL_DC_FLUSH_ENABLE | PIPE_CONTROL_FLUSH_ENABLE;
        bit_group_1 |= PIPE_CONTROL_STORE_DATA_INDEX | PIPE_CONTROL_QW_WRITE | PIPE_CONTROL_CS_STALL;
        if !HAS_3D_PIPELINE((*engine).i915) { bit_group_1 &= !PIPE_CONTROL_3D_ARCH_FLAGS; }
        else if (*engine).class == COMPUTE_CLASS as u8 { bit_group_1 &= !PIPE_CONTROL_3D_ENGINE_FLAGS; }
        let mut cs = intel_ring_begin(rq, 6); if IS_ERR(cs) { return ptr_err(cs); }
        cs = gen12_emit_pipe_control(cs, bit_group_0, bit_group_1, crate::intel_lrc_types_upstream::LRC_PPHWSP_SCRATCH_ADDR as i32);
        intel_ring_advance(rq, cs);
    }
    if mode & EMIT_INVALIDATE != 0 {
        let err = mtl_dummy_pipe_control(rq); if err != 0 { return err; }
        let mut flags = PIPE_CONTROL_COMMAND_CACHE_INVALIDATE | PIPE_CONTROL_TLB_INVALIDATE;
        flags |= PIPE_CONTROL_INSTRUCTION_CACHE_INVALIDATE | PIPE_CONTROL_TEXTURE_CACHE_INVALIDATE;
        flags |= PIPE_CONTROL_VF_CACHE_INVALIDATE | PIPE_CONTROL_CONST_CACHE_INVALIDATE | PIPE_CONTROL_STATE_CACHE_INVALIDATE;
        flags |= PIPE_CONTROL_STORE_DATA_INDEX | PIPE_CONTROL_QW_WRITE | PIPE_CONTROL_CS_STALL;
        if !HAS_3D_PIPELINE((*engine).i915) { flags &= !PIPE_CONTROL_3D_ARCH_FLAGS; }
        else if (*engine).class == COMPUTE_CLASS as u8 { flags &= !PIPE_CONTROL_3D_ENGINE_FLAGS; }
        let mut count = 8; if gen12_needs_ccs_aux_inv((*rq).engine) { count += 8; }
        let mut cs = intel_ring_begin(rq, count); if IS_ERR(cs) { return ptr_err(cs); }
        *cs = preparser_disable(true); cs=cs.add(1);
        cs = gen8_emit_pipe_control(cs, flags, crate::intel_lrc_types_upstream::LRC_PPHWSP_SCRATCH_ADDR as i32);
        cs = gen12_emit_aux_table_inv(engine, cs);
        *cs = preparser_disable(false); cs=cs.add(1);
        intel_ring_advance(rq, cs);
    }
    0
}

// upstream: gen8_engine_cs.c gen12_emit_flush_xcs()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen12_emit_flush_xcs(rq: *mut I915Request, mode: u32) -> i32 {
    let mut cmd = 4;
    if mode & EMIT_INVALIDATE != 0 { cmd += 2; if gen12_needs_ccs_aux_inv((*rq).engine) { cmd += 8; } }
    let mut cs = intel_ring_begin(rq, cmd); if IS_ERR(cs) { return ptr_err(cs); }
    if mode & EMIT_INVALIDATE != 0 { *cs=preparser_disable(true); cs=cs.add(1); }
    cmd = MI_FLUSH_DW + 1;
    cmd |= MI_FLUSH_DW_STORE_INDEX | MI_FLUSH_DW_OP_STOREDW;
    if mode & EMIT_INVALIDATE != 0 {
        cmd |= MI_INVALIDATE_TLB;
        if (*(*rq).engine).class == VIDEO_DECODE_CLASS as u8 { cmd |= MI_INVALIDATE_BSD; }
        if gen12_needs_ccs_aux_inv((*rq).engine) && (*(*rq).engine).class == COPY_ENGINE_CLASS as u8 { cmd |= MI_FLUSH_DW_CCS; }
    }
    *cs=cmd; cs=cs.add(1); *cs=crate::intel_lrc_types_upstream::LRC_PPHWSP_SCRATCH_ADDR as u32; cs=cs.add(1);
    *cs=0; cs=cs.add(1); *cs=0; cs=cs.add(1);
    cs=gen12_emit_aux_table_inv((*rq).engine,cs);
    if mode & EMIT_INVALIDATE != 0 { *cs=preparser_disable(false); cs=cs.add(1); }
    intel_ring_advance(rq,cs); 0
}

// upstream: gen8_engine_cs.c preempt_address()
unsafe fn preempt_address(engine: *mut IntelEngineCs) -> u32 { i915_ggtt_offset((*engine).status_page.vma) + crate::intel_engine_api_upstream::I915_GEM_HWS_PREEMPT_ADDR as u32 }

// upstream: gen8_engine_cs.c hwsp_offset()
unsafe fn hwsp_offset(rq: *const I915Request) -> u32 {
    let tl = (*rq).timeline;
    ((*tl).hwsp_offset & !(crate::linux::config::PAGE_SIZE as u32 - 1))
        + offset_in_page((*rq).hwsp_seqno as usize) as u32
}

// upstream: gen8_engine_cs.c gen8_emit_init_breadcrumb()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen8_emit_init_breadcrumb(rq: *mut I915Request) -> i32 {
    GEM_BUG_ON!(i915_request_has_initial_breadcrumb(rq));
    if !(*i915_request_timeline(rq)).has_initial_breadcrumb { return 0; }
    let mut cs = intel_ring_begin(rq, 6); if IS_ERR(cs) { return ptr_err(cs); }
    *cs=MI_STORE_DWORD_IMM_GEN4 | MI_USE_GGTT; cs=cs.add(1); *cs=hwsp_offset(rq); cs=cs.add(1);
    *cs=0; cs=cs.add(1); *cs=(*rq).fence.seqno as u32 - 1; cs=cs.add(1); *cs=MI_NOOP; cs=cs.add(1); *cs=MI_ARB_CHECK; cs=cs.add(1);
    intel_ring_advance(rq,cs); (*rq).infix=intel_ring_offset(rq,cs.cast());
    set_bit(I915_FENCE_FLAG_INITIAL_BREADCRUMB, &mut (*rq).fence.flags); 0
}

// upstream: gen8_engine_cs.c __xehp_emit_bb_start()
unsafe fn __xehp_emit_bb_start(rq: *mut I915Request, offset: u64, _len: u32, flags: u32, arb: u32) -> i32 {
    let ce=(*rq).context; let wa_offset=lrc_indirect_bb(ce);
    GEM_BUG_ON!((*ce).wa_bb_page==0);
    let mut cs=intel_ring_begin(rq,12); if IS_ERR(cs){return ptr_err(cs);}
    *cs=MI_ARB_ON_OFF|arb; cs=cs.add(1);
    *cs=MI_LOAD_REGISTER_MEM_GEN8|MI_SRM_LRM_GLOBAL_GTT|MI_LRI_LRM_CS_MMIO; cs=cs.add(1);
    *cs=i915_mmio_reg_offset(RING_PREDICATE_RESULT(0)); cs=cs.add(1); *cs=wa_offset+crate::intel_lrc_types_upstream::DG2_PREDICATE_RESULT_WA as u32; cs=cs.add(1); *cs=0; cs=cs.add(1);
    *cs=MI_BATCH_BUFFER_START_GEN8|if flags&I915_DISPATCH_SECURE!=0{0}else{BIT!(8)}; cs=cs.add(1);
    *cs=offset as u32; cs=cs.add(1); *cs=(offset>>32) as u32; cs=cs.add(1);
    *cs=MI_BATCH_BUFFER_START_GEN8; cs=cs.add(1); *cs=wa_offset+crate::intel_lrc_types_upstream::DG2_PREDICATE_RESULT_BB as u32; cs=cs.add(1); *cs=0; cs=cs.add(1);
    *cs=MI_ARB_ON_OFF|MI_ARB_DISABLE; cs=cs.add(1); intel_ring_advance(rq,cs); 0
}
// upstream: gen8_engine_cs.c xehp_emit_bb_start_noarb()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xehp_emit_bb_start_noarb(rq:*mut I915Request,offset:u64,len:u32,flags:u32)->i32 { __xehp_emit_bb_start(rq,offset,len,flags,MI_ARB_DISABLE) }
// upstream: gen8_engine_cs.c xehp_emit_bb_start()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xehp_emit_bb_start(rq:*mut I915Request,offset:u64,len:u32,flags:u32)->i32 { __xehp_emit_bb_start(rq,offset,len,flags,MI_ARB_ENABLE) }

// upstream: gen8_engine_cs.c gen8_emit_bb_start_noarb()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen8_emit_bb_start_noarb(rq:*mut I915Request,offset:u64,_len:u32,flags:u32)->i32 {
    let mut cs=intel_ring_begin(rq,4); if IS_ERR(cs){return ptr_err(cs);}
    *cs=MI_ARB_ON_OFF|MI_ARB_DISABLE; cs=cs.add(1); *cs=MI_BATCH_BUFFER_START_GEN8|if flags&I915_DISPATCH_SECURE!=0{0}else{BIT!(8)}; cs=cs.add(1);
    *cs=offset as u32; cs=cs.add(1); *cs=(offset>>32) as u32; cs=cs.add(1); intel_ring_advance(rq,cs); 0
}
// upstream: gen8_engine_cs.c gen8_emit_bb_start()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen8_emit_bb_start(rq:*mut I915Request,offset:u64,len:u32,flags:u32)->i32 {
    if test_bit(crate::i915_request_types_upstream::I915_FENCE_FLAG_NOPREEMPT, &(*rq).fence.flags){return gen8_emit_bb_start_noarb(rq,offset,len,flags);}
    let mut cs=intel_ring_begin(rq,6); if IS_ERR(cs){return ptr_err(cs);}
    *cs=MI_ARB_ON_OFF|MI_ARB_ENABLE; cs=cs.add(1); *cs=MI_BATCH_BUFFER_START_GEN8|if flags&I915_DISPATCH_SECURE!=0{0}else{BIT!(8)}; cs=cs.add(1);
    *cs=offset as u32; cs=cs.add(1); *cs=(offset>>32) as u32; cs=cs.add(1); *cs=MI_ARB_ON_OFF|MI_ARB_DISABLE; cs=cs.add(1); *cs=MI_NOOP; cs=cs.add(1); intel_ring_advance(rq,cs); 0
}

// upstream: gen8_engine_cs.c assert_request_valid()
unsafe fn assert_request_valid(rq:*mut I915Request){let ring=(*rq).ring; GEM_BUG_ON!(intel_ring_direction(ring,(*rq).wa_tail,(*rq).head)<=0);}
// upstream: gen8_engine_cs.c gen8_emit_wa_tail()
unsafe fn gen8_emit_wa_tail(rq:*mut I915Request,mut cs:*mut u32)->*mut u32 {*cs=MI_ARB_CHECK;cs=cs.add(1);*cs=MI_NOOP;cs=cs.add(1);(*rq).wa_tail=intel_ring_offset(rq,cs.cast());assert_request_valid(rq);cs}
// upstream: gen8_engine_cs.c emit_preempt_busywait()
unsafe fn emit_preempt_busywait(rq:*mut I915Request,mut cs:*mut u32)->*mut u32 {
    *cs=MI_ARB_CHECK;cs=cs.add(1);*cs=MI_SEMAPHORE_WAIT|MI_SEMAPHORE_GLOBAL_GTT|MI_SEMAPHORE_POLL|MI_SEMAPHORE_SAD_EQ_SDD;cs=cs.add(1);
    *cs=0;cs=cs.add(1);*cs=preempt_address((*rq).engine);cs=cs.add(1);*cs=0;cs=cs.add(1);*cs=MI_NOOP;cs=cs.add(1);cs
}
// upstream: gen8_engine_cs.c gen8_emit_fini_breadcrumb_tail()
unsafe fn gen8_emit_fini_breadcrumb_tail(rq:*mut I915Request,mut cs:*mut u32)->*mut u32 {
    *cs=MI_USER_INTERRUPT;cs=cs.add(1);*cs=MI_ARB_ON_OFF|MI_ARB_ENABLE;cs=cs.add(1);
    if intel_engine_has_semaphores((*rq).engine)&&!intel_uc_uses_guc_submission(core::ptr::addr_of_mut!((*(*(*rq).engine).gt).uc)){cs=emit_preempt_busywait(rq,cs);}
    (*rq).tail=intel_ring_offset(rq,cs.cast());assert_ring_tail_valid((*rq).ring,(*rq).tail);gen8_emit_wa_tail(rq,cs)
}
// upstream: gen8_engine_cs.c emit_xcs_breadcrumb()
unsafe fn emit_xcs_breadcrumb(rq:*mut I915Request,cs:*mut u32)->*mut u32 {gen8_emit_ggtt_write(cs,(*rq).fence.seqno as u32,hwsp_offset(rq),0)}
// upstream: gen8_engine_cs.c gen8_emit_fini_breadcrumb_xcs()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen8_emit_fini_breadcrumb_xcs(rq:*mut I915Request,cs:*mut u32)->*mut u32 {gen8_emit_fini_breadcrumb_tail(rq,emit_xcs_breadcrumb(rq,cs))}
// upstream: gen8_engine_cs.c gen8_emit_fini_breadcrumb_rcs()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen8_emit_fini_breadcrumb_rcs(rq:*mut I915Request,mut cs:*mut u32)->*mut u32 {
    cs=gen8_emit_pipe_control(cs,PIPE_CONTROL_CS_STALL|PIPE_CONTROL_TLB_INVALIDATE|PIPE_CONTROL_RENDER_TARGET_CACHE_FLUSH|PIPE_CONTROL_DEPTH_CACHE_FLUSH|PIPE_CONTROL_DC_FLUSH_ENABLE,0);
    cs=gen8_emit_ggtt_write_rcs(cs,(*rq).fence.seqno as u32,hwsp_offset(rq),PIPE_CONTROL_FLUSH_ENABLE|PIPE_CONTROL_CS_STALL);gen8_emit_fini_breadcrumb_tail(rq,cs)
}
// upstream: gen8_engine_cs.c gen11_emit_fini_breadcrumb_rcs()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen11_emit_fini_breadcrumb_rcs(rq:*mut I915Request,mut cs:*mut u32)->*mut u32 {
    cs=gen8_emit_pipe_control(cs,PIPE_CONTROL_CS_STALL|PIPE_CONTROL_TLB_INVALIDATE|PIPE_CONTROL_TILE_CACHE_FLUSH|PIPE_CONTROL_RENDER_TARGET_CACHE_FLUSH|PIPE_CONTROL_DEPTH_CACHE_FLUSH|PIPE_CONTROL_DC_FLUSH_ENABLE,0);
    cs=gen8_emit_ggtt_write_rcs(cs,(*rq).fence.seqno as u32,hwsp_offset(rq),PIPE_CONTROL_FLUSH_ENABLE|PIPE_CONTROL_CS_STALL);gen8_emit_fini_breadcrumb_tail(rq,cs)
}

// upstream: gen8_engine_cs.c gen12_emit_preempt_busywait()
unsafe fn gen12_emit_preempt_busywait(rq:*mut I915Request,mut cs:*mut u32)->*mut u32 {
    *cs=MI_ARB_CHECK;cs=cs.add(1);*cs=MI_SEMAPHORE_WAIT_TOKEN|MI_SEMAPHORE_GLOBAL_GTT|MI_SEMAPHORE_POLL|MI_SEMAPHORE_SAD_EQ_SDD;cs=cs.add(1);
    *cs=0;cs=cs.add(1);*cs=preempt_address((*rq).engine);cs=cs.add(1);*cs=0;cs=cs.add(1);*cs=0;cs=cs.add(1);cs
}
// upstream: gen8_engine_cs.c hold_switchout_semaphore_offset()
unsafe fn hold_switchout_semaphore_offset(rq:*mut I915Request)->u32 {i915_ggtt_offset((*(*rq).context).state)+(crate::intel_lrc_types_upstream::LRC_PPHWSP_PN * crate::linux::config::PAGE_SIZE) as u32+0x540}
// upstream: gen8_engine_cs.c hold_switchout_emit_wa_busywait()
unsafe fn hold_switchout_emit_wa_busywait(rq:*mut I915Request,mut cs:*mut u32)->*mut u32 {
    *cs=MI_ATOMIC_INLINE|MI_ATOMIC_GLOBAL_GTT|MI_ATOMIC_CS_STALL|MI_ATOMIC_MOVE;cs=cs.add(1);*cs=hold_switchout_semaphore_offset(rq);cs=cs.add(1);*cs=0;cs=cs.add(1);*cs=1;cs=cs.add(1);
    for _ in 0..8{*cs=0;cs=cs.add(1);} *cs=MI_SEMAPHORE_WAIT|MI_SEMAPHORE_GLOBAL_GTT|MI_SEMAPHORE_POLL|MI_SEMAPHORE_SAD_EQ_SDD;cs=cs.add(1);*cs=0;cs=cs.add(1);*cs=hold_switchout_semaphore_offset(rq);cs=cs.add(1);*cs=0;cs=cs.add(1);cs
}
// upstream: gen8_engine_cs.c gen12_emit_fini_breadcrumb_tail()
unsafe fn gen12_emit_fini_breadcrumb_tail(rq:*mut I915Request,mut cs:*mut u32)->*mut u32 {
    *cs=MI_USER_INTERRUPT;cs=cs.add(1);*cs=MI_ARB_ON_OFF|MI_ARB_ENABLE;cs=cs.add(1);
    if intel_engine_has_semaphores((*rq).engine)&&!intel_uc_uses_guc_submission(core::ptr::addr_of_mut!((*(*(*rq).engine).gt).uc)){cs=gen12_emit_preempt_busywait(rq,cs);}
    if intel_engine_uses_wa_hold_switchout((*rq).engine){cs=hold_switchout_emit_wa_busywait(rq,cs);}
    (*rq).tail=intel_ring_offset(rq,cs.cast());assert_ring_tail_valid((*rq).ring,(*rq).tail);gen8_emit_wa_tail(rq,cs)
}
// upstream: gen8_engine_cs.c gen12_emit_fini_breadcrumb_xcs()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen12_emit_fini_breadcrumb_xcs(rq:*mut I915Request,mut cs:*mut u32)->*mut u32 {cs=emit_xcs_breadcrumb(rq,__gen8_emit_flush_dw(cs,0,0,0));gen12_emit_fini_breadcrumb_tail(rq,cs)}
// upstream: gen8_engine_cs.c gen12_emit_fini_breadcrumb_rcs()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen12_emit_fini_breadcrumb_rcs(rq:*mut I915Request,mut cs:*mut u32)->*mut u32 {
    let i915=(*rq).i915;let gt=(*(*rq).engine).gt;
    let mut flags=PIPE_CONTROL_CS_STALL|PIPE_CONTROL_TLB_INVALIDATE|PIPE_CONTROL_TILE_CACHE_FLUSH|PIPE_CONTROL_RENDER_TARGET_CACHE_FLUSH|PIPE_CONTROL_DEPTH_CACHE_FLUSH|PIPE_CONTROL_DC_FLUSH_ENABLE|PIPE_CONTROL_FLUSH_ENABLE;
    if graphics_ver_full(i915)<IP_VER(12,70){flags|=PIPE_CONTROL_FLUSH_L3;}
    if IS_GFX_GT_IP_RANGE(gt,IP_VER(12,70),IP_VER(12,74))||IS_DG2(i915){cs=gen12_emit_pipe_control(cs,0,PIPE_CONTROL_DEPTH_CACHE_FLUSH,0);}
    if graphics_ver(i915)==12&&graphics_ver_full(i915)<IP_VER(12,55){flags|=PIPE_CONTROL_DEPTH_STALL;}
    if !HAS_3D_PIPELINE(i915){flags&=!PIPE_CONTROL_3D_ARCH_FLAGS;}else if (*(*rq).engine).class==COMPUTE_CLASS as u8{flags&=!PIPE_CONTROL_3D_ENGINE_FLAGS;}
    cs=gen12_emit_pipe_control(cs,PIPE_CONTROL0_HDC_PIPELINE_FLUSH,flags,0);
    cs=gen12_emit_ggtt_write_rcs(cs,(*rq).fence.seqno as u32,hwsp_offset(rq),0,PIPE_CONTROL_FLUSH_ENABLE|PIPE_CONTROL_CS_STALL);
    gen12_emit_fini_breadcrumb_tail(rq,cs)
}
