use core::{
    alloc::Layout,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::{NonNull, addr_of},
};

use crate::scope::{ActiveScope, Scope};

/// Type-erased metadata for one scope-local value.
#[doc(hidden)]
pub struct Item {
    /// Layout of the stored value.
    pub layout: Layout,
    /// Initializes the stored value.
    pub init: fn(NonNull<()>),
    /// Drops the stored value.
    pub drop: fn(NonNull<()>),
}

pub(crate) struct Registry;

impl Deref for Registry {
    type Target = [Item];

    fn deref(&self) -> &Self::Target {
        unsafe extern "Rust" {
            static __start_scope_local: Item;
            static __stop_scope_local: Item;
        }
        let start = addr_of!(__start_scope_local) as usize;
        let len = (addr_of!(__stop_scope_local) as usize - start) / core::mem::size_of::<Item>();
        unsafe { core::slice::from_raw_parts(start as *const Item, len) }
    }
}

impl Item {
    #[inline]
    pub(crate) fn index(&'static self) -> usize {
        unsafe { (self as *const Item).offset_from_unsigned(Registry.as_ptr()) }
    }
}

/// A scope-local item.
pub struct LocalItem<T> {
    item: &'static Item,
    _p: PhantomData<T>,
}

impl<T> LocalItem<T> {
    /// Creates a typed handle from registered metadata.
    #[doc(hidden)]
    pub const fn new(item: &'static Item) -> Self {
        Self {
            item,
            _p: PhantomData,
        }
    }

    /// Returns a reference to this item within `scope`.
    pub fn scope<'scope>(&self, scope: &'scope Scope) -> ScopeItem<'scope, T> {
        ScopeItem {
            item: self.item,
            scope,
            _p: PhantomData,
        }
    }

    /// Returns a mutable reference to this item within `scope`.
    pub fn scope_mut<'scope>(&self, scope: &'scope mut Scope) -> ScopeItemMut<'scope, T> {
        ScopeItemMut {
            item: self.item,
            scope,
            _p: PhantomData,
        }
    }
}

impl<T> Deref for LocalItem<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        ActiveScope::get(self.item).as_ref()
    }
}

/// An immutable reference to a value in a specific scope.
pub struct ScopeItem<'scope, T> {
    item: &'static Item,
    scope: &'scope Scope,
    _p: PhantomData<T>,
}

impl<'scope, T> Deref for ScopeItem<'scope, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.scope.get(self.item).as_ref()
    }
}

/// A mutable reference to a value in a specific scope.
pub struct ScopeItemMut<'scope, T> {
    item: &'static Item,
    scope: &'scope mut Scope,
    _p: PhantomData<T>,
}

impl<'scope, T> Deref for ScopeItemMut<'scope, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.scope.get(self.item).as_ref()
    }
}

impl<'scope, T> DerefMut for ScopeItemMut<'scope, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.scope.get_mut(self.item).as_mut()
    }
}

/// Defines one or more scope-local values.
#[macro_export]
macro_rules! scope_local {
    ( $( $(#[$attr:meta])* $vis:vis static $name:ident: $ty:ty = $default:expr; )+ ) => {
        $(
            $(#[$attr])*
            $vis static $name: $crate::LocalItem<$ty> = {
                #[unsafe(link_section = "scope_local")]
                static ITEM: $crate::Item = $crate::Item {
                    layout: core::alloc::Layout::new::<$ty>(),
                    init: |ptr| {
                        let val: $ty = $default;
                        unsafe { ptr.cast().write(val) }
                    },
                    drop: |ptr| unsafe {
                        ptr.cast::<$ty>().drop_in_place();
                    },
                };
                $crate::LocalItem::new(&ITEM)
            };
        )+
    }
}
