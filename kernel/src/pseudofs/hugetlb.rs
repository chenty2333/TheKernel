//! hugetlbfs superblock construction and mount-option admission.
//!
//! The huge mapping implementation is deliberately kept out of this module:
//! this provider owns only the distinct namespace, capacity reservation and
//! superblock identity.  It never aliases a hugetlbfs mount to tmpfs.

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{Filesystem, Location, NodePermission};
use axhal::paging::PageSize;
use axtask::current;

use super::tmp::MemoryFs;
use crate::{
    file::{FileMmapRequest, PreparedFileMmap},
    task::AsThread,
};

pub const HUGETLB_PAGE_SIZE: u64 = 2 * 1024 * 1024;
pub const HUGETLB_1G_PAGE_SIZE: u64 = 1024 * 1024 * 1024;
const DEFAULT_HUGETLB_PAGES: u64 = 8;

#[derive(Clone, Copy)]
struct MountOptions {
    capacity_bytes: u64,
    mode: NodePermission,
    page_size: PageSize,
    max_inodes: usize,
    uid: u32,
    gid: u32,
    min_size: u64,
}

fn parse_size(value: &str) -> Option<u64> {
    let value = value.trim();
    let (digits, scale) = match value.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], 1024),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        Some(b'b' | b'B') => (&value[..value.len() - 1], 1),
        Some(_) => (value, 1),
        None => return None,
    };
    digits.trim().parse::<u64>().ok()?.checked_mul(scale)
}

fn parse_mode(value: &str) -> Option<NodePermission> {
    let value = value.strip_prefix("0o").unwrap_or(value);
    u16::from_str_radix(value, 8)
        .ok()
        .filter(|mode| *mode & !0o7777 == 0)
        .map(NodePermission::from_bits_truncate)
}

fn parse_mount_options(data: &str) -> AxResult<MountOptions> {
    let mut capacity_bytes = DEFAULT_HUGETLB_PAGES * HUGETLB_PAGE_SIZE;
    let mut mode = NodePermission::from_bits_truncate(0o755);
    let mut page_size = PageSize::Size2M;
    let mut max_inodes = 65_536;
    let mut uid = 0;
    let mut gid = 0;
    let mut min_size = 0;
    let mut size_explicit = false;
    for option in data.split(',') {
        let option = option.trim();
        if option.is_empty()
            || matches!(
                option,
                "rw" | "ro" | "suid" | "nosuid" | "dev" | "nodev" | "exec" | "noexec" | "relatime"
            )
        {
            continue;
        }
        let Some((key, value)) = option.split_once('=') else {
            return Err(AxError::OperationNotSupported);
        };
        match key {
            "size" => {
                let bytes = parse_size(value).ok_or(AxError::InvalidInput)?;
                if bytes == 0 {
                    return Err(AxError::InvalidInput);
                }
                capacity_bytes = bytes;
                size_explicit = true;
            }
            "pagesize" => {
                let requested_page_size = parse_size(value).ok_or(AxError::InvalidInput)?;
                page_size = match requested_page_size {
                    HUGETLB_PAGE_SIZE => PageSize::Size2M,
                    HUGETLB_1G_PAGE_SIZE => PageSize::Size1G,
                    _ => return Err(AxError::OperationNotSupported),
                };
            }
            "nr_inodes" => {
                max_inodes = value.parse::<usize>().map_err(|_| AxError::InvalidInput)?;
                if max_inodes == 0 {
                    return Err(AxError::InvalidInput);
                }
            }
            "uid" => uid = value.parse::<u32>().map_err(|_| AxError::InvalidInput)?,
            "gid" => gid = value.parse::<u32>().map_err(|_| AxError::InvalidInput)?,
            "min_size" => {
                min_size = parse_size(value).ok_or(AxError::InvalidInput)?;
            }
            "mode" => mode = parse_mode(value).ok_or(AxError::InvalidInput)?,
            _ => return Err(AxError::OperationNotSupported),
        }
    }
    if !size_explicit {
        capacity_bytes = DEFAULT_HUGETLB_PAGES
            .checked_mul(page_size as u64)
            .ok_or(AxError::NoMemory)?;
    }
    if !capacity_bytes.is_multiple_of(page_size as u64)
        || min_size > capacity_bytes
        || !min_size.is_multiple_of(page_size as u64)
    {
        return Err(AxError::InvalidInput);
    }
    // Mount options are written in the caller's user namespace.  Preserve
    // the kernel-internal identity in the inode, while rejecting IDs that do
    // not exist in that namespace instead of silently storing an unmapped
    // raw value.
    let user_ns = current().as_thread().current_cred().user_ns().clone();
    uid = user_ns
        .make_kuid(uid)
        .ok_or(AxError::InvalidInput)?
        .into_raw();
    gid = user_ns
        .make_kgid(gid)
        .ok_or(AxError::InvalidInput)?
        .into_raw();
    Ok(MountOptions {
        capacity_bytes,
        mode,
        page_size,
        max_inodes,
        uid,
        gid,
        min_size,
    })
}

pub fn new_hugetlbfs(data: &str) -> AxResult<Filesystem> {
    let options = parse_mount_options(data)?;
    MemoryFs::new_hugetlbfs_with_capacity(
        options.mode,
        options.capacity_bytes,
        options.page_size,
        options.max_inodes,
        options.uid,
        options.gid,
        options.min_size,
    )
    .map_err(AxError::from)
}

/// Exports the inode-owned huge backing through the regular-file mmap
/// capability boundary.  This is deliberately location based so one inode
/// retains one backing through every bind alias; an open-FD or mount-id table
/// would break Linux shared-mapping identity.
pub(crate) fn prepare_mmap(
    location: &Location,
    request: FileMmapRequest,
) -> AxResult<Option<PreparedFileMmap>> {
    if location.filesystem().name() != "hugetlbfs" {
        return Ok(None);
    }
    super::tmp::prepare_hugetlbfs_mmap(location, request)
}
