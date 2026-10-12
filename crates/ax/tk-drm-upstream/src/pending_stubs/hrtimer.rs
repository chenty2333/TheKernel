// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for kernel/time/hrtimer.c (kthread/timer batch). Each symbol is defined once
//! here; delete the entry when its owner lands. Replaced by hrtimer_upstream.rs when the timer batch lands.
//!
//! Symbols: hrtimer_active, hrtimer_forward_now.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use tk_linux_kapi::types::Hrtimer;

// upstream: include/linux/hrtimer.h hrtimer_active()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hrtimer_active(_timer: *const Hrtimer) -> bool {
    false
}

// upstream: kernel/time/hrtimer.c hrtimer_forward_now()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hrtimer_forward_now(_timer: *mut Hrtimer, _interval: i64) -> u64 {
    1
}
