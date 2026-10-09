// SPDX-License-Identifier: MIT
// Copyright © 2014 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/gt/intel_renderstate.c.
// The MIT grant is retained in ../LICENSE-MIT. The gen6-gen9 immutable
// render-state data tables are external owner data; Gen12 selects no table.

#![allow(unsafe_code, non_snake_case)]

use core::ptr;

use crate::{
    i915_gem_object_api_upstream::{i915_gem_object_lock, i915_gem_object_put},
    i915_gem_object_types_upstream::{DrmI915GemObject, I915_MAP_WB},
    i915_gem_pages_upstream::{
        __i915_gem_object_flush_map, __i915_gem_object_release_map, i915_gem_object_pin_map,
    },
    i915_gem_ww_upstream::{
        I915GemWwCtx, i915_gem_ww_ctx_backoff, i915_gem_ww_ctx_fini, i915_gem_ww_ctx_init,
    },
    i915_request_types_upstream::I915Request,
    i915_vma_api_upstream::{
        i915_vma_close, i915_vma_move_to_active, i915_vma_pin_ww, i915_vma_unpin,
    },
    i915_vma_types_upstream::I915Vma,
    intel_context_api_upstream::{intel_context_pin_ww, intel_context_unpin},
    intel_context_types_upstream::IntelContext,
    intel_engine_api_upstream::CACHELINE_DWORDS,
    intel_engine_cs_upstream::IntelEngineCs,
    intel_engine_types_upstream::RENDER_CLASS,
    intel_ring_upstream::i915_gem_object_create_internal,
    linux::{
        config::{EINVAL, PAGE_SIZE, PIN_GLOBAL, PIN_HIGH},
        i915::{GRAPHICS_VER, HAS_64BIT_RELOC, HAS_POOLED_EU, i915_ggtt_offset},
        registers::{GEN9_MEDIA_POOL_ENABLE, GEN9_MEDIA_POOL_STATE, MI_BATCH_BUFFER_END, MI_NOOP},
    },
    linux_config::{IS_ERR, PTR_ERR},
    linux_i915_private::DrmI915Private,
};

#[repr(C)]
pub struct IntelRenderstateRodata {
    pub reloc: *const i32,
    pub batch: *const u32,
    pub batch_items: u32,
}

#[repr(C)]
pub struct IntelRenderstate {
    pub ww: I915GemWwCtx,
    pub rodata: *const IntelRenderstateRodata,
    pub vma: *mut I915Vma,
    pub batch_offset: u32,
    pub batch_size: u32,
    pub aux_offset: u32,
    pub aux_size: u32,
}

unsafe extern "C" {
    static gen6_null_state: IntelRenderstateRodata;
    static gen7_null_state: IntelRenderstateRodata;
    static gen8_null_state: IntelRenderstateRodata;
    static gen9_null_state: IntelRenderstateRodata;
}

const _: [(); 88] = [(); core::mem::size_of::<IntelRenderstate>()];

// upstream: intel_renderstate.c render_state_get_rodata()
unsafe fn render_state_get_rodata(engine: *const IntelEngineCs) -> *const IntelRenderstateRodata {
    if unsafe { (*engine).class as i32 } != RENDER_CLASS {
        return ptr::null();
    }

    match unsafe { GRAPHICS_VER((*engine).i915) } {
        6 => ptr::addr_of!(gen6_null_state),
        7 => ptr::addr_of!(gen7_null_state),
        8 => ptr::addr_of!(gen8_null_state),
        9 => ptr::addr_of!(gen9_null_state),
        _ => ptr::null(),
    }
}

unsafe fn out_batch(data: *mut u32, index: &mut usize, value: u32) -> bool {
    if *index >= PAGE_SIZE / core::mem::size_of::<u32>() {
        return false;
    }
    unsafe { data.add(*index).write(value) };
    *index += 1;
    true
}

// upstream: intel_renderstate.c render_state_setup()
unsafe fn render_state_setup(so: *mut IntelRenderstate, i915: *mut DrmI915Private) -> i32 {
    let rodata = unsafe { &*(*so).rodata };
    let mut index = 0usize;
    let mut reloc_index = 0usize;
    let mut ret = -EINVAL;
    let data = unsafe { i915_gem_object_pin_map((*(*so).vma).obj, I915_MAP_WB) }.cast::<u32>();
    if IS_ERR(data) {
        return PTR_ERR(data);
    }

    while index < rodata.batch_items as usize {
        let mut word = unsafe { *rodata.batch.add(index) };
        if index * core::mem::size_of::<u32>() == unsafe { *rodata.reloc.add(reloc_index) } as usize
        {
            let address = u64::from(word)
                .wrapping_add(unsafe { crate::i915_vma_api_upstream::i915_vma_offset((*so).vma) });
            word = address as u32;
            if unsafe { HAS_64BIT_RELOC(i915) } {
                if index + 1 >= rodata.batch_items as usize
                    || unsafe { *rodata.batch.add(index + 1) } != 0
                {
                    break;
                }
                unsafe { data.add(index).write(word) };
                index += 1;
                word = (address >> 32) as u32;
            }
            reloc_index += 1;
        }
        unsafe { data.add(index).write(word) };
        index += 1;
    }

    if index < rodata.batch_items as usize {
        // Preserve the source's common `out:` cleanup after relocation overflow.
    } else if unsafe { *rodata.reloc.add(reloc_index) } != -1 {
        drm_err!(
            unsafe { ptr::addr_of_mut!((*i915).drm) },
            "only %d relocs resolved\n",
            reloc_index
        );
    } else {
        unsafe {
            (*so).batch_offset = i915_ggtt_offset((*so).vma);
            (*so).batch_size = rodata.batch_items * core::mem::size_of::<u32>() as u32;
        }
        let mut overflow = false;
        while index % CACHELINE_DWORDS != 0 {
            if !unsafe { out_batch(data, &mut index, MI_NOOP) } {
                overflow = true;
                break;
            }
        }
        if !overflow {
            unsafe { (*so).aux_offset = (index * core::mem::size_of::<u32>()) as u32 };
            if unsafe { HAS_POOLED_EU(i915) } {
                for value in [
                    GEN9_MEDIA_POOL_STATE,
                    GEN9_MEDIA_POOL_ENABLE,
                    0x0077_7000,
                    0,
                    0,
                    0,
                ] {
                    if !unsafe { out_batch(data, &mut index, value) } {
                        overflow = true;
                        break;
                    }
                }
            }
            if !overflow && !unsafe { out_batch(data, &mut index, MI_BATCH_BUFFER_END) } {
                overflow = true;
            }
        }
        if !overflow {
            unsafe {
                (*so).aux_size = (index * core::mem::size_of::<u32>()) as u32 - (*so).aux_offset;
                (*so).aux_offset += (*so).batch_offset;
                (*so).aux_size = ((*so).aux_size + 7) & !7;
            }
            ret = 0;
        }
    }

    unsafe {
        __i915_gem_object_flush_map((*(*so).vma).obj, 0, (index * 4) as u64);
        __i915_gem_object_release_map((*(*so).vma).obj);
    }
    ret
}

// upstream: intel_renderstate.c intel_renderstate_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_renderstate_init(
    so: *mut IntelRenderstate,
    ce: *mut IntelContext,
) -> i32 {
    let engine = unsafe { (*ce).engine };
    let mut obj: *mut DrmI915GemObject = ptr::null_mut();
    let mut err: i32;
    unsafe { ptr::write_bytes(so, 0, 1) };
    unsafe { (*so).rodata = render_state_get_rodata(engine) };
    if !unsafe { (*so).rodata }.is_null() {
        let rodata = unsafe { &*(*so).rodata };
        if rodata.batch_items as usize * 4 > PAGE_SIZE {
            return -EINVAL;
        }
        obj = unsafe { i915_gem_object_create_internal((*engine).i915, PAGE_SIZE as u64) };
        if IS_ERR(obj) {
            return PTR_ERR(obj);
        }
        let vm = unsafe { &mut (*(*(*engine).gt).ggtt).vm };
        unsafe {
            (*so).vma = crate::i915_vma_api_upstream::i915_vma_instance(obj, vm, ptr::null())
        };
        if unsafe { IS_ERR((*so).vma) } {
            err = unsafe { PTR_ERR((*so).vma) };
            unsafe { i915_gem_object_put(obj) };
            unsafe { (*so).vma = ptr::null_mut() };
            return err;
        }
    }

    unsafe { i915_gem_ww_ctx_init(ptr::addr_of_mut!((*so).ww), true) };
    'retry: loop {
        err = unsafe { intel_context_pin_ww(ce, ptr::addr_of_mut!((*so).ww)) };
        if err != 0 {
            break;
        }
        if unsafe { (*so).rodata.is_null() } {
            return 0;
        }
        err = unsafe { i915_gem_object_lock((*(*so).vma).obj, ptr::addr_of_mut!((*so).ww)) };
        if err != 0 {
            unsafe { intel_context_unpin(ce) };
        } else {
            err = unsafe {
                i915_vma_pin_ww(
                    (*so).vma,
                    ptr::addr_of_mut!((*so).ww),
                    0,
                    0,
                    PIN_GLOBAL | PIN_HIGH,
                )
            };
            if err == 0 {
                err = unsafe { render_state_setup(so, (*engine).i915) };
                if err == 0 {
                    return 0;
                }
                unsafe { i915_vma_unpin((*so).vma) };
            }
            unsafe { intel_context_unpin(ce) };
        }
        if err == -crate::linux_config::EDEADLK {
            err = unsafe { i915_gem_ww_ctx_backoff(ptr::addr_of_mut!((*so).ww)) };
            if err == 0 {
                continue 'retry;
            }
        }
        break;
    }
    unsafe { i915_gem_ww_ctx_fini(ptr::addr_of_mut!((*so).ww)) };
    if !obj.is_null() {
        unsafe { i915_gem_object_put(obj) };
    }
    unsafe { (*so).vma = ptr::null_mut() };
    err
}

// upstream: intel_renderstate.c intel_renderstate_emit()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_renderstate_emit(
    so: *mut IntelRenderstate,
    rq: *mut I915Request,
) -> i32 {
    let engine = unsafe { (*rq).engine };
    if unsafe { (*so).vma.is_null() } {
        return 0;
    }
    let mut err = unsafe { i915_vma_move_to_active((*so).vma, rq, 0) };
    if err != 0 {
        return err;
    }
    let emit =
        unsafe { (*engine).emit_bb_start }.expect("engine emit_bb_start callback is uninitialized");
    err = unsafe {
        emit(
            rq,
            (*so).batch_offset as u64,
            (*so).batch_size,
            crate::intel_engine_types_upstream::I915_DISPATCH_SECURE,
        )
    };
    if err != 0 {
        return err;
    }
    if unsafe { (*so).aux_size > 8 } {
        err = unsafe {
            emit(
                rq,
                (*so).aux_offset as u64,
                (*so).aux_size,
                crate::intel_engine_types_upstream::I915_DISPATCH_SECURE,
            )
        };
    }
    err
}

// upstream: intel_renderstate.c intel_renderstate_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_renderstate_fini(so: *mut IntelRenderstate, ce: *mut IntelContext) {
    if !unsafe { (*so).vma.is_null() } {
        unsafe {
            i915_vma_unpin((*so).vma);
            i915_vma_close((*so).vma);
        }
    }
    unsafe {
        intel_context_unpin(ce);
        i915_gem_ww_ctx_fini(ptr::addr_of_mut!((*so).ww));
    }
    if !unsafe { (*so).vma.is_null() } {
        unsafe { i915_gem_object_put((*(*so).vma).obj) };
    }
}

const _: [(); 24] = [(); core::mem::size_of::<IntelRenderstateRodata>()];
const _: [(); core::mem::align_of::<usize>()] =
    [(); core::mem::align_of::<IntelRenderstateRodata>()];
