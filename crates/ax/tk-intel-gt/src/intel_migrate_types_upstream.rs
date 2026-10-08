// SPDX-License-Identifier: MIT
// Copyright © 2020 Intel Corporation.
//
//! Record from Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/intel_migrate_types.h`.

use crate::intel_context_types_upstream::IntelContext;

/// `struct intel_migrate`.
#[repr(C)]
pub struct IntelMigrate {
    pub context: *mut IntelContext,
}

// x86_64 Linux 7.2.3: the record contains one pointer only.
const _: [(); 8] = [(); core::mem::size_of::<IntelMigrate>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelMigrate>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelMigrate, context)];
