//! BPF map batch commands.
//!
//! The Linux ABI deliberately permits partial completion.  `count` reports
//! completed elements where the particular map operation reaches its normal
//! epilogue; a usercopy fault can leave both a changed map and count zero.
use core::mem::{offset_of, size_of};

use axerrno::{AxError, AxResult, LinuxError};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext};

use crate::{
    bpf::{read_bpf_attr, read_user_bytes, write_bpf_attr_value, write_user_bytes},
    file::{FileLike, bpf::BpfMapFd},
};

const BPF_F_LOCK: u64 = 4;
const BPF_ATTR_MAX_SIZE: usize = 4096;
// `union bpf_attr` is 168 bytes in the pinned v6.18 UAPI (the prog-load
// member is largest).  Batch's known member itself ends at byte 56.
const LINUX_BPF_ATTR_SIZE: usize = 168;

#[derive(Clone, Copy)]
enum BatchOp {
    Lookup,
    LookupDelete,
    Update,
    Delete,
}

/// This mirrors the v6.18 map-ops table: having an element method is not the
/// same as advertising its batch callback (notably queue/stack and arrays'
/// delete path).
fn supports_batch(map_type: u32, operation: BatchOp) -> bool {
    use crate::bpf::defs::*;
    match operation {
        BatchOp::Lookup => matches!(
            map_type,
            BPF_MAP_TYPE_HASH
                | BPF_MAP_TYPE_LRU_HASH
                | BPF_MAP_TYPE_PERCPU_HASH
                | BPF_MAP_TYPE_ARRAY
                | BPF_MAP_TYPE_PERCPU_ARRAY
                | BPF_MAP_TYPE_LPM_TRIE
        ),
        BatchOp::LookupDelete => matches!(
            map_type,
            BPF_MAP_TYPE_HASH | BPF_MAP_TYPE_LRU_HASH | BPF_MAP_TYPE_PERCPU_HASH
        ),
        BatchOp::Update => matches!(
            map_type,
            BPF_MAP_TYPE_HASH
                | BPF_MAP_TYPE_LRU_HASH
                | BPF_MAP_TYPE_PERCPU_HASH
                | BPF_MAP_TYPE_ARRAY
                | BPF_MAP_TYPE_PERCPU_ARRAY
                | BPF_MAP_TYPE_LPM_TRIE
        ),
        BatchOp::Delete => matches!(
            map_type,
            BPF_MAP_TYPE_HASH
                | BPF_MAP_TYPE_LRU_HASH
                | BPF_MAP_TYPE_PERCPU_HASH
                | BPF_MAP_TYPE_LPM_TRIE
        ),
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern)]
struct Batch {
    in_batch: u64,
    out_batch: u64,
    keys: u64,
    values: u64,
    count: u32,
    map_fd: u32,
    elem_flags: u64,
    flags: u64,
}
const _: [(); 56] = [(); size_of::<Batch>()];

fn input<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<Batch> {
    let supplied = s as usize;
    if supplied > BPF_ATTR_MAX_SIZE {
        return Err(AxError::ArgumentListTooLong);
    }
    // Extension bytes beyond the complete Linux union are checked first.
    // Their non-zero/EFAULT outcome wins over a malformed known batch body.
    if supplied > LINUX_BPF_ATTR_SIZE {
        let tail = read_user_bytes(
            m,
            a.checked_add(LINUX_BPF_ATTR_SIZE)
                .ok_or(AxError::InvalidInput)?,
            supplied - LINUX_BPF_ATTR_SIZE,
        )?;
        if tail.iter().any(|byte| *byte != 0) {
            return Err(AxError::ArgumentListTooLong);
        }
    }
    let known_end = supplied.min(LINUX_BPF_ATTR_SIZE);
    if known_end > size_of::<Batch>() {
        let tail = read_user_bytes(
            m,
            a.checked_add(size_of::<Batch>())
                .ok_or(AxError::InvalidInput)?,
            known_end - size_of::<Batch>(),
        )?;
        // Bytes that are inside Linux's complete union but outside the batch
        // member are CHECK_ATTR bytes and therefore EINVAL.
        if tail.iter().any(|byte| *byte != 0) {
            return Err(AxError::InvalidInput);
        }
    }
    // Copy only Batch's supplied prefix after both tail classes have been
    // checked.  A zero-length attr is an all-zero legacy input, so FD zero is
    // resolved by the normal descriptor lookup rather than rejected here.
    let b: Batch = if supplied == 0 {
        Batch {
            in_batch: 0,
            out_batch: 0,
            keys: 0,
            values: 0,
            count: 0,
            map_fd: 0,
            elem_flags: 0,
            flags: 0,
        }
    } else {
        read_bpf_attr(m, a, supplied.min(size_of::<Batch>()) as u32)?
    };
    Ok(b)
}
fn ranges(b: Batch, map: &dyn crate::bpf::map::BpfMap) -> AxResult<(usize, usize, usize)> {
    let n = b.count as usize;
    let k = n
        .checked_mul(map.key_size() as usize)
        .ok_or(AxError::NoMemory)?;
    let v = n
        .checked_mul(map.user_value_size())
        .ok_or(AxError::NoMemory)?;
    Ok((k, v, n))
}
fn element_flags(b: Batch) -> AxResult<()> {
    // v6.18 only reserves BPF_F_LOCK here.  None of this kernel's map value
    // layouts declares a BTF spin lock, so the otherwise-valid flag is
    // rejected by the corresponding provider as Linux does.
    if b.elem_flags & !BPF_F_LOCK != 0 || b.elem_flags & BPF_F_LOCK != 0 {
        Err(AxError::InvalidInput)
    } else {
        Ok(())
    }
}
fn write_count<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
    count: u32,
) -> AxResult<()> {
    write_bpf_attr_value::<Batch, _, _>(m, a, s, offset_of!(Batch, count), &count)
}

fn hash_lookup_batch<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
    b: Batch,
    f: &BpfMapFd,
    delete: bool,
) -> AxResult<isize> {
    if b.flags != 0 {
        return Err(AxError::InvalidInput);
    }
    let count = b.count as usize;
    if count == 0 {
        return Ok(0);
    }
    // Hash callbacks zero count before consuming the optional input cursor.
    write_count(m, a, s, 0)?;
    let mut bucket = if b.in_batch == 0 {
        0
    } else {
        let raw = read_user_bytes(m, b.in_batch as usize, size_of::<u32>())?;
        u32::from_ne_bytes(raw.try_into().map_err(|_| AxError::InvalidInput)?)
    };
    // Linux rejects a cursor beyond the fixed bucket table before its normal
    // output epilogue; in particular it must not fault a null out_batch.
    if !f.map.hash_batch_cursor_valid(bucket) {
        return Err(LinuxError::ENOENT.into());
    }
    let key_size = f.map.key_size() as usize;
    let value_size = f.map.user_value_size();
    let mut done = 0usize;
    let mut terminal = false;
    let mut terminal_error: Option<AxError> = None;
    while done < count {
        let page = match f.map.hash_batch_page(bucket, count - done, delete) {
            Ok(page) => page,
            Err(error) if error == LinuxError::ENOSPC.into() && done != 0 => {
                // A later whole bucket does not fit.  Linux returns the
                // already-completed prefix and points at that bucket.
                break;
            }
            Err(error) => {
                terminal_error = Some(error);
                break;
            }
        };
        bucket = page.next_bucket;
        for (key, value) in page.entries {
            // Deletion, where requested, was committed as the bucket page was
            // detached.  Thus a fault below has Linux's observable ordering.
            write_user_bytes(
                m,
                (b.keys as usize)
                    .checked_add(done.checked_mul(key_size).ok_or(AxError::NoMemory)?)
                    .ok_or(AxError::InvalidInput)?,
                &key,
            )?;
            write_user_bytes(
                m,
                (b.values as usize)
                    .checked_add(done.checked_mul(value_size).ok_or(AxError::NoMemory)?)
                    .ok_or(AxError::InvalidInput)?,
                &value,
            )?;
            done += 1;
        }
        if page.exhausted {
            terminal = true;
            break;
        }
    }
    // A full result still needs the next *non-empty* bucket cursor.  Empty
    // buckets are skipped without snapshotting or deleting a later page.
    if done == count && !terminal && terminal_error.is_none() {
        loop {
            match f.map.hash_batch_page(bucket, 0, false) {
                Err(error) if error == LinuxError::ENOSPC.into() => break,
                Ok(page) => {
                    bucket = page.next_bucket;
                    if page.exhausted {
                        terminal = true;
                        break;
                    }
                }
                Err(error) => {
                    terminal_error = Some(error);
                    break;
                }
            }
        }
    }
    // Hash-family callbacks always publish their opaque continuation cursor
    // at the normal epilogue, including an empty terminal/ENOSPC page.
    write_user_bytes(m, b.out_batch as usize, &bucket.to_ne_bytes())?;
    write_count(m, a, s, done as u32)?;
    if let Some(error) = terminal_error {
        return Err(error);
    }
    if terminal {
        Err(LinuxError::ENOENT.into())
    } else {
        Ok(0)
    }
}

pub fn bpf_map_update_batch<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<isize> {
    let b = input(m, a, s)?;
    let f = BpfMapFd::from_fd(b.map_fd as _)?;
    let _write_active = crate::bpf::map::map_write_active(&*f.map)?;
    f.require_write()?;
    if !supports_batch(f.map.map_type(), BatchOp::Update) {
        return Err(AxError::OperationNotSupported);
    }
    element_flags(b)?;
    let (_key_bytes, _value_bytes, count) = ranges(b, &*f.map)?;
    if count == 0 {
        return Ok(0);
    }
    let key_size = f.map.key_size() as usize;
    let value_size = f.map.user_value_size();
    // Generic v6.18 update batches ignore batch.flags and process one copied
    // key/value pair at a time.  The early zero establishes the EFAULT rule.
    write_count(m, a, s, 0)?;
    let mut done = 0u32;
    for index in 0..count {
        let key = match read_user_bytes(
            m,
            (b.keys as usize)
                .checked_add(index.checked_mul(key_size).ok_or(AxError::NoMemory)?)
                .ok_or(AxError::InvalidInput)?,
            key_size,
        ) {
            Ok(key) => key,
            Err(error) => {
                write_count(m, a, s, done)?;
                return Err(error);
            }
        };
        let value = match read_user_bytes(
            m,
            (b.values as usize)
                .checked_add(index.checked_mul(value_size).ok_or(AxError::NoMemory)?)
                .ok_or(AxError::InvalidInput)?,
            value_size,
        ) {
            Ok(value) => value,
            Err(error) => {
                write_count(m, a, s, done)?;
                return Err(error);
            }
        };
        if let Err(error) = f.map.update_user(&key, &value, b.elem_flags) {
            write_count(m, a, s, done)?;
            return Err(error);
        }
        done += 1;
    }
    write_count(m, a, s, done)?;
    Ok(0)
}

pub fn bpf_map_delete_batch<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<isize> {
    let b = input(m, a, s)?;
    let f = BpfMapFd::from_fd(b.map_fd as _)?;
    let _write_active = crate::bpf::map::map_write_active(&*f.map)?;
    f.require_write()?;
    if !supports_batch(f.map.map_type(), BatchOp::Delete) {
        return Err(AxError::OperationNotSupported);
    }
    element_flags(b)?;
    let (_key_bytes, _, count) = ranges(b, &*f.map)?;
    if count == 0 {
        return Ok(0);
    }
    let key_size = f.map.key_size() as usize;
    write_count(m, a, s, 0)?;
    let mut done = 0u32;
    for index in 0..count {
        let key = match read_user_bytes(
            m,
            (b.keys as usize)
                .checked_add(index.checked_mul(key_size).ok_or(AxError::NoMemory)?)
                .ok_or(AxError::InvalidInput)?,
            key_size,
        ) {
            Ok(key) => key,
            Err(error) => {
                write_count(m, a, s, done)?;
                return Err(error);
            }
        };
        if let Err(error) = f.map.delete(&key) {
            write_count(m, a, s, done)?;
            return Err(error);
        }
        done += 1;
    }
    write_count(m, a, s, done)?;
    Ok(0)
}

pub fn bpf_map_lookup_batch<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
    delete: bool,
) -> AxResult<isize> {
    let b = input(m, a, s)?;
    let f = BpfMapFd::from_fd(b.map_fd as _)?;
    let _write_active = if delete {
        Some(crate::bpf::map::map_write_active(&*f.map)?)
    } else {
        None
    };
    f.require_read()?;
    if delete {
        f.require_write()?;
    }
    if !supports_batch(
        f.map.map_type(),
        if delete {
            BatchOp::LookupDelete
        } else {
            BatchOp::Lookup
        },
    ) {
        return Err(AxError::OperationNotSupported);
    }
    element_flags(b)?;
    let (_, _, count) = ranges(b, &*f.map)?;
    if count == 0 {
        return Ok(0);
    }
    let key_size = f.map.key_size() as usize;
    let hash_family = matches!(
        f.map.map_type(),
        crate::bpf::defs::BPF_MAP_TYPE_HASH
            | crate::bpf::defs::BPF_MAP_TYPE_LRU_HASH
            | crate::bpf::defs::BPF_MAP_TYPE_PERCPU_HASH
    );
    if hash_family {
        return hash_lookup_batch(m, a, s, b, &f, delete);
    }
    let mut cursor = if b.in_batch == 0 {
        None
    } else {
        Some(read_user_bytes(m, b.in_batch as usize, key_size)?)
    };
    // Generic providers ignore batch.flags.
    write_count(m, a, s, 0)?;
    let mut done = 0u32;
    let value_size = f.map.user_value_size();
    let mut exhausted = false;
    for _ in 0..count {
        let key = f.map.get_next_key(cursor.as_deref());
        let Some(key) = key else {
            exhausted = true;
            break;
        };
        let Some(value) = f.map.lookup_user(&key) else {
            // Generic lookup skips an element concurrently removed between
            // get-next-key and lookup, then continues the enumeration.
            cursor = Some(key);
            continue;
        };
        // Hash lookup-and-delete removes before copyout.  Keep that ordering
        // for every currently supported provider rather than inventing an
        // atomic output guarantee that Linux expressly does not make.
        if delete {
            if let Err(error) = f.map.delete(&key) {
                write_count(m, a, s, done)?;
                return Err(error);
            }
        }
        // `done`, not the scan iteration, defines compact output slots: a
        // concurrently removed key is skipped rather than leaving a hole.
        let slot = done as usize;
        write_user_bytes(
            m,
            (b.keys as usize)
                .checked_add(slot.checked_mul(key_size).ok_or(AxError::NoMemory)?)
                .ok_or(AxError::InvalidInput)?,
            &key,
        )?;
        write_user_bytes(
            m,
            (b.values as usize)
                .checked_add(slot.checked_mul(value_size).ok_or(AxError::NoMemory)?)
                .ok_or(AxError::InvalidInput)?,
            &value,
        )?;
        cursor = Some(key);
        done += 1;
    }
    // A nonzero result always has an out cursor; a null pointer must fault.
    // Generic exhaustion still publishes count/cursor before returning ENOENT.
    if done != 0 {
        write_user_bytes(m, b.out_batch as usize, cursor.as_deref().unwrap())?;
    }
    write_count(m, a, s, done)?;
    if exhausted {
        Err(LinuxError::ENOENT.into())
    } else {
        Ok(0)
    }
}
