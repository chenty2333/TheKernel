use core::{mem::MaybeUninit, ptr::NonNull, slice};

use bytemuck::{AnyBitPattern, NoUninit};

use crate::{UserMemory, UserMemoryContext, VmResult};

/// Extension methods for typed userspace pointers.
pub trait VmPtr: Copy {
    /// The pointed-to type.
    type Target;

    #[doc(hidden)]
    fn as_ptr(self) -> *const Self::Target;

    /// Returns `None` for a null pointer and `Some(self)` otherwise.
    fn nullable(self) -> Option<Self> {
        if self.as_ptr().is_null() {
            None
        } else {
            Some(self)
        }
    }

    /// Reads a value without assuming that its bit pattern is valid.
    fn vm_read_uninit<M: UserMemory + ?Sized>(
        self,
        memory: &mut UserMemoryContext<'_, M>,
    ) -> VmResult<MaybeUninit<Self::Target>> {
        let mut value = MaybeUninit::<Self::Target>::uninit();
        memory.read_slice(self.as_ptr(), slice::from_mut(&mut value))?;
        Ok(value)
    }

    /// Reads a value whose type accepts every bit pattern.
    fn vm_read<M: UserMemory + ?Sized>(
        self,
        memory: &mut UserMemoryContext<'_, M>,
    ) -> VmResult<Self::Target>
    where
        Self::Target: AnyBitPattern,
    {
        let value = self.vm_read_uninit(memory)?;
        // SAFETY: the provider initialized every byte and `AnyBitPattern`
        // makes every resulting representation valid.
        Ok(unsafe { value.assume_init() })
    }
}

impl<T> VmPtr for *const T {
    type Target = T;

    fn as_ptr(self) -> *const T {
        self
    }
}

impl<T> VmPtr for *mut T {
    type Target = T;

    fn as_ptr(self) -> *const T {
        self
    }
}

impl<T> VmPtr for NonNull<T> {
    type Target = T;

    fn as_ptr(self) -> *const T {
        NonNull::as_ptr(self).cast_const()
    }
}

/// Extension methods for mutable typed userspace pointers.
pub trait VmMutPtr: VmPtr {
    /// Writes a value whose type has no uninitialized padding.
    fn vm_write<M: UserMemory + ?Sized>(
        self,
        memory: &mut UserMemoryContext<'_, M>,
        value: Self::Target,
    ) -> VmResult
    where
        Self::Target: NoUninit,
    {
        memory.write_slice(self.as_ptr().cast_mut(), slice::from_ref(&value))
    }

    /// Writes a value whose complete object representation is initialized.
    ///
    /// # Safety
    ///
    /// Every byte in `value`, including padding, must be initialized.
    unsafe fn vm_write_unchecked<M: UserMemory + ?Sized>(
        self,
        memory: &mut UserMemoryContext<'_, M>,
        value: Self::Target,
    ) -> VmResult {
        // SAFETY: forwarded from this function's caller.
        unsafe { memory.write_slice_unchecked(self.as_ptr().cast_mut(), slice::from_ref(&value)) }
    }
}

impl<T> VmMutPtr for *mut T {}
impl<T> VmMutPtr for NonNull<T> {}
