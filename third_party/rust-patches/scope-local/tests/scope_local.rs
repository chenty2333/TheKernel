#![feature(allocator_api)]

use scope_local::{Scope, scope_local};

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
