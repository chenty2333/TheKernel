// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
use std::cell::RefCell;

use tk_intel_display::{
    Error, RegisterIo,
    display::{Pipe, ReadoutIo},
    universal_plane::*,
};
struct Model {
    powered: bool,
    words: [u32; 6],
    reads: RefCell<Vec<u32>>,
    missing: Option<u32>,
}
impl Model {
    fn linear() -> Self {
        Self {
            powered: true,
            words: [
                (1 << 31) | (4 << 24),
                1 << 13,
                0x200000,
                0,
                (1079 << 16) | 1919,
                120,
            ],
            reads: RefCell::new(Vec::new()),
            missing: None,
        }
    }
    fn get(&self) -> Result<Option<InitialPlaneConfig>, Error> {
        skl_get_initial_plane_config(self, Pipe::A, Plane::PRIMARY)
    }
}
impl RegisterIo for Model {
    fn read32(&self, r: u32) -> Result<u32, Error> {
        self.reads.borrow_mut().push(r);
        if self.missing == Some(r) {
            return Err(Error::Unavailable(r));
        }
        [0x70180, 0x701cc, 0x7019c, 0x701a4, 0x70190, 0x70188]
            .iter()
            .position(|v| *v == r)
            .map(|i| self.words[i])
            .ok_or(Error::Unavailable(r))
    }
    fn write32(&self, _: u32, _: u32) -> Result<(), Error> {
        panic!("readout wrote")
    }
}
impl ReadoutIo for Model {
    fn pipe_powered(&self, _: Pipe) -> bool {
        self.powered
    }
}
#[test]
fn linear_1080p_readout_preserves_order_and_raw_evidence() {
    let m = Model::linear();
    let s = m.get().unwrap().unwrap();
    assert!(s.native_linear_xrgb());
    assert_eq!(
        (s.width, s.height, s.pitch, s.main_size),
        (1920, 1080, 7680, 8294400)
    );
    assert_eq!(s.surface(), 0x200000);
    assert_eq!(s.fourcc, u32::from_le_bytes(*b"XR24"));
    assert_eq!(
        *m.reads.borrow(),
        [
            0x70180, 0x70180, 0x701cc, 0x7019c, 0x701a4, 0x70190, 0x70188
        ]
    );
    for n in 0..5 {
        let p = Plane::new(n).unwrap();
        assert_eq!(p.register(Pipe::D, 0x701cc), 0x731cc + u32::from(n) * 0x100);
    }
    assert!(Plane::new(5).is_err());
}
#[test]
fn dark_disabled_missing_and_rotated_planes_never_become_native() {
    let mut m = Model::linear();
    m.powered = false;
    assert_eq!(m.get(), Ok(None));
    assert!(m.reads.borrow().is_empty());
    m.powered = true;
    m.words[0] = 0;
    assert_eq!(m.get(), Ok(None));
    assert_eq!(m.reads.borrow().len(), 1);
    for r in [0x70180, 0x701cc, 0x7019c, 0x701a4, 0x70190, 0x70188] {
        let mut m = Model::linear();
        m.missing = Some(r);
        assert_eq!(m.get(), Err(Error::Unavailable(r)));
    }
    for rot in [1, 3] {
        let mut m = Model::linear();
        m.words[0] |= rot;
        assert_eq!(m.get(), Err(Error::Refused));
    }
}
#[test]
fn tiling5_is_yf_not_dg2_4tile_and_ccs_does_not_authorize_ownership() {
    for (bits, modifier, pitch, height) in [
        (1 << 10, Modifier::X, 61440, 1080),
        (4 << 10, Modifier::Y, 15360, 1088),
        (5 << 10, Modifier::Yf, 15360, 1088),
        ((4 << 10) | (1 << 15), Modifier::YGen12RcCcs, 15360, 1088),
        ((4 << 10) | (1 << 4), Modifier::YGen12McCcs, 15360, 1088),
        ((5 << 10) | (1 << 15), Modifier::YfCcs, 15360, 1088),
    ] {
        let mut m = Model::linear();
        m.words[0] |= bits;
        let s = m.get().unwrap().unwrap();
        assert_eq!(s.modifier, modifier);
        assert_eq!(s.pitch, pitch);
        assert_eq!(s.main_size, u64::from(pitch) * height);
        assert!(!s.native_linear_xrgb());
        assert_eq!(modifier.drm() >> 56, 1);
    }
    for tile in [2, 3, 6, 7] {
        let mut m = Model::linear();
        m.words[0] |= tile << 10;
        assert_eq!(m.get(), Err(Error::Refused));
    }
}
#[test]
fn unsupported_or_color_transformed_layouts_are_not_silently_equivalent() {
    for bits in [1 << 20, 1 << 21, 1 << 15, 1 << 9, 1 << 8, 1 << 4, 2] {
        let mut m = Model::linear();
        m.words[0] |= bits;
        assert!(!m.get().unwrap().unwrap().native_linear_xrgb());
    }
    for bits in [1 << 21, 1 << 20, 1 << 17, 1 << 15, 1 << 14, 1 << 4] {
        let mut m = Model::linear();
        m.words[1] |= bits;
        assert!(!m.get().unwrap().unwrap().native_linear_xrgb());
    }
    for format in 0..32 {
        let mut m = Model::linear();
        m.words[0] = (1 << 31) | (format << 23);
        let s = m.get().unwrap().unwrap();
        assert_eq!(s.native_linear_xrgb(), format == 8);
    }
    let mut m = Model::linear();
    m.words[1] = 0;
    assert!(!m.get().unwrap().unwrap().native_linear_xrgb());
    let mut m = Model::linear();
    m.words[3] = 1;
    assert!(!m.get().unwrap().unwrap().native_linear_xrgb());
    let mut m = Model::linear();
    m.words[2] |= 4;
    assert!(!m.get().unwrap().unwrap().native_linear_xrgb());
    let mut m = Model::linear();
    m.words[5] = 1;
    assert!(!m.get().unwrap().unwrap().native_linear_xrgb());
}
