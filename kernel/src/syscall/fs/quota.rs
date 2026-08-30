//! Linux quota-control ABI backed by each VFS mount root.
use alloc::{collections::{BTreeMap, BTreeSet}, sync::Arc, vec, vec::Vec};
use core::ffi::c_char;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{DeviceId, Location, path::Path};
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::general::{AT_FDCWD, CAP_SYS_ADMIN};
use thekernel_linux_cred::Kgid;
use thekernel_linux_usercopy::{
    UserMemory, UserMemoryContext, vm_load, vm_load_until_nul, vm_write_slice,
};

use super::ctl::validate_pathname;
use crate::{
    file::{
        Directory, File, ResolveAtResult, get_file_like, permission::VfsSecurityContext,
        resolve_at_with_security,
    },
    mm::map_usercopy_error,
    task::AsThread,
};

const SUBCMDMASK: u32 = 0xff;
const Q_SYNC: u32 = 0x800001;
const Q_QUOTAON: u32 = 0x800002;
const Q_QUOTAOFF: u32 = 0x800003;
const Q_GETFMT: u32 = 0x800004;
const Q_GETINFO: u32 = 0x800005;
const Q_SETINFO: u32 = 0x800006;
const Q_GETQUOTA: u32 = 0x800007;
const Q_SETQUOTA: u32 = 0x800008;
const Q_GETNEXTQUOTA: u32 = 0x800009;
const QFMT_VFS_V1: u32 = 4;
const QFMT_VFS_OLD: u32 = 1;
const QIF_BLIMITS: u32 = 1;
const QIF_SPACE: u32 = 2;
const QIF_ILIMITS: u32 = 4;
const QIF_INODES: u32 = 8;
const QIF_BTIME: u32 = 16;
const QIF_ITIME: u32 = 32;
const QIF_BGRACE: u32 = 1;
const QIF_IGRACE: u32 = 2;
const QIF_FLAGS: u32 = 4;
const DQBLK_VALID_MASK: u32 = QIF_BLIMITS | QIF_SPACE | QIF_ILIMITS | QIF_INODES | QIF_BTIME | QIF_ITIME;
const DQINFO_VALID_MASK: u32 = QIF_BGRACE | QIF_IGRACE | QIF_FLAGS;
// Linux's v2 on-disk quota format.  All fields are explicitly little endian:
// quota files are data files, not native-endian kernel snapshots.
const V2_MAGIC: u32 = 0xd9c0_1f11;
const V2_VERSION: u32 = 1;
const QUOTA_BLOCK: usize = 1024;
// A quota file is administrative metadata, not an unbounded userspace data
// stream.  Keep activation memory bounded; the sparse v2 tree itself is also
// limited by this cap.
const MAX_QUOTA_FILE_BYTES: usize = 64 * 1024 * 1024;

#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct IfDqblk {
    bhardlimit: u64,
    bsoftlimit: u64,
    curspace: u64,
    ihardlimit: u64,
    isoftlimit: u64,
    curinodes: u64,
    btime: u64,
    itime: u64,
    valid: u32,
    _pad: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct IfDqinfo {
    bgrace: u64,
    igrace: u64,
    flags: u32,
    valid: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct IfNextDqblk {
    bhardlimit: u64,
    bsoftlimit: u64,
    curspace: u64,
    ihardlimit: u64,
    isoftlimit: u64,
    curinodes: u64,
    btime: u64,
    itime: u64,
    valid: u32,
    id: u32,
}
#[derive(Clone, Default)]
struct QuotaData {
    enabled: [bool; 3],
    records: BTreeMap<(u8, u32), IfDqblk>,
    info: [IfDqinfo; 3],
    // Keep the inode rather than the spelling supplied to Q_QUOTAON.  A quota
    // file may be reached through a bind mount or a hard link, neither of
    // which may consume the quota it controls.
    quota_files: [Option<Location>; 3],
    formats: [QuotaFormat; 3],
    dirty: bool,
}
#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum QuotaFormat { OldV1, #[default] V2 }
#[derive(Default)]
struct QuotaState(Mutex<QuotaData>);

const V2_MAGICS: [u32; 3] = [0xd9c0_1f11, 0xd9c0_1927, 0xd9c0_3f14];
const V2_INFO_OFF: usize = 8;
const V2_LEAF_HEAD: usize = 16;
const V2R1_ENTRY: usize = 72;

fn put32(buf: &mut [u8], at: usize, value: u32) { buf[at..at + 4].copy_from_slice(&value.to_le_bytes()); }
fn put64(buf: &mut [u8], at: usize, value: u64) { buf[at..at + 8].copy_from_slice(&value.to_le_bytes()); }
fn get32(buf: &[u8], at: usize) -> AxResult<u32> { buf.get(at..at + 4).and_then(|v| v.try_into().ok()).map(u32::from_le_bytes).ok_or(AxError::InvalidInput) }
fn get64(buf: &[u8], at: usize) -> AxResult<u64> { buf.get(at..at + 8).and_then(|v| v.try_into().ok()).map(u64::from_le_bytes).ok_or(AxError::InvalidInput) }

fn encode_state(data: &QuotaData, ty: usize) -> AxResult<Vec<u8>> {
    let records: Vec<_> = data.records.iter().filter(|((kind, _), _)| *kind as usize == ty).collect();
    let mut bytes = vec![0; QUOTA_BLOCK * 2]; // header + root pointer block
    put32(&mut bytes, 0, V2_MAGICS[ty]); put32(&mut bytes, 4, V2_VERSION);
    put32(&mut bytes, V2_INFO_OFF, data.info[ty].bgrace as u32);
    put32(&mut bytes, V2_INFO_OFF + 4, data.info[ty].igrace as u32);
    put32(&mut bytes, V2_INFO_OFF + 8, data.info[ty].flags);
    // dqi_blocks/free_blk/free_entry are patched after every tree allocation.
    let mut nodes = BTreeMap::<(u8, u32), u32>::new();
    let mut allocate = |bytes: &mut Vec<u8>| -> AxResult<u32> {
        let block = (bytes.len() / QUOTA_BLOCK) as u32;
        bytes.try_reserve(QUOTA_BLOCK).map_err(|_| AxError::NoMemory)?;
        bytes.resize(bytes.len() + QUOTA_BLOCK, 0); Ok(block)
    };
    for (&(_, id), record) in records {
        let mut parent = 1u32;
        for (level, shift) in [(0u8, 24u32), (1, 16), (2, 8)] {
            let prefix = id >> shift;
            let child = match nodes.get(&(level, prefix)) { Some(&v) => v, None => {
                let v = allocate(&mut bytes)?; nodes.insert((level, prefix), v); v
            }};
            put32(&mut bytes, parent as usize * QUOTA_BLOCK + (((id >> shift) & 0xff) as usize * 4), child);
            parent = child;
        }
        let leaf = allocate(&mut bytes)?;
        put32(&mut bytes, parent as usize * QUOTA_BLOCK + ((id & 0xff) as usize * 4), leaf);
        let off = leaf as usize * QUOTA_BLOCK;
        bytes[off + 8..off + 10].copy_from_slice(&1u16.to_le_bytes());
        put32(&mut bytes, off + V2_LEAF_HEAD, id);
        put64(&mut bytes, off + V2_LEAF_HEAD + 8, record.ihardlimit);
        put64(&mut bytes, off + V2_LEAF_HEAD + 16, record.isoftlimit);
        put64(&mut bytes, off + V2_LEAF_HEAD + 24, record.curinodes);
        put64(&mut bytes, off + V2_LEAF_HEAD + 32, record.bhardlimit);
        put64(&mut bytes, off + V2_LEAF_HEAD + 40, record.bsoftlimit);
        put64(&mut bytes, off + V2_LEAF_HEAD + 48, record.curspace);
        put64(&mut bytes, off + V2_LEAF_HEAD + 56, record.btime);
        put64(&mut bytes, off + V2_LEAF_HEAD + 64, record.itime);
    }
    let block_count = (bytes.len() / QUOTA_BLOCK) as u32;
    put32(&mut bytes, V2_INFO_OFF + 12, block_count);
    Ok(bytes)
}

// The legacy VFS quota format is an id-indexed array of eight native-endian
// u32 fields. Linux defines it as host-endian; on TheKernel's x86_64-only ABI
// that is little endian. Record zero carries the grace periods.
fn encode_v1(data: &QuotaData, ty: usize) -> AxResult<Vec<u8>> {
    let max = data.records.range((ty as u8, 0)..=(ty as u8, u32::MAX)).map(|((_, id), _)| *id).max().unwrap_or(0);
    let bytes_len = (max as usize + 1).checked_mul(32).ok_or(AxError::NoMemory)?;
    let mut bytes = vec![0; bytes_len];
    put32(&mut bytes, 24, data.info[ty].igrace.min(u32::MAX as u64) as u32);
    put32(&mut bytes, 28, data.info[ty].bgrace.min(u32::MAX as u64) as u32);
    for (&(_, id), r) in data.records.range((ty as u8, 0)..=(ty as u8, u32::MAX)) {
        let o = id as usize * 32;
        put32(&mut bytes,o,r.bhardlimit.min(u32::MAX as u64) as u32); put32(&mut bytes,o+4,r.bsoftlimit.min(u32::MAX as u64) as u32);
        put32(&mut bytes,o+8,(r.curspace.div_ceil(1024)).min(u32::MAX as u64) as u32); put32(&mut bytes,o+12,r.ihardlimit.min(u32::MAX as u64) as u32);
        put32(&mut bytes,o+16,r.isoftlimit.min(u32::MAX as u64) as u32); put32(&mut bytes,o+20,r.curinodes.min(u32::MAX as u64) as u32);
        put32(&mut bytes,o+24,r.itime.min(u32::MAX as u64) as u32); put32(&mut bytes,o+28,r.btime.min(u32::MAX as u64) as u32);
    }
    Ok(bytes)
}

fn flush_locked(data: &mut QuotaData) -> AxResult<()> {
    if !data.dirty { return Ok(()); }
    for (ty, file) in data.quota_files.iter().enumerate() {
        let Some(file) = file else { continue; };
        let encoded = if data.formats[ty] == QuotaFormat::OldV1 { encode_v1(data, ty)? } else { encode_state(data, ty)? };
        // Never store quota state in an implementation xattr: quota tools
        // hand us a quota file and expect that file to be authoritative.
        let written = file.entry().as_file()?.write_at(&encoded, 0)?;
        if written != encoded.len() { return Err(AxError::Io); }
        file.sync(false)?;
    }
    data.dirty = false;
    Ok(())
}

fn decode_state(bytes: &[u8], data: &mut QuotaData, ty: usize) -> AxResult<()> {
    if bytes.len() < QUOTA_BLOCK * 2 || bytes.len() % QUOTA_BLOCK != 0
        || get32(bytes, 0)? != V2_MAGICS[ty] || get32(bytes, 4)? != V2_VERSION {
        return Err(AxError::InvalidInput);
    }
    let blocks = get32(bytes, V2_INFO_OFF + 12)? as usize;
    if blocks < 2 || blocks > bytes.len() / QUOTA_BLOCK { return Err(AxError::InvalidInput); }
    // Files cannot be atomically shortened by every supported backend.  The
    // validated dqi_blocks boundary is authoritative; stale tail blocks are
    // deliberately ignored rather than parsed as a second tree.
    let bytes = &bytes[..blocks * QUOTA_BLOCK];
    data.info[ty].bgrace = get32(bytes, V2_INFO_OFF)? as u64;
    data.info[ty].igrace = get32(bytes, V2_INFO_OFF + 4)? as u64;
    data.info[ty].flags = get32(bytes, V2_INFO_OFF + 8)?;
    data.records.retain(|(kind, _), _| *kind as usize != ty);
    let mut seen = BTreeSet::new(); let mut stack = Vec::new(); stack.push((1u32, 0u8, 0u32));
    while let Some((block, depth, prefix)) = stack.pop() {
        if block == 0 || block as usize >= blocks || !seen.insert(block) { return Err(AxError::InvalidInput); }
        let off = block as usize * QUOTA_BLOCK;
        if depth < 3 { for i in 0..256 { let next = get32(bytes, off + i * 4)?; if next != 0 { stack.push((next, depth + 1, prefix | ((i as u32) << (24 - depth as u32 * 8)))); } } }
        else { for i in 0..256 { let leaf = get32(bytes, off + i * 4)?; if leaf == 0 { continue; } if leaf as usize >= blocks || !seen.insert(leaf) { return Err(AxError::InvalidInput); }
            let lo = leaf as usize * QUOTA_BLOCK; let entries = u16::from_le_bytes(bytes[lo + 8..lo + 10].try_into().unwrap()) as usize;
            if entries == 0 || entries > (QUOTA_BLOCK - V2_LEAF_HEAD) / V2R1_ENTRY { return Err(AxError::InvalidInput); }
            for n in 0..entries { let e = lo + V2_LEAF_HEAD + n * V2R1_ENTRY; let id = get32(bytes, e)?; if id != (prefix | i as u32) { return Err(AxError::InvalidInput); } if !data.records.insert((ty as u8, id), IfDqblk { ihardlimit: get64(bytes,e+8)?, isoftlimit:get64(bytes,e+16)?, curinodes:get64(bytes,e+24)?, bhardlimit:get64(bytes,e+32)?, bsoftlimit:get64(bytes,e+40)?, curspace:get64(bytes,e+48)?, btime:get64(bytes,e+56)?, itime:get64(bytes,e+64)?, ..Default::default() }).is_none() { return Err(AxError::InvalidInput); } }
        }}
    }
    Ok(())
}

fn decode_v1(bytes: &[u8], data: &mut QuotaData, ty: usize) -> AxResult<()> {
    if bytes.is_empty() || bytes.len() % 32 != 0 { return Err(AxError::InvalidInput); }
    data.records.retain(|(kind, _), _| *kind as usize != ty);
    data.info[ty].igrace = get32(bytes, 24)? as u64; data.info[ty].bgrace = get32(bytes, 28)? as u64; data.info[ty].flags = 0;
    for id in 0..bytes.len() / 32 { let o = id * 32; let r = IfDqblk { bhardlimit:get32(bytes,o)? as u64, bsoftlimit:get32(bytes,o+4)? as u64, curspace:(get32(bytes,o+8)? as u64)*1024, ihardlimit:get32(bytes,o+12)? as u64, isoftlimit:get32(bytes,o+16)? as u64, curinodes:get32(bytes,o+20)? as u64, itime:get32(bytes,o+24)? as u64, btime:get32(bytes,o+28)? as u64, ..Default::default() }; if r.bhardlimit != 0 || r.bsoftlimit != 0 || r.curspace != 0 || r.ihardlimit != 0 || r.isoftlimit != 0 || r.curinodes != 0 { data.records.insert((ty as u8, id as u32), r); } }
    Ok(())
}

fn mark_dirty(root: &Location) {
    if let Ok(state) = state(root) { state.0.lock().dirty = true; }
}

fn quota_type(cmd: u32) -> AxResult<usize> {
    match cmd & SUBCMDMASK {
        0..=2 => Ok((cmd & SUBCMDMASK) as usize),
        _ => Err(AxError::InvalidInput),
    }
}
fn admin() -> AxResult<()> {
    current()
        .as_thread()
        .has_effective_capability(CAP_SYS_ADMIN)
        .then_some(())
        .ok_or_else(|| LinuxError::EPERM.into())
}
fn may_read(ty: usize, id: u32) -> bool {
    let task = current();
    let thread = task.as_thread();
    thread.has_effective_capability(CAP_SYS_ADMIN)
        || (ty == 0 && current().as_thread().fsuid().into_raw() == id)
        || (ty == 1
            && (thread.fsgid().into_raw() == id
                || Kgid::from_raw(id).is_some_and(|gid| thread.current_cred().groups().contains(gid))))
}
fn merge_record(old: &mut IfDqblk, new: IfDqblk) {
    let valid = new.valid;
    if valid & QIF_BLIMITS != 0 {
        old.bhardlimit = new.bhardlimit;
        old.bsoftlimit = new.bsoftlimit;
    }
    if valid & QIF_SPACE != 0 {
        old.curspace = new.curspace;
    }
    if valid & QIF_ILIMITS != 0 {
        old.ihardlimit = new.ihardlimit;
        old.isoftlimit = new.isoftlimit;
    }
    if valid & QIF_INODES != 0 {
        old.curinodes = new.curinodes;
    }
    if valid & QIF_BTIME != 0 {
        old.btime = new.btime;
    }
    if valid & QIF_ITIME != 0 {
        old.itime = new.itime;
    }
    old.valid |= valid;
}
fn merge_info(old: &mut IfDqinfo, new: IfDqinfo) {
    let valid = new.valid;
    if valid & QIF_BGRACE != 0 {
        old.bgrace = new.bgrace;
    }
    if valid & QIF_IGRACE != 0 {
        old.igrace = new.igrace;
    }
    if valid & QIF_FLAGS != 0 {
        old.flags = new.flags;
    }
    old.valid |= valid;
}
fn root_for_path<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const c_char,
) -> AxResult<Location> {
    let bytes = vm_load_until_nul(memory, ptr.cast()).map_err(map_usercopy_error)?;
    if bytes.is_empty() {
        return Err(LinuxError::ENOENT.into());
    }
    let string = core::str::from_utf8(&bytes).map_err(|_| AxError::IllegalBytes)?;
    let path = Path::new(string);
    validate_pathname(path)?;
    let security = VfsSecurityContext::new(current().as_thread().current_cred());
    let loc = match resolve_at_with_security(AT_FDCWD, Some(path.as_str()), 0, &security)? {
        ResolveAtResult::File(loc) => loc,
        ResolveAtResult::Other(_) => return Err(AxError::InvalidInput),
    };
    Ok(loc.mountpoint().root_location())
}
fn location_for_fd(fd: i32) -> AxResult<Location> {
    let file = get_file_like(fd)?;
    let loc = if let Some(file) = file.downcast_ref::<File>() {
        file.inner().location().clone()
    } else if let Some(dir) = file.downcast_ref::<Directory>() {
        dir.inner().clone()
    } else {
        return Err(AxError::InvalidInput);
    };
    Ok(loc)
}
fn state(root: &Location) -> AxResult<Arc<QuotaState>> {
    root.user_data().try_get_or_insert_with(QuotaState::default)
}

fn root_for_location(loc: &Location) -> Location {
    loc.mountpoint().root_location()
}

fn owners(metadata: &axfs_ng_vfs::Metadata) -> [u32; 3] {
    [metadata.uid, metadata.gid, metadata.project_id]
}

/// Mount-wide inode enumeration used by Q_QUOTAON.
///
/// A directory walk is not an accounting oracle: it misses an unlinked inode
/// still held open and it describes hard links more than once. Ask the mounted
/// filesystem for its live-inode registry instead. Backends which cannot make
/// that complete promise fail with EOPNOTSUPP through the VFS contract.
fn seed_usage(root: &Location, data: &mut QuotaData, ty: usize) -> AxResult<()> {
    for ((kind, _), record) in data.records.iter_mut() {
        if *kind as usize == ty {
            record.curinodes = 0;
            record.curspace = 0;
        }
    }
    let quota_inodes: Vec<_> = data.quota_files.iter().flatten().filter_map(|file| {
        file.same_mount(root).then(|| file.inode())
    }).collect();
    root.filesystem().enumerate_inodes(&mut |metadata| {
        // unlink refunds quota at namespace removal, not at final close.
        // Backends may keep a zero-link inode physically allocated while an
        // open descriptor pins it; it must not be recharged on activation.
        if metadata.nlink == 0 { return Ok(()); }
        if quota_inodes.contains(&metadata.inode) { return Ok(()); }
        let owner = owners(&metadata)[ty];
        let record = data.records.entry((ty as u8, owner)).or_default();
        record.curinodes = record.curinodes.checked_add(1).ok_or(LinuxError::ENOSPC)?;
        record.curspace = record.curspace.checked_add(metadata.blocks.saturating_mul(512)).ok_or(LinuxError::ENOSPC)?;
        Ok(())
    })?;
    Ok(())
}

fn limit_reached(record: &mut IfDqblk, info: IfDqinfo, space: i128, inodes: i128) -> AxResult<()> {
    let now = crate::time::wall_time().as_secs();
    let next_space = (record.curspace as i128).checked_add(space).ok_or(LinuxError::ENOSPC)?;
    let next_inodes = (record.curinodes as i128).checked_add(inodes).ok_or(LinuxError::ENOSPC)?;
    if next_space < 0 || next_inodes < 0 { return Err(AxError::BadState); }
    let check = |next: u64, hard: u64, soft: u64, time: &mut u64, grace: u64| -> AxResult<()> {
        // VFS v1 limits are expressed in KiB while curspace is bytes.
        if hard != 0 && next > hard.saturating_mul(1024) { return Err(LinuxError::EDQUOT.into()); }
        if soft != 0 && next > soft.saturating_mul(1024) {
            if *time == 0 { *time = now.saturating_add(grace); }
            else if now >= *time { return Err(LinuxError::EDQUOT.into()); }
        } else { *time = 0; }
        Ok(())
    };
    check(next_space as u64, record.bhardlimit, record.bsoftlimit, &mut record.btime, info.bgrace)?;
    // inode limits are counts, not KiB blocks.
    if record.ihardlimit != 0 && next_inodes as u64 > record.ihardlimit { return Err(LinuxError::EDQUOT.into()); }
    if record.isoftlimit != 0 && next_inodes as u64 > record.isoftlimit {
        if record.itime == 0 { record.itime = now.saturating_add(info.igrace); }
        else if now >= record.itime { return Err(LinuxError::EDQUOT.into()); }
    } else { record.itime = 0; }
    record.curspace = next_space as u64;
    record.curinodes = next_inodes as u64;
    Ok(())
}

/// A charged VFS mutation.  Charges are made before the backend mutation and
/// are undone unless the caller commits after that mutation succeeds.
pub(crate) struct QuotaCharge { root: Location, owners: [u32; 3], enabled: [bool; 3], space: i128, baseline_space: Option<i128>, inodes: i128, committed: bool }
impl QuotaCharge {
    pub(crate) fn commit(mut self) {
        mark_dirty(&self.root);
        self.committed = true;
    }
    /// Settles a conservative write reservation to the filesystem's actual
    /// 512-byte allocation count after publication.
    pub(crate) fn commit_actual_blocks(mut self, location: &Location) -> AxResult<()> {
        if let Some(before) = self.baseline_space {
            let actual = location.metadata()?.blocks as i128 * 512 - before;
            let adjustment = actual - self.space;
            if adjustment != 0 {
                let state = state(&self.root)?;
                let mut data = state.0.lock();
                for (kind, id) in self.owners.iter().copied().enumerate() {
                    if !self.enabled[kind] { continue; }
                    let info = data.info[kind];
                    limit_reached(data.records.entry((kind as u8, id)).or_default(), info, adjustment, 0)?;
                }
            }
        }
        self.committed = true;
        mark_dirty(&self.root);
        Ok(())
    }
}
impl Drop for QuotaCharge {
    fn drop(&mut self) {
        if self.committed { return; }
        if let Ok(state) = state(&self.root) {
            let mut data = state.0.lock();
            for (kind, id) in self.owners.iter().copied().enumerate() {
                if !self.enabled[kind] { continue; }
                let info = data.info[kind];
                let record = data.records.entry((kind as u8, id)).or_default();
                // Reversal cannot cross a limit; an internal inconsistency is
                // deliberately contained rather than escaping Drop.
                let _ = limit_reached(record, info, -self.space, -self.inodes);
            }
        }
    }
}

fn charge(loc: &Location, metadata: &axfs_ng_vfs::Metadata, space: i128, inodes: i128) -> AxResult<QuotaCharge> {
    let root = root_for_location(loc);
    let state = state(&root)?;
    let owners = owners(metadata);
    let mut data = state.0.lock();
    if data.quota_files.iter().flatten().any(|file| file.same_node(loc)) { return Ok(QuotaCharge { root, owners, enabled: [false; 3], space: 0, baseline_space: None, inodes: 0, committed: true }); }
    // All enabled dimensions are admitted as one transaction.  Roll back the
    // dimensions already charged when a later one rejects the operation.
    let mut charged = 0;
    for (kind, id) in owners.iter().copied().enumerate() {
        if !data.enabled[kind] { continue; }
        let info = data.info[kind];
        let record = data.records.entry((kind as u8, id)).or_default();
        // Grace deadlines are part of the transaction too: never leave a
        // failed later dimension with a freshly armed deadline.
        let mut updated = *record;
        if let Err(error) = limit_reached(&mut updated, info, space, inodes) {
            for rollback_kind in 0..charged {
                if !data.enabled[rollback_kind] { continue; }
                let rollback_info = data.info[rollback_kind];
                let rollback = data.records.entry((rollback_kind as u8, owners[rollback_kind])).or_default();
                let _ = limit_reached(rollback, rollback_info, -space, -inodes);
            }
            return Err(error);
        }
        *record = updated;
        charged = kind + 1;
    }
    Ok(QuotaCharge { root, owners, enabled: data.enabled, space, baseline_space: None, inodes, committed: false })
}

pub(crate) fn admit_inode_create(parent: &Location, metadata: &axfs_ng_vfs::Metadata) -> AxResult<QuotaCharge> {
    charge(parent, metadata, 0, 1)
}
pub(crate) fn admit_resize(loc: &Location, old_len: u64, new_len: u64) -> AxResult<QuotaCharge> {
    let metadata = loc.metadata()?;
    let before = metadata.blocks as i128 * 512;
    // This is a bound, not an assertion about allocation: sparse backends may
    // allocate less and are settled from Metadata.blocks after success.
    let predicted = (new_len.div_ceil(512) as i128 * 512).max(before);
    let mut charge = charge(loc, &metadata, predicted - before, 0)?;
    charge.baseline_space = Some(before);
    let _ = old_len;
    Ok(charge)
}
pub(crate) fn admit_unlink(loc: &Location, metadata: &axfs_ng_vfs::Metadata) -> AxResult<QuotaCharge> {
    charge(loc, metadata, -(metadata.blocks as i128 * 512), -1)
}
pub(crate) fn admit_chown(loc: &Location, old: &axfs_ng_vfs::Metadata, new: &axfs_ng_vfs::Metadata) -> AxResult<(QuotaCharge, QuotaCharge)> {
    // Transfer is reserved against the new owner first; both guards make the
    // operation rollback-safe if metadata publication fails.
    let space = old.blocks as i128 * 512;
    Ok((charge(loc, new, space, 1)?, charge(loc, old, -space, -1)?))
}
fn read_struct<M: UserMemory + ?Sized, T: bytemuck::Pod>(
    memory: &mut UserMemoryContext<'_, M>,
    addr: usize,
) -> AxResult<T> {
    if addr == 0 {
        return Err(LinuxError::EFAULT.into());
    }
    let bytes = vm_load(memory, addr as *const u8, core::mem::size_of::<T>())
        .map_err(map_usercopy_error)?;
    Ok(bytemuck::pod_read_unaligned(&bytes))
}
fn write_struct<M: UserMemory + ?Sized, T: bytemuck::NoUninit>(
    memory: &mut UserMemoryContext<'_, M>,
    addr: usize,
    value: &T,
) -> AxResult<()> {
    if addr == 0 {
        return Err(LinuxError::EFAULT.into());
    }
    vm_write_slice(memory, addr as *mut u8, bytemuck::bytes_of(value)).map_err(map_usercopy_error)
}
fn quotactl<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    root: Location,
    cmd: u32,
    id: u32,
    addr: usize,
) -> AxResult<isize> {
    let op = cmd & !SUBCMDMASK;
    let ty = quota_type(cmd)?;
    let state = state(&root)?;
    match op {
        Q_SYNC => {
            admin()?;
            let mut data = state.0.lock();
            flush_locked(&mut data)?;
            Ok(0)
        }
        Q_QUOTAON => {
            admin()?;
            if ty == 2 && root.filesystem().name() == "fat" {
                // FAT has no durable inode project-id representation.  Do
                // not silently collapse all project IDs into zero.
                return Err(AxError::OperationNotSupported);
            }
            if id != QFMT_VFS_V1 && id != QFMT_VFS_OLD { return Err(AxError::InvalidInput); }
            let path = vm_load_until_nul(memory, addr as *const u8).map_err(map_usercopy_error)?;
            if path.is_empty() { return Err(LinuxError::ENOENT.into()); }
            let path = core::str::from_utf8(&path).map_err(|_| AxError::IllegalBytes)?;
            validate_pathname(Path::new(path))?;
            let security = VfsSecurityContext::new(current().as_thread().current_cred());
            let quota_file = match resolve_at_with_security(AT_FDCWD, Some(path), 0, &security)? {
                ResolveAtResult::File(file) => file,
                ResolveAtResult::Other(_) => return Err(AxError::InvalidInput),
            };
            if !quota_file.same_mount(&root) {
                return Err(LinuxError::EXDEV.into());
            }
            let mut q = state.0.lock();
            // Everything below is fallible, including parsing the supplied
            // file and enumerating the mount. Keep the published state intact
            // until both have succeeded.
            let mut next = q.clone();
            if !next.enabled[ty] {
                let len = usize::try_from(quota_file.metadata()?.size).map_err(|_| AxError::InvalidInput)?;
                if len > MAX_QUOTA_FILE_BYTES { return Err(AxError::InvalidInput); }
                if len != 0 {
                    let mut bytes = Vec::new();
                    bytes.try_reserve_exact(len).map_err(|_| AxError::NoMemory)?;
                    bytes.resize(len, 0);
                    let read = quota_file.entry().as_file()?.read_at(&mut bytes, 0)?;
                    bytes.truncate(read);
                    next.formats[ty] = if bytes.len() >= 8 && get32(&bytes, 0).ok() == Some(V2_MAGICS[ty]) { QuotaFormat::V2 } else { QuotaFormat::OldV1 };
                    if next.formats[ty] == QuotaFormat::V2 { decode_state(&bytes, &mut next, ty)?; } else { decode_v1(&bytes, &mut next, ty)?; }
                    // Each quota type is explicitly activated by Q_QUOTAON;
                    // persisted enabled bits describe the prior clean state,
                    // not an implicit mount-time activation.
                    next.enabled = [false; 3];
                }
            }
            if next.enabled[ty] {
                Err(LinuxError::EBUSY.into())
            } else {
                next.formats[ty] = if id == QFMT_VFS_OLD { QuotaFormat::OldV1 } else { QuotaFormat::V2 };
                next.quota_files[ty] = Some(quota_file);
                // Existing inodes predate quota activation.  Their current
                // uid/gid/project and allocated 512-byte block count seed the
                // ledger before new mutations are admitted.
                seed_usage(&root, &mut next, ty)?;
                next.enabled[ty] = true;
                next.dirty = true;
                *q = next;
                Ok(0)
            }
        }
        Q_QUOTAOFF => {
            admin()?;
            let mut q = state.0.lock();
            if !q.enabled[ty] {
                Err(AxError::InvalidInput)
            } else {
                flush_locked(&mut q)?;
                q.enabled[ty] = false;
                q.quota_files[ty] = None;
                Ok(0)
            }
        }
        Q_GETFMT => {
            if !state.0.lock().enabled[ty] {
                return Err(AxError::InvalidInput);
            }
            write_struct(memory, addr, &QFMT_VFS_V1)?;
            Ok(0)
        }
        Q_GETINFO => {
            let q = state.0.lock();
            if !q.enabled[ty] { return Err(AxError::InvalidInput); }
            let info = IfDqinfo { valid: DQINFO_VALID_MASK, ..q.info[ty] };
            write_struct(memory, addr, &info)?;
            Ok(0)
        }
        Q_SETINFO => {
            admin()?;
            let new: IfDqinfo = read_struct(memory, addr)?;
            if new.valid & !DQINFO_VALID_MASK != 0 {
                return Err(AxError::InvalidInput);
            }
            merge_info(&mut state.0.lock().info[ty], new);
            state.0.lock().dirty = true;
            Ok(0)
        }
        Q_GETQUOTA => {
            if !may_read(ty, id) {
                return Err(LinuxError::EPERM.into());
            }
            let q = state.0.lock();
            if !q.enabled[ty] {
                return Err(AxError::InvalidInput);
            }
            write_struct(
                memory,
                addr,
                &IfDqblk { valid: DQBLK_VALID_MASK, ..q.records.get(&(ty as u8, id)).copied().unwrap_or_default() },
            )?;
            Ok(0)
        }
        Q_SETQUOTA => {
            admin()?;
            let record: IfDqblk = read_struct(memory, addr)?;
            if record.valid & !DQBLK_VALID_MASK != 0 {
                return Err(AxError::InvalidInput);
            }
            let mut q = state.0.lock();
            if !q.enabled[ty] {
                return Err(AxError::InvalidInput);
            }
            merge_record(q.records.entry((ty as u8, id)).or_default(), record);
            q.dirty = true;
            Ok(0)
        }
        Q_GETNEXTQUOTA => {
            if !may_read(ty, id) {
                return Err(LinuxError::EPERM.into());
            }
            let q = state.0.lock();
            if !q.enabled[ty] {
                return Err(AxError::InvalidInput);
            }
            let Some((&(_, next), record)) = q
                .records
                .range((ty as u8, id)..)
                .find(|((kind, _), _)| *kind == ty as u8)
            else {
                return Err(LinuxError::ESRCH.into());
            };
            if !may_read(ty, next) { return Err(LinuxError::EPERM.into()); }
            write_struct(
                memory,
                addr,
                &IfNextDqblk {
                    bhardlimit: record.bhardlimit,
                    bsoftlimit: record.bsoftlimit,
                    curspace: record.curspace,
                    ihardlimit: record.ihardlimit,
                    isoftlimit: record.isoftlimit,
                    curinodes: record.curinodes,
                    btime: record.btime,
                    itime: record.itime,
                    valid: DQBLK_VALID_MASK,
                    id: next,
                },
            )?;
            Ok(0)
        }
        _ => Err(AxError::InvalidInput),
    }
}
pub fn sys_quotactl<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    cmd: u32,
    special: *const c_char,
    id: u32,
    addr: usize,
) -> AxResult<isize> {
    if cmd & !SUBCMDMASK == Q_SYNC && special.is_null() {
        admin()?;
        let _ = quota_type(cmd)?;
        let mut devices = BTreeSet::new();
        for mount in crate::mounts::snapshot()? {
            if !devices.insert(mount.dev) { continue; }
            let root = crate::mounts::mounted_root_location(DeviceId(mount.dev))?;
            let state = state(&root)?;
            flush_locked(&mut state.0.lock())?;
        }
        return Ok(0);
    }
    let root = root_for_path(memory, special)?;
    quotactl(memory, root, cmd, id, addr)
}
pub fn sys_quotactl_fd<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    cmd: u32,
    id: u32,
    addr: usize,
) -> AxResult<isize> {
    let root = root_for_location(&location_for_fd(fd)?);
    quotactl(memory, root, cmd, id, addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn next_dqblk_matches_linux_layout() {
        assert_eq!(size_of::<IfNextDqblk>(), 72);
        assert_eq!(offset_of!(IfNextDqblk, id), 68);
    }

    #[test]
    fn hard_limits_use_vfs_v1_kib_units() {
        let mut record = IfDqblk { bhardlimit: 1, ..Default::default() };
        assert!(limit_reached(&mut record, IfDqinfo::default(), 1024, 0).is_ok());
        assert_eq!(
            limit_reached(&mut record, IfDqinfo::default(), 1, 0),
            Err(LinuxError::EDQUOT.into())
        );
    }

    #[test]
    fn inode_hard_limit_is_a_count() {
        let mut record = IfDqblk { ihardlimit: 1, ..Default::default() };
        assert!(limit_reached(&mut record, IfDqinfo::default(), 0, 1).is_ok());
        assert_eq!(
            limit_reached(&mut record, IfDqinfo::default(), 0, 1),
            Err(LinuxError::EDQUOT.into())
        );
    }

    #[test]
    fn quota_file_header_round_trips_little_endian_records() {
        let mut data = QuotaData::default();
        data.enabled[0] = true;
        data.info[0].flags = 0x55aa;
        data.records.insert((0, 42), IfDqblk { bhardlimit: 7, curspace: 4096, ..Default::default() });
        let bytes = encode_state(&data, 0).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[..4].try_into().unwrap()), V2_MAGICS[0]);
        assert_eq!(bytes.len() % 1, 0);
        let mut restored = QuotaData::default();
        decode_state(&bytes, &mut restored, 0).unwrap();
        assert!(restored.enabled[0]);
        assert_eq!(restored.records[&(0, 42)].curspace, 4096);
        assert_eq!(restored.info[0].flags, 0x55aa);
    }

    #[test]
    fn quota_file_rejects_wrong_endian_or_truncated_tree() {
        let mut data = QuotaData::default();
        let mut bytes = encode_state(&data, 0).unwrap();
        bytes[..4].reverse();
        assert_eq!(decode_state(&bytes, &mut data, 0), Err(AxError::InvalidInput));
        let mut bytes = encode_state(&QuotaData::default(), 0).unwrap();
        bytes.truncate(QUOTA_BLOCK - 1);
        assert_eq!(decode_state(&bytes, &mut data, 0), Err(AxError::InvalidInput));
    }

    #[test]
    fn quota_file_rejects_wrong_tree_path_or_version() {
        let mut data = QuotaData::default();
        data.records.insert((0, 0x0102_0304), IfDqblk::default());
        let mut bytes = encode_state(&data, 0).unwrap();
        // The first non-root child is the top byte of this ID. Pointing it at
        // a valid lower subtree makes the tree malformed, not a second name
        // for the same record.
        let child = get32(&bytes, QUOTA_BLOCK + 1 * 4).unwrap();
        put32(&mut bytes, QUOTA_BLOCK + 2 * 4, child);
        assert_eq!(decode_state(&bytes, &mut QuotaData::default(), 0), Err(AxError::InvalidInput));

        let mut bytes = encode_state(&data, 0).unwrap();
        put32(&mut bytes, 4, 0);
        assert_eq!(decode_state(&bytes, &mut QuotaData::default(), 0), Err(AxError::InvalidInput));
    }

    #[test]
    fn old_v1_dqblk_array_round_trips_and_rejects_partial_record() {
        let mut data = QuotaData::default();
        data.info[0].bgrace = 60;
        data.records.insert((0, 3), IfDqblk { bhardlimit: 9, curspace: 1025, ..Default::default() });
        let bytes = encode_v1(&data, 0).unwrap();
        let mut restored = QuotaData::default();
        decode_v1(&bytes, &mut restored, 0).unwrap();
        assert_eq!(restored.info[0].bgrace, 60);
        assert_eq!(restored.records[&(0, 3)].curspace, 2048);
        assert_eq!(decode_v1(&bytes[..bytes.len() - 1], &mut restored, 0), Err(AxError::InvalidInput));
    }
}
