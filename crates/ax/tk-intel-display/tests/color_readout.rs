// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
mod color_support;
use color_support::Model;
use tk_intel_display::{Error, color::*, display::Pipe};
#[test]
fn disabled_color_touches_no_table_or_selector() {
    let io = Model::new(Pipe::A, 0, 0, 1);
    let state = intel_color_get_config(&io, Pipe::A, false, &mut [], &mut []).unwrap();
    assert!(state.bypassed());
    assert_eq!(io.trace.borrow().len(), 3);
    io.powered.set(false);
    assert_eq!(
        intel_color_get_config(&io, Pipe::A, false, &mut [], &mut []),
        Err(Error::Refused)
    );
    assert_eq!(io.trace.borrow().len(), 3);
}
#[test]
fn capacity_and_unsupported_precision_refuse_before_selector_mutation() {
    for mode in [1 << 31, 1 << 30, (1 << 30) | 1, (1 << 30) | 3] {
        let io = Model::new(Pipe::D, mode, 0, 42);
        let before = io.selectors.borrow().clone();
        assert_eq!(
            intel_color_get_config(&io, Pipe::D, false, &mut [], &mut []),
            Err(Error::Truncated)
        );
        assert_eq!(*io.selectors.borrow(), before);
        assert!(io.trace.borrow().iter().all(|v| v.0 == 0));
    }
    let io = Model::new(Pipe::A, (1 << 31) | (1 << 30) | 2, 0, 1);
    let mut pre = vec![LutEntry::default(); 129];
    let mut post = vec![LutEntry::default(); 1024];
    assert_eq!(
        intel_color_get_config(&io, Pipe::A, false, &mut pre, &mut post),
        Err(Error::Refused)
    );
    assert_eq!(io.trace.borrow().len(), 3);
}
#[test]
fn all_indexed_fault_prefixes_restore_before_image_or_quarantine() {
    for mode in [1 << 31, (1 << 30) | 1, (1 << 30) | 3] {
        let good = Model::new(Pipe::B, mode, 0, 0x76543210);
        let mut pre = vec![LutEntry::default(); 129];
        let mut post = vec![LutEntry::default(); 1024];
        intel_color_get_config(&good, Pipe::B, false, &mut pre, &mut post).unwrap();
        let total = good.accesses.get();
        for fault in 0..total {
            let io = Model::new(Pipe::B, mode, 0, 0x76543210);
            let before = io.selectors.borrow().clone();
            io.fault.set(Some(fault));
            let result = intel_color_get_config(&io, Pipe::B, false, &mut pre, &mut post);
            assert!(result.is_err(), "{mode:x} fault={fault}");
            assert_eq!(*io.selectors.borrow(), before, "{mode:x} fault={fault}");
            assert!(!io.locked.get());
            if fault == total - 2 || fault == total - 1 {
                assert!(matches!(result, Err(Error::RestoreFailed(_))));
            }
        }
    }
}
#[test]
fn csc_and_lut_reads_preserve_raw_state_and_mark_upstream_incompleteness() {
    for pipe in [Pipe::A, Pipe::B, Pipe::C, Pipe::D] {
        let io = Model::new(
            pipe,
            (1 << 31) | (1 << 30) | 3,
            (1 << 31) | (1 << 30),
            0x12345678,
        );
        let before = io.selectors.borrow().clone();
        let mut pre = vec![LutEntry::default(); 129];
        let untouched = LutEntry {
            red: 42,
            ..LutEntry::default()
        };
        let mut post = vec![untouched; 1024];
        let s = intel_color_get_config(&io, pipe, false, &mut pre, &mut post).unwrap();
        assert_eq!(s.degamma_entries, 129);
        assert_eq!(s.post_lut, PostLut::MultiSegmentSuperFineOnly);
        assert!(s.pipe_csc.is_some() && s.output_csc.is_some());
        assert!(!s.bypassed());
        assert_eq!(post[9], untouched);
        assert_eq!(*io.selectors.borrow(), before);
        assert_eq!(pre[0].red, pre[0].raw[0].min(65535) as u16);
    }
}
#[test]
fn palette_packing_extremes_and_c8_override_are_explicit() {
    assert_eq!(i9xx_lut_8_pack(0xffffff).red, 65535);
    assert_eq!(ilk_lut_10_pack(0x3fffffff).green, 65535);
    assert_eq!(glk_degamma_lut_pack(u32::MAX).blue, 65535);
    let v = ilk_lut_12p4_pack(u32::MAX, u32::MAX);
    assert_eq!((v.red, v.green, v.blue), (65535, 65535, 65535));
    let low = ilk_lut_12p4_pack((3 << 24) | (5 << 14) | (7 << 4), 0);
    assert_eq!((low.red, low.green, low.blue), (3, 5, 7));
    let io = Model::new(Pipe::A, 0, 0, 7);
    let mut post = vec![LutEntry::default(); 256];
    let s = intel_color_get_config(&io, Pipe::A, true, &mut [], &mut post).unwrap();
    assert_eq!(s.post_lut, PostLut::Legacy8);
    assert!(!s.bypassed());
}
