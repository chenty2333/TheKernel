//! V3D (VideoCore IV 3D engine) register map, BCM2835/6/7.
//!
//! The engine lives at ARM-view physical `0x3fc0_0000` (GPU view
//! `0x7ec0_0000`). Register offsets follow the Broadcom V3D register
//! specification; bit definitions match the upstream Linux `vc4` driver's
//! `vc4_regs.h`.

/// ARM-view physical base of the V3D block.
pub const V3D_BASE: usize = 0x3fc0_0000;
/// V3D interrupt (armctrl bank 1, bit 10).
pub const V3D_IRQ: usize = 42;

pub const V3D_IDENT0: usize = 0x000;
pub const V3D_IDENT1: usize = 0x004;
pub const V3D_L2CACTL: usize = 0x020;
pub const V3D_SLCACTL: usize = 0x024;
pub const V3D_INTCTL: usize = 0x030;
pub const V3D_INTENA: usize = 0x034;
pub const V3D_INTDIS: usize = 0x038;

pub const V3D_CT0CS: usize = 0x100;
pub const V3D_CTNCS: usize = 0x100;
pub const V3D_CT0EA: usize = 0x108;
pub const V3D_CTNEA: usize = 0x108;
pub const V3D_CT0CA: usize = 0x110;
pub const V3D_CTNCA: usize = 0x110;
pub const V3D_CT00RA0: usize = 0x118;
pub const V3D_CT01RA0: usize = 0x11c;

pub const V3D_BPOA: usize = 0x308;
pub const V3D_BPOS: usize = 0x30c;
pub const V3D_VPMBASE: usize = 0x504;

/// Expected `V3D_IDENT0` value: version 2 and the ASCII "V3D" magic.
pub const V3D_EXPECTED_IDENT0: u32 = (2 << 24) | ('V' as u32) | ('3' as u32) << 8 | ('D' as u32) << 16;

pub const V3D_INT_OUTOMEM: u32 = 1 << 2;
pub const V3D_INT_FLDONE: u32 = 1 << 1;
pub const V3D_INT_FRDONE: u32 = 1 << 0;
pub const V3D_DRIVER_IRQS: u32 = V3D_INT_OUTOMEM | V3D_INT_FLDONE | V3D_INT_FRDONE;

pub const V3D_L2CACTL_L2CCLR: u32 = 1 << 2;

pub const V3D_SLCACTL_T1CC: u32 = 0xf << 24;
pub const V3D_SLCACTL_T0CC: u32 = 0xf << 16;
pub const V3D_SLCACTL_UCC: u32 = 0xf << 8;
pub const V3D_SLCACTL_ICC: u32 = 0xf << 0;

// ---------------------------------------------------------------------------
// Command list (CL) packet opcodes and render control list (RCL) packets.
// ---------------------------------------------------------------------------

pub const PACKET_HALT: u8 = 0;
pub const PACKET_NOP: u8 = 1;
pub const PACKET_FLUSH: u8 = 4;
pub const PACKET_FLUSH_ALL: u8 = 5;
pub const PACKET_START_TILE_BINNING: u8 = 6;
pub const PACKET_INCREMENT_SEMAPHORE: u8 = 7;
pub const PACKET_WAIT_ON_SEMAPHORE: u8 = 8;
pub const PACKET_BRANCH: u8 = 16;
pub const PACKET_BRANCH_TO_SUB_LIST: u8 = 17;
pub const PACKET_STORE_MS_TILE_BUFFER: u8 = 24;
pub const PACKET_STORE_MS_TILE_BUFFER_AND_EOF: u8 = 25;
pub const PACKET_STORE_FULL_RES_TILE_BUFFER: u8 = 26;
pub const PACKET_LOAD_FULL_RES_TILE_BUFFER: u8 = 27;
pub const PACKET_STORE_TILE_BUFFER_GENERAL: u8 = 28;
pub const PACKET_LOAD_TILE_BUFFER_GENERAL: u8 = 29;
pub const PACKET_TILE_BINNING_MODE_CONFIG: u8 = 112;
pub const PACKET_TILE_RENDERING_MODE_CONFIG: u8 = 113;
pub const PACKET_CLEAR_COLORS: u8 = 114;
pub const PACKET_TILE_COORDINATES: u8 = 115;

/// Tile dimensions used by the binner (32x32 unless configured otherwise).
pub const TILE_SIZE: u32 = 32;

/// Color buffer format for `CLEAR_COLORS` / tile buffer store: RGBA8888.
pub const RCL_COLOR_FORMAT_RGBA8888: u32 = 4;

// ---------------------------------------------------------------------------
// CPRMAN (clock manager) registers for the V3D clock.
// ---------------------------------------------------------------------------

pub const CPRMAN_PADDR: usize = 0x3f10_1000;
pub const CM_V3DCTL: usize = 0x038;
pub const CM_V3DDIV: usize = 0x03c;

/// Broadcom write-protection password for CPRMAN/PM registers.
pub const PM_PASSWORD: u32 = 0x5a00_0000;

pub const CM_CTL_ENAB: u32 = 1 << 4;
pub const CM_CTL_SRC_SHIFT: u32 = 12;
pub const CM_CTL_MASH_SHIFT: u32 = 9;
pub const CM_CTL_BUSY: u32 = 1 << 7;

/// V3D clock source: PLLD.
pub const CM_SRC_PLLD: u32 = 4;

/// V3D clock divider integer bits (bits 12-19 of the DIV register).
pub const CM_DIV_INT_SHIFT: u32 = 12;
