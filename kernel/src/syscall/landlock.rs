//! Landlock and LSM userspace ABI control plane.
//!
//! Ruleset descriptors deliberately retain the `Location` selected by the
//! caller.  A pathname is not a security object: keeping the VFS object avoids
//! retargeting a rule when a name is renamed or reused.

use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::{
    mem::{MaybeUninit, size_of},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use bytemuck::{Pod, Zeroable, try_pod_read_unaligned};
use linux_raw_sys::general::CAP_SYS_ADMIN;
use tk_linux_usercopy::{UserMemory, UserMemoryContext, VmMutPtr, VmPtr};

use crate::{
    file::{
        FileHandle, FileLike, Kstat, anon_inode_stat, get_typed_file, inotify::location_for_fd,
    },
    mm::copy_struct_from_user,
    task::{
        AsThread,
        security::{
            LANDLOCK_ACCESS_FS_EXECUTE, LANDLOCK_ACCESS_FS_IOCTL_DEV,
            LANDLOCK_ACCESS_FS_MAKE_BLOCK, LANDLOCK_ACCESS_FS_MAKE_CHAR,
            LANDLOCK_ACCESS_FS_MAKE_DIR, LANDLOCK_ACCESS_FS_MAKE_FIFO, LANDLOCK_ACCESS_FS_MAKE_REG,
            LANDLOCK_ACCESS_FS_MAKE_SOCK, LANDLOCK_ACCESS_FS_MAKE_SYM, LANDLOCK_ACCESS_FS_READ_DIR,
            LANDLOCK_ACCESS_FS_READ_FILE, LANDLOCK_ACCESS_FS_REFER, LANDLOCK_ACCESS_FS_REMOVE_DIR,
            LANDLOCK_ACCESS_FS_REMOVE_FILE, LANDLOCK_ACCESS_FS_RESOLVE_UNIX,
            LANDLOCK_ACCESS_FS_TRUNCATE, LANDLOCK_ACCESS_FS_WRITE_FILE,
            LANDLOCK_ACCESS_NET_BIND_TCP, LANDLOCK_ACCESS_NET_BIND_UDP,
            LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP, LANDLOCK_ACCESS_NET_CONNECT_TCP,
            LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET, LANDLOCK_SCOPE_SIGNAL, LandlockPolicy,
        },
    },
};

const LANDLOCK_ABI_VERSION: u32 = 10;
const CREATE_VERSION: u32 = 1;
const CREATE_ERRATA: u32 = 2;
// Linux v7.2.3: security/landlock/errata/abi-1.h fixes erratum 3,
// abi-4.h fixes erratum 1, and abi-6.h fixes erratum 2.  No erratum was added
// for ABI 10, so the mask is unchanged from the ABI 6..9 value.
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
    | LANDLOCK_ACCESS_FS_IOCTL_DEV
    | LANDLOCK_ACCESS_FS_RESOLVE_UNIX;
const NET_ACCESS_MASK: u64 = LANDLOCK_ACCESS_NET_BIND_TCP
    | LANDLOCK_ACCESS_NET_CONNECT_TCP
    | LANDLOCK_ACCESS_NET_BIND_UDP
    | LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP;
const SCOPE_MASK: u64 = LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET | LANDLOCK_SCOPE_SIGNAL;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct RulesetAttr {
    fs: u64,
    net: u64,
    scoped: u64,
    quiet_fs: u64,
    quiet_net: u64,
    quiet_scoped: u64,
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
    // Linux `security/landlock/syscalls.c:build_check_abi()`:
    // `BUILD_BUG_ON(sizeof(ruleset_attr) != 48)`.
    assert!(size_of::<RulesetAttr>() == 48);
    assert!(size_of::<PathBeneathAttr>() == 12);
    assert!(size_of::<NetPortAttr>() == 16);
    assert!(size_of::<LsmCtx>() == 32);
};

#[derive(Clone)]
struct PathRule {
    allowed: u64,
    location: axfs_ng_vfs::Location,
    /// `LANDLOCK_ADD_RULE_QUIET`: denials of this object's quiet access bits
    /// are kept out of the audit log (`security/landlock/audit.c`:
    /// `landlock_log_denial()`).
    quiet: bool,
}
#[derive(Clone)]
struct NetRule {
    allowed: u64,
    port: u16,
    quiet: bool,
}
pub(crate) struct LandlockRuleset {
    fs: u64,
    net: u64,
    scoped: u64,
    quiet_fs: u64,
    quiet_net: u64,
    quiet_scoped: u64,
    paths: Mutex<Vec<PathRule>>,
    ports: Mutex<Vec<NetRule>>,
    snapshot_gate: Mutex<()>,
}

impl FileLike for LandlockRuleset {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:[landlock-ruleset]",
        )))
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
        copied_paths
            .try_reserve_exact(paths.len())
            .map_err(|_| AxError::NoMemory)?;
        copied_paths.extend(paths.iter().cloned());
        let mut copied_ports = Vec::new();
        copied_ports
            .try_reserve_exact(ports.len())
            .map_err(|_| AxError::NoMemory)?;
        copied_ports.extend(ports.iter().cloned());
        Arc::try_new(Self {
            fs: self.fs,
            net: self.net,
            scoped: self.scoped,
            quiet_fs: self.quiet_fs,
            quiet_net: self.quiet_net,
            quiet_scoped: self.quiet_scoped,
            paths: Mutex::new(copied_paths),
            ports: Mutex::new(copied_ports),
            snapshot_gate: Mutex::new(()),
        })
        .map_err(|_| AxError::NoMemory)
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
        // REFER is exceptional: unlike ordinary unhandled access rights, a
        // ruleset that does not declare it denies cross-directory traversal by
        // default.  This is what makes older rulesets fail closed for rename
        // and link transitions introduced with the REFER ABI.
        if access & LANDLOCK_ACCESS_FS_REFER != 0 && self.fs & LANDLOCK_ACCESS_FS_REFER == 0 {
            return false;
        }
        // Rules covering different ancestors compose: a child may grant one
        // handled right while an enclosing hierarchy grants another.  A bit
        // absent from every matching rule remains denied.
        tk_linux_landlock::allows_path_access(
            self.fs,
            access,
            self.paths
                .lock()
                .iter()
                .filter(|rule| rule.location.is_same_or_ancestor_of(target))
                .map(|rule| rule.allowed),
        )
    }
    pub(crate) fn destination_is_no_less_restrictive(
        &self,
        source: &axfs_ng_vfs::Location,
        destination: &axfs_ng_vfs::Location,
        access: u64,
    ) -> bool {
        tk_linux_landlock::destination_is_no_less_restrictive(
            self.fs,
            access,
            self.allowed_path_access(source),
            self.allowed_path_access(destination),
        )
    }
}

impl LandlockPolicy for LandlockRuleset {
    fn scoped(&self) -> u64 {
        self.scoped()
    }
    fn allows_path(&self, target: &axfs_ng_vfs::Location, access: u64) -> bool {
        self.allows_path(target, access)
    }
    fn allows_net_port(&self, port: u16, access: u64) -> bool {
        self.allows_net_port(port, access)
    }
    fn handles_fs_access(&self, access: u64) -> bool {
        self.fs & access == access
    }
    fn destination_is_no_less_restrictive(
        &self,
        source: &axfs_ng_vfs::Location,
        destination: &axfs_ng_vfs::Location,
        access: u64,
    ) -> bool {
        self.destination_is_no_less_restrictive(source, destination, access)
    }
    fn quiets_path_denial(&self, target: &axfs_ng_vfs::Location, access: u64) -> bool {
        // Linux records one quiet bit per covered object and ORs it when two
        // rules for the same object are merged (`ruleset.c`: `add_rule()`), so
        // any quiet rule covering the target marks it for this layer.  The
        // whole requested mask stands in for the denied bits, which is a
        // superset: suppression stays at least as strict as Linux.
        let object_marked_quiet = self
            .paths
            .lock()
            .iter()
            .any(|rule| rule.quiet && rule.location.is_same_or_ancestor_of(target));
        tk_linux_landlock::quiet_object_denial(object_marked_quiet, self.quiet_fs, access)
    }
    fn quiets_net_denial(&self, port: u16, access: u64) -> bool {
        let object_marked_quiet = self
            .ports
            .lock()
            .iter()
            .any(|rule| rule.quiet && rule.port == port);
        tk_linux_landlock::quiet_object_denial(object_marked_quiet, self.quiet_net, access)
    }
    fn quiets_scope_denial(&self, scope: u64) -> bool {
        tk_linux_landlock::quiet_scope_denial(self.quiet_scoped, scope)
    }
}

fn read_value<M: UserMemory + ?Sized, T: Pod>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const T,
) -> AxResult<T> {
    VmPtr::vm_read(ptr, memory).map_err(|_| AxError::BadAddress)
}

const COPY_STRUCT_MAX: usize = memory_addr::PAGE_SIZE_4K;

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
    (flags != 0).then_some(Err(AxError::InvalidInput))
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
    if size > COPY_STRUCT_MAX {
        return Err(LinuxError::E2BIG.into());
    }
    let a: RulesetAttr = copy_struct_from_user(memory, attr, size)?;
    match tk_linux_landlock::admit_ruleset_attr(
        a.fs,
        a.net,
        a.scoped,
        a.quiet_fs,
        a.quiet_net,
        a.quiet_scoped,
    ) {
        Ok(()) => {}
        Err(tk_linux_landlock::RulesetAttrReject::Empty) => {
            return Err(LinuxError::ENOMSG.into());
        }
        Err(_) => return Err(AxError::InvalidInput),
    }
    let ruleset = Arc::try_new(LandlockRuleset {
        fs: a.fs,
        net: a.net,
        scoped: a.scoped,
        quiet_fs: a.quiet_fs,
        quiet_net: a.quiet_net,
        quiet_scoped: a.quiet_scoped,
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
    // Linux `SYSCALL_DEFINE4(landlock_add_rule, ...)`: the flag word is
    // validated before the ruleset descriptor is even resolved.
    if !tk_linux_landlock::admit_add_rule_flags(flags) {
        return Err(AxError::InvalidInput);
    }
    let quiet = flags & tk_linux_landlock::ADD_RULE_QUIET != 0;
    let ruleset = get_landlock_ruleset(ruleset_fd)?;
    let _snapshot_gate = ruleset.snapshot_gate.lock();
    match rule_type {
        RULE_PATH_BENEATH => {
            let a: PathBeneathAttr = read_value(memory, rule_attr.cast())?;
            match tk_linux_landlock::admit_path_rule_access(ruleset.fs, a.allowed, quiet) {
                Err(tk_linux_landlock::PathRuleReject::EmptyAccess) => {
                    return Err(LinuxError::ENOMSG.into());
                }
                Err(
                    tk_linux_landlock::PathRuleReject::UnhandledAccess
                    | tk_linux_landlock::PathRuleReject::QuietWithoutQuietMask,
                ) => return Err(AxError::InvalidInput),
                Err(tk_linux_landlock::PathRuleReject::NonDirectoryAccess) | Ok(()) => {}
            }
            // A quiet rule with an empty access mask still has to name a
            // ruleset that spends quiet bits on this object type.
            if quiet && ruleset.quiet_fs == 0 {
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
            match tk_linux_landlock::admit_path_rule(
                ruleset.fs,
                a.allowed,
                quiet,
                ruleset.quiet_fs,
                location.is_dir(),
            ) {
                Err(tk_linux_landlock::PathRuleReject::NonDirectoryAccess) => {
                    return Err(AxError::InvalidInput);
                }
                Err(
                    tk_linux_landlock::PathRuleReject::EmptyAccess
                    | tk_linux_landlock::PathRuleReject::UnhandledAccess
                    | tk_linux_landlock::PathRuleReject::QuietWithoutQuietMask,
                ) => unreachable!("validated before descriptor lookup"),
                Ok(()) => {}
            }
            let mut rules = ruleset.paths.lock();
            // Linux has no per-ruleset rule cap: `insert_rule()` reports -E2BIG
            // only once `ruleset->num_rules >= LANDLOCK_MAX_NUM_RULES`, and
            // that limit is U32_MAX (security/landlock/ruleset.c:281,
            // security/landlock/limits.h:20):
            //
            // 	/* There is no match for @id. */
            // 	build_check_ruleset();
            // 	if (ruleset->num_rules >= LANDLOCK_MAX_NUM_RULES)
            // 		return -E2BIG;
            // 	new_rule = create_rule(id, layers, num_layers, NULL);
            // 	if (IS_ERR(new_rule))
            // 		return PTR_ERR(new_rule);
            //
            // The only way to fail before that boundary is `create_rule()`'s
            // allocation, i.e. -ENOMEM -- which is what the fallible reservation
            // below reports.  The previous fixed limit of 4096 rules answered
            // -E2BIG long before Linux would, with an errno Linux reserves for
            // a count that cannot be represented.
            rules.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            rules.push(PathRule {
                allowed: a.allowed,
                location,
                quiet,
            });
        }
        RULE_NET_PORT => {
            let a: NetPortAttr = read_value(memory, rule_attr.cast())?;
            match tk_linux_landlock::admit_net_rule(
                ruleset.net,
                a.allowed,
                quiet,
                ruleset.quiet_net,
                a.port,
            ) {
                Err(tk_linux_landlock::NetRuleReject::EmptyAccess) => {
                    return Err(LinuxError::ENOMSG.into());
                }
                Err(
                    tk_linux_landlock::NetRuleReject::UnhandledAccess
                    | tk_linux_landlock::NetRuleReject::QuietWithoutQuietMask
                    | tk_linux_landlock::NetRuleReject::PortOutOfRange,
                ) => return Err(AxError::InvalidInput),
                Ok(()) => {}
            }
            let mut rules = ruleset.ports.lock();
            // Same rule count as the path case: -E2BIG belongs to the U32_MAX
            // boundary, not to a capacity this kernel invented.
            rules.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            rules.push(NetRule {
                allowed: a.allowed,
                port: a.port as u16,
                quiet,
            });
        }
        _ => return Err(AxError::InvalidInput),
    }
    Ok(0)
}

/// Applies one prepared domain to every live thread of the calling process.
///
/// Linux `landlock_restrict_sibling_threads()` (`security/landlock/tsync.c`)
/// gives all-or-nothing semantics: every thread prepares a credential first,
/// and only after the last one has prepared does the group commit, so a
/// failure can never leave the group half-restricted.  This implementation
/// reaches the same property by completing every fallible step -- thread
/// discovery, target pinning, one domain clone per target, and one credential
/// transition per target that still needs the caller's `no_new_privs` bit --
/// before the first thread slot is written; the commit itself is infallible.
///
/// Returns whether the group-leader identity slot must follow the new domain.
fn restrict_sibling_threads(
    caller: &crate::task::Thread,
    domain: &crate::task::security::LandlockDomain,
) -> AxResult<bool> {
    use crate::task::{AsThread, get_task};

    let leader_tid = caller.proc_data.proc.pid();
    // The process-lifecycle lock excludes membership publication and teardown:
    // `clone(CLONE_THREAD)` holds that same lock across
    // `TaskTableAdmission::commit_with_publication()`, which links the
    // task-table entry and marks the sibling's membership live in one critical
    // section.  A live membership therefore always resolves here, so discovery
    // needs exactly one pass -- and retrying could not help, because the guard
    // stays held across a yield and blocks the very publication being waited
    // for.  A sibling's own domain snapshot either copies the synchronized
    // domain to a child or is caught by the discovery below.
    let _lifecycle = caller.proc_data.lock_process_lifecycle();
    let mut targets = Vec::new();
    targets
        .try_reserve_exact(caller.proc_data.proc.thread_ids().count())
        .map_err(|_| AxError::NoMemory)?;
    for tid in caller.proc_data.proc.thread_ids() {
        if tid == caller.kernel_tid() {
            continue;
        }
        let Ok(task) = get_task(tid) else {
            unreachable!("a live sibling membership is published with its task");
        };
        let Some(thread) = task.try_as_thread() else {
            continue;
        };
        if !Arc::ptr_eq(&thread.proc_data.proc, &caller.proc_data.proc)
            || thread.kernel_tid() != tid
        {
            continue;
        }
        // Linux skips threads that already passed PF_EXITING.
        if thread.pending_exit() {
            continue;
        }
        targets.push(task);
    }
    // Fallible phase: one clone per target plus the caller's own value.
    let mut prepared = Vec::new();
    prepared
        .try_reserve_exact(targets.len() + 1)
        .map_err(|_| AxError::NoMemory)?;
    for _ in 0..targets.len() {
        prepared.push(domain.try_clone()?);
    }
    let caller_domain = domain.try_clone()?;
    // The mandatory `no_new_privs` propagation of the TSYNC path:
    //
    // 	if (ctx->set_no_new_privs)
    // 		task_set_no_new_privs(current);
    //
    // with the bit sampled once from the caller by
    // `shared_ctx.set_no_new_privs = task_no_new_privs(current);`
    // (security/landlock/tsync.c).  A sibling that lacks the bit would drop
    // the new domain at its next execve(2), because a domain survives an
    // exec only under `no_new_privs` (security/landlock/domain.c
    // `landlock_cred_security` is re-evaluated against
    // `task_no_new_privs()`), so the caller's bit is copied to every
    // sibling.  Only siblings: Linux leaves the caller's own bit -- set
    // already, or unset because the caller used CAP_SYS_ADMIN -- alone.
    let propagate_no_new_privs = caller.no_new_privs();
    let mut prepared_no_new_privs = Vec::new();
    if propagate_no_new_privs {
        prepared_no_new_privs
            .try_reserve_exact(targets.len())
            .map_err(|_| AxError::NoMemory)?;
        for task in &targets {
            let thread = task
                .try_as_thread()
                .expect("TSYNC targets were validated as threads");
            prepared_no_new_privs.push(thread.prepare_no_new_privs()?);
        }
    }
    // Infallible commit: no allocation and no failure path, so the group
    // is never left with only some threads synchronized.
    let mut leader_synced = caller.kernel_tid() == leader_tid;
    caller.replace_landlock_domain(caller_domain);
    let mut no_new_privs = prepared_no_new_privs.into_iter();
    for (task, value) in targets.iter().zip(prepared) {
        let thread = task
            .try_as_thread()
            .expect("TSYNC targets were validated as threads");
        if let Some(transition) = no_new_privs.next().flatten() {
            // Linux updates the credential before it publishes the new
            // one, so the bit is visible no later than the domain.
            thread.commit_no_new_privs(transition);
        }
        if thread.kernel_tid() == leader_tid {
            leader_synced = true;
        }
        thread.replace_landlock_domain(value);
    }
    Ok(leader_synced)
}

/// Domain attachment.  Linux `SYSCALL_DEFINE2(landlock_restrict_self, ...)`
/// requires `no_new_privs` or `CAP_SYS_ADMIN`, validates `flags` against
/// `LANDLOCK_MASK_RESTRICT_SELF`, and only accepts `ruleset_fd == -1` together
/// with `LANDLOCK_RESTRICT_SELF_LOG_SUBDOMAINS_OFF` (optionally combined with
/// `LANDLOCK_RESTRICT_SELF_TSYNC`).
pub fn sys_landlock_restrict_self(ruleset_fd: i32, flags: u32) -> AxResult<isize> {
    let current = axtask::current();
    let caller = current.as_thread();
    let credential = caller.current_cred();
    // Linux `SYSCALL_DEFINE2(landlock_restrict_self)` tests the no_new_privs /
    // `CAP_SYS_ADMIN` rule (`security/landlock/syscalls.c`) before it masks
    // the flag word, so EPERM outranks EINVAL.
    if !caller.no_new_privs()
        && !crate::task::ns_capable(&credential, credential.user_ns(), CAP_SYS_ADMIN)
    {
        return Err(AxError::OperationNotPermitted);
    }
    if flags | crate::task::security::LANDLOCK_RESTRICT_SELF_MASK
        != crate::task::security::LANDLOCK_RESTRICT_SELF_MASK
    {
        return Err(AxError::InvalidInput);
    }
    let tsync = flags & crate::task::security::LANDLOCK_RESTRICT_SELF_TSYNC != 0;
    // A ruleset may only be omitted for `-1` with exactly the subdomain-logging
    // flag, optionally combined with TSYNC.  Any other flag word still has to
    // resolve the descriptor and therefore reports EBADF.
    let ruleset_free = tk_linux_landlock::restrict_self_without_ruleset(ruleset_fd, flags);
    let domain = if ruleset_free {
        caller.landlock_domain().mute_subdomains()
    } else {
        let ruleset = get_landlock_ruleset(ruleset_fd)?;
        let snapshot = ruleset.snapshot()?;
        caller.landlock_domain().push(snapshot, flags)?
    };
    if tsync {
        if restrict_sibling_threads(caller, &domain)? {
            caller
                .proc_data
                .replace_group_leader_landlock_domain(domain);
        }
    } else {
        caller.replace_landlock_domain(domain);
        if caller.proc_data.proc.pid() == caller.kernel_tid() {
            caller
                .proc_data
                .replace_group_leader_landlock_domain(caller.landlock_domain());
        }
    }
    Ok(0)
}

const LSM_ID_UNDEF: u64 = 0;
const LSM_ID_CAPABILITY: u64 = 100;
const LSM_ID_LANDLOCK: u64 = 110;
const LSM_FLAG_SINGLE: u32 = 1;
const LSM_ATTR_UNDEF: u32 = 0;

// Keep the UAPI registry separate from the implementation's hook registry.
// The latter owns policy ordering; this one describes exactly which boot
// active modules may contribute one of the task-label attributes.  Commoncap
// and Landlock intentionally advertise no label attribute: neither Linux LSM
// registers `getselfattr`/`setselfattr`.  Linux `security_getselfattr()`
// therefore skips every module and reports `LSM_RET_DEFAULT(getselfattr)`,
// which `include/linux/lsm_hook_defs.h` defines as `-EOPNOTSUPP`; the same
// default applies to `security_setselfattr()`.  An unknown but structurally
// valid module ID is *not* an error: it simply matches no module.
const ACTIVE_LSMS: [u64; 2] = [LSM_ID_CAPABILITY, LSM_ID_LANDLOCK];

/// Copies the whole caller-supplied `struct lsm_ctx` buffer and validates its
/// structural invariants.
///
/// Linux `security_setselfattr()` reaches `memdup_user()` before it looks at
/// any field, so an inaccessible tail is `-EFAULT` even when the header would
/// have been rejected: the copy therefore happens first and the checks run on
/// the copied bytes.  `ctx == NULL` is not special-cased here for the same
/// reason -- the copy reports `-EFAULT`.
fn copy_lsm_context<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ctx: *const u8,
    supplied_size: usize,
) -> AxResult<LsmCtx> {
    let mut header = [0u8; size_of::<LsmCtx>()];
    let mut offset = 0usize;
    while offset < supplied_size {
        let chunk_len = (supplied_size - offset).min(size_of::<LsmCtx>());
        let address = (ctx as usize)
            .checked_add(offset)
            .ok_or(AxError::BadAddress)?;
        let mut chunk = [0u8; size_of::<LsmCtx>()];
        // SAFETY: the usercopy provider initializes the requested range.
        memory
            .read_bytes(address, unsafe {
                core::slice::from_raw_parts_mut(
                    chunk.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                    chunk_len,
                )
            })
            .map_err(|_| AxError::BadAddress)?;
        if offset < header.len() {
            let head_len = (header.len() - offset).min(chunk_len);
            header[offset..offset + head_len].copy_from_slice(&chunk[..head_len]);
        }
        offset += chunk_len;
    }
    let header = try_pod_read_unaligned::<LsmCtx>(&header).map_err(|_| AxError::InvalidInput)?;
    let minimum = (size_of::<LsmCtx>() as u64)
        .checked_add(header.ctx_len)
        .ok_or(AxError::InvalidInput)?;
    // `len` names the whole individual lsm_ctx record.  It can contain
    // trailing provider-private padding, but it must be wholly within the
    // caller's stated buffer and contain the context payload.
    if header.len < minimum
        || header.len > supplied_size as u64
        || header.ctx_len > supplied_size as u64 - size_of::<LsmCtx>() as u64
    {
        return Err(AxError::InvalidInput);
    }
    Ok(header)
}

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
    let required = (size_of::<u64>() * ACTIVE_LSMS.len()) as u32;
    let supplied = read_value(memory, size.cast_const())?;
    VmMutPtr::vm_write(size, memory, required).map_err(|_| AxError::BadAddress)?;
    if supplied < required {
        return Err(AxError::from(LinuxError::E2BIG));
    }
    for (index, lsm) in ACTIVE_LSMS.into_iter().enumerate() {
        VmMutPtr::vm_write(buffer.wrapping_add(index), memory, lsm)
            .map_err(|_| AxError::BadAddress)?;
    }
    Ok(ACTIVE_LSMS.len() as isize)
}

/// Linux `SYSCALL_DEFINE4(lsm_get_self_attr, ...)` ->
/// `security_getselfattr()` (`security/security.c`).
///
/// The errno order is the ABI: `LSM_ATTR_UNDEF` is the only `attr` value the
/// core rejects with `-EINVAL`; a NULL `size` is `-EINVAL`; `flags` must be
/// zero or `LSM_FLAG_SINGLE`, and the single case rejects a NULL context and
/// an undefined module ID with `-EINVAL` and an unreadable header with
/// `-EFAULT`.  After the (empty, in this kernel) `getselfattr` pass Linux
/// writes the produced total back through `size` and only then reports that no
/// module provided the attribute, which is `-EOPNOTSUPP` -- including for an
/// unknown but structurally valid module ID.
pub fn sys_lsm_get_self_attr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr: usize,
    ctx: *mut u8,
    size: *mut u32,
    flags: u32,
) -> AxResult<isize> {
    // `SYSCALL_DEFINE4` declares `unsigned int attr`, so the upper 32 bits
    // never reach the core.
    let attr = attr as u32;
    if attr == LSM_ATTR_UNDEF {
        return Err(AxError::InvalidInput);
    }
    if size.is_null() {
        return Err(AxError::InvalidInput);
    }
    // `get_user(left, size)` runs before the flag check, so an unreadable size
    // pointer is EFAULT even for an unknown flag word.
    let _supplied = read_value(memory, size.cast_const())?;
    if flags != 0 && flags != LSM_FLAG_SINGLE {
        return Err(AxError::InvalidInput);
    }
    if flags == LSM_FLAG_SINGLE {
        // "Only flag supported is LSM_FLAG_SINGLE" and an absent context is
        // rejected before its header is read.
        if ctx.is_null() {
            return Err(AxError::InvalidInput);
        }
        // `security_getselfattr()` runs `memdup_user(ctx, *size)` before it
        // looks at the ID, so a fault anywhere in the caller-declared `*size`
        // bytes is `-EFAULT`, even though only the header is inspected.
        let left = _supplied as usize;
        let mut copied = Vec::new();
        copied
            .try_reserve_exact(left)
            .map_err(|_| AxError::NoMemory)?;
        copied.resize(left, MaybeUninit::uninit());
        memory
            .read_bytes(ctx as usize, &mut copied)
            .map_err(|_| AxError::BadAddress)?;
        let mut header_bytes = [0u8; size_of::<LsmCtx>()];
        let inspected = left.min(header_bytes.len());
        // SAFETY: `read_bytes` initialized the first `left` bytes.
        let initialized =
            unsafe { core::slice::from_raw_parts(copied.as_ptr().cast::<u8>(), inspected) };
        header_bytes[..inspected].copy_from_slice(initialized);
        let header: LsmCtx =
            try_pod_read_unaligned(&header_bytes).map_err(|_| AxError::BadAddress)?;
        // "If the LSM ID isn't specified it is an error."
        if header.id == LSM_ID_UNDEF {
            return Err(AxError::InvalidInput);
        }
        // Linux reads only the ID here.  `len`/`ctx_len` belong to the
        // producing module and are not validated on the get path.
    }
    // No boot-active module registers `getselfattr`, so the pass completes
    // with a zero total; `put_user(total, size)` still runs and its fault wins
    // over the "nothing provided this attribute" result.
    VmMutPtr::vm_write(size, memory, 0u32).map_err(|_| AxError::BadAddress)?;
    Err(AxError::OperationNotSupported)
}

/// Linux `SYSCALL_DEFINE4(lsm_set_self_attr, ...)` ->
/// `security_setselfattr()` (`security/security.c`).
///
/// The core never inspects `attr`: with no module registering `setselfattr`
/// every structurally valid request reports the hook default `-EOPNOTSUPP`,
/// whatever the module ID or attribute.  Structural failures keep their own
/// errnos: `-EINVAL` for `flags`, for `size` below one `struct lsm_ctx`, and
/// for an inconsistent `len`/`ctx_len`; `-E2BIG` above one page; `-EFAULT` for
/// an unreadable buffer, including a NULL one.
pub fn sys_lsm_set_self_attr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr: usize,
    ctx: *const u8,
    size: u32,
    flags: u32,
) -> AxResult<isize> {
    // `security_setselfattr()` never inspects @attr.
    let _ = attr;
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }
    let size = size as usize;
    if size < size_of::<LsmCtx>() {
        return Err(AxError::InvalidInput);
    }
    if size > COPY_STRUCT_MAX {
        return Err(AxError::from(LinuxError::E2BIG));
    }
    let _context = copy_lsm_context(memory, ctx, size)?;
    // No boot-active module registers `setselfattr`, so the loop matches
    // nothing and `LSM_RET_DEFAULT(setselfattr)` applies.
    Err(AxError::OperationNotSupported)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// `config/linux-contracts.toml` names this symbol. The name is deliberately
    /// version-free: these assertions track the release pinned by
    /// `config/linux-abi.toml`, and a re-pin must not leave a stale number here.
    #[test]
    fn abi_shapes_are_pinned_linux_release() {
        assert_eq!(LANDLOCK_ABI_VERSION, 10);
        assert_eq!(size_of::<RulesetAttr>(), 48);
        assert_eq!(size_of::<PathBeneathAttr>(), 12);
        assert_eq!(size_of::<NetPortAttr>(), 16);
    }
    #[test]
    fn masks_exclude_unknown_bits() {
        assert_eq!(FS_ACCESS_MASK, 0x1_ffff);
        assert_eq!(NET_ACCESS_MASK, 0xf);
        assert_eq!(SCOPE_MASK, 3);
        assert_eq!(ACTIVE_LSMS, [100, 110]);
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
        assert_eq!(
            create_ruleset_query(core::ptr::null(), 0, CREATE_VERSION),
            Some(Ok(LANDLOCK_ABI_VERSION as isize))
        );
    }

    #[test]
    fn ruleset_attr_decoding_gates_every_abi10_field() {
        // `LANDLOCK_ACCESS_FS_RESOLVE_UNIX` and the UDP rights are the ABI 9
        // and ABI 10 additions; both are accepted and unknown bits are not.
        assert_eq!(
            tk_linux_landlock::admit_ruleset_attr(0x1_ffff, 0, 0, 0, 0, 0),
            Ok(())
        );
        assert_eq!(
            tk_linux_landlock::admit_ruleset_attr(1 << 16, 0, 0, 1 << 16, 0, 0),
            Ok(())
        );
        assert_eq!(
            tk_linux_landlock::admit_ruleset_attr(0, 0xf, 0, 0, 0xf, 0),
            Ok(())
        );
        assert_eq!(
            tk_linux_landlock::admit_ruleset_attr(0x2_0000, 0, 0, 0, 0, 0),
            Err(tk_linux_landlock::RulesetAttrReject::UnknownFsAccess)
        );
        assert_eq!(
            tk_linux_landlock::admit_ruleset_attr(0, 0x10, 0, 0, 0, 0),
            Err(tk_linux_landlock::RulesetAttrReject::UnknownNetAccess)
        );
        // A quiet mask outside its handled mask is EINVAL, and an empty
        // ruleset is ENOMSG -- the two errnos differ, so order matters.
        assert_eq!(
            tk_linux_landlock::admit_ruleset_attr(0, 0, 0, 0, 0, 0),
            Err(tk_linux_landlock::RulesetAttrReject::Empty)
        );
        assert_eq!(
            tk_linux_landlock::admit_ruleset_attr(1, 0, 0, 2, 0, 0),
            Err(tk_linux_landlock::RulesetAttrReject::QuietFsWithoutHandled)
        );
    }

    #[test]
    fn restrict_self_flag_mask_covers_tsync() {
        use crate::task::security::{
            LANDLOCK_RESTRICT_SELF_LOG_MASK, LANDLOCK_RESTRICT_SELF_MASK,
            LANDLOCK_RESTRICT_SELF_TSYNC,
        };
        const SUBDOMAINS_OFF: u32 = 1 << 2;
        assert_eq!(LANDLOCK_RESTRICT_SELF_MASK, 0b1111);
        assert_eq!(LANDLOCK_RESTRICT_SELF_TSYNC, 0b1000);
        assert_eq!(
            LANDLOCK_RESTRICT_SELF_MASK & !LANDLOCK_RESTRICT_SELF_LOG_MASK,
            0b1000
        );
        // Unknown flag bits are EINVAL (`flags | MASK != MASK`).
        let defined =
            |flags: u32| flags | LANDLOCK_RESTRICT_SELF_MASK == LANDLOCK_RESTRICT_SELF_MASK;
        assert!(defined(0));
        assert!(defined(SUBDOMAINS_OFF | LANDLOCK_RESTRICT_SELF_TSYNC));
        assert!(!defined(1 << 4));
        // A ruleset-free call needs exactly the subdomain flag, with TSYNC as
        // an optional companion; every other flag word still resolves the
        // descriptor, so `-1` is EBADF there.
        assert!(tk_linux_landlock::restrict_self_without_ruleset(
            -1,
            SUBDOMAINS_OFF
        ));
        assert!(tk_linux_landlock::restrict_self_without_ruleset(
            -1,
            SUBDOMAINS_OFF | LANDLOCK_RESTRICT_SELF_TSYNC
        ));
        assert!(!tk_linux_landlock::restrict_self_without_ruleset(-1, 0));
        assert!(!tk_linux_landlock::restrict_self_without_ruleset(
            0,
            SUBDOMAINS_OFF
        ));
    }
}
