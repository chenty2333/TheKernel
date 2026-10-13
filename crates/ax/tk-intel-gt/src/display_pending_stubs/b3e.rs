// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! B3e CDCLK exports, retained under the pending-stub path for call-site stability.

pub use crate::intel_cdclk_upstream::{
    intel_cdclk_clock_changed,
    intel_cdclk_get_cdclk,
    intel_cdclk_init,
    intel_cdclk_init_hw,
    intel_cdclk_read_hw,
    intel_cdclk_uninit_hw,
    intel_cdclk_update_dbuf_bw_min_cdclk,
    intel_init_cdclk_hooks,
    intel_read_rawclk,
    intel_update_max_cdclk,
};
