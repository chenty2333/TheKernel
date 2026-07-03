/// The base address of the user space.
pub const USER_SPACE_BASE: usize = 0x1000;

/// The size of the user stack.
pub const USER_STACK_SIZE: usize = 0x800_000;

/// The lowest address of the user heap.
pub const USER_HEAP_BASE: usize = 0x4000_0000;
/// The size of the user heap.
pub const USER_HEAP_SIZE: usize = 0x1_0000;
/// The maximum size of the user heap (for brk expansion).
pub const USER_HEAP_SIZE_MAX: usize = 0x2000_0000;

/// The base address for user interpreter.
pub const USER_INTERP_BASE: usize = 0x400_0000;

/// The address of signal trampoline (placed at top of user heap).
pub const SIGNAL_TRAMPOLINE: usize = 0x6000_1000;
