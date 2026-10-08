//! Intel I225-family operations translated from `sys/dev/igc/igc_i225.c`.
//!
//! FreeBSD commit `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright 2021 Intel Corp; Copyright 2021 Rubicon Communications, LLC.

use super::api::{IgcApiCallback, IgcHardware};

// upstream: igc_i225.c igc_init_function_pointers_i225()
pub fn init_function_pointers_i225(hw: &mut IgcHardware) {
    hw.mac_ops.init_params = Some(IgcApiCallback::MacInitParamsI225);
    hw.nvm_ops.init_params = Some(IgcApiCallback::NvmInitParamsI225);
    hw.phy_ops.init_params = Some(IgcApiCallback::PhyInitParamsI225);
}
