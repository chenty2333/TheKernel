pub use super::common::*;

/// The size of the kernel stack.
pub const KERNEL_STACK_SIZE: usize = 0x4_0000;

/// Linux's `TASK_SIZE_MAX` for the 4-level paging user address space.
///
/// `arch_prctl(ARCH_SET_FS)` / `(ARCH_SET_GS)` reject `arg2 >= TASK_SIZE_MAX`
/// with `-EPERM`, so this is an exclusive upper bound on a legal user segment
/// base and on user mappings. It is Linux's `(1UL << 47) - PAGE_SIZE`; the
/// final canonical user page is intentionally left unmapped.
pub const TASK_SIZE_MAX: usize = (1usize << 47) - 0x1000;

/// The size of the user address space `[USER_SPACE_BASE, TASK_SIZE_MAX)`.
pub const USER_SPACE_SIZE: usize = TASK_SIZE_MAX - USER_SPACE_BASE;

/// The highest address of the user stack.
pub const USER_STACK_TOP: usize = 0x7fff_0000_0000;

const _: () = assert!(USER_SPACE_BASE + USER_SPACE_SIZE == TASK_SIZE_MAX);
