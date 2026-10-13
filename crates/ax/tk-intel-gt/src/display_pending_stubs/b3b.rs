// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by batch B3b (power domains, PMDemand, PCH, HTI).
//! Replaced by intel_display_power_upstream.rs, intel_pch_upstream.rs,
//! intel_pmdemand_upstream.rs, intel_hti_upstream.rs (B3b).

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_display_core_types_upstream::IntelDisplay;

pub use crate::intel_pch_upstream::intel_pch_detect;

pub use crate::intel_display_power_upstream::intel_display_power_init;
pub use crate::intel_pmdemand_upstream::intel_pmdemand_init_early;
pub use crate::intel_display_power_upstream::intel_display_power_init_hw;
pub use crate::intel_pmdemand_upstream::intel_pmdemand_init;
pub use crate::intel_display_power_upstream::intel_display_power_driver_remove;
pub use crate::intel_hti_upstream::intel_hti_init;

