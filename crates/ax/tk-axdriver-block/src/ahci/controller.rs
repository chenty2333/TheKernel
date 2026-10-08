//! AHCI controller reset and setup routines.
//!
//! This ports the early controller functions from FreeBSD
//! `sys/dev/ahci/ahci.c`; port/channel transaction functions follow in the
//! same source order. The I/O trait maps bus-space register access and delay
//! to the platform's MMIO implementation.

use super::regs::*;

/// Minimal platform boundary for an AHCI register aperture.
pub trait AhciIo {
    /// Read one little-endian 32-bit AHCI register.
    fn read32(&mut self, offset: usize) -> u32;
    /// Write one little-endian 32-bit AHCI register.
    fn write32(&mut self, offset: usize, value: u32);
    /// Busy-wait for at least the supplied number of microseconds.
    fn delay_us(&mut self, micros: u32);
}

/// Controller-wide state needed by the upstream setup/reset paths.
pub struct AhciController<I> {
    io: I,
    pub capabilities: u32,
    pub capabilities2: u32,
    pub quirks: u32,
    pub ccc: u16,
    pub ccc_vector: u8,
}

impl<I: AhciIo> AhciController<I> {
    /// Create the controller state over an already mapped MMIO aperture.
    pub const fn new(io: I, capabilities: u32, capabilities2: u32, quirks: u32) -> Self {
        Self {
            io,
            capabilities,
            capabilities2,
            quirks,
            ccc: 0,
            ccc_vector: 0,
        }
    }

    /// Borrow the MMIO adapter for a following channel-attach stage.
    pub fn io_mut(&mut self) -> &mut I {
        &mut self.io
    }

    /// Move the MMIO adapter out after the controller has been detached.
    pub fn into_io(self) -> I {
        self.io
    }

    /// FreeBSD `ahci_ch_detval`: preserve the requested DET except when PHY is
    /// explicitly disabled for this channel.
    // upstream: ahci.c ahci_ch_detval()
    pub const fn ahci_ch_detval(disable_phy: bool, value: u32) -> u32 {
        if disable_phy {
            ATA_SC_DET_DISABLE
        } else {
            value
        }
    }

    /// FreeBSD `ahci_ctlr_setup`: clear controller interrupts, optionally
    /// configure command-completion coalescing, then enable global interrupts.
    // upstream: ahci.c ahci_ctlr_setup()
    pub fn ahci_ctlr_setup(&mut self) {
        let status = self.io.read32(AHCI_IS);
        self.io.write32(AHCI_IS, status);
        if self.ccc != 0 {
            let ports = self.io.read32(AHCI_PI);
            self.io.write32(AHCI_CCCP, ports);
            self.io.write32(
                AHCI_CCCC,
                (u32::from(self.ccc) << AHCI_CCCC_TV_SHIFT)
                    | (4 << AHCI_CCCC_CC_SHIFT)
                    | AHCI_CCCC_EN,
            );
            self.ccc_vector =
                ((self.io.read32(AHCI_CCCC) & AHCI_CCCC_INT_MASK) >> AHCI_CCCC_INT_SHIFT) as u8;
        }
        let ghc = self.io.read32(AHCI_GHC);
        self.io.write32(AHCI_GHC, ghc | AHCI_GHC_IE);
    }

    /// FreeBSD `ahci_ctlr_reset`: request BIOS/OS handoff, enter AHCI mode,
    /// reset the HBA, and restore capability bits on quirked controllers.
    // upstream: ahci.c ahci_ctlr_reset()
    pub fn ahci_ctlr_reset(&mut self) -> Result<(), ControllerError> {
        let version = self.io.read32(AHCI_VS);
        if version >= 0x0001_0200 && self.capabilities2 & AHCI_CAP2_BOH != 0 {
            let mut handoff = self.io.read32(AHCI_BOHC);
            if handoff & AHCI_BOHC_OOS == 0 {
                handoff |= AHCI_BOHC_OOS;
                self.io.write32(AHCI_BOHC, handoff);
                // Upstream waits at most 80 * 25ms for BIOS release.
                for _ in 0..80 {
                    self.io.delay_us(25_000);
                    handoff = self.io.read32(AHCI_BOHC);
                    if handoff & AHCI_BOHC_BOS == 0 || handoff & AHCI_BOHC_BB == 0 {
                        break;
                    }
                }
            }
        }

        self.io.write32(AHCI_GHC, AHCI_GHC_AE);
        self.io.write32(AHCI_GHC, AHCI_GHC_AE | AHCI_GHC_HR);
        let mut reset = false;
        for _ in 0..1_000 {
            self.io.delay_us(1_000);
            if self.io.read32(AHCI_GHC) & AHCI_GHC_HR == 0 {
                reset = true;
                break;
            }
        }
        if !reset {
            return Err(ControllerError::ResetTimeout);
        }
        self.io.write32(AHCI_GHC, AHCI_GHC_AE);
        if self.quirks & AHCI_Q_RESTORE_CAP != 0 {
            self.io.write32(AHCI_CAP, self.capabilities);
        }
        Ok(())
    }
}

/// Failure reported by a controller-level reset operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControllerError {
    /// The HBA did not clear GHC.HR within the upstream one-second bound.
    ResetTimeout,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeAhciIo {
        regs: [u32; 64],
        writes: [(usize, u32); 16],
        write_count: usize,
        delays: [u32; 1_024],
        delay_count: usize,
        reset_stuck: bool,
    }

    impl Default for FakeAhciIo {
        fn default() -> Self {
            Self {
                regs: [0; 64],
                writes: [(0, 0); 16],
                write_count: 0,
                delays: [0; 1_024],
                delay_count: 0,
                reset_stuck: false,
            }
        }
    }

    impl AhciIo for FakeAhciIo {
        fn read32(&mut self, offset: usize) -> u32 {
            self.regs[offset / 4]
        }

        fn write32(&mut self, offset: usize, value: u32) {
            self.writes[self.write_count] = (offset, value);
            self.write_count += 1;
            self.regs[offset / 4] =
                if offset == AHCI_GHC && value & AHCI_GHC_HR != 0 && !self.reset_stuck {
                    value & !AHCI_GHC_HR
                } else if offset == AHCI_CCCC {
                    (value & !AHCI_CCCC_INT_MASK) | (self.regs[offset / 4] & AHCI_CCCC_INT_MASK)
                } else {
                    value
                };
        }

        fn delay_us(&mut self, micros: u32) {
            self.delays[self.delay_count] = micros;
            self.delay_count += 1;
        }
    }

    #[test]
    fn channel_det_value_obeys_disable_phy_override() {
        assert_eq!(AhciController::<FakeAhciIo>::ahci_ch_detval(false, 3), 3);
        assert_eq!(
            AhciController::<FakeAhciIo>::ahci_ch_detval(true, 3),
            ATA_SC_DET_DISABLE
        );
    }

    #[test]
    fn setup_clears_status_configures_ccc_and_enables_interrupts() {
        let mut io = FakeAhciIo::default();
        io.regs[AHCI_IS / 4] = 0x15;
        io.regs[AHCI_PI / 4] = 0b101;
        io.regs[AHCI_CCCC / 4] = 3 << AHCI_CCCC_INT_SHIFT;
        let mut ctlr = AhciController::new(io, 0, 0, 0);
        ctlr.ccc = 10;
        ctlr.ahci_ctlr_setup();
        let ccc_vector = ctlr.ccc_vector;
        let io = ctlr.into_io();
        assert!(io.writes[..io.write_count].contains(&(AHCI_IS, 0x15)));
        assert!(io.writes[..io.write_count].contains(&(AHCI_CCCP, 0b101)));
        assert!(io.writes[..io.write_count].contains(&(AHCI_CCCC, (10 << 16) | (4 << 8) | 1)));
        assert!(io.writes[..io.write_count].contains(&(AHCI_GHC, AHCI_GHC_IE)));
        assert_eq!(ccc_vector, 3);
    }

    #[test]
    fn reset_applies_hba_reset_and_capability_restore_quirk() {
        let mut io = FakeAhciIo::default();
        io.regs[AHCI_VS / 4] = 0x0001_0300;
        io.regs[AHCI_GHC / 4] = AHCI_GHC_AE;
        io.regs[AHCI_CAP2 / 4] = AHCI_CAP2_BOH;
        let mut ctlr = AhciController::new(io, 0x1234, AHCI_CAP2_BOH, AHCI_Q_RESTORE_CAP);
        assert_eq!(ctlr.ahci_ctlr_reset(), Ok(()));
        let io = ctlr.into_io();
        assert!(io.writes[..io.write_count].contains(&(AHCI_GHC, AHCI_GHC_AE | AHCI_GHC_HR)));
        assert!(io.writes[..io.write_count].contains(&(AHCI_CAP, 0x1234)));
    }

    #[test]
    fn reset_timeout_is_bounded_to_one_second() {
        let mut io = FakeAhciIo {
            reset_stuck: true,
            ..FakeAhciIo::default()
        };
        io.regs[AHCI_GHC / 4] = AHCI_GHC_AE;
        let mut ctlr = AhciController::new(io, 0, 0, 0);
        assert_eq!(ctlr.ahci_ctlr_reset(), Err(ControllerError::ResetTimeout));
        let io = ctlr.into_io();
        assert_eq!(
            io.delays[..io.delay_count]
                .iter()
                .filter(|&&n| n == 1_000)
                .count(),
            1_000
        );
    }
}
