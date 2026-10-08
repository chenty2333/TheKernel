// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_fw.c:
// guc_load_done status decoding; register read is supplied by GtIo caller.
// Copyright © 2014-2019 Intel Corporation. Full grant: LICENSE-MIT.

use crate::{Error, GtIo};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisterWrite {
    pub offset: u32,
    pub value: u32,
}

// upstream: intel_guc_fw.c guc_prepare_xfer()
/// Gen12.0 branch: shim cache/clock policy must precede DMA, then enable
/// doorbells. Older Gen12 SRAM/MIA settings are retained verbatim.
pub const fn gen12_prepare_xfer() -> [RegisterWrite; 2] {
    [
        RegisterWrite {
            offset: 0xc064, // GUC_SHIM_CONTROL
            value: (1 << 1) | (1 << 9) | (1 << 10) | (1 << 15) | (1 << 0) | (1 << 2),
        },
        RegisterWrite {
            offset: 0x13816c, // GEN9_GT_PM_CONFIG
            value: 1,         // GT_DOORBELL_ENABLE
        },
    ]
}

// upstream: intel_guc_fw.c guc_load_done()
/// `None` means GuC remains in a transitional state; `Some(false)` is a
/// terminal firmware/BootROM failure, and `Some(true)` is READY.
pub fn load_done(status: u32) -> Option<bool> {
    let ukernel = (status >> 8) & 0xff;
    let bootrom = (status >> 1) & 0x7f;
    match ukernel {
        0xf0 => return Some(true),
        0x02 | 0x03 | 0x04 | 0x07 | 0x60 | 0x70 | 0x71 | 0x73 | 0x74 | 0x75 => {
            return Some(false);
        }
        _ => {}
    }
    match bootrom {
        0x13 | 0x50 | 0x73 | 0x74 | 0x75 | 0x77 | 0x79 | 0x7a | 0x7e | 0x2b => Some(false),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadError {
    Io(Error),
    Firmware(u32),
    Timeout { status: u32, attempts: u8 },
}

// upstream: intel_guc_fw.c guc_wait_ucode()
pub fn wait_ucode(io: &impl GtIo) -> Result<u32, LoadError> {
    wait_ucode_with(io, 1_000_000, 3)
}

fn wait_ucode_with(io: &impl GtIo, wait_us: u64, attempts: u8) -> Result<u32, LoadError> {
    let mut status = 0;
    for attempt in 0..attempts {
        let start = io.now_us();
        loop {
            status = io.read(0xc000).map_err(LoadError::Io)?; // GUC_STATUS
            match load_done(status) {
                Some(true) => return Ok(status),
                Some(false) => return Err(LoadError::Firmware(status)),
                None => {}
            }
            if io.now_us().saturating_sub(start) >= wait_us {
                break;
            }
            io.delay_us(1);
        }
        if attempt + 1 < attempts {
            continue;
        }
    }
    Err(LoadError::Timeout { status, attempts })
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    struct ScriptedIo {
        statuses: &'static [u32],
        index: Cell<usize>,
        time: Cell<u64>,
    }
    impl GtIo for ScriptedIo {
        fn read(&self, offset: u32) -> Result<u32, Error> {
            if offset != 0xc000 {
                return Err(Error::Unavailable(offset));
            }
            let index = self.index.get();
            self.index.set(index.saturating_add(1));
            Ok(self.statuses[index.min(self.statuses.len() - 1)])
        }
        fn write(&self, _offset: u32, _value: u32) -> Result<(), Error> {
            Err(Error::Refused)
        }
        fn now_us(&self) -> u64 {
            self.time.get()
        }
        fn delay_us(&self, micros: u32) {
            self.time
                .set(self.time.get().saturating_add(u64::from(micros)));
        }
    }

    #[test]
    fn load_poll_distinguishes_ready_terminal_errors_and_pending() {
        assert_eq!(load_done(0xf0 << 8), Some(true));
        assert_eq!(load_done(0x02 << 8), Some(false));
        assert_eq!(load_done(0x50 << 1), Some(false));
        assert_eq!(load_done(0x30 << 8), None);
        assert_eq!(load_done(0), None);
    }

    #[test]
    fn gen12_guc_transfer_order_and_flags_match_upstream() {
        let writes = gen12_prepare_xfer();
        assert_eq!(writes[0].offset, 0xc064);
        assert_eq!(writes[0].value, 0x8607);
        assert_eq!(writes[1].offset, 0x13816c);
        assert_eq!(writes[1].value, 1);
    }

    #[test]
    fn wait_poll_retries_transitional_status_and_stops_on_result() {
        let ready = ScriptedIo {
            statuses: &[0, 0xf0 << 8],
            index: Cell::new(0),
            time: Cell::new(0),
        };
        assert_eq!(wait_ucode_with(&ready, 2, 3), Ok(0xf0 << 8));

        let failed = ScriptedIo {
            statuses: &[0x02 << 8],
            index: Cell::new(0),
            time: Cell::new(0),
        };
        assert_eq!(
            wait_ucode_with(&failed, 2, 3),
            Err(LoadError::Firmware(0x02 << 8))
        );

        let pending = ScriptedIo {
            statuses: &[0],
            index: Cell::new(0),
            time: Cell::new(0),
        };
        assert_eq!(
            wait_ucode_with(&pending, 2, 3),
            Err(LoadError::Timeout {
                status: 0,
                attempts: 3
            })
        );
    }
}
