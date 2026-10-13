// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by batch B3d (BW, display IRQ, global state, CMTG).
//! Replaced by intel_bw_upstream.rs, intel_display_irq_upstream.rs,
//! intel_global_state_upstream.rs, intel_dbuf_bw_upstream.rs (B3d).

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_display_core_types_upstream::IntelDisplay;
// Preserve this dependency path while the B3a/B3b callers are wired to their
// B3d owner; these are aliases to the real translated lifetime/lock code.
pub use crate::intel_global_state_upstream::{
    intel_atomic_global_obj_cleanup, intel_atomic_global_obj_init,
};
pub use crate::{
    intel_bw_upstream::{
        icl_sagv_post_plane_update, icl_sagv_pre_plane_update, intel_atomic_get_bw_state,
        intel_atomic_get_new_bw_state, intel_atomic_get_old_bw_state, intel_bw_atomic_check,
        intel_bw_can_enable_sagv, intel_bw_crtc_disable_noatomic, intel_bw_init, intel_bw_init_hw,
        intel_bw_pmdemand_needs_update, intel_bw_qgv_point_peakbw, intel_bw_update_hw_state,
    },
    intel_dbuf_bw_upstream::intel_dbuf_bw_init,
    intel_display_irq_upstream::{
        bdw_disable_pipe_irq, bdw_enable_pipe_irq, bdw_update_port_irq,
        gen8_irq_power_well_post_enable, gen8_irq_power_well_pre_disable, gen11_gu_misc_irq_ack,
        gen11_gu_misc_irq_handler, intel_display_irq_ack, intel_display_irq_handler,
        intel_display_irq_init, intel_display_irq_postinstall, intel_display_irq_reset,
    },
    skl_watermark_upstream::intel_dbuf_init,
};

// upstream: intel_display_irq.c ilk_enable_display_irq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ilk_enable_display_irq(_display: *mut IntelDisplay, _mask: u32) {}

// upstream: intel_display_irq.c ilk_disable_display_irq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ilk_disable_display_irq(_display: *mut IntelDisplay, _mask: u32) {}
