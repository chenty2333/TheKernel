//! User address space management.

use alloc::{borrow::ToOwned, string::String, vec, vec::Vec};
use core::{cell::Cell, ffi::CStr};

use axerrno::{AxError, AxResult};
use axfs::{CachedFile, FS_CONTEXT};
use axfs_ng_vfs::Location;
use axhal::{
    mem::virt_to_phys,
    paging::{MappingFlags, PageSize},
};
use axsync::Mutex;
use axtask::current_may_uninit;
use kernel_elf_parser::{
    AuxEntry, AuxType, ELFHeaders, ELFHeadersBuilder, ELFParser, app_stack_region,
};
use linux_raw_sys::general::R_OK;
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr};
use ouroboros::self_referencing;
use uluru::LRUCache;

use crate::{
    config::{USER_SPACE_BASE, USER_SPACE_SIZE},
    file::permission::{DacFsContextExt, check_execute_permissions, check_open_permissions},
    mm::aspace::{AddrSpace, Backend},
    task::{AsThread, DacCredentialView},
};

const BINPRM_BUF_SIZE: usize = 256;
const MAX_INTERPRETER_PATH: u64 = 4096;
// Linux permits five binfmt rewrites before returning ELOOP.
const MAX_SCRIPT_RECURSION: usize = 5;

#[derive(Clone, Copy)]
enum ExecAccess<'a> {
    TrustedBoot,
    User {
        credentials: &'a DacCredentialView,
        all_readable: &'a Cell<bool>,
        has_setid_bits: &'a Cell<bool>,
    },
}

impl ExecAccess<'_> {
    fn resolve(self, path: &str) -> AxResult<Location> {
        let fs = FS_CONTEXT.lock();
        match self {
            Self::TrustedBoot => fs.resolve(path),
            Self::User { credentials, .. } => fs.resolve_dac(path, credentials),
        }
    }

    fn check_location(self, loc: &Location) -> AxResult {
        match self {
            Self::TrustedBoot => Ok(()),
            Self::User {
                credentials,
                all_readable,
                has_setid_bits,
            } => {
                check_execute_permissions(loc, credentials)?;
                if loc.metadata()?.mode.bits() & 0o6000 != 0 {
                    has_setid_bits.set(true);
                }
                match check_open_permissions(loc, R_OK, credentials) {
                    Ok(()) => Ok(()),
                    Err(AxError::PermissionDenied) => {
                        all_readable.set(false);
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
        }
    }
}

/// Security facts collected across the complete executable chain, including
/// shebang interpreters and `PT_INTERP` dynamic linkers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExecImageAccess {
    all_readable: bool,
    has_setid_bits: bool,
}

impl ExecImageAccess {
    pub(crate) fn allows_user_dumpable(self) -> bool {
        self.all_readable && !self.has_setid_bits
    }

    #[cfg(test)]
    pub(crate) const fn for_test(all_readable: bool, has_setid_bits: bool) -> Self {
        Self {
            all_readable,
            has_setid_bits,
        }
    }
}

pub(crate) struct LoadedUserApp {
    pub(crate) entry_point: VirtAddr,
    pub(crate) stack_pointer: VirtAddr,
    pub(crate) image_access: ExecImageAccess,
}

/// Creates a new empty user address space.
pub fn new_user_aspace_empty() -> AxResult<AddrSpace> {
    AddrSpace::new_empty(VirtAddr::from_usize(USER_SPACE_BASE), USER_SPACE_SIZE)
}

/// If the target architecture requires it, the kernel portion of the address
/// space will be copied to the user address space.
pub fn copy_from_kernel(_aspace: &mut AddrSpace) -> AxResult {
    #[cfg(not(any(target_arch = "aarch64", target_arch = "loongarch64")))]
    {
        // ARMv8 (aarch64) and LoongArch64 use separate page tables for user space
        // (aarch64: TTBR0_EL1, LoongArch64: PGDL), so there is no need to copy the
        // kernel portion to the user page table.
        let kspace = axmm::kernel_aspace().lock();
        _aspace.page_table_mut().cursor_no_flush().copy_from(
            kspace.page_table(),
            kspace.base(),
            kspace.size(),
        );
    }
    Ok(())
}

/// Map the signal trampoline to the user address space.
pub fn map_trampoline(aspace: &mut AddrSpace) -> AxResult {
    let signal_trampoline_paddr =
        virt_to_phys(starry_signal::arch::signal_trampoline_address().into());
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

        // TDOO: flush the I-cache
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
    fn load(loc: Location) -> AxResult<Result<Self, Vec<u8>>> {
        let file_len = loc.metadata()?.size;
        let cache = CachedFile::get_or_create(loc);

        let mut data = vec![0; 4096];
        let read = cache.read_at(&mut data[..], 0)?;
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
                if cache.read_at(&mut buf[..], range.start)? != len {
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

struct ElfLoader(LRUCache<ElfCacheEntry, 32>);

type LoadResult = Result<(VirtAddr, Vec<AuxEntry>), Vec<u8>>;

impl ElfLoader {
    const fn new() -> Self {
        Self(LRUCache::new())
    }

    fn load_path(
        &mut self,
        uspace: &mut AddrSpace,
        path: &str,
        access: ExecAccess<'_>,
    ) -> AxResult<LoadResult> {
        let loc = access.resolve(path)?;
        self.load_location(uspace, loc, access)
    }

    fn load_location(
        &mut self,
        uspace: &mut AddrSpace,
        loc: Location,
        access: ExecAccess<'_>,
    ) -> AxResult<LoadResult> {
        access.check_location(&loc)?;

        if !self.0.touch(|e| e.borrow_cache().location().ptr_eq(&loc)) {
            match ElfCacheEntry::load(loc)? {
                Ok(e) => {
                    self.0.insert(e);
                }
                Err(data) => {
                    return Ok(Err(data));
                }
            }
        }

        uspace.clear();
        map_trampoline(uspace)?;

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
            let read = cache.read_at(&mut data[..], header.offset)?;
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

        let (elf, ldso) = if let Some(ldso) = ldso {
            let loc = access.resolve(&ldso)?;
            access.check_location(&loc)?;
            if loc.ptr_eq(&executable_loc) {
                return Err(AxError::InvalidExecutable);
            }
            if !self.0.touch(|e| e.borrow_cache().location().ptr_eq(&loc)) {
                let e = ElfCacheEntry::load(loc)?
                    .map_err(|_| AxError::from(axerrno::LinuxError::ELIBBAD))?;
                self.0.insert(e);
            }

            let mut iter = self.0.iter();
            let ldso = iter.next().ok_or(AxError::BadState)?;
            let elf = iter.next().ok_or(AxError::InvalidExecutable)?;
            (elf, Some(ldso))
        } else {
            (entry, None)
        };

        let elf = map_elf(uspace, crate::config::USER_SPACE_BASE, elf)?;
        let ldso = ldso
            .map(|elf| map_elf(uspace, crate::config::USER_INTERP_BASE, elf))
            .transpose()?;

        let entry = VirtAddr::from_usize(
            ldso.as_ref()
                .map_or_else(|| elf.entry(), |ldso| ldso.entry()),
        );
        let (uid, euid, gid, egid) = current_may_uninit()
            .and_then(|task| {
                let thread = task.try_as_thread()?;
                let cred = thread.current_cred();
                let ids = cred.ids();
                let user_ns = cred.user_ns();
                Some((
                    user_ns.from_kuid_munged(ids.ruid) as usize,
                    user_ns.from_kuid_munged(ids.euid) as usize,
                    user_ns.from_kgid_munged(ids.rgid) as usize,
                    user_ns.from_kgid_munged(ids.egid) as usize,
                ))
            })
            .unwrap_or((0, 0, 0, 0));
        let secure = usize::from(uid != euid || gid != egid);
        let mut auxv = elf
            .aux_vector(PAGE_SIZE_4K, ldso.map(|elf| elf.base()))
            .collect::<Vec<_>>();
        auxv.extend([
            AuxEntry::new(AuxType::FLAGS, 0),
            AuxEntry::new(AuxType::HWCAP, 0),
            AuxEntry::new(AuxType::CLKTCK, 100),
            AuxEntry::new(AuxType::PLATFORM, 0),
            AuxEntry::new(AuxType::UID, uid),
            AuxEntry::new(AuxType::EUID, euid),
            AuxEntry::new(AuxType::GID, gid),
            AuxEntry::new(AuxType::EGID, egid),
            AuxEntry::new(AuxType::SECURE, secure),
        ]);

        Ok(Ok((entry, auxv)))
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

fn script_interpreter_args(shebang: &Shebang<'_>, path: &str, args: &[String]) -> Vec<String> {
    let mut new_args = Vec::with_capacity(args.len().saturating_add(2));
    new_args.push(shebang.interpreter.to_owned());
    if let Some(optional_arg) = shebang.optional_arg {
        new_args.push(optional_arg.to_owned());
    }
    new_args.push(path.to_owned());
    new_args.extend(args.iter().skip(1).cloned());
    new_args
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
) -> AxResult<(VirtAddr, VirtAddr)> {
    let ustack_top = VirtAddr::from_usize(crate::config::USER_STACK_TOP);
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

    let heap_start = VirtAddr::from_usize(crate::config::USER_HEAP_BASE);
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

fn finish_load_user_app(
    uspace: &mut AddrSpace,
    path: &str,
    execfn: &str,
    args: &[String],
    envs: &[String],
    load_result: AxResult<LoadResult>,
    access: ExecAccess<'_>,
    script_depth: usize,
) -> AxResult<(VirtAddr, VirtAddr)> {
    let (entry, auxv) = match load_result {
        Ok(Ok((entry, auxv))) => (entry, auxv),
        Ok(Err(data)) => {
            let Some(shebang) = parse_shebang(&data)? else {
                return Err(AxError::InvalidExecutable);
            };
            if script_depth >= MAX_SCRIPT_RECURSION {
                return Err(axerrno::LinuxError::ELOOP.into());
            }

            let new_args = script_interpreter_args(&shebang, path, args);
            return load_user_app_path(
                uspace,
                shebang.interpreter,
                execfn,
                &new_args,
                envs,
                access,
                script_depth + 1,
            );
        }
        Err(err) => return Err(err),
    };

    install_loaded_user_app(uspace, execfn, args, envs, entry, &auxv)
}

fn load_user_app_path(
    uspace: &mut AddrSpace,
    path: &str,
    execfn: &str,
    args: &[String],
    envs: &[String],
    access: ExecAccess<'_>,
    script_depth: usize,
) -> AxResult<(VirtAddr, VirtAddr)> {
    let load_result = ELF_LOADER.lock().load_path(uspace, path, access);
    finish_load_user_app(
        uspace,
        path,
        execfn,
        args,
        envs,
        load_result,
        access,
        script_depth,
    )
}

/// Load a trusted early-boot app without Linux DAC admission.
///
/// This raw path API is only for boot before a Linux credential-bearing thread
/// exists. User-originated exec must use [`load_user_app_at`].
pub(crate) fn load_user_app_trusted(
    uspace: &mut AddrSpace,
    path: Option<&str>,
    args: &[String],
    envs: &[String],
) -> AxResult<(VirtAddr, VirtAddr)> {
    let path = path
        .or_else(|| args.first().map(String::as_str))
        .ok_or(AxError::InvalidInput)?;

    load_user_app_path(uspace, path, path, args, envs, ExecAccess::TrustedBoot, 0)
}

/// Load an already resolved executable location into the user address space.
///
/// The same pre-exec credential view checks the final target, `PT_INTERP`, and
/// every shebang interpreter lookup.
pub(crate) fn load_user_app_at(
    uspace: &mut AddrSpace,
    loc: Location,
    path: &str,
    args: &[String],
    envs: &[String],
    credentials: &DacCredentialView,
) -> AxResult<LoadedUserApp> {
    let all_readable = Cell::new(true);
    let has_setid_bits = Cell::new(false);
    let access = ExecAccess::User {
        credentials,
        all_readable: &all_readable,
        has_setid_bits: &has_setid_bits,
    };
    let load_result = ELF_LOADER.lock().load_location(uspace, loc, access);
    let (entry_point, stack_pointer) =
        finish_load_user_app(uspace, path, path, args, envs, load_result, access, 0)?;
    Ok(LoadedUserApp {
        entry_point,
        stack_pointer,
        image_access: ExecImageAccess {
            all_readable: all_readable.get(),
            has_setid_bits: has_setid_bits.get(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_access_exec_image_facts_require_readable_non_setid_chain() {
        assert!(
            ExecImageAccess {
                all_readable: true,
                has_setid_bits: false,
            }
            .allows_user_dumpable()
        );
        for facts in [
            ExecImageAccess {
                all_readable: false,
                has_setid_bits: false,
            },
            ExecImageAccess {
                all_readable: true,
                has_setid_bits: true,
            },
        ] {
            assert!(!facts.allows_user_dumpable());
        }
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
            script_interpreter_args(&shebang, "/tmp/test.sh", &args),
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
