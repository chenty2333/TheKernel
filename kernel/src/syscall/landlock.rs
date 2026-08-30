//! Landlock and LSM userspace ABI control plane.
//!
//! Ruleset descriptors deliberately retain the `Location` selected by the
//! caller.  A pathname is not a security object: keeping the VFS object avoids
//! retargeting a rule when a name is renamed or reused.

use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::{mem::{MaybeUninit, size_of}, task::Context};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use bytemuck::{Pod, Zeroable};
use linux_raw_sys::general::CAP_SYS_ADMIN;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmMutPtr, VmPtr};

use crate::{
    file::{FileHandle, FileLike, Kstat, anon_inode_stat, get_typed_file, inotify::location_for_fd},
    task::{
        AsThread,
        security::{
            LANDLOCK_ACCESS_FS_EXECUTE, LANDLOCK_ACCESS_FS_IOCTL_DEV,
            LANDLOCK_ACCESS_FS_MAKE_BLOCK, LANDLOCK_ACCESS_FS_MAKE_CHAR,
            LANDLOCK_ACCESS_FS_MAKE_DIR, LANDLOCK_ACCESS_FS_MAKE_FIFO, LANDLOCK_ACCESS_FS_MAKE_REG,
            LANDLOCK_ACCESS_FS_MAKE_SOCK, LANDLOCK_ACCESS_FS_MAKE_SYM, LANDLOCK_ACCESS_FS_READ_DIR,
            LANDLOCK_ACCESS_FS_READ_FILE, LANDLOCK_ACCESS_FS_REFER, LANDLOCK_ACCESS_FS_REMOVE_DIR,
            LANDLOCK_ACCESS_FS_REMOVE_FILE, LANDLOCK_ACCESS_FS_TRUNCATE,
            LANDLOCK_ACCESS_FS_WRITE_FILE, LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET,
            LANDLOCK_SCOPE_SIGNAL,
        },
    },
};

const LANDLOCK_ABI_VERSION: u32 = 6;
const CREATE_VERSION: u32 = 1;
const CREATE_ERRATA: u32 = 2;
// Linux v6.12.103: security/landlock/errata/abi-1.h fixes erratum 3,
// abi-4.h fixes erratum 1, and abi-6.h fixes erratum 2.
const LANDLOCK_ERRATA_FIXED: u32 = 0b111;
const RULE_PATH_BENEATH: u32 = 1;
const RULE_NET_PORT: u32 = 2;
const FS_ACCESS_MASK: u64 = LANDLOCK_ACCESS_FS_EXECUTE
    | LANDLOCK_ACCESS_FS_WRITE_FILE
    | LANDLOCK_ACCESS_FS_READ_FILE
    | LANDLOCK_ACCESS_FS_READ_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_FILE
    | LANDLOCK_ACCESS_FS_MAKE_CHAR
    | LANDLOCK_ACCESS_FS_MAKE_DIR
    | LANDLOCK_ACCESS_FS_MAKE_REG
    | LANDLOCK_ACCESS_FS_MAKE_SOCK
    | LANDLOCK_ACCESS_FS_MAKE_FIFO
    | LANDLOCK_ACCESS_FS_MAKE_BLOCK
    | LANDLOCK_ACCESS_FS_MAKE_SYM
    | LANDLOCK_ACCESS_FS_REFER
    | LANDLOCK_ACCESS_FS_TRUNCATE
    | LANDLOCK_ACCESS_FS_IOCTL_DEV;
const NET_ACCESS_MASK: u64 = 3;
const SCOPE_MASK: u64 = LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET | LANDLOCK_SCOPE_SIGNAL;
const NON_DIRECTORY_FS_ACCESS_MASK: u64 = LANDLOCK_ACCESS_FS_EXECUTE
    | LANDLOCK_ACCESS_FS_WRITE_FILE
    | LANDLOCK_ACCESS_FS_READ_FILE
    | LANDLOCK_ACCESS_FS_TRUNCATE
    | LANDLOCK_ACCESS_FS_IOCTL_DEV;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct RulesetAttr {
    fs: u64,
    net: u64,
    scoped: u64,
}
#[repr(C, packed)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PathBeneathAttr {
    allowed: u64,
    parent_fd: i32,
}
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct NetPortAttr {
    allowed: u64,
    port: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct LsmCtx {
    id: u64,
    flags: u64,
    len: u64,
    ctx_len: u64,
}
const _: () = {
    assert!(size_of::<RulesetAttr>() == 24);
    assert!(size_of::<PathBeneathAttr>() == 12);
    assert!(size_of::<NetPortAttr>() == 16);
    assert!(size_of::<LsmCtx>() == 32);
};

#[derive(Clone)]
struct PathRule {
    allowed: u64,
    location: axfs_ng_vfs::Location,
}
#[derive(Clone)]
struct NetRule {
    allowed: u64,
    port: u16,
}
pub(crate) struct LandlockRuleset {
    fs: u64,
    net: u64,
    scoped: u64,
    paths: Mutex<Vec<PathRule>>,
    ports: Mutex<Vec<NetRule>>,
    snapshot_gate: Mutex<()>,
}

impl FileLike for LandlockRuleset {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok("anon_inode:[landlock-ruleset]".into())
    }
    fn set_nonblocking(&self, _: bool) -> AxResult {
        Ok(())
    }
}
impl Pollable for LandlockRuleset {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }
    fn register<'a>(
        &'a self,
        _: &mut Context<'_>,
        _: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}
impl LandlockRuleset {
    pub(crate) const fn scoped(&self) -> u64 {
        self.scoped
    }
    fn snapshot(&self) -> AxResult<Arc<Self>> {
        let _gate = self.snapshot_gate.lock();
        let paths = self.paths.lock();
        let ports = self.ports.lock();
        let mut copied_paths = Vec::new();
        copied_paths.try_reserve_exact(paths.len()).map_err(|_| AxError::NoMemory)?;
        copied_paths.extend(paths.iter().cloned());
        let mut copied_ports = Vec::new();
        copied_ports.try_reserve_exact(ports.len()).map_err(|_| AxError::NoMemory)?;
        copied_ports.extend(ports.iter().cloned());
        Arc::try_new(Self {
            fs: self.fs,
            net: self.net,
            scoped: self.scoped,
            paths: Mutex::new(copied_paths),
            ports: Mutex::new(copied_ports),
            snapshot_gate: Mutex::new(()),
        }).map_err(|_| AxError::NoMemory)
    }
    pub(crate) fn allows_net_port(&self, port: u16, access: u64) -> bool {
        let requested = access & self.net;
        requested == 0
            || self
                .ports
                .lock()
                .iter()
                .any(|rule| rule.port == port && rule.allowed & requested == requested)
    }
    fn allowed_path_access(&self, target: &axfs_ng_vfs::Location) -> u64 {
        self.paths
            .lock()
            .iter()
            .filter(|rule| rule.location.is_same_or_ancestor_of(target))
            .fold(0, |mask, rule| mask | rule.allowed)
    }
    pub(crate) fn allows_path(&self, target: &axfs_ng_vfs::Location, access: u64) -> bool {
        // A layer can only deny rights that it handles.  This includes REFER:
        // an unhandled cross-directory transition is not converted to EXDEV
        // by this ruleset.
        let requested = handled_path_access(self.fs, access);
        // Rules covering different ancestors compose: a child may grant one
        // handled right while an enclosing hierarchy grants another.  A bit
        // absent from every matching rule remains denied.
        requested == 0 || self.allowed_path_access(target) & requested == requested
    }
    pub(crate) fn destination_is_no_less_restrictive(
        &self,
        source: &axfs_ng_vfs::Location,
        destination: &axfs_ng_vfs::Location,
        access: u64,
    ) -> bool {
        self.allowed_path_access(destination)
            & self.fs
            & access
            & !(self.allowed_path_access(source) & self.fs)
            == 0
    }
}

const fn handled_path_access(handled: u64, requested: u64) -> u64 {
    handled & requested
}

fn read_value<M: UserMemory + ?Sized, T: Pod>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const T,
) -> AxResult<T> {
    VmPtr::vm_read(ptr, memory).map_err(|_| AxError::BadAddress)
}

const COPY_STRUCT_MAX: usize = 4096;

fn copy_struct_from_user<M: UserMemory + ?Sized, T: Pod + Zeroable>(
    memory: &mut UserMemoryContext<'_, M>,
    attr: *const u8,
    size: usize,
) -> AxResult<T> {
    if attr.is_null() || size == 0 {
        return Err(AxError::InvalidInput);
    }
    if size > COPY_STRUCT_MAX {
        return Err(LinuxError::E2BIG.into());
    }
    let mut known = T::zeroed();
    let copied = size.min(size_of::<T>());
    // SAFETY: the provider initializes every requested byte or reports EFAULT.
    memory
        .read_bytes(attr as usize, unsafe {
            core::slice::from_raw_parts_mut(
                (&mut known as *mut T).cast::<MaybeUninit<u8>>(),
                copied,
            )
        })
        .map_err(|_| AxError::BadAddress)?;
    let mut offset = copied;
    while offset < size {
        let count = (size - offset).min(32);
        let mut tail = [0u8; 32];
        let address = (attr as usize)
            .checked_add(offset)
            .ok_or(AxError::BadAddress)?;
        // SAFETY: as above, only the first `count` bytes are exposed.
        memory
            .read_bytes(address, unsafe {
                core::slice::from_raw_parts_mut(tail.as_mut_ptr().cast::<MaybeUninit<u8>>(), count)
            })
            .map_err(|_| AxError::BadAddress)?;
        if tail[..count].iter().any(|byte| *byte != 0) {
            return Err(LinuxError::E2BIG.into());
        }
        offset += count;
    }
    Ok(known)
}

fn get_landlock_ruleset(fd: i32) -> AxResult<FileHandle<LandlockRuleset>> {
    get_typed_file(fd).map_err(|error| {
        if error == AxError::InvalidInput {
            LinuxError::EBADFD.into()
        } else {
            error
        }
    })
}

fn create_ruleset_query(attr: *const u8, size: usize, flags: u32) -> Option<AxResult<isize>> {
    if flags == CREATE_VERSION || flags == CREATE_ERRATA {
        return Some(if !attr.is_null() || size != 0 {
            Err(AxError::InvalidInput)
        } else if flags == CREATE_VERSION {
            Ok(LANDLOCK_ABI_VERSION as isize)
        } else {
            Ok(LANDLOCK_ERRATA_FIXED as isize)
        });
    }
    (flags != 0).then(|| Err(AxError::InvalidInput))
}

/// `copy_struct_from_user()` accepts older short structures (with a zeroed
/// suffix) and future extensions only when their trailing bytes are zero.
pub fn sys_landlock_create_ruleset<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr: *const u8,
    size: usize,
    flags: u32,
) -> AxResult<isize> {
    if let Some(result) = create_ruleset_query(attr, size, flags) {
        return result;
    }
    // Linux faults a missing attribute pointer before considering whether the
    // supplied structure size is old or incomplete.
    if attr.is_null() {
        return Err(AxError::BadAddress);
    }
    if size < size_of::<u64>() {
        return Err(AxError::InvalidInput);
    }
    let a: RulesetAttr = copy_struct_from_user(memory, attr, size)?;
    if a.fs & !FS_ACCESS_MASK != 0
        || a.net & !NET_ACCESS_MASK != 0
        || a.scoped & !SCOPE_MASK != 0
    {
        return Err(AxError::InvalidInput);
    }
    if a.fs == 0 && a.net == 0 && a.scoped == 0 {
        return Err(LinuxError::ENOMSG.into());
    }
    let ruleset = Arc::try_new(LandlockRuleset {
        fs: a.fs,
        net: a.net,
        scoped: a.scoped,
        paths: Mutex::new(Vec::new()),
        ports: Mutex::new(Vec::new()),
        snapshot_gate: Mutex::new(()),
    })
    .map_err(|_| AxError::NoMemory)?;
    crate::file::add_file_like(ruleset, true).map(|fd| fd as isize)
}

pub fn sys_landlock_add_rule<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ruleset_fd: i32,
    rule_type: u32,
    rule_attr: *const u8,
    flags: u32,
) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }
    let ruleset = get_landlock_ruleset(ruleset_fd)?;
    let _snapshot_gate = ruleset.snapshot_gate.lock();
    match rule_type {
        RULE_PATH_BENEATH => {
            let a: PathBeneathAttr = read_value(memory, rule_attr.cast())?;
            if a.allowed == 0 {
                return Err(LinuxError::ENOMSG.into());
            }
            if a.allowed & !ruleset.fs != 0 {
                return Err(AxError::InvalidInput);
            }
            // A present descriptor of an unsupported object type is EBADFD;
            // an absent descriptor remains EBADF.
            let location = match location_for_fd(a.parent_fd) {
                Some(location) => location,
                None => {
                    crate::file::get_file_like(a.parent_fd)?;
                    return Err(LinuxError::EBADFD.into());
                }
            };
            if !location.is_dir() && a.allowed & !NON_DIRECTORY_FS_ACCESS_MASK != 0 {
                return Err(AxError::InvalidInput);
            }
            ruleset
                .paths
                .lock()
                .try_reserve(1)
                .map_err(|_| AxError::NoMemory)?;
            ruleset.paths.lock().push(PathRule {
                allowed: a.allowed,
                location,
            });
        }
        RULE_NET_PORT => {
            let a: NetPortAttr = read_value(memory, rule_attr.cast())?;
            if a.allowed == 0 {
                return Err(LinuxError::ENOMSG.into());
            }
            if a.allowed & !ruleset.net != 0 || a.port > u16::MAX as u64 {
                return Err(AxError::InvalidInput);
            }
            ruleset
                .ports
                .lock()
                .try_reserve(1)
                .map_err(|_| AxError::NoMemory)?;
            ruleset.ports.lock().push(NetRule {
                allowed: a.allowed,
                port: a.port as u16,
            });
        }
        _ => return Err(AxError::InvalidInput),
    }
    Ok(0)
}

/// Domain attachment is intentionally fail-closed until the thread credential
/// owns the domain stack.  Returning EPERM is Linux's normal admission error
/// when no_new_privs/CAP_SYS_ADMIN is absent, and avoids a ruleset that appears
/// enforced while it is not.
pub fn sys_landlock_restrict_self(ruleset_fd: i32, flags: u32) -> AxResult<isize> {
    let current = axtask::current();
    let caller = current.as_thread();
    let credential = caller.current_cred();
    if !caller.no_new_privs() && !crate::task::ns_capable(&credential, credential.user_ns(), CAP_SYS_ADMIN) {
        return Err(AxError::OperationNotPermitted);
    }
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }
    let ruleset = get_landlock_ruleset(ruleset_fd)?;
    let snapshot = ruleset.snapshot()?;
    let domain = caller.landlock_domain().push(snapshot)?;
    caller.replace_landlock_domain(domain);
    if caller.proc_data.proc.pid() == caller.kernel_tid() {
        caller.proc_data.replace_group_leader_landlock_domain(caller.landlock_domain());
    }
    Ok(0)
}

const LSM_ID_CAPABILITY: u64 = 100;
const LSM_ID_LANDLOCK: u64 = 110;
const ACTIVE_LSM_IDS: [u64; 2] = [LSM_ID_CAPABILITY, LSM_ID_LANDLOCK];
const LSM_FLAG_SINGLE: u32 = 1;

/// Returns the actual boot-frozen modules as the Linux ABI's u64 ID array.
pub fn sys_lsm_list_modules<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    buffer: *mut u64,
    size: *mut u32,
    flags: u32,
) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }
    let required = (size_of::<u64>() * ACTIVE_LSM_IDS.len()) as u32;
    let supplied = read_value(memory, size.cast_const())?;
    VmMutPtr::vm_write(size, memory, required).map_err(|_| AxError::BadAddress)?;
    if supplied < required {
        return Err(AxError::from(LinuxError::E2BIG));
    }
    for (index, id) in ACTIVE_LSM_IDS.into_iter().enumerate() {
        VmMutPtr::vm_write(buffer.wrapping_add(index), memory, id)
            .map_err(|_| AxError::BadAddress)?;
    }
    Ok(ACTIVE_LSM_IDS.len() as isize)
}

pub fn sys_lsm_get_self_attr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr: usize,
    ctx: *mut u8,
    size: *mut u32,
    flags: u32,
) -> AxResult<isize> {
    let attr = attr as u32;
    if attr == 0 || size.is_null() {
        return Err(AxError::InvalidInput);
    }
    let _supplied: u32 = read_value(memory, size.cast_const())?;
    if flags != 0 && flags != LSM_FLAG_SINGLE {
        return Err(AxError::InvalidInput);
    }
    if flags == LSM_FLAG_SINGLE {
        if ctx.is_null() {
            return Err(AxError::InvalidInput);
        }
        let selected: LsmCtx = read_value(memory, ctx.cast_const().cast())?;
        if selected.id == 0 {
            return Err(AxError::InvalidInput);
        }
    }
    // Neither commoncap nor Landlock implements a task label attribute.
    // Linux still reports this successful empty aggregate and updates size.
    VmMutPtr::vm_write(size, memory, 0u32).map_err(|_| AxError::BadAddress)?;
    Err(AxError::OperationNotSupported)
}
pub fn sys_lsm_set_self_attr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    _attr: usize,
    ctx: *const u8,
    size: u32,
    flags: u32,
) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }
    if size < size_of::<LsmCtx>() as u32 {
        return Err(AxError::InvalidInput);
    }
    if size > 4096 {
        return Err(AxError::from(LinuxError::E2BIG));
    }
    let supplied: LsmCtx = read_value(memory, ctx.cast())?;
    // lsm_set_self_attr copies the complete caller-provided context, not just
    // its fixed header.  This preserves EFAULT for an inaccessible tail.
    let mut offset = 0usize;
    while offset < size as usize {
        let count = ((size as usize) - offset).min(32);
        let mut copied = [0u8; 32];
        let address = (ctx as usize)
            .checked_add(offset)
            .ok_or(AxError::BadAddress)?;
        // SAFETY: the usercopy provider initializes the requested range.
        memory
            .read_bytes(address, unsafe {
                core::slice::from_raw_parts_mut(
                    copied.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                    count,
                )
            })
            .map_err(|_| AxError::BadAddress)?;
        offset += count;
    }
    let minimum = (size_of::<LsmCtx>() as u64)
        .checked_add(supplied.ctx_len)
        .ok_or(AxError::InvalidInput)?;
    if supplied.len > size as u64 || supplied.len < minimum {
        return Err(AxError::InvalidInput);
    }
    Err(AxError::OperationNotSupported)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn abi_shapes_are_linux_612() {
        assert_eq!(LANDLOCK_ABI_VERSION, 6);
        assert_eq!(size_of::<PathBeneathAttr>(), 12);
    }
    #[test]
    fn masks_exclude_unknown_bits() {
        assert_eq!(FS_ACCESS_MASK, 0xffff);
        assert_eq!(NET_ACCESS_MASK, 3);
        assert_eq!(SCOPE_MASK, 3);
        assert_eq!(ACTIVE_LSM_IDS, [100, 110]);
    }

    #[test]
    fn create_ruleset_errata_query_requires_null_zero_arguments() {
        assert_eq!(
            create_ruleset_query(core::ptr::null(), 0, CREATE_ERRATA),
            Some(Ok(LANDLOCK_ERRATA_FIXED as isize))
        );
        assert_eq!(
            create_ruleset_query(core::ptr::dangling(), 0, CREATE_ERRATA),
            Some(Err(AxError::InvalidInput))
        );
    }

    #[test]
    fn unhandled_refer_does_not_constrain_a_ruleset_layer() {
        assert_eq!(handled_path_access(0, LANDLOCK_ACCESS_FS_REFER), 0);
        assert_eq!(
            handled_path_access(LANDLOCK_ACCESS_FS_REFER, LANDLOCK_ACCESS_FS_REFER),
            LANDLOCK_ACCESS_FS_REFER
        );
    }
}
