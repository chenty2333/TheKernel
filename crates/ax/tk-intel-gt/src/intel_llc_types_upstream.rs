// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//
//! Type binding from Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/intel_llc_types.h`.

/// `struct intel_llc` is empty in the enabled v7.2.3 source configuration.
#[repr(C)]
pub struct IntelLlc {}

// GCC's Linux C ABI treats this GNU empty record as zero-sized/alignment 1.
const _: [(); 0] = [(); core::mem::size_of::<IntelLlc>()];
const _: [(); 1] = [(); core::mem::align_of::<IntelLlc>()];
