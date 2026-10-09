//! AHCI controller reset and setup routines.
//!
//! This ports the early controller functions from FreeBSD
//! `sys/dev/ahci/ahci.c`; port/channel transaction functions follow in the
//! same source order. The I/O trait maps bus-space register access and delay
//! to the platform's MMIO implementation.

use super::regs::*;

/// Minimal platform boundary for an AHCI register aperture.
pub trait AhciIo: Send + Sync {
    /// Read one little-endian 32-bit AHCI register.
    fn read32(&mut self, offset: usize) -> u32;
    /// Write one little-endian 32-bit AHCI register.
    fn write32(&mut self, offset: usize, value: u32);
    /// Busy-wait for at least the supplied number of microseconds.
    fn delay_us(&mut self, micros: u32);

    /// Whether the PCI binding admitted MSI-X, MSI, or routed INTx.
    fn has_interrupt(&self) -> bool {
        false
    }

    /// Current interrupt completion generation, when a message route exists.
    fn interrupt_generation(&self) -> Option<u64> {
        None
    }

    /// Wait for a bounded period for completion progress, or delay when no
    /// task-context interrupt waiter is available.
    fn wait_for_interrupt(&mut self, observed: u64, timeout_us: u64) {
        let _ = observed;
        self.delay_us(timeout_us.min(u64::from(u32::MAX)) as u32);
    }

    /// Install a bounded task-wake callback on the retained PCI endpoint.
    fn install_completion_notifier(
        &mut self,
        _notifier: Option<crate::BlockCompletionNotifier>,
        _context: usize,
    ) -> bool {
        false
    }
}

/// Controller-wide state needed by the upstream setup/reset paths.
pub struct AhciController<I> {
    io: I,
    pub capabilities: u32,
    pub capabilities2: u32,
    pub quirks: u32,
    pub ccc: u16,
    pub ccc_vector: u8,
    pub version: u32,
    pub enclosure_capabilities: u32,
    pub enclosure_location: u32,
    pub implemented_ports: u32,
    pub num_channels: u8,
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
            version: 0,
            enclosure_capabilities: 0,
            enclosure_location: 0,
            implemented_ports: 0,
            num_channels: 0,
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

    /// FreeBSD `ahci_attach`: discover HBA version/capabilities, apply the
    /// channel-count quirks, and record the implemented-port topology.
    ///
    /// The bus-resource manager, DMA-tag creation, interrupt allocation,
    /// child-device attachment, and verbose diagnostics are platform services
    /// and are handled by the binding layer; this method ports the register and
    /// capability state transitions from the function body.
    // upstream: ahci.c ahci_attach()
    pub fn ahci_attach(&mut self, requested_ccc_ms: u16) {
        self.ccc = requested_ccc_ms;
        self.version = self.io.read32(AHCI_VS);
        self.capabilities = self.io.read32(AHCI_CAP);
        if self.version >= 0x0001_0200 {
            self.capabilities2 = self.io.read32(AHCI_CAP2);
        } else {
            self.capabilities2 = 0;
        }
        if self.capabilities & AHCI_CAP_EMS != 0 {
            self.enclosure_capabilities = self.io.read32(AHCI_EM_CTL);
        } else {
            self.enclosure_capabilities = 0;
        }

        if self.quirks & AHCI_Q_FORCE_PI != 0 {
            let num_ports = (self.capabilities & AHCI_CAP_NPMASK) + 1;
            let port_mask = if num_ports >= 32 {
                u32::MAX
            } else {
                (1u32 << num_ports) - 1
            };
            self.io.write32(AHCI_PI, port_mask);
        }
        self.implemented_ports = self.io.read32(AHCI_PI);

        if self.quirks & AHCI_Q_ALTSIG != 0 && self.capabilities & AHCI_CAP_SPM == 0 {
            self.quirks |= AHCI_Q_NOBSYRES;
        }
        if self.quirks & AHCI_Q_1CH != 0 {
            self.capabilities &= !AHCI_CAP_NPMASK;
            self.implemented_ports &= 0x01;
        }
        if self.quirks & AHCI_Q_2CH != 0 {
            self.capabilities = (self.capabilities & !AHCI_CAP_NPMASK) | 1;
            self.implemented_ports &= 0x03;
        }
        if self.quirks & AHCI_Q_4CH != 0 {
            self.capabilities = (self.capabilities & !AHCI_CAP_NPMASK) | 3;
            self.implemented_ports &= 0x0f;
        }
        let implemented_count = 32 - self.implemented_ports.leading_zeros();
        let capability_count = (self.capabilities & AHCI_CAP_NPMASK) + 1;
        self.num_channels = implemented_count.max(capability_count) as u8;
        if self.quirks & AHCI_Q_NOPMP != 0 {
            self.capabilities &= !AHCI_CAP_SPM;
        }
        if self.quirks & AHCI_Q_NONCQ != 0 {
            self.capabilities &= !AHCI_CAP_SNCQ;
        }
        if self.capabilities & AHCI_CAP_CCCS == 0 {
            self.ccc = 0;
        }
        self.enclosure_location = self.io.read32(AHCI_EM_LOC);
        // FreeBSD creates the DMA tag immediately before this point. DMA
        // ownership is supplied by the TheKernel binding, then the same
        // interrupt/status/CCC setup is applied here.
        self.ahci_ctlr_setup();
    }

    /// FreeBSD `ahci_start`: prepare error/interrupt state, optional FBS, and
    /// start command processing on one port.
    // upstream: ahci.c ahci_start()
    pub fn ahci_start(&mut self, port: &mut PortState, fbs: bool) {
        // The upstream optional `ch->start` hook has no registered callback
        // in this port yet; this snapshot has no caller that installs one.
        let base = port.register_base();
        self.io.write32(base + AHCI_P_SERR, u32::MAX);
        self.io.write32(base + AHCI_P_IS, u32::MAX);
        if port.channel_capabilities & AHCI_P_CMD_FBSCP != 0 {
            port.fbs_enabled = fbs && port.pm_present;
            self.io.write32(
                base + AHCI_P_FBS,
                if port.fbs_enabled { AHCI_P_FBS_EN } else { 0 },
            );
        }
        let command = self.io.read32(base + AHCI_P_CMD) & !AHCI_P_CMD_PMA;
        self.io.write32(
            base + AHCI_P_CMD,
            command | AHCI_P_CMD_ST | if port.pm_present { AHCI_P_CMD_PMA } else { 0 },
        );
    }

    /// FreeBSD `ahci_stop`: stop command issue and wait at most 500.02ms for CR
    /// to clear, while retiring the channel's error-slot bitmap.
    // upstream: ahci.c ahci_stop()
    pub fn ahci_stop(&mut self, port: &mut PortState) -> bool {
        let base = port.register_base();
        let command = self.io.read32(base + AHCI_P_CMD);
        self.io.write32(base + AHCI_P_CMD, command & !AHCI_P_CMD_ST);
        let mut stopped = false;
        for timeout in 0..=50_001 {
            self.io.delay_us(10);
            if timeout > 50_000 {
                break;
            }
            if self.io.read32(base + AHCI_P_CMD) & AHCI_P_CMD_CR == 0 {
                stopped = true;
                break;
            }
        }
        port.error_slots = 0;
        stopped
    }

    /// FreeBSD `ahci_clo`: issue Command List Override when the HBA advertises
    /// SCLO and wait for the command bit to self-clear.
    // upstream: ahci.c ahci_clo()
    pub fn ahci_clo(&mut self, port: &PortState) -> bool {
        if self.capabilities & AHCI_CAP_SCLO == 0 {
            return true;
        }
        let base = port.register_base();
        let command = self.io.read32(base + AHCI_P_CMD) | AHCI_P_CMD_CLO;
        self.io.write32(base + AHCI_P_CMD, command);
        for timeout in 0..=50_001 {
            self.io.delay_us(10);
            if timeout > 50_000 {
                return false;
            }
            if self.io.read32(base + AHCI_P_CMD) & AHCI_P_CMD_CLO == 0 {
                return true;
            }
        }
        false
    }

    /// FreeBSD `ahci_stop_fr`: stop FIS reception and wait for FR to clear.
    // upstream: ahci.c ahci_stop_fr()
    pub fn ahci_stop_fr(&mut self, port: &PortState) -> bool {
        let base = port.register_base();
        let command = self.io.read32(base + AHCI_P_CMD);
        self.io
            .write32(base + AHCI_P_CMD, command & !AHCI_P_CMD_FRE);
        for timeout in 0..=50_001 {
            self.io.delay_us(10);
            if timeout > 50_000 {
                return false;
            }
            if self.io.read32(base + AHCI_P_CMD) & AHCI_P_CMD_FR == 0 {
                return true;
            }
        }
        false
    }

    /// FreeBSD `ahci_start_fr`: enable FIS reception for one port.
    // upstream: ahci.c ahci_start_fr()
    pub fn ahci_start_fr(&mut self, port: &PortState) {
        let base = port.register_base();
        let command = self.io.read32(base + AHCI_P_CMD) | AHCI_P_CMD_FRE;
        self.io.write32(base + AHCI_P_CMD, command);
    }

    /// FreeBSD `ahci_wait_ready`: poll BSY/DRQ with the supplied millisecond
    /// bound and return the elapsed poll count for diagnostics.
    // upstream: ahci.c ahci_wait_ready()
    pub fn ahci_wait_ready(
        &mut self,
        port: &PortState,
        timeout_ms: u32,
        offset_ms: u32,
    ) -> Result<u32, ReadyTimeout> {
        let base = port.register_base();
        let mut elapsed = 0;
        loop {
            let task_file = self.io.read32(base + AHCI_P_TFD);
            if task_file & (ATA_S_BUSY | ATA_S_DRQ) == 0 {
                return Ok(elapsed + offset_ms);
            }
            if elapsed > timeout_ms {
                return Err(ReadyTimeout {
                    elapsed_ms: elapsed + offset_ms,
                    task_file,
                });
            }
            self.io.delay_us(1_000);
            elapsed += 1;
        }
    }

    /// FreeBSD `ahci_sata_connect`: wait for an active PHY and clear SERR after
    /// a successful link. `false` distinguishes the source's nonfatal no-link
    /// outcome from a controller I/O error.
    // upstream: ahci.c ahci_sata_connect()
    pub fn ahci_sata_connect(&mut self, port: &PortState) -> bool {
        let base = port.register_base();
        let timeout_slot = if port.quirks & AHCI_Q_SLOWDEV != 0 {
            5_000
        } else {
            1_000
        };
        let mut found = false;
        for timeout in 0..timeout_slot {
            let status = self.io.read32(base + AHCI_P_SSTS);
            if status & ATA_SS_DET_MASK != ATA_SS_DET_NO_DEVICE {
                found = true;
            }
            if status & ATA_SS_DET_MASK == ATA_SS_DET_PHY_ONLINE
                && status & ATA_SS_SPD_MASK != ATA_SS_SPD_NO_SPEED
                && status & ATA_SS_IPM_MASK == ATA_SS_IPM_ACTIVE
            {
                self.io.write32(base + AHCI_P_SERR, u32::MAX);
                return true;
            }
            if status & ATA_SS_DET_MASK == ATA_SS_DET_PHY_OFFLINE {
                return false;
            }
            if !found && timeout >= 100 {
                break;
            }
            self.io.delay_us(100);
        }
        false
    }

    /// FreeBSD `ahci_sata_phy_reset`: assert/deassert DET reset at the selected
    /// SATA generation, disable partial/slumber during reset, and handle the
    /// no-link listening/PHY-disable fallbacks.
    // upstream: ahci.c ahci_sata_phy_reset()
    pub fn ahci_sata_phy_reset(&mut self, port: &mut PortState) -> bool {
        let base = port.register_base();
        if port.listening {
            let command = self.io.read32(base + AHCI_P_CMD) | AHCI_P_CMD_SUD;
            self.io.write32(base + AHCI_P_CMD, command);
            port.listening = false;
        }
        let revision = port.user_revision[if port.pm_present { 15 } else { 0 }];
        let speed = match revision {
            1 => ATA_SC_SPD_SPEED_GEN1,
            2 => ATA_SC_SPD_SPEED_GEN2,
            3 => ATA_SC_SPD_SPEED_GEN3,
            _ => 0,
        };
        let det = Self::ahci_ch_detval(port.disable_phy, ATA_SC_DET_RESET);
        self.io.write32(
            base + AHCI_P_SCTL,
            det | speed | ATA_SC_IPM_DIS_PARTIAL | ATA_SC_IPM_DIS_SLUMBER,
        );
        self.io.delay_us(1_000);
        let det = Self::ahci_ch_detval(port.disable_phy, ATA_SC_DET_IDLE);
        let ipm = if port.pm_level > 0 {
            0
        } else {
            ATA_SC_IPM_DIS_PARTIAL | ATA_SC_IPM_DIS_SLUMBER
        };
        self.io.write32(base + AHCI_P_SCTL, det | speed | ipm);
        if self.ahci_sata_connect(port) {
            return true;
        }
        if self.capabilities & AHCI_CAP_SSS != 0 {
            let command = self.io.read32(base + AHCI_P_CMD) & !AHCI_P_CMD_SUD;
            self.io.write32(base + AHCI_P_CMD, command);
            port.listening = true;
        } else if port.pm_level > 0 {
            self.io.write32(base + AHCI_P_SCTL, ATA_SC_DET_DISABLE);
        }
        false
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
    // upstream: ahci_pci.c ahci_pci_ctlr_reset()
    pub fn ahci_ctlr_reset(&mut self) -> Result<(), ControllerError> {
        let version = self.io.read32(AHCI_VS);
        let capabilities2 = if version >= 0x0001_0200 {
            self.io.read32(AHCI_CAP2)
        } else {
            0
        };
        if version >= 0x0001_0200 && capabilities2 & AHCI_CAP2_BOH != 0 {
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

/// Per-port state consumed by the upstream start/stop/reset routines.
#[derive(Clone, Debug)]
pub struct PortState {
    pub index: u8,
    pub channel_capabilities: u32,
    pub quirks: u32,
    pub pm_present: bool,
    pub fbs_enabled: bool,
    pub listening: bool,
    pub disable_phy: bool,
    pub pm_level: i32,
    pub error_slots: u32,
    pub user_revision: [i32; 16],
}

impl PortState {
    pub const fn new(index: u8) -> Self {
        Self {
            index,
            channel_capabilities: 0,
            quirks: 0,
            pm_present: false,
            fbs_enabled: false,
            listening: false,
            disable_phy: false,
            pm_level: 0,
            error_slots: 0,
            user_revision: [0; 16],
        }
    }

    pub(crate) const fn register_base(&self) -> usize {
        AHCI_OFFSET + self.index as usize * AHCI_STEP
    }
}

/// Diagnostic state when `ahci_wait_ready` exhausts its polling interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadyTimeout {
    pub elapsed_ms: u32,
    pub task_file: u32,
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
        regs: [u32; 128],
        writes: [(usize, u32); 16],
        write_count: usize,
        delays: [u32; 1_024],
        delay_count: usize,
        reset_stuck: bool,
    }

    impl Default for FakeAhciIo {
        fn default() -> Self {
            Self {
                regs: [0; 128],
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
    #[test]
    fn attach_reads_version_and_resolves_port_quirks() {
        let mut io = FakeAhciIo::default();
        io.regs[AHCI_VS / 4] = 0x0001_0200;
        io.regs[AHCI_CAP / 4] = AHCI_CAP_CCCS | AHCI_CAP_SPM | 3;
        io.regs[AHCI_CAP2 / 4] = AHCI_CAP2_BOH;
        io.regs[AHCI_PI / 4] = 0b1011;
        io.regs[AHCI_EM_LOC / 4] = 0x1234;
        let mut ctlr = AhciController::new(io, 0, 0, AHCI_Q_2CH | AHCI_Q_NONCQ);
        ctlr.ahci_attach(20);
        assert_eq!(ctlr.version, 0x0001_0200);
        assert_eq!(ctlr.capabilities2, AHCI_CAP2_BOH);
        assert_eq!(ctlr.implemented_ports, 0b0011);
        assert_eq!(ctlr.num_channels, 2);
        assert_eq!(ctlr.ccc, 20);
        assert_eq!(ctlr.enclosure_location, 0x1234);
        assert_eq!(ctlr.capabilities & AHCI_CAP_SNCQ, 0);
    }

    #[test]
    fn attach_forces_pi_without_overflowing_at_32_ports() {
        let mut io = FakeAhciIo::default();
        io.regs[AHCI_CAP / 4] = AHCI_CAP_NPMASK;
        let mut ctlr = AhciController::new(io, 0, 0, AHCI_Q_FORCE_PI);
        ctlr.ahci_attach(0);
        assert_eq!(ctlr.implemented_ports, u32::MAX);
        assert!(ctlr.io.writes[..ctlr.io.write_count].contains(&(AHCI_PI, u32::MAX)));
        assert_eq!(ctlr.num_channels, 32);
    }
    #[test]
    fn start_clears_errors_sets_fbs_and_starts_command_engine() {
        let mut io = FakeAhciIo::default();
        io.regs[(AHCI_OFFSET + AHCI_P_CMD) / 4] = AHCI_P_CMD_PMA;
        let mut ctlr = AhciController::new(io, 0, 0, 0);
        let mut port = PortState::new(0);
        port.channel_capabilities = AHCI_P_CMD_FBSCP;
        port.pm_present = true;
        ctlr.ahci_start(&mut port, true);
        assert!(port.fbs_enabled);
        let io = ctlr.into_io();
        assert!(io.writes[..io.write_count].contains(&(AHCI_OFFSET + AHCI_P_SERR, u32::MAX)));
        assert!(io.writes[..io.write_count].contains(&(AHCI_OFFSET + AHCI_P_FBS, AHCI_P_FBS_EN)));
        assert!(
            io.writes[..io.write_count]
                .contains(&(AHCI_OFFSET + AHCI_P_CMD, AHCI_P_CMD_ST | AHCI_P_CMD_PMA))
        );
    }

    #[test]
    fn wait_ready_preserves_the_upstream_timeout_boundary() {
        let mut io = FakeAhciIo::default();
        io.regs[(AHCI_OFFSET + AHCI_P_TFD) / 4] = ATA_S_BUSY;
        let mut ctlr = AhciController::new(io, 0, 0, 0);
        let port = PortState::new(0);
        assert_eq!(
            ctlr.ahci_wait_ready(&port, 2, 5),
            Err(ReadyTimeout {
                elapsed_ms: 8,
                task_file: ATA_S_BUSY
            })
        );
        let io = ctlr.into_io();
        assert_eq!(io.delay_count, 3);
    }

    #[test]
    fn sata_phy_reset_uses_user_generation_and_slow_device_timeout() {
        let mut io = FakeAhciIo::default();
        io.regs[(AHCI_OFFSET + AHCI_P_SSTS) / 4] =
            ATA_SS_DET_PHY_ONLINE | ATA_SS_SPD_GEN2 | ATA_SS_IPM_ACTIVE;
        let mut ctlr = AhciController::new(io, 0, 0, 0);
        let mut port = PortState::new(0);
        port.user_revision[0] = 2;
        assert!(ctlr.ahci_sata_phy_reset(&mut port));
        let io = ctlr.into_io();
        assert!(io.writes[..io.write_count].contains(&(
            AHCI_OFFSET + AHCI_P_SCTL,
            ATA_SC_DET_RESET
                | ATA_SC_SPD_SPEED_GEN2
                | ATA_SC_IPM_DIS_PARTIAL
                | ATA_SC_IPM_DIS_SLUMBER
        )));
        assert!(io.writes[..io.write_count].contains(&(AHCI_OFFSET + AHCI_P_SERR, u32::MAX)));
    }
    #[test]
    fn reset_reads_boh_capability_instead_of_relying_on_cached_attach_state() {
        let mut io = FakeAhciIo::default();
        io.regs[AHCI_VS / 4] = 0x0001_0200;
        io.regs[AHCI_CAP2 / 4] = AHCI_CAP2_BOH;
        io.regs[AHCI_BOHC / 4] = AHCI_BOHC_BOS | AHCI_BOHC_BB;
        io.regs[AHCI_GHC / 4] = AHCI_GHC_AE;
        let mut ctlr = AhciController::new(io, 0, 0, 0);
        assert_eq!(ctlr.ahci_ctlr_reset(), Ok(()));
        let io = ctlr.into_io();
        assert!(
            io.writes[..io.write_count]
                .contains(&(AHCI_BOHC, AHCI_BOHC_BOS | AHCI_BOHC_BB | AHCI_BOHC_OOS,))
        );
    }
}
