// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_ct.c and ABI headers
// guc_communication_ctb_abi.h, guc_communication_mmio_abi.h, guc_messages_abi.h.
// Copyright © 2016-2019 Intel Corporation.
// Copyright © 2014-2021 Intel Corporation. Full MIT grant: ../LICENSE-MIT.

use alloc::vec::Vec;
use core::sync::atomic::{Ordering, fence as atomic_fence};

use crate::{Error, GtIo};

pub const CTB_DESC_SIZE: usize = 2048;
pub const CTB_SEND_DESC_OFFSET: usize = 0;
pub const CTB_RECV_DESC_OFFSET: usize = CTB_DESC_SIZE;
pub const CTB_SEND_BUFFER_OFFSET: usize = 2 * CTB_DESC_SIZE;
pub const CTB_H2G_BUFFER_SIZE: usize = 4096;
pub const CTB_RECV_BUFFER_OFFSET: usize = CTB_SEND_BUFFER_OFFSET + CTB_H2G_BUFFER_SIZE;
pub const CTB_G2H_BUFFER_SIZE: usize = 4 * CTB_H2G_BUFFER_SIZE;
pub const G2H_ROOM_BUFFER_SIZE: usize = CTB_G2H_BUFFER_SIZE / 4;
pub const CTB_BLOB_SIZE: usize = 2 * CTB_DESC_SIZE + CTB_H2G_BUFFER_SIZE + CTB_G2H_BUFFER_SIZE;
pub const CTB_HDR_LEN: usize = 1;
pub const CTB_MSG_MIN_LEN: usize = CTB_HDR_LEN;
pub const CTB_MSG_MAX_LEN: usize = 256;
pub const CTB_MSG_0_FENCE_MASK: u32 = 0xffff << 16;
pub const CTB_MSG_0_FORMAT_MASK: u32 = 0xf << 12;
pub const CTB_MSG_0_RESERVED_MASK: u32 = 0xf << 8;
pub const CTB_MSG_0_NUM_DWORDS_MASK: u32 = 0xff;
pub const GUC_CTB_MSG_0_FENCE: u32 = CTB_MSG_0_FENCE_MASK;
pub const GUC_CTB_MSG_0_FORMAT: u32 = CTB_MSG_0_FORMAT_MASK;
pub const GUC_CTB_MSG_0_RESERVED: u32 = CTB_MSG_0_RESERVED_MASK;
pub const GUC_CTB_MSG_0_NUM_DWORDS: u32 = CTB_MSG_0_NUM_DWORDS_MASK;
pub const CTB_FORMAT_HXG: u32 = 0;
pub const CTB_STATUS_NO_ERROR: u32 = 0;
pub const CTB_STATUS_OVERFLOW: u32 = 1 << 0;
pub const CTB_STATUS_UNDERFLOW: u32 = 1 << 1;
pub const CTB_STATUS_MISMATCH: u32 = 1 << 2;
pub const CTB_STATUS_UNUSED: u32 = 1 << 3;
pub const GUC_CTB_STATUS_NO_ERROR: u32 = CTB_STATUS_NO_ERROR;
pub const GUC_CTB_STATUS_OVERFLOW: u32 = CTB_STATUS_OVERFLOW;
pub const GUC_CTB_STATUS_UNDERFLOW: u32 = CTB_STATUS_UNDERFLOW;
pub const GUC_CTB_STATUS_MISMATCH: u32 = CTB_STATUS_MISMATCH;
pub const GUC_CTB_STATUS_UNUSED: u32 = CTB_STATUS_UNUSED;
pub const GUC_CTB_HDR_LEN: usize = CTB_HDR_LEN;
pub const GUC_CTB_MSG_MIN_LEN: usize = CTB_MSG_MIN_LEN;
pub const GUC_CTB_MSG_MAX_LEN: usize = CTB_MSG_MAX_LEN;
pub const GUC_CTB_FORMAT_HXG: u32 = CTB_FORMAT_HXG;

pub const HXG_ORIGIN_GUC: u32 = 1 << 31;
pub const HXG_ORIGIN_HOST: u32 = 0;
pub const GUC_HXG_MSG_0_ORIGIN: u32 = 1 << 31;
pub const HXG_MSG_MIN_LEN: usize = 1;
pub const HXG_AUX_MASK: u32 = 0x0fff_ffff;
pub const HXG_PAYLOAD_MASK: u32 = u32::MAX;
pub const GUC_HXG_MSG_MIN_LEN: usize = HXG_MSG_MIN_LEN;
pub const GUC_HXG_MSG_0_TYPE: u32 = HXG_TYPE_MASK;
pub const GUC_HXG_MSG_0_AUX: u32 = HXG_AUX_MASK;
pub const GUC_HXG_MSG_N_PAYLOAD: u32 = HXG_PAYLOAD_MASK;
pub const HXG_TYPE_MASK: u32 = 7 << 28;
pub const HXG_TYPE_REQUEST: u32 = 0 << 28;
pub const HXG_TYPE_EVENT: u32 = 1 << 28;
pub const HXG_TYPE_FAST_REQUEST: u32 = 2 << 28;
pub const HXG_TYPE_NO_RESPONSE_BUSY: u32 = 3 << 28;
pub const HXG_TYPE_NO_RESPONSE_RETRY: u32 = 5 << 28;
pub const HXG_TYPE_RESPONSE_FAILURE: u32 = 6 << 28;
pub const HXG_TYPE_RESPONSE_SUCCESS: u32 = 7 << 28;
pub const HXG_ACTION_MASK: u32 = 0x0000_ffff;
pub const HXG_DATA0_MASK: u32 = 0x0fff_0000;
pub const HXG_REQUEST_ACTION_MASK: u32 = HXG_ACTION_MASK;
pub const HXG_REQUEST_DATA0_MASK: u32 = HXG_DATA0_MASK;
pub const GUC_HXG_REQUEST_MSG_MIN_LEN: usize = HXG_MSG_MIN_LEN;
pub const GUC_HXG_REQUEST_MSG_0_ACTION: u32 = HXG_REQUEST_ACTION_MASK;
pub const GUC_HXG_REQUEST_MSG_0_DATA0: u32 = HXG_REQUEST_DATA0_MASK;
pub const GUC_HXG_REQUEST_MSG_N_DATAN: u32 = HXG_PAYLOAD_MASK;
pub const HXG_EVENT_ACTION_MASK: u32 = HXG_ACTION_MASK;
pub const HXG_EVENT_DATA0_MASK: u32 = HXG_DATA0_MASK;
pub const GUC_HXG_EVENT_MSG_MIN_LEN: usize = HXG_MSG_MIN_LEN;
pub const GUC_HXG_EVENT_MSG_0_ACTION: u32 = HXG_EVENT_ACTION_MASK;
pub const GUC_HXG_EVENT_MSG_0_DATA0: u32 = HXG_EVENT_DATA0_MASK;
pub const GUC_HXG_EVENT_MSG_N_DATAN: u32 = HXG_PAYLOAD_MASK;
pub const HXG_RETRY_REASON_MASK: u32 = HXG_AUX_MASK;
pub const HXG_BUSY_COUNTER_MASK: u32 = HXG_AUX_MASK;
pub const GUC_HXG_BUSY_MSG_LEN: usize = HXG_MSG_MIN_LEN;
pub const GUC_HXG_BUSY_MSG_0_COUNTER: u32 = HXG_BUSY_COUNTER_MASK;
pub const GUC_HXG_RETRY_MSG_LEN: usize = HXG_MSG_MIN_LEN;
pub const GUC_HXG_RETRY_MSG_0_REASON: u32 = HXG_RETRY_REASON_MASK;
pub const GUC_HXG_RETRY_REASON_UNSPECIFIED: u32 = 0;
pub const HXG_FAILURE_ERROR_MASK: u32 = 0xffff;
pub const HXG_FAILURE_HINT_MASK: u32 = 0x0fff_0000;
pub const GUC_HXG_FAILURE_MSG_LEN: usize = HXG_MSG_MIN_LEN;
pub const GUC_HXG_FAILURE_MSG_0_HINT: u32 = HXG_FAILURE_HINT_MASK;
pub const GUC_HXG_FAILURE_MSG_0_ERROR: u32 = HXG_FAILURE_ERROR_MASK;
pub const GUC_HXG_RESPONSE_MSG_MIN_LEN: usize = HXG_MSG_MIN_LEN;
pub const GUC_HXG_RESPONSE_MSG_0_DATA0: u32 = HXG_AUX_MASK;
pub const GUC_HXG_RESPONSE_MSG_N_DATAN: u32 = HXG_PAYLOAD_MASK;
pub const INTEL_GUC_MSG_TYPE_SHIFT: u32 = 28;
pub const INTEL_GUC_MSG_TYPE_MASK: u32 = 0xf << INTEL_GUC_MSG_TYPE_SHIFT;
pub const INTEL_GUC_MSG_DATA_SHIFT: u32 = 16;
pub const INTEL_GUC_MSG_DATA_MASK: u32 = 0xfff << INTEL_GUC_MSG_DATA_SHIFT;
pub const INTEL_GUC_MSG_CODE_SHIFT: u32 = 0;
pub const INTEL_GUC_MSG_CODE_MASK: u32 = 0xffff << INTEL_GUC_MSG_CODE_SHIFT;
pub const INTEL_GUC_MSG_TYPE_REQUEST: u32 = 0;
pub const INTEL_GUC_MSG_TYPE_RESPONSE: u32 = 0xf;
pub const CTB_HXG_MSG_MIN_LEN: usize = CTB_MSG_MIN_LEN + HXG_MSG_MIN_LEN;
pub const CTB_HXG_MSG_MAX_LEN: usize = CTB_MSG_MAX_LEN;
pub const GUC_CTB_HXG_MSG_MIN_LEN: usize = CTB_HXG_MSG_MIN_LEN;
pub const GUC_CTB_HXG_MSG_MAX_LEN: usize = CTB_HXG_MSG_MAX_LEN;
pub const MAX_MMIO_MSG_LEN: usize = 4;
pub const GUC_MAX_MMIO_MSG_LEN: usize = MAX_MMIO_MSG_LEN;
pub const CT_SEND_NB: u32 = 1 << 31;
pub const CT_SEND_G2H_DW_MASK: u32 = 0xff;
pub const ACTION_HOST2GUC_CONTROL_CTB: u32 = 0x4509;
pub const CTB_CONTROL_DISABLE: u32 = 0;
pub const CTB_CONTROL_ENABLE: u32 = 1;
pub const KLV_SELF_CFG_H2G_CTB_ADDR: u16 = 0x0902;
pub const KLV_SELF_CFG_H2G_CTB_DESCRIPTOR_ADDR: u16 = 0x0903;
pub const KLV_SELF_CFG_H2G_CTB_SIZE: u16 = 0x0904;
pub const KLV_SELF_CFG_G2H_CTB_ADDR: u16 = 0x0905;
pub const KLV_SELF_CFG_G2H_CTB_DESCRIPTOR_ADDR: u16 = 0x0906;
pub const KLV_SELF_CFG_G2H_CTB_SIZE: u16 = 0x0907;
pub const ACTION_SCHED_CONTEXT_MODE_DONE: u16 = 0x1002;
pub const ACTION_CONTEXT_RESET_NOTIFICATION: u16 = 0x1008;
pub const ACTION_ENGINE_FAILURE_NOTIFICATION: u16 = 0x1009;
pub const ACTION_DEREGISTER_CONTEXT_DONE: u16 = 0x4600;
pub const ACTION_TLB_INVALIDATION_DONE: u16 = 0x7001;
pub const ACTION_STATE_CAPTURE_NOTIFICATION: u16 = 0x8002;
pub const ACTION_NOTIFY_FLUSH_LOG_BUFFER_TO_FILE: u16 = 0x8003;
pub const ACTION_NOTIFY_CRASH_DUMP_POSTED: u16 = 0x8004;
pub const ACTION_NOTIFY_EXCEPTION: u16 = 0x8005;
pub const CTB_DEADLOCK_TIMEOUT_US: u64 = 1_500_000;
pub const CTB_RESPONSE_TIMEOUT_SHORT_US: u64 = 10_000;
pub const CTB_RESPONSE_TIMEOUT_LONG_US: u64 = 1_000_000;

/// The shared descriptor is 16 dwords; only the first three are writable.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CtbDescriptor {
    pub head: u32,
    pub tail: u32,
    pub status: u32,
    pub reserved: [u32; 13],
}

impl Default for CtbDescriptor {
    fn default() -> Self {
        Self {
            head: 0,
            tail: 0,
            status: 0,
            reserved: [0; 13],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CtError {
    InvalidSize,
    InvalidMessage,
    NoRoom,
    Broken(u32),
    Incomplete,
    UnexpectedResponse,
    Interrupted,
    Timeout,
    Deadlocked,
    Failure { error: u16, hint: u16 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CtbAddresses {
    pub send_descriptor: u32,
    pub send_buffer: u32,
    pub receive_descriptor: u32,
    pub receive_buffer: u32,
}

/// upstream: intel_guc_ct.c intel_guc_ct_init() single-blob layout.
pub fn ctb_addresses(base: u32) -> Result<CtbAddresses, CtError> {
    if base == 0 || base & 0xfff != 0 {
        return Err(CtError::InvalidSize);
    }
    let address = |offset: usize| {
        base.checked_add(u32::try_from(offset).map_err(|_| CtError::InvalidSize)?)
            .ok_or(CtError::InvalidSize)
    };
    Ok(CtbAddresses {
        send_descriptor: address(CTB_SEND_DESC_OFFSET)?,
        receive_descriptor: address(CTB_RECV_DESC_OFFSET)?,
        send_buffer: address(CTB_SEND_BUFFER_OFFSET)?,
        receive_buffer: address(CTB_RECV_BUFFER_OFFSET)?,
    })
}

/// upstream: intel_guc_ct.c ct_register_buffer().
pub fn register_buffer(
    io: &impl GtIo,
    send: bool,
    descriptor: u32,
    buffer: u32,
    size_bytes: u32,
) -> Result<(), Error> {
    register_buffer_with_regs(
        io,
        crate::guc_fw::GT_GUC_SEND_REGS,
        send,
        descriptor,
        buffer,
        size_bytes,
    )
}

pub fn register_buffer_with_regs(
    io: &impl GtIo,
    regs: crate::guc_fw::GucSendRegs,
    send: bool,
    descriptor: u32,
    buffer: u32,
    size_bytes: u32,
) -> Result<(), Error> {
    let (desc_key, buffer_key, size_key) = if send {
        (
            KLV_SELF_CFG_H2G_CTB_DESCRIPTOR_ADDR,
            KLV_SELF_CFG_H2G_CTB_ADDR,
            KLV_SELF_CFG_H2G_CTB_SIZE,
        )
    } else {
        (
            KLV_SELF_CFG_G2H_CTB_DESCRIPTOR_ADDR,
            KLV_SELF_CFG_G2H_CTB_ADDR,
            KLV_SELF_CFG_G2H_CTB_SIZE,
        )
    };
    crate::guc_fw::self_config64_with_regs(io, regs, desc_key, u64::from(descriptor))?;
    crate::guc_fw::self_config64_with_regs(io, regs, buffer_key, u64::from(buffer))?;
    crate::guc_fw::self_config32_with_regs(io, regs, size_key, size_bytes)
}

/// upstream: intel_guc_ct.c guc_action_control_ctb()/ct_control_enable().
pub fn control_buffer_transport(io: &impl GtIo, enable: bool) -> Result<(), Error> {
    control_buffer_transport_with_regs(io, crate::guc_fw::GT_GUC_SEND_REGS, enable)
}

pub fn control_buffer_transport_with_regs(
    io: &impl GtIo,
    regs: crate::guc_fw::GucSendRegs,
    enable: bool,
) -> Result<(), Error> {
    let control = if enable {
        CTB_CONTROL_ENABLE
    } else {
        CTB_CONTROL_DISABLE
    };
    let result = crate::guc_fw::send_mmio_with_regs(
        io,
        regs,
        &[ACTION_HOST2GUC_CONTROL_CTB, control],
        None,
    )?;
    if result == 0 {
        Ok(())
    } else {
        Err(Error::Unavailable(ACTION_HOST2GUC_CONTROL_CTB))
    }
}

/// Disable buffer transport before releasing its pinned blob. As upstream,
/// callers must invoke this only while GuC firmware is still running.
/// upstream: intel_guc_ct.c intel_guc_ct_disable()/ct_control_enable().
pub fn disable_buffer_transport(io: &impl GtIo) -> Result<(), Error> {
    control_buffer_transport(io, false)
}

/// upstream: intel_guc_ct.c intel_guc_ct_enable(). G2H is registered first.
pub fn enable_buffer_transport(io: &impl GtIo, addresses: CtbAddresses) -> Result<(), Error> {
    enable_buffer_transport_with_regs(io, crate::guc_fw::GT_GUC_SEND_REGS, addresses)
}

pub fn enable_buffer_transport_with_regs(
    io: &impl GtIo,
    regs: crate::guc_fw::GucSendRegs,
    addresses: CtbAddresses,
) -> Result<(), Error> {
    register_buffer_with_regs(
        io,
        regs,
        false,
        addresses.receive_descriptor,
        addresses.receive_buffer,
        CTB_G2H_BUFFER_SIZE as u32,
    )?;
    register_buffer_with_regs(
        io,
        regs,
        true,
        addresses.send_descriptor,
        addresses.send_buffer,
        CTB_H2G_BUFFER_SIZE as u32,
    )?;
    control_buffer_transport_with_regs(io, regs, true)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HxgType {
    Event,
    ResponseSuccess,
    ResponseFailure,
    NoResponseRetry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CtbMessage<'a> {
    pub fence: u16,
    pub hxg_type: HxgType,
    pub aux: u32,
    pub payload: &'a [u32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CtbEvent<'a> {
    pub action: u16,
    pub data0: u16,
    pub payload: &'a [u32],
    pub released_response_credit: u32,
    /// TLB invalidation completion must bypass deferred event work.
    pub process_immediately: bool,
}

/// Owned G2H event queued for process-context dispatch. The CT IRQ path must
/// return reserved credits before queuing an event, and TLB invalidations are
/// dispatched inline to avoid blocking on work queued behind an invalidation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedCtbEvent {
    pub action: u16,
    pub data0: u16,
    pub payload: Vec<u32>,
}

/// Consumer for the action-specific GuC event handlers owned by GT/submission.
pub trait CtEventHandler {
    fn process(&mut self, action: u16, data0: u16, payload: &[u32]) -> Result<(), CtError>;
}

/// Scheduling primitive used by the source-equivalent busy-loop sender.
pub trait CtBusyWait {
    /// Sleep for `milliseconds`; true means the wait was interrupted.
    fn sleep_ms(&mut self, milliseconds: u32) -> bool;
    fn relax(&mut self);
}

/// Retry a nonblocking CT send while it reports no room, matching the inline
/// `intel_guc_send_busy_loop()` wrapper. `may_sleep` must reflect the caller's
/// atomic/IRQ context; callers that cannot sleep use `relax()` instead.
/// upstream: intel_guc.h intel_guc_send_busy_loop().
pub fn send_busy_loop<W, F>(
    wait: &mut W,
    may_sleep: bool,
    loop_on_busy: bool,
    mut send_nonblocking: F,
) -> Result<(), CtError>
where
    W: CtBusyWait,
    F: FnMut() -> Result<(), CtError>,
{
    let mut sleep_period_ms = 1u32;
    loop {
        match send_nonblocking() {
            Err(CtError::NoRoom) if loop_on_busy => {
                if may_sleep {
                    if wait.sleep_ms(sleep_period_ms) {
                        return Err(CtError::Interrupted);
                    }
                    sleep_period_ms = sleep_period_ms.saturating_mul(2);
                } else {
                    wait.relax();
                }
            }
            result => return result,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CtCompletion {
    Success {
        data0: u32,
        payload: Vec<u32>,
        truncated: bool,
    },
    Retry {
        reason: u32,
    },
    Failure {
        error: u16,
        hint: u16,
    },
}

struct PendingRequest {
    fence: u16,
    response_capacity: usize,
    completion: Option<CtCompletion>,
}

/// upstream: intel_guc_ct.c ct_handle_msg()/ct_handle_hxg().
pub fn parse_message(words: &[u32]) -> Result<CtbMessage<'_>, CtError> {
    if words.len() < CTB_HXG_MSG_MIN_LEN
        || words.len() > CTB_MSG_MAX_LEN
        || (words[0] & CTB_MSG_0_FORMAT_MASK) != CTB_FORMAT_HXG << 12
        || (words[0] & CTB_MSG_0_NUM_DWORDS_MASK) as usize + CTB_HDR_LEN != words.len()
    {
        return Err(CtError::InvalidMessage);
    }
    let hxg = words[1];
    if hxg & HXG_ORIGIN_GUC == 0 {
        return Err(CtError::InvalidMessage);
    }
    let hxg_type = match hxg & HXG_TYPE_MASK {
        HXG_TYPE_EVENT => HxgType::Event,
        HXG_TYPE_RESPONSE_SUCCESS => HxgType::ResponseSuccess,
        HXG_TYPE_RESPONSE_FAILURE => HxgType::ResponseFailure,
        HXG_TYPE_NO_RESPONSE_RETRY => HxgType::NoResponseRetry,
        _ => return Err(CtError::InvalidMessage),
    };
    Ok(CtbMessage {
        fence: (words[0] >> 16) as u16,
        hxg_type,
        aux: hxg & HXG_AUX_MASK,
        payload: &words[2..],
    })
}

/// upstream: guc_communication_ctb_abi.h CTB header fields.
pub const fn ctb_message_header(fence: u16, dwords_after_header: u8) -> u32 {
    ((fence as u32) << 16) | (CTB_FORMAT_HXG << 12) | dwords_after_header as u32
}

/// upstream: guc_messages_abi.h HXG request / fast-request message header.
pub const fn hxg_request_header(action_data0: u32, fast: bool) -> u32 {
    let kind = if fast {
        HXG_TYPE_FAST_REQUEST
    } else {
        HXG_TYPE_REQUEST
    };
    kind | (action_data0 & (HXG_ACTION_MASK | HXG_DATA0_MASK))
}

/// upstream: intel_guc_ct.c G2H_LEN_DW().
pub const fn response_dwords(flags: u32) -> u32 {
    let len = flags & CT_SEND_G2H_DW_MASK;
    if len == 0 { 0 } else { len + 2 }
}

/// One side of a CTB ring. Descriptor head is peer-owned, tail is local-owned.
pub struct CtbBuffer {
    pub descriptor: CtbDescriptor,
    commands: Vec<u32>,
    pub reserved_space: u32,
    pub broken: bool,
    local_head: u32,
    local_tail: u32,
    space: u32,
}

impl CtbBuffer {
    /// upstream: intel_guc_ct.c guc_ct_buffer_init()/guc_ct_buffer_reset().
    pub fn new(size_bytes: usize, reserved_bytes: usize) -> Result<Self, CtError> {
        if size_bytes == 0
            || !size_bytes.is_multiple_of(4)
            || reserved_bytes >= size_bytes
            || !reserved_bytes.is_multiple_of(4)
        {
            return Err(CtError::InvalidSize);
        }
        let size_dw = u32::try_from(size_bytes / 4).map_err(|_| CtError::InvalidSize)?;
        let reserved_space = u32::try_from(reserved_bytes / 4).map_err(|_| CtError::InvalidSize)?;
        Ok(Self {
            descriptor: CtbDescriptor::default(),
            commands: alloc::vec![0; size_dw as usize],
            reserved_space,
            broken: false,
            local_head: 0,
            local_tail: 0,
            space: size_dw - 1 - reserved_space,
        })
    }

    pub fn capacity_dwords(&self) -> u32 {
        self.commands.len() as u32
    }

    pub fn available_dwords(&self) -> u32 {
        self.space
    }

    /// Apply the peer-owned descriptor head (send) or tail (receive).
    /// upstream: intel_guc_ct.c ct_write()/ct_read() descriptor import.
    pub fn update_peer(&mut self, position: u32) -> Result<(), CtError> {
        if position >= self.capacity_dwords() {
            self.descriptor.status |= CTB_STATUS_OVERFLOW;
            self.broken = true;
            return Err(CtError::Broken(self.descriptor.status));
        }
        if self.reserved_space == 0 {
            self.descriptor.head = position;
        } else {
            self.descriptor.tail = position;
        }
        self.refresh_space()
    }

    /// upstream: intel_guc_ct.c h2g_has_room()/g2h_has_room().
    fn refresh_space(&mut self) -> Result<(), CtError> {
        if self.broken || self.descriptor.status != 0 {
            self.broken = true;
            return Err(CtError::Broken(self.descriptor.status));
        }
        // Upstream's receive `space` counter tracks response credits reserved
        // by H2G requests, not the physical ring occupancy.
        if self.reserved_space != 0 {
            return Ok(());
        }
        let size = self.capacity_dwords();
        let peer = if self.reserved_space == 0 {
            self.descriptor.head
        } else {
            self.descriptor.tail
        };
        if peer >= size || self.local_tail >= size || self.local_head >= size {
            self.descriptor.status |= CTB_STATUS_OVERFLOW;
            self.broken = true;
            return Err(CtError::Broken(self.descriptor.status));
        }
        self.space = (peer + size - self.local_tail - 1) % size;
        Ok(())
    }

    fn write_word(&mut self, value: u32) {
        let index = self.local_tail as usize;
        self.commands[index] = value;
        self.local_tail = (self.local_tail + 1) % self.capacity_dwords();
    }

    /// upstream: intel_guc_ct.c ct_write().
    pub fn write_hxg(&mut self, action: &[u32], fence: u16, fast: bool) -> Result<u32, CtError> {
        if action.is_empty() || action.len() > CTB_MSG_MAX_LEN - 1 {
            return Err(CtError::InvalidMessage);
        }
        if self.broken || self.descriptor.status != 0 {
            self.broken = true;
            return Err(CtError::Broken(self.descriptor.status));
        }
        if self.descriptor.tail != self.local_tail {
            self.descriptor.status |= CTB_STATUS_MISMATCH;
            self.broken = true;
            return Err(CtError::Broken(self.descriptor.status));
        }
        self.refresh_space()?;
        let required = action.len() as u32 + 1;
        if self.space < required {
            return Err(CtError::NoRoom);
        }

        self.write_word(ctb_message_header(fence, action.len() as u8));
        self.write_word(hxg_request_header(action[0], fast));
        for word in action.iter().copied().skip(1) {
            self.write_word(word);
        }
        self.space -= required;
        atomic_fence(Ordering::Release);
        self.descriptor.tail = self.local_tail;
        Ok(self.local_tail)
    }

    /// upstream: intel_guc_ct.c ct_read().
    pub fn read_message(&mut self) -> Result<Option<Vec<u32>>, CtError> {
        if self.broken || self.descriptor.status != 0 {
            self.broken = true;
            return Err(CtError::Broken(self.descriptor.status));
        }
        if self.descriptor.head != self.local_head {
            self.descriptor.status |= CTB_STATUS_MISMATCH;
            self.broken = true;
            return Err(CtError::Broken(self.descriptor.status));
        }
        let size = self.capacity_dwords();
        let tail = self.descriptor.tail;
        if tail >= size || self.local_head >= size {
            self.descriptor.status |= CTB_STATUS_OVERFLOW;
            self.broken = true;
            return Err(CtError::Broken(self.descriptor.status));
        }
        let available = (tail + size - self.local_head) % size;
        if available == 0 {
            return Ok(None);
        }
        let header = self.commands[self.local_head as usize];
        let total = ((header & 0xff) as u32 + CTB_HDR_LEN as u32) as usize;
        if total > available as usize || total > CTB_MSG_MAX_LEN {
            self.descriptor.status |= CTB_STATUS_UNDERFLOW;
            self.broken = true;
            return Err(CtError::Incomplete);
        }
        let mut message = Vec::new();
        message
            .try_reserve_exact(total)
            .map_err(|_| CtError::NoRoom)?;
        let mut cursor = self.local_head;
        for _ in 0..total {
            message.push(self.commands[cursor as usize]);
            cursor = (cursor + 1) % size;
        }
        self.local_head = cursor;
        atomic_fence(Ordering::Release);
        self.descriptor.head = cursor;
        Ok(Some(message))
    }

    fn reserve_response_space(&mut self, dwords: u32) -> Result<(), CtError> {
        if dwords > self.space {
            return Err(CtError::NoRoom);
        }
        self.space -= dwords;
        Ok(())
    }

    fn release_response_space(&mut self, dwords: u32) -> Result<(), CtError> {
        let maximum = self.capacity_dwords() - 1 - self.reserved_space;
        if self.space.saturating_add(dwords) > maximum {
            return Err(CtError::InvalidMessage);
        }
        self.space += dwords;
        Ok(())
    }

    /// upstream: intel_guc_ct.c guc_ct_buffer_reset().
    pub fn reset(&mut self) {
        self.descriptor = CtbDescriptor::default();
        self.local_head = 0;
        self.local_tail = 0;
        self.space = self.capacity_dwords() - 1 - self.reserved_space;
        self.broken = false;
    }
}

/// Outstanding G2H response credits are held back from unsolicited events.
pub struct CtbPair {
    pub send: CtbBuffer,
    pub receive: CtbBuffer,
    /// Set when the peer posts G2H after CT shutdown; caller should diagnose.
    pub unused_receive_status_seen: bool,
    next_fence: u16,
    pending: Vec<PendingRequest>,
    incoming: Vec<QueuedCtbEvent>,
}

impl CtbPair {
    pub fn new() -> Result<Self, CtError> {
        Ok(Self {
            send: CtbBuffer::new(CTB_H2G_BUFFER_SIZE, 0)?,
            receive: CtbBuffer::new(CTB_G2H_BUFFER_SIZE, G2H_ROOM_BUFFER_SIZE)?,
            unused_receive_status_seen: false,
            next_fence: 0,
            pending: Vec::new(),
            incoming: Vec::new(),
        })
    }

    /// Reset both shared rings and discard stale request/event state before
    /// registering them again with GuC.
    /// upstream: intel_guc_ct.c intel_guc_ct_enable() buffer reset sequence.
    pub fn reset(&mut self) {
        self.send.reset();
        self.receive.reset();
        self.unused_receive_status_seen = false;
        self.next_fence = 0;
        self.pending.clear();
        self.incoming.clear();
    }

    /// upstream: intel_guc_ct.c ct_send_nb().
    pub fn send_nonblocking(&mut self, action: &[u32], flags: u32) -> Result<u16, CtError> {
        let response = response_dwords(flags);
        if self.receive.available_dwords() < response {
            return Err(CtError::NoRoom);
        }
        self.next_fence = self.next_fence.wrapping_add(1);
        let fence = self.next_fence;
        let fast = flags & CT_SEND_NB != 0;
        self.send.write_hxg(action, fence, fast)?;
        self.receive.reserve_response_space(response)?;
        Ok(fence)
    }

    /// upstream: intel_guc_ct.c g2h_release_space().
    pub fn release_response_space(&mut self, dwords: u32) -> Result<(), CtError> {
        self.receive.release_response_space(dwords)
    }

    /// Report/clear the upstream UNUSED-status notification after CT shutdown.
    pub fn take_unused_receive_status_seen(&mut self) -> bool {
        core::mem::replace(&mut self.unused_receive_status_seen, false)
    }

    /// Initial shared blob created by intel_guc_ct_init().
    // upstream: intel_guc_ct.c intel_guc_ct_init()
    pub fn initial_blob(&self) -> Vec<u8> {
        let mut blob = alloc::vec![0; CTB_BLOB_SIZE];
        encode_descriptor(&mut blob, CTB_SEND_DESC_OFFSET, &self.send.descriptor);
        encode_descriptor(&mut blob, CTB_RECV_DESC_OFFSET, &self.receive.descriptor);
        encode_commands(&mut blob, CTB_SEND_BUFFER_OFFSET, &self.send.commands);
        encode_commands(&mut blob, CTB_RECV_BUFFER_OFFSET, &self.receive.commands);
        blob
    }

    /// Import peer-owned descriptor fields and G2H command dwords before read.
    /// upstream: intel_guc_ct.c ct_read() peer descriptor/data import.
    pub fn sync_from_blob(&mut self, blob: &[u8]) -> Result<(), CtError> {
        if blob.len() < CTB_BLOB_SIZE {
            return Err(CtError::InvalidSize);
        }
        self.send.descriptor.head = read_dword(blob, CTB_SEND_DESC_OFFSET)?;
        self.send.descriptor.tail = read_dword(blob, CTB_SEND_DESC_OFFSET + 4)?;
        self.send.descriptor.status = read_dword(blob, CTB_SEND_DESC_OFFSET + 8)?;
        if self.send.descriptor.tail != self.send.local_tail {
            self.send.descriptor.status |= CTB_STATUS_MISMATCH;
        }
        self.send.update_peer(self.send.descriptor.head)?;

        self.receive.descriptor.head = read_dword(blob, CTB_RECV_DESC_OFFSET)?;
        self.receive.descriptor.tail = read_dword(blob, CTB_RECV_DESC_OFFSET + 4)?;
        let receive_status = read_dword(blob, CTB_RECV_DESC_OFFSET + 8)?;
        self.unused_receive_status_seen |= receive_status & CTB_STATUS_UNUSED != 0;
        self.receive.descriptor.status = receive_status & !CTB_STATUS_UNUSED;
        if self.receive.descriptor.head != self.receive.local_head {
            self.receive.descriptor.status |= CTB_STATUS_MISMATCH;
        }
        self.receive.update_peer(self.receive.descriptor.tail)?;
        decode_commands(blob, CTB_RECV_BUFFER_OFFSET, &mut self.receive.commands)?;
        Ok(())
    }

    /// Publish host-owned H2G dwords and descriptor positions to the shared blob.
    /// upstream: intel_guc_ct.c ct_write() host-owned descriptor/data publish.
    pub fn sync_to_blob(&self, blob: &mut [u8]) -> Result<(), CtError> {
        if blob.len() < CTB_BLOB_SIZE {
            return Err(CtError::InvalidSize);
        }
        write_dword(blob, CTB_SEND_DESC_OFFSET + 4, self.send.local_tail)?;
        write_dword(blob, CTB_RECV_DESC_OFFSET, self.receive.local_head)?;
        encode_commands(blob, CTB_SEND_BUFFER_OFFSET, &self.send.commands);
        Ok(())
    }

    /// upstream: intel_guc_ct.c ct_send() request registration and CT write.
    pub fn send_request(
        &mut self,
        action: &[u32],
        response_capacity: usize,
    ) -> Result<u16, CtError> {
        if action.is_empty()
            || action.len() > CTB_MSG_MAX_LEN - 1
            || response_capacity > CTB_MSG_MAX_LEN - CTB_HXG_MSG_MIN_LEN
        {
            return Err(CtError::InvalidMessage);
        }
        let fence = self.next_fence.wrapping_add(1);
        if self.receive.available_dwords() < CTB_MSG_MAX_LEN as u32
            || self.pending.iter().any(|entry| entry.fence == fence)
        {
            return Err(CtError::NoRoom);
        }
        self.pending.try_reserve(1).map_err(|_| CtError::NoRoom)?;
        self.next_fence = fence;
        self.send.write_hxg(action, fence, false)?;
        self.receive
            .reserve_response_space(CTB_MSG_MAX_LEN as u32)?;
        self.pending.push(PendingRequest {
            fence,
            response_capacity,
            completion: None,
        });
        Ok(fence)
    }

    /// upstream: intel_guc_ct.c ct_send() wait/retry loop and ct_deadlocked().
    /// `exchange` synchronizes the shared blob, notifies GuC for `Some(fence)`,
    /// polls the peer response using the supplied short/long deadlines, and
    /// refreshes peer head state when called with `None` after ring backpressure.
    pub fn send_request_with_retry(
        &mut self,
        io: &impl crate::GtIo,
        action: &[u32],
        response_capacity: usize,
        mut exchange: impl FnMut(&mut Self, Option<u16>, u64, u64) -> Result<(), CtError>,
    ) -> Result<CtCompletion, CtError> {
        let mut stall_start = io.now_us();
        loop {
            match self.send_request(action, response_capacity) {
                Ok(fence) => {
                    let response_start = io.now_us();
                    exchange(
                        self,
                        Some(fence),
                        CTB_RESPONSE_TIMEOUT_SHORT_US,
                        CTB_RESPONSE_TIMEOUT_LONG_US,
                    )?;
                    if io.now_us().saturating_sub(response_start)
                        > CTB_RESPONSE_TIMEOUT_SHORT_US + CTB_RESPONSE_TIMEOUT_LONG_US
                    {
                        return Err(CtError::Timeout);
                    }
                    match self.finish_request(fence)? {
                        completion @ CtCompletion::Success { .. } => return Ok(completion),
                        CtCompletion::Failure { error, hint } => {
                            return Err(CtError::Failure { error, hint });
                        }
                        CtCompletion::Retry { .. } => {
                            stall_start = io.now_us();
                            continue;
                        }
                    }
                }
                Err(CtError::NoRoom) => {
                    if io.now_us().saturating_sub(stall_start) > CTB_DEADLOCK_TIMEOUT_US {
                        return Err(CtError::Deadlocked);
                    }
                    exchange(
                        self,
                        None,
                        CTB_RESPONSE_TIMEOUT_SHORT_US,
                        CTB_RESPONSE_TIMEOUT_LONG_US,
                    )?;
                    io.delay_us(1000);
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// upstream: intel_guc_ct.c ct_handle_response().
    pub fn handle_response(&mut self, message: CtbMessage<'_>) -> Result<(), CtError> {
        if message.hxg_type == HxgType::Event {
            return Err(CtError::InvalidMessage);
        }
        let request = self
            .pending
            .iter_mut()
            .find(|entry| entry.fence == message.fence)
            .ok_or(CtError::UnexpectedResponse)?;
        if request.completion.is_some() {
            return Err(CtError::UnexpectedResponse);
        }
        request.completion = Some(match message.hxg_type {
            HxgType::ResponseSuccess => {
                let truncated = message.payload.len() > request.response_capacity;
                let payload_len = message.payload.len().min(request.response_capacity);
                let mut payload = Vec::new();
                payload
                    .try_reserve_exact(payload_len)
                    .map_err(|_| CtError::NoRoom)?;
                payload.extend_from_slice(&message.payload[..payload_len]);
                CtCompletion::Success {
                    data0: message.aux,
                    payload,
                    truncated,
                }
            }
            HxgType::NoResponseRetry => CtCompletion::Retry {
                reason: message.aux,
            },
            HxgType::ResponseFailure => CtCompletion::Failure {
                error: (message.aux & HXG_FAILURE_ERROR_MASK) as u16,
                hint: ((message.aux & HXG_FAILURE_HINT_MASK) >> 16) as u16,
            },
            HxgType::Event => return Err(CtError::InvalidMessage),
        });
        Ok(())
    }

    /// upstream: intel_guc_ct.c ct_handle_event()/ct_handle_hxg().
    /// Responses complete their request here; events return to the IRQ owner
    /// for immediate or deferred dispatch, after releasing reserved credits
    /// for event classes that can otherwise deadlock H2G submission.
    pub fn handle_incoming_message<'a>(
        &mut self,
        words: &'a [u32],
    ) -> Result<Option<CtbEvent<'a>>, CtError> {
        let message = parse_message(words)?;
        if message.hxg_type != HxgType::Event {
            self.handle_response(message)?;
            return Ok(None);
        }
        let action = (message.aux & HXG_ACTION_MASK) as u16;
        let data0 = ((message.aux & HXG_DATA0_MASK) >> 16) as u16;
        let release_credit = if matches!(
            action,
            ACTION_SCHED_CONTEXT_MODE_DONE
                | ACTION_DEREGISTER_CONTEXT_DONE
                | ACTION_TLB_INVALIDATION_DONE
        ) {
            u32::try_from(words.len()).map_err(|_| CtError::InvalidMessage)?
        } else {
            0
        };
        if release_credit != 0 {
            self.receive.release_response_space(release_credit)?;
        }
        Ok(Some(CtbEvent {
            action,
            data0,
            payload: message.payload,
            released_response_credit: release_credit,
            process_immediately: action == ACTION_TLB_INVALIDATION_DONE,
        }))
    }

    /// Handle an event in the receive context or queue it for the worker.
    /// TLB invalidation completion is synchronous, matching ct_handle_event().
    /// upstream: intel_guc_ct.c ct_handle_event()/ct_process_request().
    pub fn dispatch_event<H: CtEventHandler>(
        &mut self,
        event: CtbEvent<'_>,
        handler: &mut H,
    ) -> Result<(), CtError> {
        if event.process_immediately {
            return handler.process(event.action, event.data0, event.payload);
        }
        self.incoming.push(QueuedCtbEvent {
            action: event.action,
            data0: event.data0,
            payload: event.payload.to_vec(),
        });
        Ok(())
    }

    /// Drain events in arrival order. Like the upstream work item, continue
    /// until the queue is empty so concurrent arrivals are not stranded.
    /// upstream: intel_guc_ct.c ct_process_incoming_requests()/ct_incoming_request_worker_func().
    pub fn process_incoming_requests<H: CtEventHandler>(
        &mut self,
        handler: &mut H,
    ) -> Result<usize, CtError> {
        let mut processed = 0;
        while !self.incoming.is_empty() {
            let event = self.incoming.remove(0);
            handler.process(event.action, event.data0, &event.payload)?;
            processed += 1;
        }
        Ok(processed)
    }

    /// Consume a completed request and release the full maximum-size G2H
    /// reservation, matching ct_send() after its wait finishes.
    /// upstream: intel_guc_ct.c ct_send() wait completion and release credits.
    pub fn finish_request(&mut self, fence: u16) -> Result<CtCompletion, CtError> {
        let index = self
            .pending
            .iter()
            .position(|entry| entry.fence == fence)
            .ok_or(CtError::UnexpectedResponse)?;
        let completion = self.pending[index]
            .completion
            .as_ref()
            .ok_or(CtError::Incomplete)?
            .clone();
        self.pending.remove(index);
        self.receive
            .release_response_space(CTB_MSG_MAX_LEN as u32)?;
        Ok(completion)
    }
}

fn read_dword(blob: &[u8], offset: usize) -> Result<u32, CtError> {
    let bytes = blob
        .get(offset..offset.checked_add(4).ok_or(CtError::InvalidSize)?)
        .ok_or(CtError::InvalidSize)?;
    Ok(u32::from_le_bytes(
        bytes.try_into().map_err(|_| CtError::InvalidSize)?,
    ))
}

fn write_dword(blob: &mut [u8], offset: usize, value: u32) -> Result<(), CtError> {
    let bytes = blob
        .get_mut(offset..offset.checked_add(4).ok_or(CtError::InvalidSize)?)
        .ok_or(CtError::InvalidSize)?;
    bytes.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn encode_descriptor(blob: &mut [u8], offset: usize, desc: &CtbDescriptor) {
    let _ = write_dword(blob, offset, desc.head);
    let _ = write_dword(blob, offset + 4, desc.tail);
    let _ = write_dword(blob, offset + 8, desc.status);
    for (index, value) in desc.reserved.iter().copied().enumerate() {
        let _ = write_dword(blob, offset + 12 + index * 4, value);
    }
}

fn encode_commands(blob: &mut [u8], offset: usize, commands: &[u32]) {
    for (index, word) in commands.iter().copied().enumerate() {
        let _ = write_dword(blob, offset + index * 4, word);
    }
}

fn decode_commands(blob: &[u8], offset: usize, commands: &mut [u32]) -> Result<(), CtError> {
    for (index, word) in commands.iter_mut().enumerate() {
        *word = read_dword(blob, offset + index * 4)?;
    }
    Ok(())
}

/// upstream: intel_guc_ct.c intel_guc_ct_max_queue_time_jiffies().
pub const fn max_queue_time_ms() -> u32 {
    (CTB_H2G_BUFFER_SIZE as u32 * 1000) / 2048
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::cell::{Cell, RefCell};

    use super::*;

    #[derive(Default)]
    struct RecordingEventHandler(Vec<(u16, u16, Vec<u32>)>);
    impl CtEventHandler for RecordingEventHandler {
        fn process(&mut self, action: u16, data0: u16, payload: &[u32]) -> Result<(), CtError> {
            self.0.push((action, data0, payload.to_vec()));
            Ok(())
        }
    }

    #[derive(Default)]
    struct BusyWaitTrace {
        sleeps: Vec<u32>,
        relaxes: usize,
        interrupt: bool,
    }
    impl CtBusyWait for BusyWaitTrace {
        fn sleep_ms(&mut self, milliseconds: u32) -> bool {
            self.sleeps.push(milliseconds);
            self.interrupt
        }
        fn relax(&mut self) {
            self.relaxes += 1;
        }
    }

    struct ClockIo(Cell<u64>);
    impl crate::GtIo for ClockIo {
        fn read(&self, offset: u32) -> Result<u32, Error> {
            Err(Error::Unavailable(offset))
        }
        fn write(&self, offset: u32, _value: u32) -> Result<(), Error> {
            Err(Error::Unavailable(offset))
        }
        fn now_us(&self) -> u64 {
            self.0.get()
        }
        fn delay_us(&self, micros: u32) {
            self.0.set(self.0.get().saturating_add(u64::from(micros)));
        }
    }

    struct MmioIo {
        writes: RefCell<Vec<(u32, u32)>>,
        notifications: Cell<usize>,
        response_data: u32,
    }
    impl GtIo for MmioIo {
        fn read(&self, offset: u32) -> Result<u32, Error> {
            if offset == 0x190240 {
                Ok(HXG_ORIGIN_GUC | HXG_TYPE_RESPONSE_SUCCESS | self.response_data)
            } else if (0x190244..0x190250).contains(&offset) {
                Ok(0)
            } else {
                Err(Error::Unavailable(offset))
            }
        }
        fn write(&self, offset: u32, value: u32) -> Result<(), Error> {
            self.writes.borrow_mut().push((offset, value));
            if offset == 0x1901f0 {
                self.notifications.set(self.notifications.get() + 1);
            }
            Ok(())
        }
        fn now_us(&self) -> u64 {
            0
        }
        fn delay_us(&self, _micros: u32) {}
    }

    #[test]
    fn ctb_abi_struct_layout_and_blob_sizes_match_upstream() {
        assert_eq!(core::mem::size_of::<CtbDescriptor>(), 64);
        assert_eq!(CTB_DESC_SIZE, 2048);
        assert_eq!(CTB_BLOB_SIZE, 24 * 1024);
        assert_eq!(
            ctb_addresses(0x10_0000).unwrap(),
            CtbAddresses {
                send_descriptor: 0x10_0000,
                receive_descriptor: 0x10_0800,
                send_buffer: 0x10_1000,
                receive_buffer: 0x10_2000,
            }
        );
        assert_eq!(ctb_addresses(0x10_0001), Err(CtError::InvalidSize));
        assert_eq!(max_queue_time_ms(), 2000);
        assert_eq!(ctb_message_header(0x1234, 2), (0x1234 << 16) | 2);
        assert_eq!(hxg_request_header(0x1234, false), 0x1234);
        assert_eq!(
            hxg_request_header(0x1234, true),
            HXG_TYPE_FAST_REQUEST | 0x1234
        );
    }

    #[test]
    fn ctb_registration_uses_receive_descriptor_buffer_size_order() {
        let io = MmioIo {
            writes: RefCell::new(Vec::new()),
            notifications: Cell::new(0),
            response_data: 1,
        };
        assert_eq!(
            register_buffer(&io, false, 0x10_0800, 0x10_2000, CTB_G2H_BUFFER_SIZE as u32),
            Ok(())
        );
        let writes = io.writes.borrow();
        assert_eq!(writes[0], (0x190240, 0x0508));
        assert_eq!(writes[1], (0x190244, (0x0906 << 16) | 2));
        assert_eq!(writes[2], (0x190248, 0x10_0800));
        assert_eq!(writes[3], (0x19024c, 0));
        assert_eq!(writes[5], (0x190240, 0x0508));
        assert_eq!(writes[6], (0x190244, (0x0905 << 16) | 2));
        assert_eq!(writes[10], (0x190240, 0x0508));
        assert_eq!(writes[11], (0x190244, (0x0907 << 16) | 1));
        assert_eq!(writes[12], (0x190248, CTB_G2H_BUFFER_SIZE as u32));
        assert_eq!(io.notifications.get(), 3);
        drop(writes);

        let pass_through = MmioIo {
            writes: RefCell::new(Vec::new()),
            notifications: Cell::new(0),
            response_data: 1,
        };
        register_buffer(&pass_through, true, 0, 0, 1).unwrap();
    }

    #[test]
    fn ctb_enable_and_disable_use_the_mmio_control_action() {
        let io = MmioIo {
            writes: RefCell::new(Vec::new()),
            notifications: Cell::new(0),
            response_data: 0,
        };
        assert_eq!(control_buffer_transport(&io, true), Ok(()));
        assert_eq!(
            io.writes.borrow()[0],
            (0x190240, ACTION_HOST2GUC_CONTROL_CTB)
        );
        assert_eq!(io.writes.borrow()[1], (0x190244, CTB_CONTROL_ENABLE));
        assert_eq!(disable_buffer_transport(&io), Ok(()));
        assert_eq!(io.writes.borrow()[4], (0x190244, CTB_CONTROL_DISABLE));
    }

    #[test]
    fn ctb_pair_reset_clears_ring_and_pending_state_before_reenable() {
        let mut pair = CtbPair::new().unwrap();
        let _ = pair.send_request(&[0x4000], 1).unwrap();
        pair.send.descriptor.status = CTB_STATUS_OVERFLOW;
        pair.incoming.push(QueuedCtbEvent {
            action: ACTION_CONTEXT_RESET_NOTIFICATION,
            data0: 0,
            payload: vec![1],
        });
        pair.reset();
        assert_eq!(pair.send.descriptor, CtbDescriptor::default());
        assert!(pair.pending.is_empty());
        assert!(pair.incoming.is_empty());
        assert_eq!(pair.next_fence, 0);
        assert_eq!(pair.receive.available_dwords(), 3071);
    }

    #[test]
    fn ctb_send_and_receive_preserve_header_payload_and_wrap() {
        let mut pair = CtbPair::new().unwrap();
        pair.send.local_tail = pair.send.capacity_dwords() - 2;
        pair.send.descriptor.tail = pair.send.local_tail;
        pair.send.descriptor.head = 2;
        let fence = pair.send_nonblocking(&[0x1234, 0xfeed_beef], 0).unwrap();
        assert_eq!(fence, 1);
        assert_eq!(pair.send.descriptor.tail, 1);
        let size = pair.send.capacity_dwords() as usize;
        assert_eq!(pair.send.commands[size - 2], ctb_message_header(1, 2));
        assert_eq!(pair.send.commands[size - 1], HXG_TYPE_REQUEST | 0x1234);
        assert_eq!(pair.send.commands[0], 0xfeed_beef);

        let receive_size = pair.receive.capacity_dwords() as usize;
        let receive_head = receive_size - 1;
        pair.receive.local_head = receive_head as u32;
        pair.receive.descriptor.head = receive_head as u32;
        pair.receive.commands[receive_head] = ctb_message_header(9, 2);
        pair.receive.commands[0] = HXG_ORIGIN_GUC | HXG_TYPE_RESPONSE_SUCCESS;
        pair.receive.commands[1] = 0x55;
        pair.receive.descriptor.tail = 2;
        assert_eq!(
            pair.receive.read_message().unwrap(),
            Some(vec![
                ctb_message_header(9, 2),
                HXG_ORIGIN_GUC | HXG_TYPE_RESPONSE_SUCCESS,
                0x55
            ])
        );
        assert_eq!(pair.receive.descriptor.head, 2);
    }

    #[test]
    fn ctb_shared_blob_sync_uses_host_owned_and_peer_owned_descriptor_fields() {
        let mut pair = CtbPair::new().unwrap();
        let mut blob = pair.initial_blob();
        pair.send_nonblocking(&[0x2202, 0x55], 0).unwrap();
        pair.sync_to_blob(&mut blob).unwrap();
        assert_eq!(read_dword(&blob, CTB_SEND_DESC_OFFSET + 4), Ok(3));
        assert_eq!(
            read_dword(&blob, CTB_SEND_BUFFER_OFFSET),
            Ok(ctb_message_header(1, 2))
        );

        // Simulate GuC consuming H2G and posting one G2H response.
        write_dword(&mut blob, CTB_SEND_DESC_OFFSET, 3).unwrap();
        write_dword(&mut blob, CTB_RECV_DESC_OFFSET + 4, 3).unwrap();
        write_dword(&mut blob, CTB_RECV_BUFFER_OFFSET, ctb_message_header(1, 2)).unwrap();
        write_dword(
            &mut blob,
            CTB_RECV_BUFFER_OFFSET + 4,
            HXG_ORIGIN_GUC | HXG_TYPE_RESPONSE_SUCCESS,
        )
        .unwrap();
        write_dword(&mut blob, CTB_RECV_BUFFER_OFFSET + 8, 7).unwrap();
        pair.sync_from_blob(&blob).unwrap();
        assert!(pair.send.available_dwords() > 1000);
        let response = pair.receive.read_message().unwrap().unwrap();
        assert_eq!(response[2], 7);
        pair.sync_to_blob(&mut blob).unwrap();
        assert_eq!(read_dword(&blob, CTB_RECV_DESC_OFFSET), Ok(3));
    }

    #[test]
    fn unused_receive_status_is_ignored_but_real_status_bits_break_the_ctb() {
        let mut pair = CtbPair::new().unwrap();
        let mut blob = pair.initial_blob();
        write_dword(&mut blob, CTB_RECV_DESC_OFFSET + 8, CTB_STATUS_UNUSED).unwrap();
        pair.sync_from_blob(&blob).unwrap();
        assert!(!pair.receive.broken);
        assert!(pair.unused_receive_status_seen);

        write_dword(
            &mut blob,
            CTB_RECV_DESC_OFFSET + 8,
            CTB_STATUS_UNUSED | CTB_STATUS_OVERFLOW,
        )
        .unwrap();
        assert_eq!(
            pair.sync_from_blob(&blob),
            Err(CtError::Broken(CTB_STATUS_OVERFLOW))
        );
    }

    #[test]
    fn ctb_rejects_corrupt_or_incomplete_peer_messages() {
        let mut buffer = CtbBuffer::new(64, 16).unwrap();
        buffer.descriptor.status = CTB_STATUS_OVERFLOW;
        assert_eq!(
            buffer.read_message(),
            Err(CtError::Broken(CTB_STATUS_OVERFLOW))
        );

        let mut buffer = CtbBuffer::new(64, 16).unwrap();
        buffer.commands[0] = ctb_message_header(0, 3);
        buffer.descriptor.tail = 2;
        assert_eq!(buffer.read_message(), Err(CtError::Incomplete));
        assert_ne!(buffer.descriptor.status & CTB_STATUS_UNDERFLOW, 0);
    }

    #[test]
    fn ctb_hxg_parser_validates_format_origin_and_incoming_types() {
        let response = [
            ctb_message_header(7, 2),
            HXG_ORIGIN_GUC | HXG_TYPE_RESPONSE_SUCCESS | 0x12,
            0xfeed_beef,
        ];
        let parsed = parse_message(&response).unwrap();
        assert_eq!(parsed.fence, 7);
        assert_eq!(parsed.hxg_type, HxgType::ResponseSuccess);
        assert_eq!(parsed.aux, 0x12);
        assert_eq!(parsed.payload, &[0xfeed_beef]);

        let bad_origin = [ctb_message_header(0, 1), HXG_TYPE_EVENT];
        assert_eq!(parse_message(&bad_origin), Err(CtError::InvalidMessage));
        let unsupported = [
            ctb_message_header(0, 1),
            HXG_ORIGIN_GUC | HXG_TYPE_FAST_REQUEST,
        ];
        assert_eq!(parse_message(&unsupported), Err(CtError::InvalidMessage));
        let reserved_header = [
            ctb_message_header(0, 1) | CTB_MSG_0_RESERVED_MASK,
            HXG_ORIGIN_GUC | HXG_TYPE_EVENT,
        ];
        assert_eq!(
            parse_message(&reserved_header).unwrap().hxg_type,
            HxgType::Event
        );
    }

    #[test]
    fn ctb_event_handler_returns_action_and_releases_reserved_irq_credit() {
        let mut pair = CtbPair::new().unwrap();
        pair.receive.reserve_response_space(3).unwrap();
        let words = [
            ctb_message_header(0, 2),
            HXG_ORIGIN_GUC | HXG_TYPE_EVENT | u32::from(ACTION_TLB_INVALIDATION_DONE),
            0x55,
        ];
        let event = pair.handle_incoming_message(&words).unwrap().unwrap();
        assert_eq!(event.action, ACTION_TLB_INVALIDATION_DONE);
        assert_eq!(event.payload, [0x55]);
        assert_eq!(event.released_response_credit, 3);
        assert!(event.process_immediately);
        assert_eq!(
            pair.receive.available_dwords(),
            CTB_G2H_BUFFER_SIZE as u32 / 4 - 1 - G2H_ROOM_BUFFER_SIZE as u32 / 4
        );
    }

    #[test]
    fn ctb_event_worker_defers_normal_events_but_processes_tlb_inline() {
        let mut pair = CtbPair::new().unwrap();
        let mut handler = RecordingEventHandler::default();
        let normal = CtbEvent {
            action: ACTION_CONTEXT_RESET_NOTIFICATION,
            data0: 2,
            payload: &[0xaa],
            released_response_credit: 0,
            process_immediately: false,
        };
        pair.dispatch_event(normal, &mut handler).unwrap();
        assert!(handler.0.is_empty());
        assert_eq!(pair.process_incoming_requests(&mut handler), Ok(1));
        assert_eq!(
            handler.0,
            [(ACTION_CONTEXT_RESET_NOTIFICATION, 2, vec![0xaa])]
        );

        let tlb = CtbEvent {
            action: ACTION_TLB_INVALIDATION_DONE,
            data0: 0,
            payload: &[0xbb],
            released_response_credit: 1,
            process_immediately: true,
        };
        pair.dispatch_event(tlb, &mut handler).unwrap();
        assert_eq!(handler.0.len(), 2);
        assert_eq!(handler.0[1], (ACTION_TLB_INVALIDATION_DONE, 0, vec![0xbb]));
        assert_eq!(pair.process_incoming_requests(&mut handler), Ok(0));
    }

    #[test]
    fn send_busy_loop_retries_only_busy_and_obeys_context_sleep_policy() {
        let mut wait = BusyWaitTrace::default();
        let mut attempts = 0;
        send_busy_loop(&mut wait, true, true, || {
            attempts += 1;
            if attempts < 4 {
                Err(CtError::NoRoom)
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(attempts, 4);
        assert_eq!(wait.sleeps, [1, 2, 4]);

        let mut wait = BusyWaitTrace::default();
        let mut attempts = 0;
        send_busy_loop(&mut wait, false, true, || {
            attempts += 1;
            if attempts == 1 {
                Err(CtError::NoRoom)
            } else {
                Err(CtError::Failure { error: 1, hint: 0 })
            }
        })
        .unwrap_err();
        assert_eq!(wait.relaxes, 1);
        assert!(wait.sleeps.is_empty());
    }

    #[test]
    fn ctb_pair_reserves_expected_responses_and_wraps_fences() {
        let mut pair = CtbPair::new().unwrap();
        pair.next_fence = u16::MAX - 1;
        assert_eq!(pair.send_nonblocking(&[0x1001], 2), Ok(u16::MAX));
        assert_eq!(pair.receive.available_dwords(), 3071 - 4);
        assert_eq!(pair.send_nonblocking(&[0x1002], CT_SEND_NB | 1), Ok(0));
        assert_eq!(pair.receive.available_dwords(), 3071 - 7);
        pair.release_response_space(4).unwrap();
        assert_eq!(pair.receive.available_dwords(), 3071 - 3);
        assert_eq!(pair.release_response_space(4), Err(CtError::InvalidMessage));
    }

    #[test]
    fn ctb_synchronous_request_tracks_response_and_retry_status() {
        let mut pair = CtbPair::new().unwrap();
        let fence = pair.send_request(&[0x4000, 0x1234], 1).unwrap();
        assert_eq!(
            pair.receive.available_dwords(),
            3071 - CTB_MSG_MAX_LEN as u32
        );
        let response = [
            ctb_message_header(fence, 2),
            HXG_ORIGIN_GUC | HXG_TYPE_RESPONSE_SUCCESS | 0x12,
            0xfeed_beef,
        ];
        pair.handle_response(parse_message(&response).unwrap())
            .unwrap();
        assert_eq!(
            pair.finish_request(fence),
            Ok(CtCompletion::Success {
                data0: 0x12,
                payload: vec![0xfeed_beef],
                truncated: false,
            })
        );
        assert_eq!(pair.receive.available_dwords(), 3071);

        let retry_fence = pair.send_request(&[0x4001], 0).unwrap();
        let retry = [
            ctb_message_header(retry_fence, 1),
            HXG_ORIGIN_GUC | HXG_TYPE_NO_RESPONSE_RETRY | 3,
        ];
        pair.handle_response(parse_message(&retry).unwrap())
            .unwrap();
        assert_eq!(
            pair.finish_request(retry_fence),
            Ok(CtCompletion::Retry { reason: 3 })
        );
    }

    #[test]
    fn synchronous_send_resends_retry_response_with_new_fence_and_timeout_policy() {
        let mut pair = CtbPair::new().unwrap();
        let io = ClockIo(Cell::new(0));
        let mut fences = Vec::new();
        let mut replies = 0;
        let completion = pair
            .send_request_with_retry(&io, &[0x4000, 0x1234], 0, |pair, fence, short, long| {
                assert_eq!(short, CTB_RESPONSE_TIMEOUT_SHORT_US);
                assert_eq!(long, CTB_RESPONSE_TIMEOUT_LONG_US);
                let fence = fence.ok_or(CtError::InvalidMessage)?;
                fences.push(fence);
                let (hxg_type, aux) = if replies == 0 {
                    (HxgType::NoResponseRetry, 7)
                } else {
                    (HxgType::ResponseSuccess, 9)
                };
                replies += 1;
                pair.handle_response(CtbMessage {
                    fence,
                    hxg_type,
                    aux,
                    payload: &[],
                })
            })
            .unwrap();
        assert_eq!(fences, [1, 2]);
        assert_eq!(
            completion,
            CtCompletion::Success {
                data0: 9,
                payload: vec![],
                truncated: false,
            }
        );
    }
}
