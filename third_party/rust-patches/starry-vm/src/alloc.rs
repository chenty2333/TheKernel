extern crate alloc;

use alloc::vec::Vec;

use bytemuck::{AnyBitPattern, Pod, bytes_of, zeroed};

use crate::{VmError, VmImpl, VmIo, VmResult, vm_read_slice};

/// Loads a vector of elements from the virtual memory.
///
/// # Safety
///
/// The caller must ensure the memory pointed to by `ptr` is valid and
/// initialized.
pub unsafe fn vm_load_any<T>(ptr: *const T, len: usize) -> VmResult<Vec<T>> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(len).map_err(|_| VmError::NoMemory)?;
    vm_read_slice(ptr, &mut buf.spare_capacity_mut()[..len])?;
    // SAFETY: The caller guarantees that the memory is valid and initialized.
    unsafe { buf.set_len(len) }
    Ok(buf)
}

/// Loads a vector of elements from the virtual memory.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn vm_load<T: AnyBitPattern>(ptr: *const T, len: usize) -> VmResult<Vec<T>> {
    // SAFETY: `AnyBitPattern`
    unsafe { vm_load_any(ptr, len) }
}

#[inline]
fn is_zero<T: Pod>(value: &T) -> bool {
    bytes_of(value) == bytes_of(&zeroed::<T>())
}

const MAX_BYTES: usize = 131072;

fn element_address(base: usize, index: usize, size: usize) -> VmResult<usize> {
    index
        .checked_mul(size)
        .and_then(|offset| base.checked_add(offset))
        .ok_or(VmError::BadAddress)
}

fn chunk_elements(start: usize, size: usize, remaining: usize) -> usize {
    const CHUNK_SIZE: usize = 32;
    let bytes_to_boundary = CHUNK_SIZE - start % CHUNK_SIZE;
    (bytes_to_boundary / size).max(1).min(remaining)
}

/// Loads elements from the given pointer until a zero element is found.
pub fn vm_load_until_nul<T: Pod>(ptr: *const T) -> VmResult<Vec<T>> {
    if !ptr.is_aligned() {
        return Err(VmError::BadAddress);
    }

    let size = size_of::<T>();
    if size == 0 {
        return Err(VmError::BadAddress);
    }
    let max_elements = MAX_BYTES / size;
    if max_elements == 0 {
        return Err(VmError::TooLong);
    }
    let mut result = Vec::new();
    let mut vm = VmImpl::new();

    loop {
        if result.len() >= max_elements {
            return Err(VmError::TooLong);
        }
        let start = element_address(ptr.addr(), result.len(), size)?;
        let len = chunk_elements(start, size, max_elements - result.len());

        result.try_reserve(len).map_err(|_| VmError::NoMemory)?;
        let buf = &mut result.spare_capacity_mut()[..len];
        vm.read(start, buf.as_bytes_mut())?;

        // SAFETY: `vm.read` initialized the entire buffer and `Pod` allows
        // reinterpreting the bytes as `T`.
        let buf = unsafe { core::slice::from_raw_parts(buf.as_ptr().cast::<T>(), len) };
        let pos = buf.iter().position(is_zero);

        let initialized = result
            .len()
            .checked_add(pos.unwrap_or(len))
            .ok_or(VmError::TooLong)?;
        unsafe { result.set_len(initialized) };
        if result.len() >= max_elements {
            return Err(VmError::TooLong);
        }

        if pos.is_some() {
            break;
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{chunk_elements, element_address};
    use crate::VmError;

    #[test]
    fn element_address_rejects_user_pointer_overflow() {
        assert_eq!(element_address(usize::MAX - 3, 1, 8), Err(VmError::BadAddress));
    }

    #[test]
    fn chunk_length_is_nonzero_and_bounded() {
        assert_eq!(chunk_elements(0x1000, 1, 100), 32);
        assert_eq!(chunk_elements(0x101f, 8, 5), 1);
        assert_eq!(chunk_elements(0x1000, 64, 3), 1);
        assert_eq!(chunk_elements(0x1000, 1, 7), 7);
    }
}
