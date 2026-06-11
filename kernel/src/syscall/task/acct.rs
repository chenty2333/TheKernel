use alloc::vec::Vec;
use core::ffi::c_char;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::NodeType;
use axhal::time::{NANOS_PER_SEC, monotonic_time_nanos};
use axio::Cursor;
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::general::{CAP_SYS_PACCT, O_APPEND, O_CLOEXEC, O_WRONLY};

use super::super::fs::openat_inner;
use crate::{
    file::{File, FileLike, close_file_like},
    mm::vm_load_string,
    task::{AsThread, ProcessData, TaskUsage},
};

const ACCT_COMM: usize = 16;
const ACCT_RECORD_SIZE: usize = 64;
const ACCT_HZ: u64 = 100;
const COMP_T_MANTISSA_BITS: u32 = 13;
const COMP_T_EXPONENT_SHIFT: u32 = 3;
const COMP_T_MAX_FRACTION: u64 = (1 << COMP_T_MANTISSA_BITS) - 1;

static PROCESS_ACCOUNTING: Mutex<Option<AccountingFile>> = Mutex::new(None);

#[derive(Clone)]
struct AccountingFile {
    file: crate::file::FileHandle<File>,
    uid: u32,
    gid: u32,
}

fn current_has_pacct_capability() -> bool {
    current()
        .as_thread()
        .proc_data
        .has_effective_capability(CAP_SYS_PACCT)
}

fn encode_comp_t(mut value: u64) -> u16 {
    let mut exp = 0u16;
    let mut round = false;
    while value > COMP_T_MAX_FRACTION {
        round = value & (1 << (COMP_T_EXPONENT_SHIFT - 1)) != 0;
        value >>= COMP_T_EXPONENT_SHIFT;
        exp = exp.saturating_add(1);
    }
    if round {
        value += 1;
        if value > COMP_T_MAX_FRACTION {
            value >>= COMP_T_EXPONENT_SHIFT;
            exp = exp.saturating_add(1);
        }
    }

    let max_exp = u16::MAX >> COMP_T_MANTISSA_BITS;
    if exp > max_exp {
        return u16::MAX;
    }
    (exp << COMP_T_MANTISSA_BITS) | value as u16
}

fn nanos_to_acct_ticks(nanos: u64) -> u64 {
    nanos.saturating_mul(ACCT_HZ).saturating_div(NANOS_PER_SEC)
}

fn append_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_ne_bytes());
}

fn append_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_ne_bytes());
}

fn build_record(
    proc_data: &ProcessData,
    command: &str,
    usage: TaskUsage,
    exit_code: i32,
    owner_uid: u32,
    owner_gid: u32,
) -> [u8; ACCT_RECORD_SIZE] {
    let elapsed_ns = monotonic_time_nanos().saturating_sub(proc_data.start_monotonic_ns());
    let elapsed_ticks = nanos_to_acct_ticks(elapsed_ns);
    let btime = proc_data.start_realtime_sec().min(u32::MAX as u64) as u32;

    let mut out = Vec::with_capacity(ACCT_RECORD_SIZE);
    out.push(0); // ac_flag
    append_u16(&mut out, owner_uid.min(u16::MAX as u32) as u16);
    append_u16(&mut out, owner_gid.min(u16::MAX as u32) as u16);
    append_u16(&mut out, 0); // ac_tty
    append_u32(&mut out, btime);
    append_u16(&mut out, encode_comp_t(nanos_to_acct_ticks(usage.utime_ns)));
    append_u16(&mut out, encode_comp_t(nanos_to_acct_ticks(usage.stime_ns)));
    append_u16(&mut out, encode_comp_t(elapsed_ticks));
    append_u16(&mut out, 0); // ac_mem
    append_u16(&mut out, 0); // ac_io
    append_u16(&mut out, 0); // ac_rw
    append_u16(&mut out, 0); // ac_minflt
    append_u16(&mut out, 0); // ac_majflt
    append_u16(&mut out, 0); // ac_swaps
    append_u32(&mut out, exit_code as u32);

    let mut comm = [0u8; ACCT_COMM + 1];
    let bytes = command.as_bytes();
    let copy_len = bytes.len().min(ACCT_COMM);
    comm[..copy_len].copy_from_slice(&bytes[..copy_len]);
    out.extend_from_slice(&comm);
    out.extend_from_slice(&[0u8; 10]);

    let mut record = [0u8; ACCT_RECORD_SIZE];
    record.copy_from_slice(&out[..ACCT_RECORD_SIZE]);
    record
}

pub fn acct_process_exit(proc_data: &ProcessData, exit_code: i32, usage: TaskUsage) {
    let Some(accounting) = PROCESS_ACCOUNTING.lock().clone() else {
        return;
    };

    let command = current().name();
    let record = build_record(
        proc_data,
        &command,
        usage,
        exit_code,
        accounting.uid,
        accounting.gid,
    );
    let mut cursor = Cursor::new(record.as_slice());
    let _ = accounting.file.write(&mut cursor);
}

pub fn sys_acct(name: *const c_char) -> AxResult<isize> {
    if !current_has_pacct_capability() {
        return Err(AxError::OperationNotPermitted);
    }

    if name.is_null() {
        *PROCESS_ACCOUNTING.lock() = None;
        return Ok(0);
    }

    let path = vm_load_string(name)?;
    let fd = openat_inner(
        linux_raw_sys::general::AT_FDCWD as _,
        &path,
        (O_WRONLY | O_APPEND | O_CLOEXEC) as i32,
        0,
    )? as i32;
    let file = match File::from_fd(fd) {
        Ok(file) => file,
        Err(err) => {
            let _ = close_file_like(fd);
            return Err(err);
        }
    };
    let stat = match file.inner().location().metadata() {
        Ok(stat) => stat,
        Err(err) => {
            let _ = close_file_like(fd);
            return Err(err.into());
        }
    };
    if stat.node_type != NodeType::RegularFile {
        let _ = close_file_like(fd);
        return Err(AxError::from(LinuxError::EACCES));
    }

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let accounting = AccountingFile {
        file,
        uid: proc_data.uid(),
        gid: proc_data.gid(),
    };
    let _ = close_file_like(fd);
    *PROCESS_ACCOUNTING.lock() = Some(accounting);
    Ok(0)
}
