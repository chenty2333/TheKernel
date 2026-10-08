// SPDX-License-Identifier: MIT
// PCode mailbox register offsets from Linux 7.2.3 `intel_pcode_regs.h` and
// `i915_reg.h` (Intel MIT); source definitions are translated in
// `tk-intel-display::intel_pcode_full`.

use super::{Meaning, Register};

pub(crate) const GEN6_PCODE_MAILBOX: Register = Register::read_write(
    "GEN6_PCODE_MAILBOX",
    0x138124,
    Meaning::BringUp,
    None,
);
pub(crate) const GEN6_PCODE_DATA: Register = Register::read_write(
    "GEN6_PCODE_DATA",
    0x138128,
    Meaning::BringUp,
    None,
);
pub(crate) const GEN6_PCODE_DATA1: Register = Register::read_write(
    "GEN6_PCODE_DATA1",
    0x13812c,
    Meaning::BringUp,
    None,
);
