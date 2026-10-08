//! Function-level Rust translation of FreeBSD `sys/dev/ichiic/ig4_iic.c`.
//! FreeBSD source snapshot 2026-10-08; BSD-3-Clause.
//! Copyright (c) 2014 The DragonFly Project. This code is derived from
//! software contributed by Matthew Dillon and ported to FreeBSD by Michael
//! Gmelin. Full notice is retained in `LICENSES/BSD-3-Clause.txt`.

use crate::reg::*;

pub const IIC_M_RD: u16 = 0x0001;
pub const IIC_M_NOSTOP: u16 = 0x0002;
pub const IIC_M_NOSTART: u16 = 0x0004;
pub const IIC_REQUEST_BUS: i32 = 1;
pub const IIC_RELEASE_BUS: i32 = 2;
pub const IIC_WAIT: i32 = 1;
pub const IIC_DONTWAIT: i32 = 0;
pub const IIC_UNKNOWN: u8 = 0xff;
const IG4_FIFO_LOWAT: i32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum IicError {
    NoError             = 0,
    BusError            = 1,
    NoAck               = 2,
    Timeout             = 3,
    BusBusy             = 4,
    Status              = 5,
    Underflow           = 6,
    Overflow            = 7,
    NotSupported        = 8,
    HardwareUnavailable = 9,
    Invalid             = 22,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum Version {
    Emag       = 0,
    Haswell    = 1,
    Atom       = 2,
    Skylake    = 3,
    ApolloLake = 4,
    CannonLake = 5,
    TigerLake  = 6,
    GeminiLake = 7,
}

impl Version {
    pub const fn has_addregs(self) -> bool {
        (self as usize) >= Version::Skylake as usize
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Config {
    pub version: u32,
    pub bus_speed: u32,
    pub ss_scl_hcnt: u16,
    pub ss_scl_lcnt: u16,
    pub ss_sda_hold: u16,
    pub fs_scl_hcnt: u16,
    pub fs_scl_lcnt: u16,
    pub fs_sda_hold: u16,
    pub txfifo_depth: i32,
    pub rxfifo_depth: i32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Hardware {
    pub ic_clock_rate: u32,
    pub sda_fall_time: u32,
    pub scl_fall_time: u32,
    pub sda_hold_time: u32,
    pub txfifo_depth: i32,
    pub rxfifo_depth: i32,
}

const IG4IIC_HW: [Hardware; 8] = [
    Hardware {
        ic_clock_rate: 100,
        sda_fall_time: 0,
        scl_fall_time: 0,
        sda_hold_time: 0,
        txfifo_depth: 0,
        rxfifo_depth: 0,
    },
    Hardware {
        ic_clock_rate: 100,
        sda_fall_time: 0,
        scl_fall_time: 0,
        sda_hold_time: 90,
        txfifo_depth: 32,
        rxfifo_depth: 32,
    },
    Hardware {
        ic_clock_rate: 100,
        sda_fall_time: 280,
        scl_fall_time: 240,
        sda_hold_time: 60,
        txfifo_depth: 32,
        rxfifo_depth: 32,
    },
    Hardware {
        ic_clock_rate: 120,
        sda_fall_time: 0,
        scl_fall_time: 0,
        sda_hold_time: 230,
        txfifo_depth: 0,
        rxfifo_depth: 0,
    },
    Hardware {
        ic_clock_rate: 133,
        sda_fall_time: 171,
        scl_fall_time: 208,
        sda_hold_time: 207,
        txfifo_depth: 0,
        rxfifo_depth: 0,
    },
    Hardware {
        ic_clock_rate: 216,
        sda_fall_time: 0,
        scl_fall_time: 0,
        sda_hold_time: 230,
        txfifo_depth: 0,
        rxfifo_depth: 0,
    },
    Hardware {
        ic_clock_rate: 133,
        sda_fall_time: 171,
        scl_fall_time: 208,
        sda_hold_time: 42,
        txfifo_depth: 0,
        rxfifo_depth: 0,
    },
    Hardware {
        ic_clock_rate: 133,
        sda_fall_time: 171,
        scl_fall_time: 290,
        sda_hold_time: 313,
        txfifo_depth: 0,
        rxfifo_depth: 0,
    },
];

#[derive(Debug)]
pub struct IicMessage<'a> {
    pub slave: u16,
    pub flags: u16,
    pub buf: &'a mut [u8],
}

/// The operating-system seam. Register I/O uses ordered MMIO accessors;
/// sleep/delay, ACPI package evaluation, child-bus attachment and IRQ lifetime
/// reproduce the boundaries of FreeBSD's bus framework without copying it.
pub trait Backend {
    fn read32(&mut self, register: u32) -> u32;
    fn write32(&mut self, register: u32, value: u32);
    fn delay_us(&mut self, microseconds: u32);
    fn pause_ms(&mut self, message: &'static str, milliseconds: u32);
    fn do_poll(&self) -> bool;
    fn wait_irq(&mut self, milliseconds: u32);
    fn acpi_clock_params(&mut self, method: &str) -> Result<[u64; 3], ()>;
    fn add_iicbus_child(&mut self) -> bool;
    fn attach_iicbus_children(&mut self);
    fn detach_iicbus_children(&mut self) -> Result<(), IicError>;
    fn setup_interrupt(&mut self) -> Result<(), IicError>;
    fn teardown_interrupt(&mut self);
    fn suspend_iicbus_children(&mut self) -> Result<(), IicError>;
    fn resume_iicbus_children(&mut self) -> Result<(), IicError>;
    fn debug_register(&mut self, name: &'static str, value: u32);
    fn init_locks(&mut self);
    fn destroy_locks(&mut self);
    fn call_lock_owned(&self) -> bool;
    fn call_try_lock(&mut self) -> bool;
    fn call_lock(&mut self);
    fn call_unlock(&mut self);
    fn wake_waiters(&mut self);
}

pub struct Ig4<I> {
    pub io: I,
    pub version: Version,
    pub cfg: Config,
    pub intr_mask: u32,
    pub last_slave: u8,
    pub platform_attached: bool,
    pub use_10bit: bool,
    pub slave_valid: bool,
    pub timings: i32,
    pub dump_mask: u32,
}

impl<I: Backend> Ig4<I> {
    pub fn new(io: I, version: Version, timings: i32) -> Self {
        Self {
            io,
            version,
            cfg: Config::default(),
            intr_mask: 0,
            last_slave: 0,
            platform_attached: false,
            use_10bit: false,
            slave_valid: false,
            timings,
            dump_mask: 0,
        }
    }

    // upstream: ig4_iic.c reg_write()
    fn reg_write(&mut self, reg: u32, value: u32) {
        self.io.write32(reg, value);
    }
    // upstream: ig4_iic.c reg_read()
    fn reg_read(&mut self, reg: u32) -> u32 {
        self.io.read32(reg)
    }

    // upstream: ig4_iic.c ig4iic_set_intr_mask()
    fn ig4iic_set_intr_mask(&mut self, val: u32) {
        if self.intr_mask != val {
            self.reg_write(IG4_REG_INTR_MASK, val);
            self.intr_mask = val;
        }
    }

    // upstream: ig4_iic.c intrstat2iic()
    fn intrstat2iic(&mut self, val: u32) -> IicError {
        if val & IG4_INTR_RX_UNDER != 0 {
            self.reg_read(IG4_REG_CLR_RX_UNDER);
        }
        if val & IG4_INTR_RX_OVER != 0 {
            self.reg_read(IG4_REG_CLR_RX_OVER);
        }
        if val & IG4_INTR_TX_OVER != 0 {
            self.reg_read(IG4_REG_CLR_TX_OVER);
        }
        if val & IG4_INTR_TX_ABRT != 0 {
            let src = self.reg_read(IG4_REG_TX_ABRT_SOURCE);
            self.reg_read(IG4_REG_CLR_TX_ABORT);
            if src & IG4_ABRTSRC_TRANSFER != 0 {
                return IicError::Status;
            }
            if src & IG4_ABRTSRC_ARBLOST != 0 {
                return IicError::BusBusy;
            }
            if src
                & (IG4_ABRTSRC_TXNOACK_ADDR7
                    | IG4_ABRTSRC_TXNOACK_ADDR10_1
                    | IG4_ABRTSRC_TXNOACK_ADDR10_2
                    | IG4_ABRTSRC_TXNOACK_DATA
                    | IG4_ABRTSRC_GENCALL_NOACK)
                != 0
            {
                return IicError::NoAck;
            }
            if src
                & (IG4_ABRTSRC_GENCALL_READ
                    | IG4_ABRTSRC_NORESTART_START
                    | IG4_ABRTSRC_NORESTART_10)
                != 0
            {
                return IicError::NotSupported;
            }
            if src & IG4_ABRTSRC_ACKED_START != 0 {
                return IicError::BusError;
            }
        }
        if val & (IG4_INTR_TX_OVER | IG4_INTR_RX_OVER) != 0 {
            return IicError::Overflow;
        }
        if val & IG4_INTR_RX_UNDER != 0 {
            return IicError::Underflow;
        }
        IicError::NoError
    }

    // upstream: ig4_iic.c set_controller()
    fn set_controller(&mut self, ctl: u32) -> IicError {
        self.ig4iic_set_intr_mask(0);
        if ctl & IG4_I2C_ENABLE != 0 {
            self.reg_read(IG4_REG_CLR_INTR);
        }
        self.reg_write(IG4_REG_I2C_EN, ctl);
        let mut error = IicError::Timeout;
        for _retry in (1..=100).rev() {
            let v = self.reg_read(IG4_REG_ENABLE_STATUS);
            if (v ^ ctl) & IG4_I2C_ENABLE == 0 {
                error = IicError::NoError;
                break;
            }
            self.io.pause_ms("i2cslv", 1);
        }
        error
    }

    // upstream: ig4_iic.c wait_intr()
    fn wait_intr(&mut self, intr: u32) -> IicError {
        let mut txlvl = -1i32;
        let mut count_us = 0u32;
        let limit_us = 1_000_000u32;
        loop {
            let mut v = self.reg_read(IG4_REG_RAW_INTR_STAT);
            let mut error = self.intrstat2iic(v & IG4_INTR_ERR_MASK);
            if error != IicError::NoError || v & intr != 0 {
                break error;
            }
            if intr & (IG4_INTR_TX_EMPTY | IG4_INTR_STOP_DET) != 0 {
                v = self.reg_read(IG4_REG_TXFLR) & IG4_FIFOLVL_MASK;
                if txlvl != v as i32 {
                    txlvl = v as i32;
                    count_us = 0;
                }
            }
            if count_us >= limit_us {
                error = IicError::Timeout;
                break error;
            }
            if !self.io.do_poll() {
                self.ig4iic_set_intr_mask(intr | IG4_INTR_ERR_MASK);
                self.io.wait_irq(10);
                self.ig4iic_intr();
                self.ig4iic_set_intr_mask(0);
                count_us = count_us.saturating_add(10_000);
            } else {
                self.io.delay_us(25);
                count_us += 25;
            }
        }
    }

    // upstream: ig4_iic.c set_slave_addr()
    fn set_slave_addr(&mut self, slave: u8) {
        let use_10bit = false;
        if self.slave_valid && self.last_slave == slave && self.use_10bit == use_10bit {
            return;
        }
        self.use_10bit = use_10bit;
        self.reg_write(IG4_REG_TX_TL, 0);
        self.wait_intr(IG4_INTR_TX_EMPTY);
        self.set_controller(0);
        let mut ctl = self.reg_read(IG4_REG_CTL);
        ctl &= !IG4_CTL_10BIT;
        ctl |= IG4_CTL_RESTARTEN;
        let mut tar = u32::from(slave);
        if self.use_10bit {
            tar |= IG4_TAR_10BIT;
            ctl |= IG4_CTL_10BIT;
        }
        self.reg_write(IG4_REG_CTL, ctl);
        self.reg_write(IG4_REG_TAR_ADD, tar);
        self.set_controller(IG4_I2C_ENABLE);
        self.slave_valid = true;
        self.last_slave = slave;
    }

    // upstream: ig4_iic.c ig4iic_xfer_start()
    fn ig4iic_xfer_start(&mut self, slave: u16, repeated_start: bool) -> IicError {
        self.set_slave_addr((slave >> 1) as u8);
        if !repeated_start {
            self.reg_read(IG4_REG_CLR_INTR);
        }
        IicError::NoError
    }
    // upstream: ig4_iic.c ig4iic_xfer_is_started()
    fn ig4iic_xfer_is_started(&mut self) -> bool {
        self.reg_read(IG4_REG_RAW_INTR_STAT) & (IG4_INTR_START_DET | IG4_INTR_STOP_DET)
            == IG4_INTR_START_DET
    }
    // upstream: ig4_iic.c ig4iic_xfer_abort()
    fn ig4iic_xfer_abort(&mut self) -> IicError {
        self.set_controller(IG4_I2C_ABORT | IG4_I2C_ENABLE);
        let error = self.wait_intr(IG4_INTR_STOP_DET);
        self.set_controller(IG4_I2C_ENABLE);
        if error == IicError::Status {
            IicError::NoError
        } else {
            error
        }
    }

    // upstream: ig4_iic.c ig4iic_read()
    fn ig4iic_read(&mut self, buf: &mut [u8], repeated_start: bool, stop: bool) -> IicError {
        let len = buf.len();
        if len == 0 {
            return IicError::NoError;
        }
        let (mut requested, mut received) = (0i32, 0i32);
        let mut error = IicError::NoError;
        while received < len as i32 {
            let mut burst =
                self.cfg.txfifo_depth - (self.reg_read(IG4_REG_TXFLR) & IG4_FIFOLVL_MASK) as i32;
            if burst <= 0 {
                self.reg_write(IG4_REG_TX_TL, IG4_FIFO_LOWAT as u32);
                error = self.wait_intr(IG4_INTR_TX_EMPTY);
                if error != IicError::NoError {
                    break;
                }
                burst = self.cfg.txfifo_depth
                    - (self.reg_read(IG4_REG_TXFLR) & IG4_FIFOLVL_MASK) as i32;
            }
            burst = burst.min(self.cfg.rxfifo_depth - (requested - received));
            let target = (requested + burst).min(len as i32);
            while requested < target {
                let mut cmd = IG4_DATA_COMMAND_RD;
                if repeated_start && requested == 0 {
                    cmd |= IG4_DATA_RESTART;
                }
                if stop && requested == len as i32 - 1 {
                    cmd |= IG4_DATA_STOP;
                }
                self.reg_write(IG4_REG_DATA_CMD, cmd);
                requested += 1;
            }
            let lowat = if requested != len as i32 && requested - received > IG4_FIFO_LOWAT {
                IG4_FIFO_LOWAT
            } else {
                0
            };
            while received < requested - lowat {
                burst = (requested - received)
                    .min((self.reg_read(IG4_REG_RXFLR) & IG4_FIFOLVL_MASK) as i32);
                if burst > 0 {
                    while burst > 0 {
                        buf[received as usize] = (self.reg_read(IG4_REG_DATA_CMD) & 0xff) as u8;
                        received += 1;
                        burst -= 1;
                    }
                } else {
                    self.reg_write(IG4_REG_RX_TL, (requested - received - lowat - 1) as u32);
                    error = self.wait_intr(IG4_INTR_RX_FULL);
                    if error != IicError::NoError {
                        return error;
                    }
                }
            }
        }
        error
    }

    // upstream: ig4_iic.c ig4iic_write()
    fn ig4iic_write(&mut self, buf: &[u8], repeated_start: bool, stop: bool) -> IicError {
        let len = buf.len();
        if len == 0 {
            return IicError::NoError;
        }
        let mut sent = 0i32;
        let mut error = IicError::NoError;
        while sent < len as i32 {
            let burst =
                self.cfg.txfifo_depth - (self.reg_read(IG4_REG_TXFLR) & IG4_FIFOLVL_MASK) as i32;
            let target = (sent + burst).min(len as i32);
            while sent < target {
                let mut cmd = u32::from(buf[sent as usize]);
                if repeated_start && sent == 0 {
                    cmd |= IG4_DATA_RESTART;
                }
                if stop && sent == len as i32 - 1 {
                    cmd |= IG4_DATA_STOP;
                }
                self.reg_write(IG4_REG_DATA_CMD, cmd);
                sent += 1;
            }
            if sent < len as i32 {
                let lowat = if len as i32 - sent <= self.cfg.txfifo_depth {
                    self.cfg.txfifo_depth - (len as i32 - sent)
                } else {
                    IG4_FIFO_LOWAT
                };
                self.reg_write(IG4_REG_TX_TL, lowat as u32);
                error = self.wait_intr(IG4_INTR_TX_EMPTY);
                if error != IicError::NoError {
                    break;
                }
            }
        }
        error
    }

    // upstream: ig4_iic.c ig4iic_transfer()
    pub fn ig4iic_transfer(&mut self, msgs: &mut [IicMessage<'_>]) -> IicError {
        for i in 0..msgs.len() {
            if msgs[i].buf.is_empty() {
                return IicError::NotSupported;
            }
            if i > 0 {
                if msgs[i].flags & IIC_M_NOSTART != 0 && msgs[i - 1].flags & IIC_M_NOSTOP == 0 {
                    return IicError::NotSupported;
                }
                if msgs[i - 1].flags & IIC_M_NOSTOP != 0 && msgs[i].slave != msgs[i - 1].slave {
                    return IicError::NotSupported;
                }
                if msgs[i].flags & IIC_M_NOSTART != 0
                    && (msgs[i].flags & IIC_M_RD) != (msgs[i - 1].flags & IIC_M_RD)
                {
                    return IicError::NotSupported;
                }
            }
        }
        let allocated = self.io.call_lock_owned();
        if !allocated {
            self.io.call_lock();
        }
        let mut i = 0usize;
        self.reg_read(IG4_REG_CLR_TX_ABORT);
        let (mut rpstart, mut error) = (false, IicError::NoError);
        while i < msgs.len() {
            if msgs[i].flags & IIC_M_NOSTART == 0 {
                error = self.ig4iic_xfer_start(msgs[i].slave, rpstart);
            } else if !self.slave_valid || (msgs[i].slave >> 1) as u8 != self.last_slave {
                error = IicError::Invalid;
                break;
            } else {
                rpstart = false;
            }
            if error != IicError::NoError {
                break;
            }
            let stop = msgs[i].flags & IIC_M_NOSTOP == 0;
            if msgs[i].flags & IIC_M_RD != 0 {
                error = self.ig4iic_read(msgs[i].buf, rpstart, stop);
            } else {
                error = self.ig4iic_write(msgs[i].buf, rpstart, stop);
            }
            if stop && error == IicError::NoError {
                error = self.wait_intr(IG4_INTR_STOP_DET);
                if error == IicError::NoError {
                    self.reg_read(IG4_REG_CLR_INTR);
                }
            }
            if error != IicError::NoError {
                if self.ig4iic_xfer_is_started() && self.ig4iic_xfer_abort() != IicError::NoError {
                    let _ = self.ig4iic_set_config(true, false);
                } else {
                    while self.reg_read(IG4_REG_I2C_STA) & IG4_STATUS_RX_NOTEMPTY != 0 {
                        self.reg_read(IG4_REG_DATA_CMD);
                    }
                    self.reg_read(IG4_REG_TX_ABRT_SOURCE);
                    self.reg_read(IG4_REG_CLR_INTR);
                }
                break;
            }
            rpstart = !stop;
            i += 1;
        }
        if !allocated {
            self.io.call_unlock();
        }
        error
    }

    // upstream: ig4_iic.c ig4iic_reset()
    pub fn ig4iic_reset(&mut self, speed: u8, addr: u8, oldaddr: Option<&mut u8>) -> IicError {
        let allocated = self.io.call_lock_owned();
        if !allocated {
            self.io.call_lock();
        }
        let _ = speed;
        if let Some(old) = oldaddr {
            *old = self.last_slave << 1;
        }
        self.set_slave_addr(addr >> 1);
        if addr == IIC_UNKNOWN {
            self.slave_valid = false;
        }
        if !allocated {
            self.io.call_unlock();
        }
        IicError::NoError
    }
    // upstream: ig4_iic.c ig4iic_callback()
    pub fn ig4iic_callback(&mut self, index: i32, how: i32) -> IicError {
        match index {
            IIC_REQUEST_BUS => {
                if how & IIC_WAIT == 0 {
                    if !self.io.call_try_lock() {
                        IicError::BusBusy
                    } else {
                        IicError::NoError
                    }
                } else {
                    self.io.call_lock();
                    IicError::NoError
                }
            }
            IIC_RELEASE_BUS => {
                self.io.call_unlock();
                IicError::NoError
            }
            _ => IicError::Invalid,
        }
    }

    // upstream: ig4_iic.c ig4iic_clk_params()
    pub fn ig4iic_clk_params(
        hw: &Hardware,
        speed: i32,
        vals: &mut [u16; 3],
    ) -> Result<(), IicError> {
        let (thigh, tlow, tf_max) = match speed as u32 {
            IG4_CTL_SPEED_STD => (4000u32, 4700u32, 300u32),
            IG4_CTL_SPEED_FAST => (600, 1300, 300),
            _ => return Err(IicError::Invalid),
        };
        let sda = if hw.sda_fall_time == 0 {
            tf_max
        } else {
            hw.sda_fall_time
        };
        vals[0] = ((hw.ic_clock_rate * (thigh + sda) + 500) / 1000 - 3) as u16;
        let scl = if hw.scl_fall_time == 0 {
            tf_max
        } else {
            hw.scl_fall_time
        };
        vals[1] = ((hw.ic_clock_rate * (tlow + scl) + 500) / 1000 - 1) as u16;
        if hw.sda_hold_time != 0 {
            vals[2] = ((hw.ic_clock_rate * hw.sda_hold_time + 500) / 1000) as u16;
        }
        Ok(())
    }

    // upstream: ig4_iic.c ig4iic_acpi_params()
    pub fn ig4iic_acpi_params(
        &mut self,
        method: &str,
        vals: &mut [u16; 3],
    ) -> Result<(), IicError> {
        let p = self
            .io
            .acpi_clock_params(method)
            .map_err(|_| IicError::Invalid)?;
        for i in 0..3 {
            vals[i] = (p[i] as u32
                & if i == 2 {
                    IG4_SDA_TX_HOLD_MASK
                } else {
                    IG4_SCL_CLOCK_MASK
                }) as u16;
        }
        Ok(())
    }

    // upstream: ig4_iic.c ig4iic_get_config()
    pub fn ig4iic_get_config(&mut self) -> Config {
        let mut c = Config {
            version: self.reg_read(IG4_REG_COMP_VER),
            bus_speed: self.reg_read(IG4_REG_CTL) & IG4_CTL_SPEED_MASK,
            ss_scl_hcnt: (self.reg_read(IG4_REG_SS_SCL_HCNT) & IG4_SCL_CLOCK_MASK) as u16,
            ss_scl_lcnt: (self.reg_read(IG4_REG_SS_SCL_LCNT) & IG4_SCL_CLOCK_MASK) as u16,
            fs_scl_hcnt: (self.reg_read(IG4_REG_FS_SCL_HCNT) & IG4_SCL_CLOCK_MASK) as u16,
            fs_scl_lcnt: (self.reg_read(IG4_REG_FS_SCL_LCNT) & IG4_SCL_CLOCK_MASK) as u16,
            txfifo_depth: self.cfg.txfifo_depth,
            rxfifo_depth: self.cfg.rxfifo_depth,
            ..Config::default()
        };
        let hold = (self.reg_read(IG4_REG_SDA_HOLD) & IG4_SDA_TX_HOLD_MASK) as u16;
        c.ss_sda_hold = hold;
        c.fs_sda_hold = hold;
        if c.bus_speed != IG4_CTL_SPEED_STD {
            c.bus_speed = IG4_CTL_SPEED_FAST;
        }
        if self.version == Version::Haswell || self.version == Version::Atom {
            let v = self.reg_read(IG4_REG_COMP_PARAM1);
            if ig4_param1_txfifo_depth(v) != 0 {
                c.txfifo_depth = ig4_param1_txfifo_depth(v) as i32;
            }
            if ig4_param1_rxfifo_depth(v) != 0 {
                c.rxfifo_depth = ig4_param1_rxfifo_depth(v) as i32;
            }
        }
        if self.timings < 2 {
            let hw = IG4IIC_HW[self.version as usize];
            c.bus_speed = IG4_CTL_SPEED_FAST;
            let mut ss = [c.ss_scl_hcnt, c.ss_scl_lcnt, c.ss_sda_hold];
            let mut fs = [c.fs_scl_hcnt, c.fs_scl_lcnt, c.fs_sda_hold];
            let _ = Self::ig4iic_clk_params(&hw, IG4_CTL_SPEED_STD as i32, &mut ss);
            let _ = Self::ig4iic_clk_params(&hw, IG4_CTL_SPEED_FAST as i32, &mut fs);
            c.ss_scl_hcnt = ss[0];
            c.ss_scl_lcnt = ss[1];
            c.ss_sda_hold = ss[2];
            c.fs_scl_hcnt = fs[0];
            c.fs_scl_lcnt = fs[1];
            c.fs_sda_hold = fs[2];
            if hw.txfifo_depth != 0 {
                c.txfifo_depth = hw.txfifo_depth;
            }
            if hw.rxfifo_depth != 0 {
                c.rxfifo_depth = hw.rxfifo_depth;
            }
        } else if self.timings == 2 {
            c.bus_speed = IG4_CTL_SPEED_STD;
            c.ss_scl_hcnt = 100;
            c.fs_scl_hcnt = 100;
            c.ss_scl_lcnt = 125;
            c.fs_scl_lcnt = 125;
            if self.version == Version::Skylake {
                c.ss_sda_hold = 28;
                c.fs_sda_hold = 28;
            }
        }
        if self.timings == 0 {
            let mut ss = [c.ss_scl_hcnt, c.ss_scl_lcnt, c.ss_sda_hold];
            if self.ig4iic_acpi_params("SSCN", &mut ss).is_ok() {
                c.ss_scl_hcnt = ss[0];
                c.ss_scl_lcnt = ss[1];
                c.ss_sda_hold = ss[2];
            }
            let mut fs = [c.fs_scl_hcnt, c.fs_scl_lcnt, c.fs_sda_hold];
            if self.ig4iic_acpi_params("FMCN", &mut fs).is_ok() {
                c.fs_scl_hcnt = fs[0];
                c.fs_scl_lcnt = fs[1];
                c.fs_sda_hold = fs[2];
            }
        }
        self.cfg = c;
        c
    }

    // upstream: ig4_iic.c ig4iic_set_config()
    pub fn ig4iic_set_config(
        &mut self,
        mut reset: bool,
        force_restore: bool,
    ) -> Result<(), IicError> {
        let mut v = self.reg_read(IG4_REG_DEVIDLE_CTRL);
        if self.version.has_addregs() && (force_restore || v & IG4_RESTORE_REQUIRED != 0) {
            self.reg_write(IG4_REG_DEVIDLE_CTRL, IG4_DEVICE_IDLE | IG4_RESTORE_REQUIRED);
            self.reg_write(IG4_REG_DEVIDLE_CTRL, 0);
            self.io.pause_ms("i2crst", 1);
            reset = true;
        }
        if self.version == Version::Haswell {
            v = self.reg_read(IG4_REG_CLK_PARMS);
            if v & IG4_CLK_PARMS_EN == 0 {
                self.reg_write(IG4_REG_CLK_PARMS, v | IG4_CLK_PARMS_EN);
            }
        }
        if (self.version == Version::Haswell || self.version == Version::Atom) && reset {
            self.reg_write(IG4_REG_RESETS_HSW, IG4_RESETS_ASSERT_HSW);
            self.reg_write(IG4_REG_RESETS_HSW, IG4_RESETS_DEASSERT_HSW);
        } else if self.version.has_addregs() && reset {
            self.reg_write(IG4_REG_RESETS_SKL, IG4_RESETS_ASSERT_SKL);
            self.reg_write(IG4_REG_RESETS_SKL, IG4_RESETS_DEASSERT_SKL);
            let mut i = 0;
            while i < 100 {
                if self.reg_read(IG4_REG_COMP_TYPE) == IG4_COMP_TYPE {
                    break;
                }
                self.io.delay_us(10);
                i += 1;
            }
        }
        if self.version == Version::Atom {
            let _ = self.reg_read(IG4_REG_COMP_TYPE);
        }
        if self.version == Version::Haswell || self.version == Version::Atom {
            let _ = self.reg_read(IG4_REG_COMP_PARAM1);
            v = self.reg_read(IG4_REG_GENERAL);
            if self.version == Version::Haswell && v & IG4_GENERAL_SWMODE == 0 {
                v |= IG4_GENERAL_SWMODE;
                self.reg_write(IG4_REG_GENERAL, v);
                let _ = self.reg_read(IG4_REG_GENERAL);
            }
        }
        if self.version == Version::Haswell {
            let _ = self.reg_read(IG4_REG_SW_LTR_VALUE);
            let _ = self.reg_read(IG4_REG_AUTO_LTR_VALUE);
        } else if self.version.has_addregs() {
            let _ = self.reg_read(IG4_REG_ACTIVE_LTR_VALUE);
            let _ = self.reg_read(IG4_REG_IDLE_LTR_VALUE);
        }
        if (self.version == Version::Haswell || self.version == Version::Atom)
            && self.reg_read(IG4_REG_COMP_VER) < IG4_COMP_MIN_VER
        {
            return Err(IicError::HardwareUnavailable);
        }
        if self.set_controller(0) != IicError::NoError {
            return Err(IicError::HardwareUnavailable);
        }
        self.reg_read(IG4_REG_CLR_INTR);
        self.reg_write(IG4_REG_INTR_MASK, 0);
        self.intr_mask = 0;
        self.reg_write(IG4_REG_SS_SCL_HCNT, u32::from(self.cfg.ss_scl_hcnt));
        self.reg_write(IG4_REG_SS_SCL_LCNT, u32::from(self.cfg.ss_scl_lcnt));
        self.reg_write(IG4_REG_FS_SCL_HCNT, u32::from(self.cfg.fs_scl_hcnt));
        self.reg_write(IG4_REG_FS_SCL_LCNT, u32::from(self.cfg.fs_scl_lcnt));
        self.reg_write(
            IG4_REG_SDA_HOLD,
            u32::from(
                if self.cfg.bus_speed & IG4_CTL_SPEED_MASK == IG4_CTL_SPEED_STD {
                    self.cfg.ss_sda_hold
                } else {
                    self.cfg.fs_sda_hold
                },
            ),
        );
        self.reg_write(IG4_REG_RX_TL, 0);
        self.reg_write(IG4_REG_TX_TL, 0);
        self.reg_write(
            IG4_REG_CTL,
            IG4_CTL_MASTER
                | IG4_CTL_SLAVE_DISABLE
                | IG4_CTL_RESTARTEN
                | (self.cfg.bus_speed & IG4_CTL_SPEED_MASK),
        );
        self.slave_valid = false;
        Ok(())
    }

    // upstream: ig4_iic.c ig4iic_get_fifo()
    pub fn ig4iic_get_fifo(&mut self) {
        let mut v;
        if self.cfg.txfifo_depth == 0 {
            v = self.reg_read(IG4_REG_TX_TL);
            self.reg_write(IG4_REG_TX_TL, v | IG4_FIFO_MASK);
            self.cfg.txfifo_depth = ((self.reg_read(IG4_REG_TX_TL) & IG4_FIFO_MASK) + 1) as i32;
            self.reg_write(IG4_REG_TX_TL, v);
        }
        if self.cfg.rxfifo_depth == 0 {
            v = self.reg_read(IG4_REG_RX_TL);
            self.reg_write(IG4_REG_RX_TL, v | IG4_FIFO_MASK);
            self.cfg.rxfifo_depth = ((self.reg_read(IG4_REG_RX_TL) & IG4_FIFO_MASK) + 1) as i32;
            self.reg_write(IG4_REG_RX_TL, v);
        }
    }

    // upstream: ig4_iic.c ig4iic_attach()
    pub fn ig4iic_attach(&mut self) -> Result<(), IicError> {
        self.io.init_locks();
        self.ig4iic_get_config();
        let reset = self.version.has_addregs();
        self.ig4iic_set_config(reset, false)?;
        self.ig4iic_get_fifo();
        if !self.io.add_iicbus_child() {
            return Err(IicError::HardwareUnavailable);
        }
        if self.set_controller(IG4_I2C_ENABLE) != IicError::NoError {
            return Err(IicError::HardwareUnavailable);
        }
        if self.set_controller(0) != IicError::NoError {
            return Err(IicError::HardwareUnavailable);
        }
        let irq = self.io.setup_interrupt();
        self.io.attach_iicbus_children();
        irq
    }
    // upstream: ig4_iic.c ig4iic_detach()
    pub fn ig4iic_detach(&mut self) -> Result<(), IicError> {
        self.io.detach_iicbus_children()?;
        self.io.teardown_interrupt();
        self.io.call_lock();
        self.io.write32(IG4_REG_INTR_MASK, 0);
        self.set_controller(0);
        self.io.call_unlock();
        self.io.destroy_locks();
        Ok(())
    }
    // upstream: ig4_iic.c ig4iic_suspend()
    pub fn ig4iic_suspend(&mut self) -> Result<(), IicError> {
        let error = self.io.suspend_iicbus_children();
        self.io.call_lock();
        self.set_controller(0);
        if self.version.has_addregs() {
            self.reg_write(IG4_REG_DEVIDLE_CTRL, IG4_DEVICE_IDLE);
            self.reg_write(IG4_REG_RESETS_SKL, IG4_RESETS_ASSERT_SKL);
        }
        self.io.call_unlock();
        error
    }
    // upstream: ig4_iic.c ig4iic_resume()
    pub fn ig4iic_resume(&mut self) -> Result<(), IicError> {
        self.io.call_lock();
        let _ = self.ig4iic_set_config(self.version.has_addregs(), true);
        self.io.call_unlock();
        self.io.resume_iicbus_children()
    }
    // upstream: ig4_iic.c ig4iic_intr()
    pub fn ig4iic_intr(&mut self) -> bool {
        if self.intr_mask != 0 && self.reg_read(IG4_REG_INTR_STAT) != 0 {
            self.ig4iic_set_intr_mask(0);
            self.io.wake_waiters();
            true
        } else {
            false
        }
    }
    // upstream: ig4_iic.c ig4iic_dump()
    pub fn ig4iic_dump(&mut self) {
        for (name, reg) in [
            ("IG4_REG_CTL", IG4_REG_CTL),
            ("IG4_REG_TAR_ADD", IG4_REG_TAR_ADD),
            ("IG4_REG_SS_SCL_HCNT", IG4_REG_SS_SCL_HCNT),
            ("IG4_REG_SS_SCL_LCNT", IG4_REG_SS_SCL_LCNT),
            ("IG4_REG_FS_SCL_HCNT", IG4_REG_FS_SCL_HCNT),
            ("IG4_REG_FS_SCL_LCNT", IG4_REG_FS_SCL_LCNT),
            ("IG4_REG_INTR_STAT", IG4_REG_INTR_STAT),
            ("IG4_REG_INTR_MASK", IG4_REG_INTR_MASK),
            ("IG4_REG_RAW_INTR_STAT", IG4_REG_RAW_INTR_STAT),
            ("IG4_REG_RX_TL", IG4_REG_RX_TL),
            ("IG4_REG_TX_TL", IG4_REG_TX_TL),
            ("IG4_REG_I2C_EN", IG4_REG_I2C_EN),
            ("IG4_REG_I2C_STA", IG4_REG_I2C_STA),
            ("IG4_REG_TXFLR", IG4_REG_TXFLR),
            ("IG4_REG_RXFLR", IG4_REG_RXFLR),
            ("IG4_REG_SDA_HOLD", IG4_REG_SDA_HOLD),
            ("IG4_REG_TX_ABRT_SOURCE", IG4_REG_TX_ABRT_SOURCE),
            ("IG4_REG_SLV_DATA_NACK", IG4_REG_SLV_DATA_NACK),
            ("IG4_REG_DMA_CTRL", IG4_REG_DMA_CTRL),
            ("IG4_REG_DMA_TDLR", IG4_REG_DMA_TDLR),
            ("IG4_REG_DMA_RDLR", IG4_REG_DMA_RDLR),
            ("IG4_REG_SDA_SETUP", IG4_REG_SDA_SETUP),
            ("IG4_REG_ENABLE_STATUS", IG4_REG_ENABLE_STATUS),
            ("IG4_REG_COMP_PARAM1", IG4_REG_COMP_PARAM1),
            ("IG4_REG_COMP_VER", IG4_REG_COMP_VER),
        ] {
            let v = self.reg_read(reg);
            self.io.debug_register(name, v);
        }
        if self.version == Version::Atom {
            let v = self.reg_read(IG4_REG_COMP_TYPE);
            self.io.debug_register("IG4_REG_COMP_TYPE", v);
            let v = self.reg_read(IG4_REG_CLK_PARMS);
            self.io.debug_register("IG4_REG_CLK_PARMS", v);
        }
        if self.version == Version::Haswell || self.version == Version::Atom {
            let v = self.reg_read(IG4_REG_RESETS_HSW);
            self.io.debug_register("IG4_REG_RESETS_HSW", v);
            let v = self.reg_read(IG4_REG_GENERAL);
            self.io.debug_register("IG4_REG_GENERAL", v);
        } else if self.version == Version::Skylake {
            let v = self.reg_read(IG4_REG_RESETS_SKL);
            self.io.debug_register("IG4_REG_RESETS_SKL", v);
        }
        if self.version == Version::Haswell {
            let v = self.reg_read(IG4_REG_SW_LTR_VALUE);
            self.io.debug_register("IG4_REG_SW_LTR_VALUE", v);
            let v = self.reg_read(IG4_REG_AUTO_LTR_VALUE);
            self.io.debug_register("IG4_REG_AUTO_LTR_VALUE", v);
        } else if self.version.has_addregs() {
            let v = self.reg_read(IG4_REG_ACTIVE_LTR_VALUE);
            self.io.debug_register("IG4_REG_ACTIVE_LTR_VALUE", v);
            let v = self.reg_read(IG4_REG_IDLE_LTR_VALUE);
            self.io.debug_register("IG4_REG_IDLE_LTR_VALUE", v);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::*;

    struct Fake {
        registers: [u32; 520],
        writes: Vec<(u32, u32)>,
        lock: bool,
    }
    impl Fake {
        fn new() -> Self {
            let mut registers = [0; 520];
            registers[(IG4_REG_CTL / 4) as usize] = IG4_CTL_SPEED_FAST;
            registers[(IG4_REG_COMP_VER / 4) as usize] = IG4_COMP_MIN_VER;
            registers[(IG4_REG_RAW_INTR_STAT / 4) as usize] = IG4_INTR_TX_EMPTY | IG4_INTR_STOP_DET;
            registers[(IG4_REG_I2C_STA / 4) as usize] = IG4_STATUS_TX_EMPTY;
            registers[(IG4_REG_TX_TL / 4) as usize] = 15;
            registers[(IG4_REG_RX_TL / 4) as usize] = 15;
            Self {
                registers,
                writes: Vec::new(),
                lock: false,
            }
        }
    }
    impl Backend for Fake {
        fn read32(&mut self, r: u32) -> u32 {
            self.registers[(r / 4) as usize]
        }
        fn write32(&mut self, r: u32, v: u32) {
            self.writes.push((r, v));
            if r != IG4_REG_DATA_CMD {
                self.registers[(r / 4) as usize] = v;
            }
            if r == IG4_REG_I2C_EN {
                self.registers[(IG4_REG_ENABLE_STATUS / 4) as usize] = v & IG4_I2C_ENABLE;
            }
        }
        fn delay_us(&mut self, _: u32) {}
        fn pause_ms(&mut self, _: &'static str, _: u32) {}
        fn do_poll(&self) -> bool {
            true
        }
        fn wait_irq(&mut self, _: u32) {}
        fn acpi_clock_params(&mut self, _: &str) -> Result<[u64; 3], ()> {
            Err(())
        }
        fn add_iicbus_child(&mut self) -> bool {
            true
        }
        fn attach_iicbus_children(&mut self) {}
        fn detach_iicbus_children(&mut self) -> Result<(), IicError> {
            Ok(())
        }
        fn setup_interrupt(&mut self) -> Result<(), IicError> {
            Ok(())
        }
        fn teardown_interrupt(&mut self) {}
        fn suspend_iicbus_children(&mut self) -> Result<(), IicError> {
            Ok(())
        }
        fn resume_iicbus_children(&mut self) -> Result<(), IicError> {
            Ok(())
        }
        fn debug_register(&mut self, _: &'static str, _: u32) {}
        fn init_locks(&mut self) {}
        fn destroy_locks(&mut self) {}
        fn call_lock_owned(&self) -> bool {
            self.lock
        }
        fn call_try_lock(&mut self) -> bool {
            if self.lock {
                false
            } else {
                self.lock = true;
                true
            }
        }
        fn call_lock(&mut self) {
            self.lock = true;
        }
        fn call_unlock(&mut self) {
            self.lock = false;
        }
        fn wake_waiters(&mut self) {}
    }

    #[test]
    fn clock_counts_match_upstream_equations() {
        let mut standard = [0; 3];
        Ig4::<Fake>::ig4iic_clk_params(
            &IG4IIC_HW[Version::TigerLake as usize],
            IG4_CTL_SPEED_STD as i32,
            &mut standard,
        )
        .unwrap();
        assert_eq!(standard, [552, 652, 6]);
        let mut fast = [0; 3];
        Ig4::<Fake>::ig4iic_clk_params(
            &IG4IIC_HW[Version::TigerLake as usize],
            IG4_CTL_SPEED_FAST as i32,
            &mut fast,
        )
        .unwrap();
        assert_eq!(fast, [100, 200, 6]);
    }

    #[test]
    fn transfer_queues_data_and_stop_with_upstream_flags() {
        let mut ig4 = Ig4::new(Fake::new(), Version::TigerLake, 3);
        ig4.cfg = Config {
            bus_speed: IG4_CTL_SPEED_FAST,
            txfifo_depth: 16,
            rxfifo_depth: 16,
            ..Config::default()
        };
        let mut bytes = [0x3a, 0x7f];
        let mut messages = [IicMessage {
            slave: 0x50 << 1,
            flags: 0,
            buf: &mut bytes,
        }];
        assert_eq!(ig4.ig4iic_transfer(&mut messages), IicError::NoError);
        let writes = &ig4.io.writes;
        assert!(writes.contains(&(IG4_REG_TAR_ADD, 0x50)));
        assert!(writes.contains(&(IG4_REG_DATA_CMD, 0x3a)));
        assert!(writes.contains(&(IG4_REG_DATA_CMD, 0x7f | IG4_DATA_STOP)));
    }

    #[test]
    fn combined_write_read_uses_repeated_start_and_drains_rx_fifo() {
        let mut ig4 = Ig4::new(Fake::new(), Version::TigerLake, 3);
        ig4.cfg = Config {
            bus_speed: IG4_CTL_SPEED_FAST,
            txfifo_depth: 16,
            rxfifo_depth: 16,
            ..Config::default()
        };
        ig4.io.registers[(IG4_REG_RXFLR / 4) as usize] = 1;
        ig4.io.registers[(IG4_REG_DATA_CMD / 4) as usize] = 0x5a;
        ig4.io.registers[(IG4_REG_RAW_INTR_STAT / 4) as usize] |= IG4_INTR_RX_FULL;
        let mut register = [0x09];
        let mut read = [0];
        let mut messages = [
            IicMessage {
                slave: 0x50 << 1,
                flags: IIC_M_NOSTOP,
                buf: &mut register,
            },
            IicMessage {
                slave: 0x50 << 1,
                flags: IIC_M_RD,
                buf: &mut read,
            },
        ];
        assert_eq!(ig4.ig4iic_transfer(&mut messages), IicError::NoError);
        assert_eq!(read, [0x5a]);
        assert!(ig4.io.writes.contains(&(IG4_REG_DATA_CMD, 0x09)));
        assert!(ig4.io.writes.contains(&(
            IG4_REG_DATA_CMD,
            IG4_DATA_COMMAND_RD | IG4_DATA_RESTART | IG4_DATA_STOP
        )));
    }
}
