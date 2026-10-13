// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! ADL-P DDI signal-level tables from Linux v7.2.3 `intel_ddi_buf_trans.c`.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::mem::ManuallyDrop;

use crate::{
    intel_ddi_buf_trans_types_upstream::{
        IclDdiBufTrans, IntelDdiBufTrans, IntelDdiBufTransEntry, TglDklPhyDdiBufTrans,
    },
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::{PORT_A, PORT_B},
    intel_display_types_upstream::{
        INTEL_OUTPUT_EDP, INTEL_OUTPUT_HDMI, IntelCrtcState, IntelEncoder,
    },
    intel_display_wa_upstream::DISPLAY_VER,
};

#[repr(transparent)]
struct ReadOnlyTable(IntelDdiBufTrans);

// Linux tables are `static const`; the imported table wrapper uses mutable raw
// pointers for ABI layout, but no call site writes through either pointer.
unsafe impl Sync for ReadOnlyTable {}

macro_rules! icl_entry {
    ($dw2:expr, $dw7:expr, $cursor:expr, $post2:expr, $post1:expr) => {
        IntelDdiBufTransEntry {
            icl: ManuallyDrop::new(IclDdiBufTrans {
                dw2_swing_sel: $dw2,
                dw7_n_scalar: $dw7,
                dw4_cursor_coeff: $cursor,
                dw4_post_cursor_2: $post2,
                dw4_post_cursor_1: $post1,
            }),
        }
    };
}

macro_rules! dkl_entry {
    ($vswing:expr, $preshoot:expr, $de_emphasis:expr) => {
        IntelDdiBufTransEntry {
            dkl: ManuallyDrop::new(TglDklPhyDdiBufTrans {
                vswing: $vswing,
                preshoot: $preshoot,
                de_emphasis: $de_emphasis,
            }),
        }
    };
}

static ICL_COMBO_HDMI_ENTRIES: [IntelDdiBufTransEntry; 7] = [
    icl_entry!(0x0a, 0x60, 0x3f, 0x00, 0x00),
    icl_entry!(0x0b, 0x73, 0x36, 0x00, 0x09),
    icl_entry!(0x06, 0x7f, 0x31, 0x00, 0x0e),
    icl_entry!(0x0b, 0x73, 0x3f, 0x00, 0x00),
    icl_entry!(0x06, 0x7f, 0x37, 0x00, 0x08),
    icl_entry!(0x06, 0x7f, 0x3f, 0x00, 0x00),
    icl_entry!(0x06, 0x7f, 0x35, 0x00, 0x0a),
];

static TGL_DKL_HDMI_ENTRIES: [IntelDdiBufTransEntry; 10] = [
    dkl_entry!(0x7, 0, 0),
    dkl_entry!(0x6, 0, 0),
    dkl_entry!(0x4, 0, 0),
    dkl_entry!(0x2, 0, 0),
    dkl_entry!(0x0, 0, 0),
    dkl_entry!(0x0, 0, 0x5),
    dkl_entry!(0x0, 0, 0x6),
    dkl_entry!(0x0, 0, 0x7),
    dkl_entry!(0x0, 0, 0x8),
    dkl_entry!(0x0, 0, 0xa),
];

static ADLP_COMBO_DP_HBR_ENTRIES: [IntelDdiBufTransEntry; 10] = [
    icl_entry!(0x0a, 0x35, 0x3f, 0x00, 0x00),
    icl_entry!(0x0a, 0x4f, 0x37, 0x00, 0x08),
    icl_entry!(0x0c, 0x71, 0x31, 0x00, 0x0e),
    icl_entry!(0x06, 0x7f, 0x2c, 0x00, 0x13),
    icl_entry!(0x0a, 0x4c, 0x3f, 0x00, 0x00),
    icl_entry!(0x0c, 0x73, 0x34, 0x00, 0x0b),
    icl_entry!(0x06, 0x7f, 0x2f, 0x00, 0x10),
    icl_entry!(0x0c, 0x7c, 0x3c, 0x00, 0x03),
    icl_entry!(0x06, 0x7f, 0x35, 0x00, 0x0a),
    icl_entry!(0x06, 0x7f, 0x3f, 0x00, 0x00),
];

static ADLP_COMBO_DP_HBR2_HBR3_ENTRIES: [IntelDdiBufTransEntry; 10] = [
    icl_entry!(0x0a, 0x35, 0x3f, 0x00, 0x00),
    icl_entry!(0x0a, 0x4f, 0x37, 0x00, 0x08),
    icl_entry!(0x0c, 0x71, 0x30, 0x00, 0x0f),
    icl_entry!(0x06, 0x7f, 0x2b, 0x00, 0x14),
    icl_entry!(0x0a, 0x4c, 0x3f, 0x00, 0x00),
    icl_entry!(0x0c, 0x73, 0x34, 0x00, 0x0b),
    icl_entry!(0x06, 0x7f, 0x30, 0x00, 0x0f),
    icl_entry!(0x0c, 0x63, 0x3f, 0x00, 0x00),
    icl_entry!(0x06, 0x7f, 0x38, 0x00, 0x07),
    icl_entry!(0x06, 0x7f, 0x3f, 0x00, 0x00),
];

static ADLP_DKL_DP_HBR_ENTRIES: [IntelDdiBufTransEntry; 10] = [
    dkl_entry!(0x7, 0x0, 0x01),
    dkl_entry!(0x5, 0x0, 0x06),
    dkl_entry!(0x2, 0x0, 0x0b),
    dkl_entry!(0x0, 0x0, 0x17),
    dkl_entry!(0x5, 0x0, 0x00),
    dkl_entry!(0x2, 0x0, 0x08),
    dkl_entry!(0x0, 0x0, 0x14),
    dkl_entry!(0x2, 0x0, 0x00),
    dkl_entry!(0x0, 0x0, 0x0b),
    dkl_entry!(0x0, 0x0, 0x00),
];

static ADLP_DKL_DP_HBR2_HBR3_ENTRIES: [IntelDdiBufTransEntry; 10] = [
    dkl_entry!(0x7, 0x0, 0x00),
    dkl_entry!(0x5, 0x0, 0x04),
    dkl_entry!(0x2, 0x0, 0x0a),
    dkl_entry!(0x0, 0x0, 0x18),
    dkl_entry!(0x5, 0x0, 0x00),
    dkl_entry!(0x2, 0x0, 0x06),
    dkl_entry!(0x0, 0x0, 0x14),
    dkl_entry!(0x2, 0x0, 0x00),
    dkl_entry!(0x0, 0x0, 0x09),
    dkl_entry!(0x0, 0x0, 0x00),
];

static ICL_COMBO_HDMI: ReadOnlyTable = ReadOnlyTable(IntelDdiBufTrans {
    entries: ICL_COMBO_HDMI_ENTRIES.as_ptr().cast_mut(),
    num_entries: ICL_COMBO_HDMI_ENTRIES.len() as u8,
    hdmi_default_entry: (ICL_COMBO_HDMI_ENTRIES.len() - 1) as u8,
    _pad_10: [0; 6],
});
static TGL_DKL_HDMI: ReadOnlyTable = ReadOnlyTable(IntelDdiBufTrans {
    entries: TGL_DKL_HDMI_ENTRIES.as_ptr().cast_mut(),
    num_entries: TGL_DKL_HDMI_ENTRIES.len() as u8,
    hdmi_default_entry: (TGL_DKL_HDMI_ENTRIES.len() - 1) as u8,
    _pad_10: [0; 6],
});
static ADLP_COMBO_DP_HBR: ReadOnlyTable = ReadOnlyTable(IntelDdiBufTrans {
    entries: ADLP_COMBO_DP_HBR_ENTRIES.as_ptr().cast_mut(),
    num_entries: ADLP_COMBO_DP_HBR_ENTRIES.len() as u8,
    hdmi_default_entry: 0,
    _pad_10: [0; 6],
});
static ADLP_COMBO_DP_HBR2_HBR3: ReadOnlyTable = ReadOnlyTable(IntelDdiBufTrans {
    entries: ADLP_COMBO_DP_HBR2_HBR3_ENTRIES.as_ptr().cast_mut(),
    num_entries: ADLP_COMBO_DP_HBR2_HBR3_ENTRIES.len() as u8,
    hdmi_default_entry: 0,
    _pad_10: [0; 6],
});
static ADLP_DKL_DP_HBR: ReadOnlyTable = ReadOnlyTable(IntelDdiBufTrans {
    entries: ADLP_DKL_DP_HBR_ENTRIES.as_ptr().cast_mut(),
    num_entries: ADLP_DKL_DP_HBR_ENTRIES.len() as u8,
    hdmi_default_entry: 0,
    _pad_10: [0; 6],
});
static ADLP_DKL_DP_HBR2_HBR3: ReadOnlyTable = ReadOnlyTable(IntelDdiBufTrans {
    entries: ADLP_DKL_DP_HBR2_HBR3_ENTRIES.as_ptr().cast_mut(),
    num_entries: ADLP_DKL_DP_HBR2_HBR3_ENTRIES.len() as u8,
    hdmi_default_entry: 0,
    _pad_10: [0; 6],
});

fn select_table(
    output_types: u32,
    clock: i32,
    hdmi: &'static ReadOnlyTable,
    dp_hbr: &'static ReadOnlyTable,
    dp_hbr2_hbr3: &'static ReadOnlyTable,
) -> Option<&'static ReadOnlyTable> {
    if output_types & (1 << INTEL_OUTPUT_HDMI) != 0 {
        Some(hdmi)
    } else if output_types & (1 << INTEL_OUTPUT_EDP) != 0 {
        None
    } else if clock > 270_000 {
        Some(dp_hbr2_hbr3)
    } else {
        Some(dp_hbr)
    }
}

#[inline]
fn table_ptr(table: &'static ReadOnlyTable) -> *mut IntelDdiBufTrans {
    core::ptr::addr_of!(table.0).cast_mut()
}

unsafe extern "C" fn adlp_combo_get_buf_trans(
    _encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
    num_entries: *mut i32,
) -> *mut IntelDdiBufTrans {
    if num_entries.is_null() {
        return core::ptr::null_mut();
    }
    if crtc_state.is_null() {
        unsafe { *num_entries = 0 };
        return core::ptr::null_mut();
    }
    let table = select_table(
        unsafe { (*crtc_state).output_types },
        unsafe { (*crtc_state).port_clock },
        &ICL_COMBO_HDMI,
        &ADLP_COMBO_DP_HBR,
        &ADLP_COMBO_DP_HBR2_HBR3,
    );
    let Some(table) = table else {
        unsafe { *num_entries = 0 };
        return core::ptr::null_mut();
    };
    unsafe { *num_entries = i32::from(table.0.num_entries) };
    table_ptr(table)
}

unsafe extern "C" fn adlp_dkl_get_buf_trans(
    _encoder: *mut IntelEncoder,
    crtc_state: *mut IntelCrtcState,
    num_entries: *mut i32,
) -> *mut IntelDdiBufTrans {
    if num_entries.is_null() {
        return core::ptr::null_mut();
    }
    if crtc_state.is_null() {
        unsafe { *num_entries = 0 };
        return core::ptr::null_mut();
    }
    let table = select_table(
        unsafe { (*crtc_state).output_types },
        unsafe { (*crtc_state).port_clock },
        &TGL_DKL_HDMI,
        &ADLP_DKL_DP_HBR,
        &ADLP_DKL_DP_HBR2_HBR3,
    );
    let Some(table) = table else {
        unsafe { *num_entries = 0 };
        return core::ptr::null_mut();
    };
    unsafe { *num_entries = i32::from(table.0.num_entries) };
    table_ptr(table)
}

/// ADL-P buffer table selection for combo A/B and DKL TC ports.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddi_buf_trans_init(encoder: *mut IntelEncoder) {
    if encoder.is_null() {
        return;
    }
    let display = unsafe {
        let dev = (*encoder).base.dev;
        if dev.is_null() || (*dev).dev_private.is_null() {
            core::ptr::null_mut()
        } else {
            let i915 = (*dev)
                .dev_private
                .cast::<crate::linux_i915_private::DrmI915Private>();
            (*i915).display.cast::<IntelDisplay>()
        }
    };
    if display.is_null() || !unsafe { DISPLAY_VER(display) == 13 } {
        unsafe { (*encoder).get_buf_trans = None };
        return;
    }
    let is_adlp = unsafe {
        let bits = core::ptr::addr_of!((*display).platform).cast::<u8>();
        *bits.add(52 / 8) & (1 << (52 % 8)) != 0
    };
    if !is_adlp {
        unsafe { (*encoder).get_buf_trans = None };
        return;
    }
    unsafe {
        (*encoder).get_buf_trans =
            Some(if (*encoder).port == PORT_A || (*encoder).port == PORT_B {
                adlp_combo_get_buf_trans
            } else {
                adlp_dkl_get_buf_trans
            });
    }
}

/// HOBL tables are eDP-only and deliberately not selected by this build.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn is_hobl_buf_trans(_table: *const IntelDdiBufTrans) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adlp_buffer_tables_keep_exact_computed_levels() {
        assert_eq!(ICL_COMBO_HDMI.0.num_entries, 7);
        assert_eq!(ICL_COMBO_HDMI.0.hdmi_default_entry, 6);
        assert_eq!(TGL_DKL_HDMI.0.num_entries, 10);
        assert_eq!(ADLP_COMBO_DP_HBR.0.num_entries, 10);
        assert_eq!(ADLP_COMBO_DP_HBR2_HBR3.0.num_entries, 10);
        assert_eq!(ADLP_DKL_DP_HBR.0.num_entries, 10);
        assert_eq!(ADLP_DKL_DP_HBR2_HBR3.0.num_entries, 10);

        unsafe {
            let combo_hbr = &*ADLP_COMBO_DP_HBR.0.entries;
            assert_eq!(combo_hbr.icl.dw2_swing_sel, 0x0a);
            assert_eq!(combo_hbr.icl.dw7_n_scalar, 0x35);
            let combo_hbr3 = &*ADLP_COMBO_DP_HBR2_HBR3.0.entries.add(2);
            assert_eq!(combo_hbr3.icl.dw4_cursor_coeff, 0x30);
            assert_eq!(combo_hbr3.icl.dw4_post_cursor_1, 0x0f);
            let dkl_hbr = &*ADLP_DKL_DP_HBR.0.entries.add(9);
            assert_eq!(dkl_hbr.dkl.vswing, 0x0);
            assert_eq!(dkl_hbr.dkl.de_emphasis, 0x0);
            let dkl_hbr3 = &*ADLP_DKL_DP_HBR2_HBR3.0.entries.add(3);
            assert_eq!(dkl_hbr3.dkl.de_emphasis, 0x18);
        }
    }

    #[test]
    fn buffer_table_clock_threshold_matches_upstream_hbr_selection() {
        let hdmi = select_table(
            1 << INTEL_OUTPUT_HDMI,
            600_000,
            &ICL_COMBO_HDMI,
            &ADLP_COMBO_DP_HBR,
            &ADLP_COMBO_DP_HBR2_HBR3,
        );
        let low = select_table(
            0,
            270_000,
            &ICL_COMBO_HDMI,
            &ADLP_COMBO_DP_HBR,
            &ADLP_COMBO_DP_HBR2_HBR3,
        );
        let high = select_table(
            0,
            270_001,
            &ICL_COMBO_HDMI,
            &ADLP_COMBO_DP_HBR,
            &ADLP_COMBO_DP_HBR2_HBR3,
        );
        let edp = select_table(
            1 << INTEL_OUTPUT_EDP,
            600_000,
            &ICL_COMBO_HDMI,
            &ADLP_COMBO_DP_HBR,
            &ADLP_COMBO_DP_HBR2_HBR3,
        );
        assert!(core::ptr::eq(hdmi.unwrap(), &ICL_COMBO_HDMI));
        assert!(core::ptr::eq(low.unwrap(), &ADLP_COMBO_DP_HBR));
        assert!(core::ptr::eq(high.unwrap(), &ADLP_COMBO_DP_HBR2_HBR3));
        assert!(edp.is_none());
    }
}
