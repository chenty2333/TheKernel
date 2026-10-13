// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Intel graphics OpRegion ownership and VBT mailbox handling.
//!
//! The Linux implementation also services ACPI notifications, SWSCI commands,
//! and ASLE work. TheKernel currently has no ACPI notifier/backlight service,
//! but the OpRegion mapping and mailbox #4 VBT are core display inputs and are
//! implemented here. The mapping remains owned until cleanup; BIOS consumers
//! receive a bounded private copy, never a pointer into firmware memory.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::{c_int, c_void},
    mem::{offset_of, size_of},
    ptr,
};

use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_device_upstream::intel_display_device_enabled;
use crate::intel_display_types_upstream::{
    IntelEncoder, INTEL_OUTPUT_ANALOG, INTEL_OUTPUT_DDI, INTEL_OUTPUT_DP, INTEL_OUTPUT_DP_MST,
    INTEL_OUTPUT_DSI, INTEL_OUTPUT_EDP, INTEL_OUTPUT_HDMI,
};
use crate::intel_display_limits_types_upstream::PORT_E;

const ENODEV: c_int = 19;
const EINVAL: c_int = 22;
const ENOMEM: c_int = 12;
const EIO: c_int = 5;
// Linux's internal ENOTSUPP (not the userspace EOPNOTSUPP errno).
const ENOTSUPP: c_int = 524;
const GFP_KERNEL: u32 = tk_linux_kapi::config::GFP_KERNEL;

const ASLS: c_int = 0xfc;
const OPREGION_SIZE: usize = 8 * 1024;
const OPREGION_HEADER_SIZE: usize = 0x100;
const OPREGION_ASLE_OFFSET: usize = 0x300;
const OPREGION_VBT_OFFSET: usize = 0x400;
const OPREGION_ASLE_EXT_OFFSET: usize = 0x1c00;
const MBOX_ACPI: u32 = 1 << 0;
const MBOX_SWSCI: u32 = 1 << 1;
const MBOX_ASLE: u32 = 1 << 2;
const MBOX_ASLE_EXT: u32 = 1 << 4;
const MBOX_BACKLIGHT: u32 = 1 << 5;
const MAX_VBT_SIZE: usize = u16::MAX as usize;
const OPREGION_SWSCI_OFFSET: usize = 0x200;
const SWSCI: c_int = 0xe8;
const SWSCI_SCIC_INDICATOR: u32 = 1 << 0;
const SWSCI_SCIC_MAIN_FUNCTION_SHIFT: u32 = 1;
const SWSCI_SCIC_MAIN_FUNCTION_MASK: u32 = 0xf << SWSCI_SCIC_MAIN_FUNCTION_SHIFT;
const SWSCI_SCIC_SUB_FUNCTION_SHIFT: u32 = 8;
const SWSCI_SCIC_SUB_FUNCTION_MASK: u32 = 0xff << SWSCI_SCIC_SUB_FUNCTION_SHIFT;
const SWSCI_SCIC_EXIT_STATUS_SHIFT: u32 = 5;
const SWSCI_SCIC_EXIT_STATUS_MASK: u32 = 7 << SWSCI_SCIC_EXIT_STATUS_SHIFT;
const SWSCI_SCIC_EXIT_STATUS_SUCCESS: u32 = 1;
const SWSCI_SCISEL: u16 = 1 << 15;
const SWSCI_GSSCIE: u16 = 1 << 0;
const SWSCI_GBDA: u32 = 4;
const SWSCI_SBCB: u32 = 6;
const SWSCI_GBDA_SUPPORTED_CALLS: u32 = SWSCI_GBDA << 1;
const SWSCI_GBDA_REQUESTED_CALLBACKS: u32 = (SWSCI_GBDA << 1) | (1 << 8);
const SWSCI_SBCB_SUPPORTED_CALLBACKS: u32 = SWSCI_SBCB << 1;
const SWSCI_SBCB_ADAPTER_POWER_STATE: u32 = (SWSCI_SBCB << 1) | (7 << 8);
const SWSCI_SBCB_DISPLAY_POWER_STATE: u32 = (SWSCI_SBCB << 1) | (8 << 8);
const MAX_DSLP_MS: u32 = 1500;

const OPREGION_SIGNATURE: &[u8; 16] = b"IntelGraphicsMem";

/// `intel_opregion.c:struct opregion_header`.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct OpRegionHeader {
    signature: [u8; 16],
    /// OpRegion size in KiB.
    size: u32,
    /// reserved, revision, minor, major
    version: [u8; 4],
    bios_ver: [u8; 32],
    vbios_ver: [u8; 16],
    driver_ver: [u8; 16],
    mboxes: u32,
    driver_model: u32,
    pcon: u32,
    dver: [u8; 32],
    rsvd: [u8; 124],
}

/// The portion of mailbox #3 that locates the optional raw VBT.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct OpRegionAsle {
    ardy: u32,
    aslc: u32,
    tche: u32,
    alsi: u32,
    bclp: u32,
    pfit: u32,
    cblv: u32,
    bclm: [u16; 20],
    cpfm: u32,
    epfm: u32,
    plut: [u8; 74],
    pfmb: u32,
    cddv: u32,
    pcft: u32,
    srot: u32,
    iuer: u32,
    fdss: u64,
    fdsp: u32,
    stat: u32,
    rvda: u64,
    rvds: u32,
    rsvd: [u8; 58],
}

/// Private state behind `IntelDisplay::opregion`; it mirrors the resource
/// ownership of Linux's `struct intel_opregion` without publishing a new ABI.
#[repr(C)]
struct IntelOpRegion {
    display: *mut IntelDisplay,
    physical: u64,
    mapped: *mut c_void,
    /// A separately mapped RVDA mailbox VBT, if one was valid.
    rvda_mapped: *mut c_void,
/// A validated, owned copy of the firmware VBT, released at cleanup.
    vbt: *const u8,
    vbt_size: usize,
    mailboxes: u32,
    swsci_gbda_sub_functions: u32,
    swsci_sbcb_sub_functions: u32,
    major: u8,
    minor: u8,
    pcon: u32,
}

unsafe extern "C" {
    fn pci_read_config_dword(device: *mut c_void, offset: c_int, value: *mut u32) -> c_int;
    fn pci_read_config_word(device: *mut c_void, offset: c_int, value: *mut u16) -> c_int;
    fn pci_write_config_word(device: *mut c_void, offset: c_int, value: u16) -> c_int;
}

#[inline]
unsafe fn device_pci(display: *mut IntelDisplay) -> *mut c_void {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return ptr::null_mut();
    }
    let device = unsafe { (*(*display).drm).dev };
    if device.is_null() {
        return ptr::null_mut();
    }
    unsafe { crate::linux_platform::to_pci_dev(device) }
}

#[inline]
unsafe fn read_io<T: Copy>(base: *const u8, offset: usize) -> T {
    let mut value = core::mem::MaybeUninit::<T>::uninit();
    unsafe {
        tk_linux_kapi::primitives::memcpy_fromio(
            value.as_mut_ptr().cast(),
            base.add(offset).cast(),
            size_of::<T>(),
        );
        value.assume_init()
    }
}

unsafe fn copy_io_region(src: *const u8, size: usize) -> *mut u8 {
    let copy = unsafe { tk_linux_kapi::memory::kmalloc(size, GFP_KERNEL).cast::<u8>() };
    if copy.is_null() {
        return ptr::null_mut();
    }
    unsafe { tk_linux_kapi::primitives::memcpy_fromio(copy.cast(), src.cast(), size) };
    copy
}

unsafe fn free_opregion(opregion: *mut IntelOpRegion) {
    if opregion.is_null() {
        return;
    }
    unsafe {
        if !(*opregion).rvda_mapped.is_null() {
            tk_linux_kapi::primitives::iounmap((*opregion).rvda_mapped);
        }
        if !(*opregion).mapped.is_null() {
            tk_linux_kapi::primitives::iounmap((*opregion).mapped);
        }
        tk_linux_kapi::memory::kfree(opregion.cast::<c_void>());
    }
}

unsafe fn copy_valid_vbt(opregion: *mut IntelOpRegion, vbt: *const u8, size: usize) {
    if vbt.is_null() || size < size_of::<crate::intel_vbt_defs_types_upstream::VbtHeader>() {
        return;
    }
    let copy = unsafe { copy_io_region(vbt, size) };
    if copy.is_null() {
        return;
    }
    if unsafe { crate::intel_bios_upstream::intel_bios_is_valid_vbt_bytes(copy, size) } {
        unsafe {
            (*opregion).vbt = copy;
            (*opregion).vbt_size = size;
        }
    } else {
        unsafe { tk_linux_kapi::memory::kfree(copy.cast::<c_void>()) };
    }
}

fn rvda_physical_address(asls: u32, major: u8, minor: u8, rvda: u64) -> Option<u64> {
    if major > 2 || (major == 2 && minor >= 1) {
        if rvda < OPREGION_SIZE as u64 {
            return None;
        }
        (asls as u64).checked_add(rvda)
    } else {
        Some(rvda)
    }
}

#[inline]
unsafe fn has_ddi(display: *mut IntelDisplay) -> bool {
    if display.is_null() || unsafe { (*display).info.__device_info.is_null() } {
        return false;
    }
    unsafe { (*(*display).info.__device_info)._bitfields_46[0] & (1 << 3) != 0 }
}

#[inline]
unsafe fn swsci_mailbox(opregion: *mut IntelOpRegion) -> *mut u8 {
    if opregion.is_null()
        || unsafe { (*opregion).mailboxes & MBOX_SWSCI == 0 || (*opregion).major >= 3 }
    {
        ptr::null_mut()
    } else {
        unsafe { (*opregion).mapped.cast::<u8>().add(OPREGION_SWSCI_OFFSET) }
    }
}

unsafe fn check_swsci_function(display: *mut IntelDisplay, function: u32) -> c_int {
    if display.is_null() || unsafe { (*display).opregion.is_null() } {
        return -ENODEV;
    }
    let opregion = unsafe { (*display).opregion.cast::<IntelOpRegion>() };
    if unsafe { swsci_mailbox(opregion) }.is_null() {
        return -ENODEV;
    }
    let main = (function & SWSCI_SCIC_MAIN_FUNCTION_MASK) >> SWSCI_SCIC_MAIN_FUNCTION_SHIFT;
    let sub = (function & SWSCI_SCIC_SUB_FUNCTION_MASK) >> SWSCI_SCIC_SUB_FUNCTION_SHIFT;
    let supported = unsafe {
        if main == SWSCI_SBCB {
            (*opregion).swsci_sbcb_sub_functions
        } else if main == SWSCI_GBDA {
            (*opregion).swsci_gbda_sub_functions
        } else {
            return 0;
        }
    };
    if supported & (1u32.checked_shl(sub).unwrap_or(0)) == 0 {
        -EINVAL
    } else {
        0
    }
}

/// Linux's synchronous SWSCI transaction through mailbox #2.
unsafe fn swsci(
    display: *mut IntelDisplay,
    function: u32,
    parm: u32,
    parm_out: *mut u32,
) -> c_int {
    let ret = unsafe { check_swsci_function(display, function) };
    if ret != 0 {
        return ret;
    }
    let opregion = unsafe { (*display).opregion.cast::<IntelOpRegion>() };
    let mailbox = unsafe { swsci_mailbox(opregion) };
    let scic_ptr = mailbox.cast::<u32>();
    let parm_ptr = unsafe { mailbox.add(4).cast::<u32>() };
    let dslp_ptr = unsafe { mailbox.add(8).cast::<u32>() };
    let mut dslp = unsafe { ptr::read_volatile(dslp_ptr) };
    if dslp == 0 {
        dslp = 50;
    } else if dslp > MAX_DSLP_MS {
        dslp = MAX_DSLP_MS;
    }
    let mut scic = unsafe { ptr::read_volatile(scic_ptr) };
    if scic & SWSCI_SCIC_INDICATOR != 0 {
        return -tk_linux_kapi::config::EBUSY;
    }
    unsafe {
        ptr::write_volatile(parm_ptr, parm);
        ptr::write_volatile(scic_ptr, function | SWSCI_SCIC_INDICATOR);
    }

    let pdev = unsafe { device_pci(display) };
    if pdev.is_null() {
        return -ENODEV;
    }
    let mut swsci_cfg = 0u16;
    let _ = unsafe { pci_read_config_word(pdev, SWSCI, &mut swsci_cfg) };
    if swsci_cfg & SWSCI_SCISEL == 0 || swsci_cfg & SWSCI_GSSCIE != 0 {
        swsci_cfg |= SWSCI_SCISEL;
        swsci_cfg &= !SWSCI_GSSCIE;
        let _ = unsafe { pci_write_config_word(pdev, SWSCI, swsci_cfg) };
    }
    swsci_cfg |= SWSCI_GSSCIE;
    let _ = unsafe { pci_write_config_word(pdev, SWSCI, swsci_cfg) };

    let ret = tk_linux_kapi::poll_timeout_us!(
        { scic = unsafe { ptr::read_volatile(scic_ptr) } },
        (scic & SWSCI_SCIC_INDICATOR) == 0,
        1000,
        dslp as u64 * 1000,
        false,
    );
    if ret != 0 {
        return ret;
    }
    let exit_status = (scic & SWSCI_SCIC_EXIT_STATUS_MASK) >> SWSCI_SCIC_EXIT_STATUS_SHIFT;
    if exit_status != SWSCI_SCIC_EXIT_STATUS_SUCCESS {
        return -EIO;
    }
    if !parm_out.is_null() {
        unsafe { *parm_out = ptr::read_volatile(parm_ptr) };
    }
    0
}

unsafe fn swsci_setup(display: *mut IntelDisplay) {
    let opregion = unsafe { (*display).opregion.cast::<IntelOpRegion>() };
    if unsafe { swsci_mailbox(opregion) }.is_null() {
        return;
    }
    unsafe {
        (*opregion).swsci_gbda_sub_functions = 1;
        (*opregion).swsci_sbcb_sub_functions = 1;
    }
    let mut supported = 0u32;
    if unsafe { swsci(display, SWSCI_GBDA_SUPPORTED_CALLS, 0, &mut supported) } == 0 {
        unsafe { (*opregion).swsci_gbda_sub_functions |= supported << 1 };
    }
    let requested = unsafe {
        swsci(display, SWSCI_GBDA_REQUESTED_CALLBACKS, 0, &mut supported) == 0
    };
    if requested {
        unsafe { (*opregion).swsci_sbcb_sub_functions |= supported };
    }
    if unsafe { swsci(display, SWSCI_SBCB_SUPPORTED_CALLBACKS, 0, &mut supported) } == 0 {
        let low = supported & 0x7ff;
        let high = supported & !0xfff;
        let mask = (high << 4) | (low << 1) | 1;
        if !requested {
            unsafe { (*opregion).swsci_sbcb_sub_functions |= mask };
        }
    }
}

/// Linux `intel_opregion_setup()`: read PCI ASLS, map the OpRegion, validate
/// its header, and retain mailbox #4 (or a valid RVDA VBT) for BIOS parsing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_opregion_setup(display: *mut IntelDisplay) -> c_int {
    if display.is_null() {
        return -EINVAL;
    }
    if unsafe { !(*display).opregion.is_null() } {
        return 0;
    }

    let pdev = unsafe { device_pci(display) };
    if pdev.is_null() {
        return -ENODEV;
    }
    let mut asls = 0u32;
    let ret = unsafe { pci_read_config_dword(pdev, ASLS, &mut asls) };
    if ret != 0 {
        return if ret < 0 { ret } else { -EIO };
    }
    if asls == 0 {
        return -ENOTSUPP;
    }
    // ASLS is a physical address supplied by firmware. Refuse malformed or
    // wrapping ranges before asking the mapping provider to establish it.
    if asls & 0xfff != 0 || (asls as u64).checked_add(OPREGION_SIZE as u64).is_none() {
        return -EINVAL;
    }

    let mapped = unsafe {
        tk_linux_kapi::primitives::ioremap(asls as u64, OPREGION_SIZE as u64)
    };
    if mapped.is_null() {
        return -ENOMEM;
    }

    let header = unsafe { read_io::<OpRegionHeader>(mapped.cast(), 0) };
    let sig = unsafe { ptr::addr_of!(header.signature).read_unaligned() };
    let opregion_kib = unsafe { ptr::addr_of!(header.size).read_unaligned() };
    let mailboxes = unsafe { ptr::addr_of!(header.mboxes).read_unaligned() };
    let version = unsafe { ptr::addr_of!(header.version).read_unaligned() };
    let pcon = unsafe { ptr::addr_of!(header.pcon).read_unaligned() };

    if &sig != OPREGION_SIGNATURE || opregion_kib < (OPREGION_SIZE / 1024) as u32 {
        unsafe { tk_linux_kapi::primitives::iounmap(mapped) };
        return -EINVAL;
    }

    let opregion = unsafe {
        tk_linux_kapi::memory::kmalloc(size_of::<IntelOpRegion>(), GFP_KERNEL)
            .cast::<IntelOpRegion>()
    };
    if opregion.is_null() {
        unsafe { tk_linux_kapi::primitives::iounmap(mapped) };
        return -ENOMEM;
    }
    unsafe {
        ptr::write(
            opregion,
            IntelOpRegion {
                display,
                physical: asls as u64,
                mapped,
                rvda_mapped: ptr::null_mut(),
                vbt: ptr::null(),
                vbt_size: 0,
                mailboxes,
                swsci_gbda_sub_functions: 0,
                swsci_sbcb_sub_functions: 0,
                major: version[3],
                minor: version[2],
                pcon,
            },
        );
        (*display).opregion = opregion.cast();
    }

    unsafe { clear_unimplemented_mailbox_readiness(mapped.cast(), mailboxes) };
    unsafe { swsci_setup(display) };

    // Mailbox #3's RVDA is preferred when supplied by OpRegion 2.0+.
    if mailboxes & MBOX_ASLE != 0 && version[3] >= 2 {
        let asle = unsafe { read_io::<OpRegionAsle>(mapped.cast(), OPREGION_ASLE_OFFSET) };
        let rvda = unsafe { ptr::addr_of!(asle.rvda).read_unaligned() };
        let rvds = unsafe { ptr::addr_of!(asle.rvds).read_unaligned() as usize };
        if rvda != 0 && rvds != 0 && rvds <= MAX_VBT_SIZE {
            if let Some(physical) = rvda_physical_address(asls, version[3], version[2], rvda)
                .filter(|physical| *physical != 0 && physical.checked_add(rvds as u64).is_some())
            {
                let rvda_map = unsafe {
                    tk_linux_kapi::primitives::ioremap(physical, rvds as u64)
                };
                if !rvda_map.is_null() {
                    unsafe { (*opregion).rvda_mapped = rvda_map };
                    unsafe { copy_valid_vbt(opregion, rvda_map.cast(), rvds) };
                    if !unsafe { (*opregion).vbt.is_null() } {
                        return 0;
                    }
                    unsafe {
                        tk_linux_kapi::primitives::iounmap(rvda_map);
                        (*opregion).rvda_mapped = ptr::null_mut();
                    }
                }
            }
        }
    }

    // Mailbox #4 is the normal VBT source. The extension mailbox truncates
    // its available range at 0x1c00; otherwise the OpRegion ends at 0x2000.
    let vbt_limit = if mailboxes & MBOX_ASLE_EXT != 0 {
        OPREGION_ASLE_EXT_OFFSET
    } else {
        OPREGION_SIZE
    };
    let vbt_size = vbt_limit.saturating_sub(OPREGION_VBT_OFFSET);
    if vbt_size != 0 {
        unsafe {
            copy_valid_vbt(
                opregion,
                mapped.cast::<u8>().add(OPREGION_VBT_OFFSET),
                vbt_size,
            );
        }
    }

    0
}

/// Return whether setup found a validated VBT, matching Linux's predicate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_opregion_vbt_present(display: *mut IntelDisplay) -> bool {
    if display.is_null() || unsafe { (*display).opregion.is_null() } {
        return false;
    }
    let opregion = unsafe { (*display).opregion.cast::<IntelOpRegion>() };
    unsafe { !(*opregion).vbt.is_null() }
}

/// Return a caller-owned VBT copy. Linux BIOS init frees this result after
/// parsing, so the copy never exposes the lifetime of firmware memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_opregion_get_vbt(
    display: *mut IntelDisplay,
    size: *mut usize,
) -> *const c_void {
    if !size.is_null() {
        unsafe { *size = 0 };
    }
    if display.is_null() || unsafe { (*display).opregion.is_null() } {
        return ptr::null();
    }
    let opregion = unsafe { (*display).opregion.cast::<IntelOpRegion>() };
    let vbt = unsafe { (*opregion).vbt };
    let len = unsafe { (*opregion).vbt_size };
    if vbt.is_null() || len == 0 {
        return ptr::null();
    }
    let copy = unsafe { tk_linux_kapi::memory::kmalloc(len, GFP_KERNEL).cast::<u8>() };
    if copy.is_null() {
        return ptr::null();
    }
    unsafe {
        ptr::copy_nonoverlapping(vbt, copy, len);
        if !size.is_null() {
            *size = len;
        }
    }
    copy.cast()
}

/// Linux `intel_opregion_headless_sku()` reads the PCON bit only from OpRegion
/// v2.3 and later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_opregion_headless_sku(display: *mut IntelDisplay) -> bool {
    if display.is_null() || unsafe { (*display).opregion.is_null() } {
        return false;
    }
    let opregion = unsafe { (*display).opregion.cast::<IntelOpRegion>() };
    let major = unsafe { (*opregion).major };
    let minor = unsafe { (*opregion).minor };
    (major > 2 || (major == 2 && minor >= 3))
        && unsafe { (*opregion).pcon & (1 << 13) != 0 }
}

/// ACPI notifications and ASLE interrupt work are not connected to a
/// TheKernel event source. Mailbox #4 VBT and SWSCI remain usable, but do not
/// tell firmware that the driver handles ACPI notifications or ASLE events.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_opregion_register(display: *mut IntelDisplay) {
    unsafe { intel_opregion_resume(display) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_opregion_unregister(display: *mut IntelDisplay) {
    unsafe { intel_opregion_suspend(display, 1) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_opregion_suspend(display: *mut IntelDisplay, state: u32) {
    if display.is_null() || unsafe { (*display).opregion.is_null() } {
        return;
    }
    let _ = unsafe { intel_opregion_notify_adapter(display, state) };
    if !unsafe { intel_display_device_enabled(display) } {
        return;
    }
    let opregion = unsafe { (*display).opregion.cast::<IntelOpRegion>() };
    let mapped = unsafe { (*opregion).mapped.cast::<u8>() };
    unsafe { clear_unimplemented_mailbox_readiness(mapped, (*opregion).mailboxes) };
}

/// Keep optional ACPI/ASLE services unavailable until their event handlers
/// exist. ASLE_TCHE advertises capabilities as well as ARDY being its
/// readiness handshake; both must remain clear.
unsafe fn clear_unimplemented_mailbox_readiness(mapped: *mut u8, mailboxes: u32) {
    if mapped.is_null() {
        return;
    }

    if mailboxes & MBOX_ACPI != 0 {
        unsafe {
            // ACPI mailbox: CSTS then DRDY, matching Linux's resume ordering
            // while keeping this driver not-ready.
            ptr::write_volatile(mapped.add(0x104).cast::<u32>(), 0);
            ptr::write_volatile(mapped.add(0x100).cast::<u32>(), 0);
        }
    }
    if mailboxes & MBOX_ASLE != 0 {
        unsafe {
            let asle = mapped.add(OPREGION_ASLE_OFFSET);
            ptr::write_volatile(asle.cast::<u32>(), 0); // ASLE_ARDY_NOT_READY
            ptr::write_volatile(asle.add(8).cast::<u32>(), 0); // no ASLE capabilities
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_opregion_resume(display: *mut IntelDisplay) {
    if display.is_null() || unsafe { (*display).opregion.is_null() } {
        return;
    }
    let opregion = unsafe { (*display).opregion.cast::<IntelOpRegion>() };
    if unsafe { intel_display_device_enabled(display) } {
        let mapped = unsafe { (*opregion).mapped.cast::<u8>() };
        unsafe { clear_unimplemented_mailbox_readiness(mapped, (*opregion).mailboxes) };
    }
    let _ = unsafe { intel_opregion_notify_adapter(display, 0) };
}

/// The display interrupt path may call this Linux OpRegion hook, but this
/// build has no ASLE worker. Keep it an explicit no-op and never advertise
/// mailbox #3 readiness/capabilities until asynchronous ASLE service exists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_opregion_asle_intr(_display: *mut IntelDisplay) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_opregion_notify_adapter(
    display: *mut IntelDisplay,
    state: u32,
) -> c_int {
    if display.is_null() || unsafe { !has_ddi(display) } {
        return 0;
    }
    let parm = match state {
        0 => 0x00, // PCI_D0
        1 => 0x01, // PCI_D1
        2 => 0x02, // PCI_D2
        3 | 4 => 0x04, // PCI_D3hot / PCI_D3cold
        _ => return -EINVAL,
    };
    unsafe { swsci(display, SWSCI_SBCB_ADAPTER_POWER_STATE, parm, ptr::null_mut()) }
}

/// Linux `intel_opregion_notify_encoder()` reports the legacy connector class
/// and power state through SWSCI. The port is range-checked before shifting.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_opregion_notify_encoder(
    encoder: *mut IntelEncoder,
    enable: bool,
) -> c_int {
    if encoder.is_null() {
        return -EINVAL;
    }
    let drm = unsafe { (*encoder).base.dev };
    if drm.is_null() || unsafe { (*drm).dev_private.is_null() } {
        return -ENODEV;
    }
    let i915 = unsafe { (*drm).dev_private.cast::<crate::linux_i915_private::DrmI915Private>() };
    let display = unsafe { (*i915).display.cast::<IntelDisplay>() };
    if !unsafe { has_ddi(display) } {
        return 0;
    }
    let ret = unsafe { check_swsci_function(display, SWSCI_SBCB_DISPLAY_POWER_STATE) };
    if ret != 0 {
        return ret;
    }

    let output = unsafe { (*encoder).r#type };
    let mut port = unsafe { (*encoder).port };
    let mut parm = 0u32;
    if output == INTEL_OUTPUT_DSI {
        port = 0;
    } else if port == PORT_E {
        port = 0;
    } else {
        if !(0..32).contains(&port) {
            return -EINVAL;
        }
        parm |= 1u32 << port;
        port += 1;
    }
    if port > 4 {
        return -EINVAL;
    }
    if !enable {
        parm |= 4 << 8;
    }
    let display_type = match output {
        INTEL_OUTPUT_ANALOG => 0, // CRT
        INTEL_OUTPUT_DDI | INTEL_OUTPUT_DP | INTEL_OUTPUT_HDMI | INTEL_OUTPUT_DP_MST => 2,
        INTEL_OUTPUT_EDP | INTEL_OUTPUT_DSI => 3,
        _ => return -EINVAL,
    };
    parm |= display_type << (16 + port as u32 * 3);
    unsafe { swsci(display, SWSCI_SBCB_DISPLAY_POWER_STATE, parm, ptr::null_mut()) }
}

/// `intel_opregion_cleanup()` unmaps both firmware regions and drops private
/// VBT state. It is safe for partial setup and repeated teardown.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_opregion_cleanup(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    let opregion = unsafe { (*display).opregion.cast::<IntelOpRegion>() };
    if opregion.is_null() {
        return;
    }
    unsafe {
        (*display).opregion = ptr::null_mut();
        if !(*opregion).vbt.is_null() {
            tk_linux_kapi::memory::kfree((*opregion).vbt.cast_mut().cast::<c_void>());
        }
        free_opregion(opregion);
    }
}

/// Header-size assertions mirror `intel_opregion.c` BUILD_BUG_ON checks.
const _: () = assert!(size_of::<OpRegionHeader>() == OPREGION_HEADER_SIZE);
const _: () = assert!(size_of::<OpRegionAsle>() == 0x100);
const _: () = assert!(offset_of!(OpRegionAsle, rvda) == 0xba);
const _: () = assert!(offset_of!(OpRegionAsle, rvds) == 0xc2);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailbox_constants_bound_vbt_before_extension() {
        assert_eq!(OPREGION_VBT_OFFSET, 0x400);
        assert_eq!(OPREGION_ASLE_EXT_OFFSET - OPREGION_VBT_OFFSET, 0x1800);
        assert_eq!(OPREGION_SIZE - OPREGION_VBT_OFFSET, 0x1c00);
    }

    #[test]
    fn rvda_relative_offset_must_not_point_inside_opregion() {
        assert_eq!(rvda_physical_address(0x1000, 2, 1, 0x1fff), None);
        assert_eq!(rvda_physical_address(0x1000, 2, 1, 0x2000), Some(0x3000));
        assert_eq!(rvda_physical_address(0x1000, 2, 0, 0x200), Some(0x200));
        assert_eq!(rvda_physical_address(u32::MAX - 0x1000, 3, 0, 0x2000), None);
    }

    #[test]
    fn optional_acpi_and_asle_mailboxes_stay_not_ready() {
        let mut mailbox = [u32::MAX; OPREGION_SIZE / size_of::<u32>()];
        let mapped = mailbox.as_mut_ptr().cast::<u8>();

        unsafe {
            clear_unimplemented_mailbox_readiness(mapped, MBOX_ACPI | MBOX_ASLE | MBOX_SWSCI);
        }

        assert_eq!(mailbox[0x100 / 4], 0, "ACPI DRDY must stay clear");
        assert_eq!(mailbox[0x104 / 4], 0, "ACPI CSTS must stay clear");
        assert_eq!(mailbox[OPREGION_ASLE_OFFSET / 4], 0, "ASLE ARDY must stay clear");
        assert_eq!(mailbox[(OPREGION_ASLE_OFFSET + 8) / 4], 0, "ASLE capabilities must stay clear");
        assert_eq!(mailbox[OPREGION_SWSCI_OFFSET / 4], u32::MAX, "SWSCI is independent");
    }
}
