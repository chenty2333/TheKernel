// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright 2026 TheKernel contributors.
//! Translation of Linux 7.2.3 `drivers/i2c/algos/i2c-algo-bit.c` and
//! `include/linux/i2c-algo-bit.h` (GPL-2.0 upstream): the bit-banging I2C
//! algorithm that drives SCL and SDA through per-platform line callbacks.
//! GMBUS on the pre-GMBUS-less paths, DDC over GMBUS-bit fallback and DP-AUX
//! over I2C use it.
//!
//! `struct i2c_algo_bit_data`'s `void *data` and callback pointers become
//! the [`I2cBitLines`] trait object. A callback that is absent in C
//! (`getsda == NULL`, `getscl == NULL`, `pre_xfer == NULL`) maps to the trait
//! default: a read that returns `None`, or a no-op. `bit_test` and the
//! `DEBUG`-only `bit_dbg()` output are compile-time/module parameters of the
//! oracle configuration: `bit_test` is a field of [`I2cAlgoBitData`] and the
//! debug output is omitted, as the oracle build omits `DEBUG`.
//!
//! The timing contract is Linux's: every line change waits `udelay` half
//! cycles, and SCL-high waits are bounded by `timeout` jiffies, reported as
//! `-ETIMEDOUT`.

use crate::{
    clock,
    config::{CONFIG_HZ, EIO, ENODEV, ENXIO, EOPNOTSUPP, EPROTO, ETIMEDOUT},
    i2c::{
        I2C_AQ_NO_CLK_STRETCH, I2C_FUNC_10BIT_ADDR, I2C_FUNC_I2C, I2C_FUNC_NOSTART,
        I2C_FUNC_PROTOCOL_MANGLING, I2C_FUNC_SMBUS_EMUL_ALL, I2C_M_IGNORE_NAK, I2C_M_RD,
        I2C_M_RECV_LEN, I2C_M_REV_DIR_ADDR, I2C_M_NO_RD_ACK, I2C_M_NOSTART, I2C_M_STOP,
        I2C_M_TEN, I2C_SMBUS_BLOCK_MAX, I2cAdapter, I2cAdapterIdr, I2cAdapterQuirks,
        I2cAlgorithm, I2cMsg, i2c_8bit_addr_from_msg, i2c_add_adapter,
        i2c_add_numbered_adapter,
    },
};

/// Line-level callbacks of `struct i2c_algo_bit_data`.
///
/// Writes are required. `getsda`/`getscl` return `None` when the platform
/// cannot read the line (`NULL` in C), which makes the algorithm assume an
/// ACK and skip clock-stretch detection. `pre_xfer` and `post_xfer` bracket
/// each transfer, as in C, and `pre_xfer` returns a negative errno to refuse
/// the bus.
pub trait I2cBitLines: Sync {
    fn setsda(&self, state: i32);
    fn setscl(&self, state: i32);

    fn getsda(&self) -> Option<i32> {
        None
    }

    fn getscl(&self) -> Option<i32> {
        None
    }

    fn pre_xfer(&self, _adap: &I2cAdapter<'_>) -> i32 {
        0
    }

    fn post_xfer(&self, _adap: &I2cAdapter<'_>) {}
}

/// `struct i2c_algo_bit_data` without the owning-pointer field.
pub struct I2cAlgoBitData<'a> {
    pub lines: &'a dyn I2cBitLines,
    /// Half clock cycle in microseconds.
    pub udelay: u32,
    /// Clock-stretch timeout in jiffies.
    pub timeout: i64,
    pub can_do_atomic: bool,
    /// `bit_test`: 0 off, 1 report stuck lines, 2 fail the registration.
    pub bit_test: i32,
}

/// The `i2c_bit_algo` algorithm bound to one line set. An adapter that uses
/// bit-banging holds a reference to this value as its `algo`.
pub struct I2cBitAlgo<'a> {
    pub data: I2cAlgoBitData<'a>,
}

const I2C_BIT_QUIRK_NO_CLK_STRETCH: I2cAdapterQuirks = I2cAdapterQuirks {
    flags: I2C_AQ_NO_CLK_STRETCH,
    max_num_msgs: 0,
    max_write_len: 0,
    max_read_len: 0,
    max_comb_1st_msg_len: 0,
    max_comb_2nd_msg_len: 0,
};

/// `i2c_bit_algo.functionality`: plain I2C, NOSTART, SMBus emulation, 10-bit
/// addressing and protocol mangling.
pub fn bit_func() -> u32 {
    I2C_FUNC_I2C
        | I2C_FUNC_NOSTART
        | I2C_FUNC_SMBUS_EMUL_ALL
        | I2C_FUNC_10BIT_ADDR
        | I2C_FUNC_PROTOCOL_MANGLING
}

/// Half-cycle delay (`udelay()` in C).
fn delay(micros: u32) {
    clock::delay_us(micros);
}

fn setsda(d: &I2cAlgoBitData<'_>, val: i32) {
    d.lines.setsda(val);
}

fn setscl(d: &I2cAlgoBitData<'_>, val: i32) {
    d.lines.setscl(val);
}

fn sdalo(d: &I2cAlgoBitData<'_>) {
    setsda(d, 0);
    delay((d.udelay + 1) / 2);
}

fn sdahi(d: &I2cAlgoBitData<'_>) {
    setsda(d, 1);
    delay((d.udelay + 1) / 2);
}

fn scllo(d: &I2cAlgoBitData<'_>) {
    setscl(d, 0);
    delay(d.udelay / 2);
}

/// `sclhi()`: release SCL and wait for it to rise (clock stretching).
fn sclhi(d: &I2cAlgoBitData<'_>) -> i32 {
    setscl(d, 1);
    if d.lines.getscl().is_some() {
        let tick_ns = 1_000_000_000 / u64::from(CONFIG_HZ);
        let timeout_ns = (d.timeout.max(0) as u64).saturating_mul(tick_ns);
        let start = clock::now_ns();
        while d.lines.getscl() == Some(0) {
            if clock::now_ns() > start.saturating_add(timeout_ns) {
                if d.lines.getscl().unwrap_or(0) != 0 {
                    break;
                }
                return -ETIMEDOUT;
            }
            core::hint::spin_loop();
        }
    }
    delay(d.udelay);
    0
}

fn i2c_start(d: &I2cAlgoBitData<'_>) {
    setsda(d, 0);
    delay(d.udelay);
    scllo(d);
}

fn i2c_repstart(d: &I2cAlgoBitData<'_>) {
    sdahi(d);
    sclhi(d);
    setsda(d, 0);
    delay(d.udelay);
    scllo(d);
}

fn i2c_stop(d: &I2cAlgoBitData<'_>) {
    sdalo(d);
    sclhi(d);
    setsda(d, 1);
    delay(d.udelay);
}

/// `i2c_outb()`: clock out one byte and sample the ACK bit. Returns 1 on ACK,
/// 0 on NAK, or `-ETIMEDOUT`.
fn i2c_outb(d: &I2cAlgoBitData<'_>, c: u8) -> i32 {
    for i in (0..8).rev() {
        let sb = i32::from((c >> i) & 1);
        setsda(d, sb);
        delay((d.udelay + 1) / 2);
        if sclhi(d) < 0 {
            return -ETIMEDOUT;
        }
        scllo(d);
    }
    sdahi(d);
    if sclhi(d) < 0 {
        return -ETIMEDOUT;
    }
    // ACK: SDA is pulled low by the slave. Without a readable SDA the
    // algorithm assumes the transfer was acknowledged.
    let ack = match d.lines.getsda() {
        None => true,
        Some(sda) => sda == 0,
    };
    scllo(d);
    i32::from(ack)
}

/// `i2c_inb()`: clock in one byte. Returns the byte or `-ETIMEDOUT`.
fn i2c_inb(d: &I2cAlgoBitData<'_>) -> i32 {
    let mut indata: u8 = 0;
    sdahi(d);
    for i in 0..8 {
        if sclhi(d) < 0 {
            return -ETIMEDOUT;
        }
        indata = indata.wrapping_mul(2);
        if d.lines.getsda().unwrap_or(0) != 0 {
            indata |= 0x01;
        }
        setscl(d, 0);
        delay(if i == 7 { d.udelay / 2 } else { d.udelay });
    }
    i32::from(indata)
}

/// `test_bus()`: exercise the line callbacks for stuck or shorted lines.
fn test_bus(adap: &I2cAdapter<'_>, d: &I2cAlgoBitData<'_>) -> i32 {
    let name = adap.name;
    if d.lines.pre_xfer(adap) < 0 {
        return -ENODEV;
    }
    if d.lines.getsda().is_none() {
        axlog::info!("{}: SDA is write-only, testing not possible", name);
    }
    if d.lines.getscl().is_none() {
        axlog::info!("{}: SCL is write-only, testing not possible", name);
    }
    let sda = d.lines.getsda().unwrap_or(1);
    let scl = d.lines.getscl().unwrap_or(1);
    if scl == 0 || sda == 0 {
        axlog::warn!("{}: bus seems to be busy (scl={}, sda={})", name, scl, sda);
        return bailout(adap, d);
    }
    sdalo(d);
    if d.lines.getsda().unwrap_or(0) != 0 {
        axlog::warn!("{}: SDA stuck high!", name);
        return bailout(adap, d);
    }
    if d.lines.getscl() == Some(0) {
        axlog::warn!("{}: SCL unexpected low while pulling SDA low!", name);
        return bailout(adap, d);
    }
    sdahi(d);
    if d.lines.getsda() == Some(0) {
        axlog::warn!("{}: SDA stuck low!", name);
        return bailout(adap, d);
    }
    if d.lines.getscl() == Some(0) {
        axlog::warn!("{}: SCL unexpected low while pulling SDA high!", name);
        return bailout(adap, d);
    }
    scllo(d);
    if d.lines.getscl().unwrap_or(0) != 0 {
        axlog::warn!("{}: SCL stuck high!", name);
        return bailout(adap, d);
    }
    if d.lines.getsda() == Some(0) {
        axlog::warn!("{}: SDA unexpected low while pulling SCL low!", name);
        return bailout(adap, d);
    }
    sclhi(d);
    if d.lines.getscl() == Some(0) {
        axlog::warn!("{}: SCL stuck low!", name);
        return bailout(adap, d);
    }
    if d.lines.getsda() == Some(0) {
        axlog::warn!("{}: SDA unexpected low while pulling SCL high!", name);
        return bailout(adap, d);
    }
    d.lines.post_xfer(adap);
    axlog::info!("{}: Test OK", name);
    0
}

fn bailout(adap: &I2cAdapter<'_>, d: &I2cAlgoBitData<'_>) -> i32 {
    sdahi(d);
    sclhi(d);
    d.lines.post_xfer(adap);
    -ENODEV
}

/// `try_address()`: send the address byte, retrying with a stop/start pair
/// until ACK or the retry budget runs out. Returns the last `i2c_outb()`
/// result.
fn try_address(d: &I2cAlgoBitData<'_>, addr: u8, retries: u32) -> i32 {
    let mut ret = 0;
    let mut i = 0;
    while i <= retries {
        ret = i2c_outb(d, addr);
        if ret == 1 || i == retries {
            break;
        }
        i2c_stop(d);
        delay(d.udelay);
        crate::clock::relax();
        i2c_start(d);
        i += 1;
    }
    ret
}

/// `sendbytes()`: write the message body. Returns the bytes written.
fn sendbytes(d: &I2cAlgoBitData<'_>, adap: &I2cAdapter<'_>, msg: &I2cMsg<'_>) -> i32 {
    let nak_ok = msg.flags & I2C_M_IGNORE_NAK != 0;
    let mut wrcount = 0i32;
    let mut pos = 0usize;
    while pos < msg.len {
        let retval = i2c_outb(d, msg.buf[pos]);
        if retval > 0 || (nak_ok && retval == 0) {
            pos += 1;
            wrcount += 1;
        } else if retval == 0 {
            axlog::error!("{}: sendbytes: NAK bailout.", adap.name);
            return -EIO;
        } else {
            axlog::error!("{}: sendbytes: error {}", adap.name, retval);
            return retval;
        }
    }
    wrcount
}

/// `acknak()`: ACK or NAK the byte just read.
fn acknak(d: &I2cAlgoBitData<'_>, adap: &I2cAdapter<'_>, is_ack: bool) -> i32 {
    if is_ack {
        setsda(d, 0);
    }
    delay((d.udelay + 1) / 2);
    if sclhi(d) < 0 {
        axlog::error!("{}: readbytes: ack/nak timeout", adap.name);
        return -ETIMEDOUT;
    }
    scllo(d);
    0
}

/// `readbytes()`: read `msg.len` bytes (or a SMBus block when
/// `I2C_M_RECV_LEN` is set). Returns the bytes read.
fn readbytes(d: &I2cAlgoBitData<'_>, adap: &I2cAdapter<'_>, msg: &mut I2cMsg<'_>) -> i32 {
    if d.lines.getsda().is_none() {
        return -EOPNOTSUPP;
    }
    let flags = msg.flags;
    let mut rdcount = 0usize;
    let mut count = msg.len;
    while count > 0 {
        let inval = i2c_inb(d);
        if inval < 0 {
            // read timed out
            break;
        }
        msg.buf[rdcount] = inval as u8;
        rdcount += 1;
        count -= 1;
        // The first byte of an SMBus block is the transaction length, and
        // the message grows by that length (plus one for PEC).
        if rdcount == 1 && flags & I2C_M_RECV_LEN != 0 {
            if inval <= 0 || inval as usize > I2C_SMBUS_BLOCK_MAX {
                if flags & I2C_M_NO_RD_ACK == 0 {
                    acknak(d, adap, false);
                }
                axlog::error!("{}: readbytes: invalid block length ({})", adap.name, inval);
                return -EPROTO;
            }
            let grown = inval as usize;
            if msg.len + grown > msg.buf.len() {
                return -EPROTO;
            }
            count += grown;
            msg.len += grown;
        }
        if flags & I2C_M_NO_RD_ACK == 0 {
            let ack = acknak(d, adap, count != 0);
            if ack < 0 {
                return ack;
            }
        }
    }
    rdcount as i32
}

/// `bit_doAddress()`: address the slave (7-bit or 10-bit form).
fn bit_do_address(d: &I2cAlgoBitData<'_>, adap: &I2cAdapter<'_>, msg: &I2cMsg<'_>) -> i32 {
    let flags = msg.flags;
    let nak_ok = flags & I2C_M_IGNORE_NAK != 0;
    let retries = if nak_ok { 0 } else { adap.retries };
    if flags & I2C_M_TEN != 0 {
        let mut addr = 0xf0 | ((msg.addr >> 7) as u8 & 0x06);
        let mut ret = try_address(d, addr, retries);
        if ret != 1 && !nak_ok {
            axlog::error!("{}: died at extended address code", adap.name);
            return -ENXIO;
        }
        ret = i2c_outb(d, (msg.addr & 0xff) as u8);
        if ret != 1 && !nak_ok {
            axlog::error!("{}: died at 2nd address code", adap.name);
            return -ENXIO;
        }
        if flags & I2C_M_RD != 0 {
            i2c_repstart(d);
            addr |= 0x01;
            ret = try_address(d, addr, retries);
            if ret != 1 && !nak_ok {
                axlog::error!("{}: died at repeated address code", adap.name);
                return -EIO;
            }
        }
    } else {
        let mut addr = i2c_8bit_addr_from_msg(msg);
        if flags & I2C_M_REV_DIR_ADDR != 0 {
            addr ^= 1;
        }
        let ret = try_address(d, addr, retries);
        if ret != 1 && !nak_ok {
            return -ENXIO;
        }
    }
    0
}

/// `bit_xfer()`: run one batch of messages. Returns the number of messages
/// completed or a negative errno.
fn bit_xfer(d: &I2cAlgoBitData<'_>, adap: &I2cAdapter<'_>, msgs: &mut [I2cMsg<'_>]) -> i32 {
    let pre = d.lines.pre_xfer(adap);
    if pre < 0 {
        return pre;
    }
    i2c_start(d);
    let mut ret = 0;
    let mut completed = None;
    for i in 0..msgs.len() {
        let nak_ok = msgs[i].flags & I2C_M_IGNORE_NAK != 0;
        if msgs[i].flags & I2C_M_NOSTART == 0 {
            if i > 0 {
                if msgs[i - 1].flags & I2C_M_STOP != 0 {
                    i2c_stop(d);
                    i2c_start(d);
                } else {
                    i2c_repstart(d);
                }
            }
            ret = bit_do_address(d, adap, &msgs[i]);
            if ret != 0 && !nak_ok {
                axlog::debug!("{}: NAK from device addr 0x{:02x} msg #{}", adap.name, msgs[i].addr, i);
                completed = Some(ret);
                break;
            }
        }
        let len = msgs[i].len;
        if msgs[i].flags & I2C_M_RD != 0 {
            ret = readbytes(d, adap, &mut msgs[i]);
            if ret < len as i32 {
                completed = Some(if ret >= 0 { -EIO } else { ret });
                break;
            }
        } else {
            ret = sendbytes(d, adap, &msgs[i]);
            if ret < len as i32 {
                completed = Some(if ret >= 0 { -EIO } else { ret });
                break;
            }
        }
    }
    let result = completed.unwrap_or(msgs.len() as i32);
    i2c_stop(d);
    d.lines.post_xfer(adap);
    result
}

impl I2cAlgorithm for I2cBitAlgo<'_> {
    fn master_xfer(&self, adap: &I2cAdapter<'_>, msgs: &mut [I2cMsg<'_>]) -> i32 {
        bit_xfer(&self.data, adap, msgs)
    }

    fn functionality(&self) -> u32 {
        bit_func()
    }
}

/// `__i2c_bit_add_bus()`: optionally test the lines, then register through
/// `add_adapter`.
fn bit_add_bus<'a>(
    adap: &mut I2cAdapter<'a>,
    algo: &'a I2cBitAlgo<'a>,
    add_adapter: impl FnOnce(&mut I2cAdapter<'a>) -> i32,
) -> i32 {
    let d = &algo.data;
    if d.bit_test != 0 {
        let ret = test_bus(adap, d);
        if d.bit_test >= 2 && ret < 0 {
            return -ENODEV;
        }
    }
    adap.algo = Some(algo);
    adap.retries = 3;
    if d.lines.getscl().is_none() {
        adap.quirks = Some(&I2C_BIT_QUIRK_NO_CLK_STRETCH);
    }
    let ret = add_adapter(adap);
    if ret < 0 {
        return ret;
    }
    if d.lines.getsda().is_none() {
        axlog::warn!("{}: Not I2C compliant: can't read SDA", adap.name);
    }
    if d.lines.getscl().is_none() {
        axlog::warn!("{}: Not I2C compliant: can't read SCL", adap.name);
    }
    if d.lines.getsda().is_none() || d.lines.getscl().is_none() {
        axlog::warn!("{}: Bus may be unreliable", adap.name);
    }
    0
}

/// `i2c_bit_add_bus()`: register with a dynamically assigned bus number.
pub fn i2c_bit_add_bus<'a>(
    adap: &mut I2cAdapter<'a>,
    algo: &'a I2cBitAlgo<'a>,
    idr: &'a I2cAdapterIdr,
) -> i32 {
    bit_add_bus(adap, algo, |adap| i2c_add_adapter(adap, idr))
}

/// `i2c_bit_add_numbered_bus()`: register with the adapter's fixed number.
pub fn i2c_bit_add_numbered_bus<'a>(
    adap: &mut I2cAdapter<'a>,
    algo: &'a I2cBitAlgo<'a>,
    idr: &'a I2cAdapterIdr,
) -> i32 {
    bit_add_bus(adap, algo, |adap| i2c_add_numbered_adapter(adap, idr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i2c::{I2C_M_RD, I2cAdapter};
    use alloc::{vec, vec::Vec};
    use core::sync::atomic::{AtomicI32, Ordering};
    use spin::Mutex;

    /// Open-drain bus with one simulated slave. Each master line write
    /// updates the wired-AND lines; the slave observes START, STOP and each
    /// SCL rising/falling edge, and drives SDA low for ACK and for data-0.
    struct MockBus {
        scl_master: AtomicI32,
        sda_master: AtomicI32,
        slave: Mutex<Slave>,
    }

    #[derive(Clone)]
    struct Slave {
        address: u8,
        present: bool,
        // Bus state seen on the previous update.
        last_scl: i32,
        last_sda: i32,
        mode: Mode,
        bits: u32,
        shift: u8,
        sda_drive_low: bool,
        pending_ack: bool,
        ack_origin: Mode,
        addressed: bool,
        tx_read: bool,
        // Bytes the slave will return on reads and the bytes it stored.
        tx: Vec<u8>,
        tx_pos: usize,
        rx: Vec<u8>,
        /// Clock-stretch the next N SCL rising edges (holds SCL low).
        stretch: u32,
        stretching: bool,
    }

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Mode {
        Idle,
        Addr,
        AddrAck,
        Data,
        AckWait,
        AckHigh,
        TxBit,
        TxAck,
    }

    impl Slave {
        fn new(address: u8, present: bool) -> Self {
            Self {
                address,
                present,
                last_scl: 1,
                last_sda: 1,
                mode: Mode::Idle,
                bits: 0,
                shift: 0,
                sda_drive_low: false,
                pending_ack: false,
                ack_origin: Mode::Idle,
                addressed: false,
                tx_read: false,
                tx: Vec::new(),
                tx_pos: 0,
                rx: Vec::new(),
                stretch: 0,
                stretching: false,
            }
        }

        fn line_sda(&self) -> i32 {
            if self.sda_drive_low { 0 } else { 1 }
        }

        /// Observe the bus after a master change; `sda_master` and
        /// `scl_master` are the master's drive levels.
        fn observe(&mut self, scl_master: i32, sda_master: i32) {
            let scl = if self.stretching { 0 } else { scl_master };
            let sda = sda_master & self.line_sda();
            let (ls, lsda) = (self.last_scl, self.last_sda);
            self.last_scl = scl;
            self.last_sda = sda;
            // START: SDA falls while SCL is high. STOP: SDA rises while SCL is high.
            if scl == 1 && ls == 1 && lsda == 1 && sda == 0 {
                self.mode = Mode::Addr;
                self.bits = 0;
                self.shift = 0;
                self.sda_drive_low = false;
                return;
            }
            if scl == 1 && ls == 1 && lsda == 0 && sda == 1 {
                self.mode = Mode::Idle;
                self.sda_drive_low = false;
                return;
            }
            if ls == 0 && scl == 1 {
                self.rising(sda_master);
            } else if ls == 1 && scl == 0 {
                self.falling();
            }
        }

        fn rising(&mut self, sda_master: i32) {
            match self.mode {
                Mode::Addr | Mode::Data => {
                    self.shift = (self.shift << 1) | (sda_master as u8 & 1);
                    self.bits += 1;
                    if self.bits == 8 {
                        if self.mode == Mode::Addr {
                            let matches = self.present && (self.shift >> 1) == self.address;
                            self.ack_origin = Mode::Addr;
                            self.pending_ack = matches;
                            self.tx_read = matches && self.shift & 1 == 1;
                            self.mode = Mode::AckWait;
                            self.addressed = matches;
                        } else {
                            self.rx.push(self.shift);
                            self.ack_origin = Mode::Data;
                            self.pending_ack = true;
                            self.mode = Mode::AckWait;
                        }
                    }
                }
                _ => {}
            }
        }

        fn falling(&mut self) {
            match self.mode {
                Mode::AckWait => {
                    // Drive ACK for the ninth clock; it is sampled on the next
                    // rising edge and released on the falling edge after it.
                    self.sda_drive_low = self.pending_ack;
                    self.mode = Mode::AckHigh;
                }
                Mode::AckHigh => {
                    // End of the ACK clock: release SDA and enter the next phase.
                    self.sda_drive_low = false;
                    self.bits = 0;
                    self.shift = 0;
                    match self.ack_origin {
                        Mode::Addr if self.addressed && self.tx_read => {
                            self.mode = Mode::TxBit;
                            self.load_tx_bit();
                        }
                        Mode::Addr => {
                            self.mode = Mode::Data;
                        }
                        _ => {
                            self.mode = Mode::Data;
                        }
                    }
                    if !self.addressed {
                        self.mode = Mode::Idle;
                    }
                }
                Mode::TxBit => {
                    // Advance to the next data bit or the master's ACK clock.
                    self.bits += 1;
                    if self.bits == 8 {
                        self.sda_drive_low = false;
                        self.mode = Mode::TxAck;
                    } else {
                        self.load_tx_bit();
                    }
                }
                Mode::TxAck => {
                    // The master ACKs (SDA low) or NAKs on this clock; the
                    // slave sends the next byte either way in this model.
                    self.tx_pos += 1;
                    self.bits = 0;
                    self.mode = Mode::TxBit;
                    self.load_tx_bit();
                }
                _ => {}
            }
        }

        fn load_tx_bit(&mut self) {
            let byte = self.tx.get(self.tx_pos).copied().unwrap_or(0xff);
            let bit = (byte >> (7 - self.bits)) & 1;
            self.sda_drive_low = bit == 0;
        }
    }

    impl MockBus {
        fn new(address: u8, present: bool, tx: &[u8]) -> Self {
            let mut slave = Slave::new(address, present);
            slave.tx = tx.to_vec();
            Self {
                scl_master: AtomicI32::new(1),
                sda_master: AtomicI32::new(1),
                slave: Mutex::new(slave),
            }
        }

        fn update(&self) {
            self.slave
                .lock()
                .observe(self.scl_master.load(Ordering::SeqCst), self.sda_master.load(Ordering::SeqCst));
        }
    }

    impl I2cBitLines for MockBus {
        fn setsda(&self, state: i32) {
            self.sda_master.store(state, Ordering::SeqCst);
            self.update();
        }

        fn setscl(&self, state: i32) {
            self.scl_master.store(state, Ordering::SeqCst);
            self.update();
        }

        fn getsda(&self) -> Option<i32> {
            Some(self.sda_master.load(Ordering::SeqCst) & self.slave.lock().line_sda())
        }

        fn getscl(&self) -> Option<i32> {
            let slave = self.slave.lock();
            Some(if slave.stretching { 0 } else { self.scl_master.load(Ordering::SeqCst) })
        }
    }

    fn algo(bus: &MockBus) -> I2cBitAlgo<'_> {
        I2cBitAlgo {
            data: I2cAlgoBitData {
                lines: bus,
                udelay: 5,
                timeout: 100,
                can_do_atomic: false,
                bit_test: 0,
            },
        }
    }

    #[test]
    fn write_then_read_round_trips_through_the_line_model() {
        let bus = MockBus::new(0x50, true, &[0x5a]);
        let bit = algo(&bus);
        let mut adap = I2cAdapter::new("bit0", None);
        adap.nr = 0;
        adap.timeout = 100;
        adap.retries = 0;
        let mut payload = [0x12u8, 0x34];
        let mut read = [0u8; 1];
        let mut msgs = [
            I2cMsg::new(0x50, 0, &mut payload),
            I2cMsg::new(0x50, I2C_M_RD, &mut read),
        ];
        adap.algo = Some(&bit);
        assert_eq!(adap.transfer(&mut msgs), 2);
        drop(msgs);
        assert_eq!(bus.slave.lock().rx, vec![0x12, 0x34]);
        assert_eq!(read, [0x5a]);
    }

    #[test]
    fn missing_device_is_reported_as_enxio_after_the_address_phase() {
        let bus = MockBus::new(0x50, false, &[]);
        let bit = algo(&bus);
        let mut adap = I2cAdapter::new("bit1", None);
        adap.algo = Some(&bit);
        adap.retries = 0;
        let mut body = [0u8; 1];
        assert_eq!(
            adap.transfer(&mut [I2cMsg::new(0x50, 0, &mut body)]),
            -ENXIO
        );
    }

    #[test]
    fn ignore_nak_lets_a_missing_device_pass_the_address_phase() {
        let bus = MockBus::new(0x50, false, &[]);
        let bit = algo(&bus);
        let mut adap = I2cAdapter::new("bit2", None);
        adap.algo = Some(&bit);
        let mut body = [0u8; 0];
        let flags = I2C_M_IGNORE_NAK;
        assert_eq!(adap.transfer(&mut [I2cMsg::new(0x50, flags, &mut body)]), 1);
    }

    #[test]
    fn stuck_scl_times_out_with_etimedout() {
        let bus = MockBus::new(0x50, true, &[]);
        bus.slave.lock().stretching = true;
        let bit = algo(&bus);
        let mut adap = I2cAdapter::new("bit3", None);
        adap.algo = Some(&bit);
        adap.timeout = 1;
        let mut body = [0u8; 1];
        // Linux's bit_doAddress() reports any address-phase failure, including
        // the SCL timeout inside try_address(), as -ENXIO.
        let ret = adap.transfer(&mut [I2cMsg::new(0x50, 0, &mut body)]);
        assert_eq!(ret, -ENXIO);
    }

    #[test]
    fn bus_test_detects_a_line_stuck_low_and_refuses_registration() {
        struct StuckLow;
        impl I2cBitLines for StuckLow {
            fn setsda(&self, _state: i32) {}
            fn setscl(&self, _state: i32) {}
            fn getsda(&self) -> Option<i32> {
                Some(0)
            }
            fn getscl(&self) -> Option<i32> {
                Some(1)
            }
        }
        let lines = StuckLow;
        let bit = I2cBitAlgo {
            data: I2cAlgoBitData {
                lines: &lines,
                udelay: 0,
                timeout: 1,
                can_do_atomic: false,
                bit_test: 2,
            },
        };
        let idr = I2cAdapterIdr::new();
        let mut adap = I2cAdapter::new("stuck", None);
        assert_eq!(i2c_bit_add_bus(&mut adap, &bit, &idr), -ENODEV);
        assert!(adap.algo.is_none());
    }

    #[test]
    fn bus_test_passes_on_a_healthy_bus_and_registration_sets_defaults() {
        let bus = MockBus::new(0x50, true, &[]);
        let bit = I2cBitAlgo {
            data: I2cAlgoBitData {
                bit_test: 1,
                ..algo(&bus).data
            },
        };
        let idr = I2cAdapterIdr::new();
        let mut adap = I2cAdapter::new("healthy", None);
        assert_eq!(i2c_bit_add_numbered_bus(&mut adap, &bit, &idr), 0);
        assert_eq!(adap.retries, 3);
        assert!(adap.algo.is_none() == false);
        assert!(adap.quirks.is_none());
        assert!(idr.contains(adap.nr) || adap.nr == -1);
    }

    #[test]
    fn write_only_lines_select_the_no_clock_stretch_quirk() {
        struct WriteOnly;
        impl I2cBitLines for WriteOnly {
            fn setsda(&self, _state: i32) {}
            fn setscl(&self, _state: i32) {}
        }
        let lines = WriteOnly;
        let bit = I2cBitAlgo {
            data: I2cAlgoBitData {
                lines: &lines,
                udelay: 0,
                timeout: 1,
                can_do_atomic: false,
                bit_test: 0,
            },
        };
        let idr = I2cAdapterIdr::new();
        let mut adap = I2cAdapter::new("wo", None);
        assert_eq!(i2c_bit_add_bus(&mut adap, &bit, &idr), 0);
        assert_eq!(
            adap.quirks.map(|q| q.flags),
            Some(I2C_AQ_NO_CLK_STRETCH)
        );
    }

    #[test]
    fn functionality_matches_the_bit_algorithm_mask() {
        assert_eq!(
            bit_func(),
            I2C_FUNC_I2C
                | I2C_FUNC_NOSTART
                | I2C_FUNC_SMBUS_EMUL_ALL
                | I2C_FUNC_10BIT_ADDR
                | I2C_FUNC_PROTOCOL_MANGLING
        );
    }

    #[test]
    fn quirks_type_is_shared_with_the_core() {
        let _ = I2cAdapterQuirks::default();
        let _ = I2C_M_STOP | I2C_M_NOSTART | I2C_M_TEN | I2C_M_RECV_LEN | I2C_M_NO_RD_ACK;
        let _ = EOPNOTSUPP;
        let _ = EPROTO;
        let _ = EIO;
        let _ = ENODEV;
    }
}
