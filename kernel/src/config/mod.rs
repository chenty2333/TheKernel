//! Architecture-specific configurations.

mod common;

#[rustfmt::skip]
mod x86_64;
pub use x86_64::*;
