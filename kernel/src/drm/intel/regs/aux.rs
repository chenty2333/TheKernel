// SPDX-License-Identifier: MIT
// Copyright © 2023 Intel Corporation
//! AUX channel register instances used by the display-12/13 DP transport.
//!
//! The ADL-N port currently admits AUX A/B only. These addresses and the
//! data-window stride follow `intel_dp_aux_regs.h`; they are separate typed
//! entries so channel and data-dword indices cannot be synthesized from
//! unchecked caller offsets.

use super::{Meaning, Register};

pub(crate) const DP_AUX_CH_CTL_A: Register =
    Register::read_write("DP_AUX_CH_CTL(A)", 0x64010, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA0_A: Register =
    Register::read_write("DP_AUX_CH_DATA(A,0)", 0x64014, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA1_A: Register =
    Register::read_write("DP_AUX_CH_DATA(A,1)", 0x64018, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA2_A: Register =
    Register::read_write("DP_AUX_CH_DATA(A,2)", 0x6401c, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA3_A: Register =
    Register::read_write("DP_AUX_CH_DATA(A,3)", 0x64020, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA4_A: Register =
    Register::read_write("DP_AUX_CH_DATA(A,4)", 0x64024, Meaning::BusController, None);

pub(crate) const DP_AUX_CH_CTL_B: Register =
    Register::read_write("DP_AUX_CH_CTL(B)", 0x64110, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA0_B: Register =
    Register::read_write("DP_AUX_CH_DATA(B,0)", 0x64114, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA1_B: Register =
    Register::read_write("DP_AUX_CH_DATA(B,1)", 0x64118, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA2_B: Register =
    Register::read_write("DP_AUX_CH_DATA(B,2)", 0x6411c, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA3_B: Register =
    Register::read_write("DP_AUX_CH_DATA(B,3)", 0x64120, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA4_B: Register =
    Register::read_write("DP_AUX_CH_DATA(B,4)", 0x64124, Meaning::BusController, None);
