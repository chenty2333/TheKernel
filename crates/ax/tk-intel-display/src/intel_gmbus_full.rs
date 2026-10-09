// Copyright (c) 2006 Dave Airlie <airlied@linux.ie>
// Copyright © 2006-2008,2010 Intel Corporation
//   Jesse Barnes <jesse.barnes@intel.com>
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice (including the next
// paragraph) shall be included in all copies or substantial portions of the
// Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
// THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.
//
// Authors:
// Eric Anholt <eric@anholt.net>
// Chris Wilson <chris@chris-wilson.co.uk>

//! Source-ordered translation of Linux 7.2.3 `intel_gmbus.c`.
//!
//! The transaction/retry state machine and GPIO bit manipulation live here.
//! MMIO, timing, power, IRQ wait-queue, locking, and bit-bang framework services
//! are explicit in [`GmbusIo`]; this module provides no default hardware path.

use core::cmp::min;

const I2C_RISEFALL_TIME: u32 = 10;
const INTEL_GMBUS_BURST_READ_MAX_LEN: usize = 767;
const GMBUS_FORCE_BIT_RETRY: u32 = 1 << 31;

const GPIO_CLOCK_DIR_MASK: u32 = 1 << 0;
const GPIO_CLOCK_DIR_IN: u32 = 0 << 1;
const GPIO_CLOCK_DIR_OUT: u32 = 1 << 1;
const GPIO_CLOCK_VAL_MASK: u32 = 1 << 2;
const GPIO_CLOCK_VAL_IN: u32 = 1 << 4;
const GPIO_CLOCK_PULLUP_DISABLE: u32 = 1 << 5;
const GPIO_DATA_DIR_MASK: u32 = 1 << 8;
const GPIO_DATA_DIR_IN: u32 = 0 << 9;
const GPIO_DATA_DIR_OUT: u32 = 1 << 9;
const GPIO_DATA_VAL_MASK: u32 = 1 << 10;
const GPIO_DATA_VAL_IN: u32 = 1 << 12;
const GPIO_DATA_PULLUP_DISABLE: u32 = 1 << 13;

const GMBUS_AKSV_SELECT: u32 = 1 << 11;
const GMBUS_BYTE_CNT_OVERRIDE: u32 = 1 << 6;
const GMBUS_SW_CLR_INT: u32 = 1 << 31;
const GMBUS_SW_RDY: u32 = 1 << 30;
const GMBUS_CYCLE_WAIT: u32 = 1 << 25;
const GMBUS_CYCLE_INDEX: u32 = 2 << 25;
const GMBUS_CYCLE_STOP: u32 = 4 << 25;
const GMBUS_BYTE_COUNT_SHIFT: u32 = 16;
const GMBUS_SLAVE_INDEX_SHIFT: u32 = 8;
const GMBUS_SLAVE_ADDR_SHIFT: u32 = 1;
const GMBUS_SLAVE_READ: u32 = 1;
const GMBUS_SLAVE_WRITE: u32 = 0;
const GMBUS_HW_WAIT_PHASE: u32 = 1 << 14;
const GMBUS_HW_RDY: u32 = 1 << 11;
const GMBUS_SATOER: u32 = 1 << 10;
const GMBUS_ACTIVE: u32 = 1 << 9;
const GMBUS_IDLE_EN: u32 = 1 << 2;
const GMBUS_HW_WAIT_EN: u32 = 1 << 1;
const GMBUS_HW_RDY_EN: u32 = 1;
const GMBUS_2BYTE_INDEX_EN: u32 = 1 << 31;
/// Default `reg0` rate selected by `intel_gmbus_setup()` upstream.
pub const GMBUS_RATE_100KHZ: u32 = 0 << 8;
/// Bit-bang adapter timeout configured by `intel_gpio_setup()` upstream.
pub const I2C_BITBANG_TIMEOUT_US: u32 = 2200;
const GMBUS_BYTE_COUNT_MAX: usize = 256;
const GEN9_GMBUS_BYTE_COUNT_MAX: usize = 511;

const GMBUS0_OFFSET: u32 = 0x5100;
const GMBUS1_OFFSET: u32 = 0x5104;
const GMBUS2_OFFSET: u32 = 0x5108;
const GMBUS3_OFFSET: u32 = 0x510c;
const GMBUS4_OFFSET: u32 = 0x5110;
const GMBUS5_OFFSET: u32 = 0x5120;
const GPIO_OFFSET: u32 = 0x5010;
const PNV_GMBUSUNIT_CLOCK_GATE_DISABLE: u32 = 1 << 24;
const PCH_GMBUSUNIT_CLOCK_GATE_DISABLE: u32 = 1 << 31;
const BXT_GMBUS_GATING_DIS: u32 = 1 << 14;

const I2C_M_RD: u16 = 1;
const DRM_HDCP_DDC_ADDR: u16 = 0x3a;
const DRM_HDCP_DDC_AKSV: u8 = 0x10;
const DRM_HDCP_KSV_LEN: usize = 5;

const EAGAIN: i32 = -11;
const ENXIO: i32 = -6;
const ETIMEDOUT: i32 = -110;

/// Ordered PCH families used by the source's `>= PCH_*` pin-table selection.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum PchType {
    #[default]
    None,
    Lpt,
    Spt,
    Cnp,
    Icp,
    Dg1,
    Dg2,
    Mtl,
}

/// Platform predicates consumed by the upstream GMBUS paths.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GmbusPlatform {
    pub i830: bool,
    pub i845g: bool,
    pub pineview: bool,
    pub broadwell: bool,
    pub broxton: bool,
    pub geminilake: bool,
    pub kabylake: bool,
}

/// Immutable display facts required by the translated source.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GmbusDisplay {
    /// Base used by the GMBUS and GPIO register macros.
    pub mmio_base: u32,
    pub display_version: u8,
    pub pch_type: PchType,
    pub platform: GmbusPlatform,
    pub has_pch_cnp: bool,
    pub has_pch_spt: bool,
    pub has_gmch: bool,
    pub wa_16025573575: bool,
}

/// GPIO blocks used by GMBUS pin mappings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum GmbusGpio {
    A = 0,
    B = 1,
    C = 2,
    D = 3,
    E = 4,
    F = 5,
    G = 6,
    H = 7,
    J = 9,
    K = 10,
    L = 11,
    M = 12,
    N = 13,
    O = 14,
}

/// A source pin name and its GPIO register block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GmbusPin {
    pub name: &'static str,
    pub gpio: GmbusGpio,
}

const fn pin(name: &'static str, gpio: GmbusGpio) -> Option<GmbusPin> {
    Some(GmbusPin { name, gpio })
}

const GPIO_NONE: Option<GmbusPin> = None;
const GMBUS_PINS: [Option<GmbusPin>; 15] = [
    GPIO_NONE,
    pin("ssc", GmbusGpio::B),
    pin("vga", GmbusGpio::A),
    pin("panel", GmbusGpio::C),
    pin("dpc", GmbusGpio::D),
    pin("dpb", GmbusGpio::E),
    pin("dpd", GmbusGpio::F),
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
];
const GMBUS_PINS_BDW: [Option<GmbusPin>; 15] = [
    GPIO_NONE,
    GPIO_NONE,
    pin("vga", GmbusGpio::A),
    GPIO_NONE,
    pin("dpc", GmbusGpio::D),
    pin("dpb", GmbusGpio::E),
    pin("dpd", GmbusGpio::F),
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
];
const GMBUS_PINS_SKL: [Option<GmbusPin>; 15] = [
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    pin("dpc", GmbusGpio::D),
    pin("dpb", GmbusGpio::E),
    pin("dpd", GmbusGpio::F),
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
];
const GMBUS_PINS_BXT: [Option<GmbusPin>; 15] = [
    GPIO_NONE,
    pin("dpb", GmbusGpio::B),
    pin("dpc", GmbusGpio::C),
    pin("misc", GmbusGpio::D),
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
];
const GMBUS_PINS_CNP: [Option<GmbusPin>; 15] = [
    GPIO_NONE,
    pin("dpb", GmbusGpio::B),
    pin("dpc", GmbusGpio::C),
    pin("misc", GmbusGpio::D),
    pin("dpd", GmbusGpio::E),
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
];
const GMBUS_PINS_ICP: [Option<GmbusPin>; 15] = [
    GPIO_NONE,
    pin("dpa", GmbusGpio::B),
    pin("dpb", GmbusGpio::C),
    pin("dpc", GmbusGpio::D),
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    pin("tc1", GmbusGpio::J),
    pin("tc2", GmbusGpio::K),
    pin("tc3", GmbusGpio::L),
    pin("tc4", GmbusGpio::M),
    pin("tc5", GmbusGpio::N),
    pin("tc6", GmbusGpio::O),
];
const GMBUS_PINS_DG1: [Option<GmbusPin>; 15] = [
    GPIO_NONE,
    pin("dpa", GmbusGpio::B),
    pin("dpb", GmbusGpio::C),
    pin("dpc", GmbusGpio::D),
    pin("dpd", GmbusGpio::E),
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
];
const GMBUS_PINS_DG2: [Option<GmbusPin>; 15] = [
    GPIO_NONE,
    pin("dpa", GmbusGpio::B),
    pin("dpb", GmbusGpio::C),
    pin("dpc", GmbusGpio::D),
    pin("dpd", GmbusGpio::E),
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    pin("tc1", GmbusGpio::J),
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
];
const GMBUS_PINS_MTP: [Option<GmbusPin>; 15] = [
    GPIO_NONE,
    pin("dpa", GmbusGpio::B),
    pin("dpb", GmbusGpio::C),
    pin("dpc", GmbusGpio::D),
    pin("dpd", GmbusGpio::E),
    pin("dpe", GmbusGpio::F),
    GPIO_NONE,
    GPIO_NONE,
    GPIO_NONE,
    pin("tc1", GmbusGpio::J),
    pin("tc2", GmbusGpio::K),
    pin("tc3", GmbusGpio::L),
    pin("tc4", GmbusGpio::M),
    GPIO_NONE,
    GPIO_NONE,
];

/// Linux GMBUS message shape. `len` is kept separately as in `struct i2c_msg`.
#[derive(Debug)]
pub struct I2cMessage<'a> {
    pub address: u16,
    pub flags: u16,
    pub len: usize,
    pub buffer: &'a mut [u8],
}

impl I2cMessage<'_> {
    pub const fn is_read(&self) -> bool {
        self.flags & I2C_M_RD != 0
    }
}

/// State corresponding to one upstream `struct intel_gmbus`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GmbusBus {
    pub display: GmbusDisplay,
    pub adapter_name: &'static str,
    pub force_bit: u32,
    pub reg0: u32,
    /// Absolute MMIO address of this pin's GPIO register.
    pub gpio_reg: u32,
}

/// Firmware versus untraced MMIO access, matching the corresponding DE calls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    Firmware,
    Untraced,
}

/// The values returned by an upstream-style polling helper.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PollResult {
    pub status: u32,
    pub matched: bool,
}

/// Failures from the backend or the I2C protocol. Protocol variants preserve
/// the Linux errno outcomes used by GMBUS callers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GmbusError {
    Io,
    Power,
    Invalid,
    NoDevice,
    Timeout,
    Again,
}

impl GmbusError {
    pub const fn errno(self) -> i32 {
        match self {
            Self::NoDevice => ENXIO,
            Self::Timeout => ETIMEDOUT,
            Self::Again => EAGAIN,
            Self::Io => -5,
            Self::Power => -5,
            Self::Invalid => -22,
        }
    }
}

/// Diagnostics that were `drm_dbg_kms()` messages in the upstream source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GmbusDiagnostic {
    IdleTimeout {
        adapter: &'static str,
    },
    NakIdleTimeout {
        adapter: &'static str,
    },
    Nak {
        adapter: &'static str,
        address: u16,
        read: bool,
        len: usize,
    },
    FirstMessageNakRetry {
        adapter: &'static str,
    },
    TimeoutBitBang {
        adapter: &'static str,
        pin: u32,
    },
    ForceBit {
        adapter: &'static str,
        enabled: bool,
        count: u32,
    },
}

/// Integration boundary for MMIO, delay, waits/IRQs, power, locking, bitbang,
/// and diagnostics. Polling functions retain the source timeout/interval
/// arguments so a kernel adapter can provide its ordinary wait implementation.
pub trait GmbusIo {
    fn read32(&mut self, register: u32, access: Access) -> Result<u32, GmbusError>;
    fn write32(&mut self, register: u32, value: u32, access: Access) -> Result<(), GmbusError>;
    fn posting_read(&mut self, register: u32) -> Result<u32, GmbusError>;
    fn rmw32(&mut self, register: u32, clear: u32, set: u32) -> Result<(), GmbusError>;
    fn delay_us(&mut self, microseconds: u32);
    fn poll_timeout_us_atomic(
        &mut self,
        register: u32,
        mask: u32,
        timeout_us: u32,
    ) -> Result<PollResult, GmbusError>;
    fn poll_timeout_us(
        &mut self,
        register: u32,
        mask: u32,
        interval_us: u32,
        timeout_us: u32,
    ) -> Result<PollResult, GmbusError>;
    fn wait_register_mask_ms(
        &mut self,
        register: u32,
        mask: u32,
        expected: u32,
        timeout_ms: u32,
    ) -> Result<(), GmbusError>;
    fn add_waiter(&mut self);
    fn remove_waiter(&mut self);
    fn wake_waiters(&mut self);
    fn parent_irq_enabled(&self) -> bool;
    fn display_power_get(&mut self) -> Result<(), GmbusError>;
    fn display_power_put(&mut self);
    fn gmbus_mutex_lock(&mut self);
    fn gmbus_mutex_unlock(&mut self);
    fn bitbang_master_xfer(&mut self, messages: &mut [I2cMessage<'_>])
    -> Result<usize, GmbusError>;
    fn diagnostic(&mut self, diagnostic: GmbusDiagnostic);
}

const fn gmbus_register(display: GmbusDisplay, offset: u32) -> u32 {
    display.mmio_base + offset
}

/// Map a GPIO enum to the GMBUS register address used by the source macro.
pub const fn gpio_register(display: GmbusDisplay, gpio: GmbusGpio) -> u32 {
    display.mmio_base + GPIO_OFFSET + 4 * (gpio as u32)
}

/// Select the platform-specific GMBUS pin tables.
// upstream: intel_gmbus.c get_gmbus_pin()
pub fn get_gmbus_pin(display: GmbusDisplay, pin_index: usize) -> Option<GmbusPin> {
    let pins = if display.pch_type >= PchType::Mtl {
        &GMBUS_PINS_MTP
    } else if display.pch_type >= PchType::Dg2 {
        &GMBUS_PINS_DG2
    } else if display.pch_type >= PchType::Dg1 {
        &GMBUS_PINS_DG1
    } else if display.pch_type >= PchType::Icp {
        &GMBUS_PINS_ICP
    } else if display.has_pch_cnp {
        &GMBUS_PINS_CNP
    } else if display.platform.geminilake || display.platform.broxton {
        &GMBUS_PINS_BXT
    } else if display.display_version == 9 {
        &GMBUS_PINS_SKL
    } else if display.platform.broadwell {
        &GMBUS_PINS_BDW
    } else {
        &GMBUS_PINS
    };

    pins.get(pin_index).copied().flatten()
}

/// Report whether `pin` is present in the display's selected pin table.
// upstream: intel_gmbus.c intel_gmbus_is_valid_pin()
pub fn intel_gmbus_is_valid_pin(display: GmbusDisplay, pin_index: usize) -> bool {
    get_gmbus_pin(display, pin_index).is_some()
}

/// Reset the GMBUS controller registers.
// upstream: intel_gmbus.c intel_gmbus_reset()
pub fn intel_gmbus_reset(io: &mut impl GmbusIo, display: GmbusDisplay) -> Result<(), GmbusError> {
    io.write32(gmbus_register(display, GMBUS0_OFFSET), 0, Access::Firmware)?;
    io.write32(gmbus_register(display, GMBUS4_OFFSET), 0, Access::Firmware)
}

/// Enable or disable Pineview GMBUS clock gating.
// upstream: intel_gmbus.c pnv_gmbus_clock_gating()
pub fn pnv_gmbus_clock_gating(io: &mut impl GmbusIo, enable: bool) -> Result<(), GmbusError> {
    // When using bit bashing for I2C, this bit needs to be set to 1.
    io.rmw32(
        0x6200,
        PNV_GMBUSUNIT_CLOCK_GATE_DISABLE,
        if !enable {
            PNV_GMBUSUNIT_CLOCK_GATE_DISABLE
        } else {
            0
        },
    )
}

/// Enable or disable PCH GMBUS clock gating.
// upstream: intel_gmbus.c pch_gmbus_clock_gating()
pub fn pch_gmbus_clock_gating(io: &mut impl GmbusIo, enable: bool) -> Result<(), GmbusError> {
    io.rmw32(
        0xc2020,
        PCH_GMBUSUNIT_CLOCK_GATE_DISABLE,
        if !enable {
            PCH_GMBUSUNIT_CLOCK_GATE_DISABLE
        } else {
            0
        },
    )
}

/// Enable or disable Broxton GMBUS clock gating.
// upstream: intel_gmbus.c bxt_gmbus_clock_gating()
pub fn bxt_gmbus_clock_gating(io: &mut impl GmbusIo, enable: bool) -> Result<(), GmbusError> {
    io.rmw32(
        0x4653c,
        BXT_GMBUS_GATING_DIS,
        if !enable { BXT_GMBUS_GATING_DIS } else { 0 },
    )
}

/// Return GPIO bits that must be preserved in software.
// upstream: intel_gmbus.c get_reserved()
pub fn get_reserved(io: &mut impl GmbusIo, bus: &GmbusBus) -> Result<u32, GmbusError> {
    if bus.display.platform.i830 || bus.display.platform.i845g {
        return Ok(0);
    }

    // On most chips, these bits must be preserved in software.
    let mut preserve_bits = GPIO_DATA_PULLUP_DISABLE | GPIO_CLOCK_PULLUP_DISABLE;
    // Wa_16025573575: the masks bits need to be preserved through out.
    if bus.display.wa_16025573575 {
        preserve_bits |=
            GPIO_CLOCK_DIR_MASK | GPIO_CLOCK_VAL_MASK | GPIO_DATA_DIR_MASK | GPIO_DATA_VAL_MASK;
    }

    Ok(io.read32(bus.gpio_reg, Access::Untraced)? & preserve_bits)
}

/// Read the current GPIO clock level by releasing and sampling the line.
// upstream: intel_gmbus.c get_clock()
pub fn get_clock(io: &mut impl GmbusIo, bus: &GmbusBus) -> Result<bool, GmbusError> {
    let reserved = get_reserved(io, bus)?;
    io.write32(
        bus.gpio_reg,
        reserved | GPIO_CLOCK_DIR_MASK,
        Access::Untraced,
    )?;
    io.write32(bus.gpio_reg, reserved, Access::Untraced)?;
    Ok(io.read32(bus.gpio_reg, Access::Untraced)? & GPIO_CLOCK_VAL_IN != 0)
}

/// Read the current GPIO data level by releasing and sampling the line.
// upstream: intel_gmbus.c get_data()
pub fn get_data(io: &mut impl GmbusIo, bus: &GmbusBus) -> Result<bool, GmbusError> {
    let reserved = get_reserved(io, bus)?;
    io.write32(
        bus.gpio_reg,
        reserved | GPIO_DATA_DIR_MASK,
        Access::Untraced,
    )?;
    io.write32(bus.gpio_reg, reserved, Access::Untraced)?;
    Ok(io.read32(bus.gpio_reg, Access::Untraced)? & GPIO_DATA_VAL_IN != 0)
}

/// Drive or release the GPIO clock line and flush the posted write.
// upstream: intel_gmbus.c set_clock()
pub fn set_clock(
    io: &mut impl GmbusIo,
    bus: &GmbusBus,
    state_high: bool,
) -> Result<(), GmbusError> {
    let reserved = get_reserved(io, bus)?;
    let clock_bits = if state_high {
        GPIO_CLOCK_DIR_IN | GPIO_CLOCK_DIR_MASK
    } else {
        GPIO_CLOCK_DIR_OUT | GPIO_CLOCK_DIR_MASK | GPIO_CLOCK_VAL_MASK
    };

    io.write32(bus.gpio_reg, reserved | clock_bits, Access::Untraced)?;
    io.posting_read(bus.gpio_reg)?;
    Ok(())
}

/// Drive or release the GPIO data line and flush the posted write.
// upstream: intel_gmbus.c set_data()
pub fn set_data(io: &mut impl GmbusIo, bus: &GmbusBus, state_high: bool) -> Result<(), GmbusError> {
    let reserved = get_reserved(io, bus)?;
    let data_bits = if state_high {
        GPIO_DATA_DIR_IN | GPIO_DATA_DIR_MASK
    } else {
        GPIO_DATA_DIR_OUT | GPIO_DATA_DIR_MASK | GPIO_DATA_VAL_MASK
    };

    io.write32(bus.gpio_reg, reserved | data_bits, Access::Untraced)?;
    io.posting_read(bus.gpio_reg)?;
    Ok(())
}

/// Set or clear the four GPIO mask bits for Wa_16025573575.
// upstream: intel_gmbus.c ptl_handle_mask_bits()
pub fn ptl_handle_mask_bits(
    io: &mut impl GmbusIo,
    bus: &GmbusBus,
    set: bool,
) -> Result<(), GmbusError> {
    let mut reg_val = io.read32(bus.gpio_reg, Access::Untraced)?;
    let mask_bits =
        GPIO_CLOCK_DIR_MASK | GPIO_CLOCK_VAL_MASK | GPIO_DATA_DIR_MASK | GPIO_DATA_VAL_MASK;
    if set {
        reg_val |= mask_bits;
    } else {
        reg_val &= !mask_bits;
    }

    io.write32(bus.gpio_reg, reg_val, Access::Untraced)?;
    io.posting_read(bus.gpio_reg)?;
    Ok(())
}

/// Prepare GPIO bit-banging before an I2C transfer.
// upstream: intel_gmbus.c intel_gpio_pre_xfer()
pub fn intel_gpio_pre_xfer(io: &mut impl GmbusIo, bus: &GmbusBus) -> Result<(), GmbusError> {
    intel_gmbus_reset(io, bus.display)?;

    if bus.display.platform.pineview {
        pnv_gmbus_clock_gating(io, false)?;
    }

    if bus.display.wa_16025573575 {
        ptl_handle_mask_bits(io, bus, true)?;
    }

    set_data(io, bus, true)?;
    set_clock(io, bus, true)?;
    io.delay_us(I2C_RISEFALL_TIME);
    Ok(())
}

/// Restore released GPIO levels and clock-gating state after bit-banging.
// upstream: intel_gmbus.c intel_gpio_post_xfer()
pub fn intel_gpio_post_xfer(io: &mut impl GmbusIo, bus: &GmbusBus) -> Result<(), GmbusError> {
    set_data(io, bus, true)?;
    set_clock(io, bus, true)?;

    if bus.display.platform.pineview {
        pnv_gmbus_clock_gating(io, true)?;
    }

    if bus.display.wa_16025573575 {
        ptl_handle_mask_bits(io, bus, false)?;
    }

    Ok(())
}

/// Check both the display's GMBUS IRQ support and parent IRQ state.
// upstream: intel_gmbus.c has_gmbus_irq()
pub fn has_gmbus_irq(io: &impl GmbusIo, display: GmbusDisplay) -> bool {
    // encoder->shutdown() may want GMBUS after irqs have already been disabled.
    display.display_version >= 4 && io.parent_irq_enabled()
}

/// Wait for a GMBUS status bit, including NAK polling and IRQ arming.
// upstream: intel_gmbus.c gmbus_wait()
pub fn gmbus_wait(
    io: &mut impl GmbusIo,
    display: GmbusDisplay,
    mut status: u32,
    mut irq_en: u32,
) -> Result<(), GmbusError> {
    // The hardware handles only the first bit, so set only one. NAK is checked
    // by polling because the hardware does not provide its own interrupt bit.
    if !has_gmbus_irq(io, display) {
        irq_en = 0;
    }

    io.add_waiter();
    let wait_result = (|| {
        io.write32(
            gmbus_register(display, GMBUS4_OFFSET),
            irq_en,
            Access::Firmware,
        )?;
        status |= GMBUS_SATOER;
        let mut polled =
            io.poll_timeout_us_atomic(gmbus_register(display, GMBUS2_OFFSET), status, 2)?;
        if !polled.matched {
            polled = io.poll_timeout_us(
                gmbus_register(display, GMBUS2_OFFSET),
                status,
                500,
                50 * 1000,
            )?;
        }
        Ok::<PollResult, GmbusError>(polled)
    })();
    let disable_result = io.write32(gmbus_register(display, GMBUS4_OFFSET), 0, Access::Firmware);
    io.remove_waiter();
    let polled = wait_result?;
    disable_result?;

    if polled.status & GMBUS_SATOER != 0 {
        return Err(GmbusError::NoDevice);
    }
    if !polled.matched {
        return Err(GmbusError::Timeout);
    }

    Ok(())
}

/// Wait for the GMBUS active bit to clear, optionally using its IRQ.
// upstream: intel_gmbus.c gmbus_wait_idle()
pub fn gmbus_wait_idle(io: &mut impl GmbusIo, display: GmbusDisplay) -> Result<(), GmbusError> {
    // The hardware handles only the first interrupt-enable bit.
    let mut irq_enable = 0;
    if has_gmbus_irq(io, display) {
        irq_enable = GMBUS_IDLE_EN;
    }

    io.add_waiter();
    let wait_result = (|| {
        io.write32(
            gmbus_register(display, GMBUS4_OFFSET),
            irq_enable,
            Access::Firmware,
        )?;
        io.wait_register_mask_ms(gmbus_register(display, GMBUS2_OFFSET), GMBUS_ACTIVE, 0, 10)
    })();
    let disable_result = io.write32(gmbus_register(display, GMBUS4_OFFSET), 0, Access::Firmware);
    io.remove_waiter();
    wait_result?;
    disable_result
}

/// Return the controller byte-count limit for this display version.
// upstream: intel_gmbus.c gmbus_max_xfer_size()
pub const fn gmbus_max_xfer_size(display: GmbusDisplay) -> usize {
    if display.display_version >= 9 {
        GEN9_GMBUS_BYTE_COUNT_MAX
    } else {
        GMBUS_BYTE_COUNT_MAX
    }
}

/// Transfer one hardware GMBUS read chunk, including burst override handling.
// upstream: intel_gmbus.c gmbus_xfer_read_chunk()
pub fn gmbus_xfer_read_chunk(
    io: &mut impl GmbusIo,
    display: GmbusDisplay,
    address: u16,
    buffer: &mut [u8],
    mut len: usize,
    gmbus0_reg: u32,
    gmbus1_index: u32,
) -> Result<(), GmbusError> {
    let mut size = len;
    let burst_read = len > gmbus_max_xfer_size(display);
    let mut extra_byte_added = false;

    if burst_read {
        // As per HW spec, for 512 bytes need to read an extra byte and ignore it.
        if len == 512 {
            extra_byte_added = true;
            len += 1;
        }
        size = len % 256 + 256;
        io.write32(
            gmbus_register(display, GMBUS0_OFFSET),
            gmbus0_reg | GMBUS_BYTE_CNT_OVERRIDE,
            Access::Firmware,
        )?;
    }

    io.write32(
        gmbus_register(display, GMBUS1_OFFSET),
        gmbus1_index
            | GMBUS_CYCLE_WAIT
            | ((size as u32) << GMBUS_BYTE_COUNT_SHIFT)
            | ((address as u32) << GMBUS_SLAVE_ADDR_SHIFT)
            | GMBUS_SLAVE_READ
            | GMBUS_SW_RDY,
        Access::Firmware,
    )?;
    let mut buffer_index = 0;
    while len != 0 {
        gmbus_wait(io, display, GMBUS_HW_RDY, GMBUS_HW_RDY_EN)?;

        let mut val = io.read32(gmbus_register(display, GMBUS3_OFFSET), Access::Firmware)?;
        let mut loop_count = 0;
        loop {
            if extra_byte_added && len == 1 {
                len -= 1;
                break;
            }

            let slot = buffer.get_mut(buffer_index).ok_or(GmbusError::Invalid)?;
            *slot = val as u8;
            buffer_index += 1;
            val >>= 8;
            len -= 1;
            if len == 0 {
                break;
            }
            loop_count += 1;
            if loop_count >= 4 {
                break;
            }
        }

        if burst_read && len == size - 4 {
            // Reset the override bit.
            io.write32(
                gmbus_register(display, GMBUS0_OFFSET),
                gmbus0_reg,
                Access::Firmware,
            )?;
        }
    }

    Ok(())
}

/// Read all bytes of one I2C message in hardware-sized chunks.
// upstream: intel_gmbus.c gmbus_xfer_read()
pub fn gmbus_xfer_read(
    io: &mut impl GmbusIo,
    display: GmbusDisplay,
    message: &mut I2cMessage<'_>,
    gmbus0_reg: u32,
    gmbus1_index: u32,
) -> Result<(), GmbusError> {
    let mut rx_size = message.len;
    if rx_size > message.buffer.len() {
        return Err(GmbusError::Invalid);
    }
    let mut offset = 0;
    loop {
        let len = if display.display_version >= 10 || display.platform.kabylake {
            min(rx_size, INTEL_GMBUS_BURST_READ_MAX_LEN)
        } else {
            min(rx_size, gmbus_max_xfer_size(display))
        };

        gmbus_xfer_read_chunk(
            io,
            display,
            message.address,
            &mut message.buffer[offset..offset + len],
            len,
            gmbus0_reg,
            gmbus1_index,
        )?;

        rx_size -= len;
        offset += len;
        if rx_size == 0 {
            break;
        }
    }

    Ok(())
}

/// Transfer one hardware GMBUS write chunk.
// upstream: intel_gmbus.c gmbus_xfer_write_chunk()
pub fn gmbus_xfer_write_chunk(
    io: &mut impl GmbusIo,
    display: GmbusDisplay,
    address: u16,
    buffer: &[u8],
    mut len: usize,
    gmbus1_index: u32,
) -> Result<(), GmbusError> {
    let chunk_size = len;
    if len > buffer.len() {
        return Err(GmbusError::Invalid);
    }
    let mut val = 0u32;
    let mut loop_count = 0;
    while len != 0 && loop_count < 4 {
        val |= u32::from(buffer[chunk_size - len]) << (8 * loop_count);
        loop_count += 1;
        len -= 1;
    }

    io.write32(
        gmbus_register(display, GMBUS3_OFFSET),
        val,
        Access::Firmware,
    )?;
    io.write32(
        gmbus_register(display, GMBUS1_OFFSET),
        gmbus1_index
            | GMBUS_CYCLE_WAIT
            | ((chunk_size as u32) << GMBUS_BYTE_COUNT_SHIFT)
            | ((address as u32) << GMBUS_SLAVE_ADDR_SHIFT)
            | GMBUS_SLAVE_WRITE
            | GMBUS_SW_RDY,
        Access::Firmware,
    )?;
    let mut buffer_index = chunk_size - len;
    while len != 0 {
        val = 0;
        loop_count = 0;
        loop {
            val |= u32::from(buffer[buffer_index]) << (8 * loop_count);
            buffer_index += 1;
            len -= 1;
            if len == 0 {
                break;
            }
            loop_count += 1;
            if loop_count >= 4 {
                break;
            }
        }

        io.write32(
            gmbus_register(display, GMBUS3_OFFSET),
            val,
            Access::Firmware,
        )?;
        gmbus_wait(io, display, GMBUS_HW_RDY, GMBUS_HW_RDY_EN)?;
    }

    Ok(())
}

/// Write all bytes of one I2C message in hardware-sized chunks.
// upstream: intel_gmbus.c gmbus_xfer_write()
pub fn gmbus_xfer_write(
    io: &mut impl GmbusIo,
    display: GmbusDisplay,
    message: &mut I2cMessage<'_>,
    gmbus1_index: u32,
) -> Result<(), GmbusError> {
    let mut tx_size = message.len;
    if tx_size > message.buffer.len() {
        return Err(GmbusError::Invalid);
    }
    let mut offset = 0;
    loop {
        let len = min(tx_size, gmbus_max_xfer_size(display));
        gmbus_xfer_write_chunk(
            io,
            display,
            message.address,
            &message.buffer[offset..offset + len],
            len,
            gmbus1_index,
        )?;

        offset += len;
        tx_size -= len;
        if tx_size == 0 {
            break;
        }
    }

    Ok(())
}

/// Determine whether an I2C message pair can use the hardware INDEX cycle.
// upstream: intel_gmbus.c gmbus_is_index_xfer()
pub fn gmbus_is_index_xfer(messages: &[I2cMessage<'_>], index: usize) -> bool {
    index + 1 < messages.len()
        && messages[index].address == messages[index + 1].address
        && messages[index].flags & I2C_M_RD == 0
        && (messages[index].len == 1 || messages[index].len == 2)
        && messages[index + 1].len > 0
}

/// Perform a one- or two-byte indexed GMBUS transfer.
// upstream: intel_gmbus.c gmbus_index_xfer()
pub fn gmbus_index_xfer(
    io: &mut impl GmbusIo,
    display: GmbusDisplay,
    messages: &mut [I2cMessage<'_>],
    gmbus0_reg: u32,
) -> Result<(), GmbusError> {
    let mut gmbus1_index = 0u32;
    let mut gmbus5 = 0u32;
    if messages[0].len == 2 {
        gmbus5 = GMBUS_2BYTE_INDEX_EN
            | u32::from(messages[0].buffer[1])
            | (u32::from(messages[0].buffer[0]) << 8);
    }
    if messages[0].len == 1 {
        gmbus1_index =
            GMBUS_CYCLE_INDEX | (u32::from(messages[0].buffer[0]) << GMBUS_SLAVE_INDEX_SHIFT);
    }

    // GMBUS5 holds the 16-bit index.
    if gmbus5 != 0 {
        io.write32(
            gmbus_register(display, GMBUS5_OFFSET),
            gmbus5,
            Access::Firmware,
        )?;
    }

    let ret = if messages[1].flags & I2C_M_RD != 0 {
        gmbus_xfer_read(io, display, &mut messages[1], gmbus0_reg, gmbus1_index)
    } else {
        gmbus_xfer_write(io, display, &mut messages[1], gmbus1_index)
    };

    // Clear GMBUS5 after each index transfer.
    if gmbus5 != 0 {
        io.write32(gmbus_register(display, GMBUS5_OFFSET), 0, Access::Firmware)?;
    }

    ret
}

/// Execute GMBUS messages, recover from NAKs, and fall back after a timeout.
// upstream: intel_gmbus.c do_gmbus_xfer()
pub fn do_gmbus_xfer(
    io: &mut impl GmbusIo,
    bus: &GmbusBus,
    messages: &mut [I2cMessage<'_>],
    gmbus0_source: u32,
) -> Result<usize, GmbusError> {
    let display = bus.display;
    let mut i = 0usize;
    let mut try_count = 0;

    // Display WA #0868: skl,bxt,kbl,cfl,glk.
    if display.platform.geminilake || display.platform.broxton {
        bxt_gmbus_clock_gating(io, false)?;
    } else if display.has_pch_spt || display.has_pch_cnp {
        pch_gmbus_clock_gating(io, false)?;
    }

    let transfer_result = (|| {
        'retry: loop {
            io.write32(
                gmbus_register(display, GMBUS0_OFFSET),
                gmbus0_source | bus.reg0,
                Access::Firmware,
            )?;

            while i < messages.len() {
                let mut increment = 1;
                let transfer = if gmbus_is_index_xfer(messages, i) {
                    increment = 2; // An index transmission is two messages.
                    gmbus_index_xfer(io, display, &mut messages[i..], gmbus0_source | bus.reg0)
                } else if messages[i].flags & I2C_M_RD != 0 {
                    gmbus_xfer_read(io, display, &mut messages[i], gmbus0_source | bus.reg0, 0)
                } else {
                    gmbus_xfer_write(io, display, &mut messages[i], 0)
                };

                let transfer = match transfer {
                    Ok(()) => gmbus_wait(io, display, GMBUS_HW_WAIT_PHASE, GMBUS_HW_WAIT_EN),
                    Err(error) => Err(error),
                };
                match transfer {
                    Ok(()) => i += increment,
                    Err(GmbusError::Timeout) => {
                        io.diagnostic(GmbusDiagnostic::TimeoutBitBang {
                            adapter: bus.adapter_name,
                            pin: bus.reg0 & 0xff,
                        });
                        io.write32(gmbus_register(display, GMBUS0_OFFSET), 0, Access::Firmware)?;
                        // Hardware may not support GMBUS over these pins. Use
                        // EAGAIN to have I2C core retry with GPIO bit-banging.
                        return Err(GmbusError::Again);
                    }
                    Err(error) => {
                        // Wait for IDLE before clearing NAK. If it does not
                        // quiesce, preserve the upstream timeout result.
                        let mut ret = GmbusError::NoDevice;
                        if gmbus_wait_idle(io, display).is_err() {
                            io.diagnostic(GmbusDiagnostic::NakIdleTimeout {
                                adapter: bus.adapter_name,
                            });
                            ret = GmbusError::Timeout;
                        }

                        io.write32(
                            gmbus_register(display, GMBUS1_OFFSET),
                            GMBUS_SW_CLR_INT,
                            Access::Firmware,
                        )?;
                        io.write32(gmbus_register(display, GMBUS1_OFFSET), 0, Access::Firmware)?;
                        io.write32(gmbus_register(display, GMBUS0_OFFSET), 0, Access::Firmware)?;

                        io.diagnostic(GmbusDiagnostic::Nak {
                            adapter: bus.adapter_name,
                            address: messages[i].address,
                            read: messages[i].flags & I2C_M_RD != 0,
                            len: messages[i].len,
                        });

                        // Passive adapters may NAK the first probe. Retry only
                        // that first message once; bit-banging retries inside.
                        if ret == GmbusError::NoDevice && i == 0 && try_count == 0 {
                            try_count += 1;
                            io.diagnostic(GmbusDiagnostic::FirstMessageNakRetry {
                                adapter: bus.adapter_name,
                            });
                            continue 'retry;
                        }
                        if !matches!(error, GmbusError::NoDevice | GmbusError::Timeout) {
                            return Err(error);
                        }
                        return Err(ret);
                    }
                }
            }

            // Generate STOP with the unconditional additional GMBUS cycle.
            io.write32(
                gmbus_register(display, GMBUS1_OFFSET),
                GMBUS_CYCLE_STOP | GMBUS_SW_RDY,
                Access::Firmware,
            )?;

            // Disable the interface after waiting for idle; it is re-enabled
            // next time and can sleep in the meantime.
            let mut ret = Ok(i);
            if gmbus_wait_idle(io, display).is_err() {
                io.diagnostic(GmbusDiagnostic::IdleTimeout {
                    adapter: bus.adapter_name,
                });
                ret = Err(GmbusError::Timeout);
            }
            io.write32(gmbus_register(display, GMBUS0_OFFSET), 0, Access::Firmware)?;
            return ret;
        }
    })();

    // Display WA #0868: restore clock gating on every path leaving transfer.
    if display.platform.geminilake || display.platform.broxton {
        bxt_gmbus_clock_gating(io, true)?;
    } else if display.has_pch_spt || display.has_pch_cnp {
        pch_gmbus_clock_gating(io, true)?;
    }

    transfer_result
}

/// Transfer through GMBUS or the bit-bang algorithm selected for this bus.
// upstream: intel_gmbus.c gmbus_xfer()
pub fn gmbus_xfer(
    io: &mut impl GmbusIo,
    bus: &mut GmbusBus,
    messages: &mut [I2cMessage<'_>],
) -> Result<usize, GmbusError> {
    io.display_power_get()?;
    let ret = if bus.force_bit != 0 {
        let result = io.bitbang_master_xfer(messages);
        if result.is_err() {
            bus.force_bit &= !GMBUS_FORCE_BIT_RETRY;
        }
        result
    } else {
        let result = do_gmbus_xfer(io, bus, messages, 0);
        if result == Err(GmbusError::Again) {
            bus.force_bit |= GMBUS_FORCE_BIT_RETRY;
        }
        result
    };
    io.display_power_put();
    ret
}

/// Output the HDCP AKSV using the hardware-provided GMBUS data value.
// upstream: intel_gmbus.c intel_gmbus_output_aksv()
pub fn intel_gmbus_output_aksv(io: &mut impl GmbusIo, bus: &GmbusBus) -> Result<usize, GmbusError> {
    let mut command = [DRM_HDCP_DDC_AKSV];
    let mut buffer = [0u8; DRM_HDCP_KSV_LEN];
    let mut messages = [
        I2cMessage {
            address: DRM_HDCP_DDC_ADDR,
            flags: 0,
            len: command.len(),
            buffer: &mut command,
        },
        I2cMessage {
            address: DRM_HDCP_DDC_ADDR,
            flags: 0,
            len: buffer.len(),
            buffer: &mut buffer,
        },
    ];

    io.display_power_get()?;
    io.gmbus_mutex_lock();
    // Use an indexed write for the command and select the hardware AKSV source.
    let ret = do_gmbus_xfer(io, bus, &mut messages, GMBUS_AKSV_SELECT);
    io.gmbus_mutex_unlock();
    io.display_power_put();
    ret
}

/// Wake waiters registered by `gmbus_wait()` or `gmbus_wait_idle()`.
// upstream: intel_gmbus.c intel_gmbus_irq_handler()
pub fn intel_gmbus_irq_handler(io: &mut impl GmbusIo) {
    io.wake_waiters();
}

/// Increment or decrement the force-bit-bang count under the GMBUS lock.
// upstream: intel_gmbus.c intel_gmbus_force_bit()
pub fn intel_gmbus_force_bit(io: &mut impl GmbusIo, bus: &mut GmbusBus, force_bit: bool) {
    io.gmbus_mutex_lock();
    bus.force_bit = if force_bit {
        bus.force_bit.wrapping_add(1)
    } else {
        bus.force_bit.wrapping_sub(1)
    };
    io.diagnostic(GmbusDiagnostic::ForceBit {
        adapter: bus.adapter_name,
        enabled: force_bit,
        count: bus.force_bit,
    });
    io.gmbus_mutex_unlock();
}

/// Report whether this adapter is currently forced to use bit-banging.
// upstream: intel_gmbus.c intel_gmbus_is_forced_bit()
pub const fn intel_gmbus_is_forced_bit(bus: &GmbusBus) -> bool {
    bus.force_bit != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_pin_tables_follow_pch_and_platform_order() {
        let mut display = GmbusDisplay::default();
        assert_eq!(get_gmbus_pin(display, 1).unwrap().name, "ssc");
        display.platform.broadwell = true;
        assert_eq!(get_gmbus_pin(display, 1), None);
        assert_eq!(get_gmbus_pin(display, 2).unwrap().name, "vga");
        display.platform.broadwell = false;
        display.display_version = 9;
        assert_eq!(get_gmbus_pin(display, 3), None);
        assert_eq!(get_gmbus_pin(display, 4).unwrap().name, "dpc");
        display.pch_type = PchType::Icp;
        assert_eq!(get_gmbus_pin(display, 1).unwrap().name, "dpa");
        assert_eq!(get_gmbus_pin(display, 9).unwrap().name, "tc1");
    }

    #[test]
    fn index_cycle_requires_matching_short_write_and_followup() {
        let mut index = [0x50];
        let mut data = [0u8; 8];
        let messages = [
            I2cMessage {
                address: 0x50,
                flags: 0,
                len: 1,
                buffer: &mut index,
            },
            I2cMessage {
                address: 0x50,
                flags: I2C_M_RD,
                len: 8,
                buffer: &mut data,
            },
        ];
        assert!(gmbus_is_index_xfer(&messages, 0));
        assert!(!gmbus_is_index_xfer(&messages, 1));
        assert_eq!(
            gmbus_max_xfer_size(GmbusDisplay {
                display_version: 8,
                ..GmbusDisplay::default()
            }),
            256
        );
        assert_eq!(
            gmbus_max_xfer_size(GmbusDisplay {
                display_version: 12,
                ..GmbusDisplay::default()
            }),
            511
        );
    }
}
