//! Explicit-context utilities for accessing userspace memory.
//!
//! This crate never selects an address space implicitly and never dereferences
//! a userspace pointer. The kernel adapter supplies a [`UserMemory`]
//! implementation and creates a [`UserMemoryContext`] for each operation.

#![no_std]
#![warn(missing_docs)]

#[cfg(feature = "alloc")]
extern crate alloc;

use core::{fmt, mem::MaybeUninit, slice};

use bytemuck::{NoUninit, Pod, Zeroable};

/// Errors produced before an operating-system adapter maps them to errno.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[non_exhaustive]
pub enum UserCopyError {
    /// The address is invalid, outside userspace, or overflows.
    BadAddress,
    /// The requested access is not permitted by the selected address space.
    AccessDenied,
    /// A bounded NUL-terminated read reached its maximum length.
    #[cfg(feature = "alloc")]
    TooLong,
    /// An owned snapshot could not reserve its required storage.
    #[cfg(feature = "alloc")]
    NoMemory,
}

impl fmt::Display for UserCopyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadAddress => f.write_str("invalid userspace address"),
            Self::AccessDenied => f.write_str("userspace access denied"),
            #[cfg(feature = "alloc")]
            Self::TooLong => f.write_str("bounded userspace value is too long"),
            #[cfg(feature = "alloc")]
            Self::NoMemory => f.write_str("usercopy snapshot allocation failed"),
        }
    }
}

/// A user-memory operation result.
pub type VmResult<T = ()> = Result<T, UserCopyError>;

/// Errors produced by a versioned structure copy.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum CopyStructError {
    /// The supplied user-memory operation failed.
    UserCopy(UserCopyError),
    /// A userspace extension contains a non-zero byte unknown to the kernel.
    NonZeroTrailing,
}

impl fmt::Display for CopyStructError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UserCopy(error) => error.fmt(f),
            Self::NonZeroTrailing => f.write_str("non-zero unknown structure extension"),
        }
    }
}

impl From<UserCopyError> for CopyStructError {
    fn from(error: UserCopyError) -> Self {
        Self::UserCopy(error)
    }
}

/// A versioned structure copy result.
pub type CopyStructResult<T> = Result<T, CopyStructError>;

/// An address-space provider used by one explicit usercopy operation.
///
/// # Safety
///
/// Implementations must not directly dereference `start` as a kernel pointer.
/// They must validate the full userspace range and access permissions. On
/// successful [`read`](UserMemory::read), every destination byte must have
/// been initialized. Returning an error may leave destination bytes partially
/// initialized because callers never observe them as initialized after an
/// error.
pub unsafe trait UserMemory {
    /// Reads exactly `dst.len()` bytes from `start`.
    fn read(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult;

    /// Writes exactly `src.len()` bytes to `start`.
    fn write(&mut self, start: usize, src: &[u8]) -> VmResult;

    /// Validates a complete writable user range without changing user memory.
    /// Providers that can distinguish mappings must override this; the
    /// conservative default retains compatibility for synthetic test memory.
    fn validate_write(&mut self, start: usize, len: usize) -> VmResult {
        checked_end(start, len)?;
        Ok(())
    }
}

// SAFETY: delegating through an exclusive reference preserves the underlying
// provider's safety contract and does not add another access path.
unsafe impl<M: UserMemory + ?Sized> UserMemory for &mut M {
    fn read(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult {
        (**self).read(start, dst)
    }

    fn write(&mut self, start: usize, src: &[u8]) -> VmResult {
        (**self).write(start, src)
    }

    fn validate_write(&mut self, start: usize, len: usize) -> VmResult {
        (**self).validate_write(start, len)
    }
}

/// Explicit operation context binding usercopy helpers to one provider.
///
/// Holding an exclusive provider reference makes accidental address-space
/// switching inside a multi-step copy impossible without constructing a new
/// context.
pub struct UserMemoryContext<'a, M: ?Sized> {
    memory: &'a mut M,
}

impl<'a, M: UserMemory + ?Sized> UserMemoryContext<'a, M> {
    /// Binds an operation context to `memory`.
    pub const fn new(memory: &'a mut M) -> Self {
        Self { memory }
    }

    /// Returns the provider for a lower-level adapter operation.
    pub fn memory_mut(&mut self) -> &mut M {
        self.memory
    }

    /// Validates the entire output extent before an operation starts copying.
    pub fn validate_write_range(&mut self, start: usize, len: usize) -> VmResult {
        self.memory.validate_write(start, len)
    }

    /// Reads a byte range after checked address arithmetic.
    pub fn read_bytes(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult {
        if dst.is_empty() {
            return Ok(());
        }
        checked_end(start, dst.len())?;
        self.memory.read(start, dst)
    }

    /// Writes a byte range after checked address arithmetic.
    pub fn write_bytes(&mut self, start: usize, src: &[u8]) -> VmResult {
        if src.is_empty() {
            return Ok(());
        }
        checked_end(start, src.len())?;
        self.memory.write(start, src)
    }

    /// Reads a typed slice without assuming that its bit patterns are valid.
    ///
    /// The userspace address need not satisfy `T`'s Rust alignment. Linux
    /// usercopy treats it as a byte address; only the kernel-owned destination
    /// is dereferenced as `T` after the provider initialized every byte.
    pub fn read_slice<T>(&mut self, ptr: *const T, dst: &mut [MaybeUninit<T>]) -> VmResult {
        let byte_len = core::mem::size_of_val(dst);
        if byte_len == 0 {
            return Ok(());
        }
        let start = ptr as usize;
        // SAFETY: `MaybeUninit<T>` may hold any byte pattern. The byte slice
        // covers exactly the same initialized-or-uninitialized storage.
        let bytes = unsafe {
            slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<MaybeUninit<u8>>(), byte_len)
        };
        self.read_bytes(start, bytes)
    }

    /// Writes a typed slice whose type has no uninitialized padding.
    #[allow(clippy::not_unsafe_ptr_arg_deref)] // Pointer is an opaque user address, never dereferenced.
    pub fn write_slice<T: NoUninit>(&mut self, ptr: *mut T, src: &[T]) -> VmResult {
        // SAFETY: `NoUninit` guarantees that every byte of each value,
        // including padding, is initialized and safe to copy.
        unsafe { self.write_slice_unchecked(ptr, src) }
    }

    /// Writes the complete object representation of a typed slice.
    ///
    /// # Safety
    ///
    /// Every byte in each source value, including padding, must be initialized.
    /// Prefer [`write_slice`](Self::write_slice) with `T: NoUninit`.
    pub unsafe fn write_slice_unchecked<T>(&mut self, ptr: *mut T, src: &[T]) -> VmResult {
        let byte_len = core::mem::size_of_val(src);
        if byte_len == 0 {
            return Ok(());
        }
        // The pointer is an opaque Linux userspace byte address. The source
        // slice is kernel-owned and aligned; this function never dereferences
        // `ptr` as `T`.
        let start = ptr as usize;
        // SAFETY: the caller guarantees that all bytes, including padding,
        // are initialized; the resulting slice has the exact object extent.
        let bytes = unsafe { slice::from_raw_parts(src.as_ptr().cast::<u8>(), byte_len) };
        self.write_bytes(start, bytes)
    }
}

fn checked_end(start: usize, len: usize) -> VmResult<usize> {
    start.checked_add(len).ok_or(UserCopyError::BadAddress)
}

/// Copies a versioned userspace structure into a zero-initialized value.
///
/// Short userspace structures leave the kernel value's suffix zeroed.  For a
/// larger userspace structure, the unknown extension must be all zero.  The
/// extension is read before the common prefix, preserving Linux's fault
/// precedence when both regions are inaccessible or the extension is invalid.
/// Callers retain syscall-specific pointer, minimum-size, and maximum-size
/// validation policy.
pub fn copy_struct_from_user<M: UserMemory + ?Sized, T: Pod + Zeroable>(
    memory: &mut UserMemoryContext<'_, M>,
    src: *const u8,
    user_size: usize,
) -> CopyStructResult<T> {
    let kernel_size = core::mem::size_of::<T>();
    let copied = kernel_size.min(user_size);
    let mut value = T::zeroed();

    if user_size > kernel_size {
        let mut offset = kernel_size;
        while offset < user_size {
            let count = (user_size - offset).min(32);
            let address = (src as usize)
                .checked_add(offset)
                .ok_or(UserCopyError::BadAddress)?;
            let mut trailing = [MaybeUninit::<u8>::uninit(); 32];
            memory.read_bytes(address, &mut trailing[..count])?;
            let nonzero = trailing[..count].iter().any(|byte| {
                // SAFETY: UserMemory::read initializes every requested byte
                // on success; the check is restricted to exactly that range.
                unsafe { byte.assume_init() != 0 }
            });
            if nonzero {
                return Err(CopyStructError::NonZeroTrailing);
            }
            offset += count;
        }
    }

    if copied != 0 {
        let bytes = bytemuck::bytes_of_mut(&mut value);
        // SAFETY: Pod covers the full initialized object representation.  The
        // user pointer remains opaque; only the kernel-owned destination is
        // reinterpreted as MaybeUninit bytes.
        let destination = unsafe {
            slice::from_raw_parts_mut(bytes.as_mut_ptr().cast::<MaybeUninit<u8>>(), copied)
        };
        memory.read_bytes(src as usize, destination)?;
    }
    Ok(value)
}

/// Copies a versioned kernel structure to userspace.
///
/// Unknown userspace tail bytes are cleared before the common prefix is
/// copied.  The returned flag reports whether a short userspace structure
/// omits a non-zero kernel byte.  Callers retain syscall-specific pointer,
/// minimum-size, and maximum-size validation policy.
pub fn copy_struct_to_user<M: UserMemory + ?Sized, T: Pod>(
    memory: &mut UserMemoryContext<'_, M>,
    dst: *mut u8,
    user_size: usize,
    source: &T,
) -> VmResult<bool> {
    let source = bytemuck::bytes_of(source);
    let kernel_size = source.len();
    let copied = kernel_size.min(user_size);

    if user_size > kernel_size {
        let tail_start = (dst as usize)
            .checked_add(kernel_size)
            .ok_or(UserCopyError::BadAddress)?;
        let zeroes = [0u8; 32];
        let mut offset = 0;
        while offset < user_size - kernel_size {
            let count = (user_size - kernel_size - offset).min(zeroes.len());
            let address = tail_start
                .checked_add(offset)
                .ok_or(UserCopyError::BadAddress)?;
            memory.write_bytes(address, &zeroes[..count])?;
            offset += count;
        }
    }

    let ignored_trailing =
        user_size < kernel_size && source[copied..].iter().any(|byte| *byte != 0);
    if copied != 0 {
        memory.write_bytes(dst as usize, &source[..copied])?;
    }
    Ok(ignored_trailing)
}

/// Reads a typed slice using an explicit operation context.
pub fn vm_read_slice<M: UserMemory + ?Sized, T>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const T,
    dst: &mut [MaybeUninit<T>],
) -> VmResult {
    memory.read_slice(ptr, dst)
}

/// Writes a typed slice using an explicit operation context.
pub fn vm_write_slice<M: UserMemory + ?Sized, T: NoUninit>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *mut T,
    src: &[T],
) -> VmResult {
    memory.write_slice(ptr, src)
}

/// Writes typed bytes whose complete object representation is initialized.
///
/// # Safety
///
/// Every source byte, including padding, must be initialized.
pub unsafe fn vm_write_slice_unchecked<M: UserMemory + ?Sized, T>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *mut T,
    src: &[T],
) -> VmResult {
    // SAFETY: forwarded from this function's caller.
    unsafe { memory.write_slice_unchecked(ptr, src) }
}

mod thin;
pub use thin::{VmMutPtr, VmPtr};

mod sigevent;
pub use sigevent::RawSigevent;

#[cfg(feature = "alloc")]
#[path = "alloc.rs"]
mod owned;
#[cfg(feature = "alloc")]
pub use owned::{
    MAX_NUL_SEARCH_BYTES, vm_load, vm_load_any, vm_load_any_until_nul,
    vm_load_any_until_nul_bounded, vm_load_until_nul, vm_load_until_nul_bounded,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_range_rejects_overflow_but_allows_empty_top_address() {
        assert_eq!(checked_end(usize::MAX, 1), Err(UserCopyError::BadAddress));
        assert_eq!(checked_end(usize::MAX, 0), Ok(usize::MAX));
    }
}
