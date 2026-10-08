// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_submission.c and
// intel_guc_fwif.h: context scheduling state, v70 work queue and registration ABI.
// Copyright © 2014-2019 Intel Corporation. Full MIT grant: ../LICENSE-MIT.

use alloc::vec::Vec;
#[cfg(test)]
use core::mem::size_of;

use crate::Error;

pub const PARENT_SCRATCH_SIZE: usize = 4096;
pub const CACHELINE_BYTES: usize = 64;
pub const WQ_SIZE: usize = PARENT_SCRATCH_SIZE / 2;
pub const WQ_OFFSET: usize = PARENT_SCRATCH_SIZE - WQ_SIZE;
pub const WQ_TYPE_BATCH_BUF: u32 = 1;
pub const WQ_TYPE_PSEUDO: u32 = 2;
pub const WQ_TYPE_INORDER: u32 = 3;
pub const WQ_TYPE_NOOP: u32 = 4;
pub const WQ_TYPE_MULTI_LRC: u32 = 5;
pub const WQ_TYPE_MASK: u32 = 0xff;
pub const WQ_LEN_MASK: u32 = 0x07ff_0000;
pub const WQ_GUC_ID_MASK: u32 = 0x0000_ffff;
pub const WQ_RING_TAIL_MASK: u32 = 0x1ffc_0000;
pub const CONTEXT_REGISTRATION_FLAG_KMD: u32 = 1;
pub const CONTEXT_POLICY_FLAG_PREEMPT_TO_IDLE_V69: u32 = 1;
pub const GUC_INVALID_CONTEXT_ID: u32 = u32::MAX;

const SCHED_STATE_WAIT_FOR_DEREGISTER_TO_REGISTER: u32 = 1 << 0;
const SCHED_STATE_DESTROYED: u32 = 1 << 1;
const SCHED_STATE_PENDING_DISABLE: u32 = 1 << 2;
const SCHED_STATE_BANNED: u32 = 1 << 3;
const SCHED_STATE_ENABLED: u32 = 1 << 4;
const SCHED_STATE_PENDING_ENABLE: u32 = 1 << 5;
const SCHED_STATE_REGISTERED: u32 = 1 << 6;
const SCHED_STATE_POLICY_REQUIRED: u32 = 1 << 7;
const SCHED_STATE_CLOSED: u32 = 1 << 8;
const SCHED_STATE_BLOCKED_SHIFT: u32 = 9;
const SCHED_STATE_BLOCKED: u32 = 1 << SCHED_STATE_BLOCKED_SHIFT;
const SCHED_STATE_BLOCKED_MASK: u32 = 0xfff << SCHED_STATE_BLOCKED_SHIFT;
const SCHED_STATE_VALID_INIT: u32 =
    SCHED_STATE_BLOCKED_MASK | SCHED_STATE_CLOSED | SCHED_STATE_REGISTERED;

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct GuCWorkQueueItem {
    pub header: u32,
    pub context_desc: u32,
    pub submit_element_info: u32,
    pub fence_id: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct GuCProcessDescV69 {
    pub stage_id: u32,
    pub db_base_addr: u64,
    pub head: u32,
    pub tail: u32,
    pub error_offset: u32,
    pub wq_base_addr: u64,
    pub wq_size_bytes: u32,
    pub wq_status: u32,
    pub engine_presence: u32,
    pub priority: u32,
    pub reserved: [u32; 36],
}

impl Default for GuCProcessDescV69 {
    fn default() -> Self {
        Self {
            stage_id: 0,
            db_base_addr: 0,
            head: 0,
            tail: 0,
            error_offset: 0,
            wq_base_addr: 0,
            wq_size_bytes: 0,
            wq_status: 0,
            engine_presence: 0,
            priority: 0,
            reserved: [0; 36],
        }
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct GuCSchedWqDesc {
    pub head: u32,
    pub tail: u32,
    pub error_offset: u32,
    pub wq_status: u32,
    pub reserved: [u32; 28],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct GuCContextRegistrationInfo {
    pub flags: u32,
    pub context_idx: u32,
    pub engine_class: u32,
    pub engine_submit_mask: u32,
    pub wq_desc_lo: u32,
    pub wq_desc_hi: u32,
    pub wq_base_lo: u32,
    pub wq_base_hi: u32,
    pub wq_size: u32,
    pub hwlrca_lo: u32,
    pub hwlrca_hi: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct GuCLrcDescV69 {
    pub hw_context_desc: u32,
    pub slpm_perf_mode_hint: u32,
    pub slpm_freq_hint: u32,
    pub engine_submit_mask: u32,
    pub engine_class: u8,
    pub reserved0: [u8; 3],
    pub priority: u32,
    pub process_desc: u32,
    pub wq_addr: u32,
    pub wq_size: u32,
    pub context_flags: u32,
    pub execution_quantum: u32,
    pub preemption_timeout: u32,
    pub policy_flags: u32,
    pub reserved1: [u32; 19],
}

/// Linux engine-class enum order to GuC engine-class order.
pub const fn engine_class_to_guc_class(engine_class: u8) -> Result<u8, Error> {
    match engine_class {
        0 => Ok(0), // render
        1 => Ok(3), // copy / blitter
        2 => Ok(1), // video decode
        3 => Ok(2), // video enhancement
        4 => Ok(4), // compute
        5 => Ok(5), // GSC / other
        _ => Err(Error::Refused),
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextSchedState(u32);

impl ContextSchedState {
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    /// upstream: intel_guc_submission.c init_sched_state().
    /// Preserve nested blocked count, clear all other scheduling state.
    pub fn init(&mut self) {
        self.0 &= SCHED_STATE_BLOCKED_MASK;
    }

    /// upstream: intel_guc_submission.c sched_state_is_init().
    pub const fn is_init(self) -> bool {
        self.0 & !SCHED_STATE_VALID_INIT == 0
    }

    const fn get(self, flag: u32) -> bool {
        self.0 & flag != 0
    }

    fn set(&mut self, flag: u32) {
        self.0 |= flag;
    }

    fn clear(&mut self, flag: u32) {
        self.0 &= !flag;
    }

    /// upstream: intel_guc_submission.c context_wait_for_deregister_to_register().
    pub fn wait_for_deregister_to_register(self) -> bool {
        self.get(SCHED_STATE_WAIT_FOR_DEREGISTER_TO_REGISTER)
    }
    /// upstream: intel_guc_submission.c set_context_wait_for_deregister_to_register().
    pub fn set_wait_for_deregister_to_register(&mut self) {
        self.set(SCHED_STATE_WAIT_FOR_DEREGISTER_TO_REGISTER);
    }
    /// upstream: intel_guc_submission.c clr_context_wait_for_deregister_to_register().
    pub fn clear_wait_for_deregister_to_register(&mut self) {
        self.clear(SCHED_STATE_WAIT_FOR_DEREGISTER_TO_REGISTER);
    }
    /// upstream: intel_guc_submission.c context_destroyed().
    pub fn destroyed(self) -> bool {
        self.get(SCHED_STATE_DESTROYED)
    }
    /// upstream: intel_guc_submission.c set_context_destroyed().
    pub fn set_destroyed(&mut self) {
        self.set(SCHED_STATE_DESTROYED);
    }
    /// upstream: intel_guc_submission.c clr_context_destroyed().
    pub fn clear_destroyed(&mut self) {
        self.clear(SCHED_STATE_DESTROYED);
    }
    /// upstream: intel_guc_submission.c context_pending_disable().
    pub fn pending_disable(self) -> bool {
        self.get(SCHED_STATE_PENDING_DISABLE)
    }
    /// upstream: intel_guc_submission.c set_context_pending_disable().
    pub fn set_pending_disable(&mut self) {
        self.set(SCHED_STATE_PENDING_DISABLE);
    }
    /// upstream: intel_guc_submission.c clr_context_pending_disable().
    pub fn clear_pending_disable(&mut self) {
        self.clear(SCHED_STATE_PENDING_DISABLE);
    }
    /// upstream: intel_guc_submission.c context_banned().
    pub fn banned(self) -> bool {
        self.get(SCHED_STATE_BANNED)
    }
    /// upstream: intel_guc_submission.c set_context_banned().
    pub fn set_banned(&mut self) {
        self.set(SCHED_STATE_BANNED);
    }
    /// upstream: intel_guc_submission.c clr_context_banned().
    pub fn clear_banned(&mut self) {
        self.clear(SCHED_STATE_BANNED);
    }
    /// upstream: intel_guc_submission.c context_enabled().
    pub fn enabled(self) -> bool {
        self.get(SCHED_STATE_ENABLED)
    }
    /// upstream: intel_guc_submission.c set_context_enabled().
    pub fn set_enabled(&mut self) {
        self.set(SCHED_STATE_ENABLED);
    }
    /// upstream: intel_guc_submission.c clr_context_enabled().
    pub fn clear_enabled(&mut self) {
        self.clear(SCHED_STATE_ENABLED);
    }
    /// upstream: intel_guc_submission.c context_pending_enable().
    pub fn pending_enable(self) -> bool {
        self.get(SCHED_STATE_PENDING_ENABLE)
    }
    /// upstream: intel_guc_submission.c set_context_pending_enable().
    pub fn set_pending_enable(&mut self) {
        self.set(SCHED_STATE_PENDING_ENABLE);
    }
    /// upstream: intel_guc_submission.c clr_context_pending_enable().
    pub fn clear_pending_enable(&mut self) {
        self.clear(SCHED_STATE_PENDING_ENABLE);
    }
    /// upstream: intel_guc_submission.c context_registered().
    pub fn registered(self) -> bool {
        self.get(SCHED_STATE_REGISTERED)
    }
    /// upstream: intel_guc_submission.c set_context_registered().
    pub fn set_registered(&mut self) {
        self.set(SCHED_STATE_REGISTERED);
    }
    /// upstream: intel_guc_submission.c clr_context_registered().
    pub fn clear_registered(&mut self) {
        self.clear(SCHED_STATE_REGISTERED);
    }
    /// upstream: intel_guc_submission.c context_policy_required().
    pub fn policy_required(self) -> bool {
        self.get(SCHED_STATE_POLICY_REQUIRED)
    }
    /// upstream: intel_guc_submission.c set_context_policy_required().
    pub fn set_policy_required(&mut self) {
        self.set(SCHED_STATE_POLICY_REQUIRED);
    }
    /// upstream: intel_guc_submission.c clr_context_policy_required().
    pub fn clear_policy_required(&mut self) {
        self.clear(SCHED_STATE_POLICY_REQUIRED);
    }
    /// upstream: intel_guc_submission.c context_close_done().
    pub fn close_done(self) -> bool {
        self.get(SCHED_STATE_CLOSED)
    }
    /// upstream: intel_guc_submission.c set_context_close_done().
    pub fn set_close_done(&mut self) {
        self.set(SCHED_STATE_CLOSED);
    }

    /// upstream: intel_guc_submission.c context_blocked().
    pub const fn blocked(self) -> u32 {
        (self.0 & SCHED_STATE_BLOCKED_MASK) >> SCHED_STATE_BLOCKED_SHIFT
    }

    /// upstream: intel_guc_submission.c incr_context_blocked().
    pub fn increment_blocked(&mut self) -> Result<u32, Error> {
        if self.blocked() == 0xfff {
            return Err(Error::Refused);
        }
        self.0 += SCHED_STATE_BLOCKED;
        Ok(self.blocked())
    }

    /// upstream: intel_guc_submission.c decr_context_blocked().
    pub fn decrement_blocked(&mut self) -> Result<u32, Error> {
        if self.blocked() == 0 {
            return Err(Error::Refused);
        }
        self.0 -= SCHED_STATE_BLOCKED;
        Ok(self.blocked())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MultiLrcItem {
    pub context_desc: u32,
    pub guc_id: u16,
    /// Ring tail in 64-bit units as encoded in WQ_RING_TAIL_MASK.
    pub ring_tail: u16,
    pub fence_id: u32,
    pub child_tails: [u32; 32],
    pub child_count: u8,
}

/// A model of intel_guc_submission.c's v70 circular work queue. Head is
/// supplied by GuC and tail is host-owned; the last dword remains empty.
pub struct WorkQueue {
    dwords: Vec<u32>,
    head: usize,
    tail: usize,
    published_tail: usize,
}

impl WorkQueue {
    pub fn new() -> Result<Self, Error> {
        let mut dwords = Vec::new();
        dwords
            .try_reserve_exact(WQ_SIZE / 4)
            .map_err(|_| Error::Refused)?;
        dwords.resize(WQ_SIZE / 4, 0);
        Ok(Self {
            dwords,
            head: 0,
            tail: 0,
            published_tail: 0,
        })
    }

    pub const fn tail(&self) -> usize {
        self.tail
    }
    pub const fn published_tail(&self) -> usize {
        self.published_tail
    }

    fn space(&self) -> usize {
        (self.head + WQ_SIZE - self.tail - 4) & (WQ_SIZE - 1)
    }

    /// upstream: intel_guc_submission.c get_wq_pointer().
    fn reserve(&mut self, bytes: usize, peer_head: usize) -> Result<usize, Error> {
        if bytes == 0 || bytes > WQ_SIZE - 4 || !bytes.is_multiple_of(4) {
            return Err(Error::Refused);
        }
        if bytes > self.space() {
            if peer_head >= WQ_SIZE || !peer_head.is_multiple_of(4) {
                return Err(Error::Refused);
            }
            self.head = peer_head;
        }
        if bytes > self.space() {
            Err(Error::Unavailable(0))
        } else {
            Ok(self.tail / 4)
        }
    }

    /// upstream: intel_guc_submission.c write_wqi(). Caller publishes only
    /// after the item dwords have been written (release fence at the adapter).
    fn publish(&mut self, bytes: usize) {
        self.tail = (self.tail + bytes) & (WQ_SIZE - 1);
        self.published_tail = self.tail;
    }

    /// upstream: intel_guc_submission.c guc_wq_noop_append().
    fn append_noop(&mut self, peer_head: usize) -> Result<(), Error> {
        let until_wrap = WQ_SIZE - self.tail;
        let slot = self.reserve(until_wrap, peer_head)?;
        let len_dwords = until_wrap / 4 - 1;
        if len_dwords > (WQ_LEN_MASK >> 16) as usize {
            return Err(Error::Refused);
        }
        self.dwords[slot] = (WQ_TYPE_NOOP & WQ_TYPE_MASK) | ((len_dwords as u32) << 16);
        self.tail = 0; // The wrapped item publishes its tail after its payload.
        Ok(())
    }

    /// upstream: intel_guc_submission.c __guc_wq_item_append() multi-LRC
    /// portion. Context/register state checks are performed by the caller.
    pub fn append_multi_lrc(
        &mut self,
        item: MultiLrcItem,
        peer_head: usize,
    ) -> Result<usize, Error> {
        let child_count = usize::from(item.child_count);
        if child_count > item.child_tails.len() || item.guc_id == u16::MAX || item.ring_tail > 0x7ff
        {
            return Err(Error::Refused);
        }
        let bytes = (child_count + 4).checked_mul(4).ok_or(Error::Refused)?;
        if bytes > WQ_SIZE - 4 {
            return Err(Error::Refused);
        }
        if bytes > WQ_SIZE - self.tail {
            self.append_noop(peer_head)?;
        }
        let slot = self.reserve(bytes, peer_head)?;
        let len_dwords = bytes / 4 - 1;
        if len_dwords as u32 > WQ_LEN_MASK >> 16 {
            return Err(Error::Refused);
        }
        self.dwords[slot] = WQ_TYPE_MULTI_LRC | ((len_dwords as u32) << 16);
        self.dwords[slot + 1] = item.context_desc;
        self.dwords[slot + 2] = u32::from(item.guc_id) | (u32::from(item.ring_tail) << 18);
        self.dwords[slot + 3] = item.fence_id;
        for (index, tail) in item.child_tails[..child_count].iter().copied().enumerate() {
            self.dwords[slot + 4 + index] = tail;
        }
        self.publish(bytes);
        Ok(self.published_tail)
    }

    pub fn update_peer_head(&mut self, head: usize) -> Result<(), Error> {
        if head >= WQ_SIZE || !head.is_multiple_of(4) {
            return Err(Error::Refused);
        }
        self.head = head;
        Ok(())
    }

    pub fn snapshot(&self) -> &[u32] {
        &self.dwords
    }
}

/// upstream: intel_guc_submission.c __get_parent_scratch_offset().
pub fn parent_scratch_offset(page_index: u32) -> Result<u32, Error> {
    page_index
        .checked_mul(PARENT_SCRATCH_SIZE as u32)
        .ok_or(Error::Refused)
}

/// upstream: intel_guc_submission.c __get_wq_offset().
pub fn work_queue_offset(page_index: u32) -> Result<u32, Error> {
    parent_scratch_offset(page_index)?
        .checked_add(WQ_OFFSET as u32)
        .ok_or(Error::Refused)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guc_submission_abi_structures_and_class_mapping_match_v70() {
        assert_eq!(size_of::<GuCWorkQueueItem>(), 16);
        assert_eq!(size_of::<GuCProcessDescV69>(), 192);
        assert_eq!(size_of::<GuCSchedWqDesc>(), 128);
        assert_eq!(size_of::<GuCContextRegistrationInfo>(), 44);
        assert_eq!(size_of::<GuCLrcDescV69>(), 128);
        assert_eq!(engine_class_to_guc_class(1), Ok(3));
        assert_eq!(engine_class_to_guc_class(2), Ok(1));
        assert_eq!(engine_class_to_guc_class(6), Err(Error::Refused));
    }

    #[test]
    fn sched_state_preserves_nested_blocks_and_checks_transition_counts() {
        let mut state = ContextSchedState::default();
        state.increment_blocked().unwrap();
        state.set_enabled();
        state.set_registered();
        state.init();
        assert_eq!(state.blocked(), 1);
        assert!(!state.enabled());
        assert!(!state.registered());
        assert!(state.is_init());
        assert_eq!(state.decrement_blocked(), Ok(0));
        assert_eq!(state.decrement_blocked(), Err(Error::Refused));
        let mut full = ContextSchedState::from_bits(0xfff << SCHED_STATE_BLOCKED_SHIFT);
        assert_eq!(full.increment_blocked(), Err(Error::Refused));
    }

    #[test]
    fn multi_lrc_work_queue_noops_before_wrap_and_publishes_after_payload() {
        let mut queue = WorkQueue::new().unwrap();
        for _ in 0..127 {
            queue
                .append_multi_lrc(
                    MultiLrcItem {
                        context_desc: 0x1000,
                        guc_id: 7,
                        ring_tail: 12,
                        fence_id: 0,
                        child_tails: [0; 32],
                        child_count: 0,
                    },
                    0,
                )
                .unwrap();
        }
        assert_eq!(queue.tail(), 127 * 16);
        queue.update_peer_head(512).unwrap();
        let tail = queue
            .append_multi_lrc(
                MultiLrcItem {
                    context_desc: 0x2000,
                    guc_id: 8,
                    ring_tail: 16,
                    fence_id: 0,
                    child_tails: [0x55; 32],
                    child_count: 32,
                },
                0,
            )
            .unwrap();
        assert_eq!(tail, 144);
        assert_eq!(
            queue.snapshot()[WQ_SIZE / 4 - 4] & WQ_TYPE_MASK,
            WQ_TYPE_NOOP
        );
        assert_eq!(queue.snapshot()[0] & WQ_TYPE_MASK, WQ_TYPE_MULTI_LRC);
        assert_eq!(queue.snapshot()[1], 0x2000);
        assert_eq!(queue.snapshot()[4], 0x55);
    }
}
