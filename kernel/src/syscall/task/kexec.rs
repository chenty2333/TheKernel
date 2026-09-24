//! x86_64 raw kexec image admission.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
#[cfg(feature = "smp-tlb-shootdown")]
use core::sync::atomic::AtomicUsize;
use core::{
    ffi::c_void,
    mem::{MaybeUninit, align_of, size_of},
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

use axalloc::{UsageKind, global_allocator, replace_pages_at};
use axerrno::{AxError, AxResult, LinuxError};
use axhal::mem::{phys_to_virt, virt_to_phys};
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::general::CAP_SYS_BOOT;
use tk_linux_usercopy::{UserMemory, UserMemoryContext, VmPtr};

use crate::{
    file::{File, FileLike, get_typed_file},
    task::{
        AsThread, Cred,
        security::{KernelLoadKind, authorize_kernel_load_data},
    },
};

const KEXEC_SEGMENT_MAX: usize = 16;
const KEXEC_ON_CRASH: u64 = 1;
const KEXEC_PRESERVE_CONTEXT: u64 = 2;
const KEXEC_UPDATE_ELFCOREHDR: u64 = 4;
const KEXEC_CRASH_HOTPLUG_SUPPORT: u64 = 8;
/// `KEXEC_FLAGS` (include/linux/kexec.h) for the `CONFIG_KEXEC_JUMP=n` build
/// TheKernel and the pinned 7.2.3 oracle are both produced from.
const KEXEC_FLAGS: u64 = KEXEC_ON_CRASH | KEXEC_UPDATE_ELFCOREHDR | KEXEC_CRASH_HOTPLUG_SUPPORT;
/// `KEXEC_PRESERVE_CONTEXT` is a legal `kexec_load` flag only under
/// `CONFIG_KEXEC_JUMP`, so it must stay out of [`KEXEC_FLAGS`] here.
const _: () = assert!(KEXEC_FLAGS & KEXEC_PRESERVE_CONTEXT == 0);
const KEXEC_ARCH_MASK: u64 = 0xffff << 16;
/// `KEXEC_DESTINATION_MEMORY_LIMIT` (arch/x86/include/asm/kexec.h):
///
/// ```c
/// /* Maximum address we can reach in physical address mode */
/// # define KEXEC_DESTINATION_MEMORY_LIMIT (MAXMEM-1)
/// ```
///
/// with `MAXMEM` from arch/x86/include/asm/pgtable_64_types.h:
///
/// ```c
/// # define MAX_PHYSMEM_BITS	(pgtable_l5_enabled() ? 52 : 46)
/// # define MAXMEM			(1UL << MAX_PHYSMEM_BITS)
/// ```
///
/// TheKernel programs four-level page tables here (nothing sets `CR4.LA57`,
/// see crates/ax/tk-axplat-x86-pc/src/boot.rs), so `MAX_PHYSMEM_BITS` is 46.
const KEXEC_DESTINATION_MEMORY_LIMIT: usize = (1 << 46) - 1;
const KEXEC_ARCH_X86_64: u64 = 62 << 16;
const PAGE_SIZE: usize = 4096;
const KEXEC_FILE_UNLOAD: u64 = 1;
const KEXEC_FILE_ON_CRASH: u64 = 2;
const KEXEC_FILE_NO_INITRAMFS: u64 = 4;
const KEXEC_FILE_DEBUG: u64 = 8;
const KEXEC_FILE_NO_CMA: u64 = 16;
const KEXEC_FILE_FORCE_DTB: u64 = 32;
const KEXEC_FILE_MAX_IMAGE: usize = 512 * 1024 * 1024;
const MAX_32BIT_PADDR: usize = u32::MAX as usize;
const LINUX_BOOT_PARAMS_PADDR: usize = 0x90000;
const LINUX_CMDLINE_PADDR: usize = 0x91000;
const LINUX_RSDP_PADDR: usize = 0x92000;
const BOOT_PARAMS_SIZE: usize = PAGE_SIZE;
const BOOT_STACK_SIZE: usize = PAGE_SIZE;
const COPY_PAGE_HEADER: usize = 3 * size_of::<u64>();
const COPY_PAGE_PAYLOAD: usize = PAGE_SIZE - COPY_PAGE_HEADER;
const PTE_PRESENT_RW: u64 = 0x003;
const SETUP_HEADER_OFFSET: usize = 0x1f1;
const SETUP_HEADER_END: usize = 0x290;
const BOOT_FLAG_OFFSET: usize = 0x1fe;
const TYPE_OF_LOADER_OFFSET: usize = 0x210;
const LOADFLAGS_OFFSET: usize = 0x211;
const CODE32_START_OFFSET: usize = 0x214;
const RAMDISK_IMAGE_OFFSET: usize = 0x218;
const RAMDISK_SIZE_OFFSET: usize = 0x21c;
const CMDLINE_PTR_OFFSET: usize = 0x228;
const XLOADFLAGS_OFFSET: usize = 0x236;
const CMDLINE_SIZE_OFFSET: usize = 0x238;
const LOADED_HIGH: u8 = 1;
const XLF_KERNEL_64: u16 = 1;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern)]
pub(crate) struct KexecSegment {
    buf: *const c_void,
    bufsz: usize,
    mem: usize,
    memsz: usize,
}
const _: () = {
    assert!(size_of::<KexecSegment>() == 32);
    assert!(align_of::<KexecSegment>() == 8);
};

struct ReservedSegment {
    paddr: usize,
    pages: usize,
    bytes: Vec<u8>,
    owns_pages: bool,
}
impl Drop for ReservedSegment {
    fn drop(&mut self) {
        // A zero-length destination owns no page: `pages == 0` must not reach
        // the allocator.  Linux keeps the same distinction by never allocating
        // a page for a segment whose `memsz` is zero.
        if self.owns_pages && self.pages != 0 {
            global_allocator().dealloc_pages(
                phys_to_virt(self.paddr.into()).as_usize(),
                self.pages,
                UsageKind::Kexec,
            );
        }
    }
}
struct KexecImage {
    entry: usize,
    segments: Vec<ReservedSegment>,
    boot_params: usize,
    startup_32: bool,
}

/// Crash images carry every fallible transition resource before publication.
/// The panic path may run with allocator and scheduler state compromised, so
/// it is limited to a try-lock, CPU quiesce, raw copies, and the final jump.
struct CrashKexecImage {
    image: KexecImage,
    #[cfg(target_os = "none")]
    transition: TransitionImage,
}

impl CrashKexecImage {
    fn prepare(image: KexecImage) -> AxResult<Self> {
        #[cfg(target_os = "none")]
        {
            let transition = TransitionImage::new(&image)?;
            prepare_transition_trampoline(&image, &transition)?;
            Ok(Self { image, transition })
        }
        #[cfg(not(target_os = "none"))]
        {
            Ok(Self { image })
        }
    }
}

/// All state which must remain usable after CR3 is replaced.  Page-table
/// pages, low boot parameters and the low stack are reserved up front and are
/// never allocated while the terminal copy is under way.
struct TransitionImage {
    tables: Vec<ReservedSegment>,
    boot_params: ReservedSegment,
    stack: ReservedSegment,
    trampoline: ReservedSegment,
    copier: ReservedSegment,
    control: ReservedSegment,
    copy_pages: Vec<ReservedSegment>,
    cr3: usize,
}

#[repr(C)]
struct CopyControl {
    first: u64,
    cr3: u64,
    stack_top: u64,
    boot_params: u64,
    entry: u64,
    startup_32: u64,
    handoff_stub: u64,
}

impl TransitionImage {
    fn overlaps(exclusions: &[(usize, usize)], paddr: usize, pages: usize) -> bool {
        let Some(end) = paddr.checked_add(pages * PAGE_SIZE) else {
            return true;
        };
        exclusions
            .iter()
            .any(|&(start, excluded_end)| paddr < excluded_end && start < end)
    }

    fn reserve_page_avoiding(
        exclusions: &[(usize, usize)],
        quarantine: &mut Vec<ReservedSegment>,
    ) -> AxResult<ReservedSegment> {
        loop {
            quarantine.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            let vaddr = global_allocator()
                .alloc_pages(1, PAGE_SIZE, UsageKind::Kexec)
                .map_err(|_| AxError::NoMemory)?;
            let paddr = virt_to_phys(vaddr.into()).as_usize();
            let page = ReservedSegment {
                paddr,
                pages: 1,
                bytes: Vec::new(),
                owns_pages: true,
            };
            if !Self::overlaps(exclusions, paddr, 1) {
                return Ok(page);
            }
            // Retain rejected pages until every transition resource has been
            // placed so the allocator cannot hand the same destination page
            // back on the next iteration.
            quarantine.push(page);
        }
    }

    fn reserve_fixed(paddr: usize, bytes: usize, content: Vec<u8>) -> AxResult<ReservedSegment> {
        replace_pages_at(
            phys_to_virt(paddr.into()).as_usize(),
            bytes / PAGE_SIZE,
            PAGE_SIZE,
        )
        .map_err(|_| AxError::NoMemory)?;
        Ok(ReservedSegment {
            paddr,
            pages: bytes / PAGE_SIZE,
            bytes: content,
            owns_pages: true,
        })
    }

    fn reserve_table(
        tables: &mut Vec<ReservedSegment>,
        exclusions: &[(usize, usize)],
        quarantine: &mut Vec<ReservedSegment>,
    ) -> AxResult<usize> {
        // Reserve vector ownership before allocating the physical page.  Once
        // allocation succeeds, zeroing and push are infallible, so every page
        // is either owned by `tables` (and dropped once) or was never taken.
        tables.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        let page = Self::reserve_page_avoiding(exclusions, quarantine)?;
        let paddr = page.paddr;
        // SAFETY: `page` was just reserved for this table, so its PAGE_SIZE bytes in the direct
        // map belong to nobody else.
        unsafe { core::ptr::write_bytes(phys_to_virt(paddr.into()).as_mut_ptr(), 0, PAGE_SIZE) };
        tables.push(page);
        Ok(paddr)
    }

    fn table_entry(table: usize, index: usize) -> *mut u64 {
        // SAFETY: `table` is the root or a table page this builder reserved, and every caller
        // masks `index` to 0..=511, so the entry lies inside that page's direct-map image.
        unsafe {
            phys_to_virt(table.into())
                .as_mut_ptr()
                .cast::<u64>()
                .add(index)
        }
    }

    fn map_page(
        &mut self,
        va: usize,
        pa: usize,
        exclusions: &[(usize, usize)],
        quarantine: &mut Vec<ReservedSegment>,
    ) -> AxResult<()> {
        let mut table = self.cr3;
        for shift in [39usize, 30, 21] {
            let slot = Self::table_entry(table, (va >> shift) & 511);
            // SAFETY: `slot` points into a transition table page this builder owns (see
            // `table_entry`).
            let entry = unsafe { slot.read_volatile() };
            if entry & 1 == 0 {
                let next = Self::reserve_table(&mut self.tables, exclusions, quarantine)?;
                // SAFETY: as above; `next` is a freshly reserved, zeroed table page.
                unsafe { slot.write_volatile((next as u64) | PTE_PRESENT_RW) };
                table = next;
            } else {
                table = entry as usize & !0xfff;
            }
        }
        // SAFETY: the final-level entry lies in a table page this builder owns (see
        // `table_entry`).
        unsafe {
            Self::table_entry(table, (va >> 12) & 511).write_volatile((pa as u64) | PTE_PRESENT_RW)
        };
        Ok(())
    }

    fn map_range(
        &mut self,
        paddr: usize,
        bytes: usize,
        exclusions: &[(usize, usize)],
        quarantine: &mut Vec<ReservedSegment>,
    ) -> AxResult<()> {
        for offset in (0..bytes).step_by(PAGE_SIZE) {
            let pa = paddr.checked_add(offset).ok_or(AxError::InvalidInput)?;
            // Preserve both the direct-map alias used by the platform and the
            // identity alias expected by the Linux decompressor.
            self.map_page(pa, pa, exclusions, quarantine)?;
            self.map_page(
                phys_to_virt(pa.into()).as_usize(),
                pa,
                exclusions,
                quarantine,
            )?;
        }
        Ok(())
    }

    /// Maps code which is currently reached through the direct map as both
    /// its direct-map and physical identity aliases.  The extra page covers a
    /// small Rust wrapper straddling a page boundary; assembly ranges supply
    /// their exact length separately.
    fn map_kernel_code_range(
        &mut self,
        vaddr: usize,
        bytes: usize,
        exclusions: &[(usize, usize)],
        quarantine: &mut Vec<ReservedSegment>,
    ) -> AxResult<()> {
        if bytes == 0 {
            return Err(AxError::BadState);
        }
        let first = vaddr & !(PAGE_SIZE - 1);
        let last = range_end(vaddr, bytes.saturating_sub(1))? & !(PAGE_SIZE - 1);
        for va in (first..=last).step_by(PAGE_SIZE) {
            let pa = virt_to_phys(va.into()).as_usize();
            self.map_page(va, pa, exclusions, quarantine)?;
            self.map_page(pa, pa, exclusions, quarantine)?;
        }
        Ok(())
    }

    fn new(image: &KexecImage) -> AxResult<Self> {
        let mut exclusions = Vec::new();
        exclusions
            .try_reserve_exact(image.segments.len())
            .map_err(|_| AxError::NoMemory)?;
        for segment in &image.segments {
            exclusions.push((
                segment.paddr,
                segment
                    .paddr
                    .checked_add(segment.pages * PAGE_SIZE)
                    .ok_or(AxError::InvalidInput)?,
            ));
        }
        let mut quarantine = Vec::new();
        let mut tables = Vec::new();
        let cr3 = Self::reserve_table(&mut tables, &exclusions, &mut quarantine)?;
        let boot_params = Self::reserve_page_avoiding(&exclusions, &mut quarantine)?;
        let stack = reserve_transition_low_page(&exclusions)?;
        let trampoline = reserve_transition_low_page(&exclusions)?;
        let copier = Self::reserve_page_avoiding(&exclusions, &mut quarantine)?;
        let control = Self::reserve_page_avoiding(&exclusions, &mut quarantine)?;

        let copy_page_count = image
            .segments
            .iter()
            .try_fold(0usize, |count, segment| {
                segment
                    .pages
                    .checked_mul(PAGE_SIZE)
                    .and_then(|bytes| count.checked_add(bytes.div_ceil(COPY_PAGE_PAYLOAD)))
            })
            .ok_or(AxError::NoMemory)?;
        let mut copy_pages = Vec::new();
        copy_pages
            .try_reserve_exact(copy_page_count)
            .map_err(|_| AxError::NoMemory)?;
        let mut first = 0usize;
        let mut previous = 0usize;
        for segment in &image.segments {
            let total = segment
                .pages
                .checked_mul(PAGE_SIZE)
                .ok_or(AxError::InvalidInput)?;
            let mut offset = 0usize;
            while offset < total {
                let page = Self::reserve_page_avoiding(&exclusions, &mut quarantine)?;
                let current = page.paddr;
                let length = (total - offset).min(COPY_PAGE_PAYLOAD);
                let address = phys_to_virt(current.into()).as_mut_ptr();
                // SAFETY: `page` was just reserved, so its direct-map page is exclusively ours,
                // and the header words at offsets 8 and 16 lie inside it.
                unsafe {
                    core::ptr::write_bytes(address, 0, PAGE_SIZE);
                    address
                        .cast::<u64>()
                        .add(1)
                        .write((segment.paddr + offset) as u64);
                    address.cast::<u64>().add(2).write(length as u64);
                }
                if offset < segment.bytes.len() {
                    let initialized = length.min(segment.bytes.len() - offset);
                    // SAFETY: `initialized <= segment.bytes.len() - offset`, and `COPY_PAGE_HEADER
                    // + length <= PAGE_SIZE` because `length <= COPY_PAGE_PAYLOAD`; the source Vec
                    // and the reserved page cannot overlap.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            segment.bytes.as_ptr().add(offset),
                            address.add(COPY_PAGE_HEADER),
                            initialized,
                        )
                    };
                }
                if first == 0 {
                    first = current;
                }
                if previous != 0 {
                    // SAFETY: `previous` is the page reserved on the prior iteration, still owned
                    // by this chain; its first word is the next-page link.
                    unsafe {
                        phys_to_virt(previous.into())
                            .as_mut_ptr()
                            .cast::<u64>()
                            .write(current as u64)
                    };
                }
                previous = current;
                copy_pages.push(page);
                offset += length;
            }
        }

        let copy_blob = axhal::kexec::copy_transition_blob();
        if copy_blob.is_empty() || copy_blob.len() > PAGE_SIZE {
            return Err(AxError::BadState);
        }
        // SAFETY: the copier and control pages were reserved for this transition; the blob fits in
        // one page (checked above) and the 56-byte `CopyControl` fits in the control page.
        unsafe {
            let destination = phys_to_virt(copier.paddr.into()).as_mut_ptr();
            core::ptr::write_bytes(destination, 0, PAGE_SIZE);
            core::ptr::copy_nonoverlapping(copy_blob.as_ptr(), destination, copy_blob.len());
            let control_record = CopyControl {
                first: first as u64,
                cr3: cr3 as u64,
                stack_top: (stack.paddr + BOOT_STACK_SIZE) as u64,
                boot_params: image.boot_params as u64,
                entry: image.entry as u64,
                startup_32: image.startup_32 as u64,
                handoff_stub: trampoline.paddr as u64,
            };
            core::ptr::write(
                phys_to_virt(control.paddr.into())
                    .as_mut_ptr()
                    .cast::<CopyControl>(),
                control_record,
            );
        }
        let mut transition = Self {
            tables,
            boot_params,
            stack,
            trampoline,
            copier,
            control,
            copy_pages,
            cr3,
        };
        for segment in &image.segments {
            transition.map_range(
                segment.paddr,
                segment.pages * PAGE_SIZE,
                &exclusions,
                &mut quarantine,
            )?;
        }
        for index in 0..transition.copy_pages.len() {
            let paddr = transition.copy_pages[index].paddr;
            transition.map_range(paddr, PAGE_SIZE, &exclusions, &mut quarantine)?;
        }
        let control_paddr = transition.control.paddr;
        transition.map_range(control_paddr, PAGE_SIZE, &exclusions, &mut quarantine)?;
        let copier_paddr = transition.copier.paddr;
        transition.map_range(copier_paddr, PAGE_SIZE, &exclusions, &mut quarantine)?;
        let transition_boot_params = transition.boot_params.paddr;
        transition.map_range(
            transition_boot_params,
            BOOT_PARAMS_SIZE,
            &exclusions,
            &mut quarantine,
        )?;
        let transition_stack = transition.stack.paddr;
        transition.map_range(
            transition_stack,
            BOOT_STACK_SIZE,
            &exclusions,
            &mut quarantine,
        )?;
        // The CR3 switch returns through both this Rust ABI wrapper and its
        // separately linked assembly implementation.  Map each at its direct
        // and identity aliases before changing CR3.
        transition.map_kernel_code_range(
            axhal::kexec::copy_transition as *const () as usize,
            PAGE_SIZE * 2,
            &exclusions,
            &mut quarantine,
        )?;
        let (copy_enter, copy_enter_len) = axhal::kexec::copy_transition_entry_range();
        transition.map_kernel_code_range(
            copy_enter,
            copy_enter_len,
            &exclusions,
            &mut quarantine,
        )?;
        let transition_trampoline = transition.trampoline.paddr;
        transition.map_range(
            transition_trampoline,
            PAGE_SIZE,
            &exclusions,
            &mut quarantine,
        )?;
        // All retained resources are outside the image.  Rejected allocator
        // pages can now return to normal ownership; the terminal engine never
        // dereferences this temporary vector.
        quarantine.clear();
        Ok(transition)
    }
}
static NORMAL: Mutex<Option<KexecImage>> = Mutex::new(None);
static CRASH: AtomicPtr<CrashKexecImage> = AtomicPtr::new(ptr::null_mut());
static KEXEC_LOAD_TRANSACTION: Mutex<()> = Mutex::new(());
static KEXEC_LOAD_DISABLED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "smp-tlb-shootdown")]
static STOP_ACKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp-tlb-shootdown")]
static CRASH_STOP: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "smp-tlb-shootdown")]
static STOP_HANDLER_READY: AtomicBool = AtomicBool::new(false);

fn publish_crash_image(image: Option<CrashKexecImage>) -> AxResult<()> {
    let new = match image {
        Some(image) => Box::into_raw(Box::try_new(image).map_err(|_| AxError::NoMemory)?),
        None => ptr::null_mut(),
    };
    let old = CRASH.swap(new, Ordering::AcqRel);
    if !old.is_null() {
        // SAFETY: every non-null `CRASH` pointer comes from `Box::into_raw`, and the swap
        // transferred sole ownership of `old` to this call.
        unsafe { drop(Box::from_raw(old)) };
    }
    Ok(())
}

/// Execute the CPU-local restoration edge before publishing terminal-stop
/// acknowledgement. The caller never resumes normal execution afterwards.
#[cfg(feature = "smp-tlb-shootdown")]
fn restore_hwp_then_ack(restore: impl FnOnce()) {
    restore();
    STOP_ACKS.fetch_add(1, Ordering::Release);
}

#[cfg(feature = "smp-tlb-shootdown")]
fn kexec_stop_handler() {
    if CRASH_STOP.load(Ordering::Acquire) {
        // A crash IPI cannot acquire scheduler, perf, PMU, allocator, or
        // device locks: it may have interrupted their owner.  Silence every
        // NMI/PMI producer through the prebuilt lock-free hardware path before
        // publishing the terminal acknowledgement.
        crash_quiesce_local();
        STOP_ACKS.fetch_add(1, Ordering::Release);
        axhal::asm::disable_irqs();
        loop {
            core::hint::spin_loop();
        }
    }
    #[cfg(all(feature = "pmu", target_os = "none"))]
    // AUX owns PT/DS/LBR state independently of the ordinary counter.  Its
    // baseline must be restored before this CPU publishes its terminal ACK.
    axhal::perf_precise_aux::quiesce_aux_for_kexec_local();
    #[cfg(feature = "perf-sampling")]
    crate::file::PerfSampleBackend::quiesce_current_cpu();
    restore_hwp_then_ack(|| {
        #[cfg(all(feature = "pmu", target_os = "none"))]
        {
            // Counter placement is quarantined before AUX; restore the core
            // and package-owner baselines before HWP's terminal ACK edge.
            let _ = axhal::pmu::restore_current_baseline();
            let _ = axhal::perf_uncore::restore_owner_baseline_current();
        }
        #[cfg(feature = "hwp-uclamp")]
        // Restore this CPU's firmware-owned request before acknowledging the
        // terminal stop; the initiator may hand execution to another kernel.
        let _ = axhal::hwp::restore_current_request();
        // CET is task-owned during ordinary execution.  A kexec stop is a
        // terminal handoff, so restore the per-CPU firmware snapshot before
        // this CPU acknowledges that its state is safe for the next kernel.
        axhal::cet::restore_current_boot_baseline_for_kexec();
    });
    axhal::asm::disable_irqs();
    loop {
        core::hint::spin_loop();
    }
}

fn crash_quiesce_local() {
    // Crash-stop cannot acquire the CET fleet baseline lock.  Clear every
    // CET control instead, which is the terminal safe-disabled state.
    axhal::asm::disable_user_cet_for_terminal_handoff();
    #[cfg(target_os = "none")]
    axhal::asm::crash_quiesce_debug_registers();
    #[cfg(all(feature = "pmu", target_os = "none"))]
    {
        axhal::perf_precise_aux::crash_quiesce_aux_current();
        axhal::pmu::crash_quiesce_current();
        axhal::perf_uncore::crash_quiesce_owner_current();
    }
}

#[cfg(feature = "smp-tlb-shootdown")]
fn stop_other_cpus() -> AxResult<()> {
    let cpu_num = axhal::cpu_num();
    if cpu_num <= 1 {
        return Ok(());
    }
    if !STOP_HANDLER_READY.load(Ordering::Acquire) {
        if CRASH_STOP.load(Ordering::Acquire)
            || !axhal::irq::register_ipi_reason(
                axhal::irq::IpiReason::KexecStop,
                kexec_stop_handler,
            )
        {
            return Err(AxError::BadState);
        }
        STOP_HANDLER_READY.store(true, Ordering::Release);
    }
    STOP_ACKS.store(0, Ordering::Release);
    let cpu = axhal::percpu::this_cpu_id();
    axhal::irq::send_ipi_reason(
        axhal::irq::IpiReason::KexecStop,
        axhal::irq::IpiTarget::AllExceptCurrent {
            cpu_id: cpu,
            cpu_num,
        },
    )
    .map_err(|_| AxError::BadState)?;
    for _ in 0..10_000_000 {
        if STOP_ACKS.load(Ordering::Acquire) == cpu_num - 1 {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(AxError::TimedOut)
}

fn boot_capable() -> AxResult<Arc<Cred>> {
    let cred = current().as_thread().current_cred();
    if !cred.user_ns().is_initial() || !cred.has_effective_capability_in_own_user_ns(CAP_SYS_BOOT) {
        return Err(AxError::OperationNotPermitted);
    }
    if KEXEC_LOAD_DISABLED.load(Ordering::Acquire) {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(cred)
}
fn range_end(start: usize, len: usize) -> AxResult<usize> {
    start.checked_add(len).ok_or(AxError::InvalidInput)
}
fn valid_flags(flags: u64) -> AxResult<bool> {
    // include/linux/kexec.h:
    //
    // 	/* List of defined/legal kexec flags */
    // 	#ifndef CONFIG_KEXEC_JUMP
    // 	#define KEXEC_FLAGS    (KEXEC_ON_CRASH | KEXEC_UPDATE_ELFCOREHDR | KEXEC_CRASH_HOTPLUG_SUPPORT)
    // 	#else
    // 	#define KEXEC_FLAGS    (KEXEC_ON_CRASH | KEXEC_PRESERVE_CONTEXT | KEXEC_UPDATE_ELFCOREHDR | \
    // 				KEXEC_CRASH_HOTPLUG_SUPPORT)
    // 	#endif
    //
    // and kernel/kexec.c `kexec_load_check()` compares the request against it:
    //
    // 	if ((flags & KEXEC_FLAGS) != (flags & ~KEXEC_ARCH_MASK))
    // 		return -EINVAL;
    if (flags & KEXEC_FLAGS) != (flags & !KEXEC_ARCH_MASK) {
        return Err(AxError::InvalidInput);
    }
    // The arch test is the next statement of `SYSCALL_DEFINE4(kexec_load)`,
    // with the same -EINVAL:
    //
    // 	if (((flags & KEXEC_ARCH_MASK) != KEXEC_ARCH) &&
    // 		((flags & KEXEC_ARCH_MASK) != KEXEC_ARCH_DEFAULT))
    // 		return -EINVAL;
    if !matches!(flags & KEXEC_ARCH_MASK, 0 | KEXEC_ARCH_X86_64) {
        return Err(AxError::InvalidInput);
    }
    // The image type depends only on KEXEC_ON_CRASH, matching
    // `kexec_load_check()`'s `int image_type = (flags & KEXEC_ON_CRASH) ? ...`.
    Ok(flags & KEXEC_ON_CRASH != 0)
}
pub fn sys_kexec_load<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    entry: usize,
    n: usize,
    source: *const KexecSegment,
    flags: u64,
) -> AxResult<isize> {
    let actor = boot_capable()?;
    // `SYSCALL_DEFINE4(kexec_load)` runs the `security_kernel_load_data()`
    // LSM hook before `kexec_load_check()` inspects the flags; the kind only
    // depends on the KEXEC_ON_CRASH bit, which needs no validation.
    authorize_kernel_load_data(
        &actor,
        if flags & KEXEC_ON_CRASH != 0 {
            KernelLoadKind::KexecCrashImage
        } else {
            KernelLoadKind::KexecImage
        },
        false,
    )?;
    let crash = valid_flags(flags)?;
    // `kexec_load_check()` caps the segment count after the flag and LSM
    // admission:
    //
    // 	/* Put an artificial cap on the number
    // 	 * of segments passed to kexec_load.
    // 	 */
    // 	if (nr_segments > KEXEC_SEGMENT_MAX)
    // 		return -EINVAL;
    if n > KEXEC_SEGMENT_MAX {
        return Err(AxError::InvalidInput);
    }
    let mut raw = Vec::new();
    raw.try_reserve_exact(n).map_err(|_| AxError::NoMemory)?;
    // This loop is `SYSCALL_DEFINE4(kexec_load)`'s `memdup_array_user()`, which
    // runs before `do_kexec_load()` takes the kexec lock.  A NULL `segments`
    // pointer is not a separate -EINVAL case: the copy faults, so Linux reports
    // -EFAULT for `nr_segments > 0` and never looks at the pointer for zero
    // segments (`memdup_array_user(NULL, 0, ...)` returns ZERO_SIZE_PTR).
    for index in 0..n {
        let address = (source as usize)
            .checked_add(
                index
                    .checked_mul(size_of::<KexecSegment>())
                    .ok_or(AxError::BadAddress)?,
            )
            .ok_or(AxError::BadAddress)?;
        raw.push(VmPtr::vm_read(address as *const KexecSegment, memory)
            .map_err(|_| AxError::BadAddress)?);
    }
    // `do_kexec_load()` opens with the serialization `kexec_load_permitted()`
    // does not cover:
    //
    // 	if (!kexec_trylock())
    // 		return -EBUSY;
    //
    // so -EBUSY is reported only once the request itself is well formed.
    let _load_transaction = KEXEC_LOAD_TRANSACTION.try_lock().ok_or(LinuxError::EBUSY)?;
    if n == 0 {
        if crash {
            publish_crash_image(None)?;
        } else {
            *NORMAL.try_lock().ok_or(LinuxError::EBUSY)? = None;
        }
        return Ok(0);
    }
    let mut destinations = Vec::new();
    destinations
        .try_reserve_exact(n)
        .map_err(|_| AxError::NoMemory)?;
    for &segment in &raw {
        validate_segment_address(segment)?;
        destinations.push((segment.mem, segment.memsz));
    }
    // Linux finishes the address loop for every segment before it reads any
    // `bufsz`, so the two passes are kept separate: a request that violates both
    // reports the address problem.
    for &segment in &raw {
        validate_segment_buffer(segment)?;
    }
    admit_destination_ranges(&destinations, axhal::mem::total_ram_size() / PAGE_SIZE)?;
    let mut image = Vec::new();
    image.try_reserve_exact(n).map_err(|_| AxError::NoMemory)?;
    for segment in raw {
        let pages = segment.memsz / PAGE_SIZE;
        let mut uninit = Vec::new();
        uninit
            .try_reserve_exact(segment.bufsz)
            .map_err(|_| AxError::NoMemory)?;
        uninit.resize_with(segment.bufsz, MaybeUninit::uninit);
        if crash {
            // An empty destination (memsz == 0) owns no page, so there is
            // nothing to take over from the allocator.  Linux reaches the same
            // place: `kimage_load_crash_segment()` copies a page at a time in a
            // `while (mbytes)` loop (kernel/kexec_core.c:887), so an empty
            // segment leaves the reserved window untouched.
            if pages != 0 {
                replace_pages_at(
                    phys_to_virt(segment.mem.into()).as_usize(),
                    pages,
                    PAGE_SIZE,
                )
                .map_err(|_| AxError::NoMemory)?;
            }
        }
        if segment.bufsz != 0
            && memory
                .read_bytes(segment.buf as usize, &mut uninit)
                .is_err()
        {
            if crash {
                global_allocator().dealloc_pages(
                    phys_to_virt(segment.mem.into()).as_usize(),
                    pages,
                    UsageKind::Kexec,
                );
            }
            return Err(AxError::BadAddress);
        }
        // SAFETY: either `bufsz` is zero or `read_bytes` succeeded and initialized every element;
        // `MaybeUninit<u8>` and `u8` have the same layout, so the allocation is unchanged.
        let bytes = unsafe { core::mem::transmute::<Vec<MaybeUninit<u8>>, Vec<u8>>(uninit) };
        image.push(ReservedSegment {
            paddr: segment.mem,
            pages,
            bytes,
            owns_pages: crash,
        });
    }
    let image = KexecImage {
        // `kimage_alloc_init()` stores the entry exactly as passed and does not
        // require it to land inside a destination segment:
        //
        // 	image->start = entry;
        //
        // (kernel/kexec.c:41).  There is no "entry outside destination" test in
        // Linux to mirror here; which entry points are usable is decided by the
        // image itself when it is booted, and `kexec_load` already requires
        // CAP_SYS_BOOT.
        entry,
        segments: image,
        boot_params: 0,
        startup_32: false,
    };
    if crash {
        let prepared = CrashKexecImage::prepare(image)?;
        publish_crash_image(Some(prepared))?;
    } else {
        *NORMAL.try_lock().ok_or(LinuxError::EBUSY)? = Some(image);
    }
    Ok(0)
}
/// `sanity_check_segment_list()`'s first loop (kernel/kexec_core.c:130-141), for
/// one `struct kexec_segment`:
///
/// ```c
/// 	mstart = image->segment[i].mem;
/// 	mend   = mstart + image->segment[i].memsz;
/// 	if (mstart > mend)
/// 		return -EADDRNOTAVAIL;
/// 	if ((mstart & ~PAGE_MASK) || (mend & ~PAGE_MASK))
/// 		return -EADDRNOTAVAIL;
/// 	if (mend >= KEXEC_DESTINATION_MEMORY_LIMIT)
/// 		return -EADDRNOTAVAIL;
/// ```
///
/// `mstart > mend` is unsigned wraparound, not a zero length: a segment whose
/// `memsz` is zero leaves `mend == mstart`, so it passes the wraparound,
/// alignment and limit tests and is *not* an address error.  Linux accepts such
/// a segment and it then contributes no pages (`kimage_load_normal_segment()`
/// loops `while (mbytes)`), so `admit_destination_ranges()` below must accept it
/// too rather than reporting the -EINVAL a zero length suggests.  The loop's own
/// comment leaves RAM validation to the caller, so no test here consults the
/// platform memory map either; `admit_destination_ranges()` is where TheKernel
/// adds that, and where the difference is declared.
fn validate_segment_address(segment: KexecSegment) -> AxResult<()> {
    let Some(end) = segment.mem.checked_add(segment.memsz) else {
        return Err(LinuxError::EADDRNOTAVAIL.into());
    };
    if segment.mem & (PAGE_SIZE - 1) != 0
        || end & (PAGE_SIZE - 1) != 0
        || end >= KEXEC_DESTINATION_MEMORY_LIMIT
    {
        return Err(LinuxError::EADDRNOTAVAIL.into());
    }
    Ok(())
}
/// `sanity_check_segment_list()`'s third loop (kernel/kexec_core.c:165-173),
/// for one `struct kexec_segment`:
///
/// ```c
/// 	/* Ensure our buffer sizes are strictly less than
/// 	 * our memory sizes.  This should always be the case,
/// 	 * and it is easier to check up front than to be surprised
/// 	 * later on.
/// 	 */
/// 	for (i = 0; i < nr_segments; i++) {
/// 		if (image->segment[i].bufsz > image->segment[i].memsz)
/// 			return -EINVAL;
/// 	}
/// ```
///
/// It is a separate pass because Linux runs the whole address loop before it:
/// a request whose first segment has `bufsz > memsz` and whose second segment
/// has an unaligned destination reports the *second* segment's -EADDRNOTAVAIL,
/// not the first segment's -EINVAL.
fn validate_segment_buffer(segment: KexecSegment) -> AxResult<()> {
    if segment.bufsz > segment.memsz {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}
fn admit_destination_ranges(ranges: &[(usize, usize)], total_pages: usize) -> AxResult<()> {
    admit_destination_ranges_in(
        ranges,
        total_pages,
        axhal::mem::total_ram_size(),
        axhal::kexec::boot_memory_regions(),
    )
}

fn admit_destination_ranges_in(
    ranges: &[(usize, usize)],
    total_pages: usize,
    total_bytes: usize,
    memory_regions: &[(usize, usize)],
) -> AxResult<()> {
    let mut checked = Vec::new();
    checked
        .try_reserve_exact(ranges.len())
        .map_err(|_| AxError::NoMemory)?;
    for &(start, bytes) in ranges {
        let Some(end) = start.checked_add(bytes) else {
            return Err(LinuxError::EADDRNOTAVAIL.into());
        };
        if start & (PAGE_SIZE - 1) != 0 || end & (PAGE_SIZE - 1) != 0 {
            return Err(LinuxError::EADDRNOTAVAIL.into());
        }
        if end >= KEXEC_DESTINATION_MEMORY_LIMIT {
            return Err(LinuxError::EADDRNOTAVAIL.into());
        }
        // A raw kexec segment is a physical-RAM destination, not merely a
        // numerically canonical address.  In particular, accepting an MMIO
        // aperture or a hole below total_ram_size() would let the terminal
        // copier issue arbitrary device writes after the normal kernel has
        // stopped.  The platform memory map is the authority here; it still
        // includes pages currently occupied by this kernel, which orderly
        // kexec is explicitly allowed to replace.
        //
        // This is stricter than Linux, deliberately and with a declared cost.
        // `sanity_check_segment_list()` bounds destinations only by
        // KEXEC_DESTINATION_MEMORY_LIMIT and says why:
        //
        // 	/*
        // 	 * Verify we have good destination addresses.  The caller is
        // 	 * responsible for making certain we don't attempt to load
        // 	 * the new image into invalid or reserved areas of RAM.  This
        // 	 * just verifies it is an address we can use.
        // 	 */
        //
        // so an address below the limit but outside a RAM region is accepted
        // there (for a *crash* image Linux does require the crash kernel's
        // reserved window, which TheKernel does not carve out).  TheKernel
        // answers -EINVAL for it, and that difference is declared in the
        // `kexec_load` contract cell.
        //
        // An empty range is *not* an out-of-RAM range: Linux accepts it
        // (`mend == mstart` passes the alignment, limit and overlap tests, and
        // the segment contributes no pages), so `bytes == 0` is deliberately
        // absent from the test below.  A caller may pass such a segment as a
        // placeholder, and rejecting it would be an -EINVAL Linux never
        // reports.
        let in_ram = end <= total_bytes
            && memory_regions
                .iter()
                .any(|&(base, length)| {
                    base.checked_add(length)
                        .is_some_and(|limit| base <= start && end <= limit)
                });
        if !in_ram {
            return Err(AxError::InvalidInput);
        }
        checked.push((start, end));
    }
    // 	/* Verify our destination addresses do not overlap.
    // 	 * If we alloed overlapping destination addresses
    // 	 * through very weird things can happen with no
    // 	 * easy explanation as one segment stops on another.
    // 	 */
    // 	for (i = 0; i < nr_segments; i++) {
    // 		unsigned long mstart, mend;
    // 		unsigned long j;
    //
    // 		mstart = image->segment[i].mem;
    // 		mend   = mstart + image->segment[i].memsz;
    // 		for (j = 0; j < i; j++) {
    // 			unsigned long pstart, pend;
    //
    // 			pstart = image->segment[j].mem;
    // 			pend   = pstart + image->segment[j].memsz;
    // 			/* Do the segments overlap ? */
    // 			if ((mend > pstart) && (mstart < pend))
    // 				return -EINVAL;
    // 		}
    // 	}
    //
    // This is a half-open intersection, so the answer does not depend on the
    // order of the request (`nr_segments` is capped at KEXEC_SEGMENT_MAX, so
    // the quadratic loop costs nothing).  Transcribing it literally also keeps
    // the zero-length destination admitted above admitted here: a segment with
    // `mend == mstart` intersects nothing, and a sort followed by a "does this
    // start precede the previous end" scan would report an overlap for two
    // segments that merely share a start.
    let mut covered = 0usize;
    for (index, &(start, end)) in checked.iter().enumerate() {
        for &(previous_start, previous_end) in &checked[..index] {
            if end > previous_start && start < previous_end {
                return Err(AxError::InvalidInput);
            }
        }
        covered = covered
            .checked_add((end - start) / PAGE_SIZE)
            .ok_or(AxError::NoMemory)?;
    }
    // 	/*
    // 	 * Verify that no more than half of memory will be consumed. If the
    // 	 * request from userspace is too large, a large amount of time will be
    // 	 * wasted allocating pages, which can cause a soft lockup.
    // 	 */
    // 	if (total_pages > nr_pages / 2)
    // 		return -EINVAL;
    if covered > total_pages / 2 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}
fn admit_reserved_segments(segments: &[ReservedSegment]) -> AxResult<()> {
    let mut ranges = Vec::new();
    ranges
        .try_reserve_exact(segments.len())
        .map_err(|_| AxError::NoMemory)?;
    for segment in segments {
        ranges.push((segment.paddr, segment.pages * PAGE_SIZE));
    }
    admit_destination_ranges(&ranges, axhal::mem::total_ram_size() / PAGE_SIZE)
}
fn read_fd_all(fd: i32) -> AxResult<Vec<u8>> {
    let file = get_typed_file::<File>(fd)?;
    let status = file.io_status_snapshot();
    file.check_io_status(status)?;
    let length = usize::try_from(file.stat()?.size).map_err(|_| AxError::NoMemory)?;
    if length == 0 || length > KEXEC_FILE_MAX_IMAGE {
        return Err(AxError::InvalidInput);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| AxError::NoMemory)?;
    // Capacity admission above makes this initialization allocation-free.
    bytes.resize(length, 0);
    file.with_read_credentials(|| {
        let mut output = axio::Cursor::new(bytes.as_mut_slice());
        let mut offset = 0u64;
        while output.position() < length as u64 {
            let read = file.read_at_with_status(status, &mut output, offset)?;
            if read == 0 {
                return Err(AxError::InvalidInput);
            }
            offset = offset
                .checked_add(read as u64)
                .ok_or(AxError::InvalidInput)?;
        }
        Ok(())
    })?;
    Ok(bytes)
}
fn le16(bytes: &[u8], offset: usize) -> AxResult<u16> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or(AxError::InvalidInput)?
            .try_into()
            .unwrap(),
    ))
}
fn le32(bytes: &[u8], offset: usize) -> AxResult<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or(AxError::InvalidInput)?
            .try_into()
            .unwrap(),
    ))
}
fn le64(bytes: &[u8], offset: usize) -> AxResult<u64> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or(AxError::InvalidInput)?
            .try_into()
            .unwrap(),
    ))
}
fn reserve_payload(paddr: usize, memsz: usize, content: &[u8]) -> AxResult<ReservedSegment> {
    if paddr & (PAGE_SIZE - 1) != 0 || content.len() > memsz {
        return Err(AxError::InvalidInput);
    }
    let bytes = memsz
        .checked_add(PAGE_SIZE - 1)
        .ok_or(AxError::InvalidInput)?
        & !(PAGE_SIZE - 1);
    // Heap ownership is complete before fixed pages are reserved.  There is
    // therefore no fallible operation after replace_pages_at succeeds.
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(content.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.extend_from_slice(content);
    TransitionImage::reserve_fixed(paddr, bytes, owned)
}

/// Reserve one low transition page outside every staged destination.  Unlike
/// payload placement this page is live before the terminal handoff, so it is
/// zeroed immediately rather than represented only by staged bytes.
fn reserve_transition_low_page(exclusions: &[(usize, usize)]) -> AxResult<ReservedSegment> {
    for &(start, length) in axhal::kexec::boot_memory_regions() {
        let end = start
            .checked_add(length)
            .ok_or(AxError::InvalidInput)?
            .min(MAX_32BIT_PADDR);
        let mut paddr = start
            .checked_add(PAGE_SIZE - 1)
            .ok_or(AxError::InvalidInput)?
            & !(PAGE_SIZE - 1);
        while paddr
            .checked_add(PAGE_SIZE)
            .is_some_and(|candidate| candidate <= end)
        {
            let overlaps = exclusions.iter().any(|&(excluded_start, excluded_end)| {
                paddr < excluded_end && excluded_start < paddr + PAGE_SIZE
            });
            if !overlaps && let Ok(page) = reserve_payload(paddr, PAGE_SIZE, &[]) {
                // SAFETY: `page` was just reserved at `paddr`, so its direct-map page is
                // exclusively ours.
                unsafe {
                    core::ptr::write_bytes(
                        phys_to_virt(page.paddr.into()).as_mut_ptr(),
                        0,
                        PAGE_SIZE,
                    )
                };
                return Ok(page);
            }
            paddr = paddr.checked_add(PAGE_SIZE).ok_or(AxError::NoMemory)?;
        }
    }
    Err(AxError::NoMemory)
}

/// Stages a fixed-destination segment without taking ownership of its final
/// pages.  Orderly kexec may target pages occupied by the running kernel;
/// they are overwritten only after all CPUs are stopped.  Crash images use
/// `reserve_payload` instead because their destinations must survive panic.
fn stage_payload(paddr: usize, memsz: usize, content: &[u8]) -> AxResult<ReservedSegment> {
    if paddr & (PAGE_SIZE - 1) != 0 || content.len() > memsz {
        return Err(AxError::InvalidInput);
    }
    let bytes = memsz
        .checked_add(PAGE_SIZE - 1)
        .ok_or(AxError::InvalidInput)?
        & !(PAGE_SIZE - 1);
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(content.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.extend_from_slice(content);
    Ok(ReservedSegment {
        paddr,
        pages: bytes / PAGE_SIZE,
        bytes: owned,
        owns_pages: false,
    })
}
/// Reserve a physically contiguous image range from the platform's usable RAM
/// map.  `replace_pages_at` is the authority: ranges occupied by this kernel,
/// firmware, or an earlier kexec admission simply fail and are skipped.
fn reserve_payload_below(
    content: &[u8],
    alignment: usize,
    preferred: Option<usize>,
    upper: usize,
) -> AxResult<ReservedSegment> {
    let bytes = content
        .len()
        .checked_add(PAGE_SIZE - 1)
        .ok_or(AxError::InvalidInput)?
        & !(PAGE_SIZE - 1);
    let alignment = alignment
        .max(PAGE_SIZE)
        .checked_next_power_of_two()
        .ok_or(AxError::InvalidInput)?;
    let try_at = |paddr: usize| -> Option<ReservedSegment> {
        if paddr.checked_add(bytes).is_none_or(|end| end > upper) {
            return None;
        }
        let mut segment = reserve_payload(paddr, bytes, content).ok()?;
        segment.bytes.truncate(content.len());
        Some(segment)
    };
    if let Some(paddr) = preferred.map(|p| p & !(alignment - 1))
        && let Some(segment) = try_at(paddr)
    {
        return Ok(segment);
    }
    for &(start, length) in axhal::kexec::boot_memory_regions() {
        let end = start
            .checked_add(length)
            .ok_or(AxError::InvalidInput)?
            .min(upper);
        let mut paddr = start
            .checked_add(alignment - 1)
            .ok_or(AxError::InvalidInput)?
            & !(alignment - 1);
        while paddr
            .checked_add(bytes)
            .is_some_and(|candidate| candidate <= end)
        {
            if let Some(segment) = try_at(paddr) {
                return Ok(segment);
            }
            paddr = paddr.checked_add(alignment).ok_or(AxError::NoMemory)?;
        }
    }
    Err(AxError::NoMemory)
}
fn reserve_payload_below_sized(
    content: &[u8],
    memsz: usize,
    alignment: usize,
    preferred: Option<usize>,
    upper: usize,
) -> AxResult<ReservedSegment> {
    let bytes = memsz
        .checked_add(PAGE_SIZE - 1)
        .ok_or(AxError::InvalidInput)?
        & !(PAGE_SIZE - 1);
    let alignment = alignment
        .max(PAGE_SIZE)
        .checked_next_power_of_two()
        .ok_or(AxError::InvalidInput)?;
    let try_at = |paddr: usize| {
        (paddr.checked_add(bytes).is_some_and(|end| end <= upper))
            .then(|| reserve_payload(paddr, bytes, content).ok())
            .flatten()
    };
    if let Some(paddr) = preferred.map(|p| p & !(alignment - 1))
        && let Some(segment) = try_at(paddr)
    {
        return Ok(segment);
    }
    for &(start, length) in axhal::kexec::boot_memory_regions() {
        let end = start
            .checked_add(length)
            .ok_or(AxError::InvalidInput)?
            .min(upper);
        let mut paddr = start
            .checked_add(alignment - 1)
            .ok_or(AxError::InvalidInput)?
            & !(alignment - 1);
        while paddr
            .checked_add(bytes)
            .is_some_and(|candidate| candidate <= end)
        {
            if let Some(segment) = try_at(paddr) {
                return Ok(segment);
            }
            paddr = paddr.checked_add(alignment).ok_or(AxError::NoMemory)?;
        }
    }
    Err(AxError::NoMemory)
}

/// Select an orderly-kexec destination without taking ownership of pages used
/// by the running kernel.  Candidates are bounded by firmware's usable RAM
/// map and by already selected image segments; the actual overwrite happens
/// only after the CPU rendezvous.
fn stage_payload_below_sized(
    content: &[u8],
    memsz: usize,
    alignment: usize,
    preferred: Option<usize>,
    upper: usize,
    selected: &[ReservedSegment],
) -> AxResult<ReservedSegment> {
    let bytes = memsz
        .checked_add(PAGE_SIZE - 1)
        .ok_or(AxError::InvalidInput)?
        & !(PAGE_SIZE - 1);
    if bytes == 0 || content.len() > memsz {
        return Err(AxError::InvalidInput);
    }
    let alignment = alignment
        .max(PAGE_SIZE)
        .checked_next_power_of_two()
        .ok_or(AxError::InvalidInput)?;
    let available = |paddr: usize, require_usable_map: bool| {
        let Some(end) = paddr.checked_add(bytes) else {
            return false;
        };
        if paddr & (alignment - 1) != 0
            || end > upper
            || selected.iter().any(|segment| {
                let selected_end = segment.paddr + segment.pages * PAGE_SIZE;
                paddr < selected_end && segment.paddr < end
            })
        {
            return false;
        }
        !require_usable_map
            || axhal::kexec::boot_memory_regions()
                .iter()
                .any(|&(start, length)| {
                    start
                        .checked_add(length)
                        .is_some_and(|region_end| start <= paddr && end <= region_end)
                })
    };
    if let Some(paddr) = preferred.map(|p| p & !(alignment - 1))
        // Conventional low boot slots may be firmware-reserved and therefore
        // absent from the usable map, but are explicitly supplied by the x86
        // boot protocol.  Still require real physical RAM and no image overlap.
        && paddr
            .checked_add(bytes)
            .is_some_and(|end| end <= axhal::mem::total_ram_size())
        && available(paddr, false)
    {
        return stage_payload(paddr, bytes, content);
    }
    for &(start, length) in axhal::kexec::boot_memory_regions() {
        let end = start
            .checked_add(length)
            .ok_or(AxError::InvalidInput)?
            .min(upper);
        let mut paddr = start
            .checked_add(alignment - 1)
            .ok_or(AxError::InvalidInput)?
            & !(alignment - 1);
        while paddr
            .checked_add(bytes)
            .is_some_and(|candidate| candidate <= end)
        {
            if available(paddr, true) {
                return stage_payload(paddr, bytes, content);
            }
            paddr = paddr.checked_add(alignment).ok_or(AxError::NoMemory)?;
        }
    }
    Err(AxError::NoMemory)
}

fn stage_payload_below(
    content: &[u8],
    alignment: usize,
    preferred: Option<usize>,
    upper: usize,
    selected: &[ReservedSegment],
) -> AxResult<ReservedSegment> {
    stage_payload_below_sized(
        content,
        content.len(),
        alignment,
        preferred,
        upper,
        selected,
    )
}
fn parse_elf64(image: &[u8], reserve_destinations: bool) -> AxResult<KexecImage> {
    probe_elf64(image)?;
    let entry = usize::try_from(le64(image, 24)?).map_err(|_| AxError::InvalidInput)?;
    let phoff = usize::try_from(le64(image, 32)?).map_err(|_| AxError::InvalidInput)?;
    let phentsz = usize::from(le16(image, 54)?);
    let phnum = usize::from(le16(image, 56)?);
    let mut segments = Vec::new();
    // `phnum` bounds every successful PT_LOAD insertion.  Admit the owner
    // slots before any payload reservation so dropping `segments` releases
    // every page on any later parse or allocation failure.
    segments
        .try_reserve_exact(phnum)
        .map_err(|_| AxError::NoMemory)?;
    for n in 0..phnum {
        let off = phoff
            .checked_add(n.checked_mul(phentsz).ok_or(AxError::InvalidInput)?)
            .ok_or(AxError::InvalidInput)?;
        if le32(image, off)? != 1 {
            continue;
        }
        let fileoff = usize::try_from(le64(image, off + 8)?).map_err(|_| AxError::InvalidInput)?;
        let paddr = usize::try_from(le64(image, off + 24)?).map_err(|_| AxError::InvalidInput)?;
        let filesz = usize::try_from(le64(image, off + 32)?).map_err(|_| AxError::InvalidInput)?;
        let memsz = usize::try_from(le64(image, off + 40)?).map_err(|_| AxError::InvalidInput)?;
        let end = fileoff.checked_add(filesz).ok_or(AxError::InvalidInput)?;
        let content = image.get(fileoff..end).ok_or(AxError::InvalidInput)?;
        segments.push(if reserve_destinations {
            reserve_payload(paddr, memsz, content)?
        } else {
            stage_payload(paddr, memsz, content)?
        });
    }
    Ok(KexecImage {
        entry,
        segments,
        boot_params: 0,
        startup_32: false,
    })
}
fn probe_elf64(image: &[u8]) -> AxResult<()> {
    if image.get(..4) != Some(b"\x7fELF")
        || image.get(4) != Some(&2)
        || image.get(5) != Some(&1)
        || le16(image, 16)? != 2
        || le16(image, 18)? != 62
    {
        return Err(AxError::InvalidInput);
    }
    let entry = usize::try_from(le64(image, 24)?).map_err(|_| AxError::InvalidInput)?;
    let phoff = usize::try_from(le64(image, 32)?).map_err(|_| AxError::InvalidInput)?;
    let phentsz = usize::from(le16(image, 54)?);
    let phnum = usize::from(le16(image, 56)?);
    if phentsz < 56 || phnum == 0 || phnum > KEXEC_SEGMENT_MAX {
        return Err(AxError::InvalidInput);
    }
    let mut ranges = Vec::new();
    ranges
        .try_reserve_exact(phnum)
        .map_err(|_| AxError::NoMemory)?;
    let mut entry_loaded = false;
    for n in 0..phnum {
        let off = phoff
            .checked_add(n.checked_mul(phentsz).ok_or(AxError::InvalidInput)?)
            .ok_or(AxError::InvalidInput)?;
        if le32(image, off)? != 1 {
            continue;
        }
        let fileoff = usize::try_from(le64(image, off + 8)?).map_err(|_| AxError::InvalidInput)?;
        let paddr = usize::try_from(le64(image, off + 24)?).map_err(|_| AxError::InvalidInput)?;
        let filesz = usize::try_from(le64(image, off + 32)?).map_err(|_| AxError::InvalidInput)?;
        let memsz = usize::try_from(le64(image, off + 40)?).map_err(|_| AxError::InvalidInput)?;
        let fileend = fileoff.checked_add(filesz).ok_or(AxError::InvalidInput)?;
        image.get(fileoff..fileend).ok_or(AxError::InvalidInput)?;
        if paddr & (PAGE_SIZE - 1) != 0 || filesz > memsz {
            return Err(AxError::InvalidInput);
        }
        let bytes = memsz
            .checked_add(PAGE_SIZE - 1)
            .ok_or(AxError::InvalidInput)?
            & !(PAGE_SIZE - 1);
        if bytes == 0 || range_end(paddr, bytes)? & (PAGE_SIZE - 1) != 0 {
            return Err(AxError::InvalidInput);
        }
        entry_loaded |= paddr
            .checked_add(bytes)
            .is_some_and(|end| paddr <= entry && entry < end);
        ranges.push((paddr, bytes));
    }
    if ranges.is_empty() || !entry_loaded {
        return Err(AxError::InvalidInput);
    }
    admit_destination_ranges(&ranges, axhal::mem::total_ram_size() / PAGE_SIZE)
}
fn bzimage_boot_params(image: &[u8], cmdline_len: usize) -> AxResult<Vec<u8>> {
    // setup_header is at 0x1f1; protocol 2.12 introduced xloadflags needed by
    // an x86_64 handoff.  The protected payload begins after setup_sects.
    if image.len() < SETUP_HEADER_END
        || image.get(0x202..0x206) != Some(b"HdrS")
        || le16(image, 0x206)? < 0x020c
        || le16(image, BOOT_FLAG_OFFSET)? != 0xaa55
        || image[LOADFLAGS_OFFSET] & LOADED_HIGH == 0
        || le16(image, XLOADFLAGS_OFFSET)? & XLF_KERNEL_64 == 0
    {
        return Err(AxError::InvalidInput);
    }
    let mut params = Vec::new();
    params
        .try_reserve_exact(PAGE_SIZE)
        .map_err(|_| AxError::NoMemory)?;
    // Capacity admission above makes this initialization allocation-free.
    params.resize(PAGE_SIZE, 0);
    // boot_params is a freshly initialized zero page.  Only setup_header is
    // supplied by the bzImage; all bootloader-owned fields are set below.
    params[SETUP_HEADER_OFFSET..SETUP_HEADER_END]
        .copy_from_slice(&image[SETUP_HEADER_OFFSET..SETUP_HEADER_END]);
    params[TYPE_OF_LOADER_OFFSET] = 0xff;
    params[CMDLINE_PTR_OFFSET..CMDLINE_PTR_OFFSET + 4].fill(0);
    params[RAMDISK_IMAGE_OFFSET..RAMDISK_IMAGE_OFFSET + 4].fill(0);
    params[RAMDISK_SIZE_OFFSET..RAMDISK_SIZE_OFFSET + 4].fill(0);
    if cmdline_len.saturating_sub(1)
        > usize::try_from(le32(image, CMDLINE_SIZE_OFFSET)?).map_err(|_| AxError::InvalidInput)?
    {
        return Err(AxError::InvalidInput);
    }
    Ok(params)
}
fn parse_bzimage(
    image: &[u8],
    cmdline: &[u8],
    initrd: Option<&[u8]>,
    reserve_destinations: bool,
) -> AxResult<KexecImage> {
    probe_bzimage(image)?;
    let mut params = bzimage_boot_params(image, cmdline.len())?;
    let setup = usize::from(image[SETUP_HEADER_OFFSET].max(4))
        .checked_add(1)
        .ok_or(AxError::InvalidInput)?
        * 512;
    let payload = image.get(setup..).ok_or(AxError::InvalidInput)?;
    let kernel_alignment =
        usize::try_from(le32(image, 0x230)?).map_err(|_| AxError::InvalidInput)?;
    let relocatable = image[0x234] != 0;
    let preferred = usize::try_from(le64(image, 0x258)?).ok();
    // At most boot params, kernel, cmdline, RSDP, and initrd are retained.
    // Reserve all ownership slots before the first physical reservation.
    let mut segments = Vec::new();
    segments
        .try_reserve_exact(5)
        .map_err(|_| AxError::NoMemory)?;
    let boot_params = if reserve_destinations {
        reserve_payload_below(
            &params,
            PAGE_SIZE,
            Some(LINUX_BOOT_PARAMS_PADDR),
            MAX_32BIT_PADDR,
        )?
    } else {
        stage_payload_below(
            &params,
            PAGE_SIZE,
            Some(LINUX_BOOT_PARAMS_PADDR),
            MAX_32BIT_PADDR,
            &segments,
        )?
    };
    segments.push(boot_params);
    let preferred = preferred.unwrap_or(0x100000);
    let init_size = usize::try_from(le32(image, 0x260)?).map_err(|_| AxError::InvalidInput)?;
    let image_size = payload.len().max(init_size);
    let kernel = if relocatable && reserve_destinations {
        reserve_payload_below_sized(
            payload,
            image_size,
            kernel_alignment,
            Some(preferred),
            MAX_32BIT_PADDR,
        )?
    } else if relocatable {
        stage_payload_below_sized(
            payload,
            image_size,
            kernel_alignment,
            Some(preferred),
            MAX_32BIT_PADDR,
            &segments,
        )?
    } else {
        let alignment = kernel_alignment
            .max(PAGE_SIZE)
            .checked_next_power_of_two()
            .ok_or(AxError::InvalidInput)?;
        if preferred & (alignment - 1) != 0 {
            return Err(AxError::InvalidInput);
        }
        if reserve_destinations {
            reserve_payload(preferred, image_size, payload)?
        } else {
            let staged = stage_payload(preferred, image_size, payload)?;
            let end = preferred
                .checked_add(staged.pages * PAGE_SIZE)
                .ok_or(AxError::InvalidInput)?;
            if end > axhal::mem::total_ram_size()
                || segments.iter().any(|segment| {
                    let selected_end = segment.paddr + segment.pages * PAGE_SIZE;
                    preferred < selected_end && segment.paddr < end
                })
            {
                return Err(AxError::InvalidInput);
            }
            staged
        }
    };
    let kernel_paddr = kernel.paddr;
    segments.push(kernel);
    if !cmdline.is_empty() {
        let command = if reserve_destinations {
            reserve_payload_below(
                cmdline,
                PAGE_SIZE,
                Some(LINUX_CMDLINE_PADDR),
                MAX_32BIT_PADDR,
            )?
        } else {
            stage_payload_below(
                cmdline,
                PAGE_SIZE,
                Some(LINUX_CMDLINE_PADDR),
                MAX_32BIT_PADDR,
                &segments,
            )?
        };
        params[CMDLINE_PTR_OFFSET..CMDLINE_PTR_OFFSET + 4]
            .copy_from_slice(&(command.paddr as u32).to_le_bytes());
        segments.push(command);
    }
    // Linux's boot_params contains a 128-entry E820 table at 0x2d0. The
    // platform has already copied Multiboot's usable ranges, so this never
    // borrows bootloader memory during the terminal transition.
    let e820 = axhal::kexec::boot_memory_regions();
    let e820_count = e820.len().min(128);
    params[0x1e8] = e820_count as u8;
    for (index, &(start, length)) in e820[..e820_count].iter().enumerate() {
        let offset = 0x2d0 + index * 20;
        params[offset..offset + 8].copy_from_slice(&(start as u64).to_le_bytes());
        params[offset + 8..offset + 16].copy_from_slice(&(length as u64).to_le_bytes());
        params[offset + 16..offset + 20].copy_from_slice(&1u32.to_le_bytes());
    }
    if let Some(rsdp) = axhal::kexec::boot_rsdp() {
        let rsdp = if reserve_destinations {
            reserve_payload_below(rsdp, PAGE_SIZE, Some(LINUX_RSDP_PADDR), MAX_32BIT_PADDR)?
        } else {
            stage_payload_below(
                rsdp,
                PAGE_SIZE,
                Some(LINUX_RSDP_PADDR),
                MAX_32BIT_PADDR,
                &segments,
            )?
        };
        params[0x70..0x78].copy_from_slice(&(rsdp.paddr as u64).to_le_bytes());
        segments.push(rsdp);
    }
    if let Some(initrd) = initrd {
        // Keep initrd below 4GiB and contiguous; the allocator supplies a
        // direct-map page range whose physical address is recorded below.
        let initrd_max =
            usize::try_from(le32(&params, 0x22c)?).map_err(|_| AxError::InvalidInput)?;
        let s = if reserve_destinations {
            reserve_payload_below(initrd, PAGE_SIZE, None, initrd_max)?
        } else {
            stage_payload_below(initrd, PAGE_SIZE, None, initrd_max, &segments)?
        };
        let paddr = s.paddr;
        segments.push(s);
        params[0x218..0x21c].copy_from_slice(&(paddr as u32).to_le_bytes());
        params[0x21c..0x220].copy_from_slice(&(initrd.len() as u32).to_le_bytes());
    }
    params[CODE32_START_OFFSET..CODE32_START_OFFSET + 4]
        .copy_from_slice(&(kernel_paddr as u32).to_le_bytes());
    segments[0].bytes[..PAGE_SIZE].copy_from_slice(&params);
    let boot_params_paddr = segments[0].paddr;
    Ok(KexecImage {
        entry: kernel_paddr,
        segments,
        boot_params: boot_params_paddr,
        startup_32: true,
    })
}
fn probe_bzimage(image: &[u8]) -> AxResult<()> {
    bzimage_boot_params(image, 0)?;
    let setup = usize::from(image[SETUP_HEADER_OFFSET].max(4))
        .checked_add(1)
        .ok_or(AxError::InvalidInput)?
        * 512;
    image.get(setup..).ok_or(AxError::InvalidInput)?;
    let alignment = usize::try_from(le32(image, 0x230)?).map_err(|_| AxError::InvalidInput)?;
    let relocatable = image[0x234] != 0;
    let preferred = usize::try_from(le64(image, 0x258)?).map_err(|_| AxError::InvalidInput)?;
    if !relocatable {
        let alignment = alignment
            .max(PAGE_SIZE)
            .checked_next_power_of_two()
            .ok_or(AxError::InvalidInput)?;
        if preferred & (alignment - 1) != 0 {
            return Err(AxError::InvalidInput);
        }
    }
    le32(image, 0x260)?;
    Ok(())
}
fn probe_kernel_image(image: &[u8]) -> AxResult<()> {
    if image.get(..4) == Some(b"\x7fELF") {
        probe_elf64(image)
    } else {
        probe_bzimage(image)
    }
}
pub fn sys_kexec_file_load<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    kernel: i32,
    initrd: i32,
    cmdline_len: usize,
    cmdline: *const i8,
    flags: u64,
) -> AxResult<isize> {
    let actor = boot_capable()?;
    let _load_transaction = KEXEC_LOAD_TRANSACTION.try_lock().ok_or(LinuxError::EBUSY)?;
    if flags
        & !(KEXEC_FILE_UNLOAD
            | KEXEC_FILE_ON_CRASH
            | KEXEC_FILE_NO_INITRAMFS
            | KEXEC_FILE_DEBUG
            | KEXEC_FILE_NO_CMA
            | KEXEC_FILE_FORCE_DTB)
        != 0
    {
        return Err(AxError::InvalidInput);
    }
    // The x86_64 product has no flattened-device-tree boot contract.  Treat
    // FORCE_DTB as a recognized, unavailable architecture feature rather
    // than conflating it with an unknown flag.
    if flags & KEXEC_FILE_FORCE_DTB != 0 {
        return Err(AxError::OperationNotSupported);
    }
    let crash = flags & KEXEC_FILE_ON_CRASH != 0;
    authorize_kernel_load_data(
        &actor,
        if crash {
            KernelLoadKind::KexecCrashImage
        } else {
            KernelLoadKind::KexecImage
        },
        true,
    )?;
    if flags & KEXEC_FILE_UNLOAD != 0 {
        if crash {
            publish_crash_image(None)?;
        } else {
            *NORMAL.try_lock().ok_or(LinuxError::EBUSY)? = None;
        }
        return Ok(0);
    }
    if cmdline_len != 0 && cmdline.is_null() {
        return Err(AxError::InvalidInput);
    }
    // Probe the kernel without reserving destination pages before initrd fd
    // access. This keeps malformed kernel errors ahead of initrd errors.
    let kernel_image = read_fd_all(kernel)?;
    probe_kernel_image(&kernel_image)?;
    let initrd_image = if flags & KEXEC_FILE_NO_INITRAMFS == 0 {
        Some(read_fd_all(initrd)?)
    } else {
        None
    };
    // Linux's cmdline_len includes the terminating NUL.  Preserve that exact
    // buffer rather than manufacturing a second terminator.
    let command_len = cmdline_len;
    let mut command_uninit = Vec::new();
    command_uninit
        .try_reserve_exact(command_len)
        .map_err(|_| AxError::NoMemory)?;
    command_uninit.resize_with(command_len, MaybeUninit::uninit);
    if cmdline_len != 0 {
        memory
            .read_bytes(cmdline as usize, &mut command_uninit[..cmdline_len])
            .map_err(|_| AxError::BadAddress)?;
    }
    // SAFETY: `command_len == cmdline_len`, so the buffer is either empty or was completely
    // initialized by `read_bytes`; `MaybeUninit<u8>` and `u8` have the same layout.
    let command = unsafe { core::mem::transmute::<Vec<MaybeUninit<u8>>, Vec<u8>>(command_uninit) };
    if !command.is_empty() && command.last() != Some(&0) {
        return Err(AxError::InvalidInput);
    }
    let parsed = if kernel_image.get(..4) == Some(b"\x7fELF") {
        parse_elf64(&kernel_image, crash)?
    } else {
        parse_bzimage(&kernel_image, &command, initrd_image.as_deref(), crash)?
    };
    admit_reserved_segments(&parsed.segments)?;
    if crash {
        let prepared = CrashKexecImage::prepare(parsed)?;
        publish_crash_image(Some(prepared))?;
    } else {
        *NORMAL.try_lock().ok_or(LinuxError::EBUSY)? = Some(parsed);
    }
    Ok(0)
}

#[cfg(target_os = "none")]
fn prepare_transition_trampoline(image: &KexecImage, transition: &TransitionImage) -> AxResult<()> {
    if !image.startup_32 {
        return Ok(());
    }
    let stub = axhal::kexec::transition32_blob();
    if stub.is_empty() || stub.len() > PAGE_SIZE {
        return Err(AxError::BadState);
    }
    // SAFETY: the stub fits in one page (checked above), the trampoline page was reserved for this
    // transition, and a static blob cannot overlap it.
    unsafe {
        core::ptr::copy_nonoverlapping(
            stub.as_ptr(),
            phys_to_virt(transition.trampoline.paddr.into()).as_mut_ptr(),
            stub.len(),
        );
    }
    Ok(())
}

#[cfg(target_os = "none")]
fn terminal_handoff<G>(
    image: KexecImage,
    transition: TransitionImage,
    loaded: G,
    crash: bool,
) -> ! {
    // Everything on this path was reserved while loading the image.  It is
    // therefore safe for both orderly reboot and a panic with a failed heap.
    axhal::asm::disable_irqs();
    #[cfg(feature = "smp-tlb-shootdown")]
    CRASH_STOP.store(crash, Ordering::Release);
    if crash {
        crash_quiesce_local();
    }
    #[cfg(feature = "smp-tlb-shootdown")]
    if stop_other_cpus().is_err() {
        axhal::power::system_off();
    }
    if !crash {
        #[cfg(feature = "perf-sampling")]
        crate::file::PerfSampleBackend::quiesce_current_cpu();
        #[cfg(feature = "pmu")]
        axhal::perf_precise_aux::quiesce_aux_for_kexec_local();
        #[cfg(feature = "pmu")]
        {
            let _ = axhal::pmu::restore_current_baseline();
            let _ = axhal::perf_uncore::restore_owner_baseline_current();
        }
        #[cfg(feature = "hwp-uclamp")]
        let _ = axhal::hwp::restore_current_request();
        axhal::cet::restore_current_boot_baseline_for_kexec();
    }
    axhal::kexec::fence_pci_bus_mastering();
    let cr3 = transition.cr3;
    let stack_top = transition.stack.paddr + BOOT_STACK_SIZE;
    let copier = transition.copier.paddr;
    let control = transition.control.paddr;
    core::mem::forget(image);
    core::mem::forget(transition);
    // Retain the publication lock across the non-returning jump so no loader
    // can reserve or rewrite destination pages during the terminal copies.
    core::mem::forget(loaded);
    // SAFETY: `transition` built the page tables, stack, copy stub and control block, and they
    // were leaked above so they stay valid; interrupts are off, other CPUs are stopped and PCI bus
    // mastering is fenced, as `copy_transition` requires.
    unsafe { axhal::kexec::copy_transition(cr3, stack_top, copier, control) }
}

/// Completes orderly kexec preparation and transfers control.
pub(crate) fn execute_loaded() -> AxResult<isize> {
    let mut loaded = NORMAL.try_lock().ok_or(LinuxError::EBUSY)?;
    let image = loaded.take().ok_or(AxError::InvalidInput)?;
    #[cfg(not(target_os = "none"))]
    {
        *loaded = Some(image);
        Err(LinuxError::EOPNOTSUPP.into())
    }
    #[cfg(target_os = "none")]
    {
        let transition = match TransitionImage::new(&image) {
            Ok(transition) => transition,
            Err(error) => {
                *loaded = Some(image);
                return Err(error);
            }
        };
        if let Err(error) = prepare_transition_trampoline(&image, &transition) {
            *loaded = Some(image);
            return Err(error);
        }
        terminal_handoff(image, transition, loaded, false)
    }
}

/// Attempts the preallocated crash image without allocating or blocking.
pub(crate) fn execute_crash_loaded() -> AxResult<isize> {
    let raw = CRASH.swap(ptr::null_mut(), Ordering::AcqRel);
    if raw.is_null() {
        return Err(AxError::InvalidInput);
    }
    #[cfg(not(target_os = "none"))]
    {
        if CRASH
            .compare_exchange(ptr::null_mut(), raw, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            unsafe { drop(Box::from_raw(raw)) };
        }
        Err(LinuxError::EOPNOTSUPP.into())
    }
    #[cfg(target_os = "none")]
    {
        // Move the prebuilt value without touching the allocator.  The tiny
        // Box allocation is intentionally leaked because this path never
        // returns and the failed kernel's heap may be corrupt.
        // SAFETY: `raw` came from `Box::into_raw` and the swap made it exclusively ours; it is
        // read once and the allocation is deliberately leaked.
        let prepared = unsafe { raw.read() };
        terminal_handoff(prepared.image, prepared.transition, raw, true)
    }
}

fn crash_kexec_panic_hook() {
    let _ = execute_crash_loaded();
}

pub(crate) fn init_crash_kexec_hook() {
    #[cfg(feature = "smp-tlb-shootdown")]
    {
        if !axhal::irq::register_ipi_reason(axhal::irq::IpiReason::KexecStop, kexec_stop_handler) {
            if axhal::cpu_num() <= 1 {
                axruntime::register_panic_crash_hook(crash_kexec_panic_hook);
            }
            return;
        }
        STOP_HANDLER_READY.store(true, Ordering::Release);
    }
    axruntime::register_panic_crash_hook(crash_kexec_panic_hook);
}

pub(crate) fn normal_image_loaded() -> bool {
    NORMAL.lock().is_some()
}

pub(crate) fn crash_image_loaded() -> bool {
    !CRASH.load(Ordering::Acquire).is_null()
}

pub(crate) fn kexec_load_disabled() -> bool {
    KEXEC_LOAD_DISABLED.load(Ordering::Acquire)
}

pub(crate) fn disable_kexec_load() -> AxResult<()> {
    let actor = current().as_thread().current_cred();
    if !actor.user_ns().is_initial() || !actor.has_effective_capability_in_own_user_ns(CAP_SYS_BOOT)
    {
        return Err(AxError::OperationNotPermitted);
    }
    KEXEC_LOAD_DISABLED.store(true, Ordering::Release);
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use super::*;

    #[cfg(feature = "smp-tlb-shootdown")]
    #[test]
    fn kexec_stop_acknowledges_only_after_cpu_local_restore() {
        STOP_ACKS.store(0, Ordering::Release);
        let mut sequence = [0_u8; 2];
        let mut next = 0;
        restore_hwp_then_ack(|| {
            sequence[next] = 1;
            next += 1;
        });
        sequence[next] = STOP_ACKS.load(Ordering::Acquire) as u8;
        assert_eq!(sequence, [1, 1]);
    }

    fn bzimage_header() -> Vec<u8> {
        let mut image = vec![0xa5; SETUP_HEADER_END];
        image[0x202..0x206].copy_from_slice(b"HdrS");
        image[0x206..0x208].copy_from_slice(&0x020cu16.to_le_bytes());
        image[BOOT_FLAG_OFFSET..BOOT_FLAG_OFFSET + 2].copy_from_slice(&0xaa55u16.to_le_bytes());
        image[LOADFLAGS_OFFSET] = LOADED_HIGH;
        image[XLOADFLAGS_OFFSET..XLOADFLAGS_OFFSET + 2]
            .copy_from_slice(&XLF_KERNEL_64.to_le_bytes());
        image[CMDLINE_SIZE_OFFSET..CMDLINE_SIZE_OFFSET + 4].copy_from_slice(&16u32.to_le_bytes());
        image
    }

    #[test]
    fn bzimage_params_copy_only_setup_header() {
        let image = bzimage_header();
        let params = bzimage_boot_params(&image, 16).unwrap();
        assert!(params[..SETUP_HEADER_OFFSET].iter().all(|&byte| byte == 0));
        for offset in SETUP_HEADER_OFFSET..SETUP_HEADER_END {
            if offset == TYPE_OF_LOADER_OFFSET
                || (CMDLINE_PTR_OFFSET..CMDLINE_PTR_OFFSET + 4).contains(&offset)
                || (RAMDISK_IMAGE_OFFSET..RAMDISK_IMAGE_OFFSET + 4).contains(&offset)
                || (RAMDISK_SIZE_OFFSET..RAMDISK_SIZE_OFFSET + 4).contains(&offset)
            {
                continue;
            }
            assert_eq!(params[offset], image[offset]);
        }
        assert_eq!(params[TYPE_OF_LOADER_OFFSET], 0xff);
        assert!(
            params[CMDLINE_PTR_OFFSET..CMDLINE_PTR_OFFSET + 4]
                .iter()
                .all(|&byte| byte == 0)
        );
        assert!(
            params[RAMDISK_IMAGE_OFFSET..RAMDISK_SIZE_OFFSET + 4]
                .iter()
                .all(|&byte| byte == 0)
        );
        assert!(params[SETUP_HEADER_END..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn bzimage_requires_64_bit_boot_contract_and_cmdline_capacity() {
        let mut image = bzimage_header();
        image[LOADFLAGS_OFFSET] = 0;
        assert!(bzimage_boot_params(&image, 0).is_err());
        let image = bzimage_header();
        assert!(bzimage_boot_params(&image, 17).is_ok());
        assert!(bzimage_boot_params(&image, 18).is_err());
    }

    #[test]
    fn destination_limit_rejects_overlaps_and_allows_adjacent_pages() {
        let memory = [(0, 4 * PAGE_SIZE)];
        assert!(
            admit_destination_ranges_in(
                &[(0, PAGE_SIZE), (PAGE_SIZE, PAGE_SIZE)], 4, 4 * PAGE_SIZE, &memory,
            ).is_ok()
        );
        // kernel/kexec_core.c `sanity_check_segment_list()` runs three loops:
        // every address is checked for wraparound, page alignment and the
        // KEXEC_DESTINATION_MEMORY_LIMIT first (-EADDRNOTAVAIL), and only then
        // are overlaps (-EINVAL) and the half-of-memory cap (-EINVAL) checked.
        // An unaligned *start* therefore reports -EADDRNOTAVAIL even when the
        // same pair would also overlap:
        //
        // 	for (i = 0; i < nr_segments; i++) {
        // 		mstart = image->segment[i].mem;
        // 		mend   = mstart + image->segment[i].memsz;
        // 		if (mstart > mend)
        // 			return -EADDRNOTAVAIL;
        // 		if ((mstart & ~PAGE_MASK) || (mend & ~PAGE_MASK))
        // 			return -EADDRNOTAVAIL;
        // 		if (mend >= KEXEC_DESTINATION_MEMORY_LIMIT)
        // 			return -EADDRNOTAVAIL;
        // 	}
        assert_eq!(
            admit_destination_ranges_in(
                &[(0, PAGE_SIZE), (PAGE_SIZE / 2, PAGE_SIZE)], 4, 4 * PAGE_SIZE, &memory,
            ),
            Err(AxError::from(LinuxError::EADDRNOTAVAIL))
        );
        // Page-aligned overlapping destinations reach the overlap loop and are
        // -EINVAL there:
        //
        // 	for (i = 0; i < nr_segments; i++) {
        // 		...
        // 		for (j = 0; j < i; j++) {
        // 			...
        // 			if ((mend > pstart) && (mstart < pend))
        // 				return -EINVAL;
        // 		}
        // 	}
        assert_eq!(
            admit_destination_ranges_in(
                &[(0, 2 * PAGE_SIZE), (PAGE_SIZE, PAGE_SIZE)], 4, 4 * PAGE_SIZE, &memory,
            ),
            Err(AxError::InvalidInput)
        );
        // More than half of memory is -EINVAL as well, not -ENOMEM.
        assert_eq!(
            admit_destination_ranges_in(&[(0, 3 * PAGE_SIZE)], 4, 4 * PAGE_SIZE, &memory),
            Err(AxError::InvalidInput)
        );
        // A numeric address below total RAM is insufficient when it lies in
        // a platform memory-map hole.  Linux would accept it (its sanity check
        // leaves RAM validation to the caller), so this stays -EINVAL as a
        // declared divergence rather than becoming a fabricated errno.
        assert_eq!(
            admit_destination_ranges_in(
                &[(PAGE_SIZE, PAGE_SIZE)], 4, 4 * PAGE_SIZE,
                &[(0, PAGE_SIZE), (2 * PAGE_SIZE, 2 * PAGE_SIZE)],
            ),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn empty_destination_segment_is_admitted_like_linux() {
        let memory = [(0, 4 * PAGE_SIZE)];
        // `sanity_check_segment_list()` lets `mend == mstart` through every
        // address test, and `kimage_load_normal_segment()` then copies nothing:
        //
        // 	mstart = image->segment[i].mem;
        // 	mend   = mstart + image->segment[i].memsz;
        // 	if (mstart > mend)
        // 		return -EADDRNOTAVAIL;
        // 	...
        // 	while (mbytes) { ... }
        //
        // so `kexec_load` reports success for an empty destination and the
        // segment contributes no pages.
        assert!(
            admit_destination_ranges_in(&[(PAGE_SIZE, 0)], 4, 4 * PAGE_SIZE, &memory).is_ok()
        );
        // An empty destination is still bounded by the same overlap rule, which
        // Linux spells `(mend > pstart) && (mstart < pend)` and which therefore
        // rejects a zero-length segment strictly inside another one:
        assert_eq!(
            admit_destination_ranges_in(
                &[(0, 2 * PAGE_SIZE), (PAGE_SIZE, 0)], 4, 4 * PAGE_SIZE, &memory,
            ),
            Err(AxError::InvalidInput)
        );
        // ...and admits one that merely touches a neighbour's edge, because
        // `mstart < pend` and `mend > pstart` are both false there.
        assert!(
            admit_destination_ranges_in(
                &[(0, PAGE_SIZE), (PAGE_SIZE, 0)], 4, 4 * PAGE_SIZE, &memory,
            )
            .is_ok()
        );
        // TheKernel's RAM admission still applies to the empty range, so an
        // empty destination outside the platform memory map stays -EINVAL -- the
        // declared divergence that keeps out-of-RAM destinations out.  The gap
        // between the two regions below has to be wider than a page for the
        // range to be empty *and* strictly outside both of them.
        assert_eq!(
            admit_destination_ranges_in(
                &[(2 * PAGE_SIZE, 0)],
                8,
                8 * PAGE_SIZE,
                &[(0, PAGE_SIZE), (4 * PAGE_SIZE, PAGE_SIZE)],
            ),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn destination_addresses_use_eaddrnotavail_not_einval() {
        let memory = [(0, 4 * PAGE_SIZE)];
        // 	unlikely(mstart > mend) -> -EADDRNOTAVAIL
        assert_eq!(
            admit_destination_ranges_in(&[(0, usize::MAX)], 4, 4 * PAGE_SIZE, &memory),
            Err(AxError::from(LinuxError::EADDRNOTAVAIL))
        );
        // 	unaligned start -> -EADDRNOTAVAIL
        assert_eq!(
            admit_destination_ranges_in(&[(1, PAGE_SIZE)], 4, 4 * PAGE_SIZE, &memory),
            Err(AxError::from(LinuxError::EADDRNOTAVAIL))
        );
        // 	unaligned end -> -EADDRNOTAVAIL
        assert_eq!(
            admit_destination_ranges_in(&[(0, PAGE_SIZE + 1)], 4, 4 * PAGE_SIZE, &memory),
            Err(AxError::from(LinuxError::EADDRNOTAVAIL))
        );
        // 	mend >= KEXEC_DESTINATION_MEMORY_LIMIT -> -EADDRNOTAVAIL
        assert_eq!(
            admit_destination_ranges_in(
                &[(KEXEC_DESTINATION_MEMORY_LIMIT - PAGE_SIZE, PAGE_SIZE)],
                4,
                KEXEC_DESTINATION_MEMORY_LIMIT + PAGE_SIZE,
                &[(0, KEXEC_DESTINATION_MEMORY_LIMIT + PAGE_SIZE)],
            ),
            Err(AxError::from(LinuxError::EADDRNOTAVAIL))
        );
    }

    #[test]
    fn raw_segment_keeps_linux_sanity_check_order_and_errnos() {
        // A zero-length destination is not a Linux error: `mend == mstart`
        // passes the wraparound, alignment and limit tests, and the segment
        // then contributes no pages.  `admit_destination_ranges()` admits it as
        // well, so the whole `kexec_load` request succeeds.
        let empty = KexecSegment {
            buf: ptr::null(),
            bufsz: 0,
            mem: 2 * PAGE_SIZE,
            memsz: 0,
        };
        assert_eq!(validate_segment_address(empty), Ok(()));
        // The address pass never reads `bufsz`, and the buffer pass never reads
        // `mem`, because Linux runs two separate loops over the request:
        //
        // 	mend = mstart + image->segment[i].memsz;      /* loop 1 */
        // 	if ((mstart & ~PAGE_MASK) || (mend & ~PAGE_MASK))
        // 		return -EADDRNOTAVAIL;
        // 	...
        // 	if (image->segment[i].bufsz > image->segment[i].memsz)  /* loop 3 */
        // 		return -EINVAL;
        //
        // so a first segment with `bufsz > memsz` cannot preempt a second
        // segment's address error, and the reverse also holds.
        let oversized = KexecSegment {
            buf: ptr::null(),
            bufsz: PAGE_SIZE + 1,
            mem: 2 * PAGE_SIZE,
            memsz: PAGE_SIZE,
        };
        assert_eq!(validate_segment_address(oversized), Ok(()));
        assert_eq!(validate_segment_buffer(oversized), Err(AxError::InvalidInput));
        assert_eq!(
            validate_segment_address(KexecSegment { mem: 2 * PAGE_SIZE + 1, ..oversized }),
            Err(AxError::from(LinuxError::EADDRNOTAVAIL))
        );
        // A NULL `buf` with a non-zero `bufsz` is *not* -EINVAL here: Linux
        // faults in `copy_from_user()` and reports -EFAULT at load time.
        assert_eq!(
            validate_segment_buffer(KexecSegment { bufsz: PAGE_SIZE, ..oversized }),
            Ok(())
        );
    }

    #[test]
    fn short_non_elf_bzimage_is_rejected_without_indexing() {
        assert!(parse_bzimage(&[], &[], None, false).is_err());
        assert!(parse_bzimage(&[0; SETUP_HEADER_OFFSET], &[], None, false).is_err());
    }

    #[test]
    fn no_loaded_image_is_invalid_for_kexec_reboot() {
        let _context = crate::test_support::scheduler_test_context();
        *NORMAL.lock() = None;
        assert!(matches!(execute_loaded(), Err(AxError::InvalidInput)));
    }
}
