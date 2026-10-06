// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/intel_engine_cs.c:
// __intel_engine_stop_cs, intel_engine_stop_cs, __cs_pending_mi_force_wakes,
// __gpm_wait_for_fw_complete (selected BCS/RCS only).
// Copyright © 2016 Intel Corporation.
// gt/intel_reset.c: gen8_engine_reset_{prepare,cancel}, gen6_hw_domain_reset,
// Gen12 branch of gen8_reset_engines (BCS/RCS, no SFC/media engine).
// Copyright © 2008-2018 Intel Corporation. Full MIT grant: ../LICENSE-MIT.
// Source timeouts/WA/double reset are retained; error handling is fail-closed.
use crate::{Error, GtIo, masked_disable, masked_enable, wait};
pub const BCS: u32 = 0x22000;

const GDRST: u32 = 0x941c;
/// Forcewake must be held by the sole owner. No further submission enters until
/// reset finishes. No global GT/display reset or firmware-PTE mutation occurs.
pub fn stop_and_reset_bcs(io: &impl GtIo) -> Result<(), Error> {
    stop_and_reset(io, BCS, 1 << 2, 0x800c)
}
/// Exact N305 RCS domain; caller additionally owns render wake and excludes
/// firmware/GuC consumers. Never reset a shared render domain concurrently.
pub fn stop_and_reset_rcs(io: &impl GtIo) -> Result<(), Error> {
    stop_and_reset(io, 0x2000, 1 << 1, 0x8000)
}
fn stop_and_reset(io: &impl GtIo, base: u32, domain: u32, idle: u32) -> Result<(), Error> {
    let mi_mode = base + 0x9c;
    let mode = base + 0x29c;
    let reset_ctl = base + 0xd0;
    let prepared = (|| {
        io.write(mi_mode, masked_enable(1 << 8))?;
        // Wa_22011802037: stop CS prefetch and complete pending MI_FORCE_WAKE.
        io.write(mode, masked_enable(1 << 10))?;
        if let Err(e) = wait(io, mi_mode, 1 << 9, 1 << 9, 101_000) {
            if !matches!(e, Error::Timeout(_)) {
                return Err(e);
            }
            if io.read(base + 0x34)? & 0x001ffffc != io.read(base + 0x30)? & 0x001ffff8 {
                return Err(e);
            }
        }
        io.read(mi_mode)?; // posting read before checking GPM handshakes.
        let idle = io.read(idle)?;
        let pending = (idle & (idle >> 16) & (31 << 9)) >> 9;
        if pending != 0 {
            io.delay_us(1);
            let result = wait(io, 0xa2a0, pending, pending, 5000);
            io.delay_us(1);
            result?;
        }
        let status = io.read(reset_ctl)?;
        let (request, mask, value) = if status & (1 << 2) != 0 {
            (1 << 2, 1 << 2, 0)
        } else if status & (1 << 1) == 0 {
            (1, 1 << 1, 1 << 1)
        } else {
            (0, 0, 0)
        };
        if request != 0 {
            io.write(reset_ctl, masked_enable(request))?;
            wait(io, reset_ctl, mask, value, 700)?;
        }
        // ADL-N (<12.70) repeats the engine-domain reset, then waits50us.
        let result = (|| {
            for _ in 0..2 {
                io.write(GDRST, domain)?;
                wait(io, GDRST, domain, 0, 2000)?;
            }
            Ok(())
        })();
        io.delay_us(50);
        result
    })();
    // Source cancels request even after reset/prepare failures. STOP_RING and
    // prefetch-disable remain until the context/ring setup explicitly resumes.
    let cancel = io.write(reset_ctl, masked_disable(1));
    if cancel.is_err() || io.read(reset_ctl).ok().map(|v| v & 1) != Some(0) {
        return Err(Error::Quarantined);
    }
    prepared
}
