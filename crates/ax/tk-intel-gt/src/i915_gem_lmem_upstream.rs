// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Source-faithful Linux 7.2.3 i915_gem_lmem.c region-residency predicate.

use crate::{
    i915_gem_object_types_upstream::DrmI915GemObject,
    linux::gem_memory::intel_memory_type_is_local,
    linux_macros::read_once,
};

/// Whether an object is currently resident in device-local memory. Migratable
/// objects are meaningful here only while their object reservation is held,
/// exactly as in the upstream contract.
pub unsafe fn i915_gem_object_is_lmem(obj: *mut DrmI915GemObject) -> bool {
    assert!(!obj.is_null());
    let region = read_once(core::ptr::addr_of!((*obj).mm.region));
    !region.is_null() && unsafe { intel_memory_type_is_local((*region).r#type) }
}

