use super::ProtectionKeyState;

#[test]
fn pkey_zero_is_reserved_and_free_reuses_without_touching_other_keys() {
    let mut state = ProtectionKeyState::default();
    assert!(state.is_allocated(0));
    assert_eq!(state.free(0).unwrap_err(), axerrno::AxError::InvalidInput);
    assert_eq!(state.allocate().unwrap(), 1);
    assert_eq!(state.allocate().unwrap(), 2);
    state.free(1).unwrap();
    assert!(state.is_allocated(2));
    assert_eq!(state.allocate().unwrap(), 1);
}

#[test]
fn pkey_allocation_is_bounded_to_the_x86_sixteen_key_domain() {
    let mut state = ProtectionKeyState::default();
    for expected in 1..ProtectionKeyState::KEYS as u8 {
        assert_eq!(state.allocate().unwrap(), expected);
    }
    assert_eq!(state.allocate().unwrap_err(), axerrno::AxError::StorageFull);
    assert_eq!(state.free(16).unwrap_err(), axerrno::AxError::InvalidInput);
}
