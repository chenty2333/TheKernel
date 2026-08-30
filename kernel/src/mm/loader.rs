//! User address space management.

use alloc::{borrow::ToOwned, string::String, sync::Arc, vec, vec::Vec};
use core::{cell::Cell, ffi::CStr};

use axerrno::{AxError, AxResult};
use axfs::{CachedFile, FS_CONTEXT};
use axfs_ng_vfs::Location;
use axhal::{
    mem::virt_to_phys,
    paging::{MappingFlags, PageSize},
};
#[cfg(not(test))]
use axsync::Mutex;
use kernel_elf_parser::{
    AuxEntry, AuxType, ELFHeaders, ELFHeadersBuilder, ELFParser, app_stack_region,
};
use linux_raw_sys::general::R_OK;
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr};
use ouroboros::self_referencing;
#[cfg(test)]
use spin::Mutex;
use uluru::LRUCache;

use crate::{
    config::{USER_SPACE_BASE, USER_SPACE_SIZE},
    file::{
        executable::CredentialReadLease,
        permission::{
            check_execute_permissions_with_security, check_open_permissions_with_security,
            check_pathwalk_search_permission_with_security,
        },
    },
    mm::aspace::{AddrSpace, Backend},
    task::{
        AT_RSEQ_ALIGN, AT_RSEQ_FEATURE_SIZE, Cred, DacCredentialView, ExecAuxIdentity,
        ExecExecutableRole, ExecFileIdentity, ExecFileOwner, ExecFileSecurityObject, Kgid, Kuid,
        UserNamespace,
        security::{ExecExecutableSecurityContext, dispatch_exec_executable},
    },
};

const BINPRM_BUF_SIZE: usize = 256;
const MAX_INTERPRETER_PATH: u64 = 4096;
// Linux permits five binfmt rewrites before returning ELOOP.
const MAX_SCRIPT_RECURSION: usize = 5;

/// The per-exec user image layout.  Keep each displacement bounded so the
/// fixed signal trampoline and the maximum brk range retain their reservations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExecLayout {
    elf_base: usize,
    interp_base: usize,
    stack_top: usize,
    heap_base: usize,
}

impl ExecLayout {
    const ELF_SLIDE_MAX: usize = 0x0200_0000;
    const INTERP_SLIDE_MAX: usize = 0x0400_0000;
    const STACK_SLIDE_MAX: usize = 0x1000_0000;
    const HEAP_SLIDE_MAX: usize = 0x0800_0000;

    pub(crate) const fn fixed() -> Self {
        Self {
            elf_base: crate::config::USER_SPACE_BASE,
            interp_base: crate::config::USER_INTERP_BASE,
            stack_top: crate::config::USER_STACK_TOP,
            heap_base: crate::config::USER_HEAP_BASE,
        }
    }

    pub(crate) fn randomized() -> Self {
        fn slide(limit: usize) -> usize {
            let mut bytes = [0u8; core::mem::size_of::<usize>()];
            crate::random::fill_insecure(&mut bytes);
            usize::from_le_bytes(bytes) % (limit / PAGE_SIZE_4K) * PAGE_SIZE_4K
        }

        Self {
            elf_base: crate::config::USER_SPACE_BASE + slide(Self::ELF_SLIDE_MAX),
            interp_base: crate::config::USER_INTERP_BASE + slide(Self::INTERP_SLIDE_MAX),
            stack_top: crate::config::USER_STACK_TOP - slide(Self::STACK_SLIDE_MAX),
            // The fixed heap's maximum end meets the signal trampoline, so
            // randomize downward rather than growing into that reservation.
            heap_base: crate::config::USER_HEAP_BASE - slide(Self::HEAP_SLIDE_MAX),
        }
    }

    #[cfg(test)]
    fn is_fixed(self) -> bool {
        self == Self::fixed()
    }

    pub(crate) const fn heap_base(self) -> usize {
        self.heap_base
    }
}

/// Substitutes path resolution so a test can exercise the loader without a
/// mounted filesystem.
#[cfg(test)]
type TestResolver<'a> = Option<&'a dyn Fn(&str) -> AxResult<Location>>;

/// Observes each resolved path component so a test can assert the order and
/// identity of the security objects the walk produced.
#[cfg(test)]
type ComponentObserver<'a> = Option<&'a dyn Fn(&ExecFileSecurityObject) -> AxResult>;

#[derive(Clone, Copy)]
enum ExecAccess<'a> {
    TrustedBoot,
    User {
        credentials: &'a DacCredentialView,
        actor: &'a Cred,
        filesystem_owner_user_ns: &'a alloc::sync::Arc<UserNamespace>,
        all_readable: &'a Cell<bool>,
        #[cfg(test)]
        test_resolver: TestResolver<'a>,
        #[cfg(test)]
        component_observer: ComponentObserver<'a>,
    },
}

impl ExecAccess<'_> {
    fn resolve(self, path: &str) -> AxResult<Location> {
        #[cfg(test)]
        if let Self::User {
            test_resolver: Some(resolve),
            ..
        } = self
        {
            return resolve(path);
        }
        let fs = FS_CONTEXT.lock();
        match self {
            Self::TrustedBoot => fs.resolve(path),
            Self::User {
                credentials,
                actor,
                filesystem_owner_user_ns,
                ..
            } => fs.resolve_with_admission(path, &mut |directory| {
                check_pathwalk_search_permission_with_security(
                    directory,
                    actor,
                    credentials,
                    filesystem_owner_user_ns,
                )
            }),
        }
    }

    fn check_location(
        self,
        loc: &Location,
        role: ExecExecutableRole,
    ) -> AxResult<Option<ExecFileSecurityObject>> {
        match self {
            Self::TrustedBoot => Ok(None),
            Self::User {
                credentials,
                actor,
                filesystem_owner_user_ns,
                all_readable,
                #[cfg(test)]
                component_observer,
                ..
            } => {
                check_execute_permissions_with_security(
                    loc,
                    actor,
                    credentials,
                    filesystem_owner_user_ns,
                )?;
                let readable = match check_open_permissions_with_security(
                    loc,
                    R_OK,
                    actor,
                    credentials,
                    filesystem_owner_user_ns,
                ) {
                    Ok(()) => true,
                    Err(AxError::PermissionDenied) => {
                        all_readable.set(false);
                        false
                    }
                    Err(error) => return Err(error),
                };
                let metadata = loc.metadata()?;
                let owner = Kuid::from_raw(metadata.uid)
                    .zip(Kgid::from_raw(metadata.gid))
                    .map(|(uid, gid)| ExecFileOwner::new(uid, gid));
                let object = ExecFileSecurityObject::new(
                    ExecFileIdentity::new(loc.mountpoint().device(), loc.inode()),
                    filesystem_owner_user_ns.clone(),
                    owner,
                    metadata.mode.bits(),
                    readable,
                    role,
                );
                dispatch_exec_executable(&ExecExecutableSecurityContext::new(actor, &object))?;
                #[cfg(test)]
                if let Some(observe) = component_observer {
                    observe(&object)?;
                }
                Ok(Some(object))
            }
        }
    }
}

/// Security facts collected across the complete executable chain, including
/// shebang interpreters and `PT_INTERP` dynamic linkers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExecImageAccess {
    all_readable: bool,
}

impl ExecImageAccess {
    pub(crate) fn executable_unreadable(self) -> bool {
        !self.all_readable
    }

    #[cfg(test)]
    pub(crate) const fn for_test(all_readable: bool) -> Self {
        Self { all_readable }
    }
}

pub(crate) struct PreparedUserApp {
    entry_point: VirtAddr,
    auxv: Vec<AuxEntry>,
    arguments: Vec<String>,
    pub(crate) credential_source: Location,
    pub(crate) credential_source_security: Option<ExecFileSecurityObject>,
    credential_lease: Option<CredentialReadLease>,
    dynamic_linker_lease: Option<CredentialReadLease>,
    pub(crate) image_access: ExecImageAccess,
    executable: Arc<ElfCacheEntry>,
    dynamic_linker: Option<Arc<ElfCacheEntry>>,
}

impl PreparedUserApp {
    pub(crate) fn take_credential_lease(&mut self) -> AxResult<CredentialReadLease> {
        self.credential_lease.take().ok_or(AxError::BadState)
    }

    pub(crate) fn take_credential_source_security(&mut self) -> AxResult<ExecFileSecurityObject> {
        self.credential_source_security
            .take()
            .ok_or(AxError::BadState)
    }
}

pub(crate) struct LoadedUserApp {
    pub(crate) entry_point: VirtAddr,
    pub(crate) stack_pointer: VirtAddr,
    pub(crate) arguments: Vec<String>,
}

/// Creates a new empty user address space.
pub fn new_user_aspace_empty() -> AxResult<AddrSpace> {
    AddrSpace::new_empty(VirtAddr::from_usize(USER_SPACE_BASE), USER_SPACE_SIZE)
}

/// Creates an address space capable of hosting the legacy page-zero mapping.
/// Normal user images retain the 4 KiB lower bound.
pub(crate) fn new_user_aspace_with_page_zero() -> AxResult<AddrSpace> {
    AddrSpace::new_empty(VirtAddr::from_usize(0), USER_SPACE_BASE + USER_SPACE_SIZE)
}

/// Copies the kernel portion into the x86_64 user address space.
pub fn copy_from_kernel(_aspace: &mut AddrSpace) -> AxResult {
    let kspace = axmm::kernel_aspace().lock();
    _aspace.page_table_mut().cursor_no_flush().copy_from(
        kspace.page_table(),
        kspace.base(),
        kspace.size(),
    );
    Ok(())
}

/// Map the signal trampoline to the user address space.
pub fn map_trampoline(aspace: &mut AddrSpace) -> AxResult {
    let signal_trampoline_paddr =
        virt_to_phys(thekernel_linux_signal::arch::signal_trampoline_address().into());
    aspace.map_linear(
        crate::config::SIGNAL_TRAMPOLINE.into(),
        signal_trampoline_paddr,
        PAGE_SIZE_4K,
        MappingFlags::READ | MappingFlags::EXECUTE | MappingFlags::USER,
    )?;
    Ok(())
}

fn mapping_flags(flags: xmas_elf::program::Flags) -> MappingFlags {
    let mut mapping_flags = MappingFlags::USER;
    if flags.is_read() {
        mapping_flags |= MappingFlags::READ;
    }
    if flags.is_write() {
        mapping_flags |= MappingFlags::WRITE;
    }
    if flags.is_execute() {
        mapping_flags |= MappingFlags::EXECUTE;
    }
    mapping_flags
}

/// Map the elf file to the user address space.
///
/// # Arguments
/// - `uspace`: The address space of the user app.
/// - `elf`: The elf file.
///
/// # Returns
/// - The entry point of the user app.
fn map_elf<'a>(
    uspace: &mut AddrSpace,
    base: usize,
    entry: &'a ElfCacheEntry,
) -> AxResult<ELFParser<'a>> {
    let elf_parser =
        ELFParser::new(entry.borrow_elf(), base).map_err(|_| AxError::InvalidExecutable)?;
    let cache = entry.borrow_cache();
    let file_len = cache.location().metadata()?.size;

    for ph in elf_parser
        .headers()
        .ph
        .iter()
        .filter(|ph| ph.get_type() == Ok(xmas_elf::program::Type::Load))
    {
        if ph.file_size > ph.mem_size {
            return Err(AxError::InvalidExecutable);
        }
        let file_end = ph
            .offset
            .checked_add(ph.file_size)
            .filter(|end| *end <= file_len)
            .ok_or(AxError::InvalidExecutable)?;
        let vaddr = usize::try_from(ph.virtual_addr)
            .ok()
            .and_then(|address| address.checked_add(elf_parser.base()))
            .ok_or(AxError::InvalidExecutable)?;
        let mem_size = usize::try_from(ph.mem_size).map_err(|_| AxError::InvalidExecutable)?;
        if mem_size == 0 {
            continue;
        }
        let segment_end = vaddr
            .checked_add(mem_size)
            .ok_or(AxError::InvalidExecutable)?;
        debug!(
            "Mapping ELF segment: [{:#x?}, {:#x?}) flags: {}",
            vaddr, segment_end, ph.flags
        );
        let seg_pad = vaddr.align_offset_4k();
        if seg_pad as u64 != ph.offset % PAGE_SIZE_4K as u64 {
            return Err(AxError::InvalidExecutable);
        }

        let seg_align_size = mem_size
            .checked_add(seg_pad)
            .and_then(|size| size.checked_add(PAGE_SIZE_4K - 1))
            .map(|size| size & !(PAGE_SIZE_4K - 1))
            .ok_or(AxError::InvalidExecutable)?;
        if seg_align_size == 0 {
            continue;
        }
        let seg_start = VirtAddr::from_usize(vaddr);

        // Note that `offset` might not be aligned to 4K here, and it's
        // backend's responsibility to properly handle it.
        let backend = Backend::new_cow(
            seg_start,
            PageSize::Size4K,
            cache.location().clone(),
            ph.offset,
            Some(file_end),
            false,
        );
        uspace.map(
            seg_start.align_down_4k(),
            seg_align_size,
            mapping_flags(ph.flags),
            false,
            backend,
        )?;

        // Executable pages are populated lazily; AddrSpace synchronizes the
        // instruction stream when those pages or execute permissions publish.
    }

    Ok(elf_parser)
}

fn map_elf_error(err: &'static str) -> AxError {
    debug!("Failed to parse ELF file: {err}");
    AxError::InvalidExecutable
}

#[self_referencing]
struct ElfCacheEntry {
    cache: CachedFile,
    data: Vec<u8>,
    #[borrows(data)]
    #[covariant]
    elf: ELFHeaders<'this>,
}

impl ElfCacheEntry {
    fn load(loc: Location, target: &ExecLoadTarget<'_>) -> AxResult<Result<Self, Vec<u8>>> {
        let file_len = loc.metadata()?.size;
        let cache = CachedFile::get_or_create(loc);

        let mut data = vec![0; 4096];
        let read = target.read_at(&cache, &mut data[..], 0)?;
        data.truncate(read);
        match ElfCacheEntry::try_new_or_recover::<AxError>(cache.clone(), data, |data| {
            let builder = ELFHeadersBuilder::new(data).map_err(map_elf_error)?;
            let range = builder.ph_range().ok_or(AxError::InvalidExecutable)?;
            if range.end > file_len {
                return Err(AxError::InvalidExecutable);
            }
            let start = usize::try_from(range.start).map_err(|_| AxError::InvalidExecutable)?;
            let end = usize::try_from(range.end).map_err(|_| AxError::InvalidExecutable)?;
            if end <= data.len() {
                builder.build(&data[start..end])
            } else {
                let len = end.checked_sub(start).ok_or(AxError::InvalidExecutable)?;
                let mut buf = Vec::new();
                buf.try_reserve_exact(len).map_err(|_| AxError::NoMemory)?;
                buf.resize(len, 0);
                if target.read_at(&cache, &mut buf[..], range.start)? != len {
                    return Err(AxError::InvalidExecutable);
                }
                builder.build(&buf)
            }
            .map_err(map_elf_error)
        }) {
            Ok(e) => Ok(Ok(e)),
            Err((_, heads)) => Ok(Err(heads.data)),
        }
    }
}

struct ElfLoader(LRUCache<Arc<ElfCacheEntry>, 32>);

struct LoadedElfImage {
    credential_source: Location,
    credential_source_security: Option<ExecFileSecurityObject>,
    credential_lease: CredentialReadLease,
    dynamic_linker_lease: Option<CredentialReadLease>,
    executable: Arc<ElfCacheEntry>,
    dynamic_linker: Option<Arc<ElfCacheEntry>>,
}

type LoadResult = Result<LoadedElfImage, Vec<u8>>;

enum ExecLoadTarget<'a> {
    Mapped(&'a mut AddrSpace),
    Probe,
}

impl ExecLoadTarget<'_> {
    fn read_at(&self, cache: &CachedFile, buf: &mut [u8], offset: u64) -> AxResult<usize> {
        match self {
            Self::Mapped(_) => cache.read_at(buf, offset),
            Self::Probe => cache.location().entry().as_file()?.read_at(buf, offset),
        }
    }

    fn reset_image(&mut self) -> AxResult {
        match self {
            Self::Mapped(uspace) => {
                uspace.clear()?;
                map_trampoline(uspace)
            }
            Self::Probe => Ok(()),
        }
    }

    fn map_images(
        &mut self,
        elf: &ElfCacheEntry,
        ldso: Option<&ElfCacheEntry>,
        layout: ExecLayout,
    ) -> AxResult<(VirtAddr, Vec<AuxEntry>)> {
        match self {
            Self::Mapped(uspace) => {
                let elf = map_elf(uspace, layout.elf_base, elf)?;
                let ldso = ldso
                    .map(|elf| map_elf(uspace, layout.interp_base, elf))
                    .transpose()?;
                let entry = VirtAddr::from_usize(
                    ldso.as_ref()
                        .map_or_else(|| elf.entry(), |ldso| ldso.entry()),
                );
                let auxv = elf
                    .aux_vector(PAGE_SIZE_4K, ldso.map(|elf| elf.base()))
                    .collect();
                Ok((entry, auxv))
            }
            // Preflight performs the real VFS, ELF, interpreter, security,
            // and lease flow, but deliberately does not apply an image.
            Self::Probe => Ok((VirtAddr::from_usize(0), Vec::new())),
        }
    }
}

impl ElfLoader {
    const fn new() -> Self {
        Self(LRUCache::new())
    }

    fn load_path(
        &mut self,
        target: &mut ExecLoadTarget<'_>,
        path: &str,
        access: ExecAccess<'_>,
        role: ExecExecutableRole,
        layout: ExecLayout,
    ) -> AxResult<LoadResult> {
        let loc = access.resolve(path)?;
        self.load_location(target, loc, access, role, layout)
    }

    fn load_location(
        &mut self,
        target: &mut ExecLoadTarget<'_>,
        loc: Location,
        access: ExecAccess<'_>,
        role: ExecExecutableRole,
        _layout: ExecLayout,
    ) -> AxResult<LoadResult> {
        // Pin content and privilege metadata before the first admission,
        // header, cache, or metadata read from this candidate. User security
        // dispatch occurs under the outer ELF_LOADER mutex, whose hook contract
        // forbids blocking, allocation, and loader/VFS reentry. Script leases
        // drop on the non-ELF return; the final ELF lease is returned to exec.
        let credential_lease = CredentialReadLease::acquire(&loc)?;
        let credential_source_security = access.check_location(&loc, role)?;

        if !self.0.touch(|e| e.borrow_cache().location().ptr_eq(&loc)) {
            match ElfCacheEntry::load(loc, target)? {
                Ok(e) => {
                    self.0.insert(Arc::new(e));
                }
                Err(data) => {
                    return Ok(Err(data));
                }
            }
        }

        let entry = self.0.front().ok_or(AxError::BadState)?;
        let executable_loc = entry.borrow_cache().location().clone();
        let ldso = if let Some(header) = entry
            .borrow_elf()
            .ph
            .iter()
            .find(|ph| ph.get_type() == Ok(xmas_elf::program::Type::Interp))
        {
            let cache = entry.borrow_cache();
            if header.file_size == 0 || header.file_size > MAX_INTERPRETER_PATH {
                return Err(AxError::InvalidExecutable);
            }
            let interp_file_len = cache.location().metadata()?.size;
            header
                .offset
                .checked_add(header.file_size)
                .filter(|end| *end <= interp_file_len)
                .ok_or(AxError::InvalidExecutable)?;
            let interp_len =
                usize::try_from(header.file_size).map_err(|_| AxError::InvalidExecutable)?;
            let mut data = vec![0; interp_len];
            let read = target.read_at(cache, &mut data[..], header.offset)?;
            if read != data.len() {
                return Err(AxError::InvalidExecutable);
            }

            let ldso = CStr::from_bytes_with_nul(&data)
                .ok()
                .and_then(|cstr| cstr.to_str().ok())
                .ok_or(AxError::InvalidInput)?;
            debug!("Loading dynamic linker: {ldso}");
            Some(ldso.to_owned())
        } else {
            None
        };

        let (elf, ldso, ldso_lease) = if let Some(ldso) = ldso {
            let loc = access.resolve(&ldso)?;
            let lease = CredentialReadLease::acquire(&loc)?;
            let _dynamic_linker_security =
                access.check_location(&loc, ExecExecutableRole::DynamicLinker)?;
            if loc.ptr_eq(&executable_loc) {
                return Err(AxError::InvalidExecutable);
            }
            if !self.0.touch(|e| e.borrow_cache().location().ptr_eq(&loc)) {
                let e = ElfCacheEntry::load(loc, target)?
                    .map_err(|_| AxError::from(axerrno::LinuxError::ELIBBAD))?;
                self.0.insert(Arc::new(e));
            }

            let mut iter = self.0.iter();
            let ldso = iter.next().ok_or(AxError::BadState)?;
            let elf = iter.next().ok_or(AxError::InvalidExecutable)?;
            (elf, Some(ldso), Some(lease))
        } else {
            (entry, None, None)
        };

        let executable = Arc::clone(elf);
        let dynamic_linker = ldso.cloned();
        // The PT_INTERP lease is needed through its last content/backing map
        // use, but it never becomes the credential source.

        Ok(Ok(LoadedElfImage {
            credential_source: executable_loc,
            credential_source_security,
            credential_lease,
            dynamic_linker_lease: ldso_lease,
            executable,
            dynamic_linker,
        }))
    }
}

static ELF_LOADER: Mutex<ElfLoader> = Mutex::new(ElfLoader::new());

#[derive(Debug, Eq, PartialEq)]
struct Shebang<'a> {
    interpreter: &'a str,
    optional_arg: Option<&'a str>,
}

fn is_script_space(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

fn parse_shebang(data: &[u8]) -> AxResult<Option<Shebang<'_>>> {
    if !data.starts_with(b"#!") {
        return Ok(None);
    }

    let head = &data[2..data.len().min(BINPRM_BUF_SIZE)];
    let terminator = head.iter().position(|byte| *byte == b'\n' || *byte == 0);
    let may_be_truncated = terminator.is_none() && data.len() >= BINPRM_BUF_SIZE;
    let line = &head[..terminator.unwrap_or(head.len())];
    let start = line
        .iter()
        .position(|byte| !is_script_space(*byte))
        .ok_or(AxError::InvalidExecutable)?;
    if may_be_truncated && !line[start..].iter().any(|byte| is_script_space(*byte)) {
        // The kernel must not execute a truncated interpreter pathname.
        return Err(AxError::InvalidExecutable);
    }
    let end = line
        .iter()
        .rposition(|byte| !is_script_space(*byte))
        .map(|index| index + 1)
        .ok_or(AxError::InvalidExecutable)?;
    let command = &line[start..end];
    let interpreter_end = command
        .iter()
        .position(|byte| is_script_space(*byte))
        .unwrap_or(command.len());
    let interpreter =
        core::str::from_utf8(&command[..interpreter_end]).map_err(|_| AxError::InvalidInput)?;
    if interpreter.is_empty() {
        return Err(AxError::InvalidExecutable);
    }

    let optional_arg = command[interpreter_end..]
        .iter()
        .position(|byte| !is_script_space(*byte))
        .map(|offset| {
            core::str::from_utf8(&command[interpreter_end + offset..])
                .map_err(|_| AxError::InvalidInput)
        })
        .transpose()?;

    Ok(Some(Shebang {
        interpreter,
        optional_arg,
    }))
}

fn try_copy_string(value: &str) -> AxResult<String> {
    let mut copy = String::new();
    copy.try_reserve_exact(value.len())
        .map_err(|_| AxError::NoMemory)?;
    copy.push_str(value);
    Ok(copy)
}

fn try_copy_args(args: &[String], extra: usize) -> AxResult<Vec<String>> {
    let capacity = args.len().checked_add(extra).ok_or(AxError::NoMemory)?;
    let mut copy = Vec::new();
    copy.try_reserve_exact(capacity)
        .map_err(|_| AxError::NoMemory)?;
    for argument in args {
        copy.push(try_copy_string(argument)?);
    }
    Ok(copy)
}

fn script_interpreter_args(
    shebang: &Shebang<'_>,
    path: &str,
    args: &[String],
) -> AxResult<Vec<String>> {
    let mut new_args = Vec::new();
    new_args
        .try_reserve_exact(args.len().checked_add(2).ok_or(AxError::NoMemory)?)
        .map_err(|_| AxError::NoMemory)?;
    new_args.push(try_copy_string(shebang.interpreter)?);
    if let Some(optional_arg) = shebang.optional_arg {
        new_args.push(try_copy_string(optional_arg)?);
    }
    new_args.push(try_copy_string(path)?);
    for argument in args.iter().skip(1) {
        new_args.push(try_copy_string(argument)?);
    }
    Ok(new_args)
}

/// Clear the ELF cache.
///
/// Useful for removing noises during memory leak detect.
pub fn clear_elf_cache() {
    ELF_LOADER.lock().0.clear();
}

fn install_loaded_user_app(
    uspace: &mut AddrSpace,
    path: &str,
    args: &[String],
    envs: &[String],
    entry: VirtAddr,
    auxv: &[AuxEntry],
    layout: ExecLayout,
) -> AxResult<(VirtAddr, VirtAddr)> {
    let ustack_top = VirtAddr::from_usize(layout.stack_top);
    let ustack_size = crate::config::USER_STACK_SIZE;
    // Reserve one page at the bottom as an unmapped guard region.
    // Accessing it triggers a page fault -> SIGSEGV, catching stack overflow.
    let guard_size = PAGE_SIZE_4K;
    let ustack_start = ustack_top - ustack_size + guard_size;
    let ustack_mapped_size = ustack_size - guard_size;
    debug!(
        "Mapping user stack: {ustack_start:#x?} -> {ustack_top:#x?} (guard page at {:#x?})",
        ustack_top - ustack_size
    );

    uspace.map(
        ustack_start,
        ustack_mapped_size,
        MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
        false,
        Backend::new_alloc(ustack_start, PageSize::Size4K),
    )?;

    let stack_data = app_stack_region(args, envs, auxv, path, ustack_top.into());
    let user_sp = ustack_top - stack_data.len();
    let user_sp_aligned = user_sp.align_down_4k();
    uspace.populate_area(
        user_sp_aligned,
        (ustack_top - user_sp_aligned).align_up_4k(),
        MappingFlags::READ | MappingFlags::WRITE,
    )?;
    uspace.write(user_sp, stack_data.as_slice())?;

    let heap_start = VirtAddr::from_usize(layout.heap_base);
    let heap_size = crate::config::USER_HEAP_SIZE;
    uspace.map(
        heap_start,
        heap_size,
        MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
        true,
        Backend::new_alloc(heap_start, PageSize::Size4K),
    )?;

    Ok((entry, user_sp))
}

fn prepare_loaded_user_app(
    target: &mut ExecLoadTarget<'_>,
    path: &str,
    args: &[String],
    load_result: AxResult<LoadResult>,
    access: ExecAccess<'_>,
    script_depth: usize,
    layout: ExecLayout,
) -> AxResult<PreparedUserApp> {
    let loaded = match load_result {
        Ok(Ok(loaded)) => loaded,
        Ok(Err(data)) => {
            let Some(shebang) = parse_shebang(&data)? else {
                return Err(AxError::InvalidExecutable);
            };
            if script_depth >= MAX_SCRIPT_RECURSION {
                return Err(axerrno::LinuxError::ELOOP.into());
            }

            let new_args = script_interpreter_args(&shebang, path, args)?;
            return prepare_user_app_path(
                target,
                shebang.interpreter,
                &new_args,
                access,
                script_depth + 1,
                ExecExecutableRole::ScriptInterpreter,
                layout,
            );
        }
        Err(err) => return Err(err),
    };

    Ok(PreparedUserApp {
        entry_point: VirtAddr::from_usize(0),
        auxv: Vec::new(),
        arguments: try_copy_args(args, 0)?,
        credential_source: loaded.credential_source,
        credential_source_security: loaded.credential_source_security,
        credential_lease: Some(loaded.credential_lease),
        dynamic_linker_lease: loaded.dynamic_linker_lease,
        image_access: ExecImageAccess {
            all_readable: match access {
                ExecAccess::TrustedBoot => true,
                ExecAccess::User { all_readable, .. } => all_readable.get(),
            },
        },
        dynamic_linker: loaded.dynamic_linker,
        executable: loaded.executable,
    })
}

fn prepare_user_app_path(
    target: &mut ExecLoadTarget<'_>,
    path: &str,
    args: &[String],
    access: ExecAccess<'_>,
    script_depth: usize,
    role: ExecExecutableRole,
    layout: ExecLayout,
) -> AxResult<PreparedUserApp> {
    let load_result = ELF_LOADER
        .lock()
        .load_path(target, path, access, role, layout);
    prepare_loaded_user_app(
        target,
        path,
        args,
        load_result,
        access,
        script_depth,
        layout,
    )
}

/// Load a trusted early-boot app without Linux DAC admission.
///
/// This raw path API is only for boot before a Linux credential-bearing thread
/// exists. User-originated exec must use [`prepare_user_app_at`].
pub(crate) fn load_user_app_trusted(
    uspace: &mut AddrSpace,
    path: Option<&str>,
    args: &[String],
    envs: &[String],
) -> AxResult<(VirtAddr, VirtAddr)> {
    let path = path
        .or_else(|| args.first().map(String::as_str))
        .ok_or(AxError::InvalidInput)?;

    ELF_LOADER.lock().0.clear();
    let prepared = {
        let mut target = ExecLoadTarget::Mapped(uspace);
        prepare_user_app_path(
            &mut target,
            path,
            args,
            ExecAccess::TrustedBoot,
            0,
            ExecExecutableRole::Requested,
            ExecLayout::fixed(),
        )?
    };
    let loaded = finish_prepared_user_app(
        uspace,
        path,
        envs,
        prepared,
        ExecAuxIdentity::trusted_boot(),
        ExecLayout::fixed(),
    )?;
    Ok((loaded.entry_point, loaded.stack_pointer))
}

/// Load an already resolved executable location into the user address space.
///
/// The same pre-exec credential view checks the final target, `PT_INTERP`, and
/// every shebang interpreter lookup.
pub(crate) fn prepare_user_app_at(
    uspace: &mut AddrSpace,
    loc: Location,
    path: &str,
    args: &[String],
    _envs: &[String],
    credentials: &DacCredentialView,
    actor: &Cred,
    filesystem_owner_user_ns: &alloc::sync::Arc<UserNamespace>,
    layout: ExecLayout,
) -> AxResult<PreparedUserApp> {
    // The cache has no VFS content-generation cookie. Never let headers from
    // an earlier exec be paired with the current lease-protected backing.
    ELF_LOADER.lock().0.clear();
    let all_readable = Cell::new(true);
    let access = ExecAccess::User {
        credentials,
        actor,
        filesystem_owner_user_ns,
        all_readable: &all_readable,
        #[cfg(test)]
        test_resolver: None,
        #[cfg(test)]
        component_observer: None,
    };
    let mut target = ExecLoadTarget::Mapped(uspace);
    let load_result = ELF_LOADER.lock().load_location(
        &mut target,
        loc,
        access,
        ExecExecutableRole::Requested,
        layout,
    );
    prepare_loaded_user_app(&mut target, path, args, load_result, access, 0, layout)
}

/// Resolves the exact executable chain and freezes its credential source
/// without applying any mapping or personality-dependent layout.
pub(crate) fn preflight_user_app_at(
    loc: Location,
    path: &str,
    args: &[String],
    credentials: &DacCredentialView,
    actor: &Cred,
    filesystem_owner_user_ns: &alloc::sync::Arc<UserNamespace>,
) -> AxResult<PreparedUserApp> {
    ELF_LOADER.lock().0.clear();
    let all_readable = Cell::new(true);
    let access = ExecAccess::User {
        credentials,
        actor,
        filesystem_owner_user_ns,
        all_readable: &all_readable,
        #[cfg(test)]
        test_resolver: None,
        #[cfg(test)]
        component_observer: None,
    };
    let mut target = ExecLoadTarget::Probe;
    let load_result = ELF_LOADER.lock().load_location(
        &mut target,
        loc,
        access,
        ExecExecutableRole::Requested,
        ExecLayout::fixed(),
    );
    prepare_loaded_user_app(
        &mut target,
        path,
        args,
        load_result,
        access,
        0,
        ExecLayout::fixed(),
    )
}

fn map_prepared_user_app(
    uspace: &mut AddrSpace,
    prepared: &mut PreparedUserApp,
    layout: ExecLayout,
) -> AxResult<()> {
    uspace.clear()?;
    map_trampoline(uspace)?;
    let ldso_base = if let Some(ldso) = prepared.dynamic_linker.as_ref() {
        map_elf(uspace, layout.interp_base, ldso)?.base()
    } else {
        0
    };
    let elf = map_elf(uspace, layout.elf_base, &prepared.executable)?;
    let entry_point = VirtAddr::from_usize(if prepared.dynamic_linker.is_some() {
        ldso_base
    } else {
        elf.entry()
    });
    let mut auxv: Vec<AuxEntry> = elf
        .aux_vector(
            PAGE_SIZE_4K,
            prepared.dynamic_linker.as_ref().map(|_| ldso_base),
        )
        .collect();
    auxv.extend([
        AuxEntry::new(AuxType::FLAGS, 0),
        AuxEntry::new(AuxType::HWCAP, 0),
        AuxEntry::new(AuxType::CLKTCK, 100),
        AuxEntry::new(AuxType::PLATFORM, 0),
        AuxEntry::new(AuxType::RSEQ_FEATURE_SIZE, AT_RSEQ_FEATURE_SIZE),
        AuxEntry::new(AuxType::RSEQ_ALIGN, AT_RSEQ_ALIGN),
    ]);
    prepared.entry_point = entry_point;
    prepared.auxv = auxv;
    Ok(())
}

/// Installs the new user stack only after exec credential derivation has
/// supplied the exact proposed identity and secure-exec bit for auxv.
pub(crate) fn finish_prepared_user_app(
    uspace: &mut AddrSpace,
    execfn: &str,
    envs: &[String],
    mut prepared: PreparedUserApp,
    identity: ExecAuxIdentity,
    layout: ExecLayout,
) -> AxResult<LoadedUserApp> {
    map_prepared_user_app(uspace, &mut prepared, layout)?;
    prepared
        .auxv
        .try_reserve_exact(5)
        .map_err(|_| AxError::NoMemory)?;
    prepared.auxv.extend([
        AuxEntry::new(AuxType::UID, identity.uid().into_raw() as usize),
        AuxEntry::new(AuxType::EUID, identity.euid().into_raw() as usize),
        AuxEntry::new(AuxType::GID, identity.gid().into_raw() as usize),
        AuxEntry::new(AuxType::EGID, identity.egid().into_raw() as usize),
        AuxEntry::new(AuxType::SECURE, usize::from(identity.is_secure())),
    ]);
    let (entry_point, stack_pointer) = install_loaded_user_app(
        uspace,
        execfn,
        &prepared.arguments,
        envs,
        prepared.entry_point,
        &prepared.auxv,
        layout,
    )?;
    Ok(LoadedUserApp {
        entry_point,
        stack_pointer,
        arguments: prepared.arguments,
    })
}

#[cfg(test)]
mod tests {
    use core::cell::RefCell;

    use axfs_ng_vfs::{Mountpoint, NodePermission, NodeType};

    use super::*;

    #[test]
    fn fixed_exec_layout_preserves_legacy_addresses() {
        let layout = ExecLayout::fixed();
        assert!(layout.is_fixed());
        assert_eq!(layout.elf_base, crate::config::USER_SPACE_BASE);
        assert_eq!(layout.interp_base, crate::config::USER_INTERP_BASE);
        assert_eq!(layout.stack_top, crate::config::USER_STACK_TOP);
        assert_eq!(layout.heap_base, crate::config::USER_HEAP_BASE);
    }

    #[test]
    fn randomized_exec_layout_stays_page_aligned_and_in_bounds() {
        let layout = ExecLayout::randomized();
        for address in [
            layout.elf_base,
            layout.interp_base,
            layout.stack_top,
            layout.heap_base,
        ] {
            assert_eq!(address % PAGE_SIZE_4K, 0);
        }
        assert!(
            (crate::config::USER_SPACE_BASE
                ..crate::config::USER_SPACE_BASE + ExecLayout::ELF_SLIDE_MAX)
                .contains(&layout.elf_base)
        );
        assert!(
            (crate::config::USER_INTERP_BASE
                ..crate::config::USER_INTERP_BASE + ExecLayout::INTERP_SLIDE_MAX)
                .contains(&layout.interp_base)
        );
        assert!(
            (crate::config::USER_STACK_TOP - ExecLayout::STACK_SLIDE_MAX
                ..=crate::config::USER_STACK_TOP)
                .contains(&layout.stack_top)
        );
        assert!(
            (crate::config::USER_HEAP_BASE - ExecLayout::HEAP_SLIDE_MAX
                ..=crate::config::USER_HEAP_BASE)
                .contains(&layout.heap_base)
        );
    }
    use crate::{file::executable, pseudofs::tmp::MemoryFs, task::UserNamespace};

    fn create_test_file(root: &Location, name: &str, contents: &[u8]) -> Location {
        let location = root
            .create(
                name,
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap();
        assert_eq!(
            location
                .entry()
                .as_file()
                .unwrap()
                .write_at(contents, 0)
                .unwrap(),
            contents.len()
        );
        location
    }

    fn dynamic_elf_with_interp(path: &[u8]) -> Vec<u8> {
        assert_eq!(path.last(), Some(&0));
        let mut bytes = include_bytes!(
            "../../../third_party/rust-patches/kernel-elf-parser/tests/ld-linux-x86-64.so.2"
        )
        .to_vec();
        let note_index = {
            let elf = xmas_elf::ElfFile::new(&bytes).unwrap();
            elf.program_iter()
                .position(|header| header.get_type() == Ok(xmas_elf::program::Type::Note))
                .unwrap()
        };
        let ph_offset = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
        let ph_entry_size = u16::from_le_bytes(bytes[54..56].try_into().unwrap()) as usize;
        assert_eq!(ph_entry_size, 56);
        let header = ph_offset + note_index * ph_entry_size;
        let path_offset = bytes.len() as u64;
        bytes.extend_from_slice(path);
        bytes[header..header + 4].copy_from_slice(&3u32.to_le_bytes());
        bytes[header + 8..header + 16].copy_from_slice(&path_offset.to_le_bytes());
        bytes[header + 32..header + 40].copy_from_slice(&(path.len() as u64).to_le_bytes());
        bytes[header + 40..header + 48].copy_from_slice(&(path.len() as u64).to_le_bytes());
        bytes
    }

    fn create_test_loader_chain() -> (Location, Location, Location) {
        let fs = MemoryFs::new_with_capacity(Some(1024 * 1024)).unwrap();
        let mount = Mountpoint::new_root(&fs);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let root = mount.root_location();
        let script = create_test_file(&root, "script", b"#!/interp\n");
        let interpreter = create_test_file(&root, "interp", &dynamic_elf_with_interp(b"/ld.so\0"));
        let dynamic_linker = create_test_file(
            &root,
            "ld.so",
            include_bytes!(
                "../../../third_party/rust-patches/kernel-elf-parser/tests/ld-linux-x86-64.so.2"
            ),
        );
        (script, interpreter, dynamic_linker)
    }

    fn assert_write_open_available(location: &Location) {
        let key = executable::retain_write_open(location).unwrap();
        executable::release_write_open(key);
    }

    #[test]
    fn real_loader_chain_preserves_roles_terminal_source_and_lease_refunds() {
        executable::init().unwrap();
        let (script, interpreter, dynamic_linker) = create_test_loader_chain();
        let device = script.mountpoint().device();
        let namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root(namespace.clone()).unwrap();
        let credentials = actor.fs_dac_credentials();
        let all_readable = Cell::new(true);
        let observed = RefCell::new(Vec::new());
        let resolve = |path: &str| match path {
            "/interp" => Ok(interpreter.clone()),
            "/ld.so" => Ok(dynamic_linker.clone()),
            _ => Err(AxError::NotFound),
        };
        let observe = |object: &ExecFileSecurityObject| {
            observed
                .borrow_mut()
                .push((object.role(), object.identity()));
            Ok(())
        };
        let access = ExecAccess::User {
            credentials: &credentials,
            actor: &actor,
            filesystem_owner_user_ns: &namespace,
            all_readable: &all_readable,
            test_resolver: Some(&resolve),
            component_observer: Some(&observe),
        };
        ELF_LOADER.lock().0.clear();
        let mut target = ExecLoadTarget::Probe;
        let load_result = ELF_LOADER.lock().load_location(
            &mut target,
            script.clone(),
            access,
            ExecExecutableRole::Requested,
            ExecLayout::fixed(),
        );
        let prepared = prepare_loaded_user_app(
            &mut target,
            "/script",
            &["/script".into()],
            load_result,
            access,
            0,
            ExecLayout::fixed(),
        )
        .unwrap_or_else(|error| {
            panic!(
                "loader chain failed after {:?}: {error:?}",
                observed.borrow().as_slice()
            )
        });

        assert_eq!(
            observed.borrow().as_slice(),
            [
                (
                    ExecExecutableRole::Requested,
                    ExecFileIdentity::new(device, script.inode()),
                ),
                (
                    ExecExecutableRole::ScriptInterpreter,
                    ExecFileIdentity::new(device, interpreter.inode()),
                ),
                (
                    ExecExecutableRole::DynamicLinker,
                    ExecFileIdentity::new(device, dynamic_linker.inode()),
                ),
            ]
        );
        assert!(prepared.credential_source.ptr_eq(&interpreter));
        let source = prepared.credential_source_security.as_ref().unwrap();
        assert_eq!(source.role(), ExecExecutableRole::ScriptInterpreter);
        assert_eq!(
            source.identity(),
            ExecFileIdentity::new(device, interpreter.inode())
        );
        assert_write_open_available(&script);
        assert_write_open_available(&dynamic_linker);
        assert_eq!(
            executable::retain_write_open(&interpreter),
            Err(axerrno::LinuxError::ETXTBSY.into())
        );

        drop(prepared);
        ELF_LOADER.lock().0.clear();
        for location in [&script, &interpreter, &dynamic_linker] {
            assert_write_open_available(location);
        }
    }

    #[test]
    fn real_loader_chain_denial_refunds_every_component_lease() {
        executable::init().unwrap();
        let (script, interpreter, dynamic_linker) = create_test_loader_chain();
        let namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root(namespace.clone()).unwrap();
        let credentials = actor.fs_dac_credentials();
        let resolve = |path: &str| match path {
            "/interp" => Ok(interpreter.clone()),
            "/ld.so" => Ok(dynamic_linker.clone()),
            _ => Err(AxError::NotFound),
        };

        for denied_role in [
            ExecExecutableRole::Requested,
            ExecExecutableRole::ScriptInterpreter,
            ExecExecutableRole::DynamicLinker,
        ] {
            let all_readable = Cell::new(true);
            let deny_component = |object: &ExecFileSecurityObject| {
                if object.role() == denied_role {
                    Err(AxError::PermissionDenied)
                } else {
                    Ok(())
                }
            };
            let access = ExecAccess::User {
                credentials: &credentials,
                actor: &actor,
                filesystem_owner_user_ns: &namespace,
                all_readable: &all_readable,
                test_resolver: Some(&resolve),
                component_observer: Some(&deny_component),
            };
            ELF_LOADER.lock().0.clear();
            let mut target = ExecLoadTarget::Probe;
            let load_result = ELF_LOADER.lock().load_location(
                &mut target,
                script.clone(),
                access,
                ExecExecutableRole::Requested,
                ExecLayout::fixed(),
            );
            let result = prepare_loaded_user_app(
                &mut target,
                "/script",
                &["/script".into()],
                load_result,
                access,
                0,
                ExecLayout::fixed(),
            );
            let error = result
                .err()
                .unwrap_or_else(|| panic!("{denied_role:?} denial unexpectedly succeeded"));
            assert!(
                matches!(error, AxError::PermissionDenied),
                "{denied_role:?} denial returned {error:?}"
            );

            ELF_LOADER.lock().0.clear();
            for location in [&script, &interpreter, &dynamic_linker] {
                assert_write_open_available(location);
            }
        }
    }

    #[test]
    fn process_access_exec_image_facts_track_complete_chain_readability() {
        assert!(!ExecImageAccess { all_readable: true }.executable_unreadable());
        assert!(
            ExecImageAccess {
                all_readable: false
            }
            .executable_unreadable()
        );
    }

    #[test]
    fn shebang_keeps_one_optional_argument() {
        let shebang = parse_shebang(b"#!/usr/bin/env -S python -O\nprint('ok')")
            .unwrap()
            .unwrap();
        assert_eq!(shebang.interpreter, "/usr/bin/env");
        assert_eq!(shebang.optional_arg, Some("-S python -O"));
    }

    #[test]
    fn shebang_arguments_follow_linux_order() {
        let shebang = parse_shebang(b"#!  /bin/sh\t-e  \n").unwrap().unwrap();
        let args = vec!["original-argv-zero".to_owned(), "tail".to_owned()];
        assert_eq!(
            script_interpreter_args(&shebang, "/tmp/test.sh", &args).unwrap(),
            ["/bin/sh", "-e", "/tmp/test.sh", "tail"]
        );
    }

    #[test]
    fn empty_shebang_is_not_a_shell_request() {
        assert!(matches!(
            parse_shebang(b"#!  \t\n"),
            Err(AxError::InvalidExecutable)
        ));
        assert_eq!(parse_shebang(b"plain text").unwrap(), None);
    }

    #[test]
    fn truncated_interpreter_path_is_rejected() {
        let mut data = vec![b'x'; BINPRM_BUF_SIZE];
        data[0] = b'#';
        data[1] = b'!';
        assert!(matches!(
            parse_shebang(&data),
            Err(AxError::InvalidExecutable)
        ));

        data[2] = b' ';
        assert!(matches!(
            parse_shebang(&data),
            Err(AxError::InvalidExecutable)
        ));
    }
}
