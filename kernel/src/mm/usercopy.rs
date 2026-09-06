//! Explicit userspace providers for address-space-bound usercopy operations.

use alloc::sync::Arc;
use core::{mem::MaybeUninit, slice};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::paging::MappingFlags;
use axsync::Mutex;
use bytemuck::{NoUninit, Pod, Zeroable};
use memory_addr::{MemoryAddr, VirtAddr};
use thekernel_linux_usercopy::{
    CopyStructError, UserCopyError, UserMemory, UserMemoryContext, VmResult,
    copy_struct_from_user as copy_struct_from_user_context,
    copy_struct_to_user as copy_struct_to_user_context, vm_load_until_nul,
    vm_load_until_nul_bounded,
};

use super::AddrSpace;
use crate::task::AsThread;

/// A user-memory provider bound to one explicitly selected address space.
pub(crate) struct AddressSpaceUserMemory {
    address_space: Arc<Mutex<AddrSpace>>,
}

/// Capability for accessing one explicitly selected userspace address space.
///
/// The capability is captured at syscall entry and is cloneable so synchronous
/// I/O objects can retain the selection while they are passed through an
/// object-safe `FileLike`/`axio` call.  It intentionally contains no task or
/// `current()` reference: every operation constructs a short-lived provider
/// and [`UserMemoryContext`] from this handle.
#[derive(Clone)]
pub struct UserMemoryCapability {
    address_space: Arc<Mutex<AddrSpace>>,
}

impl UserMemoryCapability {
    /// Binds a capability to the supplied address-space handle.
    pub fn new(address_space: Arc<Mutex<AddrSpace>>) -> Self {
        Self { address_space }
    }

    /// Returns the selected address-space handle for MM operations such as
    /// direct-I/O pinning.  The handle is still explicit; callers must not
    /// replace it with an address space obtained from `current()`.
    pub fn address_space(&self) -> &Arc<Mutex<AddrSpace>> {
        &self.address_space
    }

    /// Runs one operation with a fresh provider and user-memory context.
    pub fn with_memory<T>(
        &self,
        operation: impl for<'a> FnOnce(&mut UserMemoryContext<'a, AddressSpaceUserMemory>) -> T,
    ) -> T {
        with_user_memory(self.clone(), operation)
    }

    /// Reads an opaque byte range from this capability.
    pub fn read_bytes(&self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult {
        self.with_memory(|memory| memory.read_bytes(start, dst))
    }

    /// Writes an opaque byte range to this capability.
    pub fn write_bytes(&self, start: usize, src: &[u8]) -> VmResult {
        self.with_memory(|memory| memory.write_bytes(start, src))
    }

    /// Reads a typed value without dereferencing the userspace pointer.
    pub fn read_value<T: bytemuck::AnyBitPattern>(&self, ptr: *const T) -> VmResult<T> {
        self.with_memory(|memory| {
            let mut value = MaybeUninit::<T>::uninit();
            memory.read_slice(ptr, slice::from_mut(&mut value))?;
            // SAFETY: the context initialized the complete object and
            // `AnyBitPattern` makes every representation valid.
            Ok(unsafe { value.assume_init() })
        })
    }

    /// Reads an unaligned typed value without assuming its bit pattern.
    pub fn read_value_uninit<T>(&self, ptr: *const T) -> VmResult<MaybeUninit<T>> {
        self.with_memory(|memory| {
            let mut value = MaybeUninit::<T>::uninit();
            memory.read_slice(ptr, slice::from_mut(&mut value))?;
            Ok(value)
        })
    }

    /// Writes a typed value whose complete representation is initialized.
    pub fn write_value<T: NoUninit>(&self, ptr: *mut T, value: T) -> VmResult {
        self.with_memory(|memory| memory.write_slice(ptr, slice::from_ref(&value)))
    }

    /// Writes a typed value with an audited complete object representation.
    ///
    /// # Safety
    ///
    /// The caller must ensure that every byte of `value`, including padding,
    /// is initialized.
    pub unsafe fn write_value_unchecked<T>(&self, ptr: *mut T, value: T) -> VmResult {
        self.with_memory(|memory| {
            // SAFETY: forwarded from this function's caller.
            unsafe { memory.write_slice_unchecked(ptr, slice::from_ref(&value)) }
        })
    }

    /// Reads a typed slice through a fresh user-memory context.
    pub fn read_slice<T>(&self, ptr: *const T, dst: &mut [MaybeUninit<T>]) -> VmResult {
        self.with_memory(|memory| memory.read_slice(ptr, dst))
    }

    /// Writes a typed slice whose complete representations are initialized.
    pub fn write_slice<T: NoUninit>(&self, ptr: *mut T, src: &[T]) -> VmResult {
        self.with_memory(|memory| memory.write_slice(ptr, src))
    }

    /// Writes an audited typed slice with arbitrary element types.
    ///
    /// # Safety
    ///
    /// Every byte in each source value, including padding, must be initialized.
    pub unsafe fn write_slice_unchecked<T>(&self, ptr: *mut T, src: &[T]) -> VmResult {
        self.with_memory(|memory| {
            // SAFETY: forwarded from this function's caller.
            unsafe { memory.write_slice_unchecked(ptr, src) }
        })
    }

    /// Loads a NUL-terminated typed vector through a fresh context.
    pub fn load_until_nul<T: bytemuck::Pod>(&self, ptr: *const T) -> VmResult<alloc::vec::Vec<T>> {
        self.with_memory(|memory| vm_load_until_nul(memory, ptr))
    }

    /// Loads a bounded NUL-terminated typed vector through a fresh context.
    pub fn load_until_nul_bounded<T: bytemuck::Pod>(
        &self,
        ptr: *const T,
        scan_elements: usize,
    ) -> VmResult<alloc::vec::Vec<T>> {
        self.with_memory(|memory| vm_load_until_nul_bounded(memory, ptr, scan_elements))
    }
}

impl From<Arc<Mutex<AddrSpace>>> for UserMemoryCapability {
    fn from(address_space: Arc<Mutex<AddrSpace>>) -> Self {
        Self::new(address_space)
    }
}

impl AddressSpaceUserMemory {
    pub(crate) fn new(address_space: Arc<Mutex<AddrSpace>>) -> Self {
        Self { address_space }
    }

    fn targets_current_address_space(&self) -> bool {
        axtask::current_may_uninit().is_some_and(|current| {
            current
                .try_as_thread()
                .is_some_and(|thread| Arc::ptr_eq(&self.address_space, &thread.proc_data.aspace()))
        })
    }
}

/// Runs one usercopy operation against the explicitly supplied address space.
///
/// A provider and operation context are created for this call only; no
/// current-task or global address-space state is consulted or cached.
pub(crate) fn with_user_memory<T>(
    capability: impl Into<UserMemoryCapability>,
    operation: impl for<'a> FnOnce(&mut UserMemoryContext<'a, AddressSpaceUserMemory>) -> T,
) -> T {
    let capability = capability.into();
    let mut provider = AddressSpaceUserMemory::new(capability.address_space.clone());
    let mut memory = UserMemoryContext::new(&mut provider);
    operation(&mut memory)
}

/// Copies a versioned userspace structure through the selected user-memory
/// context.
///
/// Syscall-specific minimum-size and `PAGE_SIZE` checks remain the caller's
/// responsibility.  The underlying helper checks an oversized userspace tail
/// before copying the common prefix, so a readable non-zero extension yields
/// `E2BIG` even when the known prefix would fault; a fault in the extension
/// itself remains `EFAULT`.
pub(crate) fn copy_struct_from_user<M: UserMemory + ?Sized, T: Pod + Zeroable>(
    memory: &mut UserMemoryContext<'_, M>,
    source: *const u8,
    user_size: usize,
) -> AxResult<T> {
    copy_struct_from_user_context(memory, source, user_size).map_err(|error| match error {
        CopyStructError::UserCopy(_) => AxError::BadAddress,
        CopyStructError::NonZeroTrailing => LinuxError::E2BIG.into(),
    })
}

/// Copies a versioned kernel structure through the selected user-memory
/// context and reports whether a smaller userspace structure hid non-zero
/// kernel fields.
///
/// When userspace supplies a larger structure, its unknown tail is cleared
/// before the common prefix is copied.  Callers retain ABI size policy.
pub(crate) fn copy_struct_to_user<M: UserMemory + ?Sized, T: Pod>(
    memory: &mut UserMemoryContext<'_, M>,
    destination: *mut u8,
    user_size: usize,
    source: &T,
) -> AxResult<bool> {
    copy_struct_to_user_context(memory, destination, user_size, source)
        .map_err(|_| AxError::BadAddress)
}

fn map_address_space_error(error: AxError) -> UserCopyError {
    match error {
        AxError::NoMemory => UserCopyError::NoMemory,
        AxError::BadAddress | AxError::InvalidInput => UserCopyError::BadAddress,
        _ => UserCopyError::AccessDenied,
    }
}

/// Populate and use a user range under one mapping snapshot. Cache replacement
/// must run outside the mm lock because eviction listeners acquire that lock.
pub(super) fn with_populated_user_range<T>(
    handle: &Arc<Mutex<AddrSpace>>,
    start: usize,
    len: usize,
    access_flags: MappingFlags,
    mut operation: impl FnMut(&mut AddrSpace, VirtAddr) -> AxResult<T>,
) -> VmResult<T> {
    let start = VirtAddr::from(start);
    for _ in 0..64 {
        let caches = {
            let mut address_space = handle.lock();
            if len == 0 {
                return operation(&mut address_space, start).map_err(map_address_space_error);
            }
            if !address_space.contains_range(start, len) {
                return Err(UserCopyError::BadAddress);
            }
            if !address_space.can_access_range(start, len, access_flags) {
                return Err(UserCopyError::AccessDenied);
            }
            let end = start.checked_add(len).ok_or(UserCopyError::BadAddress)?;
            let page_start = start.align_down_4k();
            let page_end = VirtAddr::from(
                super::checked_align_up_4k(end.as_usize()).ok_or(UserCopyError::BadAddress)?,
            );
            let size = page_end.sub_addr(page_start);
            match address_space.populate_area(page_start, size, access_flags) {
                Ok(()) => {
                    return operation(&mut address_space, start).map_err(map_address_space_error);
                }
                Err(error) if error.canonicalize() == AxError::ResourceBusy => address_space
                    .file_caches_for_population_retry(page_start, size)
                    .map_err(map_address_space_error)?,
                Err(error) => return Err(map_address_space_error(error)),
            }
        };
        let mut reclaimed = false;
        for cache in caches {
            reclaimed |= cache.reclaim_one().map_err(map_address_space_error)?;
        }
        if !reclaimed {
            return Err(UserCopyError::NoMemory);
        }
    }
    Err(UserCopyError::NoMemory)
}

// SAFETY: all accesses are range-checked against the explicitly selected
// address space, and successful reads initialize every destination byte.
unsafe impl UserMemory for AddressSpaceUserMemory {
    fn read(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult {
        let current = self.targets_current_address_space();
        with_populated_user_range(
            &self.address_space,
            start,
            dst.len(),
            MappingFlags::READ,
            |address_space, start| {
                if !current && address_space.has_secret_mapping(start, dst.len()) {
                    return Err(AxError::BadAddress);
                }
                // Preserve the caller's buffer when validation or population
                // fails, but initialize it before constructing a byte slice.
                for byte in dst.iter_mut() {
                    byte.write(0);
                }
                // SAFETY: `MaybeUninit<u8>` has the same layout as `u8`;
                // every byte was initialized above.
                let dst =
                    unsafe { slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<u8>(), dst.len()) };
                if current {
                    address_space.current_uaccess_read(start, dst)
                } else {
                    address_space.read(start, dst)
                }
            },
        )
    }

    fn write(&mut self, start: usize, src: &[u8]) -> VmResult {
        let current = self.targets_current_address_space();
        with_populated_user_range(
            &self.address_space,
            start,
            src.len(),
            MappingFlags::WRITE,
            |address_space, start| {
                if current {
                    address_space.current_uaccess_write(start, src)
                } else if address_space.has_secret_mapping(start, src.len()) {
                    Err(AxError::BadAddress)
                } else {
                    address_space.write(start, src)
                }
            },
        )
    }

    fn validate_write(&mut self, start: usize, len: usize) -> VmResult {
        let current = self.targets_current_address_space();
        let address_space = self.address_space.lock();
        let start = VirtAddr::from(start);
        if len != 0
            && (!address_space.contains_range(start, len)
                || (!current && address_space.has_secret_mapping(start, len))
                || !address_space.can_access_range(start, len, MappingFlags::WRITE))
        {
            return Err(UserCopyError::BadAddress);
        }
        Ok(())
    }
}

pub(crate) fn map_usercopy_error(error: UserCopyError) -> AxError {
    match error {
        UserCopyError::BadAddress | UserCopyError::AccessDenied => AxError::BadAddress,
        UserCopyError::TooLong => AxError::NameTooLong,
        UserCopyError::NoMemory => AxError::NoMemory,
        _ => AxError::BadAddress,
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::mem::MaybeUninit;

    use axhal::paging::{MappingFlags, PageSize};
    use axsync::Mutex;
    use memory_addr::{PAGE_SIZE_4K, VirtAddr};
    use thekernel_linux_usercopy::{UserCopyError, UserMemoryContext};

    use super::{
        AddrSpace, AddressSpaceUserMemory, AxError, UserMemoryCapability, map_address_space_error,
        map_usercopy_error, with_user_memory,
    };

    #[test]
    fn provider_scope_uses_the_supplied_address_space() {
        let first = Arc::new(Mutex::new(
            AddrSpace::new_empty(VirtAddr::from(0x1000), 0x1000).unwrap(),
        ));
        let second = Arc::new(Mutex::new(
            AddrSpace::new_empty(VirtAddr::from(0x4000), 0x1000).unwrap(),
        ));

        let mut first_byte = [MaybeUninit::new(0xa5); 1];
        let first_result =
            with_user_memory(first, |memory| memory.read_bytes(0x1000, &mut first_byte));
        assert_eq!(first_result, Err(UserCopyError::AccessDenied));
        // SAFETY: the destination was initialized before the rejected read.
        assert_eq!(unsafe { first_byte[0].assume_init() }, 0xa5);

        let mut second_byte = [MaybeUninit::new(0x5a); 1];
        let second_result =
            with_user_memory(second, |memory| memory.read_bytes(0x1000, &mut second_byte));
        assert_eq!(second_result, Err(UserCopyError::BadAddress));
        // SAFETY: the destination was initialized before the rejected read.
        assert_eq!(unsafe { second_byte[0].assume_init() }, 0x5a);
    }

    #[test]
    fn provider_can_be_constructed_for_two_explicit_address_spaces() {
        let first = Arc::new(Mutex::new(
            AddrSpace::new_empty(VirtAddr::from(0x1000), 0x1000).unwrap(),
        ));
        let second = Arc::new(Mutex::new(
            AddrSpace::new_empty(VirtAddr::from(0x4000), 0x1000).unwrap(),
        ));

        let mut first_provider = AddressSpaceUserMemory::new(first);
        let mut second_provider = AddressSpaceUserMemory::new(second);
        let _first_context = UserMemoryContext::new(&mut first_provider);
        let _second_context = UserMemoryContext::new(&mut second_provider);
    }

    #[test]
    fn capability_reads_the_selected_address_space() {
        let mut selected = AddrSpace::new_empty(VirtAddr::from(0x1000), PAGE_SIZE_4K * 2).unwrap();
        selected
            .map(
                VirtAddr::from(0x1000),
                PAGE_SIZE_4K,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                false,
                super::super::Backend::new_alloc(VirtAddr::from(0x1000), PageSize::Size4K),
            )
            .unwrap();
        let selected = Arc::new(Mutex::new(selected));
        let other = Arc::new(Mutex::new(
            AddrSpace::new_empty(VirtAddr::from(0x1000), PAGE_SIZE_4K * 2).unwrap(),
        ));

        let capability = UserMemoryCapability::new(selected);
        capability.write_bytes(0x1000, &[0x5a]).unwrap();
        let mut value = [MaybeUninit::<u8>::uninit()];
        capability.read_bytes(0x1000, &mut value).unwrap();
        // SAFETY: the capability read initialized the complete byte.
        assert_eq!(unsafe { value[0].assume_init() }, 0x5a);

        // A second address-space handle covers the same virtual range but has
        // no mapping. This is the current-space stand-in: the capability must
        // continue using its captured selection instead of reselecting it.
        let other = UserMemoryCapability::new(other);
        let mut ignored = [MaybeUninit::<u8>::uninit()];
        assert_eq!(
            other.read_bytes(0x1000, &mut ignored),
            Err(UserCopyError::AccessDenied)
        );
        capability.read_bytes(0x1000, &mut ignored).unwrap();
        // SAFETY: the second read also initialized the byte.
        assert_eq!(unsafe { ignored[0].assume_init() }, 0x5a);
    }

    #[test]
    fn provider_reads_readonly_source_and_rejects_write() {
        let mut selected = AddrSpace::new_empty(VirtAddr::from(0x1000), PAGE_SIZE_4K).unwrap();
        selected
            .map(
                VirtAddr::from(0x1000),
                PAGE_SIZE_4K,
                MappingFlags::USER | MappingFlags::READ,
                false,
                super::super::Backend::new_alloc(VirtAddr::from(0x1000), PageSize::Size4K),
            )
            .unwrap();
        let capability = UserMemoryCapability::new(Arc::new(Mutex::new(selected)));
        let mut value = [MaybeUninit::<u8>::uninit()];
        capability.read_bytes(0x1000, &mut value).unwrap();
        assert_eq!(
            capability.write_bytes(0x1000, &[1]),
            Err(UserCopyError::AccessDenied)
        );
    }

    #[test]
    fn provider_range_and_address_space_errors_map_to_usercopy_errors() {
        assert_eq!(
            map_address_space_error(AxError::NoMemory),
            UserCopyError::NoMemory
        );
        assert_eq!(
            map_address_space_error(AxError::BadAddress),
            UserCopyError::BadAddress
        );
        assert_eq!(
            map_address_space_error(AxError::InvalidInput),
            UserCopyError::BadAddress
        );
        assert_eq!(
            map_address_space_error(AxError::PermissionDenied),
            UserCopyError::AccessDenied
        );

        assert_eq!(
            map_usercopy_error(UserCopyError::BadAddress),
            AxError::BadAddress
        );
        assert_eq!(
            map_usercopy_error(UserCopyError::AccessDenied),
            AxError::BadAddress
        );
        assert_eq!(
            map_usercopy_error(UserCopyError::TooLong),
            AxError::NameTooLong
        );
        assert_eq!(
            map_usercopy_error(UserCopyError::NoMemory),
            AxError::NoMemory
        );
    }

    #[test]
    fn empty_usercopy_ranges_do_not_require_address_space_access() {
        let address_space = Arc::new(Mutex::new(
            AddrSpace::new_empty(VirtAddr::from(0x1000), 0x1000).unwrap(),
        ));
        let result = with_user_memory(address_space, |memory| {
            memory.read_bytes(usize::MAX, &mut [])
        });
        assert_eq!(result, Ok(()));
    }
}
