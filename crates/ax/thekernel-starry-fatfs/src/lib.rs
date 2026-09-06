//! A FAT filesystem library implemented in Rust.
//!
//! # Usage
//!
//! This crate is [on crates.io](https://crates.io/crates/fatfs) and can be
//! used by adding `fatfs` to the dependencies in your project's `Cargo.toml`.
//!
//! ```toml
//! [dependencies]
//! fatfs = "0.4"
//! ```
//!
//! # Examples
//!
//! ```rust
//! use std::io::prelude::*;
//!
//! fn main() -> std::io::Result<()> {
//!     // Create a small in-memory volume for this example.
//!     let mut image = std::io::Cursor::new(vec![0u8; 1024 * 1024]);
//!     fatfs::format_volume(&mut fatfs::StdIoWrapper::new(&mut image), fatfs::FormatVolumeOptions::new())?;
//!     image.set_position(0);
//!     let buf_stream = fscommon::BufStream::new(image);
//!     let fs = fatfs::FileSystem::new(buf_stream, fatfs::FsOptions::new())?;
//!     let root_dir = fs.root_dir();
//!
//!     // Write a file
//!     root_dir.create_dir("foo")?;
//!     let mut file = root_dir.create_file("foo/hello.txt")?;
//!     file.truncate()?;
//!     file.write_all(b"Hello World!")?;
//!
//!     // Read a directory
//!     let dir = root_dir.open_dir("foo")?;
//!     for r in dir.iter() {
//!         let entry = r?;
//!         println!("{}", entry.file_name());
//!     }
//!     # Ok(())
//! }
//! ```

#![crate_type = "lib"]
#![cfg_attr(not(feature = "std"), no_std)]
// Disable warnings to not clutter code with cfg too much
#![cfg_attr(not(all(feature = "alloc", feature = "lfn")), allow(dead_code, unused_imports))]
#![warn(clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::cast_possible_truncation,
    clippy::bool_to_int_with_if, // less readable
    clippy::uninlined_format_args, // not supported before Rust 1.58.0
)]

extern crate log;

#[cfg(all(not(feature = "std"), feature = "alloc"))]
extern crate alloc;

#[macro_use]
mod log_macros;

mod boot_sector;
mod dir;
mod dir_entry;
mod error;
mod file;
mod fs;
mod io;
mod table;
mod time;

pub use crate::dir::*;
pub use crate::dir_entry::*;
pub use crate::error::*;
pub use crate::file::*;
pub use crate::fs::*;
pub use crate::io::*;
pub use crate::time::*;
