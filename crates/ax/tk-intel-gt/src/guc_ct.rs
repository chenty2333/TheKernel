// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_ct.c and ABI headers
// guc_communication_ctb_abi.h, guc_communication_mmio_abi.h, guc_messages_abi.h.
// Copyright © 2016-2019 Intel Corporation.
// Copyright © 2014-2021 Intel Corporation. Full MIT grant: ../LICENSE-MIT.

use alloc::vec::Vec;
use core::sync::atomic::{Ordering, fence as atomic_fence};

pub const CTB_DESC_SIZE: usize = 2048;
pub const CTB_H2G_BUFFER_SIZE: usize = 4096;
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
        || (words[0] & CTB_MSG_0_RESERVED_MASK) != 0
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
    next_fence: u16,
    pending: Vec<PendingRequest>,
}

impl CtbPair {
    pub fn new() -> Result<Self, CtError> {
        Ok(Self {
            send: CtbBuffer::new(CTB_H2G_BUFFER_SIZE, 0)?,
            receive: CtbBuffer::new(CTB_G2H_BUFFER_SIZE, G2H_ROOM_BUFFER_SIZE)?,
            next_fence: 0,
            pending: Vec::new(),
        })
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

    pub fn release_response_space(&mut self, dwords: u32) -> Result<(), CtError> {
        self.receive.release_response_space(dwords)
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

    /// Consume a completed request and release the full maximum-size G2H
    /// reservation, matching ct_send() after its wait finishes.
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

/// upstream: intel_guc_ct.c intel_guc_ct_max_queue_time_jiffies().
pub const fn max_queue_time_ms() -> u32 {
    (CTB_H2G_BUFFER_SIZE as u32 * 1000) / 2048
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn ctb_abi_struct_layout_and_blob_sizes_match_upstream() {
        assert_eq!(core::mem::size_of::<CtbDescriptor>(), 64);
        assert_eq!(CTB_DESC_SIZE, 2048);
        assert_eq!(CTB_BLOB_SIZE, 24 * 1024);
        assert_eq!(max_queue_time_ms(), 2000);
        assert_eq!(ctb_message_header(0x1234, 2), (0x1234 << 16) | 2);
        assert_eq!(hxg_request_header(0x1234, false), 0x1234);
        assert_eq!(
            hxg_request_header(0x1234, true),
            HXG_TYPE_FAST_REQUEST | 0x1234
        );
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
}
