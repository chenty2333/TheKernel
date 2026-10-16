// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_psr.c (Linux v7.2.3).
//! PSR / Panel Replay capability predicates used by the DDI and AUX power paths.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::mem::offset_of;

use crate::{
    intel_de_upstream::intel_de_write_fw,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_types_upstream::{
        INTEL_OUTPUT_DDI, INTEL_OUTPUT_DP, INTEL_OUTPUT_DP_MST, INTEL_OUTPUT_EDP, IntelCrtcState,
        IntelDigitalPort, IntelDp, IntelEncoder,
    },
    intel_psr_regs_upstream::*,
};

/// `intel_encoder_is_dp()` (intel_display_types.h, inline).
#[inline]
unsafe fn intel_encoder_is_dp(encoder: *const IntelEncoder) -> bool {
    let output_type = unsafe { (*encoder).r#type };
    match output_type {
        INTEL_OUTPUT_DP | INTEL_OUTPUT_EDP => true,
        INTEL_OUTPUT_DDI => {
            // Skip pure HDMI/DVI DDI encoders: the DP output register must be valid.
            let dig = unsafe { encoder.cast::<u8>().sub(offset_of!(IntelDigitalPort, base)) }
                .cast::<IntelDigitalPort>();
            unsafe { (*dig).dp.output_reg.reg != 0 }
        }
        _ => false,
    }
}

/// `enc_to_intel_dp()`.
#[inline]
unsafe fn enc_to_intel_dp(encoder: *const IntelEncoder) -> *mut IntelDp {
    unsafe {
        encoder
            .cast::<u8>()
            .sub(offset_of!(IntelDigitalPort, base))
            .cast::<u8>()
            .add(offset_of!(IntelDigitalPort, dp))
            .cast::<IntelDp>()
            .cast_mut()
    }
}

/// `CAN_PSR()` (intel_psr.c).
#[inline]
unsafe fn can_psr(intel_dp: *const IntelDp) -> bool {
    unsafe { (*intel_dp).psr.sink_support && (*intel_dp).psr.source_support }
}

/// `CAN_PANEL_REPLAY()` (intel_psr.h).
#[inline]
unsafe fn can_panel_replay(intel_dp: *const IntelDp) -> bool {
    unsafe {
        (*intel_dp).psr.sink_panel_replay_support && (*intel_dp).psr.source_panel_replay_support
    }
}

/// `intel_encoder_can_psr()`.
// upstream: intel_psr.c intel_encoder_can_psr()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_encoder_can_psr(encoder: *mut IntelEncoder) -> bool {
    unsafe {
        if intel_encoder_is_dp(encoder) || (*encoder).r#type == INTEL_OUTPUT_DP_MST {
            let intel_dp = enc_to_intel_dp(encoder);
            can_psr(intel_dp) || can_panel_replay(intel_dp)
        } else {
            false
        }
    }
}

/// `intel_psr_needs_aux_io_power()`: only eDP needs AUX IO power for PSR/PR.
// upstream: intel_psr.c intel_psr_needs_aux_io_power()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_psr_needs_aux_io_power(
    encoder: *mut IntelEncoder,
    crtc_state: *const IntelCrtcState,
) -> bool {
    unsafe {
        crate::intel_atomic_upstream::intel_crtc_has_type(crtc_state, INTEL_OUTPUT_EDP)
            && intel_encoder_can_psr(encoder)
    }
}

// ---- intel_psr.c PSR2 manual-track helpers (panic path) ----

/// `display->platform.alderlake_p || DISPLAY_VER(display) >= 14`.
#[inline]
unsafe fn adlp_or_ver14(display: *const IntelDisplay) -> bool {
    unsafe {
        crate::intel_display_wa_upstream::platform_alderlake_p(display)
            || crate::intel_display_reg_defs_upstream::DISPLAY_VER(display) >= 14
    }
}

/// `man_trk_ctl_enable_bit_get()`.
// upstream: intel_psr.c man_trk_ctl_enable_bit_get()
unsafe fn man_trk_ctl_enable_bit_get(display: *const IntelDisplay) -> u32 {
    if unsafe { adlp_or_ver14(display) } {
        0
    } else {
        PSR2_MAN_TRK_CTL_ENABLE
    }
}

/// `man_trk_ctl_partial_frame_bit_get()`.
// upstream: intel_psr.c man_trk_ctl_partial_frame_bit_get()
unsafe fn man_trk_ctl_partial_frame_bit_get(display: *const IntelDisplay) -> u32 {
    if unsafe { adlp_or_ver14(display) } {
        ADLP_PSR2_MAN_TRK_CTL_SF_PARTIAL_FRAME_UPDATE
    } else {
        PSR2_MAN_TRK_CTL_SF_PARTIAL_FRAME_UPDATE
    }
}

/// `man_trk_ctl_continuos_full_frame()`.
// upstream: intel_psr.c man_trk_ctl_continuos_full_frame()
unsafe fn man_trk_ctl_continuos_full_frame(display: *const IntelDisplay) -> u32 {
    if unsafe { adlp_or_ver14(display) } {
        ADLP_PSR2_MAN_TRK_CTL_SF_CONTINUOS_FULL_FRAME
    } else {
        PSR2_MAN_TRK_CTL_SF_CONTINUOS_FULL_FRAME
    }
}

/// `intel_psr2_panic_force_full_update()`: force a full PSR2 update from the
/// panic path, without taking any lock.
// upstream: intel_psr.c intel_psr2_panic_force_full_update()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_psr2_panic_force_full_update(crtc_state: *const IntelCrtcState) {
    unsafe {
        let display = crate::intel_display_conversion_upstream::__drm_to_display(
            (*(*crtc_state).uapi.crtc).dev,
        );
        let crtc = (*crtc_state)
            .uapi
            .crtc
            .cast::<crate::intel_display_types_upstream::IntelCrtc>();
        let cpu_transcoder = (*crtc_state).cpu_transcoder;

        let mut val = man_trk_ctl_enable_bit_get(display);
        // SF partial frame enable has to be set even on full update.
        val |= man_trk_ctl_partial_frame_bit_get(display);
        val |= man_trk_ctl_continuos_full_frame(display);

        // Directly write the register.
        intel_de_write_fw(
            display,
            crate::intel_display_reg_defs_upstream::_MMIO_TRANS2(
                display,
                cpu_transcoder as u32,
                crate::intel_psr_regs_upstream::_PSR2_MAN_TRK_CTL_A,
            ),
            val,
        );

        if !(*crtc_state).enable_psr2_su_region_et {
            return;
        }
        intel_de_write_fw(
            display,
            crate::intel_display_reg_defs_upstream::_MMIO_PIPE(
                (*crtc).pipe as u32,
                crate::intel_psr_regs_upstream::_PIPE_SRCSZ_ERLY_TPT_A,
                crate::intel_psr_regs_upstream::_PIPE_SRCSZ_ERLY_TPT_B,
            ),
            0,
        );
    }
}
