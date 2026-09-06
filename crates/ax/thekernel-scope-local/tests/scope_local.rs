#![feature(allocator_api)]

use thekernel_scope_local::{Scope, scope_local};

#[percpu::def_percpu]
static INITIALIZED_WORD: usize = 0x1234_abcd;

#[percpu::def_percpu]
static INITIALIZED_WEAK: std::sync::Weak<()> = std::sync::Weak::new();

#[test]
fn host_percpu_preserves_nonzero_static_initializers() {
    assert_eq!(INITIALIZED_WORD.read_current(), 0x1234_abcd);
    // Weak::new() has a nonzero sentinel. Zeroing this initializer makes
    // the first assignment drop an invalid Weak, as in scheduler switches.
    let slot = unsafe { INITIALIZED_WEAK.current_ref_mut_raw() };
    assert_eq!(slot.as_ptr(), std::sync::Weak::<()>::new().as_ptr());
    drop(std::mem::replace(slot, std::sync::Weak::new()));
}

scope_local! {
    static VALUE: usize = 7;
}

#[test]
fn fallible_scope_initializes_registered_items() {
    let mut scope = Scope::try_new().unwrap();
    assert_eq!(*VALUE.scope(&scope), 7);
    *VALUE.scope_mut(&mut scope) = 11;
    assert_eq!(*VALUE.scope(&scope), 11);
}
