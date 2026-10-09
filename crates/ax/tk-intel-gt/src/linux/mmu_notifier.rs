// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! LinuxKPI storage layout for an interval MMU-notifier embedded by the i915
//! userptr object. This provides no notifier registration or invalidation work.

use crate::intel_engine_cs_upstream::{HlistNode, RbNode};

#[repr(C)]
pub struct MmuIntervalNotifier {
    pub interval_tree: IntervalTreeNode,
    pub ops: *const MmuIntervalNotifierOps,
    pub mm: *mut MmStruct,
    pub deferred_item: HlistNode,
    pub invalidate_seq: usize,
}

#[repr(C)]
pub struct IntervalTreeNode {
    pub rb: RbNode,
    pub start: usize,
    pub last: usize,
    pub subtree_last: usize,
}

#[repr(C)]
pub struct MmuIntervalNotifierOps {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct MmStruct {
    _opaque: [u8; 0],
}

const _: [(); 48] = [(); core::mem::size_of::<IntervalTreeNode>()];
const _: [(); 88] = [(); core::mem::size_of::<MmuIntervalNotifier>()];
const _: [(); 8] = [(); core::mem::align_of::<MmuIntervalNotifier>()];
const _: [(); 48] = [(); core::mem::offset_of!(MmuIntervalNotifier, ops)];
const _: [(); 56] = [(); core::mem::offset_of!(MmuIntervalNotifier, mm)];
const _: [(); 64] = [(); core::mem::offset_of!(MmuIntervalNotifier, deferred_item)];
const _: [(); 80] = [(); core::mem::offset_of!(MmuIntervalNotifier, invalidate_seq)];
