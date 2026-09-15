//! Scope-local resource storage.

#![no_std]
#![feature(allocator_api)]
#![warn(missing_docs)]

extern crate alloc;

mod boxed;
mod item;
mod scope;

pub use item::{Item, LocalItem, ScopeItem, ScopeItemMut};
pub use scope::{ActiveScope, Scope};
