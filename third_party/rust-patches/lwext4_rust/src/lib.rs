#![no_std]
#![feature(linkage)]
#![feature(c_variadic, c_size_t)]
#![feature(associated_type_defaults)]
#![feature(allocator_api)]

extern crate alloc;

#[macro_use]
extern crate log;

mod ulibc;

pub mod ffi {
    #![allow(non_upper_case_globals)]
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]

    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

mod blockdev;
mod error;
mod fs;
mod hot;
mod inode;
mod iomap;
mod util;

pub use blockdev::{
    AsyncReadStats, AsyncReadSubmission, AsyncWriteSubmission, BlockDevice, EXT4_DEV_BSIZE,
};
pub use error::{Ext4Error, Ext4Result};
pub use fs::*;
pub use hot::{
    IoCounters, async_mapped_read_enabled, io_counters_snapshot, record_readahead_async_pages,
    reset_io_counters, set_async_mapped_read_enabled, set_extent_status_cache_enabled,
    set_io_counters_enabled,
};
pub use inode::*;
