//! ELF core dump generation.
//!
//! Produces a minimal ELF64 core file containing a PT_NOTE segment
//! (NT_PRSTATUS with register state) and PT_LOAD segments for each
//! user-accessible memory area.

use alloc::{format, vec};

use axerrno::{AxError, AxResult};
use axfs::{File, OpenOptions};
use axfs_ng_vfs::FsPathBuf;
use axhal::{paging::MappingFlags, uspace::UserContext};
use linux_raw_sys::general::{RLIM_INFINITY, RLIMIT_CORE};
use memory_addr::PAGE_SIZE_4K;

use super::Thread;

// ---- ELF constants ----

const ELFMAG: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;
const ELFOSABI_NONE: u8 = 0;
const ET_CORE: u16 = 4;
const PT_NOTE: u32 = 4;
const PT_LOAD: u32 = 1;
const PF_R: u32 = 4;
const PF_W: u32 = 2;
const PF_X: u32 = 1;
const NT_PRSTATUS: u32 = 1;

const EM_ARCH: u16 = 62; // EM_X86_64
const NUM_GREGS: usize = 27;

const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
const SHDR_SIZE: usize = 64;
const NHDR_SIZE: usize = 12;
const PN_XNUM: usize = 0xffff;

// ---- ELF structures ----

#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Ehdr {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Nhdr {
    n_namesz: u32,
    n_descsz: u32,
    n_type: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Elf64Shdr {
    sh_name: u32,
    sh_type: u32,
    sh_flags: u64,
    sh_addr: u64,
    sh_offset: u64,
    sh_size: u64,
    sh_link: u32,
    sh_info: u32,
    sh_addralign: u64,
    sh_entsize: u64,
}

/// Minimal `prstatus` for core dump (architecture-independent layout).
///
/// On Linux the exact layout depends on the architecture. We store the
/// most useful subset: signal info, PID, and general-purpose registers
/// including the program counter.
#[repr(C)]
#[derive(Clone, Copy)]
struct ElfPrstatus {
    si_signo: i32,
    si_code: i32,
    si_errno: i32,
    pr_cursig: u16,
    _pad0: u16,
    pr_sigpend: u64,
    pr_sighold: u64,
    pr_pid: i32,
    pr_ppid: i32,
    pr_pgrp: i32,
    pr_sid: i32,
    pr_utime: [u64; 2],
    pr_stime: [u64; 2],
    pr_cutime: [u64; 2],
    pr_cstime: [u64; 2],
    /// General registers followed by the program counter.
    pr_reg: [u64; NUM_GREGS + 1],
}

// ---- Helpers ----

/// Re-interprets a `#[repr(C)]` value as a byte slice.
unsafe fn as_bytes<T: Sized>(val: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts(val as *const T as *const u8, core::mem::size_of::<T>()) }
}

/// Aligns `v` up to 4-byte boundary (ELF note alignment).
const fn align4(v: usize) -> usize {
    (v + 3) & !3
}

fn truncate_len(limit: usize, offset: usize, size: usize) -> usize {
    limit.saturating_sub(offset).min(size)
}

fn write_limited(file: &File, offset: u64, data: &[u8], limit: usize) -> AxResult<usize> {
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    let mut written = 0;
    let target = truncate_len(limit, start, data.len());
    while written < target {
        let len = file.write_at(&data[written..target], offset + written as u64)?;
        if len == 0 {
            return Err(AxError::WriteZero);
        }
        written += len;
    }
    Ok(written)
}

fn mapping_flags_to_elf(flags: MappingFlags) -> u32 {
    let mut pf = 0u32;
    if flags.contains(MappingFlags::READ) {
        pf |= PF_R;
    }
    if flags.contains(MappingFlags::WRITE) {
        pf |= PF_W;
    }
    if flags.contains(MappingFlags::EXECUTE) {
        pf |= PF_X;
    }
    pf
}

// ---- Core dump generation (x86_64 register extraction) ----

fn fill_gregs(uctx: &UserContext, regs: &mut [u64; NUM_GREGS + 1]) {
    // Keep the compact register view stable: the first slots carry the
    // instruction and stack pointers, while the remaining slots are zero.
    regs[0] = uctx.ip() as u64;
    regs[1] = uctx.sp() as u64;
}

// ---- Public API ----

/// Generates an ELF core dump file at `/tmp/core.{pid}`.
///
/// This is best-effort: errors are returned but callers should not treat
/// a failed core dump as fatal.
pub fn generate_core_dump(thr: &Thread, uctx: &UserContext, signo: u8) -> AxResult<bool> {
    let proc_data = &thr.proc_data;
    let pid = proc_data.proc.pid();
    let core_limit = proc_data.rlim.read()[RLIMIT_CORE].current;
    if core_limit == 0 {
        info!("Skipping core dump for pid {pid}: RLIMIT_CORE=0");
        return Ok(false);
    }
    let core_limit = if core_limit == RLIM_INFINITY as u64 {
        usize::MAX
    } else {
        core_limit.try_into().unwrap_or(usize::MAX)
    };
    let Some(aspace_handle) = proc_data.coredump_aspace() else {
        info!("Skipping core dump for pid {pid}: process image is not dumpable");
        return Ok(false);
    };
    let path = FsPathBuf::from_vec(format!("/tmp/core.{pid}").into_bytes());
    let aspace = aspace_handle.lock();

    // Collect exact user-accessible segments.  AddrSpace splits around
    // MADV_DONTDUMP sidecars so a partial-VMA exclusion cannot leak into the
    // core or suppress adjacent dumpable bytes.
    let areas = aspace.coredump_segments()?;

    let num_loads = areas.len();
    let num_phdrs = num_loads.checked_add(1).ok_or(AxError::NoMemory)?; // 1 PT_NOTE + N PT_LOAD
    let extended_phnum = num_phdrs >= PN_XNUM;
    let extended_phnum_value = extended_phnum
        .then(|| u32::try_from(num_phdrs).map_err(|_| AxError::NoMemory))
        .transpose()?;

    // ---- Layout calculation ----
    let phdrs_offset = EHDR_SIZE;
    let phdrs_size = PHDR_SIZE.checked_mul(num_phdrs).ok_or(AxError::NoMemory)?;
    let phdrs_end = phdrs_offset
        .checked_add(phdrs_size)
        .ok_or(AxError::NoMemory)?;
    let note_offset = phdrs_end
        .checked_add(usize::from(extended_phnum) * SHDR_SIZE)
        .ok_or(AxError::NoMemory)?;

    let note_name = b"CORE\0";
    let name_aligned = align4(note_name.len());
    let prstatus_size = core::mem::size_of::<ElfPrstatus>();
    let desc_aligned = align4(prstatus_size);
    let note_total = NHDR_SIZE + name_aligned + desc_aligned;

    let load_offset = note_offset
        .checked_add(note_total)
        .and_then(|end| end.checked_add(PAGE_SIZE_4K - 1))
        .map(|end| end & !(PAGE_SIZE_4K - 1))
        .ok_or(AxError::NoMemory)?;

    // ---- Build prstatus ----
    let ppid = proc_data.proc.parent().map_or(0, |p| p.pid() as i32);
    let pgid = proc_data.proc.group().pgid() as i32;

    let mut prstatus = ElfPrstatus {
        si_signo: signo as i32,
        si_code: 0,
        si_errno: 0,
        pr_cursig: signo as u16,
        _pad0: 0,
        pr_sigpend: 0,
        pr_sighold: 0,
        pr_pid: pid as i32,
        pr_ppid: ppid,
        pr_pgrp: pgid,
        pr_sid: 0,
        pr_utime: [0; 2],
        pr_stime: [0; 2],
        pr_cutime: [0; 2],
        pr_cstime: [0; 2],
        pr_reg: [0u64; NUM_GREGS + 1],
    };
    fill_gregs(uctx, &mut prstatus.pr_reg);

    // ---- Build ELF header ----
    let mut e_ident = [0u8; 16];
    e_ident[0..4].copy_from_slice(&ELFMAG);
    e_ident[4] = ELFCLASS64;
    e_ident[5] = ELFDATA2LSB;
    e_ident[6] = EV_CURRENT;
    e_ident[7] = ELFOSABI_NONE;

    let ehdr = Elf64Ehdr {
        e_ident,
        e_type: ET_CORE,
        e_machine: EM_ARCH,
        e_version: 1,
        e_entry: 0,
        e_phoff: phdrs_offset as u64,
        e_shoff: if extended_phnum { phdrs_end as u64 } else { 0 },
        e_flags: 0,
        e_ehsize: EHDR_SIZE as u16,
        e_phentsize: PHDR_SIZE as u16,
        e_phnum: if extended_phnum {
            PN_XNUM as u16
        } else {
            num_phdrs as u16
        },
        e_shentsize: if extended_phnum { SHDR_SIZE as u16 } else { 0 },
        e_shnum: if extended_phnum { 1 } else { 0 },
        e_shstrndx: 0,
    };

    // ---- Open file ----
    let file = OpenOptions::new()
        .write(true)
        // Exclusive creation rejects both pre-existing files and dangling
        // symlinks, so a privileged dump cannot overwrite or follow an
        // attacker-prepared `/tmp/core.<pid>` path.
        .create_new(true)
        .open(&crate::task::current_fs_context().lock(), &path)?
        .into_file()?;

    let mut offset = 0u64;

    // ---- Write ELF header ----
    write_limited(&file, offset, unsafe { as_bytes(&ehdr) }, core_limit)?;
    offset += EHDR_SIZE as u64;

    // ---- Write PT_NOTE program header ----
    let note_phdr = Elf64Phdr {
        p_type: PT_NOTE,
        p_flags: 0,
        p_offset: note_offset as u64,
        p_vaddr: 0,
        p_paddr: 0,
        p_filesz: truncate_len(core_limit, note_offset, note_total) as u64,
        p_memsz: note_total as u64,
        p_align: 4,
    };
    write_limited(&file, offset, unsafe { as_bytes(&note_phdr) }, core_limit)?;
    offset += PHDR_SIZE as u64;

    // ---- Write PT_LOAD program headers ----
    let mut cur_load_offset = load_offset;
    for &(start, size, flags) in &areas {
        let phdr = Elf64Phdr {
            p_type: PT_LOAD,
            p_flags: mapping_flags_to_elf(flags),
            p_offset: cur_load_offset as u64,
            p_vaddr: start.as_usize() as u64,
            p_paddr: 0,
            p_filesz: truncate_len(core_limit, cur_load_offset, size) as u64,
            p_memsz: size as u64,
            p_align: PAGE_SIZE_4K as u64,
        };
        write_limited(&file, offset, unsafe { as_bytes(&phdr) }, core_limit)?;
        offset += PHDR_SIZE as u64;
        cur_load_offset += size;
    }

    // ELF stores an extended program-header count in section header zero's
    // sh_info field.  This keeps alternating page-sized DONTDUMP ranges from
    // wrapping e_phnum and corrupting every following file offset.
    if let Some(actual_phnum) = extended_phnum_value {
        let shdr = Elf64Shdr {
            sh_info: actual_phnum,
            ..Elf64Shdr::default()
        };
        write_limited(&file, offset, unsafe { as_bytes(&shdr) }, core_limit)?;
    }

    // ---- Write NOTE segment ----
    let nhdr = Elf64Nhdr {
        n_namesz: note_name.len() as u32,
        n_descsz: prstatus_size as u32,
        n_type: NT_PRSTATUS,
    };
    let mut note_off = note_offset as u64;
    write_limited(&file, note_off, unsafe { as_bytes(&nhdr) }, core_limit)?;
    note_off += NHDR_SIZE as u64;

    // Write name + padding.
    let mut name_buf = [0u8; 8]; // name_aligned is at most 8
    name_buf[..note_name.len()].copy_from_slice(note_name);
    write_limited(&file, note_off, &name_buf[..name_aligned], core_limit)?;
    note_off += name_aligned as u64;

    // Write prstatus descriptor.
    write_limited(&file, note_off, unsafe { as_bytes(&prstatus) }, core_limit)?;

    // ---- Write LOAD segment data (memory contents) ----
    let mut file_offset = load_offset as u64;
    let mut buf = vec![0u8; PAGE_SIZE_4K];
    for &(start, size, _) in &areas {
        if file_offset as usize >= core_limit {
            break;
        }
        let mut remaining = size;
        let mut vaddr = start;
        while remaining > 0 {
            if file_offset as usize >= core_limit {
                break;
            }
            let chunk = remaining.min(PAGE_SIZE_4K);
            buf[..chunk].fill(0);
            // Read from the address space; unmapped pages stay as zeros.
            let _ = aspace.read(vaddr, &mut buf[..chunk]);
            let written = write_limited(&file, file_offset, &buf[..chunk], core_limit)?;
            file_offset += written as u64;
            vaddr += chunk;
            remaining -= chunk;
        }
    }

    drop(aspace);

    if file_offset as usize >= core_limit {
        info!(
            "Core dump written to {:?} (truncated to {core_limit} bytes)",
            path.as_bytes()
        );
    } else {
        info!(
            "Core dump written to {:?} ({file_offset} bytes)",
            path.as_bytes()
        );
    }
    Ok(true)
}
