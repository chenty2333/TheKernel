//! ELF core dump generation.
//!
//! Produces a minimal ELF64 core file containing a PT_NOTE segment
//! (NT_PRSTATUS with register state) and PT_LOAD segments for each
//! user-accessible memory area.

use alloc::{format, vec};

use axerrno::{AxError, AxResult};
use axfs::{FS_CONTEXT, File, OpenOptions};
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

cfg_if::cfg_if! {
    if #[cfg(target_arch = "riscv64")] {
        const EM_ARCH: u16 = 243; // EM_RISCV
        const NUM_GREGS: usize = 32;
    } else if #[cfg(target_arch = "aarch64")] {
        const EM_ARCH: u16 = 183; // EM_AARCH64
        const NUM_GREGS: usize = 32;
    } else if #[cfg(target_arch = "x86_64")] {
        const EM_ARCH: u16 = 62; // EM_X86_64
        const NUM_GREGS: usize = 27;
    } else if #[cfg(target_arch = "loongarch64")] {
        const EM_ARCH: u16 = 258; // EM_LOONGARCH
        const NUM_GREGS: usize = 32;
    } else {
        const EM_ARCH: u16 = 0;
        const NUM_GREGS: usize = 32;
    }
}

const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
const NHDR_SIZE: usize = 12;

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

// ---- Core dump generation (RISC-V specific register extraction) ----

#[cfg(target_arch = "riscv64")]
fn fill_gregs(uctx: &UserContext, regs: &mut [u64; NUM_GREGS + 1]) {
    let r = &uctx.regs;
    // RISC-V register ordering: x0..x31 then pc.
    regs[0] = r.zero as u64;
    regs[1] = r.ra as u64;
    regs[2] = r.sp as u64;
    regs[3] = r.gp as u64;
    regs[4] = r.tp as u64;
    regs[5] = r.t0 as u64;
    regs[6] = r.t1 as u64;
    regs[7] = r.t2 as u64;
    regs[8] = r.s0 as u64;
    regs[9] = r.s1 as u64;
    regs[10] = r.a0 as u64;
    regs[11] = r.a1 as u64;
    regs[12] = r.a2 as u64;
    regs[13] = r.a3 as u64;
    regs[14] = r.a4 as u64;
    regs[15] = r.a5 as u64;
    regs[16] = r.a6 as u64;
    regs[17] = r.a7 as u64;
    regs[18] = r.s2 as u64;
    regs[19] = r.s3 as u64;
    regs[20] = r.s4 as u64;
    regs[21] = r.s5 as u64;
    regs[22] = r.s6 as u64;
    regs[23] = r.s7 as u64;
    regs[24] = r.s8 as u64;
    regs[25] = r.s9 as u64;
    regs[26] = r.s10 as u64;
    regs[27] = r.s11 as u64;
    regs[28] = r.t3 as u64;
    regs[29] = r.t4 as u64;
    regs[30] = r.t5 as u64;
    regs[31] = r.t6 as u64;
    regs[32] = uctx.sepc as u64; // PC
}

#[cfg(not(target_arch = "riscv64"))]
fn fill_gregs(uctx: &UserContext, regs: &mut [u64; NUM_GREGS + 1]) {
    // Fallback: store PC and SP in the first two slots.
    regs[0] = uctx.ip() as u64;
    regs[1] = uctx.sp() as u64;
    // Remaining registers left as zero.
}

// ---- Public API ----

/// Generates an ELF core dump file at `/tmp/core.{pid}`.
///
/// This is best-effort: errors are returned but callers should not treat
/// a failed core dump as fatal.
pub fn generate_core_dump(thr: &Thread, uctx: &UserContext, signo: u8) -> AxResult<()> {
    let proc_data = &thr.proc_data;
    let pid = proc_data.proc.pid();
    let core_limit = proc_data.rlim.read()[RLIMIT_CORE].current;
    if core_limit == 0 {
        info!("Skipping core dump for pid {pid}: RLIMIT_CORE=0");
        return Ok(());
    }
    let core_limit = if core_limit == RLIM_INFINITY as u64 {
        usize::MAX
    } else {
        core_limit.try_into().unwrap_or(usize::MAX)
    };
    let path = format!("/tmp/core.{}", pid);

    let aspace = proc_data.aspace.lock();

    // Collect user-accessible memory areas.
    let areas: alloc::vec::Vec<_> = aspace
        .areas()
        .filter(|a| a.flags().contains(MappingFlags::USER))
        .map(|a| (a.start(), a.size(), a.flags()))
        .collect();

    let num_loads = areas.len();
    let num_phdrs = 1 + num_loads; // 1 PT_NOTE + N PT_LOAD

    // ---- Layout calculation ----
    let phdrs_offset = EHDR_SIZE;
    let note_offset = phdrs_offset + PHDR_SIZE * num_phdrs;

    let note_name = b"CORE\0";
    let name_aligned = align4(note_name.len());
    let prstatus_size = core::mem::size_of::<ElfPrstatus>();
    let desc_aligned = align4(prstatus_size);
    let note_total = NHDR_SIZE + name_aligned + desc_aligned;

    let load_offset = note_offset + note_total;

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
        e_shoff: 0,
        e_flags: 0,
        e_ehsize: EHDR_SIZE as u16,
        e_phentsize: PHDR_SIZE as u16,
        e_phnum: num_phdrs as u16,
        e_shentsize: 0,
        e_shnum: 0,
        e_shstrndx: 0,
    };

    // ---- Open file ----
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&FS_CONTEXT.lock(), &path)?
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
        info!("Core dump written to {path} (truncated to {core_limit} bytes)");
    } else {
        info!("Core dump written to {path} ({file_offset} bytes)");
    }
    Ok(())
}
