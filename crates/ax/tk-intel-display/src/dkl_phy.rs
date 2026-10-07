// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_dkl_phy.c:
// dkl_phy_set_hip_idx, intel_dkl_phy_{read,write,rmw,posting_read}.
// intel_dkl_phy_regs.h: aperture/bank/index layout.
// Copyright © 2022 Intel Corporation. MIT permission text: ../LICENSE-MIT.
// Only the four ADL-P/N ports are admitted. Platform mapping/power stay outside.
use crate::{Error, RegisterIo};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcPort {
    Tc1,
    Tc2,
    Tc3,
    Tc4,
}
impl TcPort {
    pub const fn index(self) -> u32 {
        self as u32
    }
    pub const fn pll_enable(self) -> u32 {
        // ADLP_PORTTC_PLL_ENABLE, NOT MG_PLL_ENABLE (TGL/ICL).
        0x46038 + self.index() * 8
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DklRegister {
    port: TcPort,
    internal: u32,
}
impl DklRegister {
    pub fn new(port: TcPort, internal: u32) -> Result<Self, Error> {
        if internal >= 0x10000 || internal & 3 != 0 {
            return Err(Error::Refused);
        }
        Ok(Self { port, internal })
    }
    pub const fn aperture(self) -> u32 {
        0x168000 + self.port.index() * 0x1000 + (self.internal & 0xfff)
    }
    pub const fn selector(self) -> u32 {
        0x1010a0 // all four ADL-P ports use HIP_INDEX_REG0
    }
    pub const fn index_value(self) -> u32 {
        ((self.internal >> 12) & 0xf) << (8 * self.port.index())
    }
}

/// Backend must serialize this closure against ALL HIP access, including IRQs,
/// and keep the selected PHY's power domain pinned for its entire lifetime.
/// A fake backend may implement the same contract with an exclusive model lock.
pub trait DklIo: RegisterIo {
    fn with_dkl_lock<T>(&self, operation: impl FnOnce() -> Result<T, Error>) -> Result<T, Error>;
}

pub fn intel_dkl_phy_read(io: &impl DklIo, reg: DklRegister) -> Result<u32, Error> {
    io.with_dkl_lock(|| {
        io.write32(reg.selector(), reg.index_value())?;
        io.read32(reg.aperture())
    })
}
pub fn intel_dkl_phy_write(io: &impl DklIo, reg: DklRegister, value: u32) -> Result<(), Error> {
    io.with_dkl_lock(|| {
        io.write32(reg.selector(), reg.index_value())?;
        io.write32(reg.aperture(), value)
    })
}
pub fn intel_dkl_phy_rmw(
    io: &impl DklIo,
    reg: DklRegister,
    clear: u32,
    set: u32,
) -> Result<(), Error> {
    io.with_dkl_lock(|| {
        io.write32(reg.selector(), reg.index_value())?;
        let old = io.read32(reg.aperture())?;
        let new = (old & !clear) | set;
        // intel_de_rmw ultimately uses intel_uncore_rmw: always stores,
        // despite the stale DKL helper comment claiming unchanged-write elision.
        io.write32(reg.aperture(), new)?;
        Ok(())
    })
}
pub fn intel_dkl_phy_posting_read(io: &impl DklIo, reg: DklRegister) -> Result<(), Error> {
    intel_dkl_phy_read(io, reg).map(|_| ())
}

/// A locked view can only be constructed inside `with_preserved_selector`.
/// This readout-only safety wrapper differs from driver-owned i915: it restores
/// firmware's whole shared HIP selector on success AND every fallible prefix.
/// It never restores a PHY register, because it never writes one.
pub struct SnapshotAccess<'a, I> {
    io: &'a I,
    port: TcPort,
}
impl<I: RegisterIo> SnapshotAccess<'_, I> {
    pub fn read(&self, internal: u32) -> Result<u32, Error> {
        let reg = DklRegister::new(self.port, internal)?;
        self.io.write32(reg.selector(), reg.index_value())?;
        self.io.read32(reg.aperture())
    }
}
pub fn with_preserved_selector<I: DklIo, T>(
    io: &I,
    port: TcPort,
    operation: impl FnOnce(&SnapshotAccess<'_, I>) -> Result<T, Error>,
) -> Result<T, Error> {
    io.with_dkl_lock(|| {
        let before = io.read32(0x1010a0)?;
        let result = operation(&SnapshotAccess { io, port });
        // A failed MMIO write may have landed: never skip restoration on error.
        if io.write32(0x1010a0, before).is_err() || io.read32(0x1010a0).ok() != Some(before) {
            return Err(Error::RestoreFailed(0x1010a0));
        }
        result
    })
}
