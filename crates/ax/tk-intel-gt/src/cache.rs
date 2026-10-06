// SPDX-License-Identifier: MIT
// Linux7.2.3 gt/intel_mocs.c gen12_mocs_table/GEN11_MOCS_ENTRIES,
// get_entry_control/get_entry_l3cc/__init_mocs_table/init_l3cc_table;
// Copyright ©2015 Intel Corporation. gt/intel_gtt.c tgl_setup_private_ppat,
// Copyright ©2020 Intel Corporation. Full MIT grants: ../LICENSE-MIT.
//! Exact ADL-N Gen12.0 user-visible MOCS indices, not TGL's deprecated slot1.
//! Caller owns GT/render wake and proves every engine idle before shared writes.
use crate::{Error, GtIo};
pub fn entry(index: usize) -> (u32, u16) {
    // LE_3_WB|LE_TC_1_LLC|LE_LRUM(3), L3_3_WB is source unused index2.
    let cached = 3 | (1 << 2) | (3 << 4);
    let uncached = 1 | (1 << 2);
    match index {
        3 | 51 => (uncached, 0x10),
        4 | 49 | 61 => (uncached, 0x30),
        5 | 50 | 60 | 62 | 63 => (cached, 0x10),
        6 => (3 | (1 << 2) | (1 << 4), 0x10),
        7 => (3 | (1 << 2) | (1 << 4), 0x30),
        8 => (3 | (1 << 2) | (2 << 4), 0x10),
        9 => (3 | (1 << 2) | (2 << 4), 0x30),
        10 => (cached | (1 << 6), 0x10),
        11 => (cached | (1 << 6), 0x30),
        12 => (3 | (1 << 2) | (1 << 4) | (1 << 6), 0x10),
        13 => (3 | (1 << 2) | (1 << 4) | (1 << 6), 0x30),
        14 => (3 | (1 << 2) | (2 << 4) | (1 << 6), 0x10),
        15 => (3 | (1 << 2) | (2 << 4) | (1 << 6), 0x30),
        16 => (uncached | (1 << 14), 0x10),
        17 => (uncached | (1 << 14), 0x30),
        18 => (cached | (3 << 17), 0x30),
        19 => (cached | (7 << 8), 0x30),
        20 => (cached | (3 << 8), 0x30),
        21 => (cached | (1 << 8), 0x30),
        22 => (cached | (1 << 7) | (3 << 8), 0x30),
        23 => (cached | (1 << 7) | (7 << 8), 0x30),
        _ => (cached, 0x30),
    }
}
fn write(io: &impl GtIo, register: u32, value: u32) -> Result<(), Error> {
    io.write(register, value)?;
    if io.read(register)? != value {
        return Err(Error::Refused);
    }
    Ok(())
}
pub fn prepare(io: &impl GtIo) -> Result<(), Error> {
    // Source intel_mocs_init: global control first, then paired L3 table.
    for index in 0..64 {
        write(io, 0x4000 + index * 4, entry(index as usize).0)?;
    }
    for index in 0..32 {
        let low = entry(index as usize * 2).1;
        let high = entry(index as usize * 2 + 1).1;
        write(
            io,
            0xb020 + index * 4,
            u32::from(low) | (u32::from(high) << 16),
        )?;
    }
    // Source tgl_setup_private_ppat, also selected for ADL-N Gen12.0.
    for (index, value) in [3, 1, 2, 0, 3, 3, 3, 3].into_iter().enumerate() {
        write(io, 0x4800 + index as u32 * 4, value)?;
    }
    Ok(())
}
