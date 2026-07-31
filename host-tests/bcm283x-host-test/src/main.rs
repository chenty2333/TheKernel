//! Host-side bring-up test for the bcm283x-hvs / bcm283x-v3d drivers.
//!
//! Runs on a Raspberry Pi (Zero 2W or 0-3) under Linux as root. It maps the
//! BCM283x peripheral window at its physical address (identity mapping via
//! /dev/mem), so the drivers' fixed physical addresses work unmodified, then
//! exercises:
//!
//! 1. V3D identity + a halt-only job through both threads;
//! 2. a clear-frame job (binner + renderer) writing into a framebuffer;
//! 3. the HVS + HDMI bring-up (the screen should show the test pattern).
//!
//! The kernel `vc4` driver must be unloaded first (`sudo rmmod vc4`).

use std::alloc::{alloc, dealloc, Layout};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::io::AsRawFd;

use bcm283x_hvs::{HvsDisplay, HvsDisplayConfig, HvsResources};
use rdif_display::Interface as _;
use bcm283x_v3d::cl;
use bcm283x_v3d::regs::{V3D_INT_FLDONE, V3D_INT_FRDONE};
use bcm283x_v3d::V3dCore;

const PERIPHERAL_BASE: usize = 0x3f00_0000;
const PERIPHERAL_SIZE: usize = 0x0100_0000;

const V3D_BASE: usize = 0x3fc0_0000;
const HVS_BASE: usize = 0x3f40_0000;
const HDMI_BASE: usize = 0x3f90_2000;

const FB_WIDTH: u32 = 1920;
const FB_HEIGHT: u32 = 1080;
const FB_BPP: usize = 4;
const FB_SIZE: usize = (FB_WIDTH as usize) * (FB_HEIGHT as usize) * FB_BPP;

/// Maps `/dev/mem` at its physical address (identity mapping). Returns the
/// mapped base (== the requested physical address) on success.
fn mmap_identity(phys: usize, size: usize) -> Result<usize, String> {
    let file = File::open("/dev/mem").map_err(|e| format!("open /dev/mem: {e}"))?;
    let addr = unsafe {
        libc::mmap(
            phys as *mut libc::c_void,
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_FIXED,
            file.as_raw_fd(),
            phys as libc::off_t,
        )
    };
    if addr == libc::MAP_FAILED {
        return Err(format!(
            "mmap 0x{phys:x} size 0x{size:x}: {} (is /dev/mem restricted?)",
            std::io::Error::last_os_error()
        ));
    }
    Ok(addr as usize)
}

/// DMA-style allocation: page-aligned, locked, with the physical address
/// read from `/proc/self/pagemap`. Requires root (PFN bits).
struct DmaBuf {
    ptr: *mut u8,
    phys: usize,
    size: usize,
}

fn alloc_dma(size: usize) -> Result<DmaBuf, String> {
    // 2 MiB-aligned to keep the whole buffer in the same GPU-visible window.
    let layout = Layout::from_size_align(size, 0x20_0000)
        .map_err(|e| format!("layout: {e}"))?;
    let ptr = unsafe { alloc(layout) };
    if ptr.is_null() {
        return Err("alloc failed".into());
    }
    unsafe {
        // Touch every page so it is backed.
        let slice = std::slice::from_raw_parts_mut(ptr, size);
        slice.fill(0);
        // Lock against swapping.
        let ret = libc::mlock(ptr as *const libc::c_void, size);
        if ret != 0 {
            dealloc(ptr, layout);
            return Err(format!("mlock: {}", std::io::Error::last_os_error()));
        }
    }

    let phys = page_phys(ptr as usize).ok_or("pagemap PFN unavailable (need root?)")?;
    Ok(DmaBuf { ptr, phys, size })
}

impl Drop for DmaBuf {
    fn drop(&mut self) {
        unsafe {
            libc::munlock(self.ptr as *const libc::c_void, self.size);
        }
        let layout = Layout::from_size_align(self.size, 0x20_0000).unwrap();
        unsafe { dealloc(self.ptr, layout) };
    }
}

/// Physical address of a user virtual address, via /proc/self/pagemap.
fn page_phys(vaddr: usize) -> Option<usize> {
    let mut file = File::open("/proc/self/pagemap").ok()?;
    let entry = (vaddr / 4096) * 8;
    let mut buf = [0u8; 8];
    file.seek(SeekFrom::Start(entry as u64)).ok()?;
    file.read_exact(&mut buf).ok()?;
    let value = u64::from_le_bytes(buf);
    if value & (1 << 63) == 0 {
        return None; // not present
    }
    let pfn = value & ((1u64 << 55) - 1);
    Some(pfn as usize * 4096 + (vaddr & 0xfff))
}

/// Cleans the CPU cache for a range so the GPU sees the writes. Requires
/// SCTLR_EL1.UCI=1 (default on arm64 Linux).
fn clean_cache(ptr: usize, size: usize) {
    let line = cache_line_size();
    let mut addr = ptr & !(line - 1);
    let end = ptr + size;
    while addr < end {
        unsafe {
            core::arch::asm!("dc cvac, {0}", in(reg) addr, options(nostack));
        }
        addr += line;
    }
    unsafe {
        core::arch::asm!("dsb sy", options(nostack));
    }
}

fn cache_line_size() -> usize {
    let ctr: u64;
    unsafe {
        core::arch::asm!("mrs {0}, ctr_el0", out(reg) ctr, options(nomem, nostack));
    }
    4 << (ctr & 0xf)
}

fn wait_v3d(core: &mut V3dCore, seqno: u64, what: &str) -> bool {
    let mut ok = false;
    for _ in 0..2_000_000 {
        let irqs = core.handle_irq();
        if irqs & (V3D_INT_FLDONE | V3D_INT_FRDONE) != 0 {
            ok = true;
        }
        if core.finished_seqno() >= seqno {
            ok = true;
            break;
        }
    }
    println!(
        "  {what}: seqno {seqno} finished={} state={:?} {}",
        core.finished_seqno(),
        core.state(),
        if ok { "OK" } else { "TIMEOUT" }
    );
    ok
}

fn test_v3d() -> bool {
    println!("== V3D ==");
    let mut core = V3dCore::new(bcm283x_v3d::V3dResources {
        base: V3D_BASE,
        binner_bus_addr: 0,
        binner_size: 0,
        binner_overflow_addr: 0,
        irq: None,
    });
    core.init();
    println!(
        "  identity OK, state={:?}",
        core.state()
    );

    // Halt-only job on the binner thread.
    let cl_mem = match alloc_dma(0x10_0000) {
        Ok(m) => m,
        Err(e) => {
            println!("  FAIL: cl alloc: {e}");
            return false;
        }
    };
    let cl = unsafe { std::slice::from_raw_parts_mut(cl_mem.ptr, cl_mem.size) };
    let halt_len = cl::build_halt_cl(cl);
    clean_cache(cl_mem.ptr as usize, halt_len);
    let seqno = core.submit_cl(0, cl_mem.phys as u32, (cl_mem.phys + halt_len) as u32);
    let ok = wait_v3d(&mut core, seqno, "halt job (binner)");

    // Clear-frame job: binner memory + BCL + RCL in the same buffer.
    let tiles_x = cl::tiles_for(FB_WIDTH);
    let tiles_y = cl::tiles_for(FB_HEIGHT);
    println!("  clear job: tiles {tiles_x}x{tiles_y}");
    let tile_state_size = cl::tile_state_size(tiles_x, tiles_y);
    let tile_alloc_offset = (tile_state_size + 0xfff) & !0xfff;
    let binner_off = tile_alloc_offset + cl::tile_alloc_size(tiles_x, tiles_y);
    let cl_off = (binner_off + 0xfff) & !0xfff;

    let bcl_len = cl::build_clear_bcl(
        &mut cl[0..],
        cl_mem.phys as u32,
        (cl_mem.phys + tile_alloc_offset) as u32,
        (cl_mem.size - tile_alloc_offset) as u32,
        tiles_x,
        tiles_y,
    );
    let rcl_len = cl::build_bin_rcl(
        &mut cl[cl_off..],
        (cl_mem.phys + tile_alloc_offset) as u32,
        tiles_x,
        tiles_y,
        true,
    );
    clean_cache(cl_mem.ptr as usize, cl_off + rcl_len);
    core.flush_caches();
    let seqno = core.submit_frame(
        Some((
            cl_mem.phys as u32,
            (cl_mem.phys + bcl_len) as u32,
        )),
        (
            (cl_mem.phys + cl_off) as u32,
            (cl_mem.phys + cl_off + rcl_len) as u32,
        ),
    );
    let ok2 = wait_v3d(&mut core, seqno, "clear job (bin+render)");
    ok && ok2
}

fn test_hvs() -> bool {
    println!("== HVS + HDMI ==");
    let fb = match alloc_dma(FB_SIZE) {
        Ok(m) => m,
        Err(e) => {
            println!("  FAIL: fb alloc: {e}");
            return false;
        }
    };
    let dlist = match alloc_dma(0x1000) {
        Ok(m) => m,
        Err(e) => {
            println!("  FAIL: dlist alloc: {e}");
            return false;
        }
    };

    // Test pattern: horizontal gradient.
    let slice = unsafe { std::slice::from_raw_parts_mut(fb.ptr, fb.size) };
    for y in 0..FB_HEIGHT {
        for x in 0..FB_WIDTH {
            let i = (y as usize * FB_WIDTH as usize + x as usize) * 4;
            slice[i + 0] = (x * 255 / FB_WIDTH) as u8; // B
            slice[i + 1] = (y * 255 / FB_HEIGHT) as u8; // G
            slice[i + 2] = ((x + y) * 255 / (FB_WIDTH + FB_HEIGHT)) as u8; // R
            slice[i + 3] = 0xff;
        }
    }
    clean_cache(fb.ptr as usize, fb.size);

    let display = match HvsDisplay::new(
        HvsResources {
            hvs_base: HVS_BASE,
            hdmi_base: HDMI_BASE,
            fb_bus_addr: fb.phys as u32,
            fb_vaddr: fb.ptr as usize,
            fb_size: fb.size,
            dlist_bus_addr: dlist.phys as u32,
            dlist_vaddr: dlist.ptr as usize,
        },
        HvsDisplayConfig::xrgb8888(FB_WIDTH, FB_HEIGHT),
    ) {
        Ok(d) => d,
        Err(e) => {
            println!("  FAIL: HvsDisplay::new: {e}");
            return false;
        }
    };
    println!(
        "  display up: {}x{} stride {} (check the screen for a gradient)",
        display.info().width,
        display.info().height,
        display.info().stride
    );
    let mut display = display;
    display.flush();
    println!("  flush OK (HDMI should now show the pattern)");
    true
}

fn main() {
    println!("bcm283x host test on {}", std::env::consts::ARCH);
    match mmap_identity(PERIPHERAL_BASE, PERIPHERAL_SIZE) {
        Ok(base) if base == PERIPHERAL_BASE => {
            println!("peripheral window identity-mapped at 0x{base:x}")
        }
        Ok(base) => {
            println!("FAIL: identity map landed at 0x{base:x}, expected 0x{PERIPHERAL_BASE:x}");
            return;
        }
        Err(e) => {
            println!("FAIL: {e}");
            return;
        }
    }

    let v3d_ok = test_v3d();
    let hvs_ok = test_hvs();

    println!(
        "\nRESULT: V3D {} | HVS {}",
        if v3d_ok { "PASS" } else { "FAIL" },
        if hvs_ok { "PASS" } else { "FAIL" }
    );
}
