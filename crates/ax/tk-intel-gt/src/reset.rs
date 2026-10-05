// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/intel_engine_cs.c:
// __intel_engine_stop_cs, intel_engine_stop_cs, __cs_pending_mi_force_wakes,
// __gpm_wait_for_fw_complete (BCS-only).
// Copyright © 2016 Intel Corporation.
// gt/intel_reset.c: gen8_engine_reset_{prepare,cancel}, gen6_hw_domain_reset,
// Gen12 branch of gen8_reset_engines (BCS, no SFC/media engine).
// Copyright © 2008-2018 Intel Corporation. Full MIT grant: ../LICENSE-MIT.
// Source timeouts/WA/double reset are retained; error handling is fail-closed.
use crate::{Error, GtIo, masked_disable, masked_enable, wait};
pub const BCS: u32 = 0x22000;
const MI_MODE: u32 = BCS + 0x9c;
const MODE: u32 = BCS + 0x29c;
const RESET_CTL: u32 = BCS + 0xd0;
const GDRST: u32 = 0x941c;
/// Forcewake must be held by the sole owner. No further submission enters until
/// reset finishes. No global GT/display reset or firmware-PTE mutation occurs.
pub fn stop_and_reset_bcs(io: &impl GtIo) -> Result<(), Error> {
    let prepared = (|| {
        io.write(MI_MODE, masked_enable(1 << 8))?;
        // Wa_22011802037: stop CS prefetch and complete pending MI_FORCE_WAKE.
        io.write(MODE, masked_enable(1 << 10))?;
        if let Err(e) = wait(io, MI_MODE, 1 << 9, 1 << 9, 101_000) {
            if !matches!(e, Error::Timeout(_)) {
                return Err(e);
            }
            if io.read(BCS + 0x34)? & 0x001ffffc != io.read(BCS + 0x30)? & 0x001ffff8 {
                return Err(e);
            }
        }
        io.read(MI_MODE)?; // posting read before checking GPM handshakes.
        let idle = io.read(0x800c)?;
        let pending = (idle & (idle >> 16) & (31 << 9)) >> 9;
        if pending != 0 {
            io.delay_us(1);
            let result = wait(io, 0xa2a0, pending, pending, 5000);
            io.delay_us(1);
            result?;
        }
        let status = io.read(RESET_CTL)?;
        let (request, mask, value) = if status & (1 << 2) != 0 {
            (1 << 2, 1 << 2, 0)
        } else if status & (1 << 1) == 0 {
            (1, 1 << 1, 1 << 1)
        } else {
            (0, 0, 0)
        };
        if request != 0 {
            io.write(RESET_CTL, masked_enable(request))?;
            wait(io, RESET_CTL, mask, value, 700)?;
        }
        // ADL-N (<12.70) repeats the engine-domain reset, then waits50us.
        let result = (|| {
            for _ in 0..2 {
                io.write(GDRST, 1 << 2)?;
                wait(io, GDRST, 1 << 2, 0, 2000)?;
            }
            Ok(())
        })();
        io.delay_us(50);
        result
    })();
    // Source cancels request even after reset/prepare failures. STOP_RING and
    // prefetch-disable remain until the context/ring setup explicitly resumes.
    let cancel = io.write(RESET_CTL, masked_disable(1));
    if cancel.is_err() || io.read(RESET_CTL).ok().map(|v| v & 1) != Some(0) {
        return Err(Error::Quarantined);
    }
    prepared
}
