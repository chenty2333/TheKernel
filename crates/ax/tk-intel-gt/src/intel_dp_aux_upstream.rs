// SPDX-License-Identifier: MIT
// Copyright 2020-2021 Intel Corporation.
// Source: drivers/gpu/drm/i915/display/intel_dp_aux.c (Linux v7.2.3)
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::{c_char, c_void},
    mem::offset_of,
    ptr,
};

use tk_drm_upstream::{
    drm_dp_helper_types::{DrmDpAux, DrmDpAuxMsg},
    drm_dp_helper_upstream::{
        DP_AUX_I2C_MOT, DP_AUX_I2C_READ, DP_AUX_I2C_WRITE, DP_AUX_I2C_WRITE_STATUS_UPDATE,
        DP_AUX_NATIVE_READ, DP_AUX_NATIVE_WRITE, drm_dp_aux_init, drm_dp_dpcd_set_probe,
    },
    drm_encoder_types::DrmEncoder,
};
use tk_linux_kapi::{
    memory::{kfree, kmalloc_array},
    primitives::{msecs_to_jiffies_timeout, udelay},
    wait::{msleep, wake_up_all},
};

use crate::{
    intel_de_upstream::{intel_de_read, intel_de_read_notrace, intel_de_wait_ms, intel_de_write},
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::{
        AUX_CH_A, AUX_CH_B, AUX_CH_C, AUX_CH_D, AUX_CH_NONE, AuxCh, PORT_E,
    },
    intel_display_power_upstream::{__intel_display_power_put_async, intel_display_power_get},
    intel_display_types_upstream::{INTEL_OUTPUT_DDI, IntelDigitalPort, IntelDp, IntelEncoder},
    intel_dp_aux_regs_upstream::*,
    intel_parent_upstream::intel_parent_irq_enabled,
    intel_quirks_upstream::IntelQuirkId,
    intel_workarounds_types_upstream::I915RegT,
    linux_config::{E2BIG, EBUSY, EINVAL, EIO, ENODEV, ENXIO, ETIMEDOUT, GFP_KERNEL},
};

const AUX_CH_NAME_BUFSIZE: usize = 6;
const BARE_ADDRESS_SIZE: usize = 3;
const HEADER_SIZE: usize = 4;
const AUX_MAX_DATA: usize = 20;
const AUX_TIMEOUT_MS: u32 = 10;
const DP_AUX_HDCP_AKSV: u32 = 0x68007;
const AUX_CTL_SEND_FLAGS: u32 = DP_AUX_CH_CTL_DONE
    | DP_AUX_CH_CTL_SEND_BUSY
    | DP_AUX_CH_CTL_INTERRUPT
    | DP_AUX_CH_CTL_TIME_OUT_ERROR
    | DP_AUX_CH_CTL_RECEIVE_ERROR;

unsafe extern "C" {
    fn intel_aux_power_domain(dig_port: *mut IntelDigitalPort) -> i32;
    fn intel_digital_port_lock(encoder: *mut IntelEncoder);
    fn intel_digital_port_unlock(encoder: *mut IntelEncoder);
    fn intel_digital_port_connected_locked(encoder: *mut IntelEncoder) -> bool;
    fn intel_dp_is_edp(intel_dp: *mut IntelDp) -> bool;
    fn intel_tc_port_in_tbt_alt_mode(dig_port: *mut IntelDigitalPort) -> bool;
    fn intel_pps_lock(intel_dp: *mut IntelDp) -> *mut c_void;
    fn intel_pps_unlock(intel_dp: *mut IntelDp, wakeref: *mut c_void) -> *mut c_void;
    fn intel_pps_vdd_on_unlocked(intel_dp: *mut IntelDp) -> bool;
    fn intel_pps_vdd_off_unlocked(intel_dp: *mut IntelDp, sync: bool);
    fn intel_pps_check_power_unlocked(intel_dp: *mut IntelDp);
}

#[inline]
unsafe fn dp_to_dig_port(dp: *mut IntelDp) -> *mut IntelDigitalPort {
    if dp.is_null() {
        return ptr::null_mut();
    }
    dp.cast::<u8>()
        .wrapping_sub(offset_of!(IntelDigitalPort, dp))
        .cast()
}

#[inline]
unsafe fn dp_to_display(dp: *mut IntelDp) -> *mut IntelDisplay {
    let dig = unsafe { dp_to_dig_port(dp) };
    if dig.is_null() || unsafe { (*dig).base.base.dev.is_null() } {
        return ptr::null_mut();
    }
    unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*dig).base.base.dev) }
}

#[inline]
unsafe fn display_ver(display: *mut IntelDisplay) -> u16 {
    if display.is_null() {
        0
    } else {
        unsafe { (*display).info.__runtime_info.ip.ver }
    }
}

#[inline]
unsafe fn aux_channel(dp: *mut IntelDp) -> AuxCh {
    let dig = unsafe { dp_to_dig_port(dp) };
    if dig.is_null() {
        -1
    } else {
        unsafe { (*dig).aux_ch }
    }
}

fn aux_name_channel(channel: AuxCh, ver: u16, out: &mut [u8; AUX_CH_NAME_BUFSIZE]) -> usize {
    let label: &[u8] = if ver >= 13
        && channel >= crate::intel_display_limits_types_upstream::AUX_CH_D_XELPD
    {
        let ch =
            b'A' + (channel - crate::intel_display_limits_types_upstream::AUX_CH_D_XELPD) as u8;
        out[0] = ch;
        return 1;
    } else if ver >= 12 && channel >= crate::intel_display_limits_types_upstream::AUX_CH_USBC1 {
        out[..4].copy_from_slice(b"USBC");
        out[4] = b'1' + (channel - crate::intel_display_limits_types_upstream::AUX_CH_USBC1) as u8;
        return 5;
    } else if (0..=8).contains(&channel) {
        out[0] = b'A' + channel as u8;
        return 1;
    } else {
        b"AUX"
    };
    out[..label.len()].copy_from_slice(label);
    label.len()
}

/// Linux `intel_dp_aux_pack()`: pack up to four AUX bytes in wire order.
pub unsafe extern "C" fn intel_dp_aux_pack(src: *const u8, src_bytes: i32) -> u32 {
    if src.is_null() || src_bytes <= 0 {
        return 0;
    }
    let n = (src_bytes as usize).min(4);
    let mut value = 0u32;
    for i in 0..n {
        value |= unsafe { *src.add(i) as u32 } << ((3 - i) * 8);
    }
    value
}

#[inline]
unsafe fn aux_unpack(src: u32, dst: *mut u8, dst_bytes: usize) {
    if dst.is_null() {
        return;
    }
    for i in 0..dst_bytes.min(4) {
        unsafe { *dst.add(i) = (src >> ((3 - i) * 8)) as u8 };
    }
}

#[inline]
unsafe fn aux_ctl_reg(dp: *mut IntelDp) -> I915RegT {
    if dp.is_null() {
        return I915RegT::default();
    }
    let display = unsafe { dp_to_display(dp) };
    let channel = unsafe { aux_channel(dp) };
    let ver = unsafe { display_ver(display) };
    if ver >= 14 {
        unsafe { XELPDP_DP_AUX_CH_CTL(display, channel.max(AUX_CH_A) as u32) }
    } else if (AUX_CH_A..=AUX_CH_C).contains(&channel)
        || (crate::intel_display_limits_types_upstream::AUX_CH_USBC1
            ..=crate::intel_display_limits_types_upstream::AUX_CH_USBC6)
            .contains(&channel)
        || (ver >= 13 && channel == AUX_CH_D)
    {
        DP_AUX_CH_CTL(channel as u32)
    } else {
        DP_AUX_CH_CTL(AUX_CH_A as u32)
    }
}

#[inline]
unsafe fn aux_data_reg(dp: *mut IntelDp, index: i32) -> I915RegT {
    if dp.is_null() || !(0..5).contains(&index) {
        return I915RegT::default();
    }
    let display = unsafe { dp_to_display(dp) };
    let channel = unsafe { aux_channel(dp) };
    let ver = unsafe { display_ver(display) };
    if ver >= 14 {
        unsafe { XELPDP_DP_AUX_CH_DATA(display, channel.max(AUX_CH_A) as u32, index as u32) }
    } else if (AUX_CH_A..=AUX_CH_C).contains(&channel)
        || (crate::intel_display_limits_types_upstream::AUX_CH_USBC1
            ..=crate::intel_display_limits_types_upstream::AUX_CH_USBC6)
            .contains(&channel)
        || (ver >= 13 && channel == AUX_CH_D)
    {
        DP_AUX_CH_DATA(channel as u32, index as u32)
    } else {
        DP_AUX_CH_DATA(AUX_CH_A as u32, index as u32)
    }
}

unsafe extern "C" fn dp_aux_ctl_callback(dp: *mut IntelDp) -> I915RegT {
    unsafe { aux_ctl_reg(dp) }
}

unsafe extern "C" fn dp_aux_data_callback(dp: *mut IntelDp, index: i32) -> I915RegT {
    unsafe { aux_data_reg(dp, index) }
}

unsafe extern "C" fn dp_aux_clock_callback(dp: *mut IntelDp, index: i32) -> u32 {
    if !dp.is_null() && index == 0 { 1 } else { 0 }
}

unsafe fn aux_sync_len() -> u32 {
    // 16 precharge + 16 preamble, per the source default.
    32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_aux_fw_sync_len(dp: *mut IntelDp) -> i32 {
    if dp.is_null() {
        return 18;
    }
    let quirk = unsafe {
        crate::intel_quirks_upstream::intel_has_dpcd_quirk(dp, IntelQuirkId::QuirkFwSyncLen)
    };
    if quirk { 20 } else { 18 }
}

unsafe extern "C" fn dp_aux_send_ctl_callback(
    dp: *mut IntelDp,
    send_bytes: i32,
    _clock: u32,
) -> u32 {
    let mut value = AUX_CTL_SEND_FLAGS
        | DP_AUX_CH_CTL_TIME_OUT_MAX
        | DP_AUX_CH_CTL_MESSAGE_SIZE(send_bytes.clamp(0, 20) as u32)
        | DP_AUX_CH_CTL_FW_SYNC_PULSE_SKL(unsafe { intel_dp_aux_fw_sync_len(dp) } as u32)
        | DP_AUX_CH_CTL_SYNC_PULSE_SKL(unsafe { aux_sync_len() });
    let dig = unsafe { dp_to_dig_port(dp) };
    let display = unsafe { dp_to_display(dp) };
    if !dig.is_null() && unsafe { intel_tc_port_in_tbt_alt_mode(dig) } {
        value |= DP_AUX_CH_CTL_TBT_IO;
    }
    if unsafe { display_ver(display) } >= 14 {
        value |= XELPDP_DP_AUX_CH_CTL_POWER_REQUEST;
    }
    value
}

unsafe fn intel_dp_aux_wait_done(dp: *mut IntelDp, ctl: I915RegT) -> u32 {
    let display = unsafe { dp_to_display(dp) };
    if display.is_null() {
        return 0;
    }
    let mut status = unsafe { intel_de_read_notrace(display, ctl) };
    if unsafe { intel_parent_irq_enabled(display) } {
        let mut wait_queue = unsafe { ptr::addr_of_mut!((*display).gmbus.wait_queue) };
        let _remaining = tk_linux_kapi::wait_event_timeout!(
            wait_queue,
            {
                status = unsafe { intel_de_read_notrace(display, ctl) };
                status & DP_AUX_CH_CTL_SEND_BUSY == 0
            },
            msecs_to_jiffies_timeout(AUX_TIMEOUT_MS),
        );
    } else {
        let ret = unsafe {
            intel_de_wait_ms(
                display,
                ctl,
                DP_AUX_CH_CTL_SEND_BUSY,
                0,
                AUX_TIMEOUT_MS,
                ptr::addr_of_mut!(status),
            )
        };
        if ret == -ETIMEDOUT {
            status = unsafe { intel_de_read_notrace(display, ctl) };
        }
    }
    status
}

unsafe fn aux_hw_xfer(
    dp: *mut IntelDp,
    send: *const u8,
    send_bytes: usize,
    recv: *mut u8,
    recv_size: usize,
    flags: u32,
) -> i32 {
    if dp.is_null() || send_bytes > AUX_MAX_DATA || recv_size > AUX_MAX_DATA {
        return -E2BIG;
    }
    if send.is_null() && send_bytes != 0 || recv.is_null() && recv_size != 0 {
        return -EINVAL;
    }
    let display = unsafe { dp_to_display(dp) };
    let ctl_callback = unsafe { (*dp).aux_ch_ctl_reg };
    let data_callback = unsafe { (*dp).aux_ch_data_reg };
    let clock_callback = unsafe { (*dp).get_aux_clock_divider };
    let send_ctl_callback = unsafe { (*dp).get_aux_send_ctl };
    let (Some(ctl_callback), Some(data_callback), Some(clock_callback), Some(send_ctl_callback)) = (
        ctl_callback,
        data_callback,
        clock_callback,
        send_ctl_callback,
    ) else {
        return -ENODEV;
    };
    let ctl = unsafe { ctl_callback(dp) };
    let data = [
        unsafe { data_callback(dp, 0) },
        unsafe { data_callback(dp, 1) },
        unsafe { data_callback(dp, 2) },
        unsafe { data_callback(dp, 3) },
        unsafe { data_callback(dp, 4) },
    ];
    let dig = unsafe { dp_to_dig_port(dp) };
    if display.is_null() || dig.is_null() {
        return -ENODEV;
    }
    let encoder = ptr::addr_of_mut!((*dig).base);
    unsafe { intel_digital_port_lock(encoder) };
    if !unsafe { intel_dp_is_edp(dp) } && !unsafe { intel_digital_port_connected_locked(encoder) } {
        unsafe { intel_digital_port_unlock(encoder) };
        return -ENXIO;
    }

    let aux_domain = unsafe { intel_aux_power_domain(dig) } as u32;
    let aux_wakeref = unsafe { intel_display_power_get(display, aux_domain) };
    let use_pps_lock = unsafe { intel_dp_is_edp(dp) }
        || unsafe { (*display).platform._anon_0_0.bitmap[0] & ((1u64 << 20) | (1u64 << 27)) != 0 };
    let pps_wakeref = if use_pps_lock {
        unsafe { intel_pps_lock(dp) }
    } else {
        ptr::null_mut()
    };
    let vdd = unsafe { intel_pps_vdd_on_unlocked(dp) };
    unsafe { intel_pps_check_power_unlocked(dp) };

    let mut ret = -EBUSY;
    let mut status = 0u32;
    for _ in 0..3 {
        status = unsafe { intel_de_read_notrace(display, ctl) };
        if status & DP_AUX_CH_CTL_SEND_BUSY == 0 {
            break;
        }
        msleep(1);
    }
    if status & DP_AUX_CH_CTL_SEND_BUSY != 0 {
        ret = -EBUSY;
    } else {
        'clock: for clock_index in 0..16 {
            let clock = unsafe { clock_callback(dp, clock_index) };
            if clock == 0 {
                break;
            }
            let send_ctl = unsafe { send_ctl_callback(dp, send_bytes as i32, clock) } | flags;
            for _try in 0..5 {
                let mut i = 0;
                while i < send_bytes {
                    let n = (send_bytes - i).min(4);
                    let packed = unsafe { intel_dp_aux_pack(send.add(i), n as i32) };
                    unsafe { intel_de_write(display, data[i / 4], packed) };
                    i += 4;
                }
                unsafe { intel_de_write(display, ctl, send_ctl) };
                status = unsafe { intel_dp_aux_wait_done(dp, ctl) };
                unsafe {
                    intel_de_write(
                        display,
                        ctl,
                        status
                            | DP_AUX_CH_CTL_DONE
                            | DP_AUX_CH_CTL_TIME_OUT_ERROR
                            | DP_AUX_CH_CTL_RECEIVE_ERROR,
                    )
                };
                if status & DP_AUX_CH_CTL_TIME_OUT_ERROR != 0 {
                    continue;
                }
                if status & DP_AUX_CH_CTL_RECEIVE_ERROR != 0 {
                    tk_linux_kapi::iopoll::usleep_range(400, 500);
                    continue;
                }
                if status & DP_AUX_CH_CTL_DONE != 0 {
                    break 'clock;
                }
            }
        }

        if status & DP_AUX_CH_CTL_DONE == 0 {
            ret = -EBUSY;
        } else if status & DP_AUX_CH_CTL_RECEIVE_ERROR != 0 {
            ret = -EIO;
        } else if status & DP_AUX_CH_CTL_TIME_OUT_ERROR != 0 {
            ret = -ETIMEDOUT;
        } else {
            let received = ((status & DP_AUX_CH_CTL_MESSAGE_SIZE_MASK) >> 20) as usize;
            if received == 0 || received > AUX_MAX_DATA {
                ret = -EBUSY;
            } else {
                let count = received.min(recv_size);
                let mut i = 0;
                while i < count {
                    let n = (count - i).min(4);
                    let value = unsafe { intel_de_read(display, data[i / 4]) };
                    unsafe { aux_unpack(value, recv.add(i), n) };
                    i += 4;
                }
                ret = count as i32;
            }
        }
    }

    if vdd {
        unsafe { intel_pps_vdd_off_unlocked(dp, false) };
    }
    if !pps_wakeref.is_null() {
        unsafe { intel_pps_unlock(dp, pps_wakeref) };
    }
    unsafe { __intel_display_power_put_async(display, aux_domain, aux_wakeref, -1) };
    unsafe { intel_digital_port_unlock(encoder) };
    ret
}

fn aux_xfer_flags(msg: &DrmDpAuxMsg) -> u32 {
    if (msg.request & !DP_AUX_I2C_MOT) == DP_AUX_NATIVE_WRITE && msg.address == DP_AUX_HDCP_AKSV {
        DP_AUX_CH_CTL_AUX_AKSV_SELECT
    } else {
        0
    }
}

unsafe extern "C" fn intel_dp_aux_transfer(aux: *mut DrmDpAux, msg: *mut DrmDpAuxMsg) -> isize {
    if aux.is_null() || msg.is_null() {
        return -EINVAL as isize;
    }
    let dp = aux
        .cast::<u8>()
        .wrapping_sub(offset_of!(IntelDp, aux))
        .cast::<IntelDp>();
    let msg_ref = unsafe { &mut *msg };
    if msg_ref.size > AUX_MAX_DATA {
        return -E2BIG as isize;
    }
    if (msg_ref.size != 0) != !msg_ref.buffer.is_null() {
        return -EINVAL as isize;
    }
    let flags = aux_xfer_flags(msg_ref);
    let mut tx = [0u8; AUX_MAX_DATA];
    let mut rx = [0u8; AUX_MAX_DATA];
    tx[0] = (msg_ref.request << 4) | ((msg_ref.address >> 16) as u8 & 0x0f);
    tx[1] = (msg_ref.address >> 8) as u8;
    tx[2] = msg_ref.address as u8;
    tx[3] = msg_ref.size.saturating_sub(1) as u8;

    let request = msg_ref.request & !DP_AUX_I2C_MOT;
    let (tx_size, rx_size) = match request {
        DP_AUX_NATIVE_WRITE | DP_AUX_I2C_WRITE | DP_AUX_I2C_WRITE_STATUS_UPDATE => {
            let tx_size = if msg_ref.size != 0 {
                HEADER_SIZE + msg_ref.size
            } else {
                BARE_ADDRESS_SIZE
            };
            if tx_size > AUX_MAX_DATA {
                return -E2BIG as isize;
            }
            if !msg_ref.buffer.is_null() {
                unsafe {
                    ptr::copy_nonoverlapping(
                        msg_ref.buffer.cast::<u8>(),
                        tx.as_mut_ptr().add(HEADER_SIZE),
                        msg_ref.size,
                    )
                };
            }
            (tx_size, 2)
        }
        DP_AUX_NATIVE_READ | DP_AUX_I2C_READ => {
            let rx_size = msg_ref.size + 1;
            if rx_size > AUX_MAX_DATA {
                return -E2BIG as isize;
            }
            (
                if msg_ref.size != 0 {
                    HEADER_SIZE
                } else {
                    BARE_ADDRESS_SIZE
                },
                rx_size,
            )
        }
        _ => return -EINVAL as isize,
    };

    let ret = unsafe { aux_hw_xfer(dp, tx.as_ptr(), tx_size, rx.as_mut_ptr(), rx_size, flags) };
    if ret <= 0 {
        return ret as isize;
    }
    msg_ref.reply = rx[0] >> 4;
    if matches!(
        request,
        DP_AUX_NATIVE_WRITE | DP_AUX_I2C_WRITE | DP_AUX_I2C_WRITE_STATUS_UPDATE
    ) {
        if ret > 1 {
            (rx[1] as usize).min(msg_ref.size) as isize
        } else {
            msg_ref.size as isize
        }
    } else {
        let payload_size = (ret as usize).saturating_sub(1).min(msg_ref.size);
        if payload_size != 0 && msg_ref.buffer.is_null() {
            return -EINVAL as isize;
        }
        if payload_size != 0 {
            unsafe {
                ptr::copy_nonoverlapping(
                    rx.as_ptr().add(1),
                    msg_ref.buffer.cast::<u8>(),
                    payload_size,
                )
            };
        }
        payload_size as isize
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_aux_init(dp: *mut IntelDp) {
    if dp.is_null() {
        return;
    }
    let display = unsafe { dp_to_display(dp) };
    if display.is_null() {
        return;
    }
    let ver = unsafe { display_ver(display) };
    let dig = unsafe { dp_to_dig_port(dp) };
    if dig.is_null() {
        return;
    }
    unsafe {
        (*dp).aux_ch_ctl_reg = Some(dp_aux_ctl_callback);
        (*dp).aux_ch_data_reg = Some(dp_aux_data_callback);
        (*dp).get_aux_clock_divider = Some(dp_aux_clock_callback);
        (*dp).get_aux_send_ctl = Some(dp_aux_send_ctl_callback);
        (*dp).aux.drm_dev = (*display).drm;
        drm_dp_aux_init(ptr::addr_of_mut!((*dp).aux));
    }
    let mut channel = [0u8; AUX_CH_NAME_BUFSIZE];
    let channel_len = aux_name_channel(unsafe { (*dig).aux_ch }, ver, &mut channel);
    let encoder_name = unsafe { (*dig).base.base.name };
    let encoder_bytes = if encoder_name.is_null() {
        b"port".as_slice()
    } else {
        unsafe { core::ffi::CStr::from_ptr(encoder_name) }.to_bytes()
    };
    let name_len = 4 + channel_len + 1 + encoder_bytes.len() + 1;
    let name = unsafe { kmalloc_array(name_len, 1, GFP_KERNEL) }.cast::<u8>();
    if !name.is_null() {
        unsafe {
            name.write_bytes(0, name_len);
            ptr::copy_nonoverlapping(b"AUX ".as_ptr(), name, 4);
            ptr::copy_nonoverlapping(channel.as_ptr(), name.add(4), channel_len);
            name.add(4 + channel_len).write(b'/');
            ptr::copy_nonoverlapping(
                encoder_bytes.as_ptr(),
                name.add(5 + channel_len),
                encoder_bytes.len(),
            );
            (*dp).aux.name = name.cast::<c_char>();
        }
    }
    unsafe {
        (*dp).aux.transfer = Some(intel_dp_aux_transfer);
        drm_dp_dpcd_set_probe(ptr::addr_of_mut!((*dp).aux), true);
    }
    // The Linux CPU-latency QoS request is not modeled by the kernel KAPI.
    // The AUX path still holds the port and AUX power-domain references for
    // the full transaction, which are the required hardware liveness guards.
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_aux_fini(dp: *mut IntelDp) {
    if dp.is_null() {
        return;
    }
    unsafe {
        kfree((*dp).aux.name.cast::<c_void>());
        (*dp).aux.name = ptr::null_mut();
        (*dp).aux.transfer = None;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_aux_ch(encoder: *mut IntelEncoder) -> AuxCh {
    if encoder.is_null() {
        return AUX_CH_NONE;
    }
    let drm = unsafe { (*encoder).base.dev };
    if drm.is_null() {
        return AUX_CH_NONE;
    }
    let display = unsafe { crate::intel_display_conversion_upstream::__drm_to_display(drm) };
    let mut channel =
        unsafe { crate::intel_bios_upstream::intel_bios_dp_aux_ch((*encoder).devdata.cast()) };
    if channel == AUX_CH_NONE {
        channel = if unsafe { display_ver(display) } == 9 && unsafe { (*encoder).port } == PORT_E {
            AUX_CH_A
        } else {
            unsafe { (*encoder).port }
        };
    }
    if channel == AUX_CH_NONE || display.is_null() {
        return AUX_CH_NONE;
    }

    let head = unsafe { ptr::addr_of_mut!((*drm).mode_config.encoder_list) };
    let mut node = unsafe { (*head).next };
    while !node.is_null() && node != head {
        let drm_encoder = unsafe {
            node.cast::<u8>()
                .wrapping_sub(offset_of!(DrmEncoder, head))
                .cast::<DrmEncoder>()
        };
        let other = drm_encoder.cast::<IntelEncoder>();
        if other != encoder && unsafe { (*other).r#type } == INTEL_OUTPUT_DDI {
            let other_port = other
                .cast::<u8>()
                .wrapping_sub(offset_of!(IntelDigitalPort, base))
                .cast::<IntelDigitalPort>();
            if unsafe { (*other_port).aux_ch } == channel {
                return AUX_CH_NONE;
            }
        }
        node = unsafe { (*node).next };
    }
    channel
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dp_aux_irq_handler(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    unsafe { wake_up_all(&mut (*display).gmbus.wait_queue) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aux_pack_and_unpack_follow_wire_byte_order() {
        let data = [0x12, 0x34, 0x56, 0x78, 0x9a];
        assert_eq!(unsafe { intel_dp_aux_pack(data.as_ptr(), 4) }, 0x1234_5678);
        assert_eq!(unsafe { intel_dp_aux_pack(data.as_ptr(), 2) }, 0x1234_0000);
        assert_eq!(unsafe { intel_dp_aux_pack(data.as_ptr(), 0) }, 0);
        let mut out = [0u8; 4];
        unsafe { aux_unpack(0x1234_5678, out.as_mut_ptr(), out.len()) };
        assert_eq!(out, [0x12, 0x34, 0x56, 0x78]);
    }

    #[test]
    fn aux_channel_names_match_generation_specific_naming() {
        let mut buf = [0; AUX_CH_NAME_BUFSIZE];
        let len = aux_name_channel(
            crate::intel_display_limits_types_upstream::AUX_CH_USBC2,
            12,
            &mut buf,
        );
        assert_eq!(&buf[..len], b"USBC2");
        let len = aux_name_channel(
            crate::intel_display_limits_types_upstream::AUX_CH_D_XELPD,
            13,
            &mut buf,
        );
        assert_eq!(&buf[..len], b"D");
    }

    #[test]
    fn aux_send_control_encodes_payload_and_adlp_sync_window() {
        let size = 20;
        let control =
            AUX_CTL_SEND_FLAGS | DP_AUX_CH_CTL_TIME_OUT_MAX | DP_AUX_CH_CTL_MESSAGE_SIZE(size);
        assert_eq!((control & DP_AUX_CH_CTL_MESSAGE_SIZE_MASK) >> 20, 20);
        assert_eq!(DP_AUX_CH_CTL_FW_SYNC_PULSE_SKL(18), 17 << 5);
        assert_eq!(DP_AUX_CH_CTL_SYNC_PULSE_SKL(32), 31);
    }
}
