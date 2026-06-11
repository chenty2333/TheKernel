mod brk;
mod mincore;
mod mmap;
mod process_vm;
mod swap;

pub use self::{brk::*, mincore::*, mmap::*, process_vm::*, swap::*};
