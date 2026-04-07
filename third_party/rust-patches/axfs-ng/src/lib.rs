//! ArceOS filesystem module.
//!
//! Provides high-level filesystem operations built on top of the VFS layer,
//! including file I/O with page caching, directory traversal, and
//! `std::fs`-like APIs.

#![cfg_attr(all(not(test), not(doc)), no_std)]
#![feature(doc_cfg)]
#![allow(clippy::new_ret_no_self)]

extern crate alloc;

#[macro_use]
extern crate log;

use axdriver::{AxBlockDevice, AxDeviceContainer, prelude::*};
use axsync::Mutex;
use spin::Once;

use alloc::{format, string::String, vec::Vec};

mod fs;

mod highlevel;
pub use highlevel::*;

struct RegisteredBlockDevice {
    name: String,
    device: Option<AxBlockDevice>,
}

pub enum OpenBlockDeviceError {
    NotFound,
    Busy,
}

static EXTRA_BLOCK_DEVICES: Once<Mutex<Vec<RegisteredBlockDevice>>> = Once::new();

fn extra_device_name(index: usize) -> String {
    let letter = (b'a' + index as u8) as char;
    format!("vd{letter}")
}

pub fn block_device_names() -> Vec<String> {
    EXTRA_BLOCK_DEVICES
        .get()
        .map(|devices| {
            devices
                .lock()
                .iter()
                .map(|entry| entry.name.clone())
                .collect()
        })
        .unwrap_or_default()
}

pub fn open_block_device(name: &str) -> Result<AxBlockDevice, OpenBlockDeviceError> {
    let devices = EXTRA_BLOCK_DEVICES
        .get()
        .ok_or(OpenBlockDeviceError::NotFound)?;
    let mut devices = devices.lock();
    let entry = devices
        .iter_mut()
        .find(|entry| entry.name == name)
        .ok_or(OpenBlockDeviceError::NotFound)?;
    entry.device.take().ok_or(OpenBlockDeviceError::Busy)
}

pub fn new_block_filesystem(
    fs_type: &str,
    dev: AxBlockDevice,
) -> axfs_ng_vfs::VfsResult<axfs_ng_vfs::Filesystem> {
    fs::new_named(fs_type, dev)
}

/// Initializes the filesystem subsystem using the first available block device.
pub fn init_filesystems(mut block_devs: AxDeviceContainer<AxBlockDevice>) {
    info!("Initialize filesystem subsystem...");
    debug!("axfs init_filesystems: detected {} block device(s)", block_devs.len());
    for (index, dev) in block_devs.iter().enumerate() {
        debug!(
            "axfs init_filesystems: block[{index}] device_name={}, blocks={}",
            dev.device_name(),
            dev.num_blocks()
        );
    }

    assert!(!block_devs.is_empty(), "No block device found!");
    let root_index = block_devs
        .iter()
        .enumerate()
        .max_by_key(|(_, dev)| dev.num_blocks())
        .map(|(index, _)| index)
        .expect("No block device found!");
    let dev = block_devs.remove(root_index);
    info!(
        "  use block device {root_index}: {:?} (blocks={})",
        dev.device_name(),
        dev.num_blocks()
    );

    let fs = fs::new_default(dev).expect("Failed to initialize filesystem");
    info!("  filesystem type: {:?}", fs.name());

    let mp = axfs_ng_vfs::Mountpoint::new_root(&fs);
    ROOT_FS_CONTEXT.call_once(|| FsContext::new(mp.root_location()));

    let mut extras = Vec::new();
    let mut index = 1;
    while !block_devs.is_empty() {
        let dev = block_devs.remove(0);
        debug!(
            "axfs init_filesystems: registering extra block device {} as {}",
            dev.device_name(),
            extra_device_name(index)
        );
        extras.push(RegisteredBlockDevice {
            name: extra_device_name(index),
            device: Some(dev),
        });
        index += 1;
    }
    EXTRA_BLOCK_DEVICES.call_once(|| Mutex::new(extras));
}
