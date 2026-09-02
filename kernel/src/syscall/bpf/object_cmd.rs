//! BTF, token, iterator, statistics, and program-binding commands.
use alloc::sync::Arc;
use core::mem::{offset_of, size_of};

use axerrno::{AxError, AxResult, LinuxError};
use linux_raw_sys::general::CAP_SYS_ADMIN;
use thekernel_linux_bpf::BpfAttrEnableStats;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext};

use crate::{
    bpf::{
        btf, defs::*, read_bpf_attr, read_user_bytes, require_bpf_attr_range, write_bpf_attr_value,
        write_user_bytes,
    },
    bpf_security::{BpfAuthority, authorize_token_btf_load},
    file::{
        Directory, FileDescription, FileLike,
        bpf::{BpfBtfFd, BpfIterFd, BpfIterLink, BpfProgFd, BpfTokenFd},
        get_typed_file, reserve_fd,
    },
    task::AsThread,
};
const BPF_F_TOKEN_FD: u32 = 1 << 16;
const BPF_STATS_RUN_TIME: u32 = 0;
const BTF_MAX_SIZE: u32 = 16 * 1024 * 1024;
const BPF_LOG_MAX_SIZE: u32 = u32::MAX >> 2;
// Linux's BPF_LOG_LEVEL1, BPF_LOG_LEVEL2, BPF_LOG_STATS and BPF_LOG_FIXED.
const BTF_LOG_LEVEL_MASK: u32 = 0x0f;
pub fn bpf_enable_stats<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrEnableStats>(s, size_of::<BpfAttrEnableStats>())?;
    let x: BpfAttrEnableStats = read_bpf_attr(m, a, s)?;
    if x.stats_type != BPF_STATS_RUN_TIME {
        return Err(AxError::InvalidInput);
    }
    if !axtask::current()
        .as_thread()
        .has_effective_capability(CAP_SYS_ADMIN)
    {
        return Err(AxError::OperationNotPermitted);
    }
    crate::file::bpf::BpfStatsFd::new()
        .add_to_fd_table(true)
        .map(|fd| fd as isize)
}
#[repr(C)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern)]
struct IdQuery {
    start_id: u32,
    next_id: u32,
    open_flags: u32,
    fd_by_id_token_fd: i32,
}

pub fn bpf_btf_load<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrBtfLoad>(
        s,
        offset_of!(BpfAttrBtfLoad, btf_size) + size_of::<u32>(),
    )?;
    let x: BpfAttrBtfLoad = read_bpf_attr(m, a, s)?;
    if x.btf_flags & !BPF_F_TOKEN_FD != 0 {
        return Err(AxError::InvalidInput);
    }
    let token_authorized = if x.btf_flags & BPF_F_TOKEN_FD != 0 {
        let token = get_typed_file::<BpfTokenFd>(x.btf_token_fd)?;
        // A token grants an alternate authority path.  An ineligible token
        // does not revoke the caller's ordinary CAP_BPF authority.
        authorize_token_btf_load(&token.grant).is_ok()
    } else {
        false
    };
    if !token_authorized && !BpfAuthority::current().bpf_capable() {
        // `btf_token_fd` is reserved input unless its flag opts into token
        // authorization.  In particular a zero-filled ordinary attr is not a
        // request to look up stdin as a token.
        return Err(AxError::OperationNotPermitted);
    }
    if x.btf_size > BTF_MAX_SIZE {
        return Err(AxError::ArgumentListTooLong);
    }
    if x.btf_flags != 0 && x.btf_flags != BPF_F_TOKEN_FD {
        return Err(AxError::InvalidInput);
    }
    validate_btf_log_request(&x)?;

    // Retain only copied BTF bytes, never the user log pointer.  Parsing is
    // intentionally before log copyout so a bad log pointer wins over the
    // eventual parser failure, matching the BPF verifier ABI.
    let parsed = match read_user_bytes(m, x.btf as usize, x.btf_size as usize) {
        Ok(bytes) => btf::parse(bytes, x.btf_log_size, x.btf_log_level & (1 << 3) != 0),
        // btf_parse initializes the vlog before copying the payload, so even
        // this EFAULT completes the two-pass true-size/log protocol.
        Err(error) => Err((
            error,
            btf::BtfDiagnostic::for_log(x.btf_log_size, x.btf_log_level & (1 << 3) != 0),
        )),
    };
    let diagnostic = match &parsed {
        Ok(parsed) => parsed.diagnostic(),
        Err((_, diagnostic)) => diagnostic,
    };
    let log_short = write_btf_load_log(m, a, s, &x, diagnostic)?;
    // btf_parse's errout path intentionally overwrites the parser or input
    // error with verifier-log ENOSPC, while an actual log/attr copy EFAULT was
    // already returned by `write_btf_load_log` above.
    if log_short {
        return Err(LinuxError::ENOSPC.into());
    }
    let parsed = parsed.map_err(|(error, _)| error)?;

    // Prepare every fallible FD-table step before publishing the BTF ID.  The
    // remaining fd commit is infallible, so an ID can never be enumerated or
    // opened before its first descriptor is installable; any earlier failure
    // drops the reservation and the unpublished object.
    let object = btf::prepare(parsed)?;
    let file = Arc::try_new(BpfBtfFd::new(Arc::clone(&object))).map_err(|_| AxError::NoMemory)?;
    let description = FileDescription::new(file)?;
    let fd = reserve_fd(false)?.prepare_publication(description)?;
    btf::publish(&object)?;
    Ok(fd.commit() as isize)
}

fn validate_btf_log_request(x: &BpfAttrBtfLoad) -> AxResult<()> {
    if x.btf_log_level & !BTF_LOG_LEVEL_MASK != 0 {
        return Err(AxError::InvalidInput);
    }
    if x.btf_log_size > BPF_LOG_MAX_SIZE {
        return Err(AxError::InvalidInput);
    }
    if x.btf_log_level == 0 {
        if x.btf_log_buf != 0 || x.btf_log_size != 0 {
            return Err(AxError::InvalidInput);
        }
    } else if (x.btf_log_buf != 0) != (x.btf_log_size != 0) {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

/// Linux finalizes the NUL-terminated verifier log first, then copies
/// `btf_log_true_size` into the attr.  The latter copy is attempted even if
/// log finalization failed and its EFAULT overrides the earlier failure.
fn write_btf_load_log<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
    attr: &BpfAttrBtfLoad,
    diagnostic: &btf::BtfDiagnostic,
) -> AxResult<bool> {
    let true_size = if attr.btf_log_level == 0 {
        0
    } else {
        diagnostic.true_size()
    };
    let log_result = copy_btf_load_log(memory, attr, diagnostic, true_size);
    let true_size_end = offset_of!(BpfAttrBtfLoad, btf_log_true_size) + size_of::<u32>();
    if (attr_size as usize) >= true_size_end {
        write_bpf_attr_value::<BpfAttrBtfLoad, _, _>(
            memory,
            attr_ptr,
            attr_size,
            offset_of!(BpfAttrBtfLoad, btf_log_true_size),
            &true_size,
        )?;
    }
    log_result
}

fn copy_btf_load_log<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr: &BpfAttrBtfLoad,
    diagnostic: &btf::BtfDiagnostic,
    true_size: u32,
) -> AxResult<bool> {
    if attr.btf_log_level == 0 {
        return Ok(false);
    }

    // (buf,size)==(0,0) is the legal two-pass query.  There is no user log
    // copy to truncate, so finalization must preserve the parser/input errno.
    if attr.btf_log_size == 0 {
        return Ok(false);
    }
    // An empty retained window is not an empty verifier stream: a one-byte
    // log, or best-effort window allocation failure, must still receive its
    // NUL terminator and report ENOSPC from the logical true size.
    if true_size == 0 {
        return Ok(false);
    }
    let copied = diagnostic.window_len();
    // BtfDiagnostic has already retained the fixed prefix or rotating suffix
    // selected by this request; it never accumulates the complete stream.
    let (first, second) = diagnostic.window_slices();
    if !first.is_empty() {
        write_user_bytes(memory, attr.btf_log_buf as usize, first)?;
    }
    if !second.is_empty() {
        let second_at = (attr.btf_log_buf as usize)
            .checked_add(first.len())
            .ok_or(AxError::BadAddress)?;
        write_user_bytes(memory, second_at, second)?;
    }
    let terminator = (attr.btf_log_buf as usize)
        .checked_add(copied)
        .ok_or(AxError::BadAddress)?;
    write_user_bytes(memory, terminator, &[0])?;
    Ok(true_size > attr.btf_log_size)
}
pub fn bpf_btf_get_next_id<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<IdQuery>(s, size_of::<IdQuery>())?;
    let x: IdQuery = read_bpf_attr(m, a, s)?;
    if x.open_flags != 0 || x.fd_by_id_token_fd != 0 {
        return Err(AxError::InvalidInput);
    }
    let id = btf::next_id(x.start_id).ok_or(LinuxError::ENOENT)?;
    write_bpf_attr_value::<IdQuery, _, _>(m, a, s, offset_of!(IdQuery, next_id), &id)?;
    Ok(0)
}
pub fn bpf_btf_get_fd_by_id<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<IdQuery>(s, size_of::<IdQuery>())?;
    let x: IdQuery = read_bpf_attr(m, a, s)?;
    if x.open_flags != 0 {
        return Err(AxError::InvalidInput);
    }
    if x.fd_by_id_token_fd == 0 {
        if !BpfAuthority::current().bpf_capable() {
            return Err(AxError::OperationNotPermitted);
        }
    } else if x.fd_by_id_token_fd < 0 {
        return Err(AxError::BadFileDescriptor);
    } else {
        get_typed_file::<BpfTokenFd>(x.fd_by_id_token_fd)?
            .grant
            .authorize_by_id_lookup()?;
    }
    BpfBtfFd::new(btf::by_id(x.start_id).ok_or(LinuxError::ENOENT)?)
        .add_to_fd_table(false)
        .map(|fd| fd as isize)
}
pub fn bpf_token_create<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrTokenCreate>(s, size_of::<BpfAttrTokenCreate>())?;
    let x: BpfAttrTokenCreate = read_bpf_attr(m, a, s)?;
    if x.flags != 0 {
        return Err(AxError::InvalidInput);
    }
    let authority = BpfAuthority::current();
    if !authority.bpf_capable() {
        return Err(AxError::OperationNotPermitted);
    }
    let anchor = get_typed_file::<Directory>(x.bpffs_fd as i32)?.clone_object();
    // A token is rooted in a bpffs directory, never merely in an arbitrary
    // directory FD.  Retaining this exact opened directory makes rename,
    // cwd changes and path races irrelevant to later delegated admissions.
    if crate::mounts::metadata_for_location(anchor.inner())?.fs_type != "bpf" {
        return Err(LinuxError::EINVAL.into());
    }
    BpfTokenFd::new(authority, anchor)
        .add_to_fd_table(false)
        .map(|fd| fd as isize)
}
pub fn bpf_iter_create<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrIterCreate>(s, size_of::<BpfAttrIterCreate>())?;
    let x: BpfAttrIterCreate = read_bpf_attr(m, a, s)?;
    if x.flags != 0 {
        return Err(AxError::InvalidInput);
    }
    let link = get_typed_file::<BpfIterLink>(x.link_fd as i32)?;
    BpfIterFd::from_link(&link)
        .and_then(|iter| iter.add_to_fd_table(false))
        .map(|fd| fd as isize)
}
pub fn bpf_prog_bind_map<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrProgBindMap>(s, size_of::<BpfAttrProgBindMap>())?;
    let x: BpfAttrProgBindMap = read_bpf_attr(m, a, s)?;
    if x.flags != 0 {
        return Err(AxError::InvalidInput);
    }
    let program = get_typed_file::<BpfProgFd>(x.prog_fd as i32)?.prog.clone();
    let map = get_typed_file::<crate::file::bpf::BpfMapFd>(x.map_fd as i32)?;
    program.bind_map(map.map.clone(), map.memory_charge.clone())?;
    Ok(0)
}

pub fn bpf_prog_stream_read_by_fd<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrProgStreamRead>(
        s,
        offset_of!(BpfAttrProgStreamRead, prog_fd) + size_of::<u32>(),
    )?;
    let x: BpfAttrProgStreamRead = read_bpf_attr(m, a, s)?;
    let program = get_typed_file::<BpfProgFd>(x.prog_fd as i32)?.prog.clone();
    // Serialize readers through the program stream lock.  Consumption is
    // committed only after the entire userspace copy succeeds, so EFAULT does
    // not silently discard diagnostic bytes.
    let mut stream = program.stream(x.stream_id)?;
    let bytes = stream.snapshot(x.stream_buf_len as usize)?;
    if !bytes.is_empty() {
        write_user_bytes(m, x.stream_buf as usize, &bytes)?;
    }
    stream.consume(bytes.len());
    Ok(bytes.len() as isize)
}
