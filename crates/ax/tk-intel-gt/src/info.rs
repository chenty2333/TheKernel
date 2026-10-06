// SPDX-License-Identifier: MIT
// Linux7.2.3 gt/intel_sseu.c::gen12_sseu_info_init/gen11_compute_sseu_info
// Copyright ©2019 Intel Corporation; gt/intel_gt_clock_utils.c::
// gen11_read_clock_frequency/read_reference_ts_freq/gen11_get_crystal_clock_freq
// Copyright ©2020 Intel Corporation. Full MIT grant: ../LICENSE-MIT.
// Only N305 Gen12.0. Fuses/clock are read, never invented from the product name.
use crate::{Error, GtIo};
#[derive(Clone, Copy, Debug)]
pub struct Topology {
    pub dss: u8,
    pub eus: u16,
}
impl Topology {
    pub fn read(io: &impl GtIo) -> Result<Self, Error> {
        if io.read(0x9138)? & 255 != 1 {
            return Err(Error::Refused);
        }
        let dss = io.read(0x913c)?;
        if dss == 0 || dss & !63 != 0 {
            return Err(Error::Refused);
        }
        let pairs = !(io.read(0x9134)? as u8);
        let mut eus = 0;
        for pair in 0..8 {
            if pairs & (1 << pair) != 0 {
                eus |= 3 << (2 * pair);
            }
        }
        if eus == 0 {
            return Err(Error::Refused);
        }
        Ok(Self {
            dss: dss as u8,
            eus,
        })
    }
    pub fn eu_total(self) -> u32 {
        self.dss.count_ones() * self.eus.count_ones()
    }
    /// UAPI topology layout for source max_slices=1/max_DSS=6/EUs=16.
    pub fn wire(self) -> [u8; 30] {
        let mut out = [0; 30];
        for (i, value) in [0u16, 1, 6, 16, 1, 1, 2, 2].into_iter().enumerate() {
            out[i * 2..i * 2 + 2].copy_from_slice(&value.to_le_bytes());
        }
        out[16] = 1;
        out[17] = self.dss;
        for dss in 0..6 {
            if self.dss & (1 << dss) != 0 {
                out[18 + dss * 2..20 + dss * 2].copy_from_slice(&self.eus.to_le_bytes());
            }
        }
        out
    }
}
pub fn clock_frequency(io: &impl GtIo) -> Result<u32, Error> {
    if io.read(0xa26c)? & 1 != 0 {
        let override_value = io.read(0x44074)?;
        Ok(((override_value & 0x3ff) + 1) * 1_000_000
            + 1_000_000 / (((override_value >> 12) & 15) + 1))
    } else {
        let config = io.read(0xd00)?;
        let frequency = match (config >> 3) & 7 {
            0 => 24_000_000,
            1 => 19_200_000,
            2 => 38_400_000,
            3 => 25_000_000,
            _ => return Err(Error::Refused),
        };
        Ok(frequency >> (3 - ((config >> 1) & 3)))
    }
}
