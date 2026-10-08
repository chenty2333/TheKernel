// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_fw.c:
// guc_load_done status decoding; register read is supplied by GtIo caller.
// Copyright © 2014-2019 Intel Corporation. Full grant: LICENSE-MIT.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_poll_distinguishes_ready_terminal_errors_and_pending() {
        assert_eq!(load_done(0xf0 << 8), Some(true));
        assert_eq!(load_done(0x02 << 8), Some(false));
        assert_eq!(load_done(0x50 << 1), Some(false));
        assert_eq!(load_done(0x30 << 8), None);
        assert_eq!(load_done(0), None);
    }
}
