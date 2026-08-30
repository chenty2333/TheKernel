use alloc::{format, string::String, vec, vec::Vec};
use core::{
    ffi::c_char,
    mem::{align_of, offset_of, size_of},
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::{time::monotonic_time, uspace::UserContext};
use axtask::current;
use linux_raw_sys::{
    general::{GRND_INSECURE, GRND_NONBLOCK, GRND_RANDOM, NGROUPS_MAX},
    system::{new_utsname, sysinfo},
};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmMutPtr, VmPtr, vm_write_slice};

use super::sync::restart_futex_wait;
use crate::{
    mm::{map_usercopy_error, shmem_resident_pages, system_memory_stats},
    task::{
        AsThread, Kgid, RestartBlock, UTS_FIELD_LEN, has_pending_syscall_signal, live_thread_count,
        load_average_sample_now, load_average_sysinfo, ns_capable,
    },
};

// These generated UAPI structs do not carry bytemuck's object-representation
// markers.  Keep the x86_64 Linux layouts checked before using the explicit
// usercopy unchecked path for their fully initialized values.
const _: () = {
    assert!(align_of::<new_utsname>() == 1);
    assert!(size_of::<new_utsname>() == 390);
    assert!(offset_of!(new_utsname, sysname) == 0);
    assert!(offset_of!(new_utsname, nodename) == 65);
    assert!(offset_of!(new_utsname, release) == 130);
    assert!(offset_of!(new_utsname, version) == 195);
    assert!(offset_of!(new_utsname, machine) == 260);
    assert!(offset_of!(new_utsname, domainname) == 325);
    assert!(align_of::<sysinfo>() == 8);
    assert!(size_of::<sysinfo>() == 112);
    assert!(offset_of!(sysinfo, uptime) == 0);
    assert!(offset_of!(sysinfo, loads) == 8);
    assert!(offset_of!(sysinfo, totalram) == 32);
    assert!(offset_of!(sysinfo, freeram) == 40);
    assert!(offset_of!(sysinfo, sharedram) == 48);
    assert!(offset_of!(sysinfo, bufferram) == 56);
    assert!(offset_of!(sysinfo, totalswap) == 64);
    assert!(offset_of!(sysinfo, freeswap) == 72);
    assert!(offset_of!(sysinfo, procs) == 80);
    assert!(offset_of!(sysinfo, pad) == 82);
    assert!(offset_of!(sysinfo, totalhigh) == 88);
    assert!(offset_of!(sysinfo, freehigh) == 96);
    assert!(offset_of!(sysinfo, mem_unit) == 104);
};

const SYSINFO_MEM_UNIT: u32 = 1;

/// Linux reports only `NR_SHMEM` in `sharedram`; ordinary file cache does not
/// contribute to this field.
fn sysinfo_sharedram_bytes(shmem_pages: usize) -> usize {
    shmem_pages.saturating_mul(memory_addr::PAGE_SIZE_4K)
}

fn set_sysinfo_memory_fields(
    kinfo: &mut sysinfo,
    total_bytes: usize,
    free_bytes: usize,
    shmem_pages: usize,
) {
    kinfo.totalram = total_bytes as _;
    kinfo.freeram = free_bytes as _;
    kinfo.sharedram = sysinfo_sharedram_bytes(shmem_pages) as _;
    // axfs uses a page cache, not Linux's separate block-buffer cache.
    kinfo.bufferram = 0;
    kinfo.mem_unit = SYSINFO_MEM_UNIT;
}

fn write_sysinfo<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    info: *mut sysinfo,
    kinfo: sysinfo,
) -> AxResult {
    // SAFETY: `kinfo` starts zeroed and every exported field is initialized;
    // the checked x86_64 layout includes the ABI padding and tail exactly.
    unsafe { VmMutPtr::vm_write_unchecked(info, memory, kinfo) }.map_err(|_| AxError::BadAddress)
}

fn setfsid_abi<Id>(
    raw: u32,
    old: Id,
    make: impl FnOnce(u32) -> Option<Id>,
    set: impl FnOnce(Id) -> AxResult<Id>,
    visible: impl Fn(Id) -> u32,
) -> AxResult<isize> {
    let old_visible = visible(old);
    let Some(requested) = make(raw) else {
        return Ok(old_visible as isize);
    };
    Ok(visible(set(requested)?) as isize)
}

/// Decode setgroups' `int gidsetsize` from the x86_64 syscall argument
/// register. Linux truncates to the C `int` before applying its unsigned
/// maximum check, so negative values are rejected by the same comparison.
fn setgroups_size(raw: usize) -> AxResult<usize> {
    let size = raw as u32;
    if size > NGROUPS_MAX {
        return Err(AxError::InvalidInput);
    }
    Ok(size as usize)
}

/// Decode getgroups' `int gidsetsize` from the x86_64 syscall argument
/// register.  The C prototype makes negative values invalid after truncating
/// the register value to its low 32 bits.
fn getgroups_size(raw: usize) -> AxResult<usize> {
    let size = raw as u32 as i32;
    if size < 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(size as usize)
}

/// Decode a UTS name syscall's `int len` from the x86_64 syscall argument
/// register.
/// The syscall ABI supplies register-width arguments, but Linux truncates this
/// parameter to C `int` before checking its range.
fn uts_name_len(raw: usize) -> AxResult<usize> {
    let len = raw as u32 as i32;
    if !(0..=UTS_FIELD_LEN as i32).contains(&len) {
        return Err(AxError::InvalidInput);
    }
    Ok(len as usize)
}

/// Linux's `copy_from_user` result for the UTS name setters is always surfaced
/// as EFAULT, including provider-side failures. A zero-length copy does not
/// inspect the user pointer.
fn copy_uts_name_from_user<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    name: *const u8,
    len: usize,
) -> AxResult<[u8; UTS_FIELD_LEN]> {
    let mut hostname = [0; UTS_FIELD_LEN];
    if len == 0 {
        return Ok(hostname);
    }
    let mut copied = [core::mem::MaybeUninit::<u8>::uninit(); UTS_FIELD_LEN];
    memory
        .read_bytes(name as usize, &mut copied[..len])
        .map_err(|_| AxError::BadAddress)?;
    for (destination, source) in hostname[..len].iter_mut().zip(&copied[..len]) {
        // SAFETY: a successful read_bytes initializes every requested byte.
        *destination = unsafe { source.assume_init() };
    }
    Ok(hostname)
}

/// Copy supplementary GIDs one at a time as Linux's `groups_to_user` does.
/// In particular, do not preflight the destination: a later fault preserves
/// the prefix that has already reached user memory.
fn groups_to_user<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    list: *mut u32,
    groups: &[Kgid],
    mut visible: impl FnMut(Kgid) -> u32,
) -> AxResult<()> {
    let list_address = list as usize;
    for (index, gid) in groups.iter().copied().enumerate() {
        let offset = index
            .checked_mul(size_of::<u32>())
            .ok_or(AxError::BadAddress)?;
        let address = list_address
            .checked_add(offset)
            .ok_or(AxError::BadAddress)?;
        VmMutPtr::vm_write(address as *mut u32, memory, visible(gid))
            .map_err(|_| AxError::BadAddress)?;
    }
    Ok(())
}

fn getgroups_to_user<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    size: usize,
    list: *mut u32,
    groups: &[Kgid],
    visible: impl FnMut(Kgid) -> u32,
) -> AxResult<isize> {
    if size == 0 {
        return Ok(groups.len() as isize);
    }
    if size < groups.len() {
        return Err(AxError::InvalidInput);
    }
    groups_to_user(memory, list, groups, visible)?;
    Ok(groups.len() as isize)
}

/// Copy three visible IDs in the same order as Linux's `getresuid` and
/// `getresgid`.
/// Each destination is faulted only when reached, so an earlier successful
/// write remains visible if a later destination faults.
fn resids_to_user<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ruid: *mut u32,
    euid: *mut u32,
    suid: *mut u32,
    values: [u32; 3],
) -> AxResult<()> {
    for (destination, value) in [(ruid, values[0]), (euid, values[1]), (suid, values[2])] {
        VmMutPtr::vm_write(destination, memory, value).map_err(|_| AxError::BadAddress)?;
    }
    Ok(())
}

/// Copy and translate supplementary GIDs one at a time, matching Linux's
/// `groups_from_user`: the first invalid GID wins over any later user-memory
/// fault.
fn load_setgroups<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    list: *const u32,
    size: usize,
    mut make_kgid: impl FnMut(u32) -> Option<Kgid>,
) -> AxResult<Vec<Kgid>> {
    let mut groups = Vec::new();
    groups
        .try_reserve_exact(size)
        .map_err(|_| AxError::NoMemory)?;

    let list_address = list as usize;
    for index in 0..size {
        let offset = index
            .checked_mul(size_of::<u32>())
            .ok_or(AxError::BadAddress)?;
        let address = list_address
            .checked_add(offset)
            .ok_or(AxError::BadAddress)?;
        let gid = VmPtr::vm_read(address as *const u32, memory).map_err(|_| AxError::BadAddress)?;
        groups.push(make_kgid(gid).ok_or(AxError::InvalidInput)?);
    }
    Ok(groups)
}

pub fn sys_getuid() -> AxResult<isize> {
    let cred = current().as_thread().current_cred();
    Ok(cred.user_ns().from_kuid_munged(cred.ids().ruid) as isize)
}

pub fn sys_geteuid() -> AxResult<isize> {
    let cred = current().as_thread().current_cred();
    Ok(cred.user_ns().from_kuid_munged(cred.ids().euid) as isize)
}

pub fn sys_getresuid<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ruid: *mut u32,
    euid: *mut u32,
    suid: *mut u32,
) -> AxResult<isize> {
    let curr = current();
    let cred = curr.as_thread().current_cred();
    let ids = cred.ids();
    let namespace = cred.user_ns();
    let values = [
        namespace.from_kuid_munged(ids.ruid),
        namespace.from_kuid_munged(ids.euid),
        namespace.from_kuid_munged(ids.suid),
    ];
    resids_to_user(memory, ruid, euid, suid, values)?;
    Ok(0)
}

pub fn sys_getgid() -> AxResult<isize> {
    let cred = current().as_thread().current_cred();
    Ok(cred.user_ns().from_kgid_munged(cred.ids().rgid) as isize)
}

pub fn sys_getegid() -> AxResult<isize> {
    let cred = current().as_thread().current_cred();
    Ok(cred.user_ns().from_kgid_munged(cred.ids().egid) as isize)
}

pub fn sys_getresgid<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    rgid: *mut u32,
    egid: *mut u32,
    sgid: *mut u32,
) -> AxResult<isize> {
    let curr = current();
    let cred = curr.as_thread().current_cred();
    let ids = cred.ids();
    let namespace = cred.user_ns();
    let values = [
        namespace.from_kgid_munged(ids.rgid),
        namespace.from_kgid_munged(ids.egid),
        namespace.from_kgid_munged(ids.sgid),
    ];
    resids_to_user(memory, rgid, egid, sgid, values)?;
    Ok(0)
}

pub fn sys_setuid(uid: u32) -> AxResult<isize> {
    debug!("sys_setuid <= uid: {uid}");
    let curr = current();
    let cred = curr.as_thread().current_cred();
    let uid = cred.user_ns().make_kuid(uid).ok_or(AxError::InvalidInput)?;
    curr.as_thread().setuid(uid)?;
    Ok(0)
}

pub fn sys_setgid(gid: u32) -> AxResult<isize> {
    debug!("sys_setgid <= gid: {gid}");
    let curr = current();
    let cred = curr.as_thread().current_cred();
    let gid = cred.user_ns().make_kgid(gid).ok_or(AxError::InvalidInput)?;
    curr.as_thread().setgid(gid)?;
    Ok(0)
}

pub fn sys_setfsuid(fsuid: u32) -> AxResult<isize> {
    let curr = current();
    let thread = curr.as_thread();
    let cred = thread.current_cred();
    let namespace = cred.user_ns().clone();
    setfsid_abi(
        fsuid,
        cred.ids().fsuid,
        |raw| namespace.make_kuid(raw),
        |fsuid| thread.setfsuid(fsuid),
        |old| namespace.from_kuid_munged(old),
    )
}

pub fn sys_setfsgid(fsgid: u32) -> AxResult<isize> {
    let curr = current();
    let thread = curr.as_thread();
    let cred = thread.current_cred();
    let namespace = cred.user_ns().clone();
    setfsid_abi(
        fsgid,
        cred.ids().fsgid,
        |raw| namespace.make_kgid(raw),
        |fsgid| thread.setfsgid(fsgid),
        |old| namespace.from_kgid_munged(old),
    )
}

pub fn sys_getgroups<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    size: usize,
    list: *mut u32,
) -> AxResult<isize> {
    let size = getgroups_size(size)?;
    debug!("sys_getgroups <= size: {size}");
    let cred = current().as_thread().current_cred();
    let groups = cred.groups().as_slice();
    getgroups_to_user(memory, size, list, groups, |gid| {
        cred.user_ns().from_kgid_munged(gid)
    })
}

pub fn sys_setgroups<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    size: usize,
    list: *const u32,
) -> AxResult<isize> {
    let curr = current();
    let thread = curr.as_thread();
    // Linux rejects missing CAP_SETGID before validating or reading the user
    // array. The admission pins that one typed decision to this exact slot and
    // credential so publication can revalidate without auditing twice.
    let admission = thread.admit_setgroups()?;
    let size = setgroups_size(size)?;
    let groups = load_setgroups(memory, list, size, |gid| {
        admission.credential().user_ns().make_kgid(gid)
    })?;
    thread.set_supplementary_groups(admission, groups)?;
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

const UNAME26: u32 = 0x0002_0000;
pub(crate) const ADDR_NO_RANDOMIZE: u32 = 0x0004_0000;
const MMAP_PAGE_ZERO: u32 = 0x0010_0000;
const ADDR_COMPAT_LAYOUT: u32 = 0x0020_0000;
const READ_IMPLIES_EXEC: u32 = 0x0040_0000;
pub(crate) const PER_CLEAR_ON_SETID: u32 =
    READ_IMPLIES_EXEC | ADDR_NO_RANDOMIZE | ADDR_COMPAT_LAYOUT | MMAP_PAGE_ZERO;
const UTS_RELEASE: &str = "6.12.103";
const UTS_VERSION: &str = "#1 SMP PREEMPT_DYNAMIC 2026-08-10T00:00:00Z";
const UNAME26_RELEASE_PREFIX: &[u8] = b"2.6.72";

fn fill_uts_field(dst: &mut [c_char; 65], src: &[u8]) {
    for (dst, byte) in dst.iter_mut().zip(src.iter()) {
        *dst = *byte as c_char;
    }
}

/// Match Linux `override_release()`: retain the first non-version suffix
/// after translating every 4.x-and-later release to the UNAME26 2.6 series.
fn uname26_release(release: &[c_char; 65]) -> [u8; 65] {
    let mut result = [0u8; 65];
    result[..UNAME26_RELEASE_PREFIX.len()].copy_from_slice(UNAME26_RELEASE_PREFIX);

    let mut rest = 0;
    let mut dots = 0;
    while rest < release.len() && release[rest] != 0 {
        let byte = release[rest] as u8;
        if byte == b'.' {
            dots += 1;
            if dots >= 3 {
                break;
            }
        }
        if !byte.is_ascii_digit() && byte != b'.' {
            break;
        }
        rest += 1;
    }
    let mut output = UNAME26_RELEASE_PREFIX.len();
    while output < result.len() - 1 && rest < release.len() && release[rest] != 0 {
        result[output] = release[rest] as u8;
        output += 1;
        rest += 1;
    }
    result
}

pub(crate) fn current_utsname() -> AxResult<new_utsname> {
    let mut utsname = new_utsname {
        sysname: pad_str("Linux"),
        nodename: [0; 65],
        release: pad_str(UTS_RELEASE),
        version: pad_str(UTS_VERSION),
        machine: pad_str("x86_64"),
        domainname: [0; 65],
    };
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let uts_ns = proc_data.uts_ns();
    let (nodename, domainname) = uts_ns.names_snapshot();
    fill_uts_field(&mut utsname.nodename, &nodename);
    fill_uts_field(&mut utsname.domainname, &domainname);
    Ok(utsname)
}

pub(crate) fn proc_version_string() -> AxResult<String> {
    let utsname = current_utsname()?;
    let sysname = cstr_field_to_string(&utsname.sysname);
    let release = cstr_field_to_string(&utsname.release);
    let version = cstr_field_to_string(&utsname.version);
    let machine = cstr_field_to_string(&utsname.machine);
    Ok(format!(
        "{sysname} version {release} ({machine}) {version}\n"
    ))
}

pub(crate) fn current_sysname_string() -> AxResult<String> {
    let utsname = current_utsname()?;
    Ok(cstr_field_to_string(&utsname.sysname))
}

pub(crate) fn current_release_string() -> AxResult<String> {
    let utsname = current_utsname()?;
    Ok(cstr_field_to_string(&utsname.release))
}

pub(crate) fn current_version_string() -> AxResult<String> {
    let utsname = current_utsname()?;
    Ok(cstr_field_to_string(&utsname.version))
}

pub(crate) fn current_machine_string() -> AxResult<String> {
    let utsname = current_utsname()?;
    Ok(cstr_field_to_string(&utsname.machine))
}

/// Return the bytes visible through `/proc/sys/kernel/hostname`.
///
/// UTS names are byte arrays rather than UTF-8 strings.  The proc projection
/// ends at the first NUL but otherwise preserves every byte.
pub(crate) fn current_hostname_bytes() -> AxResult<Vec<u8>> {
    current()
        .as_thread()
        .proc_data
        .uts_ns()
        .nodename()
        .map(|value| uts_bytes_before_nul(&value).to_vec())
}

/// Return the bytes visible through `/proc/sys/kernel/domainname`.
pub(crate) fn current_domainname_bytes() -> AxResult<Vec<u8>> {
    current()
        .as_thread()
        .proc_data
        .uts_ns()
        .domainname()
        .map(|value| uts_bytes_before_nul(&value).to_vec())
}

pub(crate) fn set_hostname_bytes(hostname: &[u8]) -> AxResult<()> {
    current()
        .as_thread()
        .proc_data
        .uts_ns()
        .set_nodename(hostname)
}

pub(crate) fn set_domainname_bytes(domainname: &[u8]) -> AxResult<()> {
    current()
        .as_thread()
        .proc_data
        .uts_ns()
        .set_domainname(domainname)
}

pub(crate) fn current_can_administer_uts() -> bool {
    let current = current();
    let thread = current.as_thread();
    let cred = thread.current_cred();
    let uts_ns = thread.proc_data.uts_ns();
    ns_capable(
        &cred,
        uts_ns.owner_user_ns(),
        linux_raw_sys::general::CAP_SYS_ADMIN,
    )
}

fn cstr_field_to_string(field: &[c_char; 65]) -> String {
    let len = field.iter().position(|&ch| ch == 0).unwrap_or(field.len());
    // `c_char` is signed on the host test target and unsigned on the kernel
    // targets, so normalize before widening to `char`.
    #[allow(clippy::unnecessary_cast)]
    return field[..len]
        .iter()
        .map(|&ch| char::from(ch as u8))
        .collect();
}

fn uts_bytes_before_nul(value: &[u8]) -> &[u8] {
    &value[..value
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(value.len())]
}

pub fn sys_uname<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    name: *mut new_utsname,
) -> AxResult<isize> {
    let uts = current_utsname()?;
    let uname26 = current().as_thread().personality() & UNAME26 != 0;
    write_utsname(memory, name, uts, uname26)?;
    Ok(0)
}

/// Linux first copies the native `new_utsname`, then overwrites only the
/// release field for UNAME26. Keep this as two user copies: a fault in the
/// latter exposes the already copied native result.
fn write_utsname<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    name: *mut new_utsname,
    uts: new_utsname,
    uname26: bool,
) -> AxResult<()> {
    // SAFETY: all fields in `uts` are initialized, including the zero-filled
    // tail bytes, and the checked x86_64 layout has no padding.
    unsafe { VmMutPtr::vm_write_unchecked(name, memory, uts) }.map_err(|_| AxError::BadAddress)?;
    if uname26 {
        let release = uname26_release(&uts.release);
        // `release` begins 130 bytes into the packed native x86_64 layout.
        let release_ptr = (name as *mut u8).wrapping_add(offset_of!(new_utsname, release));
        vm_write_slice(memory, release_ptr, &release).map_err(|_| AxError::BadAddress)?;
    }
    Ok(())
}

pub fn sys_sethostname<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    name: *const u8,
    len: usize,
) -> AxResult<isize> {
    if !current_can_administer_uts() {
        return Err(AxError::OperationNotPermitted);
    }
    let len = uts_name_len(len)?;
    let hostname = copy_uts_name_from_user(memory, name, len)?;
    set_hostname_bytes(&hostname[..len])?;
    Ok(0)
}

pub fn sys_setdomainname<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    name: *const u8,
    len: usize,
) -> AxResult<isize> {
    if !current_can_administer_uts() {
        return Err(AxError::OperationNotPermitted);
    }
    let len = uts_name_len(len)?;
    let domainname = copy_uts_name_from_user(memory, name, len)?;
    set_domainname_bytes(&domainname[..len])?;
    Ok(0)
}

pub fn sys_sysinfo<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    info: *mut sysinfo,
) -> AxResult<isize> {
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
    load_average_sample_now();
    kinfo.loads = load_average_sysinfo().map(|load| load as _);
    // Linux assigns `nr_threads` directly to the u16 ABI member.
    kinfo.procs = live_thread_count() as _;
    set_sysinfo_memory_fields(
        &mut kinfo,
        stats.total_bytes,
        stats.free_bytes,
        shmem_resident_pages(),
    );
    write_sysinfo(memory, info, kinfo)?;
    Ok(0)
}

pub fn sys_personality(persona: u32) -> AxResult<isize> {
    let curr = current();
    let thread = curr.as_thread();
    let old = thread.personality();

    if persona == u32::MAX {
        return Ok(old as isize);
    }

    thread.set_personality(persona);
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

pub fn sys_getrandom<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    buf: *mut u8,
    len: usize,
    flags: u32,
) -> AxResult<isize> {
    const MAX_RW_COUNT: usize = 0x7fff_f000;
    const CHACHA_BLOCK: usize = 64;
    const PAGE_SIZE: usize = 4096;

    let flags = GetRandomFlags::from_bits(flags).ok_or(AxError::InvalidInput)?;
    if flags.contains(GetRandomFlags::RANDOM) && flags.contains(GetRandomFlags::INSECURE) {
        return Err(AxError::InvalidInput);
    }
    // Linux waits for CRNG readiness even for a zero-length secure request.
    if !flags.contains(GetRandomFlags::INSECURE) {
        loop {
            match crate::random::ensure_ready() {
                Ok(()) => break,
                Err(AxError::WouldBlock) if flags.contains(GetRandomFlags::NONBLOCK) => {
                    return Err(AxError::WouldBlock);
                }
                Err(AxError::WouldBlock) => {
                    if has_pending_syscall_signal(current().as_thread()) {
                        return Err(AxError::Interrupted);
                    }
                    axtask::yield_now();
                }
                Err(error) => return Err(error),
            }
        }
    }
    let len = len.min(MAX_RW_COUNT);
    if len != 0 {
        (buf as usize).checked_add(len).ok_or(AxError::BadAddress)?;
        memory
            .validate_write_range(buf as usize, len)
            .map_err(map_usercopy_error)?;
    }
    if len == 0 {
        return Ok(0);
    }

    debug!("sys_getrandom <= buf: {buf:p}, len: {len}, flags: {flags:?}");

    let mut total = 0;
    let mut kbuf = [0u8; CHACHA_BLOCK];
    while total < len {
        let address = (buf as usize)
            .checked_add(total)
            .ok_or(AxError::BadAddress)?;
        let page_left = PAGE_SIZE - (address & (PAGE_SIZE - 1));
        let chunk = (len - total).min(kbuf.len()).min(page_left);
        let fill_result = if flags.contains(GetRandomFlags::INSECURE) {
            crate::random::fill_insecure(&mut kbuf[..chunk])
        } else {
            crate::random::fill_secure(&mut kbuf[..chunk])
        };
        if let Err(error) = fill_result {
            return if total == 0 {
                Err(error)
            } else {
                Ok(total as isize)
            };
        }
        if let Err(error) =
            vm_write_slice(memory, address as *mut u8, &kbuf[..chunk]).map_err(map_usercopy_error)
        {
            return if total == 0 {
                Err(error)
            } else {
                Ok(total as isize)
            };
        }
        total += chunk;
        if total % PAGE_SIZE == 0 {
            if has_pending_syscall_signal(current().as_thread()) {
                return Ok(total as isize);
            }
            axtask::yield_now();
        }
    }

    Ok(total as isize)
}

pub fn sys_restart_syscall(uctx: &UserContext) -> AxResult<isize> {
    let curr = current();
    let thr = curr.as_thread();
    let Some(block) = thr.begin_restart_syscall(uctx) else {
        return Err(AxError::InvalidInput);
    };
    match block {
        RestartBlock::FutexWait(block) => restart_futex_wait(thr.proc_data.aspace(), block),
    }
}

#[cfg(test)]
mod tests {
    use core::{cell::Cell, mem::MaybeUninit};

    use thekernel_linux_usercopy::{UserCopyError, VmResult};

    use super::*;
    use crate::task::{IdMapInputExtent, Kuid, UserNamespace};

    struct GroupMemory {
        bytes: Vec<u8>,
        reads: Vec<usize>,
        writes: Vec<usize>,
        read_error: Option<UserCopyError>,
        fail_write_at: Option<usize>,
        write_error: Option<UserCopyError>,
    }

    // SAFETY: GroupMemory treats user pointers as checked byte offsets and
    // initializes every destination byte on a successful read.
    unsafe impl UserMemory for GroupMemory {
        fn read(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult {
            self.reads.push(start);
            if let Some(error) = self.read_error {
                return Err(error);
            }
            let end = start
                .checked_add(dst.len())
                .ok_or(UserCopyError::BadAddress)?;
            let source = self
                .bytes
                .get(start..end)
                .ok_or(UserCopyError::BadAddress)?;
            for (output, input) in dst.iter_mut().zip(source) {
                output.write(*input);
            }
            Ok(())
        }

        fn write(&mut self, start: usize, src: &[u8]) -> VmResult {
            self.writes.push(start);
            if self.fail_write_at == Some(start) {
                return Err(UserCopyError::BadAddress);
            }
            if let Some(error) = self.write_error {
                return Err(error);
            }
            let end = start
                .checked_add(src.len())
                .ok_or(UserCopyError::BadAddress)?;
            let destination = self
                .bytes
                .get_mut(start..end)
                .ok_or(UserCopyError::BadAddress)?;
            destination.copy_from_slice(src);
            Ok(())
        }
    }

    #[test]
    fn sysinfo_memory_fields_use_bytes_and_only_shmem_pages() {
        assert_eq!(core::mem::size_of::<sysinfo>(), 112);
        assert_eq!(core::mem::align_of::<sysinfo>(), 8);
        assert_eq!(core::mem::offset_of!(sysinfo, sharedram), 48);
        assert_eq!(core::mem::offset_of!(sysinfo, mem_unit), 104);

        let mut info: sysinfo = unsafe { core::mem::zeroed() };
        set_sysinfo_memory_fields(
            &mut info,
            9 * memory_addr::PAGE_SIZE_4K,
            3 * memory_addr::PAGE_SIZE_4K,
            2,
        );

        assert_eq!(info.totalram as usize, 9 * memory_addr::PAGE_SIZE_4K);
        assert_eq!(info.freeram as usize, 3 * memory_addr::PAGE_SIZE_4K);
        assert_eq!(info.sharedram as usize, 2 * memory_addr::PAGE_SIZE_4K);
        assert_eq!(info.bufferram, 0);
        assert_eq!(info.mem_unit, SYSINFO_MEM_UNIT);
    }

    #[test]
    fn sysinfo_copyout_errors_are_efault() {
        for error in [
            UserCopyError::BadAddress,
            UserCopyError::AccessDenied,
            UserCopyError::NoMemory,
        ] {
            let mut provider = GroupMemory {
                bytes: vec![0; core::mem::size_of::<sysinfo>()],
                reads: Vec::new(),
                writes: Vec::new(),
                read_error: None,
                fail_write_at: None,
                write_error: Some(error),
            };
            let mut memory = UserMemoryContext::new(&mut provider);
            let info: sysinfo = unsafe { core::mem::zeroed() };

            assert_eq!(
                write_sysinfo(&mut memory, core::ptr::null_mut(), info),
                Err(AxError::BadAddress)
            );
            assert_eq!(memory.memory_mut().writes, &[0]);
        }
    }

    fn mapped_child_namespace() -> alloc::sync::Arc<UserNamespace> {
        let initial = UserNamespace::try_new_root().unwrap();
        let child = initial
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        child
            .publish_uid_map(
                child
                    .try_build_uid_map(vec![IdMapInputExtent::new(0, 1000, 2)])
                    .unwrap(),
            )
            .unwrap();
        child
            .publish_gid_map(
                child
                    .try_build_gid_map(vec![IdMapInputExtent::new(0, 100, 2)])
                    .unwrap(),
                false,
            )
            .unwrap();
        child
    }

    #[test]
    fn uts_name_len_matches_linux_int_abi() {
        assert_eq!(uts_name_len(0), Ok(0));
        assert_eq!(uts_name_len(UTS_FIELD_LEN), Ok(UTS_FIELD_LEN));
        assert_eq!(uts_name_len(1_usize << 32), Ok(0));
        assert_eq!(uts_name_len((1_usize << 32) | 12), Ok(12));
        assert_eq!(
            uts_name_len((-1_isize) as usize),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            uts_name_len((UTS_FIELD_LEN + 1) as usize),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn uts_name_usercopy_allows_mapped_zero_and_skips_zero_length() {
        let mut mapped_provider = GroupMemory {
            bytes: b"node".to_vec(),
            reads: Vec::new(),
            writes: Vec::new(),
            read_error: None,
            fail_write_at: None,
            write_error: None,
        };
        let mut mapped_memory = UserMemoryContext::new(&mut mapped_provider);
        assert_eq!(
            &copy_uts_name_from_user(&mut mapped_memory, core::ptr::null(), 4).unwrap()[..4],
            b"node"
        );
        assert_eq!(mapped_memory.memory_mut().reads, &[0]);

        let mut zero_provider = GroupMemory {
            bytes: Vec::new(),
            reads: Vec::new(),
            writes: Vec::new(),
            read_error: Some(UserCopyError::NoMemory),
            fail_write_at: None,
            write_error: None,
        };
        let mut zero_memory = UserMemoryContext::new(&mut zero_provider);
        assert_eq!(
            copy_uts_name_from_user(&mut zero_memory, core::ptr::null(), 0),
            Ok([0; UTS_FIELD_LEN])
        );
        assert!(zero_memory.memory_mut().reads.is_empty());

        let mut failing_provider = GroupMemory {
            bytes: vec![0; 4],
            reads: Vec::new(),
            writes: Vec::new(),
            read_error: Some(UserCopyError::NoMemory),
            fail_write_at: None,
            write_error: None,
        };
        let mut failing_memory = UserMemoryContext::new(&mut failing_provider);
        assert_eq!(
            copy_uts_name_from_user(&mut failing_memory, core::ptr::null(), 4),
            Err(AxError::BadAddress)
        );
    }

    #[test]
    fn uts_bytes_before_nul_preserves_non_utf8_bytes() {
        assert_eq!(uts_bytes_before_nul(b"node\0suffix"), b"node");
        assert_eq!(
            uts_bytes_before_nul(&[b'n', 0xff, b'e']),
            &[b'n', 0xff, b'e']
        );
    }

    #[test]
    fn personality_namespace_preserves_arbitrary_native_u32_bits() {
        assert_eq!(PER_CLEAR_ON_SETID, 0x0074_0000);
        assert_eq!(UNAME26_RELEASE_PREFIX, b"2.6.72");
        assert_eq!(u32::MAX & !PER_CLEAR_ON_SETID, 0xff8b_ffff);
    }

    fn test_utsname() -> new_utsname {
        new_utsname {
            sysname: [b'S' as c_char; 65],
            nodename: [b'N' as c_char; 65],
            release: [b'R' as c_char; 65],
            version: [b'V' as c_char; 65],
            machine: [b'M' as c_char; 65],
            domainname: [b'D' as c_char; 65],
        }
    }

    #[test]
    fn uname26_release_matches_linux_override_release() {
        let mut release = [0; 65];
        fill_uts_field(&mut release, b"6.12.103-custom");
        assert_eq!(&uname26_release(&release)[..14], b"2.6.72-custom\0");
    }

    #[test]
    fn uname_native_copy_is_one_complete_390_byte_write() {
        let mut provider = GroupMemory {
            bytes: vec![0; size_of::<new_utsname>()],
            reads: Vec::new(),
            writes: Vec::new(),
            read_error: None,
            fail_write_at: None,
            write_error: None,
        };
        let mut memory = UserMemoryContext::new(&mut provider);
        write_utsname(&mut memory, core::ptr::null_mut(), test_utsname(), false).unwrap();
        assert_eq!(memory.memory_mut().writes, &[0]);
        assert_eq!(&memory.memory_mut().bytes[130..195], &[b'R'; 65]);
    }

    #[test]
    fn uname26_overwrites_release_in_a_second_copy_after_native_result() {
        let mut provider = GroupMemory {
            bytes: vec![0; size_of::<new_utsname>()],
            reads: Vec::new(),
            writes: Vec::new(),
            read_error: None,
            fail_write_at: None,
            write_error: None,
        };
        let mut memory = UserMemoryContext::new(&mut provider);
        write_utsname(&mut memory, core::ptr::null_mut(), test_utsname(), true).unwrap();
        assert_eq!(memory.memory_mut().writes, &[0, 130]);
        assert_eq!(&memory.memory_mut().bytes[130..137], b"2.6.72\0");
        assert_eq!(&memory.memory_mut().bytes[137..195], &[0; 58]);
    }

    #[test]
    fn uname26_second_copy_fault_keeps_native_prefix_visible_and_returns_efault() {
        let mut provider = GroupMemory {
            bytes: vec![0; size_of::<new_utsname>()],
            reads: Vec::new(),
            writes: Vec::new(),
            read_error: None,
            fail_write_at: Some(130),
            write_error: None,
        };
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            write_utsname(&mut memory, core::ptr::null_mut(), test_utsname(), true),
            Err(AxError::BadAddress)
        );
        assert_eq!(memory.memory_mut().writes, &[0, 130]);
        assert_eq!(&memory.memory_mut().bytes[130..195], &[b'R'; 65]);
    }

    #[test]
    fn invalid_or_unmapped_setfsid_returns_old_without_calling_writer() {
        let namespace = mapped_child_namespace();
        let old_uid = Kuid::from_raw(1000).unwrap();
        let old_gid = Kgid::from_raw(100).unwrap();

        for raw in [u32::MAX, 2] {
            let uid_writes = Cell::new(0);
            let result = setfsid_abi(
                raw,
                old_uid,
                |raw| namespace.make_kuid(raw),
                |requested| {
                    uid_writes.set(uid_writes.get() + 1);
                    Ok(requested)
                },
                |old| namespace.from_kuid_munged(old),
            )
            .unwrap();
            assert_eq!(result, 0);
            assert_eq!(uid_writes.get(), 0);

            let gid_writes = Cell::new(0);
            let result = setfsid_abi(
                raw,
                old_gid,
                |raw| namespace.make_kgid(raw),
                |requested| {
                    gid_writes.set(gid_writes.get() + 1);
                    Ok(requested)
                },
                |old| namespace.from_kgid_munged(old),
            )
            .unwrap();
            assert_eq!(result, 0);
            assert_eq!(gid_writes.get(), 0);
        }

        let initial = UserNamespace::try_new_root().unwrap();
        let empty = initial
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let writes = Cell::new(0);
        let result = setfsid_abi(
            0,
            old_uid,
            |raw| empty.make_kuid(raw),
            |requested| {
                writes.set(writes.get() + 1);
                Ok(requested)
            },
            |old| empty.from_kuid_munged(old),
        )
        .unwrap();
        assert_eq!(result, 65_534);
        assert_eq!(writes.get(), 0);
    }

    #[test]
    fn setgroups_size_matches_linux_int_then_unsigned_max_check() {
        assert_eq!(setgroups_size(0), Ok(0));
        assert_eq!(
            setgroups_size(NGROUPS_MAX as usize),
            Ok(NGROUPS_MAX as usize)
        );
        assert_eq!(
            setgroups_size((NGROUPS_MAX + 1) as usize),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            setgroups_size((-1_isize) as usize),
            Err(AxError::InvalidInput)
        );
        // Syscall arguments are register-width, while gidsetsize is an int.
        assert_eq!(setgroups_size(1_usize << 32), Ok(0));
    }

    #[test]
    fn setgroups_stops_usercopy_at_first_unmapped_gid() {
        let mut provider = GroupMemory {
            bytes: vec![7, 0, 0, 0, 99, 0, 0, 0],
            reads: Vec::new(),
            writes: Vec::new(),
            read_error: None,
            fail_write_at: None,
            write_error: None,
        };
        let mut memory = UserMemoryContext::new(&mut provider);

        assert_eq!(
            load_setgroups(&mut memory, core::ptr::null(), 2, |raw| {
                (raw != 7).then(|| Kgid::from_raw(raw).unwrap())
            }),
            Err(AxError::InvalidInput)
        );
        assert_eq!(memory.memory_mut().reads, &[0]);
    }

    #[test]
    fn getgroups_size_matches_linux_low_32_bit_int() {
        assert_eq!(getgroups_size(0), Ok(0));
        assert_eq!(getgroups_size(1_usize << 32), Ok(0));
        assert_eq!(getgroups_size((1_usize << 32) | 3), Ok(3));
        assert_eq!(
            getgroups_size((-1_isize) as usize),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            getgroups_size((1_usize << 32) | (i32::MIN as u32 as usize)),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn getgroups_zero_size_and_empty_groups_do_not_touch_pointer() {
        let groups = [Kgid::from_raw(7).unwrap()];
        let mut provider = GroupMemory {
            bytes: Vec::new(),
            reads: Vec::new(),
            writes: Vec::new(),
            read_error: None,
            fail_write_at: None,
            write_error: None,
        };
        let mut memory = UserMemoryContext::new(&mut provider);

        assert_eq!(
            getgroups_to_user(
                &mut memory,
                0,
                core::ptr::null_mut(),
                &groups,
                Kgid::into_raw
            ),
            Ok(1)
        );
        assert!(memory.memory_mut().writes.is_empty());
        assert_eq!(
            getgroups_to_user(&mut memory, 1, core::ptr::null_mut(), &[], Kgid::into_raw),
            Ok(0)
        );
        assert!(memory.memory_mut().writes.is_empty());
    }

    #[test]
    fn groups_to_user_preserves_duplicate_order() {
        let groups = [
            Kgid::from_raw(42).unwrap(),
            Kgid::from_raw(7).unwrap(),
            Kgid::from_raw(42).unwrap(),
        ];
        let mut provider = GroupMemory {
            bytes: vec![0; groups.len() * size_of::<u32>()],
            reads: Vec::new(),
            writes: Vec::new(),
            read_error: None,
            fail_write_at: None,
            write_error: None,
        };
        let mut memory = UserMemoryContext::new(&mut provider);

        assert_eq!(
            groups_to_user(&mut memory, core::ptr::null_mut(), &groups, Kgid::into_raw),
            Ok(())
        );
        assert_eq!(memory.memory_mut().writes, &[0, 4, 8]);
        assert_eq!(
            memory.memory_mut().bytes,
            [42, 0, 0, 0, 7, 0, 0, 0, 42, 0, 0, 0]
        );
    }

    #[test]
    fn groups_to_user_keeps_prefix_on_later_fault() {
        let groups = [Kgid::from_raw(1).unwrap(), Kgid::from_raw(2).unwrap()];
        let mut provider = GroupMemory {
            bytes: vec![0; groups.len() * size_of::<u32>()],
            reads: Vec::new(),
            writes: Vec::new(),
            read_error: None,
            fail_write_at: Some(size_of::<u32>()),
            write_error: None,
        };
        let mut memory = UserMemoryContext::new(&mut provider);

        assert_eq!(
            groups_to_user(&mut memory, core::ptr::null_mut(), &groups, Kgid::into_raw),
            Err(AxError::BadAddress)
        );
        assert_eq!(memory.memory_mut().writes, &[0, 4]);
        assert_eq!(memory.memory_mut().bytes, [1, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn getresids_address_zero_faults_without_later_writes() {
        let mut provider = GroupMemory {
            bytes: vec![0; 16],
            reads: Vec::new(),
            writes: Vec::new(),
            read_error: None,
            fail_write_at: Some(0),
            write_error: None,
        };
        let mut memory = UserMemoryContext::new(&mut provider);

        assert_eq!(
            resids_to_user(
                &mut memory,
                core::ptr::null_mut(),
                4 as *mut u32,
                8 as *mut u32,
                [11, 22, 33]
            ),
            Err(AxError::BadAddress)
        );
        assert_eq!(memory.memory_mut().writes, &[0]);
    }

    #[test]
    fn getresids_keeps_first_id_prefix_on_second_fault() {
        let mut provider = GroupMemory {
            bytes: vec![0; 16],
            reads: Vec::new(),
            writes: Vec::new(),
            read_error: None,
            fail_write_at: Some(8),
            write_error: None,
        };
        let mut memory = UserMemoryContext::new(&mut provider);

        assert_eq!(
            resids_to_user(
                &mut memory,
                4 as *mut u32,
                8 as *mut u32,
                12 as *mut u32,
                [11, 22, 33]
            ),
            Err(AxError::BadAddress)
        );
        assert_eq!(memory.memory_mut().writes, &[4, 8]);
        assert_eq!(&memory.memory_mut().bytes[4..8], &11_u32.to_ne_bytes());
        assert_eq!(&memory.memory_mut().bytes[8..], &[0; 8]);
    }

    #[test]
    fn getresids_maps_nomemory_and_writes_values_in_order() {
        let mut failing_provider = GroupMemory {
            bytes: vec![0; 16],
            reads: Vec::new(),
            writes: Vec::new(),
            read_error: None,
            fail_write_at: None,
            write_error: Some(UserCopyError::NoMemory),
        };
        let mut failing_memory = UserMemoryContext::new(&mut failing_provider);
        assert_eq!(
            resids_to_user(
                &mut failing_memory,
                4 as *mut u32,
                8 as *mut u32,
                12 as *mut u32,
                [11, 22, 33]
            ),
            Err(AxError::BadAddress)
        );
        assert_eq!(failing_memory.memory_mut().writes, &[4]);

        let mut provider = GroupMemory {
            bytes: vec![0; 16],
            reads: Vec::new(),
            writes: Vec::new(),
            read_error: None,
            fail_write_at: None,
            write_error: None,
        };
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            resids_to_user(
                &mut memory,
                4 as *mut u32,
                8 as *mut u32,
                12 as *mut u32,
                [11, 22, 33]
            ),
            Ok(())
        );
        assert_eq!(memory.memory_mut().writes, &[4, 8, 12]);
        assert_eq!(
            &memory.memory_mut().bytes[4..16],
            &[11, 0, 0, 0, 22, 0, 0, 0, 33, 0, 0, 0]
        );
    }

    #[test]
    fn group_helpers_map_all_usercopy_errors_to_efault() {
        let mut read_provider = GroupMemory {
            bytes: vec![1, 0, 0, 0],
            reads: Vec::new(),
            writes: Vec::new(),
            read_error: Some(UserCopyError::NoMemory),
            fail_write_at: None,
            write_error: None,
        };
        let mut read_memory = UserMemoryContext::new(&mut read_provider);
        assert_eq!(
            load_setgroups(
                &mut read_memory,
                core::ptr::null(),
                1,
                |raw| Kgid::from_raw(raw)
            ),
            Err(AxError::BadAddress)
        );

        let groups = [Kgid::from_raw(1).unwrap()];
        let mut write_provider = GroupMemory {
            bytes: vec![0; size_of::<u32>()],
            reads: Vec::new(),
            writes: Vec::new(),
            read_error: None,
            fail_write_at: None,
            write_error: Some(UserCopyError::NoMemory),
        };
        let mut write_memory = UserMemoryContext::new(&mut write_provider);
        assert_eq!(
            groups_to_user(
                &mut write_memory,
                core::ptr::null_mut(),
                &groups,
                Kgid::into_raw
            ),
            Err(AxError::BadAddress)
        );
    }
}
