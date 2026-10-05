// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
use std::{cell::RefCell, collections::BTreeMap};

use tk_intel_display::{
    Error, RegisterIo,
    display::{Pipe, ReadoutIo, intel_get_pipe_src_size, intel_get_transcoder_timings},
};
struct Model {
    words: BTreeMap<u32, u32>,
    reads: RefCell<Vec<u32>>,
    powered: bool,
}
impl RegisterIo for Model {
    fn read32(&self, offset: u32) -> Result<u32, Error> {
        self.reads.borrow_mut().push(offset);
        self.words
            .get(&offset)
            .copied()
            .ok_or(Error::Unavailable(offset))
    }
    fn write32(&self, _: u32, _: u32) -> Result<(), Error> {
        panic!("readout must never write")
    }
}
impl ReadoutIo for Model {
    fn pipe_powered(&self, _: Pipe) -> bool {
        self.powered
    }
}
fn model(pipe: Pipe) -> Model {
    let pair = |a: u32, b: u32| (a - 1) | ((b - 1) << 16);
    let raw = [
        (0x60000, pair(1920, 2200)),
        (0x60004, pair(1920, 2200)),
        (0x60008, pair(2008, 2052)),
        (0x6000c, pair(1080, 1125)),
        (0x60010, pair(1082, 1125)),
        (0x60014, pair(1084, 1089)),
        (0x6007c, 3),
        (0x6001c, ((1920 - 1) << 16) | (1080 - 1)),
    ];
    Model {
        words: raw
            .into_iter()
            .map(|(r, v)| (pipe.transcoder_register(r), v))
            .collect(),
        reads: RefCell::new(Vec::new()),
        powered: true,
    }
}
#[test]
fn exact_read_order_and_latency_override_follow_display_13() {
    for pipe in [Pipe::A, Pipe::B, Pipe::C, Pipe::D] {
        let m = model(pipe);
        let t = intel_get_transcoder_timings(&m, pipe, false).unwrap();
        assert_eq!(
            (t.hdisplay, t.htotal, t.vdisplay, t.vtotal, t.vblank_start),
            (1920, 2200, 1080, 1125, 1083)
        );
        assert_eq!(
            *m.reads.borrow(),
            [
                0x60000, 0x60004, 0x60008, 0x6000c, 0x60010, 0x60014, 0x6007c
            ]
            .map(|r| pipe.transcoder_register(r))
        );
        t.validate().unwrap();
        assert_eq!(intel_get_pipe_src_size(&m, pipe).unwrap(), (1920, 1080));
    }
}
#[test]
fn interlace_correction_precedes_latency_override() {
    let m = model(Pipe::A);
    let t = intel_get_transcoder_timings(&m, Pipe::A, true).unwrap();
    assert_eq!((t.vtotal, t.vblank_end, t.vblank_start), (1126, 1126, 1083));
}
#[test]
fn sleeping_domain_and_missing_read_do_not_become_zero_state() {
    let mut m = model(Pipe::A);
    m.powered = false;
    assert!(intel_get_transcoder_timings(&m, Pipe::A, false).is_err());
    assert!(intel_get_pipe_src_size(&m, Pipe::A).is_err());
    assert!(m.reads.borrow().is_empty());
    m.powered = true;
    m.words.remove(&0x60008);
    assert_eq!(
        intel_get_transcoder_timings(&m, Pipe::A, false),
        Err(Error::Unavailable(0x60008))
    );
    assert_eq!(*m.reads.borrow(), [0x60000, 0x60004, 0x60008]);
}
#[test]
fn impossible_latency_and_boundaries_refuse_admission() {
    let mut m = model(Pipe::A);
    m.words.insert(0x6007c, u32::MAX);
    assert_eq!(
        intel_get_transcoder_timings(&m, Pipe::A, false),
        Err(Error::Refused)
    );
    m.words.insert(0x6007c, 6);
    let t = intel_get_transcoder_timings(&m, Pipe::A, false).unwrap();
    assert!(t.validate().is_err());
    m.words.insert(0x6007c, 0);
    m.words.insert(0x60008, 0);
    assert!(
        intel_get_transcoder_timings(&m, Pipe::A, false)
            .unwrap()
            .validate()
            .is_err()
    );
}
