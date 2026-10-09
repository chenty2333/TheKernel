// SPDX-License-Identifier: MIT
// Copyright © 2023 Intel Corporation
//! AUX channel register instances used by the display-12/13 DP transport.
//!
//! The ADL-N port admits AUX A/B and TC1/TC2 (source channels D/E). These
//! addresses and the data-window stride follow `intel_dp_aux_regs.h`; they are separate typed
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

// TGL/ADL Type-C AUX channels follow the source `AUX_CH_*` enumeration:
// USBC1 aliases AUX_CH_D (index 3), USBC2 aliases AUX_CH_E (index 4). The
// source `_PICK_EVEN` expansion advances the A/B control/data bases by 0x100
// per channel. These are AUX MMIO channels, not DKL indexed-PHY registers.
pub(crate) const DP_AUX_CH_CTL_D: Register =
    Register::read_write("DP_AUX_CH_CTL(D)", 0x64310, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA0_D: Register =
    Register::read_write("DP_AUX_CH_DATA(D,0)", 0x64314, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA1_D: Register =
    Register::read_write("DP_AUX_CH_DATA(D,1)", 0x64318, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA2_D: Register =
    Register::read_write("DP_AUX_CH_DATA(D,2)", 0x6431c, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA3_D: Register =
    Register::read_write("DP_AUX_CH_DATA(D,3)", 0x64320, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA4_D: Register =
    Register::read_write("DP_AUX_CH_DATA(D,4)", 0x64324, Meaning::BusController, None);

pub(crate) const DP_AUX_CH_CTL_E: Register =
    Register::read_write("DP_AUX_CH_CTL(E)", 0x64410, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA0_E: Register =
    Register::read_write("DP_AUX_CH_DATA(E,0)", 0x64414, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA1_E: Register =
    Register::read_write("DP_AUX_CH_DATA(E,1)", 0x64418, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA2_E: Register =
    Register::read_write("DP_AUX_CH_DATA(E,2)", 0x6441c, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA3_E: Register =
    Register::read_write("DP_AUX_CH_DATA(E,3)", 0x64420, Meaning::BusController, None);
pub(crate) const DP_AUX_CH_DATA4_E: Register =
    Register::read_write("DP_AUX_CH_DATA(E,4)", 0x64424, Meaning::BusController, None);
