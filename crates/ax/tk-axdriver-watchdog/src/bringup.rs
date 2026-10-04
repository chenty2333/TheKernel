use crate::{Error, Result, ids::Version, regs::*};
pub struct Tco<B: Bus> {
    pub(crate) bus: B,
    pub version: Version,
    pub timeout: u32,
    pub boot_status: bool,
    pub running: bool,
}
impl<B: Bus> Tco<B> {
    pub fn start(bus: B, version: Version, timeout: u32) -> Result<Self> {
        let mut device = Self {
            bus,
            version,
            timeout: 0,
            boot_status: false,
            running: false,
        };
        device.boot_status = device.bus.read16(STATUS2) & 4 != 0;
        device.stop()?;
        if version == Version::Ich9V2 {
            let smi = device.bus.read_smi_enable();
            device.bus.write_smi_enable(smi & !(1 << 13));
        }
        device.bus.write16(STATUS1, 8);
        device.bus.write16(STATUS2, 6);
        device.set_timeout(timeout)?;
        device.enable()?;
        Ok(device)
    }
    fn no_reboot(&mut self, set: bool) -> Result {
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
    pub fn enable(&mut self) -> Result {
        self.no_reboot(false)?;
        self.ping();
        let value = self.bus.read16(CONTROL1) & !(HALT | NMI_NOW);
        self.bus.write16(CONTROL1, value);
        if self.bus.read16(CONTROL1) & HALT != 0 {
            return Err(Error::Locked);
        }
        self.running = true;
        Ok(())
    }
    pub fn stop(&mut self) -> Result {
        let value = (self.bus.read16(CONTROL1) & !NMI_NOW) | HALT;
        self.bus.write16(CONTROL1, value);
        if self.bus.read16(CONTROL1) & HALT == 0 {
            return Err(Error::Locked);
        }
        self.no_reboot(true)?;
        self.running = false;
        Ok(())
    }
    pub fn set_timeout(&mut self, seconds: u32) -> Result {
        let ticks = seconds.checked_mul(10).ok_or(Error::Invalid)? / 6;
        if !(4..=1023).contains(&ticks) {
            return Err(Error::Invalid);
        }
        let value = (self.bus.read16(TIMER) & !1023) | ticks as u16;
        self.bus.write16(TIMER, value);
        if self.bus.read16(TIMER) & 1023 != ticks as u16 {
            return Err(Error::Io);
        }
        self.timeout = seconds;
        Ok(())
    }
    pub fn ping(&mut self) {
        self.bus.write16(RELOAD, 1);
    }
    pub fn time_left(&mut self) -> u32 {
        u32::from(self.bus.read16(RELOAD) & 1023) * 6 / 10
    }
}
