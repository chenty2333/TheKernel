//! Command list builders for the V3D binner and render control lists.
//!
//! The V3D executes two control lists per frame:
//!
//! * the **bin control list** (BCL), run by the binner thread: bins
//!   primitives into per-tile sub-lists and initializes the tile state
//!   (clear colors);
//! * the **render control list** (RCL), run by the render thread: iterates
//!   tiles, branches to the binner's per-tile sub-lists, and stores the
//!   tile buffers to the framebuffer.
//!
//! This module builds both for the "clear" job shape used by the selftest
//! and as building blocks for user-provided command streams.

use crate::regs::*;

/// Returns the number of 32x32 tiles covering `size` pixels.
pub const fn tiles_for(size: u32) -> u32 {
    (size + TILE_SIZE - 1) / TILE_SIZE
}

/// Size in bytes of the tile state array (48 bytes per tile).
pub const fn tile_state_size(tiles_x: u32, tiles_y: u32) -> usize {
    48 * (tiles_x as usize) * (tiles_y as usize)
}

/// Size in bytes of one tile's bin list header slot (32 bytes per tile).
pub const fn tile_alloc_size(tiles_x: u32, tiles_y: u32) -> usize {
    32 * (tiles_x as usize) * (tiles_y as usize)
}

/// Binner flags: auto-init the tile state array and use 32-byte initial /
/// 128-byte overflow allocation blocks (matching the kernel driver).
pub const BIN_CONFIG_FLAGS: u8 = (2 << 5) | (0 << 3) | (1 << 2); // BLOCK_128 | INIT_32 | AUTO_INIT_TSDA

/// Builds the bin control list for a frame with no draw calls (clear only).
///
/// `tile_state_addr` / `tile_alloc_addr` are the GPU bus addresses of the
/// tile state array and the tile alloc buffer; the binner writes the clear
/// state and empty sub-lists there.
pub fn build_clear_bcl(
    bcl: &mut [u8],
    tile_state_addr: u32,
    tile_alloc_addr: u32,
    tile_alloc_size: u32,
    tiles_x: u32,
    tiles_y: u32,
) -> usize {
    let mut p = 0usize;
    // START_TILE_BINNING
    bcl[p] = PACKET_START_TILE_BINNING;
    p += 1;
    // TILE_BINNING_MODE_CONFIG
    bcl[p] = PACKET_TILE_BINNING_MODE_CONFIG;
    p += 1;
    bcl[p..p + 4].copy_from_slice(&tile_alloc_addr.to_le_bytes());
    p += 4;
    bcl[p..p + 4].copy_from_slice(&tile_alloc_size.to_le_bytes());
    p += 4;
    bcl[p..p + 4].copy_from_slice(&tile_state_addr.to_le_bytes());
    p += 4;
    bcl[p] = tiles_x as u8;
    p += 1;
    bcl[p] = tiles_y as u8;
    p += 1;
    bcl[p] = BIN_CONFIG_FLAGS;
    p += 1;
    // The bin CL must end with INCREMENT_SEMAPHORE then FLUSH.
    bcl[p] = PACKET_INCREMENT_SEMAPHORE;
    p += 1;
    bcl[p] = PACKET_FLUSH;
    p += 1;
    p
}

/// Builds the render control list for a binned frame.
///
/// `tile_alloc_addr` is the GPU bus address of the tile alloc buffer; the
/// per-tile sub-list header for tile `(x, y)` sits at
/// `tile_alloc_addr + (y * tiles_x + x) * 32`. The last tile ends with
/// `STORE_MS_TILE_BUFFER_AND_EOF` and the list closes with `HALT`.
pub fn build_bin_rcl(
    rcl: &mut [u8],
    tile_alloc_addr: u32,
    tiles_x: u32,
    tiles_y: u32,
    has_bin: bool,
) -> usize {
    let mut p = 0usize;
    for y in 0..tiles_y {
        for x in 0..tiles_x {
            // TILE_COORDINATES
            rcl[p] = PACKET_TILE_COORDINATES;
            rcl[p + 1] = x as u8;
            rcl[p + 2] = y as u8;
            p += 3;
            // The first tile waits for the binner's semaphore increment.
            if has_bin && x == 0 && y == 0 {
                rcl[p] = PACKET_WAIT_ON_SEMAPHORE;
                p += 1;
            }
            if has_bin {
                // BRANCH_TO_SUB_LIST: run the binner's per-tile list.
                rcl[p] = PACKET_BRANCH_TO_SUB_LIST;
                p += 1;
                let sub_addr = tile_alloc_addr + (y * tiles_x + x) * 32;
                rcl[p..p + 4].copy_from_slice(&sub_addr.to_le_bytes());
                p += 4;
            }
            // STORE_MS_TILE_BUFFER (or ..._AND_EOF on the last tile).
            let last = x == tiles_x - 1 && y == tiles_y - 1;
            rcl[p] = if last {
                PACKET_STORE_MS_TILE_BUFFER_AND_EOF
            } else {
                PACKET_STORE_MS_TILE_BUFFER
            };
            p += 1;
        }
    }
    rcl[p] = PACKET_HALT;
    p + 1
}

/// Builds a trivial control list that immediately halts; used to verify the
/// submission and interrupt path without touching memory.
pub fn build_halt_cl(cl: &mut [u8]) -> usize {
    cl[0] = PACKET_HALT;
    1
}
