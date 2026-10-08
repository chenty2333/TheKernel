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
pub const GUC_MAX_CONTEXT_ID: usize = 65_535;
pub const ACTION_SCHED_CONTEXT: u32 = 0x1000;
pub const ACTION_SCHED_CONTEXT_MODE_SET: u32 = 0x1001;
pub const ACTION_UPDATE_CONTEXT_POLICIES: u32 = 0x100b;
pub const ACTION_REGISTER_CONTEXT: u32 = 0x4502;
pub const ACTION_DEREGISTER_CONTEXT: u32 = 0x4503;
pub const ACTION_REGISTER_CONTEXT_MULTI_LRC: u32 = 0x4601;
pub const CONTEXT_ENABLE: u32 = 1;
pub const CONTEXT_DISABLE: u32 = 0;
pub const G2H_LEN_DW_SCHED_CONTEXT_MODE_SET: u8 = 2;
pub const G2H_LEN_DW_DEREGISTER_CONTEXT: u8 = 1;
pub const CONTEXT_POLICY_KLV_EXECUTION_QUANTUM: u16 = 0x2001;
pub const CONTEXT_POLICY_KLV_PREEMPTION_TIMEOUT: u16 = 0x2002;
pub const CONTEXT_POLICY_KLV_SCHEDULING_PRIORITY: u16 = 0x2003;
pub const CONTEXT_POLICY_KLV_PREEMPT_TO_IDLE: u16 = 0x2004;
pub const CONTEXT_POLICY_KLV_SLPM_GT_FREQUENCY: u16 = 0x2005;
pub const GUC_CLIENT_PRIORITY_KMD_HIGH: u8 = 0;
pub const GUC_CLIENT_PRIORITY_HIGH: u8 = 1;
pub const GUC_CLIENT_PRIORITY_KMD_NORMAL: u8 = 2;
pub const GUC_CLIENT_PRIORITY_NORMAL: u8 = 3;
const GEN12_CTX_PRIORITY_MASK: u32 = 3 << 9;

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
/// upstream: intel_guc_fwif.h engine_class_to_guc_class().
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

    /// upstream: intel_guc_submission.c __guc_add_request() scheduling choice.
    pub fn prepare_request(
        &mut self,
        context_id: u32,
        is_parent: bool,
    ) -> Result<SchedPlan, Error> {
        if context_id == GUC_INVALID_CONTEXT_ID {
            return Err(Error::Refused);
        }
        let mut plan = SchedPlan::default();
        if self.blocked() != 0 && !is_parent {
            return Ok(plan);
        }
        let already_enabled = self.enabled() || self.blocked() != 0;
        if already_enabled {
            plan.push(SchedAction::new(
                [ACTION_SCHED_CONTEXT, context_id, 0],
                2,
                0,
            ))?;
        } else {
            self.set_pending_enable();
            plan.push(SchedAction::new(
                [ACTION_SCHED_CONTEXT_MODE_SET, context_id, CONTEXT_ENABLE],
                3,
                G2H_LEN_DW_SCHED_CONTEXT_MODE_SET,
            ))?;
            if is_parent {
                plan.push(SchedAction::new(
                    [ACTION_SCHED_CONTEXT, context_id, 0],
                    2,
                    0,
                ))?;
            }
        }
        Ok(plan)
    }

    /// upstream: intel_guc_submission.c __guc_add_request() send completion.
    pub fn request_send_result(&mut self, plan: SchedPlan, first_action_succeeded: bool) {
        if plan.count != 0 && plan.actions[0].words[0] == ACTION_SCHED_CONTEXT_MODE_SET {
            if first_action_succeeded {
                self.set_enabled();
            } else {
                self.clear_pending_enable();
            }
        }
    }

    /// upstream: intel_guc_submission.c sched-context-mode completion event.
    pub fn sched_mode_done(&mut self, enabled: bool) {
        if enabled {
            self.clear_pending_enable();
        } else {
            self.clear_pending_disable();
            self.clear_enabled();
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SchedAction {
    pub words: [u32; 3],
    pub len: u8,
    pub expected_response_dwords: u8,
}

impl SchedAction {
    const fn new(words: [u32; 3], len: u8, expected_response_dwords: u8) -> Self {
        Self {
            words,
            len,
            expected_response_dwords,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SchedPlan {
    pub actions: [SchedAction; 2],
    pub count: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextIdLease {
    Single(u32),
    Multi(ContextIdRange),
}

struct SubmittedContext {
    info: GuCContextRegistrationInfo,
    policy: ContextPolicy,
    state: ContextSchedState,
    lease: ContextIdLease,
    registration_action: Vec<u32>,
}

/// Stateful GuC submission facade over CTB. The GT owner supplies the live
/// shared blob synchronization and MMIO notification around these operations.
pub struct GucSubmission {
    ids: ContextIdPool,
    contexts: Vec<SubmittedContext>,
}

impl GucSubmission {
    /// upstream: intel_guc_submission.c intel_guc_submission_init().
    pub fn new(total_ids: usize) -> Result<Self, Error> {
        Ok(Self {
            ids: ContextIdPool::new(total_ids)?,
            contexts: Vec::new(),
        })
    }

    pub fn allocate_single_id(&mut self) -> Result<ContextIdLease, Error> {
        self.ids.allocate_single().map(ContextIdLease::Single)
    }

    pub fn allocate_multi_ids(&mut self, child_count: usize) -> Result<ContextIdLease, Error> {
        self.ids
            .allocate_multi(child_count)
            .map(ContextIdLease::Multi)
    }

    pub fn release_unregistered_lease(&mut self, lease: ContextIdLease) -> Result<(), Error> {
        match lease {
            ContextIdLease::Single(id) => self.ids.release_single(id),
            ContextIdLease::Multi(range) => self.ids.release_multi(range),
        }
    }

    /// upstream: intel_guc_submission.c register_context_v70() and
    /// guc_context_policy_init_v70(). Context is retained even if the policy
    /// update fails, mirroring the upstream registered-but-policy-required state.
    pub fn register_v70(
        &mut self,
        pair: &mut crate::guc_ct::CtbPair,
        info: GuCContextRegistrationInfo,
        policy: ContextPolicy,
        lease: ContextIdLease,
        child_lrcas: &[u64],
        child_ids: &[u32],
    ) -> Result<u16, crate::guc_ct::CtError> {
        let lease_matches = match lease {
            ContextIdLease::Single(id) => id == info.context_idx && child_lrcas.is_empty(),
            ContextIdLease::Multi(range) => {
                range.base == info.context_idx
                    && range.count as usize == child_lrcas.len() + 1
                    && child_lrcas.len() == child_ids.len()
            }
        };
        if policy.context_id != info.context_idx
            || !lease_matches
            || self
                .contexts
                .iter()
                .any(|context| context.info.context_idx == info.context_idx)
        {
            return Err(crate::guc_ct::CtError::InvalidMessage);
        }
        self.contexts
            .try_reserve(1)
            .map_err(|_| crate::guc_ct::CtError::NoRoom)?;
        let action = if child_lrcas.is_empty() {
            register_context_action(info).to_vec()
        } else {
            register_multi_context_action_v70(info, child_lrcas, child_ids)
                .map_err(|_| crate::guc_ct::CtError::InvalidMessage)?
        };
        let fence = pair.send_nonblocking(&action, crate::guc_ct::CT_SEND_NB)?;
        let mut state = ContextSchedState::default();
        state.set_registered();
        let context = SubmittedContext {
            info,
            policy,
            state,
            lease,
            registration_action: action,
        };
        self.contexts.push(context);
        let context_index = self
            .contexts
            .len()
            .checked_sub(1)
            .ok_or(crate::guc_ct::CtError::InvalidMessage)?;
        let context = self
            .contexts
            .get_mut(context_index)
            .ok_or(crate::guc_ct::CtError::InvalidMessage)?;
        let policy_action = match context_policy_action(policy) {
            Ok(action) => action,
            Err(_) => {
                context.state.set_policy_required();
                return Err(crate::guc_ct::CtError::InvalidMessage);
            }
        };
        if let Err(error) = pair.send_nonblocking(&policy_action, crate::guc_ct::CT_SEND_NB) {
            context.state.set_policy_required();
            return Err(error);
        }
        Ok(fence)
    }

    /// upstream: intel_guc_submission.c __guc_add_request().
    pub fn submit_request(
        &mut self,
        pair: &mut crate::guc_ct::CtbPair,
        context_id: u32,
        is_parent: bool,
    ) -> Result<([u16; 2], usize), crate::guc_ct::CtError> {
        let context = self
            .contexts
            .iter_mut()
            .find(|context| context.info.context_idx == context_id)
            .ok_or(crate::guc_ct::CtError::UnexpectedResponse)?;
        if context.state.policy_required() {
            let action = context_policy_action(context.policy)
                .map_err(|_| crate::guc_ct::CtError::InvalidMessage)?;
            pair.send_nonblocking(&action, crate::guc_ct::CT_SEND_NB)?;
            context.state.clear_policy_required();
        }
        let plan = context
            .state
            .prepare_request(context_id, is_parent)
            .map_err(|_| crate::guc_ct::CtError::InvalidMessage)?;
        let mut fences = [0; 2];
        for index in 0..usize::from(plan.count) {
            let action = plan.actions[index];
            let len = usize::from(action.len);
            let flags = crate::guc_ct::CT_SEND_NB | u32::from(action.expected_response_dwords);
            match pair.send_nonblocking(&action.words[..len], flags) {
                Ok(fence) => fences[index] = fence,
                Err(error) => {
                    context.state.request_send_result(plan, index != 0);
                    return Err(error);
                }
            }
            if index == 0 {
                context.state.request_send_result(plan, true);
            }
        }
        Ok((fences, usize::from(plan.count)))
    }

    /// upstream: intel_guc_submission.c deregister_context() with G2H reply.
    pub fn deregister_context(
        &mut self,
        pair: &mut crate::guc_ct::CtbPair,
        context_id: u32,
    ) -> Result<u16, crate::guc_ct::CtError> {
        let context = self
            .contexts
            .iter_mut()
            .find(|context| context.info.context_idx == context_id)
            .ok_or(crate::guc_ct::CtError::UnexpectedResponse)?;
        context.state.set_destroyed();
        let action = deregister_context_action(context_id)
            .map_err(|_| crate::guc_ct::CtError::InvalidMessage)?;
        pair.send_nonblocking(
            &action,
            crate::guc_ct::CT_SEND_NB | u32::from(G2H_LEN_DW_DEREGISTER_CONTEXT),
        )
    }

    /// Prepare the corresponding context's reused ID for the deregister G2H
    /// path to register the new owner after the old owner has drained.
    pub fn wait_for_deregister_to_register(&mut self, context_id: u32) -> Result<(), Error> {
        let context = self
            .contexts
            .iter_mut()
            .find(|context| context.info.context_idx == context_id)
            .ok_or(Error::Refused)?;
        context.state.set_wait_for_deregister_to_register();
        Ok(())
    }

    /// upstream: intel_guc_submission.c G2H scheduling/deregister event
    /// processors. Event credits are released by `CtbPair` before this call.
    pub fn handle_event(
        &mut self,
        pair: &mut crate::guc_ct::CtbPair,
        event: crate::guc_ct::CtbEvent<'_>,
    ) -> Result<(), crate::guc_ct::CtError> {
        let context_id = *event
            .payload
            .first()
            .ok_or(crate::guc_ct::CtError::InvalidMessage)?;
        let index = self
            .contexts
            .iter()
            .position(|context| context.info.context_idx == context_id)
            .ok_or(crate::guc_ct::CtError::UnexpectedResponse)?;
        match event.action {
            crate::guc_ct::ACTION_SCHED_CONTEXT_MODE_DONE => {
                let context = &mut self.contexts[index];
                sched_mode_done_message(&mut context.state, event.payload)
                    .map_err(|_| crate::guc_ct::CtError::InvalidMessage)?;
            }
            crate::guc_ct::ACTION_DEREGISTER_CONTEXT_DONE => {
                let completion =
                    deregister_done_message(&mut self.contexts[index].state, event.payload)
                        .map_err(|_| crate::guc_ct::CtError::InvalidMessage)?;
                match completion {
                    DeregisterCompletion::ReRegister => {
                        let context = &mut self.contexts[index];
                        pair.send_nonblocking(
                            &context.registration_action,
                            crate::guc_ct::CT_SEND_NB,
                        )?;
                        context.state.set_registered();
                        let policy_action = context_policy_action(context.policy)
                            .map_err(|_| crate::guc_ct::CtError::InvalidMessage)?;
                        if pair
                            .send_nonblocking(&policy_action, crate::guc_ct::CT_SEND_NB)
                            .is_err()
                        {
                            context.state.set_policy_required();
                        } else {
                            context.state.clear_policy_required();
                        }
                    }
                    DeregisterCompletion::Destroy => {
                        let context = self.contexts.remove(index);
                        match context.lease {
                            ContextIdLease::Single(id) => self.ids.release_single(id),
                            ContextIdLease::Multi(range) => self.ids.release_multi(range),
                        }
                        .map_err(|_| crate::guc_ct::CtError::InvalidMessage)?;
                    }
                    DeregisterCompletion::Noop => {}
                }
            }
            _ => return Err(crate::guc_ct::CtError::InvalidMessage),
        }
        Ok(())
    }

    pub fn context_state(&self, context_id: u32) -> Option<ContextSchedState> {
        self.contexts
            .iter()
            .find(|context| context.info.context_idx == context_id)
            .map(|context| context.state)
    }

    pub fn context_count(&self) -> usize {
        self.contexts.len()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeregisterCompletion {
    ReRegister,
    Destroy,
    Noop,
}

/// upstream: intel_guc_submission.c intel_guc_sched_done_process_msg().
pub fn sched_mode_done_message(
    state: &mut ContextSchedState,
    message: &[u32],
) -> Result<bool, Error> {
    if message.len() < 2 || message[0] == GUC_INVALID_CONTEXT_ID {
        return Err(Error::Refused);
    }
    if state.destroyed() || (!state.pending_enable() && !state.pending_disable()) {
        return Err(Error::Refused);
    }
    if state.pending_enable() {
        state.clear_pending_enable();
        Ok(true)
    } else {
        state.clear_banned();
        state.clear_pending_disable();
        state.clear_enabled();
        Ok(false)
    }
}

/// upstream: intel_guc_submission.c intel_guc_deregister_done_process_msg().
pub fn deregister_done_message(
    state: &mut ContextSchedState,
    message: &[u32],
) -> Result<DeregisterCompletion, Error> {
    if message.is_empty() || message[0] == GUC_INVALID_CONTEXT_ID {
        return Err(Error::Refused);
    }
    if state.wait_for_deregister_to_register() {
        state.clear_wait_for_deregister_to_register();
        Ok(DeregisterCompletion::ReRegister)
    } else if state.destroyed() {
        Ok(DeregisterCompletion::Destroy)
    } else {
        Ok(DeregisterCompletion::Noop)
    }
}

impl SchedPlan {
    fn push(&mut self, action: SchedAction) -> Result<(), Error> {
        let slot = self
            .actions
            .get_mut(usize::from(self.count))
            .ok_or(Error::Refused)?;
        *slot = action;
        self.count += 1;
        Ok(())
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextIdRange {
    pub base: u32,
    pub count: u32,
}

/// Linux reserves a bitmap partition for contiguous multi-LRC IDs and an IDA
/// partition for single-LRC contexts. This allocator is caller-serialized.
pub struct ContextIdPool {
    multi_count: usize,
    multi_used: Vec<bool>,
    single_used: Vec<bool>,
}

impl ContextIdPool {
    /// upstream: intel_guc_submission.c intel_guc_submission_init() ID partitions.
    pub fn new(total_ids: usize) -> Result<Self, Error> {
        let multi_count = total_ids / 16;
        if total_ids <= multi_count || total_ids > GUC_MAX_CONTEXT_ID || multi_count == 0 {
            return Err(Error::Refused);
        }
        let mut multi_used = Vec::new();
        let mut single_used = Vec::new();
        multi_used
            .try_reserve_exact(multi_count)
            .map_err(|_| Error::Refused)?;
        single_used
            .try_reserve_exact(total_ids - multi_count)
            .map_err(|_| Error::Refused)?;
        multi_used.resize(multi_count, false);
        single_used.resize(total_ids - multi_count, false);
        Ok(Self {
            multi_count,
            multi_used,
            single_used,
        })
    }

    /// upstream: intel_guc_submission.c new_guc_id() parent bitmap branch.
    pub fn allocate_multi(&mut self, child_count: usize) -> Result<ContextIdRange, Error> {
        let count = child_count.checked_add(1).ok_or(Error::Refused)?;
        if count > self.multi_count {
            return Err(Error::Refused);
        }
        let block = count.checked_next_power_of_two().ok_or(Error::Refused)?;
        let mut base = 0usize;
        while base.checked_add(block).ok_or(Error::Refused)? <= self.multi_count {
            if self.multi_used[base..base + block].iter().all(|used| !used) {
                self.multi_used[base..base + block].fill(true);
                return Ok(ContextIdRange {
                    base: base as u32,
                    count: count as u32,
                });
            }
            base += block;
        }
        Err(Error::Unavailable(0))
    }

    /// upstream: intel_guc_submission.c new_guc_id() single-LRC IDA branch.
    pub fn allocate_single(&mut self) -> Result<u32, Error> {
        let index = self
            .single_used
            .iter()
            .position(|used| !used)
            .ok_or(Error::Unavailable(0))?;
        self.single_used[index] = true;
        u32::try_from(self.multi_count + index).map_err(|_| Error::Refused)
    }

    /// upstream: intel_guc_submission.c __release_guc_id().
    pub fn release_multi(&mut self, range: ContextIdRange) -> Result<(), Error> {
        let base = usize::try_from(range.base).map_err(|_| Error::Refused)?;
        let block = usize::try_from(range.count)
            .map_err(|_| Error::Refused)?
            .checked_next_power_of_two()
            .ok_or(Error::Refused)?;
        if range.count == 0
            || !base.is_multiple_of(block)
            || base + block > self.multi_count
            || self.multi_used[base..base + block].iter().any(|used| !used)
        {
            return Err(Error::Refused);
        }
        self.multi_used[base..base + block].fill(false);
        Ok(())
    }

    /// upstream: intel_guc_submission.c __release_guc_id() single-context branch.
    pub fn release_single(&mut self, id: u32) -> Result<(), Error> {
        let index = usize::try_from(id)
            .map_err(|_| Error::Refused)?
            .checked_sub(self.multi_count)
            .ok_or(Error::Refused)?;
        let used = self.single_used.get_mut(index).ok_or(Error::Refused)?;
        if !*used {
            return Err(Error::Refused);
        }
        *used = false;
        Ok(())
    }
}

/// upstream: intel_guc_submission.c prepare_context_registration_info_v70().
pub fn context_registration_info(
    context_id: u32,
    engine_class: u8,
    engine_submit_mask: u32,
    hwlrca: u64,
    guc_priority: Option<u8>,
    wq_descriptor: Option<u64>,
    wq_base: Option<u64>,
) -> Result<GuCContextRegistrationInfo, Error> {
    let guc_class = engine_class_to_guc_class(engine_class)?;
    if context_id == GUC_INVALID_CONTEXT_ID
        || engine_submit_mask == 0
        || wq_descriptor.is_some() != wq_base.is_some()
        || hwlrca & 0xfff != 0
    {
        return Err(Error::Refused);
    }
    let mut info = GuCContextRegistrationInfo {
        flags: CONTEXT_REGISTRATION_FLAG_KMD,
        context_idx: context_id,
        engine_class: u32::from(guc_class),
        engine_submit_mask,
        hwlrca_lo: hwlrca as u32,
        hwlrca_hi: (hwlrca >> 32) as u32,
        ..Default::default()
    };
    if let Some(priority) = guc_priority {
        info.hwlrca_lo |= map_guc_prio_to_lrc_desc_prio(priority)?;
    }
    if let (Some(desc), Some(base)) = (wq_descriptor, wq_base) {
        info.wq_desc_lo = desc as u32;
        info.wq_desc_hi = (desc >> 32) as u32;
        info.wq_base_lo = base as u32;
        info.wq_base_hi = (base >> 32) as u32;
        info.wq_size = WQ_SIZE as u32;
    }
    Ok(info)
}

/// upstream: intel_guc_submission.c map_guc_prio_to_lrc_desc_prio().
pub const fn map_guc_prio_to_lrc_desc_prio(priority: u8) -> Result<u32, Error> {
    let value = match priority {
        GUC_CLIENT_PRIORITY_KMD_HIGH | GUC_CLIENT_PRIORITY_HIGH => 2,
        GUC_CLIENT_PRIORITY_KMD_NORMAL => 1,
        GUC_CLIENT_PRIORITY_NORMAL => 0,
        _ => return Err(Error::Refused),
    };
    Ok((value << 9) & GEN12_CTX_PRIORITY_MASK)
}

/// upstream: intel_guc_submission.c __guc_action_register_context_v70().
pub fn register_context_action(info: GuCContextRegistrationInfo) -> [u32; 12] {
    [
        ACTION_REGISTER_CONTEXT,
        info.flags,
        info.context_idx,
        info.engine_class,
        info.engine_submit_mask,
        info.wq_desc_lo,
        info.wq_desc_hi,
        info.wq_base_lo,
        info.wq_base_hi,
        info.wq_size,
        info.hwlrca_lo,
        info.hwlrca_hi,
    ]
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextDescV69Input {
    pub engine_class: u8,
    pub engine_submit_mask: u32,
    pub hwlrca: u32,
    pub priority: u32,
    pub execution_quantum_ms: u32,
    pub preemption_timeout_ms: u32,
    pub preempt_to_idle: bool,
    pub process_desc: u32,
    pub wq_addr: u32,
    pub wq_size: u32,
}

/// upstream: intel_guc_submission.c guc_context_policy_init_v69().
pub fn guc_context_policy_init_v69(
    descriptor: &mut GuCLrcDescV69,
    execution_quantum_ms: u32,
    preemption_timeout_ms: u32,
    preempt_to_idle: bool,
) -> Result<(), Error> {
    descriptor.policy_flags = if preempt_to_idle {
        CONTEXT_POLICY_FLAG_PREEMPT_TO_IDLE_V69
    } else {
        0
    };
    descriptor.execution_quantum = execution_quantum_ms
        .checked_mul(1000)
        .ok_or(Error::Refused)?;
    descriptor.preemption_timeout = preemption_timeout_ms
        .checked_mul(1000)
        .ok_or(Error::Refused)?;
    Ok(())
}

/// upstream: intel_guc_submission.c prepare_context_registration_info_v69().
pub fn prepare_context_desc_v69(input: ContextDescV69Input) -> Result<GuCLrcDescV69, Error> {
    if input.engine_submit_mask == 0 || input.hwlrca == 0 || input.priority > 3 {
        return Err(Error::Refused);
    }
    let mut descriptor = GuCLrcDescV69 {
        hw_context_desc: input.hwlrca,
        engine_submit_mask: input.engine_submit_mask,
        engine_class: engine_class_to_guc_class(input.engine_class)?,
        priority: input.priority,
        process_desc: input.process_desc,
        wq_addr: input.wq_addr,
        wq_size: input.wq_size,
        context_flags: CONTEXT_REGISTRATION_FLAG_KMD,
        ..Default::default()
    };
    guc_context_policy_init_v69(
        &mut descriptor,
        input.execution_quantum_ms,
        input.preemption_timeout_ms,
        input.preempt_to_idle,
    )?;
    Ok(descriptor)
}

/// upstream: intel_guc_submission.c __guc_action_register_context_v69().
pub const fn register_context_action_v69(
    context_id: u32,
    descriptor_ggtt_offset: u32,
) -> Result<[u32; 3], Error> {
    if context_id == GUC_INVALID_CONTEXT_ID || descriptor_ggtt_offset == 0 {
        Err(Error::Refused)
    } else {
        Ok([ACTION_REGISTER_CONTEXT, context_id, descriptor_ggtt_offset])
    }
}

/// upstream: intel_guc_submission.c __guc_action_register_multi_lrc_v69().
pub fn register_multi_context_action_v69(
    context_id: u32,
    descriptor_offsets: &[u32],
) -> Result<Vec<u32>, Error> {
    if context_id == GUC_INVALID_CONTEXT_ID
        || descriptor_offsets.is_empty()
        || descriptor_offsets.len() > 33
        || descriptor_offsets.contains(&0)
    {
        return Err(Error::Refused);
    }
    let mut action = Vec::new();
    action
        .try_reserve_exact(3 + descriptor_offsets.len())
        .map_err(|_| Error::Refused)?;
    action.extend_from_slice(&[
        ACTION_REGISTER_CONTEXT_MULTI_LRC,
        context_id,
        descriptor_offsets.len() as u32,
    ]);
    action.extend_from_slice(descriptor_offsets);
    Ok(action)
}

/// upstream: intel_guc_submission.c __guc_action_register_multi_lrc_v70().
pub fn register_multi_context_action_v70(
    info: GuCContextRegistrationInfo,
    child_lrcas: &[u64],
    child_ids: &[u32],
) -> Result<Vec<u32>, Error> {
    if info.context_idx == GUC_INVALID_CONTEXT_ID
        || child_lrcas.len() != child_ids.len()
        || child_lrcas.len() > 32
        || child_ids
            .iter()
            .enumerate()
            .any(|(index, id)| *id != info.context_idx + index as u32 + 1)
    {
        return Err(Error::Refused);
    }
    let mut action = Vec::new();
    action
        .try_reserve_exact(13 + child_lrcas.len() * 2)
        .map_err(|_| Error::Refused)?;
    action.extend_from_slice(&[
        ACTION_REGISTER_CONTEXT_MULTI_LRC,
        info.flags,
        info.context_idx,
        info.engine_class,
        info.engine_submit_mask,
        info.wq_desc_lo,
        info.wq_desc_hi,
        info.wq_base_lo,
        info.wq_base_hi,
        info.wq_size,
        child_lrcas.len() as u32 + 1,
        info.hwlrca_lo,
        info.hwlrca_hi,
    ]);
    for lrca in child_lrcas {
        action.push(*lrca as u32);
        action.push((*lrca >> 32) as u32);
    }
    Ok(action)
}

/// upstream: intel_guc_submission.c __guc_action_deregister_context().
pub const fn deregister_context_action(context_id: u32) -> Result<[u32; 2], Error> {
    if context_id == GUC_INVALID_CONTEXT_ID {
        Err(Error::Refused)
    } else {
        Ok([ACTION_DEREGISTER_CONTEXT, context_id])
    }
}

/// upstream: intel_guc_submission.c __guc_add_request() scheduling-mode path.
pub const fn schedule_context_action(context_id: u32, enable: bool) -> Result<[u32; 3], Error> {
    if context_id == GUC_INVALID_CONTEXT_ID {
        Err(Error::Refused)
    } else {
        Ok([
            ACTION_SCHED_CONTEXT_MODE_SET,
            context_id,
            if enable {
                CONTEXT_ENABLE
            } else {
                CONTEXT_DISABLE
            },
        ])
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextPolicy {
    pub context_id: u32,
    pub priority: u32,
    pub execution_quantum_us: u32,
    pub preemption_timeout_us: u32,
    pub slpc_frequency_request: u32,
    pub preempt_to_idle: bool,
}

/// upstream: intel_guc_submission.c guc_context_policy_init_v70() and
/// __guc_context_policy_add_*(). Returns action header and key/length/value KLVs.
pub fn context_policy_action(policy: ContextPolicy) -> Result<Vec<u32>, Error> {
    if policy.context_id == GUC_INVALID_CONTEXT_ID || policy.priority > 3 {
        return Err(Error::Refused);
    }
    let mut words = Vec::new();
    let klv_count = if policy.preempt_to_idle { 5 } else { 4 };
    words
        .try_reserve_exact(2 + klv_count * 2)
        .map_err(|_| Error::Refused)?;
    words.extend_from_slice(&[ACTION_UPDATE_CONTEXT_POLICIES, policy.context_id]);
    let mut add = |key: u16, value: u32| {
        words.push((u32::from(key) << 16) | 1); // one dword value
        words.push(value);
    };
    add(CONTEXT_POLICY_KLV_SCHEDULING_PRIORITY, policy.priority);
    add(
        CONTEXT_POLICY_KLV_EXECUTION_QUANTUM,
        policy.execution_quantum_us,
    );
    add(
        CONTEXT_POLICY_KLV_PREEMPTION_TIMEOUT,
        policy.preemption_timeout_us,
    );
    add(
        CONTEXT_POLICY_KLV_SLPM_GT_FREQUENCY,
        policy.slpc_frequency_request,
    );
    if policy.preempt_to_idle {
        add(CONTEXT_POLICY_KLV_PREEMPT_TO_IDLE, 1);
    }
    Ok(words)
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
    fn context_id_partitions_keep_multilrc_ranges_contiguous() {
        let mut pool = ContextIdPool::new(512).unwrap();
        let parent = pool.allocate_multi(3).unwrap();
        let next = pool.allocate_multi(0).unwrap();
        assert_eq!(parent, ContextIdRange { base: 0, count: 4 });
        assert_eq!(next, ContextIdRange { base: 4, count: 1 });
        pool.release_multi(parent).unwrap();
        assert_eq!(
            pool.allocate_multi(1).unwrap(),
            ContextIdRange { base: 0, count: 2 }
        );
        let single = pool.allocate_single().unwrap();
        assert_eq!(single, 32);
        pool.release_single(single).unwrap();
        assert_eq!(pool.allocate_single().unwrap(), 32);
    }

    #[test]
    fn context_registration_info_splits_addresses_and_encodes_gen12_priority() {
        let info = context_registration_info(
            17,
            1,
            1,
            0x1_2345_6000,
            Some(2),
            Some(0x2_0000_3000),
            Some(0x3_0000_4000),
        )
        .unwrap();
        let flags = info.flags;
        let engine_class = info.engine_class;
        let hwlrca_lo = info.hwlrca_lo;
        let hwlrca_hi = info.hwlrca_hi;
        let wq_desc_hi = info.wq_desc_hi;
        let wq_base_hi = info.wq_base_hi;
        let wq_size = info.wq_size;
        assert_eq!(flags, CONTEXT_REGISTRATION_FLAG_KMD);
        assert_eq!(engine_class, 3);
        assert_eq!(hwlrca_lo, 0x2345_6000 | (1 << 9));
        assert_eq!(hwlrca_hi, 1);
        assert_eq!(wq_desc_hi, 2);
        assert_eq!(wq_base_hi, 3);
        assert_eq!(wq_size, WQ_SIZE as u32);
        assert_eq!(register_context_action(info)[0], ACTION_REGISTER_CONTEXT);
        assert_eq!(register_context_action(info).len(), 12);
        assert_eq!(
            deregister_context_action(17).unwrap(),
            [ACTION_DEREGISTER_CONTEXT, 17]
        );
        assert_eq!(
            schedule_context_action(17, true).unwrap(),
            [ACTION_SCHED_CONTEXT_MODE_SET, 17, 1]
        );
    }

    #[test]
    fn context_policy_packet_encodes_gen12_quantum_priority_and_preemption_klvs() {
        let words = context_policy_action(ContextPolicy {
            context_id: 3,
            priority: 2,
            execution_quantum_us: 20_000,
            preemption_timeout_us: 5_000,
            slpc_frequency_request: 0,
            preempt_to_idle: true,
        })
        .unwrap();
        assert_eq!(words.len(), 12);
        assert_eq!(words[0], ACTION_UPDATE_CONTEXT_POLICIES);
        assert_eq!(words[1], 3);
        assert_eq!(
            words[2],
            (u32::from(CONTEXT_POLICY_KLV_SCHEDULING_PRIORITY) << 16) | 1
        );
        assert_eq!(words[3], 2);
        assert_eq!(
            words[words.len() - 2],
            (u32::from(CONTEXT_POLICY_KLV_PREEMPT_TO_IDLE) << 16) | 1
        );
        assert_eq!(words[words.len() - 1], 1);
    }

    #[test]
    fn v69_context_descriptor_and_v69_v70_multi_registration_actions_match_abi() {
        let descriptor = prepare_context_desc_v69(ContextDescV69Input {
            engine_class: 1,
            engine_submit_mask: 1,
            hwlrca: 0x40_0000,
            priority: 2,
            execution_quantum_ms: 20,
            preemption_timeout_ms: 5,
            preempt_to_idle: true,
            process_desc: 0x50_0000,
            wq_addr: 0x50_0800,
            wq_size: WQ_SIZE as u32,
        })
        .unwrap();
        let descriptor_class = descriptor.engine_class;
        let descriptor_quantum = descriptor.execution_quantum;
        let descriptor_timeout = descriptor.preemption_timeout;
        let descriptor_flags = descriptor.policy_flags;
        assert_eq!(descriptor_class, 3);
        assert_eq!(descriptor_quantum, 20_000);
        assert_eq!(descriptor_timeout, 5_000);
        assert_eq!(descriptor_flags, CONTEXT_POLICY_FLAG_PREEMPT_TO_IDLE_V69);
        assert_eq!(
            register_context_action_v69(7, 0x1000).unwrap(),
            [ACTION_REGISTER_CONTEXT, 7, 0x1000]
        );
        assert_eq!(
            register_multi_context_action_v69(7, &[0x1000, 0x1080]).unwrap(),
            [ACTION_REGISTER_CONTEXT_MULTI_LRC, 7, 2, 0x1000, 0x1080]
        );

        let info =
            context_registration_info(7, 0, 1, 0x40_0000, None, Some(0x50_0000), Some(0x50_0800))
                .unwrap();
        let multi = register_multi_context_action_v70(info, &[0x60_0000], &[8]).unwrap();
        assert_eq!(multi[0], ACTION_REGISTER_CONTEXT_MULTI_LRC);
        assert_eq!(multi[1], CONTEXT_REGISTRATION_FLAG_KMD);
        assert_eq!(multi[2], 7);
        assert_eq!(multi[10], 2);
        assert_eq!(&multi[13..], &[0x60_0000, 0]);
    }

    #[test]
    fn guc_submission_state_registers_submits_handles_g2h_and_releases_ids() {
        let mut submission = GucSubmission::new(512).unwrap();
        let lease = submission.allocate_single_id().unwrap();
        let ContextIdLease::Single(context_id) = lease else {
            panic!("single ID expected")
        };
        let info =
            context_registration_info(context_id, 0, 1, 0x40_0000, None, None, None).unwrap();
        let policy = ContextPolicy {
            context_id,
            priority: GUC_CLIENT_PRIORITY_KMD_NORMAL.into(),
            execution_quantum_us: 20_000,
            preemption_timeout_us: 5_000,
            slpc_frequency_request: 0,
            preempt_to_idle: false,
        };
        let mut pair = crate::guc_ct::CtbPair::new().unwrap();
        submission
            .register_v70(&mut pair, info, policy, lease, &[], &[])
            .unwrap();
        assert_eq!(submission.context_count(), 1);
        let (fences, count) = submission
            .submit_request(&mut pair, context_id, false)
            .unwrap();
        assert_eq!(count, 1);
        assert_ne!(fences[0], 0);
        assert!(submission.context_state(context_id).unwrap().enabled());
        pair.release_response_space(4).unwrap();
        submission
            .handle_event(
                &mut pair,
                crate::guc_ct::CtbEvent {
                    action: crate::guc_ct::ACTION_SCHED_CONTEXT_MODE_DONE,
                    data0: 0,
                    payload: &[context_id, 0],
                    released_response_credit: 4,
                    process_immediately: false,
                },
            )
            .unwrap();
        assert!(
            !submission
                .context_state(context_id)
                .unwrap()
                .pending_enable()
        );
        submission
            .deregister_context(&mut pair, context_id)
            .unwrap();
        pair.release_response_space(3).unwrap();
        submission
            .handle_event(
                &mut pair,
                crate::guc_ct::CtbEvent {
                    action: crate::guc_ct::ACTION_DEREGISTER_CONTEXT_DONE,
                    data0: 0,
                    payload: &[context_id],
                    released_response_credit: 3,
                    process_immediately: false,
                },
            )
            .unwrap();
        assert_eq!(submission.context_count(), 0);
        assert_eq!(submission.allocate_single_id().unwrap(), lease);
    }

    #[test]
    fn request_path_enables_then_schedules_context_and_respects_blocking() {
        let mut state = ContextSchedState::default();
        let plan = state.prepare_request(9, false).unwrap();
        assert_eq!(plan.count, 1);
        assert_eq!(
            plan.actions[0].words,
            [ACTION_SCHED_CONTEXT_MODE_SET, 9, CONTEXT_ENABLE]
        );
        assert_eq!(
            plan.actions[0].expected_response_dwords,
            G2H_LEN_DW_SCHED_CONTEXT_MODE_SET
        );
        assert!(state.pending_enable());
        state.request_send_result(plan, true);
        assert!(state.enabled());
        assert!(state.pending_enable());
        state.sched_mode_done(true);
        assert!(!state.pending_enable());

        state.increment_blocked().unwrap();
        assert_eq!(state.prepare_request(9, false).unwrap().count, 0);
        let parent = state.prepare_request(9, true).unwrap();
        assert_eq!(parent.count, 1);
        assert_eq!(parent.actions[0].words[0], ACTION_SCHED_CONTEXT);

        let mut new_parent_state = ContextSchedState::default();
        let new_parent = new_parent_state.prepare_request(10, true).unwrap();
        assert_eq!(new_parent.count, 2);
        assert_eq!(
            new_parent.actions[0].words[0],
            ACTION_SCHED_CONTEXT_MODE_SET
        );
        assert_eq!(new_parent.actions[1].words, [ACTION_SCHED_CONTEXT, 10, 0]);
    }

    #[test]
    fn g2h_schedule_and_deregister_events_validate_pending_context_state() {
        let mut state = ContextSchedState::default();
        state.prepare_request(9, false).unwrap();
        state.request_send_result(
            SchedPlan {
                actions: [
                    SchedAction::new(
                        [ACTION_SCHED_CONTEXT_MODE_SET, 9, CONTEXT_ENABLE],
                        3,
                        G2H_LEN_DW_SCHED_CONTEXT_MODE_SET,
                    ),
                    SchedAction::default(),
                ],
                count: 1,
            },
            true,
        );
        assert_eq!(sched_mode_done_message(&mut state, &[9, 0]), Ok(true));
        assert_eq!(
            sched_mode_done_message(&mut state, &[9, 0]),
            Err(Error::Refused)
        );

        state.set_wait_for_deregister_to_register();
        assert_eq!(
            deregister_done_message(&mut state, &[9]),
            Ok(DeregisterCompletion::ReRegister)
        );
        state.set_destroyed();
        assert_eq!(
            deregister_done_message(&mut state, &[9]),
            Ok(DeregisterCompletion::Destroy)
        );
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
