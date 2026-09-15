//! x86 user-mode I/O-port permission management.

use super::gdt;

/// Size of the x86 I/O-permission bitmap, in bytes.
pub const IO_BITMAP_BYTES: usize = gdt::IO_BITMAP_BYTES;

/// Installs one task's permission map into the current CPU's TSS.
///
/// `allow_all` is the Linux `iopl(3)` emulation mode. It intentionally uses
/// an all-zero bitmap instead of elevating RFLAGS.IOPL, because real IOPL=3
/// would also allow user mode to execute CLI and STI.
///
/// Call only from the final IRQ-disabled user-return path.
pub fn install_user_io_bitmap(
    bitmap: Option<&[u8; IO_BITMAP_BYTES]>,
    revoked: Option<&[u8; IO_BITMAP_BYTES]>,
    allow_all: bool,
) {
    gdt::install_user_io_bitmap(bitmap, revoked, allow_all)
}
