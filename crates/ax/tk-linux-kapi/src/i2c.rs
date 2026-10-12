// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! I2C core for the DRM ports: the subset of Linux 7.2.3
//! `drivers/i2c/i2c-core-base.c` that GMBUS, DDC/EDID and DP-AUX-over-I2C use
//! (GPL-2.0 upstream), with the flag values of `include/uapi/linux/i2c.h`.
//!
//! Covered: `struct i2c_msg` and the `I2C_M_*` flags, `struct i2c_adapter`
//! with its algorithm and quirks, `i2c_add_adapter()`,
//! `i2c_add_numbered_adapter()`, `i2c_del_adapter()` over an adapter-number
//! registry, `i2c_lock_bus()`/`i2c_trylock_bus()`/`i2c_unlock_bus()`,
//! `__i2c_transfer()` (retry loop and quirk checks) and `i2c_transfer()`.
//!
//! Not covered: the device model (`struct device`, sysfs, client lists,
//! ACPI/OF enumeration, host-notify IRQ domains), the SMBus emulation layer
//! beyond the functionality flags that DRM tests, and the atomic-transfer
//! mode. The device model is owned by the platform, not by this shim.

use alloc::vec::Vec;

use crate::{
    clock,
    config::{CONFIG_HZ, EAGAIN, EBUSY, EINVAL, EOPNOTSUPP},
};

// Message flags (include/uapi/linux/i2c.h).
pub const I2C_M_RD: u16 = 0x0001;
pub const I2C_M_TEN: u16 = 0x0010;
pub const I2C_M_DMA_SAFE: u16 = 0x0200;
pub const I2C_M_RECV_LEN: u16 = 0x0400;
pub const I2C_M_NO_RD_ACK: u16 = 0x0800;
pub const I2C_M_IGNORE_NAK: u16 = 0x1000;
pub const I2C_M_REV_DIR_ADDR: u16 = 0x2000;
pub const I2C_M_NOSTART: u16 = 0x4000;
pub const I2C_M_STOP: u16 = 0x8000;

// Adapter functionality flags.
pub const I2C_FUNC_I2C: u32 = 0x0000_0001;
pub const I2C_FUNC_10BIT_ADDR: u32 = 0x0000_0002;
pub const I2C_FUNC_PROTOCOL_MANGLING: u32 = 0x0000_0004;
pub const I2C_FUNC_SMBUS_PEC: u32 = 0x0000_0008;
pub const I2C_FUNC_NOSTART: u32 = 0x0000_0010;
pub const I2C_FUNC_SLAVE: u32 = 0x0000_0020;
pub const I2C_FUNC_SMBUS_BLOCK_PROC_CALL: u32 = 0x0000_8000;
pub const I2C_FUNC_SMBUS_QUICK: u32 = 0x0001_0000;
pub const I2C_FUNC_SMBUS_READ_BYTE: u32 = 0x0002_0000;
pub const I2C_FUNC_SMBUS_WRITE_BYTE: u32 = 0x0004_0000;
pub const I2C_FUNC_SMBUS_READ_BYTE_DATA: u32 = 0x0008_0000;
pub const I2C_FUNC_SMBUS_WRITE_BYTE_DATA: u32 = 0x0010_0000;
pub const I2C_FUNC_SMBUS_READ_WORD_DATA: u32 = 0x0020_0000;
pub const I2C_FUNC_SMBUS_WRITE_WORD_DATA: u32 = 0x0040_0000;
pub const I2C_FUNC_SMBUS_PROC_CALL: u32 = 0x0080_0000;
pub const I2C_FUNC_SMBUS_READ_BLOCK_DATA: u32 = 0x0100_0000;
pub const I2C_FUNC_SMBUS_WRITE_BLOCK_DATA: u32 = 0x0200_0000;
pub const I2C_FUNC_SMBUS_READ_I2C_BLOCK: u32 = 0x0400_0000;
pub const I2C_FUNC_SMBUS_WRITE_I2C_BLOCK: u32 = 0x0800_0000;
pub const I2C_FUNC_SMBUS_HOST_NOTIFY: u32 = 0x1000_0000;
pub const I2C_FUNC_SMBUS_BYTE: u32 = I2C_FUNC_SMBUS_READ_BYTE | I2C_FUNC_SMBUS_WRITE_BYTE;
pub const I2C_FUNC_SMBUS_BYTE_DATA: u32 =
    I2C_FUNC_SMBUS_READ_BYTE_DATA | I2C_FUNC_SMBUS_WRITE_BYTE_DATA;
pub const I2C_FUNC_SMBUS_WORD_DATA: u32 =
    I2C_FUNC_SMBUS_READ_WORD_DATA | I2C_FUNC_SMBUS_WRITE_WORD_DATA;
pub const I2C_FUNC_SMBUS_BLOCK_DATA: u32 =
    I2C_FUNC_SMBUS_READ_BLOCK_DATA | I2C_FUNC_SMBUS_WRITE_BLOCK_DATA;
pub const I2C_FUNC_SMBUS_I2C_BLOCK: u32 =
    I2C_FUNC_SMBUS_READ_I2C_BLOCK | I2C_FUNC_SMBUS_WRITE_I2C_BLOCK;
pub const I2C_FUNC_SMBUS_EMUL: u32 = I2C_FUNC_SMBUS_QUICK
    | I2C_FUNC_SMBUS_BYTE
    | I2C_FUNC_SMBUS_BYTE_DATA
    | I2C_FUNC_SMBUS_WORD_DATA
    | I2C_FUNC_SMBUS_PROC_CALL
    | I2C_FUNC_SMBUS_WRITE_BLOCK_DATA
    | I2C_FUNC_SMBUS_I2C_BLOCK
    | I2C_FUNC_SMBUS_PEC;
pub const I2C_FUNC_SMBUS_EMUL_ALL: u32 = I2C_FUNC_SMBUS_EMUL
    | I2C_FUNC_SMBUS_READ_BLOCK_DATA
    | I2C_FUNC_SMBUS_BLOCK_PROC_CALL;

/// `I2C_SMBUS_BLOCK_MAX` from include/uapi/linux/i2c.h.
pub const I2C_SMBUS_BLOCK_MAX: usize = 32;

// Quirk flags (include/linux/i2c.h).
pub const I2C_AQ_COMB: u64 = 1 << 0;
pub const I2C_AQ_COMB_WRITE_FIRST: u64 = 1 << 1;
pub const I2C_AQ_COMB_READ_SECOND: u64 = 1 << 2;
pub const I2C_AQ_COMB_SAME_ADDR: u64 = 1 << 3;
pub const I2C_AQ_COMB_WRITE_THEN_READ: u64 =
    I2C_AQ_COMB | I2C_AQ_COMB_WRITE_FIRST | I2C_AQ_COMB_READ_SECOND | I2C_AQ_COMB_SAME_ADDR;
pub const I2C_AQ_NO_CLK_STRETCH: u64 = 1 << 4;
pub const I2C_AQ_NO_ZERO_LEN_READ: u64 = 1 << 5;
pub const I2C_AQ_NO_ZERO_LEN_WRITE: u64 = 1 << 6;
pub const I2C_AQ_NO_ZERO_LEN: u64 = I2C_AQ_NO_ZERO_LEN_READ | I2C_AQ_NO_ZERO_LEN_WRITE;
pub const I2C_AQ_NO_REP_START: u64 = 1 << 7;

// Bus lock depth flags (include/linux/i2c.h).
pub const I2C_LOCK_ROOT_ADAPTER: u32 = 1 << 0;
pub const I2C_LOCK_SEGMENT: u32 = 1 << 1;

/// `struct i2c_msg`. `buf` is the caller's buffer; `len` is the number of
/// bytes the transfer uses and must not exceed `buf.len()`. `I2C_M_RECV_LEN`
/// grows `len` by the block length the slave reports.
pub struct I2cMsg<'a> {
    pub addr: u16,
    pub flags: u16,
    pub len: usize,
    pub buf: &'a mut [u8],
}

impl<'a> I2cMsg<'a> {
    /// A message whose length is the whole buffer.
    pub fn new(addr: u16, flags: u16, buf: &'a mut [u8]) -> Self {
        let len = buf.len();
        Self {
            addr,
            flags,
            len,
            buf,
        }
    }
}

/// `struct i2c_adapter_quirks`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct I2cAdapterQuirks {
    pub flags: u64,
    pub max_num_msgs: i32,
    pub max_write_len: u16,
    pub max_read_len: u16,
    pub max_comb_1st_msg_len: u16,
    pub max_comb_2nd_msg_len: u16,
}

/// `struct i2c_algorithm`: the master transfer and functionality callbacks.
pub trait I2cAlgorithm: Sync {
    /// `master_xfer()`: returns the number of messages completed or a
    /// negative errno.
    fn master_xfer(&self, adap: &I2cAdapter<'_>, msgs: &mut [I2cMsg<'_>]) -> i32;

    /// `functionality()`: the `I2C_FUNC_*` mask this algorithm supports.
    fn functionality(&self) -> u32;
}

/// `struct i2c_adapter`, without the device model.
///
/// The bus lock is a spin lock held across a whole transfer, and the
/// `lock_bus()` family can hold it across calls.
pub struct I2cAdapter<'a> {
    pub name: &'a str,
    /// Adapter number; `-1` requests a dynamically assigned number.
    pub nr: i32,
    pub algo: Option<&'a dyn I2cAlgorithm>,
    pub quirks: Option<&'a I2cAdapterQuirks>,
    /// Extra attempts for `-EAGAIN` results within `timeout`.
    pub retries: u32,
    /// Retry window in jiffies; zero selects the core default of one second.
    pub timeout: i64,
    bus_lock: spin::Mutex<()>,
}

impl<'a> I2cAdapter<'a> {
    pub fn new(name: &'a str, algo: Option<&'a dyn I2cAlgorithm>) -> Self {
        Self {
            name,
            nr: -1,
            algo,
            quirks: None,
            retries: 0,
            timeout: 0,
            bus_lock: spin::Mutex::new(()),
        }
    }

    /// `i2c_lock_bus(adap, I2C_LOCK_ROOT_ADAPTER)`: blocks until the bus is
    /// free. Pair with [`I2cAdapter::unlock_bus`].
    pub fn lock_bus(&self) {
        core::mem::forget(self.bus_lock.lock());
    }

    /// `i2c_trylock_bus()`: returns true when the bus was taken.
    pub fn trylock_bus(&self) -> bool {
        match self.bus_lock.try_lock() {
            Some(guard) => {
                core::mem::forget(guard);
                true
            }
            None => false,
        }
    }

    /// `i2c_unlock_bus()`: releases a lock taken by `lock_bus` or
    /// `trylock_bus`.
    #[allow(unsafe_code)]
    pub fn unlock_bus(&self) {
        // SAFETY: every caller pairs this with a successful `lock_bus()` or
        // `trylock_bus()`, which leaves the lock held by the caller and no
        // guard alive. That makes the force-unlock the matching release.
        unsafe { self.bus_lock.force_unlock() };
    }

    /// `i2c_transfer()`: take the bus, run `__i2c_transfer()`, release.
    pub fn transfer(&self, msgs: &mut [I2cMsg<'_>]) -> i32 {
        let guard = self.bus_lock.lock();
        let ret = self.transfer_locked(msgs);
        drop(guard);
        ret
    }

    /// `__i2c_transfer()`: the caller holds the bus.
    pub fn transfer_locked(&self, msgs: &mut [I2cMsg<'_>]) -> i32 {
        let Some(algo) = self.algo else {
            axlog::debug!("I2C level transfers not supported");
            return -EOPNOTSUPP;
        };
        if msgs.is_empty() {
            return -EINVAL;
        }
        if let Some(quirks) = self.quirks {
            if check_for_quirks(self, quirks, msgs) {
                return -EOPNOTSUPP;
            }
        }
        let orig = clock::now_ns();
        let window_ns = self.timeout_ns();
        let mut ret = 0;
        for _ in 0..=self.retries {
            ret = algo.master_xfer(self, msgs);
            if ret != -EAGAIN {
                break;
            }
            if clock::now_ns() > orig.saturating_add(window_ns) {
                break;
            }
        }
        ret
    }

    fn timeout_ns(&self) -> u64 {
        let tick_ns = 1_000_000_000 / u64::from(CONFIG_HZ);
        (self.timeout.max(0) as u64).saturating_mul(tick_ns)
    }
}

/// `i2c_quirk_exceeded()`: a zero quirk means "no limit".
fn quirk_exceeded(value: usize, quirk: usize) -> bool {
    quirk != 0 && value > quirk
}

/// `i2c_quirk_error()`: log and report the rejected message.
fn quirk_error(adap: &I2cAdapter<'_>, msg: &I2cMsg<'_>, what: &str) -> bool {
    axlog::warn!(
        "{}: adapter quirk: {} (addr 0x{:04x}, size {}, {})",
        adap.name,
        what,
        msg.addr,
        msg.len,
        if msg.flags & I2C_M_RD != 0 { "read" } else { "write" }
    );
    true
}

/// `i2c_check_for_quirks()`: true when the adapter must reject the batch.
fn check_for_quirks(adap: &I2cAdapter<'_>, q: &I2cAdapterQuirks, msgs: &[I2cMsg<'_>]) -> bool {
    let mut max_num = q.max_num_msgs.max(0) as usize;
    let mut do_len_check = true;
    if q.flags & I2C_AQ_COMB != 0 {
        max_num = 2;
        if msgs.len() == 2 {
            if q.flags & I2C_AQ_COMB_WRITE_FIRST != 0 && msgs[0].flags & I2C_M_RD != 0 {
                return quirk_error(adap, &msgs[0], "1st comb msg must be write");
            }
            if q.flags & I2C_AQ_COMB_READ_SECOND != 0 && msgs[1].flags & I2C_M_RD == 0 {
                return quirk_error(adap, &msgs[1], "2nd comb msg must be read");
            }
            if q.flags & I2C_AQ_COMB_SAME_ADDR != 0 && msgs[0].addr != msgs[1].addr {
                return quirk_error(adap, &msgs[0], "comb msg only to same addr");
            }
            if quirk_exceeded(msgs[0].len, usize::from(q.max_comb_1st_msg_len)) {
                return quirk_error(adap, &msgs[0], "msg too long");
            }
            if quirk_exceeded(msgs[1].len, usize::from(q.max_comb_2nd_msg_len)) {
                return quirk_error(adap, &msgs[1], "msg too long");
            }
            do_len_check = false;
        }
    }
    if quirk_exceeded(msgs.len(), max_num) {
        return quirk_error(adap, &msgs[0], "too many messages");
    }
    for msg in msgs {
        let len = msg.len;
        if msg.flags & I2C_M_RD != 0 {
            if do_len_check && quirk_exceeded(len, usize::from(q.max_read_len)) {
                return quirk_error(adap, msg, "msg too long");
            }
            if q.flags & I2C_AQ_NO_ZERO_LEN_READ != 0 && len == 0 {
                return quirk_error(adap, msg, "no zero length");
            }
        } else {
            if do_len_check && quirk_exceeded(len, usize::from(q.max_write_len)) {
                return quirk_error(adap, msg, "msg too long");
            }
            if q.flags & I2C_AQ_NO_ZERO_LEN_WRITE != 0 && len == 0 {
                return quirk_error(adap, msg, "no zero length");
            }
        }
    }
    false
}

/// `i2c_8bit_addr_from_msg()`: the address byte on the wire.
pub fn i2c_8bit_addr_from_msg(msg: &I2cMsg<'_>) -> u8 {
    ((msg.addr << 1) as u8) | (msg.flags & I2C_M_RD) as u8
}

/// The adapter-number registry (`i2c_adapter_idr` in Linux).
pub struct I2cAdapterIdr {
    numbers: spin::Mutex<Vec<i32>>,
}

impl I2cAdapterIdr {
    pub const fn new() -> Self {
        Self {
            numbers: spin::Mutex::new(Vec::new()),
        }
    }

    /// `idr_alloc()` from zero: the lowest free number.
    pub fn alloc_dynamic(&self) -> i32 {
        let mut numbers = self.numbers.lock();
        let mut candidate = 0;
        while numbers.contains(&candidate) {
            candidate += 1;
        }
        numbers.push(candidate);
        candidate
    }

    /// Claim a fixed number; `-EBUSY` when it is already registered.
    pub fn alloc_fixed(&self, nr: i32) -> Result<(), i32> {
        let mut numbers = self.numbers.lock();
        if numbers.contains(&nr) {
            return Err(-EBUSY);
        }
        numbers.push(nr);
        Ok(())
    }

    /// Release a number on `i2c_del_adapter()`.
    pub fn remove(&self, nr: i32) {
        self.numbers.lock().retain(|&registered| registered != nr);
    }

    pub fn contains(&self, nr: i32) -> bool {
        self.numbers.lock().contains(&nr)
    }
}

impl Default for I2cAdapterIdr {
    fn default() -> Self {
        Self::new()
    }
}

/// The system adapter registry.
pub static I2C_ADAPTER_IDR: I2cAdapterIdr = I2cAdapterIdr::new();

/// `i2c_register_adapter()`: sanity checks, defaults and number claim.
fn register_adapter(adap: &mut I2cAdapter<'_>, idr: &I2cAdapterIdr) -> i32 {
    if adap.name.is_empty() {
        axlog::warn!("i2c adapter has no name");
        return -EINVAL;
    }
    if adap.algo.is_none() {
        axlog::error!("adapter '{}': no algo supplied!", adap.name);
        return -EINVAL;
    }
    // Linux defaults the retry window to one second when none was set.
    if adap.timeout == 0 {
        adap.timeout = i64::from(CONFIG_HZ);
    }
    if adap.nr == -1 {
        adap.nr = idr.alloc_dynamic();
        return 0;
    }
    match idr.alloc_fixed(adap.nr) {
        Ok(()) => 0,
        Err(err) => {
            axlog::error!("adapter '{}': bus number {} busy", adap.name, adap.nr);
            err
        }
    }
}

/// `i2c_add_adapter()`: register with the next free dynamic number.
pub fn i2c_add_adapter(adap: &mut I2cAdapter<'_>, idr: &I2cAdapterIdr) -> i32 {
    let saved = adap.nr;
    adap.nr = -1;
    let ret = register_adapter(adap, idr);
    if ret != 0 {
        adap.nr = saved;
    }
    ret
}

/// `i2c_add_numbered_adapter()`: `nr == -1` assigns dynamically, otherwise
/// the adapter claims its fixed number.
pub fn i2c_add_numbered_adapter(adap: &mut I2cAdapter<'_>, idr: &I2cAdapterIdr) -> i32 {
    if adap.nr == -1 {
        return i2c_add_adapter(adap, idr);
    }
    register_adapter(adap, idr)
}

/// `i2c_del_adapter()`: release the adapter's number.
pub fn i2c_del_adapter(adap: &mut I2cAdapter<'_>, idr: &I2cAdapterIdr) {
    idr.remove(adap.nr);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU32, Ordering};

    /// Algorithm stub: replies with `answers[call]` (the last entry repeats)
    /// and counts its invocations.
    struct Recorder {
        calls: AtomicU32,
        answers: [i32; 4],
    }

    impl I2cAlgorithm for Recorder {
        fn master_xfer(&self, _adap: &I2cAdapter<'_>, msgs: &mut [I2cMsg<'_>]) -> i32 {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            let answer = self.answers[(call as usize).min(3)];
            if answer < 0 { answer } else { msgs.len() as i32 }
        }

        fn functionality(&self) -> u32 {
            I2C_FUNC_I2C
        }
    }

    fn recorder(answers: [i32; 4]) -> Recorder {
        Recorder {
            calls: AtomicU32::new(0),
            answers,
        }
    }

    #[test]
    fn dynamic_and_fixed_numbers_follow_the_idr_rules() {
        let idr = I2cAdapterIdr::new();
        let algo = recorder([0; 4]);
        let mut first = I2cAdapter::new("a", Some(&algo));
        assert_eq!(i2c_add_adapter(&mut first, &idr), 0);
        assert_eq!(first.nr, 0);
        let mut second = I2cAdapter::new("b", Some(&algo));
        assert_eq!(i2c_add_numbered_adapter(&mut second, &idr), 0);
        assert_eq!(second.nr, 1);
        let mut fixed = I2cAdapter::new("c", Some(&algo));
        fixed.nr = 5;
        assert_eq!(i2c_add_numbered_adapter(&mut fixed, &idr), 0);
        let mut clash = I2cAdapter::new("d", Some(&algo));
        clash.nr = 5;
        assert_eq!(i2c_add_numbered_adapter(&mut clash, &idr), -EBUSY);
        i2c_del_adapter(&mut fixed, &idr);
        assert!(!idr.contains(5));
        let mut reused = I2cAdapter::new("e", Some(&algo));
        reused.nr = 5;
        assert_eq!(i2c_add_numbered_adapter(&mut reused, &idr), 0);
    }

    #[test]
    fn registration_rejects_nameless_or_algorithmless_adapters_and_defaults_timeout() {
        let idr = I2cAdapterIdr::new();
        let algo = recorder([0; 4]);
        let mut nameless = I2cAdapter::new("", Some(&algo));
        assert_eq!(i2c_add_adapter(&mut nameless, &idr), -EINVAL);
        let mut bare = I2cAdapter::new("bare", None);
        assert_eq!(i2c_add_adapter(&mut bare, &idr), -EINVAL);
        let mut ok = I2cAdapter::new("ok", Some(&algo));
        assert_eq!(i2c_add_adapter(&mut ok, &idr), 0);
        assert_eq!(ok.timeout, i64::from(CONFIG_HZ));
    }

    #[test]
    fn transfer_reports_unsupported_and_empty_batches() {
        let mut bare = I2cAdapter::new("bare", None);
        let mut buf = [0u8; 1];
        assert_eq!(bare.transfer(&mut [I2cMsg::new(0x50, 0, &mut buf)]), -EOPNOTSUPP);
        let algo = recorder([0; 4]);
        let adap = I2cAdapter::new("ok", Some(&algo));
        assert_eq!(adap.transfer(&mut []), -EINVAL);
    }

    #[test]
    fn retries_only_on_eagain_and_stop_at_the_retry_count() {
        let algo = recorder([-EAGAIN, -EAGAIN, 1, 1]);
        let mut adap = I2cAdapter::new("retry", Some(&algo));
        adap.retries = 3;
        adap.timeout = 1_000;
        let mut buf = [0u8; 1];
        assert_eq!(adap.transfer(&mut [I2cMsg::new(0x50, 0, &mut buf)]), 1);
        assert_eq!(algo.calls.load(Ordering::Relaxed), 3);

        let algo = recorder([-EAGAIN; 4]);
        let mut adap = I2cAdapter::new("exhaust", Some(&algo));
        adap.retries = 2;
        adap.timeout = 1_000;
        assert_eq!(adap.transfer(&mut [I2cMsg::new(0x50, 0, &mut buf)]), -EAGAIN);
        assert_eq!(algo.calls.load(Ordering::Relaxed), 3);

        let algo = recorder([-5, 1, 1, 1]);
        let mut adap = I2cAdapter::new("hard", Some(&algo));
        adap.retries = 3;
        adap.timeout = 1_000;
        assert_eq!(adap.transfer(&mut [I2cMsg::new(0x50, 0, &mut buf)]), -5);
        assert_eq!(algo.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn quirks_reject_oversized_and_malformed_batches_before_the_algorithm() {
        let algo = recorder([1, 1, 1, 1]);
        let quirks = I2cAdapterQuirks {
            flags: I2C_AQ_NO_ZERO_LEN_READ,
            max_num_msgs: 2,
            max_read_len: 4,
            ..Default::default()
        };
        let mut adap = I2cAdapter::new("quirky", Some(&algo));
        adap.quirks = Some(&quirks);
        let mut small = [0u8; 4];
        let mut big = [0u8; 8];
        assert_eq!(
            adap.transfer(&mut [I2cMsg::new(0x50, I2C_M_RD, &mut big)]),
            -EOPNOTSUPP
        );
        assert_eq!(
            adap.transfer(&mut [I2cMsg::new(0x50, I2C_M_RD, &mut small[..0])]),
            -EOPNOTSUPP
        );
        let mut a = [0u8; 1];
        let mut b = [0u8; 1];
        let mut c = [0u8; 1];
        assert_eq!(
            adap.transfer(&mut [
                I2cMsg::new(0x50, 0, &mut a),
                I2cMsg::new(0x50, 0, &mut b),
                I2cMsg::new(0x50, 0, &mut c),
            ]),
            -EOPNOTSUPP
        );
        assert_eq!(algo.calls.load(Ordering::Relaxed), 0);
        assert_eq!(adap.transfer(&mut [I2cMsg::new(0x50, 0, &mut a)]), 1);
    }

    #[test]
    fn bus_lock_is_exclusive_until_unlocked() {
        let algo = recorder([0; 4]);
        let adap = I2cAdapter::new("locked", Some(&algo));
        adap.lock_bus();
        assert!(!adap.trylock_bus());
        adap.unlock_bus();
        assert!(adap.trylock_bus());
        adap.unlock_bus();
        assert!(adap.trylock_bus());
        adap.unlock_bus();
    }

    #[test]
    fn address_byte_carries_the_read_bit() {
        let mut buf = [0u8; 1];
        let write = I2cMsg::new(0x50, 0, &mut buf);
        assert_eq!(i2c_8bit_addr_from_msg(&write), 0xa0);
        let mut buf = [0u8; 1];
        let read = I2cMsg::new(0x50, I2C_M_RD, &mut buf);
        assert_eq!(i2c_8bit_addr_from_msg(&read), 0xa1);
    }
}
