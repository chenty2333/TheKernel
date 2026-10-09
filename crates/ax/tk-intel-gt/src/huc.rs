// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_huc.c, Gen11+ GuC-auth path.
// Copyright © 2016-2019 Intel Corporation. Full MIT grant: ../LICENSE-MIT.

use crate::{Error, GtIo, guc_fw, uc::FirmwareStatus};

pub const GEN11_HUC_KERNEL_LOAD_INFO: u32 = 0xc1dc;
pub const HUC_LOAD_SUCCESSFUL: u32 = 1 << 0;

/// upstream: intel_huc.c intel_huc_is_authenticated() Gen11+ status path.
pub fn is_authenticated(io: &impl GtIo) -> Result<bool, Error> {
    Ok(io.read(GEN11_HUC_KERNEL_LOAD_INFO)? & HUC_LOAD_SUCCESSFUL != 0)
}

/// upstream: intel_huc.c intel_huc_wait_for_auth_complete().
pub fn wait_for_auth_complete(io: &impl GtIo) -> Result<u32, Error> {
    guc_fw::wait_huc_auth(io)
}

/// upstream: intel_huc.c intel_huc_auth() for legacy Gen12 GuC auth.
pub fn authenticate_by_guc(io: &impl GtIo, rsa_ggtt_offset: u32) -> Result<u32, Error> {
    if rsa_ggtt_offset == 0 || is_authenticated(io)? {
        return Err(Error::Refused);
    }
    guc_fw::authenticate_huc(io, rsa_ggtt_offset)?;
    wait_for_auth_complete(io)
}

/// upstream: intel_huc.c intel_huc_check_status() return values for the
/// legacy Gen12 firmware state (GSC partial-auth mode is not applicable).
pub fn check_status(firmware: FirmwareStatus, authenticated: bool) -> i32 {
    match firmware {
        FirmwareStatus::NotSupported => -19, // ENODEV
        FirmwareStatus::Disabled => -95,     // EOPNOTSUPP
        FirmwareStatus::Missing => -65,      // ENOPKG
        FirmwareStatus::Error => -8,         // ENOEXEC
        FirmwareStatus::InitFail => -12,     // ENOMEM
        FirmwareStatus::LoadFail => -5,      // EIO
        _ if authenticated => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};
    use core::cell::RefCell;

    use super::*;
    const GUC_SEND_BASE: u32 = 0x190240;

    struct Io {
        reads: RefCell<Vec<(u32, u32)>>,
        writes: RefCell<Vec<(u32, u32)>>,
    }
    impl GtIo for Io {
        fn read(&self, _offset: u32) -> Result<u32, Error> {
            Ok(self.reads.borrow_mut().remove(0).1)
        }
        fn write(&self, offset: u32, value: u32) -> Result<(), Error> {
            self.writes.borrow_mut().push((offset, value));
            Ok(())
        }
        fn now_us(&self) -> u64 {
            0
        }
        fn delay_us(&self, _micros: u32) {}
    }

    #[test]
    fn gen12_huc_auth_status_and_getparam_errors_match_upstream() {
        let io = Io {
            reads: RefCell::new(vec![(
                GEN11_HUC_KERNEL_LOAD_INFO,
                HUC_LOAD_SUCCESSFUL | 0x40,
            )]),
            writes: RefCell::new(vec![]),
        };
        assert!(is_authenticated(&io).unwrap());
        assert_eq!(check_status(FirmwareStatus::Missing, false), -65);
        assert_eq!(check_status(FirmwareStatus::LoadFail, true), -5);
        assert_eq!(check_status(FirmwareStatus::Running, true), 1);
        assert_eq!(check_status(FirmwareStatus::Transferred, false), 0);
    }

    #[test]
    fn auth_wrapper_refuses_already_verified_and_runs_action_before_wait() {
        let already = Io {
            reads: RefCell::new(vec![(GEN11_HUC_KERNEL_LOAD_INFO, HUC_LOAD_SUCCESSFUL)]),
            writes: RefCell::new(vec![]),
        };
        assert_eq!(authenticate_by_guc(&already, 0x1200), Err(Error::Refused));
        assert!(already.writes.borrow().is_empty());

        let io = Io {
            reads: RefCell::new(vec![
                (GEN11_HUC_KERNEL_LOAD_INFO, 0),
                (GUC_SEND_BASE, 0),
                (GUC_SEND_BASE, 0xf000_0000),
                (GEN11_HUC_KERNEL_LOAD_INFO, HUC_LOAD_SUCCESSFUL),
            ]),
            writes: RefCell::new(vec![]),
        };
        assert_eq!(authenticate_by_guc(&io, 0x1200), Ok(HUC_LOAD_SUCCESSFUL));
    }
}
