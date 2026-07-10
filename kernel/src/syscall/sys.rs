use alloc::{format, string::String, vec, vec::Vec};
use core::ffi::c_char;

use axconfig::ARCH;
use axerrno::{AxError, AxResult, LinuxError};
use axhal::{time::monotonic_time, uspace::UserContext};
use axtask::current;
#[cfg(target_arch = "riscv64")]
use bytemuck::AnyBitPattern;
use linux_raw_sys::{
    general::{GRND_INSECURE, GRND_NONBLOCK, GRND_RANDOM, NGROUPS_MAX},
    system::{new_utsname, sysinfo},
};
#[cfg(target_arch = "riscv64")]
use starry_vm::VmPtr;
use starry_vm::{VmMutPtr, vm_load, vm_write_slice};

use super::sync::restart_futex_wait;
use crate::{
    mm::system_memory_stats,
    task::{AsThread, RestartBlock, UTS_FIELD_LEN, processes},
};

pub fn sys_getuid() -> AxResult<isize> {
    Ok(current().as_thread().proc_data.uid() as isize)
}

pub fn sys_geteuid() -> AxResult<isize> {
    Ok(current().as_thread().proc_data.euid() as isize)
}

pub fn sys_getresuid(ruid: *mut u32, euid: *mut u32, suid: *mut u32) -> AxResult<isize> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    if !ruid.is_null() {
        ruid.vm_write(proc_data.uid())?;
    }
    if !euid.is_null() {
        euid.vm_write(proc_data.euid())?;
    }
    if !suid.is_null() {
        suid.vm_write(proc_data.suid())?;
    }
    Ok(0)
}

pub fn sys_getgid() -> AxResult<isize> {
    Ok(current().as_thread().proc_data.gid() as isize)
}

pub fn sys_getegid() -> AxResult<isize> {
    Ok(current().as_thread().proc_data.egid() as isize)
}

pub fn sys_getresgid(rgid: *mut u32, egid: *mut u32, sgid: *mut u32) -> AxResult<isize> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    if !rgid.is_null() {
        rgid.vm_write(proc_data.gid())?;
    }
    if !egid.is_null() {
        egid.vm_write(proc_data.egid())?;
    }
    if !sgid.is_null() {
        sgid.vm_write(proc_data.sgid())?;
    }
    Ok(0)
}

pub fn sys_setuid(uid: u32) -> AxResult<isize> {
    debug!("sys_setuid <= uid: {uid}");
    current().as_thread().proc_data.setuid(uid)?;
    Ok(0)
}

pub fn sys_setgid(gid: u32) -> AxResult<isize> {
    debug!("sys_setgid <= gid: {gid}");
    current().as_thread().proc_data.setgid(gid)?;
    Ok(0)
}

pub fn sys_setfsuid(fsuid: u32) -> AxResult<isize> {
    Ok(current().as_thread().proc_data.setfsuid(fsuid) as isize)
}

pub fn sys_setfsgid(fsgid: u32) -> AxResult<isize> {
    Ok(current().as_thread().proc_data.setfsgid(fsgid) as isize)
}

pub fn sys_getgroups(size: usize, list: *mut u32) -> AxResult<isize> {
    debug!("sys_getgroups <= size: {size}");
    let groups = current().as_thread().proc_data.supplementary_groups();
    if size == 0 {
        return Ok(groups.len() as isize);
    }
    if size < groups.len() {
        return Err(AxError::InvalidInput);
    }
    if !groups.is_empty() {
        vm_write_slice(list, &groups)?;
    }
    Ok(groups.len() as isize)
}

pub fn sys_setgroups(size: usize, list: *const u32) -> AxResult<isize> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    if !proc_data.has_effective_capability(linux_raw_sys::general::CAP_SETGID) {
        return Err(AxError::OperationNotPermitted);
    }
    if size > NGROUPS_MAX as usize {
        return Err(AxError::InvalidInput);
    }
    let groups = if size == 0 {
        vec![]
    } else {
        vm_load(list, size)?
    };
    proc_data.set_supplementary_groups(groups);
    Ok(0)
}

const fn pad_str(info: &str) -> [c_char; 65] {
    let mut data: [c_char; 65] = [0; 65];
    // this needs #![feature(const_copy_from_slice)]
    // data[..info.len()].copy_from_slice(info.as_bytes());
    unsafe {
        core::ptr::copy_nonoverlapping(info.as_ptr().cast(), data.as_mut_ptr(), info.len());
    }
    data
}

const PER_MASK: u32 = 0xff;
const UNAME26: u32 = 0x0020_000;
const SUPPORTED_PERSONALITY: u32 = UNAME26;
const UNAME26_RELEASE: &[u8] = b"2.6.60";

fn fill_uts_field(dst: &mut [c_char; 65], src: &[u8]) {
    for (dst, byte) in dst.iter_mut().zip(src.iter()) {
        *dst = *byte as c_char;
    }
}

pub(crate) fn current_utsname() -> new_utsname {
    let mut utsname = new_utsname {
        sysname: pad_str("Linux"),
        nodename: [0; 65],
        release: pad_str("6.6.0"),
        version: pad_str("6.6.0"),
        machine: pad_str(ARCH),
        domainname: [0; 65],
    };
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let uts_ns = proc_data.uts_ns();
    fill_uts_field(&mut utsname.nodename, &uts_ns.nodename());
    fill_uts_field(&mut utsname.domainname, &uts_ns.domainname());
    utsname
}

pub(crate) fn proc_version_string() -> String {
    let utsname = current_utsname();
    let sysname = cstr_field_to_string(&utsname.sysname);
    let release = cstr_field_to_string(&utsname.release);
    let version = cstr_field_to_string(&utsname.version);
    let machine = cstr_field_to_string(&utsname.machine);
    format!("{sysname} version {release} ({machine}) {version}\n")
}

pub(crate) fn current_sysname_string() -> String {
    let utsname = current_utsname();
    cstr_field_to_string(&utsname.sysname)
}

pub(crate) fn current_release_string() -> String {
    let utsname = current_utsname();
    cstr_field_to_string(&utsname.release)
}

pub(crate) fn current_version_string() -> String {
    let utsname = current_utsname();
    cstr_field_to_string(&utsname.version)
}

pub(crate) fn current_machine_string() -> String {
    let utsname = current_utsname();
    cstr_field_to_string(&utsname.machine)
}

pub(crate) fn current_hostname_string() -> String {
    current()
        .as_thread()
        .proc_data
        .uts_ns()
        .nodename()
        .into_iter()
        .map(char::from)
        .collect()
}

pub(crate) fn current_domainname_string() -> String {
    current()
        .as_thread()
        .proc_data
        .uts_ns()
        .domainname()
        .into_iter()
        .map(char::from)
        .collect()
}

pub(crate) fn set_hostname_bytes(hostname: &[u8]) {
    current()
        .as_thread()
        .proc_data
        .uts_ns()
        .set_nodename(hostname);
}

pub(crate) fn set_domainname_bytes(domainname: &[u8]) {
    current()
        .as_thread()
        .proc_data
        .uts_ns()
        .set_domainname(domainname);
}

fn cstr_field_to_string(field: &[c_char; 65]) -> String {
    let len = field.iter().position(|&ch| ch == 0).unwrap_or(field.len());
    field[..len]
        .iter()
        .map(|&ch| ch as u8)
        .collect::<Vec<_>>()
        .into_iter()
        .map(char::from)
        .collect()
}

pub fn sys_uname(name: *mut new_utsname) -> AxResult<isize> {
    let mut uts = current_utsname();
    if current().as_thread().proc_data.personality() & UNAME26 != 0 {
        uts.release = [0; 65];
        fill_uts_field(&mut uts.release, UNAME26_RELEASE);
    }
    name.vm_write(uts)?;
    Ok(0)
}

pub fn sys_sethostname(name: *const u8, len: usize) -> AxResult<isize> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    if !proc_data.has_effective_capability(linux_raw_sys::general::CAP_SYS_ADMIN) {
        return Err(AxError::OperationNotPermitted);
    }
    if len > UTS_FIELD_LEN {
        return Err(AxError::InvalidInput);
    }
    if name.is_null() {
        return Err(AxError::BadAddress);
    }
    let hostname = vm_load(name, len)?;
    set_hostname_bytes(&hostname);
    Ok(0)
}

pub fn sys_setdomainname(name: *const u8, len: usize) -> AxResult<isize> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    if !proc_data.has_effective_capability(linux_raw_sys::general::CAP_SYS_ADMIN) {
        return Err(AxError::OperationNotPermitted);
    }
    if len > UTS_FIELD_LEN {
        return Err(AxError::InvalidInput);
    }
    if name.is_null() {
        return Err(AxError::BadAddress);
    }
    let domainname = vm_load(name, len)?;
    set_domainname_bytes(&domainname);
    Ok(0)
}

pub fn sys_sysinfo(info: *mut sysinfo) -> AxResult<isize> {
    // FIXME: Zeroable
    let mut kinfo: sysinfo = unsafe { core::mem::zeroed() };
    let stats = system_memory_stats();
    let uptime = current()
        .as_thread()
        .proc_data
        .time_ns()
        .apply_boottime_offset(monotonic_time());
    let uptime_secs = uptime
        .as_secs()
        .saturating_add(u64::from(uptime.subsec_nanos() != 0));
    kinfo.uptime = uptime_secs.min(i64::MAX as u64) as _;
    kinfo.procs = processes().len() as _;
    kinfo.totalram = stats.total_bytes as _;
    kinfo.freeram = stats.free_bytes as _;
    // axfs uses a page cache, not Linux's separate block-buffer cache.
    kinfo.bufferram = 0;
    kinfo.mem_unit = 1;
    info.vm_write(kinfo)?;
    Ok(0)
}

pub fn sys_personality(persona: u32) -> AxResult<isize> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let old = proc_data.personality();

    if persona == u32::MAX {
        return Ok(old as isize);
    }

    if persona & PER_MASK != 0 || persona & !SUPPORTED_PERSONALITY != 0 {
        return Err(AxError::InvalidInput);
    }

    proc_data.set_personality(persona);
    Ok(old as isize)
}

pub fn sys_syslog(_kind: i32, _buf: *mut c_char, _len: isize) -> AxResult<isize> {
    Err(LinuxError::ENOSYS.into())
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct GetRandomFlags: u32 {
        const NONBLOCK = GRND_NONBLOCK;
        const RANDOM = GRND_RANDOM;
        const INSECURE = GRND_INSECURE;
    }
}

pub fn sys_getrandom(buf: *mut u8, len: usize, flags: u32) -> AxResult<isize> {
    const GETRANDOM_CHUNK_SIZE: usize = 4096;

    let flags = GetRandomFlags::from_bits(flags).ok_or(AxError::InvalidInput)?;
    if flags.contains(GetRandomFlags::RANDOM) && flags.contains(GetRandomFlags::INSECURE) {
        return Err(AxError::InvalidInput);
    }
    if len == 0 {
        return Ok(0);
    }

    debug!("sys_getrandom <= buf: {buf:p}, len: {len}, flags: {flags:?}");

    let mut total = 0;
    let mut kbuf = [0u8; GETRANDOM_CHUNK_SIZE];
    while total < len {
        let chunk = (len - total).min(kbuf.len());
        let fill_result = if flags.contains(GetRandomFlags::INSECURE) {
            crate::random::fill_insecure(&mut kbuf[..chunk]);
            Ok(())
        } else {
            crate::random::fill_secure(&mut kbuf[..chunk])
        };
        if let Err(error) = fill_result {
            return if total == 0 {
                Err(error.into())
            } else {
                Ok(total as isize)
            };
        }
        if let Err(error) = vm_write_slice(buf.wrapping_add(total), &kbuf[..chunk]) {
            return if total == 0 {
                Err(error.into())
            } else {
                Ok(total as isize)
            };
        }
        total += chunk;
    }

    Ok(total as isize)
}

pub fn sys_seccomp(_op: u32, _flags: u32, _args: *const ()) -> AxResult<isize> {
    Err(LinuxError::ENOSYS.into())
}

pub fn sys_restart_syscall(uctx: &UserContext) -> AxResult<isize> {
    let curr = current();
    let thr = curr.as_thread();
    let Some(block) = thr.begin_restart_syscall(uctx) else {
        return Err(AxError::InvalidInput);
    };
    match block {
        RestartBlock::FutexWait(block) => restart_futex_wait(block),
    }
}

#[cfg(target_arch = "riscv64")]
#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
pub struct RiscvHwprobe {
    key: i64,
    value: u64,
}

#[cfg(target_arch = "riscv64")]
const RISCV_HWPROBE_KEY_BASE_BEHAVIOR: i64 = 3;
#[cfg(target_arch = "riscv64")]
const RISCV_HWPROBE_BASE_BEHAVIOR_IMA: u64 = 1 << 0;
#[cfg(target_arch = "riscv64")]
const RISCV_HWPROBE_KEY_IMA_EXT_0: i64 = 4;

#[cfg(target_arch = "riscv64")]
pub fn sys_riscv_hwprobe(
    pairs: *mut RiscvHwprobe,
    pair_count: usize,
    cpu_set_size: usize,
    cpus: *mut usize,
    flags: u32,
) -> AxResult<isize> {
    debug!(
        "sys_riscv_hwprobe <= pairs: {pairs:p}, pair_count: {pair_count}, cpu_set_size: \
         {cpu_set_size}, cpus: {cpus:p}, flags: {flags:#x}"
    );

    if flags != 0 {
        return Err(AxError::InvalidInput);
    }
    if cpu_set_size != 0 || !cpus.is_null() {
        return Err(AxError::Unsupported);
    }
    if pair_count == 0 {
        return Ok(0);
    }
    if pairs.is_null() {
        return Err(AxError::BadAddress);
    }

    for index in 0..pair_count {
        let ptr = pairs.wrapping_add(index);
        let mut pair = ptr.vm_read()?;
        match pair.key {
            RISCV_HWPROBE_KEY_BASE_BEHAVIOR => {
                pair.value = RISCV_HWPROBE_BASE_BEHAVIOR_IMA;
            }
            RISCV_HWPROBE_KEY_IMA_EXT_0 => {
                pair.value = 0;
            }
            _ => {
                pair.key = -1;
                pair.value = 0;
            }
        }
        ptr.vm_write(pair)?;
    }

    Ok(0)
}

#[cfg(target_arch = "riscv64")]
pub fn sys_riscv_flush_icache() -> AxResult<isize> {
    riscv::asm::fence_i();
    Ok(0)
}
