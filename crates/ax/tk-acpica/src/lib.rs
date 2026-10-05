//! ACPICA 20260930, compiled freestanding from unchanged upstream C.
#![no_std]
unsafe extern "C" { fn tk_acpi_abi_width() -> u32; }
#[cfg(test)] mod tests {
    #[test] fn c_size_matches_rust() {
        // SAFETY: constant, side-effect-free C ABI query.
        assert_eq!(unsafe { super::tk_acpi_abi_width() }, usize::BITS);
    }
}
