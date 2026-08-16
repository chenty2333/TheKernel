//! eBPF subsystem: virtual machine, maps, verifier, and program management.

pub mod defs;
pub mod helpers;
pub mod map;
pub mod prog;
pub mod verifier;
pub mod vm;

use alloc::{vec, vec::Vec};
use core::{
    mem::MaybeUninit,
    sync::atomic::{AtomicU32, Ordering},
};

use axerrno::AxError;
use bytemuck::AnyBitPattern;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext};

use crate::mm::map_usercopy_error;

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
pub fn read_bpf_attr<M: UserMemory + ?Sized, T: AnyBitPattern>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> axerrno::AxResult<T> {
    let attr_size = attr_size as usize;
    let want = core::mem::size_of::<T>();
    if attr_size > BPF_ATTR_MAX_SIZE {
        return Err(AxError::ArgumentListTooLong);
    }

    let copy_len = attr_size.min(want);
    if copy_len == 0 {
        return Err(AxError::InvalidInput);
    }

    let src = read_user_bytes(memory, attr_ptr, copy_len)?;
    if attr_size > want {
        let tail_ptr = attr_ptr.checked_add(want).ok_or(AxError::InvalidInput)?;
        let tail = read_user_bytes(memory, tail_ptr, attr_size - want)?;
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

pub fn write_bpf_attr_value<TAttr, TValue: bytemuck::NoUninit, M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
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
    let destination = attr_ptr.checked_add(offset).ok_or(AxError::InvalidInput)?;
    memory
        .write_bytes(destination, bytemuck::bytes_of(value))
        .map_err(map_usercopy_error)?;
    Ok(())
}

/// Copies a byte range from the address space bound to this operation.
pub fn read_user_bytes<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: usize,
    len: usize,
) -> axerrno::AxResult<Vec<u8>> {
    let mut bytes = vec![0u8; len];
    // SAFETY: `MaybeUninit<u8>` has the same layout as `u8`; the provider
    // initializes every byte before this function returns successfully.
    let destination = unsafe {
        core::slice::from_raw_parts_mut(bytes.as_mut_ptr().cast::<MaybeUninit<u8>>(), len)
    };
    memory
        .read_bytes(ptr, destination)
        .map_err(map_usercopy_error)?;
    Ok(bytes)
}

/// Copies a typed slice from the address space bound to this operation.
pub fn read_user_slice<M: UserMemory + ?Sized, T: AnyBitPattern>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: usize,
    len: usize,
) -> axerrno::AxResult<Vec<T>> {
    let mut values = vec![T::zeroed(); len];
    // SAFETY: `MaybeUninit<T>` has the same layout as `T`; the provider
    // initializes every element before this function returns successfully.
    let destination = unsafe {
        core::slice::from_raw_parts_mut(values.as_mut_ptr().cast::<MaybeUninit<T>>(), len)
    };
    memory
        .read_slice(ptr as *const T, destination)
        .map_err(map_usercopy_error)?;
    Ok(values)
}

/// Copies a byte range into the address space bound to this operation.
pub fn write_user_bytes<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: usize,
    bytes: &[u8],
) -> axerrno::AxResult<()> {
    memory.write_bytes(ptr, bytes).map_err(map_usercopy_error)
}
