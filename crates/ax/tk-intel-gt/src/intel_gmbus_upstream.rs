// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_gmbus.c (Linux v7.2.3).
// ADL-P/PCH-ADP DDC/GMBUS implementation, with legacy pin tables unsupported.
#![allow(non_snake_case, dead_code, unsafe_code, clippy::missing_safety_doc)]

use core::{ffi::c_void, mem::size_of};

use tk_linux_kapi::{
    config::{EAGAIN, EINVAL, ENOMEM, ENXIO, ETIMEDOUT, GFP_KERNEL},
    i2c::{
        I2C_ADAPTER_IDR, I2C_FUNC_I2C, I2C_FUNC_SMBUS_BLOCK_PROC_CALL, I2C_FUNC_SMBUS_EMUL,
        I2C_FUNC_SMBUS_READ_BLOCK_DATA, I2C_M_RD, I2cAdapter, I2cAlgorithm, I2cMsg,
        i2c_add_adapter, i2c_del_adapter,
    },
    i2c_algo_bit::{I2cAlgoBitData, I2cBitAlgo, I2cBitLines},
    memory::{kfree, kzalloc},
    types::WaitQueueEntry,
    wait::{
        TASK_UNINTERRUPTIBLE, add_wait_queue, default_wake_function, init_waitqueue_head,
        remove_wait_queue, wait_until, wait_woken, wake_up_all,
    },
};

use crate::{
    intel_de_upstream::{
        intel_de_posting_read, intel_de_read_fw, intel_de_read_notrace, intel_de_wait_fw_ms,
        intel_de_write_fw, intel_de_write_notrace,
    },
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_power_types_upstream::POWER_DOMAIN_GMBUS,
    intel_display_reg_defs_upstream::DISPLAY_VER,
    intel_display_wa_upstream::INTEL_PCH_TYPE,
    intel_gmbus_regs_upstream::*,
    intel_pch_types_upstream::{PCH_ADP, PCH_ICP},
    intel_workarounds_types_upstream::I915RegT,
};

const GMBUS_FORCE_BIT_RETRY: u32 = 1 << 31;
const GMBUS_GPIO_COUNT: usize = 15;
const I2C_RISEFALL_TIME: u32 = 10;
const ADAPTER_RETRIES: u32 = 1;
const GMBUS_TIMEOUT_US: u32 = 50_000;

#[derive(Clone, Copy)]
struct GmbusPin {
    name: &'static str,
    gpio: u32,
}

// The Linux ICL/TGP table also serves ADL-P's PCH_ADP.
const ADLP_GMBUS_PINS: [Option<GmbusPin>; GMBUS_GPIO_COUNT] = {
    let mut pins = [None; GMBUS_GPIO_COUNT];
    pins[1] = Some(GmbusPin {
        name: "dpa",
        gpio: 1,
    });
    pins[2] = Some(GmbusPin {
        name: "dpb",
        gpio: 2,
    });
    pins[3] = Some(GmbusPin {
        name: "dpc",
        gpio: 3,
    });
    pins[9] = Some(GmbusPin {
        name: "tc1",
        gpio: 9,
    });
    pins[10] = Some(GmbusPin {
        name: "tc2",
        gpio: 10,
    });
    pins[11] = Some(GmbusPin {
        name: "tc3",
        gpio: 11,
    });
    pins[12] = Some(GmbusPin {
        name: "tc4",
        gpio: 12,
    });
    pins[13] = Some(GmbusPin {
        name: "tc5",
        gpio: 13,
    });
    pins[14] = Some(GmbusPin {
        name: "tc6",
        gpio: 14,
    });
    pins
};

#[repr(C)]
struct IntelGmbus {
    // Must be first: the connector DDC pointer is this adapter pointer.
    adapter: I2cAdapter<'static>,
    display: *mut IntelDisplay,
    gpio_reg: I915RegT,
    reg0: u32,
    force_bit: u32,
}

// GMBUS state is serialized by display.gmbus.mutex; adapter locking is handled
// by I2cAdapter before invoking this algorithm.
unsafe impl Send for IntelGmbus {}
unsafe impl Sync for IntelGmbus {}

impl I2cBitLines for IntelGmbus {
    fn setsda(&self, high: i32) {
        unsafe { set_data(self, high != 0) }
    }
    fn setscl(&self, high: i32) {
        unsafe { set_clock(self, high != 0) }
    }
    fn getsda(&self) -> Option<i32> {
        Some(unsafe { get_data(self) })
    }
    fn getscl(&self) -> Option<i32> {
        Some(unsafe { get_clock(self) })
    }
    fn pre_xfer(&self, _adapter: &I2cAdapter<'_>) -> i32 {
        unsafe { gpio_pre_xfer(self) }
    }
    fn post_xfer(&self, _adapter: &I2cAdapter<'_>) {
        unsafe { gpio_post_xfer(self) }
    }
}

impl I2cAlgorithm for IntelGmbus {
    fn master_xfer(&self, _adapter: &I2cAdapter<'_>, msgs: &mut [I2cMsg<'_>]) -> i32 {
        unsafe { gmbus_xfer(self, msgs) }
    }

    fn functionality(&self) -> u32 {
        I2C_FUNC_I2C
            | I2C_FUNC_SMBUS_EMUL
            | I2C_FUNC_SMBUS_READ_BLOCK_DATA
            | I2C_FUNC_SMBUS_BLOCK_PROC_CALL
    }
}

unsafe fn pin_for(display: *mut IntelDisplay, pin: u32) -> Option<GmbusPin> {
    if display.is_null()
        || DISPLAY_VER(display) != 13
        || INTEL_PCH_TYPE(display) < PCH_ICP
        || INTEL_PCH_TYPE(display) != PCH_ADP
    {
        return None;
    }
    ADLP_GMBUS_PINS.get(pin as usize).copied().flatten()
}

unsafe fn has_gmbus_irq(display: *mut IntelDisplay) -> bool {
    DISPLAY_VER(display) >= 4 && crate::intel_parent_upstream::intel_parent_irq_enabled(display)
}

unsafe fn get_reserved(bus: *const IntelGmbus) -> u32 {
    let display = (*bus).display;
    let mut preserve = GPIO_DATA_PULLUP_DISABLE | GPIO_CLOCK_PULLUP_DISABLE;
    if crate::intel_display_wa_upstream::__intel_display_wa(
        display,
        crate::intel_display_wa_upstream::IntelDisplayWa::Wa16025573575,
        core::ptr::null(),
    ) {
        preserve |=
            GPIO_CLOCK_DIR_MASK | GPIO_CLOCK_VAL_MASK | GPIO_DATA_DIR_MASK | GPIO_DATA_VAL_MASK;
    }
    intel_de_read_notrace(display, (*bus).gpio_reg) & preserve
}

unsafe fn get_clock(bus: *const IntelGmbus) -> i32 {
    let display = (*bus).display;
    let reserved = get_reserved(bus);
    intel_de_write_notrace(display, (*bus).gpio_reg, reserved | GPIO_CLOCK_DIR_MASK);
    intel_de_write_notrace(display, (*bus).gpio_reg, reserved);
    i32::from(intel_de_read_notrace(display, (*bus).gpio_reg) & GPIO_CLOCK_VAL_IN != 0)
}

unsafe fn get_data(bus: *const IntelGmbus) -> i32 {
    let display = (*bus).display;
    let reserved = get_reserved(bus);
    intel_de_write_notrace(display, (*bus).gpio_reg, reserved | GPIO_DATA_DIR_MASK);
    intel_de_write_notrace(display, (*bus).gpio_reg, reserved);
    i32::from(intel_de_read_notrace(display, (*bus).gpio_reg) & GPIO_DATA_VAL_IN != 0)
}

unsafe fn set_clock(bus: *const IntelGmbus, high: bool) {
    let display = (*bus).display;
    let reserved = get_reserved(bus);
    let bits = if high {
        GPIO_CLOCK_DIR_IN | GPIO_CLOCK_DIR_MASK
    } else {
        GPIO_CLOCK_DIR_OUT | GPIO_CLOCK_DIR_MASK | GPIO_CLOCK_VAL_MASK
    };
    intel_de_write_notrace(display, (*bus).gpio_reg, reserved | bits);
    intel_de_posting_read(display, (*bus).gpio_reg);
}

unsafe fn set_data(bus: *const IntelGmbus, high: bool) {
    let display = (*bus).display;
    let reserved = get_reserved(bus);
    let bits = if high {
        GPIO_DATA_DIR_IN | GPIO_DATA_DIR_MASK
    } else {
        GPIO_DATA_DIR_OUT | GPIO_DATA_DIR_MASK | GPIO_DATA_VAL_MASK
    };
    intel_de_write_notrace(display, (*bus).gpio_reg, reserved | bits);
    intel_de_posting_read(display, (*bus).gpio_reg);
}

unsafe fn ptl_handle_mask_bits(bus: *const IntelGmbus, set: bool) {
    let display = (*bus).display;
    let mask = GPIO_CLOCK_DIR_MASK | GPIO_CLOCK_VAL_MASK | GPIO_DATA_DIR_MASK | GPIO_DATA_VAL_MASK;
    let mut value = intel_de_read_notrace(display, (*bus).gpio_reg);
    if set {
        value |= mask;
    } else {
        value &= !mask;
    }
    intel_de_write_notrace(display, (*bus).gpio_reg, value);
    intel_de_posting_read(display, (*bus).gpio_reg);
}

unsafe fn gpio_pre_xfer(bus: *const IntelGmbus) -> i32 {
    intel_gmbus_reset((*bus).display);
    if crate::intel_display_wa_upstream::__intel_display_wa(
        (*bus).display,
        crate::intel_display_wa_upstream::IntelDisplayWa::Wa16025573575,
        core::ptr::null(),
    ) {
        ptl_handle_mask_bits(bus, true);
    }
    set_data(bus, true);
    set_clock(bus, true);
    tk_linux_kapi::primitives::udelay(I2C_RISEFALL_TIME);
    0
}

unsafe fn gpio_post_xfer(bus: *const IntelGmbus) {
    set_data(bus, true);
    set_clock(bus, true);
    if crate::intel_display_wa_upstream::__intel_display_wa(
        (*bus).display,
        crate::intel_display_wa_upstream::IntelDisplayWa::Wa16025573575,
        core::ptr::null(),
    ) {
        ptl_handle_mask_bits(bus, false);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gmbus_reset(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    intel_de_write_fw(display, GMBUS0(display), 0);
    intel_de_write_fw(display, GMBUS4(display), 0);
}

unsafe fn gmbus_wait(display: *mut IntelDisplay, status: u32, mut irq_en: u32) -> i32 {
    if !has_gmbus_irq(display) {
        irq_en = 0;
    }
    let reg = GMBUS2(display);
    let irq_reg = GMBUS4(display);
    let waitq = core::ptr::addr_of_mut!((*display).gmbus.wait_queue);
    let mut entry: WaitQueueEntry = core::mem::zeroed();
    entry.func = Some(default_wake_function);
    add_wait_queue(&mut *waitq, &mut entry);
    intel_de_write_fw(display, irq_reg, irq_en);
    let mask = status | GMBUS_SATOER;
    let start = tk_linux_kapi::clock::now_ns();
    let fast_end = start.saturating_add(2_000);
    let end = start.saturating_add(u64::from(GMBUS_TIMEOUT_US) * 1000);
    let mut value = 0u32;
    loop {
        value = intel_de_read_fw(display, reg);
        if value & mask != 0 {
            break;
        }
        let now = tk_linux_kapi::clock::now_ns();
        if now >= end {
            break;
        }
        if now < fast_end {
            tk_linux_kapi::clock::relax();
        } else if irq_en != 0 {
            let remaining_us = ((end - now) / 1000).min(u32::MAX as u64) as u32;
            let ticks = tk_linux_kapi::primitives::usecs_to_jiffies(remaining_us);
            let _ = wait_woken(&mut entry, TASK_UNINTERRUPTIBLE, ticks as i64);
        } else {
            let _ = wait_until(
                end - now,
                || intel_de_read_fw(display, reg) & mask != 0,
                true,
            );
        }
    }
    intel_de_write_fw(display, irq_reg, 0);
    remove_wait_queue(&mut *waitq, &mut entry);
    if value & GMBUS_SATOER != 0 {
        -ENXIO
    } else if value & status != 0 {
        0
    } else {
        -ETIMEDOUT
    }
}

unsafe fn gmbus_wait_idle(display: *mut IntelDisplay) -> i32 {
    let irq_en = if has_gmbus_irq(display) {
        GMBUS_IDLE_EN
    } else {
        0
    };
    let irq_reg = GMBUS4(display);
    intel_de_write_fw(display, irq_reg, irq_en);
    let ret = intel_de_wait_fw_ms(
        display,
        GMBUS2(display),
        GMBUS_ACTIVE,
        0,
        10,
        core::ptr::null_mut(),
    );
    intel_de_write_fw(display, irq_reg, 0);
    ret
}

unsafe fn max_xfer_size(display: *mut IntelDisplay) -> usize {
    if DISPLAY_VER(display) >= 9 {
        GEN9_GMBUS_BYTE_COUNT_MAX as usize
    } else {
        GMBUS_BYTE_COUNT_MAX as usize
    }
}

unsafe fn gmbus_read_chunk(
    display: *mut IntelDisplay,
    addr: u16,
    buf: &mut [u8],
    mut len: usize,
    gmbus0: u32,
    index: u32,
) -> i32 {
    let mut size = len;
    let burst = len > max_xfer_size(display);
    let extra = burst && len == 512;
    if burst {
        if extra {
            len += 1;
        }
        size = len % 256 + 256;
        intel_de_write_fw(display, GMBUS0(display), gmbus0 | GMBUS_BYTE_CNT_OVERRIDE);
    }
    intel_de_write_fw(
        display,
        GMBUS1(display),
        index
            | GMBUS_CYCLE_WAIT
            | ((size as u32) << GMBUS_BYTE_COUNT_SHIFT)
            | ((addr as u32) << GMBUS_SLAVE_ADDR_SHIFT)
            | GMBUS_SLAVE_READ
            | GMBUS_SW_RDY,
    );
    let mut remaining = len;
    let mut delivered = 0usize;
    while remaining != 0 {
        let ret = gmbus_wait(display, GMBUS_HW_RDY, GMBUS_HW_RDY_EN);
        if ret != 0 {
            return ret;
        }
        let mut value = intel_de_read_fw(display, GMBUS3(display));
        let mut word_bytes = 0;
        while remaining != 0 && word_bytes < 4 {
            if extra && remaining == 1 {
                remaining -= 1;
                break;
            }
            if delivered < buf.len() {
                buf[delivered] = value as u8;
                delivered += 1;
            }
            value >>= 8;
            remaining -= 1;
            word_bytes += 1;
        }
        if burst && remaining == size.saturating_sub(4) {
            intel_de_write_fw(display, GMBUS0(display), gmbus0);
        }
    }
    0
}

unsafe fn gmbus_read(
    display: *mut IntelDisplay,
    msg: &mut I2cMsg<'_>,
    gmbus0: u32,
    index: u32,
) -> i32 {
    let mut offset = 0usize;
    let mut remaining = msg.len;
    let mut first = true;
    while first || remaining != 0 {
        first = false;
        let len = if DISPLAY_VER(display) >= 10 {
            remaining.min(767)
        } else {
            remaining.min(max_xfer_size(display))
        };
        let ret = gmbus_read_chunk(
            display,
            msg.addr,
            &mut msg.buf[offset..offset + len],
            len,
            gmbus0,
            index,
        );
        if ret != 0 {
            return ret;
        }
        offset += len;
        remaining -= len;
    }
    0
}

unsafe fn gmbus_write_chunk(display: *mut IntelDisplay, addr: u16, buf: &[u8], index: u32) -> i32 {
    let len = buf.len();
    let first = len.min(4);
    let mut value = 0u32;
    for (i, byte) in buf[..first].iter().enumerate() {
        value |= (*byte as u32) << (8 * i);
    }
    intel_de_write_fw(display, GMBUS3(display), value);
    intel_de_write_fw(
        display,
        GMBUS1(display),
        index
            | GMBUS_CYCLE_WAIT
            | ((len as u32) << GMBUS_BYTE_COUNT_SHIFT)
            | ((addr as u32) << GMBUS_SLAVE_ADDR_SHIFT)
            | GMBUS_SLAVE_WRITE
            | GMBUS_SW_RDY,
    );
    let mut offset = first;
    while offset < len {
        let end = (offset + 4).min(len);
        value = 0;
        for (i, byte) in buf[offset..end].iter().enumerate() {
            value |= (*byte as u32) << (8 * i);
        }
        intel_de_write_fw(display, GMBUS3(display), value);
        let ret = gmbus_wait(display, GMBUS_HW_RDY, GMBUS_HW_RDY_EN);
        if ret != 0 {
            return ret;
        }
        offset = end;
    }
    0
}

unsafe fn gmbus_write(display: *mut IntelDisplay, msg: &I2cMsg<'_>, index: u32) -> i32 {
    let mut offset = 0usize;
    while offset < msg.len {
        let len = (msg.len - offset).min(max_xfer_size(display));
        let ret = gmbus_write_chunk(display, msg.addr, &msg.buf[offset..offset + len], index);
        if ret != 0 {
            return ret;
        }
        offset += len;
    }
    if msg.len == 0 {
        gmbus_write_chunk(display, msg.addr, &[], index)
    } else {
        0
    }
}

fn is_index_xfer(msgs: &[I2cMsg<'_>], index: usize) -> bool {
    index + 1 < msgs.len()
        && msgs[index].addr == msgs[index + 1].addr
        && msgs[index].flags & I2C_M_RD == 0
        && (msgs[index].len == 1 || msgs[index].len == 2)
        && msgs[index + 1].len > 0
}
unsafe fn gmbus_index_xfer(
    display: *mut IntelDisplay,
    msgs: &mut [I2cMsg<'_>],
    index: usize,
    gmbus0: u32,
) -> i32 {
    let (first, second) = msgs.split_at_mut(index + 1);
    let first = &first[index];
    let second = &mut second[0];
    let mut gmbus1_index = 0;
    let mut gmbus5 = 0;
    if first.len == 2 {
        gmbus5 = GMBUS_2BYTE_INDEX_EN | first.buf[1] as u32 | ((first.buf[0] as u32) << 8);
    } else if first.len == 1 {
        gmbus1_index = GMBUS_CYCLE_INDEX | ((first.buf[0] as u32) << GMBUS_SLAVE_INDEX_SHIFT);
    }
    if gmbus5 != 0 {
        intel_de_write_fw(display, GMBUS5(display), gmbus5);
    }
    let ret = if second.flags & I2C_M_RD != 0 {
        gmbus_read(display, second, gmbus0, gmbus1_index)
    } else {
        gmbus_write(display, second, gmbus1_index)
    };
    if gmbus5 != 0 {
        intel_de_write_fw(display, GMBUS5(display), 0);
    }
    ret
}

unsafe fn do_gmbus_xfer(bus: *const IntelGmbus, msgs: &mut [I2cMsg<'_>], source: u32) -> i32 {
    let display = (*bus).display;
    let mut i = 0usize;
    let mut retry_first = false;
    loop {
        let mut inc = 1usize;
        intel_de_write_fw(display, GMBUS0(display), source | (*bus).reg0);
        while i < msgs.len() {
            let ret = if is_index_xfer(msgs, i) {
                inc = 2;
                gmbus_index_xfer(display, msgs, i, source | (*bus).reg0)
            } else if msgs[i].flags & I2C_M_RD != 0 {
                gmbus_read(display, &mut msgs[i], source | (*bus).reg0, 0)
            } else {
                gmbus_write(display, &msgs[i], 0)
            };
            let status = if ret == 0 {
                gmbus_wait(display, GMBUS_HW_WAIT_PHASE, GMBUS_HW_WAIT_EN)
            } else {
                ret
            };
            if status == -ETIMEDOUT {
                intel_de_write_fw(display, GMBUS0(display), 0);
                return -EAGAIN;
            }
            if status != 0 {
                let mut error = -ENXIO;
                if gmbus_wait_idle(display) != 0 {
                    error = -ETIMEDOUT;
                }
                intel_de_write_fw(display, GMBUS1(display), GMBUS_SW_CLR_INT);
                intel_de_write_fw(display, GMBUS1(display), 0);
                intel_de_write_fw(display, GMBUS0(display), 0);
                if error == -ENXIO && i == 0 && !retry_first {
                    retry_first = true;
                    break;
                }
                return error;
            }
            i += inc;
            inc = 1;
        }
        if i == msgs.len() {
            intel_de_write_fw(display, GMBUS1(display), GMBUS_CYCLE_STOP | GMBUS_SW_RDY);
            let ret = gmbus_wait_idle(display);
            intel_de_write_fw(display, GMBUS0(display), 0);
            return if ret == 0 { i as i32 } else { -ETIMEDOUT };
        }
    }
}

unsafe fn bitbang_xfer(
    bus: *const IntelGmbus,
    adapter: *const I2cAdapter<'_>,
    msgs: &mut [I2cMsg<'_>],
) -> i32 {
    let bit_algo = I2cBitAlgo {
        data: I2cAlgoBitData {
            lines: &*bus,
            udelay: I2C_RISEFALL_TIME,
            timeout: tk_linux_kapi::primitives::usecs_to_jiffies(2200) as i64,
            can_do_atomic: false,
            bit_test: 0,
        },
    };
    I2cAlgorithm::master_xfer(&bit_algo, &*adapter, msgs)
}

unsafe fn gmbus_xfer(bus: *const IntelGmbus, msgs: &mut [I2cMsg<'_>]) -> i32 {
    if msgs.is_empty() {
        return -EINVAL;
    }
    let display = (*bus).display;
    let mutex = core::ptr::addr_of_mut!((*display).gmbus.mutex);
    tk_linux_kapi::mutex::mutex_lock(mutex);
    let wakeref =
        crate::intel_display_power_upstream::intel_display_power_get(display, POWER_DOMAIN_GMBUS);
    let bitbang = (*bus).force_bit != 0;
    let ret = if bitbang {
        bitbang_xfer(bus, core::ptr::addr_of!((*bus).adapter), msgs)
    } else {
        do_gmbus_xfer(bus, msgs, 0)
    };
    let bus_mut = bus.cast_mut();
    if bitbang && ret < 0 {
        (*bus_mut).force_bit &= !GMBUS_FORCE_BIT_RETRY;
    } else if !bitbang && ret == -EAGAIN {
        (*bus_mut).force_bit |= GMBUS_FORCE_BIT_RETRY;
    }
    crate::intel_display_power_upstream::intel_display_power_put(
        display,
        POWER_DOMAIN_GMBUS,
        wakeref,
    );
    tk_linux_kapi::mutex::mutex_unlock(mutex);
    ret
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gmbus_is_valid_pin(display: *mut IntelDisplay, pin: u32) -> bool {
    pin_for(display, pin).is_some()
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gmbus_get_adapter(
    display: *mut IntelDisplay,
    pin: u32,
) -> *mut c_void {
    if pin_for(display, pin).is_none() {
        return core::ptr::null_mut();
    }
    let bus = (*display).gmbus.bus[pin as usize];
    if bus.is_null() {
        core::ptr::null_mut()
    } else {
        core::ptr::addr_of_mut!((*bus.cast::<IntelGmbus>()).adapter).cast::<c_void>()
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gmbus_force_bit(adapter: *mut c_void, force: bool) {
    if adapter.is_null() {
        return;
    }
    let bus = adapter.cast::<IntelGmbus>();
    let display = (*bus).display;
    let mutex = core::ptr::addr_of_mut!((*display).gmbus.mutex);
    tk_linux_kapi::mutex::mutex_lock(mutex);
    if force {
        (*bus).force_bit = (*bus).force_bit.wrapping_add(1);
    } else {
        (*bus).force_bit = (*bus).force_bit.wrapping_sub(1);
    }
    tk_linux_kapi::mutex::mutex_unlock(mutex);
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gmbus_is_forced_bit(adapter: *mut c_void) -> bool {
    !adapter.is_null() && (*adapter.cast::<IntelGmbus>()).force_bit != 0
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gmbus_setup(display: *mut IntelDisplay) -> i32 {
    if display.is_null() || DISPLAY_VER(display) != 13 || INTEL_PCH_TYPE(display) != PCH_ADP {
        return -tk_linux_kapi::config::EOPNOTSUPP;
    }
    tk_linux_kapi::mutex::mutex_init(core::ptr::addr_of_mut!((*display).gmbus.mutex));
    init_waitqueue_head(&mut (*display).gmbus.wait_queue);
    (*display).gmbus.mmio_base = PCH_DISPLAY_BASE;
    for pin in 0..GMBUS_GPIO_COUNT {
        let Some(pin_info) = pin_for(display, pin as u32) else {
            continue;
        };
        let raw = kzalloc(size_of::<IntelGmbus>(), GFP_KERNEL).cast::<IntelGmbus>();
        if raw.is_null() {
            intel_gmbus_teardown(display);
            return -ENOMEM;
        }
        core::ptr::write(
            raw,
            IntelGmbus {
                adapter: I2cAdapter::new(pin_info.name, None),
                display,
                gpio_reg: GPIO(display, pin_info.gpio),
                reg0: pin as u32 | GMBUS_RATE_100KHZ,
                force_bit: 0,
            },
        );
        (*raw).adapter.algo = Some(&*raw);
        (*raw).adapter.retries = ADAPTER_RETRIES;
        let ret = i2c_add_adapter(&mut (*raw).adapter, &I2C_ADAPTER_IDR);
        if ret != 0 {
            core::ptr::drop_in_place(raw);
            kfree(raw.cast::<c_void>());
            intel_gmbus_teardown(display);
            return ret;
        }
        (*display).gmbus.bus[pin] = raw.cast::<c_void>();
    }
    intel_gmbus_reset(display);
    0
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gmbus_teardown(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    for pin in 0..GMBUS_GPIO_COUNT {
        let bus = (*display).gmbus.bus[pin].cast::<IntelGmbus>();
        if bus.is_null() {
            continue;
        }
        i2c_del_adapter(&mut (*bus).adapter, &I2C_ADAPTER_IDR);
        (*display).gmbus.bus[pin] = core::ptr::null_mut();
        core::ptr::drop_in_place(bus);
        kfree(bus.cast::<c_void>());
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gmbus_irq_handler(display: *mut IntelDisplay) {
    if !display.is_null() {
        wake_up_all(&mut (*display).gmbus.wait_queue);
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gmbus_output_aksv(adapter: *mut c_void) -> i32 {
    if adapter.is_null() {
        return -EINVAL;
    }
    let bus = adapter.cast::<IntelGmbus>();
    let display = (*bus).display;
    let mut cmd = 0x10u8; // DRM_HDCP_DDC_AKSV
    let mut data = [0u8; 5];
    let mut msgs = [
        I2cMsg::new(0x74, 0, core::slice::from_mut(&mut cmd)),
        I2cMsg::new(0x74, 0, &mut data),
    ];
    let mutex = core::ptr::addr_of_mut!((*display).gmbus.mutex);
    tk_linux_kapi::mutex::mutex_lock(mutex);
    let wakeref =
        crate::intel_display_power_upstream::intel_display_power_get(display, POWER_DOMAIN_GMBUS);
    let ret = do_gmbus_xfer(bus, &mut msgs, GMBUS_AKSV_SELECT);
    crate::intel_display_power_upstream::intel_display_power_put(
        display,
        POWER_DOMAIN_GMBUS,
        wakeref,
    );
    tk_linux_kapi::mutex::mutex_unlock(mutex);
    ret
}
