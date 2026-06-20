use alloc::{string::String, vec::Vec};
use core::ffi::c_char;

use axerrno::{AxError, AxResult, LinuxError};
use axfs::FS_CONTEXT;
use axfs_ng_vfs::{Location, NodeType};
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::general::CAP_SYS_ADMIN;
use memory_addr::PAGE_SIZE_4K;

use crate::{mm::vm_load_string, task::AsThread};

const SWAPSPACE2_MAGIC: &[u8; 10] = b"SWAPSPACE2";
const SWAP_FLAG_PRIO_MASK: i32 = 0x7fff;
const SWAP_FLAG_PREFER: i32 = 0x8000;
const SWAP_FLAG_DISCARD: i32 = 0x10000;
const SWAP_FLAG_DISCARD_ONCE: i32 = 0x20000;
const SWAP_FLAG_DISCARD_PAGES: i32 = 0x40000;
const SWAP_FLAGS_VALID: i32 = SWAP_FLAG_PRIO_MASK
    | SWAP_FLAG_PREFER
    | SWAP_FLAG_DISCARD
    | SWAP_FLAG_DISCARD_ONCE
    | SWAP_FLAG_DISCARD_PAGES;
const MAX_SWAPFILES: usize = 31;

static SWAP_MANAGER: Mutex<SwapManager> = Mutex::new(SwapManager::new());

#[derive(Clone)]
struct SwapEntry {
    path: String,
    device: u64,
    inode: u64,
    size_pages: u64,
    priority: i32,
}

struct SwapManager {
    entries: Vec<SwapEntry>,
}

impl SwapManager {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn find_by_identity(&self, device: u64, inode: u64) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.device == device && entry.inode == inode)
    }

    fn next_default_priority(&self) -> i32 {
        -(self.entries.len() as i32) - 1
    }

    fn total_bytes(&self) -> u64 {
        self.entries
            .iter()
            .map(|entry| entry.size_pages.saturating_mul(PAGE_SIZE_4K as u64))
            .sum()
    }
}

fn current_has_swap_capability() -> bool {
    current()
        .as_thread()
        .proc_data
        .has_effective_capability(CAP_SYS_ADMIN)
}

fn e(err: LinuxError) -> AxError {
    AxError::from(err)
}

fn decode_priority(flags: i32, manager: &SwapManager) -> i32 {
    if flags & SWAP_FLAG_PREFER != 0 {
        flags & SWAP_FLAG_PRIO_MASK
    } else {
        manager.next_default_priority()
    }
}

fn read_swap_header(loc: &Location) -> AxResult<[u8; PAGE_SIZE_4K]> {
    let mut header = [0u8; PAGE_SIZE_4K];
    let read = loc.entry().as_file()?.read_at(&mut header, 0)?;
    if read < PAGE_SIZE_4K {
        return Err(e(LinuxError::EINVAL));
    }
    Ok(header)
}

fn swap_header_pages(header: &[u8; PAGE_SIZE_4K]) -> AxResult<u64> {
    let magic_start = PAGE_SIZE_4K - SWAPSPACE2_MAGIC.len();
    if &header[magic_start..] != SWAPSPACE2_MAGIC {
        return Err(e(LinuxError::EINVAL));
    }

    let version = u32::from_ne_bytes(
        header[1024..1028]
            .try_into()
            .map_err(|_| AxError::InvalidInput)?,
    );
    if version != 1 {
        return Err(e(LinuxError::EINVAL));
    }
    let last_page = u32::from_ne_bytes(
        header[1028..1032]
            .try_into()
            .map_err(|_| AxError::InvalidInput)?,
    ) as u64;
    if last_page == 0 {
        return Err(e(LinuxError::EINVAL));
    }
    Ok(last_page)
}

pub fn swap_snapshot() -> String {
    let manager = SWAP_MANAGER.lock();
    let mut out = String::from("Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n");
    for entry in &manager.entries {
        let _ = core::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                "{}\t\t\tfile\t\t{}\t\t0\t\t{}\n",
                entry.path,
                entry.size_pages.saturating_mul(PAGE_SIZE_4K as u64) / 1024,
                entry.priority
            ),
        );
    }
    out
}

pub(crate) fn swap_total_bytes() -> u64 {
    SWAP_MANAGER.lock().total_bytes()
}

pub(crate) fn swap_free_bytes() -> u64 {
    // This compatibility layer records enabled swap files but does not page out
    // memory, so all enabled swap remains free in procfs accounting.
    swap_total_bytes()
}

pub fn sys_swapon(specialfile: *const c_char, swap_flags: i32) -> AxResult<isize> {
    if swap_flags & !SWAP_FLAGS_VALID != 0 {
        return Err(e(LinuxError::EINVAL));
    }
    if !current_has_swap_capability() {
        return Err(e(LinuxError::EPERM));
    }

    let path = vm_load_string(specialfile)?;
    let loc = FS_CONTEXT.lock().resolve(&path)?;
    let metadata = loc.metadata()?;
    if metadata.node_type != NodeType::RegularFile {
        return Err(e(LinuxError::EINVAL));
    }

    let header = read_swap_header(&loc)?;
    let size_pages = swap_header_pages(&header)?;
    let mut manager = SWAP_MANAGER.lock();
    if manager
        .find_by_identity(metadata.device, metadata.inode)
        .is_some()
    {
        return Err(e(LinuxError::EBUSY));
    }
    if manager.entries.len() >= MAX_SWAPFILES {
        return Err(e(LinuxError::EPERM));
    }
    let priority = decode_priority(swap_flags, &manager);
    manager.entries.push(SwapEntry {
        path,
        device: metadata.device,
        inode: metadata.inode,
        size_pages,
        priority,
    });
    Ok(0)
}

pub fn sys_swapoff(specialfile: *const c_char) -> AxResult<isize> {
    if !current_has_swap_capability() {
        return Err(e(LinuxError::EPERM));
    }

    let path = vm_load_string(specialfile)?;
    let loc = FS_CONTEXT.lock().resolve(&path)?;
    let metadata = loc.metadata()?;
    let mut manager = SWAP_MANAGER.lock();
    let Some(index) = manager.find_by_identity(metadata.device, metadata.inode) else {
        return Err(e(LinuxError::EINVAL));
    };
    manager.entries.remove(index);
    Ok(0)
}
