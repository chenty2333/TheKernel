// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! The `POWER_DOMAIN_GT_IRQ` reference rule for a device without a display.
//!
//! Linux `intel_display_power.c::get_allowed_dc_mask()` returns 0 when
//! `!HAS_DISPLAY`, so no display well is ever requested for the GT_IRQ domain
//! and its reference is a plain count (`upstream_gt.rs`). The one hardware
//! effect that remains is the DC-state request: a DC5/DC6/DC9 or DC3CO request
//! left by firmware would delay GT interrupts, so the first reference clears it
//! the way Linux `gen9_set_dc_state(DC_STATE_DISABLE)` does at init.

/// The `DC_STATE_EN` bits that request a DC state: DC5 and DC6 (`[1:0]`), DC9
/// (bit 3) and DC3CO (bit 30).
pub(super) const DC_STATE_MASK: u32 = 0b1011 | (1 << 30);

/// Clear any DC-state request in `DC_STATE_EN`, given a read and a write of that
/// register. Returns the value that was found.
pub(super) fn disable_dc_states(read: impl FnOnce() -> u32, write: impl FnOnce(u32)) -> u32 {
    let found = read();
    if found & DC_STATE_MASK != 0 {
        axlog::warn!(
            "intel-gt: DC_STATE_EN {found:#010x} requests a DC state; clearing it before GT_IRQ"
        );
        write(found & !DC_STATE_MASK);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    #[test]
    fn a_dc_request_is_cleared_and_other_bits_are_kept() {
        let register = Cell::new(0x4000_0000 | 0b1000 | 0x20);
        let found = disable_dc_states(|| register.get(), |value| register.set(value));
        assert_eq!(found, 0x4000_0000 | 0b1000 | 0x20);
        assert_eq!(register.get(), 0x20);
    }

    #[test]
    fn a_register_with_no_dc_request_is_not_written() {
        let writes = Cell::new(0u32);
        disable_dc_states(|| 0x20, |_| writes.set(writes.get() + 1));
        assert_eq!(writes.get(), 0);
    }
}
