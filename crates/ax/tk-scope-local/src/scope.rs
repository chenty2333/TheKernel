use alloc::alloc::{AllocError, alloc, dealloc, handle_alloc_error};
use core::{alloc::Layout, iter::zip, mem::MaybeUninit, ptr::NonNull};

use spin::Lazy;

use crate::{
    boxed::ItemBox,
    item::{Item, Registry},
};

/// A collection of scope-local values.
pub struct Scope {
    ptr: NonNull<ItemBox>,
}

unsafe impl Send for Scope {}
unsafe impl Sync for Scope {}

impl Scope {
    fn try_layout() -> Result<Layout, AllocError> {
        Layout::array::<ItemBox>(Registry.len()).map_err(|_| AllocError)
    }

    fn layout_or_abort() -> Layout {
        Self::try_layout().unwrap_or_else(|_| handle_alloc_error(Layout::new::<ItemBox>()))
    }

    /// Creates a new scope, aborting on allocation failure.
    pub fn new() -> Self {
        Self::try_new().unwrap_or_else(|_| handle_alloc_error(Self::layout_or_abort()))
    }

    /// Fallibly creates a new scope and all registered ownership nodes.
    pub fn try_new() -> Result<Self, AllocError> {
        let layout = Self::try_layout()?;
        let ptr = NonNull::new(unsafe { alloc(layout) })
            .ok_or(AllocError)?
            .cast::<ItemBox>();
        let slice = unsafe {
            core::slice::from_raw_parts_mut(ptr.cast::<MaybeUninit<_>>().as_ptr(), Registry.len())
        };

        let mut initialized = 0;
        for (item, destination) in zip(&*Registry, &mut *slice) {
            match ItemBox::try_new(item) {
                Ok(value) => {
                    destination.write(value);
                    initialized += 1;
                }
                Err(err) => {
                    for item in &mut slice[..initialized] {
                        unsafe { item.assume_init_drop() };
                    }
                    unsafe { dealloc(ptr.cast().as_ptr(), layout) };
                    return Err(err);
                }
            }
        }
        Ok(Self { ptr })
    }

    pub(crate) fn get(&self, item: &'static Item) -> &ItemBox {
        unsafe { self.ptr.add(item.index()).as_ref() }
    }

    pub(crate) fn get_mut(&mut self, item: &'static Item) -> &mut ItemBox {
        unsafe { self.ptr.add(item.index()).as_mut() }
    }
}

impl Default for Scope {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let ptr = NonNull::slice_from_raw_parts(self.ptr, Registry.len());
        unsafe {
            ptr.drop_in_place();
            dealloc(self.ptr.cast().as_ptr(), Self::layout_or_abort());
        }
    }
}

static GLOBAL_SCOPE: Lazy<Scope> = Lazy::new(Scope::new);

#[percpu::def_percpu]
pub(crate) static ACTIVE_SCOPE_PTR: usize = 0;

/// Access to the scope currently bound on this CPU.
pub struct ActiveScope;

impl ActiveScope {
    /// Binds `scope` as active.
    ///
    /// # Safety
    ///
    /// The caller must keep `scope` alive and prevent aliased mutation while
    /// it remains active.
    pub unsafe fn set(scope: &Scope) {
        ACTIVE_SCOPE_PTR.write_current(scope.ptr.addr().into());
    }

    /// Restores the global scope.
    pub fn set_global() {
        ACTIVE_SCOPE_PTR.write_current(0);
    }

    /// Returns whether the global scope is active.
    pub fn is_global() -> bool {
        ACTIVE_SCOPE_PTR.read_current() == 0
    }

    pub(crate) fn get<'a>(item: &'static Item) -> &'a ItemBox {
        let ptr = ACTIVE_SCOPE_PTR.read_current();
        let ptr = NonNull::new(ptr as _).unwrap_or(GLOBAL_SCOPE.ptr);
        unsafe { ptr.add(item.index()).as_ref() }
    }
}
