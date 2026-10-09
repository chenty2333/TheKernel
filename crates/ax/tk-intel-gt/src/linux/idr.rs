// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../../LICENSE-MIT.
//! Linux IDA surface backed by the LinuxKPI XArray indexed storage.

use crate::linux::{
    config::{GFP_ATOMIC, GFP_KERNEL},
    xarray::{self, XArray},
};

/// Linux `struct ida` embeds one `struct xarray` as its complete ABI.
#[repr(C)]
pub struct Ida {
    pub xa: XArray,
}

const _: [(); 16] = [(); core::mem::size_of::<Ida>()];
const _: [(); 0] = [(); core::mem::offset_of!(Ida, xa)];

pub fn ida_init(ida: &mut Ida) {
    xarray::xa_init_flags(&mut ida.xa, 0);
}

pub fn ida_destroy(ida: &mut Ida) {
    xarray::xa_destroy(&mut ida.xa);
}

/// Allocate the lowest unused ID in the inclusive range.
///
/// `IDA` stores an allocated bitmap entry per ID in the XArray. This binding
/// uses a non-null sentinel entry; the ID uniqueness and locking contract are
/// preserved, and the allocator's GFP_ATOMIC limitation is inherited from
/// the XArray backend.
pub fn ida_alloc_range(ida: &mut Ida, min: u32, max: u32, gfp: u32) -> i32 {
    if min > max {
        return -22;
    }
    if gfp & GFP_ATOMIC != 0 && gfp & GFP_KERNEL == 0 {
        return -12;
    }
    let mut flags = 0u64;
    xarray::xa_lock_irqsave(&mut ida.xa, &mut flags);
    let result = (min..=max).find(|id| xarray::xa_load::<_, u8>(&mut ida.xa, *id).is_null());
    let status = if let Some(id) = result {
        let marker = core::ptr::NonNull::<u8>::dangling().as_ptr();
        xarray::__xa_store(&mut ida.xa, id, marker, 0);
        id as i32
    } else {
        -28
    };
    xarray::xa_unlock_irqrestore(&mut ida.xa, flags);
    status
}

pub fn ida_free(ida: &mut Ida, id: u32) {
    let _ = xarray::xa_erase_irq::<_, u8>(&mut ida.xa, id);
}
