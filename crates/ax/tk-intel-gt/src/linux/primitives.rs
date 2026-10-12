// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
// Generic primitives come from tk-linux-kapi; the i915 GuC CT timing helper
// stays with the translated GuC code.

pub use tk_linux_kapi::primitives::*;

/// Linux 7.2.3 CTB bound: `(SZ_4K * HZ) / SZ_2K`.
#[inline]
pub const fn intel_guc_ct_max_queue_time_jiffies() -> i64 {
    (4096 * crate::linux_config::CONFIG_HZ as i64) / 2048
}

