//! eBPF subsystem: virtual machine, maps, verifier, and program management.

pub mod defs;
pub mod helpers;
pub mod map;
pub mod prog;
pub mod verifier;
pub mod vm;

use core::sync::atomic::{AtomicU32, Ordering};

static NEXT_MAP_ID: AtomicU32 = AtomicU32::new(1);
static NEXT_PROG_ID: AtomicU32 = AtomicU32::new(1);
const BPF_ATTR_MAX_SIZE: usize = 4096;

pub fn alloc_map_id() -> u32 {
    NEXT_MAP_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn alloc_prog_id() -> u32 {
    NEXT_PROG_ID.fetch_add(1, Ordering::Relaxed)
}

/// Read bpf attr from user space. Reads `min(attr_size, size_of::<T>())` bytes,
/// zero-fills the rest. This provides forward/backward compatibility.
pub fn read_bpf_attr<T: bytemuck::AnyBitPattern>(
    attr_ptr: usize,
    attr_size: u32,
) -> axerrno::AxResult<T> {
    use alloc::vec;

    use axerrno::AxError;

    let attr_size = attr_size as usize;
    let want = core::mem::size_of::<T>();
    if attr_size > BPF_ATTR_MAX_SIZE {
        return Err(AxError::ArgumentListTooLong);
    }

    let copy_len = attr_size.min(want);
    if copy_len == 0 {
        return Err(AxError::InvalidInput);
    }

    let src =
        starry_vm::vm_load(attr_ptr as *const u8, copy_len).map_err(|_| AxError::BadAddress)?;
    if attr_size > want {
        let tail_ptr = attr_ptr.checked_add(want).ok_or(AxError::InvalidInput)?;
        let tail = starry_vm::vm_load(tail_ptr as *const u8, attr_size - want)
            .map_err(|_| AxError::BadAddress)?;
        if tail.iter().any(|&byte| byte != 0) {
            return Err(AxError::ArgumentListTooLong);
        }
    }
    let mut buf = vec![0u8; want];
    buf[..copy_len].copy_from_slice(&src);
    Ok(bytemuck::pod_read_unaligned(&buf))
}

pub fn require_bpf_attr_range<T>(attr_size: u32, end: usize) -> axerrno::AxResult<()> {
    use axerrno::AxError;

    if end > core::mem::size_of::<T>() || (attr_size as usize) < end {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

pub fn write_bpf_attr_value<TAttr, TValue: bytemuck::NoUninit>(
    attr_ptr: usize,
    attr_size: u32,
    offset: usize,
    value: &TValue,
) -> axerrno::AxResult<()> {
    use axerrno::AxError;

    let end = offset
        .checked_add(core::mem::size_of::<TValue>())
        .ok_or(AxError::InvalidInput)?;
    require_bpf_attr_range::<TAttr>(attr_size, end)?;
    starry_vm::vm_write_slice((attr_ptr + offset) as *mut u8, bytemuck::bytes_of(value))
        .map_err(|_| AxError::BadAddress)?;
    Ok(())
}
