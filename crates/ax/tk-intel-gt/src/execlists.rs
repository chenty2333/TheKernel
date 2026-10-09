// SPDX-License-Identifier: MIT
// Translated from Linux 7.2.3 drivers/gpu/drm/i915/gt/intel_execlists_submission.c
// Copyright © 2014 Intel Corporation. Full MIT grant: LICENSE-MIT.
//! Gen12 Execlists submission-queue MMIO operations.

use crate::{Error, GtIo};

pub const EL_CTRL_LOAD: u32 = 1;
pub const GEN12_NUM_PORTS: usize = 2;
pub const GEN12_CSB_ENTRIES: usize = 12;
const GEN12_CTX_STATUS_SWITCHED_TO_NEW_QUEUE: u32 = 1;
const GEN12_CSB_SW_CTX_ID_MASK: u32 = 0x03ff_8000;
const GEN12_IDLE_CTX_ID: u32 = 0x7ff;

/// Parse a Gen12 context-status entry and report whether a context completed.
/// upstream: intel_execlists_submission.c __gen12_csb_parse()
fn __gen12_csb_parse(
    context_to_valid: bool,
    context_away_valid: bool,
    switched_to_new_queue: bool,
    switch_detail: u8,
) -> Result<bool, Error> {
    if !context_away_valid || switched_to_new_queue {
        // Upstream uses GEM_BUG_ON for these impossible hardware states.
        if !context_to_valid {
            return Err(Error::Refused);
        }
        return Ok(true);
    }
    if switch_detail != 0 {
        return Err(Error::Refused);
    }
    Ok(false)
}

/// Parse the Gen12 (TGL/RKL/ADL) CSB entry format.
/// upstream: intel_execlists_submission.c gen12_csb_parse()
pub fn gen12_csb_parse(csb: u64) -> Result<bool, Error> {
    let lower = csb as u32;
    let upper = (csb >> 32) as u32;
    __gen12_csb_parse(
        (lower & GEN12_CSB_SW_CTX_ID_MASK) >> 15 != GEN12_IDLE_CTX_ID,
        (upper & GEN12_CSB_SW_CTX_ID_MASK) >> 15 != GEN12_IDLE_CTX_ID,
        lower & GEN12_CTX_STATUS_SWITCHED_TO_NEW_QUEUE != 0,
        (upper & 0xf) as u8,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CsbProgress {
    pub head: u8,
    pub consumed: u8,
    pub promoted: u8,
}

/// Drain the Gen11/12-sized status ring up to its hardware write pointer.
/// `entries` is the 12-element `I915_HWS_CSB_BUF0_INDEX` array.
/// The full upstream `process_csb()` scheduler state machine is not implemented here.
pub fn process_gen12_csb(
    head: u8,
    write_pointer: u32,
    entries: &[u64; GEN12_CSB_ENTRIES],
) -> Result<CsbProgress, Error> {
    if usize::from(head) >= GEN12_CSB_ENTRIES {
        return Err(Error::Refused);
    }
    let tail = (write_pointer & 0xf) as u8;
    if usize::from(tail) >= GEN12_CSB_ENTRIES {
        return Err(Error::Refused);
    }
    let mut result = CsbProgress {
        head,
        consumed: 0,
        promoted: 0,
    };
    while result.head != tail {
        result.head = ((usize::from(result.head) + 1) % GEN12_CSB_ENTRIES) as u8;
        let promote = gen12_csb_parse(entries[usize::from(result.head)])?;
        result.consumed += 1;
        if promote {
            result.promoted += 1;
        }
    }
    Ok(result)
}

/// Write one descriptor into an Execlists submit queue port.
/// upstream: intel_execlists_submission.c write_desc()
pub fn write_desc(
    io: &impl GtIo,
    submit_reg: u32,
    _ctrl_reg: Option<u32>,
    descriptor: u64,
    port: u32,
) -> Result<(), Error> {
    if _ctrl_reg.is_some() {
        // Upstream submit_reg is u32 __iomem*: pointer steps are dwords,
        // while GtIo takes byte offsets. Preserve the C port * 2 stride.
        let offset = port.checked_mul(2 * 4).ok_or(Error::Refused)?;
        io.write(
            submit_reg.checked_add(offset).ok_or(Error::Refused)?,
            descriptor as u32,
        )?;
        io.write(
            submit_reg
                .checked_add(offset)
                .and_then(|value| value.checked_add(4))
                .ok_or(Error::Refused)?,
            (descriptor >> 32) as u32,
        )?;
    } else {
        // Legacy ELSP expects descriptor high dword before low dword.
        io.write(submit_reg, (descriptor >> 32) as u32)?;
        io.write(submit_reg, descriptor as u32)?;
    }
    Ok(())
}

/// Program every ELSQ entry (including zero for an empty port), then load it.
/// The reverse-order loop and identical entry count are required because ELSQ
/// is not cleared by hardware after a submit.
/// upstream: intel_execlists_submission.c execlists_submit_ports()
pub fn execlists_submit_ports(
    io: &impl GtIo,
    submit_reg: u32,
    ctrl_reg: u32,
    descriptors: [u64; GEN12_NUM_PORTS],
) -> Result<(), Error> {
    for port in (0..GEN12_NUM_PORTS).rev() {
        write_desc(
            io,
            submit_reg,
            Some(ctrl_reg),
            descriptors[port],
            port as u32,
        )?;
    }
    io.write(ctrl_reg, EL_CTRL_LOAD)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use super::*;

    #[derive(Default)]
    struct Io(RefCell<Vec<(u32, u32)>>);

    impl GtIo for Io {
        fn read(&self, _: u32) -> Result<u32, Error> {
            Err(Error::Refused)
        }

        fn write(&self, register: u32, value: u32) -> Result<(), Error> {
            // This mock is only used by the single-threaded test below.
            self.0.borrow_mut().push((register, value));
            Ok(())
        }

        fn now_us(&self) -> u64 {
            0
        }

        fn delay_us(&self, _: u32) {}
    }

    #[test]
    fn gen12_programs_both_elsq_ports_in_reverse_order_then_loads() {
        let io = Io::default();
        execlists_submit_ports(&io, 0x2510, 0x2550, [0x1122_3344_5566_7788, 0]).unwrap();
        assert_eq!(
            *io.0.borrow(),
            [
                (0x2518, 0),
                (0x251c, 0),
                (0x2510, 0x5566_7788),
                (0x2514, 0x1122_3344),
                (0x2550, EL_CTRL_LOAD),
            ]
        );
    }

    #[test]
    fn gen12_csb_parser_recognizes_completion_and_rejects_impossible_entries() {
        let valid_context = 7u64 << 15;
        let idle_context = u64::from(GEN12_IDLE_CTX_ID) << 15;
        assert_eq!(
            gen12_csb_parse(valid_context | (idle_context << 32)),
            Ok(true)
        );
        assert_eq!(
            gen12_csb_parse(valid_context | (valid_context << 32)),
            Ok(false)
        );
        assert_eq!(
            gen12_csb_parse(valid_context | u64::from(GEN12_CTX_STATUS_SWITCHED_TO_NEW_QUEUE)),
            Ok(true)
        );
        assert_eq!(
            gen12_csb_parse(idle_context | (idle_context << 32)),
            Err(Error::Refused)
        );
        assert_eq!(
            gen12_csb_parse(valid_context | (1u64 << 32)),
            Err(Error::Refused)
        );
    }

    #[test]
    fn gen12_process_csb_wraps_from_last_entry_to_zero() {
        let mut entries = [0; GEN12_CSB_ENTRIES];
        entries[0] = (u64::from(GEN12_IDLE_CTX_ID) << 47) | (3u64 << 15);
        entries[1] = (3u64 << 47) | (3u64 << 15);
        let progress = process_gen12_csb(11, 1, &entries).unwrap();
        assert_eq!(
            progress,
            CsbProgress {
                head: 1,
                consumed: 2,
                promoted: 1
            }
        );
        assert_eq!(process_gen12_csb(11, 11, &entries).unwrap().consumed, 0);
        assert_eq!(process_gen12_csb(11, 12, &entries), Err(Error::Refused));
    }
}
