//! User address space management.

use alloc::{borrow::ToOwned, string::String, vec, vec::Vec};
use core::{ffi::CStr, iter};

use axerrno::{AxError, AxResult};
use axfs::{CachedFile, FS_CONTEXT};
use axfs_ng_vfs::{Location, NodeType};
use axhal::{
    mem::virt_to_phys,
    paging::{MappingFlags, PageSize},
};
use axsync::Mutex;
use axtask::current;
use kernel_elf_parser::{
    AuxEntry, AuxType, ELFHeaders, ELFHeadersBuilder, ELFParser, app_stack_region,
};
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr};
use ouroboros::self_referencing;
use uluru::LRUCache;

use crate::{
    config::{USER_SPACE_BASE, USER_SPACE_SIZE},
    file::permission::{DacFsContextExt, check_current_execute_permissions},
    mm::aspace::{AddrSpace, Backend},
    task::AsThread,
};

const MAX_INTERPRETER_PATH: u64 = 4096;

fn resolve_exec_path(path: &str) -> AxResult<Location> {
    let fs = FS_CONTEXT.lock();
    let curr = current();
    if let Some(thread) = curr.try_as_thread() {
        let credentials = thread.proc_data.fs_dac_credentials();
        fs.resolve_dac(path, &credentials)
    } else {
        // Early kernel startup has no Linux credential-bearing thread yet.
        fs.resolve(path)
    }
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

    fn load_path(&mut self, uspace: &mut AddrSpace, path: &str) -> AxResult<LoadResult> {
        let loc = resolve_exec_path(path)?;
        self.load_location(uspace, loc)
    }

    fn load_location(&mut self, uspace: &mut AddrSpace, loc: Location) -> AxResult<LoadResult> {
        check_current_execute_permissions(&loc)?;

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
            let loc = resolve_exec_path(&ldso)?;
            if loc.ptr_eq(&executable_loc) {
                return Err(AxError::InvalidExecutable);
            }
            if !self.0.touch(|e| e.borrow_cache().location().ptr_eq(&loc)) {
                let e = ElfCacheEntry::load(loc)?.map_err(|_| AxError::InvalidInput)?;
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
        let (uid, euid, gid, egid) = if let Some(thread) = current().try_as_thread() {
            let proc_data = &thread.proc_data;
            (
                proc_data.uid() as usize,
                proc_data.euid() as usize,
                proc_data.gid() as usize,
                proc_data.egid() as usize,
            )
        } else {
            (0, 0, 0, 0)
        };
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

const SCRIPT_INTERPRETERS: &[&str] = &[
    "/musl/busybox",
    "/glibc/busybox",
    "/busybox",
    "/bin/busybox",
    "/bin/sh",
];

fn script_interpreter_args(shell: &str, path: &str, args: &[String]) -> Vec<String> {
    let mut new_args = vec![shell.to_owned()];
    if shell.ends_with("busybox") {
        new_args.push("sh".to_owned());
    }
    new_args.extend(iter::once(path.to_owned()).chain(args.iter().skip(1).cloned()));
    new_args
}

fn try_load_script_with_fallback(
    uspace: &mut AddrSpace,
    path: &str,
    args: &[String],
    envs: &[String],
) -> AxResult<(VirtAddr, VirtAddr)> {
    let mut last_err = AxError::NotFound;

    for shell in SCRIPT_INTERPRETERS.iter().copied() {
        if resolve_exec_path(shell).is_err() {
            continue;
        }

        let new_args = script_interpreter_args(shell, path, args);
        match load_user_app(uspace, None, &new_args, envs) {
            Ok(result) => return Ok(result),
            Err(err @ (AxError::NotFound | AxError::InvalidExecutable | AxError::InvalidInput)) => {
                last_err = err
            }
            Err(err) => return Err(err),
        }
    }

    Err(last_err)
}

fn permission_denied_script_fallback_allowed_for_loc(loc: &Location) -> AxResult<bool> {
    let abs_path = loc.absolute_path().map_err(|_| AxError::InvalidInput)?;
    if crate::mounts::is_noexec(abs_path.as_ref()) {
        return Ok(false);
    }

    Ok(loc.metadata()?.node_type == NodeType::RegularFile)
}

fn permission_denied_script_fallback_allowed(path: &str) -> AxResult<bool> {
    if !path.ends_with(".sh") {
        return Ok(false);
    }

    let loc = resolve_exec_path(path)?;
    permission_denied_script_fallback_allowed_for_loc(&loc)
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
    args: &[String],
    envs: &[String],
    load_result: AxResult<LoadResult>,
    fallback_loc: Option<&Location>,
) -> AxResult<(VirtAddr, VirtAddr)> {
    let (entry, auxv) = match load_result {
        Ok(Ok((entry, auxv))) => (entry, auxv),
        Ok(Err(data)) => {
            if data.starts_with(b"#!") {
                let head = &data[2..data.len().min(256)];
                let pos = head.iter().position(|c| *c == b'\n').unwrap_or(head.len());
                let line = core::str::from_utf8(&head[..pos]).map_err(|_| AxError::InvalidInput)?;

                let new_args: Vec<String> = line
                    .trim()
                    .splitn(2, |c: char| c.is_ascii_whitespace())
                    .map(|s| s.trim_ascii().to_owned())
                    .chain(iter::once(path.to_owned()))
                    .chain(args.iter().skip(1).cloned())
                    .collect();
                match load_user_app(uspace, None, &new_args, envs) {
                    Ok(result) => return Ok(result),
                    Err(
                        err @ (AxError::NotFound
                        | AxError::InvalidExecutable
                        | AxError::InvalidInput),
                    ) if path.ends_with(".sh") => {
                        debug!(
                            "script interpreter failed for {path}: {err:?}; trying shell fallback"
                        );
                        return try_load_script_with_fallback(uspace, path, args, envs);
                    }
                    Err(err) => return Err(err),
                }
            }
            // Keep `.sh` fallback for scripts without a shebang while still
            // allowing shebang-based interpreters such as `/musl/busybox sh`.
            if path.ends_with(".sh") {
                return try_load_script_with_fallback(uspace, path, args, envs);
            }
            return Err(AxError::InvalidExecutable);
        }
        Err(AxError::PermissionDenied) => {
            let fallback_allowed = if path.ends_with(".sh") {
                match fallback_loc {
                    Some(loc) => permission_denied_script_fallback_allowed_for_loc(loc)?,
                    None => permission_denied_script_fallback_allowed(path)?,
                }
            } else {
                false
            };
            if fallback_allowed {
                return try_load_script_with_fallback(uspace, path, args, envs);
            }
            return Err(AxError::PermissionDenied);
        }
        Err(err) => return Err(err),
    };

    install_loaded_user_app(uspace, path, args, envs, entry, &auxv)
}

/// Load the user app to the user address space.
///
/// # Arguments
/// - `uspace`: The address space of the user app.
/// - `args`: The arguments of the user app. The first argument is the path of
///   the user app.
/// - `envs`: The environment variables of the user app.
///
/// # Returns
/// - The entry point of the user app.
/// - The stack pointer of the user app.
pub fn load_user_app(
    uspace: &mut AddrSpace,
    path: Option<&str>,
    args: &[String],
    envs: &[String],
) -> AxResult<(VirtAddr, VirtAddr)> {
    let path = path
        .or_else(|| args.first().map(String::as_str))
        .ok_or(AxError::InvalidInput)?;

    let load_result = ELF_LOADER.lock().load_path(uspace, path);
    finish_load_user_app(uspace, path, args, envs, load_result, None)
}

/// Load an already resolved executable location into the user address space.
///
/// This is used by execve after permission checks and executable-write
/// exclusion have been performed on the same inode.
pub fn load_user_app_at(
    uspace: &mut AddrSpace,
    loc: Location,
    path: &str,
    args: &[String],
    envs: &[String],
) -> AxResult<(VirtAddr, VirtAddr)> {
    let load_result = ELF_LOADER.lock().load_location(uspace, loc.clone());
    finish_load_user_app(uspace, path, args, envs, load_result, Some(&loc))
}
