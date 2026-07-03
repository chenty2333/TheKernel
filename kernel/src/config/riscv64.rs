pub use super::common::*;

/// The size of the kernel stack.
pub const KERNEL_STACK_SIZE: usize = 0x2_0000;

/// The size of the user space.
pub const USER_SPACE_SIZE: usize = 0x3f_ffff_f000;

/// The highest address of the user stack.
pub const USER_STACK_TOP: usize = 0x4_0000_0000;
