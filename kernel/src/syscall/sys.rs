use alloc::{format, string::String, vec, vec::Vec};
use core::ffi::c_char;

use axconfig::ARCH;
use axerrno::{AxError, AxResult};
use axfs::FS_CONTEXT;
use axhal::uspace::UserContext;
use axtask::current;
#[cfg(target_arch = "riscv64")]
use bytemuck::AnyBitPattern;
use kspin::SpinNoIrq;
use linux_raw_sys::{
    general::{CAP_SYS_ADMIN, CAP_SYSLOG, GRND_INSECURE, GRND_NONBLOCK, GRND_RANDOM, NGROUPS_MAX},
    system::{new_utsname, sysinfo},
};
#[cfg(target_arch = "riscv64")]
use starry_vm::VmPtr;
use starry_vm::{VmMutPtr, vm_load, vm_write_slice};

use super::sync::restart_futex_wait;
use crate::{
    mm::system_memory_stats,
    task::{AsThread, RestartBlock, processes},
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

const UTS_FIELD_LEN: usize = 64;

#[derive(Clone, Copy)]
struct UtsState {
    nodename: [u8; UTS_FIELD_LEN],
    nodename_len: usize,
    domainname: [u8; UTS_FIELD_LEN],
    domainname_len: usize,
}

const fn copy_uts_field(dst: &mut [u8; UTS_FIELD_LEN], src: &[u8]) -> usize {
    let len = if src.len() < UTS_FIELD_LEN {
        src.len()
    } else {
        UTS_FIELD_LEN
    };
    let mut index = 0;
    while index < len {
        dst[index] = src[index];
        index += 1;
    }
    len
}

const fn init_uts_state() -> UtsState {
    let mut state = UtsState {
        nodename: [0; UTS_FIELD_LEN],
        nodename_len: 0,
        domainname: [0; UTS_FIELD_LEN],
        domainname_len: 0,
    };
    state.nodename_len = copy_uts_field(&mut state.nodename, b"starry");
    state.domainname_len = copy_uts_field(
        &mut state.domainname,
        b"https://github.com/Starry-OS/StarryOS",
    );
    state
}

impl UtsState {
    fn set_nodename(&mut self, value: &[u8]) {
        self.nodename = [0; UTS_FIELD_LEN];
        self.nodename_len = copy_uts_field(&mut self.nodename, value);
    }

    fn set_domainname(&mut self, value: &[u8]) {
        self.domainname = [0; UTS_FIELD_LEN];
        self.domainname_len = copy_uts_field(&mut self.domainname, value);
    }
}

static UTS_STATE: SpinNoIrq<UtsState> = SpinNoIrq::new(init_uts_state());
const PER_LINUX: u32 = 0;
const PER_MASK: u32 = 0xff;
const UNAME26: u32 = 0x0020_000;
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
        release: pad_str("10.0.0"),
        version: pad_str("10.0.0"),
        machine: pad_str(ARCH),
        domainname: [0; 65],
    };
    let state = UTS_STATE.lock();
    fill_uts_field(&mut utsname.nodename, &state.nodename[..state.nodename_len]);
    fill_uts_field(
        &mut utsname.domainname,
        &state.domainname[..state.domainname_len],
    );
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

pub(crate) fn current_domainname_string() -> String {
    let state = UTS_STATE.lock();
    state.domainname[..state.domainname_len]
        .iter()
        .copied()
        .map(char::from)
        .collect()
}

pub(crate) fn set_domainname_bytes(domainname: &[u8]) {
    UTS_STATE.lock().set_domainname(domainname);
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
    if len > UTS_FIELD_LEN {
        return Err(AxError::InvalidInput);
    }
    if name.is_null() {
        return Err(AxError::BadAddress);
    }
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    if !proc_data.has_effective_capability(linux_raw_sys::general::CAP_SYS_ADMIN) {
        return Err(AxError::OperationNotPermitted);
    }
    let hostname = vm_load(name, len)?;
    UTS_STATE.lock().set_nodename(&hostname);
    Ok(0)
}

pub fn sys_setdomainname(name: *const u8, len: usize) -> AxResult<isize> {
    if len > UTS_FIELD_LEN {
        return Err(AxError::InvalidInput);
    }
    if name.is_null() {
        return Err(AxError::BadAddress);
    }
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    if !proc_data.has_effective_capability(linux_raw_sys::general::CAP_SYS_ADMIN) {
        return Err(AxError::OperationNotPermitted);
    }
    let domainname = vm_load(name, len)?;
    set_domainname_bytes(&domainname);
    Ok(0)
}

pub fn sys_sysinfo(info: *mut sysinfo) -> AxResult<isize> {
    // FIXME: Zeroable
    let mut kinfo: sysinfo = unsafe { core::mem::zeroed() };
    let stats = system_memory_stats();
    kinfo.procs = processes().len() as _;
    kinfo.totalram = stats.total_bytes as _;
    kinfo.freeram = stats.free_bytes as _;
    kinfo.bufferram = stats.cached_bytes as _;
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

    if persona & !(PER_MASK | UNAME26) != 0 || (persona & PER_MASK) != PER_LINUX {
        return Err(AxError::InvalidInput);
    }

    proc_data.set_personality(persona);
    Ok(old as isize)
}

pub fn sys_syslog(kind: i32, buf: *mut c_char, len: isize) -> AxResult<isize> {
    if !(0..=10).contains(&kind) {
        return Err(AxError::InvalidInput);
    }
    if len < 0 {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let privileged = proc_data.has_effective_capability(CAP_SYSLOG)
        || proc_data.has_effective_capability(CAP_SYS_ADMIN);

    match kind {
        2 | 3 | 4 => {
            if !privileged {
                return Err(AxError::OperationNotPermitted);
            }
            if buf.is_null() {
                return Err(AxError::InvalidInput);
            }
        }
        8 => {
            if !privileged {
                return Err(AxError::OperationNotPermitted);
            }
            if len > 8 {
                return Err(AxError::InvalidInput);
            }
        }
        _ => {}
    }

    Ok(0)
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
    if len == 0 {
        return Ok(0);
    }
    let flags = GetRandomFlags::from_bits(flags).ok_or(AxError::InvalidInput)?;

    debug!("sys_getrandom <= buf: {buf:p}, len: {len}, flags: {flags:?}");

    let path = if flags.contains(GetRandomFlags::RANDOM) {
        "/dev/random"
    } else {
        "/dev/urandom"
    };

    let f = FS_CONTEXT.lock().resolve(path)?;
    let mut kbuf = vec![0; len];
    let len = f.entry().as_file()?.read_at(&mut kbuf, 0)?;

    vm_write_slice(buf, &kbuf)?;

    Ok(len as _)
}

pub fn sys_seccomp(_op: u32, _flags: u32, _args: *const ()) -> AxResult<isize> {
    warn!("dummy sys_seccomp");
    Ok(0)
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
