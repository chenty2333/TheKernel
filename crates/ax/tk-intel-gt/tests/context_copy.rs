// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
use tk_intel_gt::{Error, bcs, lrc, ppgtt};
#[test]
fn private_ppgtt_encodes_exact_n305_flags_and_refuses_alias_overflow_before_changes() {
    assert_eq!(ppgtt::pde(0x800000), Ok(0x80001b));
    assert_eq!(ppgtt::pte(0x800000, 3, false), Ok(0x800019));
    assert_eq!(ppgtt::pte(0x800000, 3, true), Ok(0x80001b));
    let mut table = [0; 512];
    ppgtt::leaf(&mut table, 0x900000, 3).unwrap();
    let before = table;
    assert_eq!(
        ppgtt::map(&mut table, 0x1ff000, &[0xa00000, 0xb00000], 3, true),
        Err(Error::Refused)
    );
    assert_eq!(table, before);
    assert_eq!(
        ppgtt::map(&mut table, 0x10000, &[0xa00000, 1 << 39], 3, true),
        Err(Error::Refused)
    );
    assert_eq!(table, before);
    ppgtt::map(&mut table, 0x10000, &[0xa00000, 0xb00000], 3, true).unwrap();
    assert_eq!(table[16], 0xa0001b);
    assert_eq!(table[18] & 2, 0);
    assert!(ppgtt::pte(0, 0, true).is_err());
}
#[test]
fn lrc_context_retains_source_slots_wa_storage_and_unique_bcs_descriptor() {
    let (mut regs, mut indirect, mut per) = ([0; 1024], [0; 1024], [0; 1024]);
    let desc = lrc::build(
        &mut regs,
        &mut indirect,
        &mut per,
        0x200000,
        0x300000,
        128,
        0x123456000,
    )
    .unwrap();
    assert_eq!(desc, 0x200000000020010d | (1 << 37));
    assert_eq!(regs[3], 0x90009);
    assert_eq!(regs[7], 128);
    assert_eq!(
        (regs[48], regs[49], regs[50], regs[51]),
        (0x22274, 1, 0x22270, 0x23456000)
    );
    assert_eq!(regs[19], 0x203005);
    assert_eq!(regs[21], 0x202002);
    assert_eq!(regs[23], 0x340);
    assert_eq!(regs[0x61], 1 << 24);
    assert_eq!(per[0], 0x05000000);
    assert_eq!(indirect[15], 0x4248);
    assert_eq!(indirect[512], 0x10400002);
    assert_eq!(indirect[513], 0x202ff8);
    assert!(indirect[22..32].iter().all(|&v| v == 0));
    assert!(
        lrc::build(
            &mut regs,
            &mut indirect,
            &mut per,
            !4095,
            0x300000,
            128,
            0x800000
        )
        .is_err()
    );
}
#[test]
fn copy_checks_both_ranges_pitch_and_overlap_and_breadcrumb_is_a_separate_stalling_flush() {
    let c = bcs::Copy {
        source: 0x11000,
        destination: 0x21000,
        source_bytes: 16384,
        destination_bytes: 16384,
        width: 64,
        height: 64,
        pitch: 256,
    };
    let words = bcs::batch(c).unwrap();
    assert_eq!(&words[..4], [0x11000001, 0x22204, 0x606, 0x50800008]);
    assert_eq!(
        (words[4], words[6], words[7], words[11]),
        (0x03000100, 0x00400040, 0x21000, 0x11000)
    );
    for bad in [
        bcs::Copy {
            destination: 0x11004,
            ..c
        },
        bcs::Copy {
            source_bytes: 16383,
            ..c
        },
        bcs::Copy { pitch: 255, ..c },
        bcs::Copy {
            source: u64::MAX - 3,
            ..c
        },
    ] {
        assert!(bcs::batch(bad).is_err());
    }
    let mut ring = [0; 32];
    assert_eq!(bcs::ring(&mut ring, 0x30000, 0x200000, 1), Ok(30));
    assert_eq!(
        &ring[18..26],
        [0x13000002, 0, 0, 0, 0x13004002, 0x2000d4, 0, 1]
    );
    assert_eq!(ring[28], 0x02800000); // source MI_ARB_CHECK preemption point.
    assert!(ring[29..].iter().all(|&n| n == 0));
    assert_eq!(
        bcs::ring(&mut ring, 0x30000, 0x200000, 0),
        Err(Error::Refused)
    );
}

#[test]
fn copy_shared_policy_steers_only_live_dss_and_restores_selector_on_landed_failures() {
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
    };

    use tk_intel_gt::GtIo;
    struct Io {
        values: RefCell<BTreeMap<u32, u32>>,
        writes: Cell<usize>,
        fail: Cell<usize>,
    }
    impl GtIo for Io {
        fn read(&self, r: u32) -> Result<u32, Error> {
            self.values
                .borrow()
                .get(&r)
                .copied()
                .ok_or(Error::Unavailable(r))
        }
        fn write(&self, r: u32, v: u32) -> Result<(), Error> {
            self.values.borrow_mut().insert(r, v);
            self.writes.set(self.writes.get() + 1);
            if self.writes.get() == self.fail.get() {
                Err(Error::Unavailable(r))
            } else {
                Ok(())
            }
        }
        fn now_us(&self) -> u64 {
            0
        }
        fn delay_us(&self, _: u32) {}
    }
    let io = || Io {
        values: RefCell::new(BTreeMap::from([
            (0x9138, 1),
            (0x913c, 0b1100),
            (0xfdc, 0x83001234),
            (0x9550, 0x55),
            (0x9424, 3),
            (0x480c, 9),
            (0x400c, 9),
            (0xb024, 0x77778888),
        ])),
        writes: Cell::new(0),
        fail: Cell::new(usize::MAX),
    };
    let normal = io();
    bcs::prepare(&normal).unwrap();
    assert_eq!(normal.read(0xfdc), Ok(0x83001234));
    assert_eq!(normal.read(0x9550), Ok(0x255));
    assert_eq!(normal.read(0x9424), Ok(1));
    assert_eq!(normal.read(0xb024), Ok(0x108888));
    for prefix in 1..=normal.writes.get() {
        let fault = io();
        fault.fail.set(prefix);
        assert!(bcs::prepare(&fault).is_err());
        assert_eq!(fault.read(0xfdc), Ok(0x83001234));
    }
    for (reg, value) in [(0x9138, 0), (0x913c, 0), (0x913c, 1 << 6), (0xfdc, 0)] {
        let bad = io();
        bad.values.borrow_mut().insert(reg, value);
        assert!(bcs::prepare(&bad).is_err());
        assert_eq!(bad.writes.get(), 0);
    }
}

#[test]
fn user_copy_plan_refuses_foreign_opcodes_tiling_addresses_and_out_of_object_rows() {
    let c = bcs::Copy {
        source: 0x10000,
        destination: 0x20000,
        source_bytes: 16384,
        destination_bytes: 16384,
        width: 64,
        height: 64,
        pitch: 256,
    };
    let batch = bcs::batch(c).unwrap();
    let words: [u32; 11] = batch[3..].try_into().unwrap();
    assert_eq!(bcs::decode_copy(&words, 16384, 16384), Ok(c));
    for (i, mask) in [
        (0, 1 << 12),
        (1, 1 << 29),
        (2, 1),
        (5, 1),
        (6, 1),
        (7, 1 << 16),
        (9, 1),
        (10, 1),
    ] {
        let mut bad = words;
        bad[i] ^= mask;
        assert!(bcs::decode_copy(&bad, 16384, 16384).is_err(), "word{i}");
    }
    assert!(bcs::decode_copy(&words, 16383, 16384).is_err());
    assert!(bcs::decode_copy(&words, 16384, 16383).is_err());
    assert!(bcs::decode_copy(&words, 65537, 16384).is_err());
}

#[test]
fn valid_image_update_preserves_gpu_generated_stream_and_unrelated_register_values() {
    for render in [false, true] {
        let mut regs = core::array::from_fn(|i| 0xdead0000 | (i as u32));
        let mut indirect = [0; 1024];
        let mut per = [0; 1024];
        let before = regs;
        if render {
            tk_intel_gt::rcs::restore_context(
                &mut regs,
                &mut indirect,
                &mut per,
                0x40000,
                0x50000,
                312,
                0x800000,
            )
            .unwrap();
        } else {
            lrc::restore_context(
                &mut regs,
                &mut indirect,
                &mut per,
                0x40000,
                0x50000,
                120,
                0x800000,
            )
            .unwrap();
        }
        for i in 0..1024 {
            if ![3usize, 5, 7, 9, 11, 19, 21, 23, 49, 51, 0x61].contains(&i)
                && !(render && i == 0x43)
            {
                assert_eq!(regs[i], before[i], "opaque image word {i}");
            }
        }
        assert_eq!(regs[35], before[35]);
        assert_eq!(regs[5], 0);
        assert_eq!(regs[9], 0x50000);
        assert_eq!(regs[11], 1);
        assert_eq!(regs[51], 0x800000);
        assert_eq!(regs[3], (before[3] & !1) | (1 << 16));
        assert_eq!(regs[0x61], (before[0x61] & !(1 << 8)) | (1 << 24));
    }
}
