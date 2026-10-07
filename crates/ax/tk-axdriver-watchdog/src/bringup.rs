use crate::{Error, Result, ids::Version, regs::*};

/// The caller owns this controller and must serialize access to it.
///
/// Construction is passive. `start` is an explicit takeover request; callers
/// may use `adopt_running` instead when firmware has already left the timer
/// counting and stopping it first would lose the countdown.
pub struct Tco<B: Bus> {
    pub(crate) bus: B,
    pub version: Version,
    pub timeout: u32,
    /// True only when the recognized ICH9 v2 BOOT_STATUS bit was observed.
    /// `false` for v6 means no supported boot-reset status is available, not
    /// that a reset was proven not to have happened.
    pub boot_status: bool,
    pub running: bool,
    owns_watchdog: bool,
    configured: bool,
}

impl<B: Bus> Tco<B> {
    /// Creates a controller without reading or changing hardware.
    pub fn new(bus: B, version: Version) -> Self {
        Self {
            bus,
            version,
            timeout: 0,
            boot_status: false,
            running: false,
            owns_watchdog: false,
            configured: false,
        }
    }

    /// Whether the controller is safe to expose as a watchdog device.
    ///
    /// A failed start can still leave a verified running watchdog (for
    /// example, if firmware locks HALT). In that case retain the controller
    /// so the owner can ping or retry stopping it.
    pub fn available(&self) -> bool {
        self.running || self.configured
    }

    /// Explicitly takes over the watchdog and starts it with `seconds`.
    pub fn start(&mut self, seconds: u32) -> Result {
        if self.owns_watchdog {
            return Err(Error::BadState);
        }
        let ticks = timeout_ticks(seconds)?;
        self.boot_status =
            self.version == Version::Ich9V2 && self.bus.read16(STATUS2) & BOOT_STATUS != 0;

        // The boot option is an explicit authorization to take control. Claim
        // ownership before the first write so a failed HALT readback does not
        // discard the only handle to a watchdog that may still be counting.
        self.owns_watchdog = true;
        self.configured = false;
        self.stop_hardware()?;
        self.set_no_reboot(true)?;
        self.disable_smi_watchdog_clear()?;
        self.bus.write16(STATUS1, 8);
        self.bus.write16(STATUS2, 6);
        self.set_timer_ticks(ticks)?;
        self.timeout = seconds;
        self.configured = true;
        self.enable()
    }

    /// Adopts a firmware-started watchdog without first halting it.
    ///
    /// Once an active timer is observed, ownership and the running state are
    /// retained even if a later setup/readback step fails, so callers can
    /// continue to ping it or retry stopping it.
    pub fn adopt_running(&mut self, seconds: u32) -> Result {
        if self.owns_watchdog {
            return Err(Error::BadState);
        }
        let ticks = timeout_ticks(seconds)?;
        if self.bus.read16(CONTROL1) & HALT != 0 {
            return Err(Error::BadState);
        }

        self.owns_watchdog = true;
        self.running = true;
        self.configured = false;
        self.boot_status =
            self.version == Version::Ich9V2 && self.bus.read16(STATUS2) & BOOT_STATUS != 0;
        self.disable_smi_watchdog_clear()?;
        self.set_timer_ticks(ticks)?;
        self.set_no_reboot(false)?;
        self.bus.write16(RELOAD, 1);
        self.timeout = seconds;
        self.configured = true;
        Ok(())
    }

    fn disable_smi_watchdog_clear(&mut self) -> Result {
        if self.version == Version::Ich9V2 {
            let mask = 1 << 13;
            let value = self.bus.read_smi_enable() & !mask;
            self.bus.write_smi_enable(value);
            if self.bus.read_smi_enable() & mask != 0 {
                return Err(Error::Locked);
            }
        }
        Ok(())
    }

    fn set_no_reboot(&mut self, set: bool) -> Result {
        if self.version == Version::CnlV6 {
            let old = self.bus.read16(CONTROL1) & !NMI_NOW;
            let value = if set { old | 1 } else { old & !1 };
            self.bus.write16(CONTROL1, value);
            if self.bus.read16(CONTROL1) & !NMI_NOW != value {
                return Err(Error::Locked);
            }
        } else {
            let old = self.bus.read_no_reboot();
            let value = if set { old | 0x20 } else { old & !0x20 };
            self.bus.write_no_reboot(value);
            if self.bus.read_no_reboot() != value {
                return Err(Error::Locked);
            }
        }
        Ok(())
    }

    /// Enables a controller already explicitly started or adopted.
    pub fn enable(&mut self) -> Result {
        if !self.owns_watchdog {
            return Err(Error::BadState);
        }
        // Opening the device must not prevent feeding a retained live timer
        // after a failed takeover. Do not restart an incompletely configured
        // stopped timer, however.
        if self.running {
            return Ok(());
        }
        if !self.configured {
            return Err(Error::BadState);
        }
        self.set_no_reboot(false)?;
        self.bus.write16(RELOAD, 1);
        let value = self.bus.read16(CONTROL1) & !(HALT | NMI_NOW);
        self.bus.write16(CONTROL1, value);
        if self.bus.read16(CONTROL1) & HALT != 0 {
            self.running = false;
            return Err(Error::Locked);
        }
        self.running = true;
        Ok(())
    }

    /// Stops a started/adopted watchdog. Retrying is valid after HALT succeeds
    /// but the protective NO_REBOOT update fails.
    pub fn stop(&mut self) -> Result {
        if !self.owns_watchdog {
            return Err(Error::BadState);
        }
        self.stop_hardware()?;
        self.set_no_reboot(true)
    }

    fn stop_hardware(&mut self) -> Result {
        let value = (self.bus.read16(CONTROL1) & !NMI_NOW) | HALT;
        self.bus.write16(CONTROL1, value);
        if self.bus.read16(CONTROL1) & HALT == 0 {
            // Keep the state truthful and the owner handle usable if HALT was
            // locked and the timer remains active.
            self.running = true;
            return Err(Error::Locked);
        }
        self.running = false;
        Ok(())
    }

    pub fn set_timeout(&mut self, seconds: u32) -> Result {
        // Timer programming alone cannot repair failed SMI/NO_REBOOT setup.
        // An incompletely configured live timer may only be fed or stopped.
        if !self.owns_watchdog || !self.configured {
            return Err(Error::BadState);
        }
        let ticks = timeout_ticks(seconds)?;
        self.set_timer_ticks(ticks)?;
        self.timeout = seconds;
        Ok(())
    }

    fn set_timer_ticks(&mut self, ticks: u16) -> Result {
        let value = (self.bus.read16(TIMER) & !1023) | ticks;
        self.bus.write16(TIMER, value);
        if self.bus.read16(TIMER) & 1023 != ticks {
            return Err(Error::Io);
        }
        Ok(())
    }

    pub fn ping(&mut self) -> Result {
        if !self.owns_watchdog || !self.running {
            return Err(Error::BadState);
        }
        self.bus.write16(RELOAD, 1);
        Ok(())
    }

    pub fn time_left(&mut self) -> u32 {
        u32::from(self.bus.read16(RELOAD) & 1023) * 6 / 10
    }
}

fn timeout_ticks(seconds: u32) -> Result<u16> {
    let ticks = seconds.checked_mul(10).ok_or(Error::Invalid)? / 6;
    if !(4..=1023).contains(&ticks) {
        return Err(Error::Invalid);
    }
    Ok(ticks as u16)
}
