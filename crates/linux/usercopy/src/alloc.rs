use alloc::vec::Vec;

use bytemuck::{AnyBitPattern, Pod};

use crate::{UserCopyError, UserMemory, UserMemoryContext, VmResult};

/// Maximum byte range inspected by a NUL-terminated owned read.
pub const MAX_NUL_SEARCH_BYTES: usize = 128 * 1024;

impl<M: UserMemory + ?Sized> UserMemoryContext<'_, M> {
    /// Loads `len` values into an owned vector without validating bit patterns.
    ///
    /// # Safety
    ///
    /// The caller must ensure that every copied representation is valid for
    /// `T` before observing the returned values.
    pub unsafe fn load_any<T>(&mut self, ptr: *const T, len: usize) -> VmResult<Vec<T>> {
        let mut values = Vec::new();
        values
            .try_reserve_exact(len)
            .map_err(|_| UserCopyError::NoMemory)?;
        self.read_slice(ptr, &mut values.spare_capacity_mut()[..len])?;
        // SAFETY: the provider initialized the requested storage and validity
        // is the responsibility of this function's caller.
        unsafe { values.set_len(len) };
        Ok(values)
    }

    /// Loads `len` values whose type accepts every bit pattern.
    #[allow(clippy::not_unsafe_ptr_arg_deref)] // Pointer is an opaque user address, never dereferenced.
    pub fn load<T: AnyBitPattern>(&mut self, ptr: *const T, len: usize) -> VmResult<Vec<T>> {
        // SAFETY: `AnyBitPattern` makes every initialized representation valid.
        unsafe { self.load_any(ptr, len) }
    }

    /// Loads representations until an all-zero value appears or the 128 KiB
    /// bound is hit.
    ///
    /// # Safety
    ///
    /// Every nonzero representation returned by the provider must be valid for
    /// `T`, and the all-zero representation must be a valid sentinel for `T`.
    /// This entry point supports raw userspace pointer arrays without enabling
    /// bytemuck's unsound raw-pointer `Pod` implementation.
    pub unsafe fn load_any_until_nul<T>(&mut self, ptr: *const T) -> VmResult<Vec<T>> {
        // SAFETY: this method has the same representation and sentinel
        // requirements as the bounded entry point.
        unsafe { self.load_any_until_nul_bounded(ptr, usize::MAX) }
    }

    /// Loads representations until an all-zero value appears within
    /// `scan_elements` elements.
    ///
    /// The scan budget includes the terminating zero element. The crate-wide
    /// 128 KiB byte ceiling remains authoritative when the requested budget
    /// is larger.
    ///
    /// # Safety
    ///
    /// Every nonzero representation returned by the provider must be valid for
    /// `T`, and the all-zero representation must be a valid sentinel for `T`.
    /// This entry point supports raw userspace pointer arrays without enabling
    /// bytemuck's unsound raw-pointer `Pod` implementation.
    pub unsafe fn load_any_until_nul_bounded<T>(
        &mut self,
        ptr: *const T,
        scan_elements: usize,
    ) -> VmResult<Vec<T>> {
        let size = core::mem::size_of::<T>();
        if size == 0 {
            return Err(UserCopyError::BadAddress);
        }
        let max_elements = scan_elements.min(MAX_NUL_SEARCH_BYTES / size);
        if max_elements == 0 {
            return Err(UserCopyError::TooLong);
        }

        let start = ptr as usize;
        let mut result = Vec::new();
        while result.len() < max_elements {
            let address = element_address(start, result.len(), size)?;
            let len = chunk_elements(address, size, max_elements - result.len());
            result
                .try_reserve_exact(len)
                .map_err(|_| UserCopyError::NoMemory)?;

            let old_len = result.len();
            let spare = &mut result.spare_capacity_mut()[..len];
            self.read_slice(address as *const T, spare)?;

            if let Some(position) = spare.iter().position(is_zero_uninit) {
                // SAFETY: all elements before the zero were initialized by the
                // provider and their validity is guaranteed by the caller.
                unsafe { result.set_len(old_len + position) };
                return Ok(result);
            }

            // SAFETY: the complete spare segment was initialized and validity
            // is guaranteed by the caller.
            unsafe { result.set_len(old_len + len) };
        }
        Err(UserCopyError::TooLong)
    }

    /// Loads `Pod` values until a zero value appears or the bound is hit.
    #[allow(clippy::not_unsafe_ptr_arg_deref)] // Pointer is an opaque user address, never dereferenced.
    pub fn load_until_nul<T: Pod>(&mut self, ptr: *const T) -> VmResult<Vec<T>> {
        self.load_until_nul_bounded(ptr, usize::MAX)
    }

    /// Loads `Pod` values until an all-zero value appears within
    /// `scan_elements` elements.
    ///
    /// The scan budget includes the terminating zero element. The crate-wide
    /// 128 KiB byte ceiling remains authoritative when the requested budget
    /// is larger.
    #[allow(clippy::not_unsafe_ptr_arg_deref)] // Pointer is an opaque user address, never dereferenced.
    pub fn load_until_nul_bounded<T: Pod>(
        &mut self,
        ptr: *const T,
        scan_elements: usize,
    ) -> VmResult<Vec<T>> {
        // SAFETY: `Pod` guarantees that every representation is valid and that
        // the all-zero representation is valid.
        unsafe { self.load_any_until_nul_bounded(ptr, scan_elements) }
    }
}

/// Loads an owned vector through an explicit context without bit validation.
///
/// # Safety
///
/// The copied representations must all be valid for `T`.
pub unsafe fn vm_load_any<M: UserMemory + ?Sized, T>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const T,
    len: usize,
) -> VmResult<Vec<T>> {
    // SAFETY: forwarded from this function's caller.
    unsafe { memory.load_any(ptr, len) }
}

/// Loads an owned vector through an explicit context.
pub fn vm_load<M: UserMemory + ?Sized, T: AnyBitPattern>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const T,
    len: usize,
) -> VmResult<Vec<T>> {
    memory.load(ptr, len)
}

/// Loads raw representations until an all-zero sentinel is found.
///
/// # Safety
///
/// Every nonzero representation must be valid for `T`, and all-zero must be a
/// valid sentinel. This is the migration path for raw userspace pointer arrays.
pub unsafe fn vm_load_any_until_nul<M: UserMemory + ?Sized, T>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const T,
) -> VmResult<Vec<T>> {
    // SAFETY: forwarded from this function's caller.
    unsafe { memory.load_any_until_nul(ptr) }
}

/// Loads raw representations until an all-zero sentinel is found within
/// `scan_elements` elements.
///
/// The scan budget includes the terminating zero element and is also limited
/// by the crate-wide 128 KiB byte ceiling.
///
/// # Safety
///
/// Every nonzero representation must be valid for `T`, and all-zero must be a
/// valid sentinel. This is the migration path for raw userspace pointer arrays.
pub unsafe fn vm_load_any_until_nul_bounded<M: UserMemory + ?Sized, T>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const T,
    scan_elements: usize,
) -> VmResult<Vec<T>> {
    // SAFETY: forwarded from this function's caller.
    unsafe { memory.load_any_until_nul_bounded(ptr, scan_elements) }
}

/// Loads a bounded NUL-terminated vector through an explicit context.
pub fn vm_load_until_nul<M: UserMemory + ?Sized, T: Pod>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const T,
) -> VmResult<Vec<T>> {
    memory.load_until_nul(ptr)
}

/// Loads a bounded NUL-terminated vector through an explicit context and an
/// element-count scan budget.
pub fn vm_load_until_nul_bounded<M: UserMemory + ?Sized, T: Pod>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const T,
    scan_elements: usize,
) -> VmResult<Vec<T>> {
    memory.load_until_nul_bounded(ptr, scan_elements)
}

fn is_zero_uninit<T>(value: &core::mem::MaybeUninit<T>) -> bool {
    let size = core::mem::size_of::<T>();
    // SAFETY: this helper is called only after a successful provider read,
    // which initialized every byte. Reading those bytes as u8 is valid.
    let bytes = unsafe { core::slice::from_raw_parts(value.as_ptr().cast::<u8>(), size) };
    bytes.iter().all(|byte| *byte == 0)
}

fn element_address(base: usize, index: usize, size: usize) -> VmResult<usize> {
    index
        .checked_mul(size)
        .and_then(|offset| base.checked_add(offset))
        .ok_or(UserCopyError::BadAddress)
}

fn chunk_elements(start: usize, size: usize, remaining: usize) -> usize {
    const CHUNK_BYTES: usize = 32;
    let bytes_to_boundary = CHUNK_BYTES - start % CHUNK_BYTES;
    (bytes_to_boundary / size).max(1).min(remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_math_rejects_overflow() {
        assert_eq!(
            element_address(usize::MAX - 3, 1, 8),
            Err(UserCopyError::BadAddress)
        );
    }

    #[test]
    fn chunks_are_nonzero_and_bounded() {
        assert_eq!(chunk_elements(0x1000, 1, 100), 32);
        assert_eq!(chunk_elements(0x101f, 8, 5), 1);
        assert_eq!(chunk_elements(0x1000, 64, 3), 1);
        assert_eq!(chunk_elements(0x1000, 1, 7), 7);
    }
}
