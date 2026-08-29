use alloc::{format, string::String, vec, vec::Vec};
use core::{
    ffi::c_char,
    mem::{align_of, offset_of, size_of},
};

use axconfig::ARCH;
use axerrno::{AxError, AxResult, LinuxError};
use axhal::{time::monotonic_time, uspace::UserContext};
use axtask::current;
use linux_raw_sys::{
    general::{GRND_INSECURE, GRND_NONBLOCK, GRND_RANDOM, NGROUPS_MAX},
    system::{new_utsname, sysinfo},
};
use thekernel_linux_usercopy::{
    UserMemory, UserMemoryContext, VmMutPtr, VmPtr, vm_load, vm_write_slice,
};

use super::sync::restart_futex_wait;
use crate::{
    mm::{map_usercopy_error, system_memory_stats},
    task::{AsThread, Kgid, RestartBlock, UTS_FIELD_LEN, ns_capable, try_processes},
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

const PER_MASK: u32 = 0xff;
const UNAME26: u32 = 0x0002_0000;
const SUPPORTED_PERSONALITY: u32 = UNAME26;
const UNAME26_RELEASE: &[u8] = b"2.6.60";

fn fill_uts_field(dst: &mut [c_char; 65], src: &[u8]) {
    for (dst, byte) in dst.iter_mut().zip(src.iter()) {
        *dst = *byte as c_char;
    }
}

pub(crate) fn current_utsname() -> AxResult<new_utsname> {
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
    fill_uts_field(&mut utsname.nodename, &uts_ns.nodename()?);
    fill_uts_field(&mut utsname.domainname, &uts_ns.domainname()?);
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

pub(crate) fn current_hostname_string() -> AxResult<String> {
    current()
        .as_thread()
        .proc_data
        .uts_ns()
        .nodename()
        .map(|value| value.into_iter().map(char::from).collect())
}

pub(crate) fn current_domainname_string() -> AxResult<String> {
    current()
        .as_thread()
        .proc_data
        .uts_ns()
        .domainname()
        .map(|value| value.into_iter().map(char::from).collect())
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

pub fn sys_uname<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    name: *mut new_utsname,
) -> AxResult<isize> {
    let mut uts = current_utsname()?;
    if current().as_thread().proc_data.personality() & UNAME26 != 0 {
        uts.release = [0; 65];
        fill_uts_field(&mut uts.release, UNAME26_RELEASE);
    }
    // SAFETY: all fields in `uts` are initialized, including the zero-filled
    // tail bytes, and the checked x86_64 layout has no padding.
    unsafe { VmMutPtr::vm_write_unchecked(name, memory, uts) }.map_err(map_usercopy_error)?;
    Ok(0)
}

pub fn sys_sethostname<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    name: *const u8,
    len: usize,
) -> AxResult<isize> {
    if !current_can_administer_uts() {
        return Err(AxError::OperationNotPermitted);
    }
    if len > UTS_FIELD_LEN {
        return Err(AxError::InvalidInput);
    }
    if name.is_null() {
        return Err(AxError::BadAddress);
    }
    let hostname = vm_load(memory, name, len).map_err(map_usercopy_error)?;
    set_hostname_bytes(&hostname)?;
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
    if len > UTS_FIELD_LEN {
        return Err(AxError::InvalidInput);
    }
    if name.is_null() {
        return Err(AxError::BadAddress);
    }
    let domainname = vm_load(memory, name, len).map_err(map_usercopy_error)?;
    set_domainname_bytes(&domainname)?;
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
    kinfo.procs = try_processes()?.len() as _;
    kinfo.totalram = stats.total_bytes as _;
    kinfo.freeram = stats.free_bytes as _;
    // axfs uses a page cache, not Linux's separate block-buffer cache.
    kinfo.bufferram = 0;
    kinfo.mem_unit = 1;
    // SAFETY: `kinfo` starts zeroed and every exported field is initialized;
    // the checked x86_64 layout includes the ABI padding and tail exactly.
    unsafe { VmMutPtr::vm_write_unchecked(info, memory, kinfo) }.map_err(map_usercopy_error)?;
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

pub fn sys_getrandom<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    buf: *mut u8,
    len: usize,
    flags: u32,
) -> AxResult<isize> {
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
                Err(error)
            } else {
                Ok(total as isize)
            };
        }
        if let Err(error) = vm_write_slice(memory, buf.wrapping_add(total), &kbuf[..chunk])
            .map_err(map_usercopy_error)
        {
            return if total == 0 {
                Err(error)
            } else {
                Ok(total as isize)
            };
        }
        total += chunk;
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
