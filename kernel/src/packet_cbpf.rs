//! Verified classic-BPF packet filters.
//!
//! The packet socket adapter owns the Linux option and lifetime rules.  This
//! module owns only the immutable program and its optional native x86_64
//! publication.  The verified interpreter remains the semantic oracle and is
//! retained even when a W^X native image is available.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use axcbpf::{
    Ancillary, InputProfile, Instruction, PacketInput, PacketInputContext,
    PacketMetadata as CbpfPacketMetadata, Program,
};
use axerrno::{AxError, AxResult, LinuxError};
use axnet::packet::{
    LinkPacketType, PacketAncillaryCapabilities, PacketFilter, PacketFilterContext,
};
#[cfg(test)]
use axnet::packet::{PacketAncillaryMetadata, PacketMetadata as LinkPacketMetadata};
use bytemuck::AnyBitPattern;
use linux_raw_sys::net::socklen_t;

use crate::{
    mm::{UserConstPtr, UserMemoryCapability, map_usercopy_error},
    seccomp_jit::{ExecutorPolicy, packet_executor_policy},
};

static PUBLISHED: AtomicU64 = AtomicU64::new(0);
// Publication reservations are consumed only by admission and procfs
// snapshot readers. Packet execution never reads or updates this state.
static PUBLISH_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
static NATIVE_EXECUTED: AtomicU64 = AtomicU64::new(0);
static INTERPRETER_EXECUTED: AtomicU64 = AtomicU64::new(0);
static FALLBACK_POLICY_INTERPRETER: AtomicU64 = AtomicU64::new(0);
static FALLBACK_TRANSLATION: AtomicU64 = AtomicU64::new(0);
static FALLBACK_PUBLICATION: AtomicU64 = AtomicU64::new(0);
static FALLBACK_OWNER: AtomicU64 = AtomicU64::new(0);
static FALLBACK_UNAVAILABLE: AtomicU64 = AtomicU64::new(0);
static JIT_REJECTED: AtomicU64 = AtomicU64::new(0);

const ANC_PROTOCOL: u8 = 1 << 0;
const ANC_PKTTYPE: u8 = 1 << 1;
const ANC_IFINDEX: u8 = 1 << 2;
const ANC_MARK: u8 = 1 << 3;
const ANC_QUEUE: u8 = 1 << 4;
const ANC_VLAN: u8 = 1 << 5;

/// Bounded packet cBPF counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Counters {
    pub published: u64,
    pub native_executed: u64,
    pub interpreter_executed: u64,
    pub fallback_policy_interpreter: u64,
    pub fallback_translation: u64,
    pub fallback_publication: u64,
    pub fallback_owner: u64,
    pub fallback_unavailable: u64,
    pub jit_rejected: u64,
}

pub(crate) fn counters() -> Counters {
    loop {
        if PUBLISH_IN_FLIGHT.load(Ordering::Acquire) != 0 {
            core::hint::spin_loop();
            continue;
        }
        let first = load_counters();
        if PUBLISH_IN_FLIGHT.load(Ordering::Acquire) != 0 {
            continue;
        }
        let second = load_counters();
        if PUBLISH_IN_FLIGHT.load(Ordering::Acquire) == 0 && second.is_monotonic_from(first) {
            return second;
        }
    }
}

fn load_counters() -> Counters {
    Counters {
        published: PUBLISHED.load(Ordering::Relaxed),
        native_executed: NATIVE_EXECUTED.load(Ordering::Relaxed),
        interpreter_executed: INTERPRETER_EXECUTED.load(Ordering::Relaxed),
        fallback_policy_interpreter: FALLBACK_POLICY_INTERPRETER.load(Ordering::Relaxed),
        fallback_translation: FALLBACK_TRANSLATION.load(Ordering::Relaxed),
        fallback_publication: FALLBACK_PUBLICATION.load(Ordering::Relaxed),
        fallback_owner: FALLBACK_OWNER.load(Ordering::Relaxed),
        fallback_unavailable: FALLBACK_UNAVAILABLE.load(Ordering::Relaxed),
        jit_rejected: JIT_REJECTED.load(Ordering::Relaxed),
    }
}

impl Counters {
    fn is_monotonic_from(self, previous: Self) -> bool {
        self.published >= previous.published
            && self.native_executed >= previous.native_executed
            && self.interpreter_executed >= previous.interpreter_executed
            && self.fallback_policy_interpreter >= previous.fallback_policy_interpreter
            && self.fallback_translation >= previous.fallback_translation
            && self.fallback_publication >= previous.fallback_publication
            && self.fallback_owner >= previous.fallback_owner
            && self.fallback_unavailable >= previous.fallback_unavailable
            && self.jit_rejected >= previous.jit_rejected
    }
}

fn increment(counter: &AtomicU64) {
    let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

pub(crate) fn try_reserve_published() -> Option<PublicationReservation> {
    PUBLISH_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
    let result = PUBLISHED.try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
        value.checked_add(1)
    });
    if result.is_ok() {
        Some(PublicationReservation { committed: false })
    } else {
        PUBLISH_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        None
    }
}

pub(crate) struct PublicationReservation {
    committed: bool,
}

impl PublicationReservation {
    pub(crate) fn commit(mut self) {
        self.committed = true;
        PUBLISH_IN_FLIGHT.fetch_sub(1, Ordering::Release);
    }
}

impl Drop for PublicationReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _ = PUBLISHED.try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_sub(1)
        });
        PUBLISH_IN_FLIGHT.fetch_sub(1, Ordering::Release);
    }
}

fn record_native_executed() {
    increment(&NATIVE_EXECUTED);
}

fn record_interpreter_executed() {
    increment(&INTERPRETER_EXECUTED);
}

#[cfg(test)]
pub(crate) fn reset_counters_for_tests() {
    for counter in [
        &PUBLISHED,
        &NATIVE_EXECUTED,
        &INTERPRETER_EXECUTED,
        &FALLBACK_POLICY_INTERPRETER,
        &FALLBACK_TRANSLATION,
        &FALLBACK_PUBLICATION,
        &FALLBACK_OWNER,
        &FALLBACK_UNAVAILABLE,
        &JIT_REJECTED,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
    PUBLISH_IN_FLIGHT.store(0, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FallbackReason {
    PolicyInterpreter,
    Translation,
    Publication,
    Owner,
    Unavailable,
}

fn record_fallback(reason: FallbackReason) {
    match reason {
        FallbackReason::PolicyInterpreter => increment(&FALLBACK_POLICY_INTERPRETER),
        FallbackReason::Translation => increment(&FALLBACK_TRANSLATION),
        FallbackReason::Publication => increment(&FALLBACK_PUBLICATION),
        FallbackReason::Owner => increment(&FALLBACK_OWNER),
        FallbackReason::Unavailable => increment(&FALLBACK_UNAVAILABLE),
    }
}

/// Linux x86_64 `struct sock_fprog` copied by SO_ATTACH_FILTER.
#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
pub(crate) struct RawSockFprog {
    pub(crate) length: u16,
    pub(crate) padding: [u8; 6],
    pub(crate) filter: u64,
}

const _: [(); 16] = [(); core::mem::size_of::<RawSockFprog>()];
const _: [(); 8] = [(); core::mem::align_of::<RawSockFprog>()];
const _: [(); 8] = [(); core::mem::offset_of!(RawSockFprog, filter)];

/// Copies the Linux `sock_fprog` envelope with Linux's generic option
/// precedence. A short option with an unreadable pointer is rejected before
/// touching userspace. For a longer option Linux first probes only the
/// generic four-byte prefix; the exact-size check happens before the complete
/// 16-byte envelope is copied. This matters at a page boundary: len=8 must
/// not read bytes 8..15 when the exact-size check will reject it.
pub(crate) fn copy_envelope(
    capability: &UserMemoryCapability,
    optval: UserConstPtr<u8>,
    optlen: socklen_t,
) -> AxResult<RawSockFprog> {
    // The generic socket option path rejects the tiny prefix without a
    // usercopy. Once four bytes are available it probes only that prefix,
    // making NULL+len>=4 EFAULT while still avoiding a full envelope read for
    // a non-exact option length.
    if (optlen as usize) < core::mem::size_of::<i32>() {
        return Err(AxError::InvalidInput);
    }
    let _prefix = capability
        .read_value::<i32>(optval.address().as_usize() as *const i32)
        .map_err(map_usercopy_error)?;
    if optlen as usize != core::mem::size_of::<RawSockFprog>() {
        return Err(AxError::InvalidInput);
    }
    let header: RawSockFprog = capability
        .read_value::<RawSockFprog>(optval.address().as_usize() as *const RawSockFprog)
        .map_err(map_usercopy_error)?;
    Ok(header)
}

/// Validates a copied envelope and copies its instruction array.  This is
/// deliberately called only after SO_LOCK_FILTER admission so a locked socket
/// reports EPERM for readable but invalid program fields.
pub(crate) fn copy_instructions(
    capability: &UserMemoryCapability,
    header: RawSockFprog,
) -> AxResult<alloc::vec::Vec<Instruction>> {
    let length = usize::from(header.length);
    if length == 0 || length > axcbpf::MAX_INSTRUCTIONS || header.filter == 0 {
        return Err(AxError::InvalidInput);
    }

    let mut instructions = alloc::vec::Vec::new();
    instructions
        .try_reserve_exact(length)
        .map_err(|_| AxError::NoMemory)?;
    instructions.resize(length, Instruction::default());
    let destination = unsafe {
        core::slice::from_raw_parts_mut(
            instructions
                .as_mut_ptr()
                .cast::<core::mem::MaybeUninit<Instruction>>(),
            length,
        )
    };
    capability
        .read_slice(header.filter as usize as *const Instruction, destination)
        .map_err(map_usercopy_error)?;
    Ok(instructions)
}

/// One verified ordinary socket-filter program.
pub(crate) struct PacketCbpfFilter {
    program: Program,
    metadata_required: bool,
    ancillary_requirements: u8,
    #[cfg(feature = "bpf")]
    native: Option<crate::jit_memory::ExecutableCode>,
}

impl PacketCbpfFilter {
    /// Verifies and fallibly owns a copied Linux classic-BPF program.
    pub(crate) fn try_new(instructions: alloc::vec::Vec<Instruction>) -> AxResult<Arc<Self>> {
        Self::try_new_with_policy(instructions, packet_executor_policy())
    }

    /// Returns the device ancillary fields required by this verified program.
    ///
    /// Protocol, packet type, and interface index are part of the mandatory
    /// hot link snapshot. Mark, queue, and VLAN are checked against the
    /// selected device before the filter is published.
    pub(crate) fn required_ancillary_capabilities(&self) -> PacketAncillaryCapabilities {
        let mut required = PacketAncillaryCapabilities::NONE;
        if self.ancillary_requirements & ANC_MARK != 0 {
            required = required.union(PacketAncillaryCapabilities::MARK);
        }
        if self.ancillary_requirements & ANC_QUEUE != 0 {
            required = required.union(PacketAncillaryCapabilities::QUEUE);
        }
        if self.ancillary_requirements & ANC_VLAN != 0 {
            required = required.union(PacketAncillaryCapabilities::VLAN);
        }
        required
    }

    fn try_new_with_policy(
        instructions: alloc::vec::Vec<Instruction>,
        policy: ExecutorPolicy,
    ) -> AxResult<Arc<Self>> {
        let program = Program::try_from_vec(instructions).map_err(map_verify_error)?;
        let ancillary_requirements = ancillary_requirements(&program);
        let metadata_required = ancillary_requirements != 0;
        #[cfg(feature = "bpf")]
        let native = select_native(&program, policy, metadata_required)?;
        #[cfg(not(feature = "bpf"))]
        select_native(&program, policy, metadata_required)?;
        #[cfg(feature = "bpf")]
        let native_selected = native.is_some();
        let filter = Arc::try_new(Self {
            program,
            metadata_required,
            ancillary_requirements,
            #[cfg(feature = "bpf")]
            native,
        })
        .map_err(|_| {
            #[cfg(feature = "bpf")]
            if native_selected {
                if policy == ExecutorPolicy::Auto {
                    record_fallback(FallbackReason::Owner);
                } else if policy == ExecutorPolicy::Jit {
                    increment(&JIT_REJECTED);
                }
            }
            AxError::NoMemory
        })?;
        Ok(filter)
    }
}

impl PacketFilter for PacketCbpfFilter {
    fn filter(&self, packet: &[u8], context: PacketFilterContext<'_>) -> AxResult<usize> {
        if !self.metadata_required {
            // Keep ordinary byte-only filters on the old hot path: no
            // metadata conversion or ancillary sidecar inspection is needed.
            return self.filter_bytes(packet);
        }
        let metadata = self.cbpf_metadata(context);
        self.filter_with_metadata(packet, metadata)
    }
}

impl PacketCbpfFilter {
    /// Applies the filter with the immutable Linux socket-filter metadata
    /// snapshot associated with this packet.
    ///
    /// The generic byte-only entry point remains available for callers that
    /// have no ancillary loads. A packet-aware capture path reaches the trait
    /// implementation above with the complete axnet metadata snapshot. A
    /// caller without a packet context cannot evaluate an ancillary program
    /// and receives `EOPNOTSUPP`.
    pub(crate) fn filter(&self, packet: &[u8]) -> AxResult<usize> {
        if self.metadata_required {
            return Err(LinuxError::EOPNOTSUPP.into());
        }
        self.filter_bytes(packet)
    }

    fn cbpf_metadata(&self, context: PacketFilterContext<'_>) -> CbpfPacketMetadata {
        let hot = context.metadata();
        let ancillary = context.ancillary();
        let mark = ancillary.mark();
        let queue = ancillary.queue();
        let (vlan_tag, vlan_present, vlan_tpid) = ancillary.vlan();

        let pkttype = match hot.packet_type {
            LinkPacketType::Host => 0,
            LinkPacketType::Broadcast => 1,
            LinkPacketType::Multicast => 2,
            LinkPacketType::OtherHost => 3,
            LinkPacketType::Outgoing => 4,
        };
        CbpfPacketMetadata::new(
            hot.protocol,
            hot.interface_index,
            pkttype,
            mark,
            queue,
            vlan_tag,
            vlan_present,
            vlan_tpid,
        )
    }
    fn filter_bytes(&self, packet: &[u8]) -> AxResult<usize> {
        #[cfg(feature = "bpf")]
        let result = self.native.as_ref().map(|native| {
            record_native_executed();
            // Byte-only images retain the original direct packet ABI; no
            // metadata snapshot is materialized on this hot path.
            native.execute(packet)
        });
        #[cfg(not(feature = "bpf"))]
        let result = None;
        let result = result.unwrap_or_else(|| {
            record_interpreter_executed();
            self.program.evaluate(packet)
        });
        Ok(usize::try_from(result)
            .unwrap_or(usize::MAX)
            .min(packet.len()))
    }

    pub(crate) fn filter_with_metadata(
        &self,
        packet: &[u8],
        metadata: CbpfPacketMetadata,
    ) -> AxResult<usize> {
        if !self.metadata_required {
            return self.filter_bytes(packet);
        }
        #[cfg(feature = "bpf")]
        let result = self.native.as_ref().map(|native| {
            record_native_executed();
            let context = PacketInputContext::new(packet, metadata);
            // The packet-aware image keeps the existing two-argument JIT ABI,
            // but receives a pointer to this typed context. The borrow and
            // synchronous call keep both the context and packet live for the
            // entire native execution.
            let context_bytes = unsafe {
                core::slice::from_raw_parts(
                    (&context as *const PacketInputContext).cast::<u8>(),
                    core::mem::size_of::<PacketInputContext>(),
                )
            };
            native.execute(context_bytes)
        });
        #[cfg(not(feature = "bpf"))]
        let result = None;
        let result = result.unwrap_or_else(|| {
            record_interpreter_executed();
            self.program.evaluate(&PacketInput::new(packet, metadata))
        });
        Ok(usize::try_from(result)
            .unwrap_or(usize::MAX)
            .min(packet.len()))
    }
}

fn ancillary_for_instruction(instruction: Instruction) -> Option<Ancillary> {
    if !matches!(
        instruction.code,
        axcbpf::opcode::LD_W_ABS | axcbpf::opcode::LD_H_ABS | axcbpf::opcode::LD_B_ABS
    ) {
        return None;
    }
    axcbpf::ancillary_from_offset(instruction.k)
}

fn ancillary_requirements(program: &Program) -> u8 {
    let mut requirements = 0;
    for instruction in program.instructions().iter().copied() {
        let Some(field) = ancillary_for_instruction(instruction) else {
            continue;
        };
        requirements |= match field {
            Ancillary::Protocol => ANC_PROTOCOL,
            Ancillary::Pkttype => ANC_PKTTYPE,
            Ancillary::Ifindex => ANC_IFINDEX,
            Ancillary::Mark => ANC_MARK,
            Ancillary::Queue => ANC_QUEUE,
            Ancillary::VlanTag | Ancillary::VlanTagPresent | Ancillary::VlanTpid => ANC_VLAN,
            _ => 0,
        };
    }
    requirements
}

fn map_verify_error(error: axcbpf::VerifyError) -> AxError {
    match error {
        axcbpf::VerifyError::NoMemory => AxError::NoMemory,
        _ => AxError::InvalidInput,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeError {
    Translation,
    Publication(AxError),
    Quarantined(AxError),
    Retained(AxError),
    Unavailable(AxError),
}

impl NativeError {
    #[cfg(feature = "bpf")]
    fn from_memory_error(error: crate::jit_memory::MemoryError) -> Self {
        match error {
            crate::jit_memory::MemoryError::Unavailable(error) => Self::Unavailable(error),
            crate::jit_memory::MemoryError::Quarantined(error) => Self::Quarantined(error),
            crate::jit_memory::MemoryError::Retained(error) => Self::Retained(error),
        }
    }

    fn into_ax_error(self) -> AxError {
        match self {
            Self::Translation => LinuxError::EOPNOTSUPP.into(),
            Self::Publication(error) => error,
            Self::Quarantined(_) | Self::Retained(_) => LinuxError::EOPNOTSUPP.into(),
            Self::Unavailable(error) => error,
        }
    }

    fn fallback_reason(self) -> FallbackReason {
        match self {
            Self::Translation => FallbackReason::Translation,
            Self::Publication(_) => FallbackReason::Publication,
            Self::Quarantined(_) | Self::Retained(_) | Self::Unavailable(_) => {
                FallbackReason::Unavailable
            }
        }
    }
}

#[cfg(feature = "bpf")]
fn select_native(
    program: &Program,
    policy: ExecutorPolicy,
    metadata_required: bool,
) -> Result<Option<crate::jit_memory::ExecutableCode>, AxError> {
    if policy == ExecutorPolicy::Interpreter {
        record_fallback(FallbackReason::PolicyInterpreter);
        return Ok(None);
    }
    let native = match try_compile(program, metadata_required) {
        Ok(native) => native,
        Err(error) => {
            if policy == ExecutorPolicy::Jit {
                increment(&JIT_REJECTED);
                return Err(error.into_ax_error());
            }
            record_fallback(error.fallback_reason());
            return Ok(None);
        }
    };
    Ok(Some(native))
}

#[cfg(not(feature = "bpf"))]
fn select_native(
    program: &Program,
    policy: ExecutorPolicy,
    _metadata_required: bool,
) -> AxResult<()> {
    let _ = program;
    if policy == ExecutorPolicy::Interpreter {
        record_fallback(FallbackReason::PolicyInterpreter);
        return Ok(());
    }
    if policy == ExecutorPolicy::Jit {
        increment(&JIT_REJECTED);
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    record_fallback(FallbackReason::Unavailable);
    Ok(())
}

#[cfg(feature = "bpf")]
fn try_compile(
    program: &Program,
    metadata_required: bool,
) -> Result<crate::jit_memory::ExecutableCode, NativeError> {
    let profile = if metadata_required {
        InputProfile::PacketContextBigEndian
    } else {
        InputProfile::PacketBytesBigEndian
    };
    let image = program
        .translate_with_profile(profile)
        .map_err(|_| NativeError::Translation)?;
    let mut writable =
        crate::jit_memory::prepare(image.bytes().len()).map_err(NativeError::from_memory_error)?;
    if let Err(error) = writable.write(0, image.bytes()) {
        let error = writable.abort(crate::jit_memory::MemoryError::Unavailable(error));
        return Err(NativeError::from_memory_error(error));
    }
    writable
        .publish(image.entry_offset() as usize)
        .map_err(NativeError::from_memory_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_filter_accept_drop_and_snaplen_follow_classic_semantics() {
        let accept = PacketCbpfFilter::try_new(alloc::vec![Instruction::statement(
            axcbpf::opcode::RET_K,
            u32::MAX,
        )])
        .unwrap();
        assert_eq!(accept.filter(&[1, 2, 3, 4]).unwrap(), 4);

        let drop = PacketCbpfFilter::try_new(alloc::vec![Instruction::statement(
            axcbpf::opcode::RET_K,
            0,
        )])
        .unwrap();
        assert_eq!(drop.filter(&[1, 2, 3, 4]).unwrap(), 0);

        let snaplen = PacketCbpfFilter::try_new(alloc::vec![Instruction::statement(
            axcbpf::opcode::RET_K,
            2,
        )])
        .unwrap();
        assert_eq!(snaplen.filter(&[1, 2, 3, 4]).unwrap(), 2);
    }

    #[test]
    fn invalid_jump_is_rejected_before_publication() {
        let result = PacketCbpfFilter::try_new(alloc::vec![Instruction::jump(
            axcbpf::opcode::JMP_JEQ_K,
            0,
            1,
            0,
        )]);
        assert!(matches!(result, Err(AxError::InvalidInput)));
    }

    #[test]
    fn ancillary_filters_require_a_real_metadata_snapshot() {
        let filter = PacketCbpfFilter::try_new_with_policy(
            alloc::vec![
                Instruction::statement(
                    axcbpf::opcode::LD_W_ABS,
                    axcbpf::Ancillary::Protocol.encoded_offset(),
                ),
                Instruction::statement(axcbpf::opcode::RET_A, 0),
            ],
            ExecutorPolicy::Interpreter,
        )
        .unwrap();
        assert_eq!(
            filter.filter(&[0x08, 0x00]).unwrap_err(),
            LinuxError::EOPNOTSUPP.into()
        );
        let metadata = CbpfPacketMetadata::new(0x0800, 3, 0, 0, 0, 0, false, 0);
        let packet = alloc::vec![0_u8; 2048];
        assert_eq!(
            filter.filter_with_metadata(&packet, metadata).unwrap(),
            2048
        );
    }

    #[test]
    fn packet_filter_trait_maps_real_link_context_for_interpreter_and_jit() {
        let hot = LinkPacketMetadata {
            interface_index: 7,
            protocol: 0x0800,
            hardware_type: axnet::packet::LinkHardwareType::Ethernet,
            packet_type: LinkPacketType::Multicast,
            link_header_len: 14,
            address: [0; 8],
            address_len: 6,
        };
        let ancillary = PacketAncillaryMetadata::canonical()
            .with_mark(0xfeed_beef)
            .with_queue(5)
            .with_vlan(0x0064, true, 0x8100);
        let context = PacketFilterContext::new(&hot, &ancillary);
        let packet = alloc::vec![0_u8; 0x1_0000];

        for (field, expected) in [
            (Ancillary::Protocol, 0x0800),
            (Ancillary::Pkttype, 2),
            (Ancillary::Ifindex, 7),
            (Ancillary::Mark, 0xbeef),
            (Ancillary::Queue, 5),
            (Ancillary::VlanTag, 0x0064),
            (Ancillary::VlanTagPresent, 1),
            (Ancillary::VlanTpid, 0x8100),
        ] {
            let filter = PacketCbpfFilter::try_new_with_policy(
                alloc::vec![
                    Instruction::statement(axcbpf::opcode::LD_W_ABS, field.encoded_offset(),),
                    Instruction::statement(axcbpf::opcode::ALU_AND_K, 0xffff),
                    Instruction::statement(axcbpf::opcode::RET_A, 0),
                ],
                ExecutorPolicy::Interpreter,
            )
            .unwrap();
            assert_eq!(
                PacketFilter::filter(filter.as_ref(), &packet, context).unwrap(),
                expected as usize,
                "{field:?} interpreter"
            );

            #[cfg(feature = "bpf")]
            {
                let result = PacketCbpfFilter::try_new_with_policy(
                    alloc::vec![
                        Instruction::statement(axcbpf::opcode::LD_W_ABS, field.encoded_offset(),),
                        Instruction::statement(axcbpf::opcode::ALU_AND_K, 0xffff),
                        Instruction::statement(axcbpf::opcode::RET_A, 0),
                    ],
                    ExecutorPolicy::Jit,
                );
                match result {
                    Ok(filter) => assert_eq!(
                        PacketFilter::filter(filter.as_ref(), &packet, context).unwrap(),
                        expected as usize,
                        "{field:?} jit"
                    ),
                    // Host tests do not construct axmm's kernel address space,
                    // so a ForceJit admission correctly reports an unavailable
                    // executable arena rather than falling back silently.
                    Err(AxError::BadState) => {}
                    Err(error) => panic!("{field:?} JIT admission failed unexpectedly: {error:?}"),
                }
            }
        }

        let canonical_mark = PacketCbpfFilter::try_new_with_policy(
            alloc::vec![
                Instruction::statement(axcbpf::opcode::LD_W_ABS, Ancillary::Mark.encoded_offset(),),
                Instruction::statement(axcbpf::opcode::RET_A, 0),
            ],
            ExecutorPolicy::Interpreter,
        )
        .unwrap();
        assert_eq!(
            PacketFilter::filter(
                canonical_mark.as_ref(),
                &packet,
                PacketFilterContext::new(&hot, &PacketAncillaryMetadata::canonical()),
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn inline_vlan_filter_differential_uses_untagged_bytes_and_sidecar() {
        // Linux strips one inline VLAN header before the packet tap. The
        // filter therefore receives the canonical Ethernet bytes while the
        // outer TCI/TPID are available only through SKF_AD_* metadata.
        let hot = LinkPacketMetadata {
            interface_index: 7,
            protocol: 0x0800,
            hardware_type: axnet::packet::LinkHardwareType::Ethernet,
            packet_type: LinkPacketType::Host,
            link_header_len: 14,
            address: [0; 8],
            address_len: 6,
        };
        let canonical = [
            0x02, 0x01, 0x02, 0x03, 0x04, 0x05, // destination
            0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, // source
            0x08, 0x00, // inner protocol at the normal Ethernet offset
            0x45, 0x00, 0x00, 0x14,
        ];
        // Packet filters return a snap length, so make the test input large
        // enough that metadata values are not clamped to this tiny fixture.
        let mut packet = alloc::vec![0_u8; 0x1_0000];
        packet[..canonical.len()].copy_from_slice(&canonical);

        for (tpid, tci) in [(0x8100, 0x0064), (0x88a8, 0x0123)] {
            let ancillary = PacketAncillaryMetadata::canonical().with_vlan(tci, true, tpid);
            let context = PacketFilterContext::new(&hot, &ancillary);
            for (field, expected) in [
                (Ancillary::Protocol, 0x0800),
                (Ancillary::VlanTag, tci),
                (Ancillary::VlanTagPresent, 1),
                (Ancillary::VlanTpid, tpid),
            ] {
                let metadata_filter = PacketCbpfFilter::try_new_with_policy(
                    alloc::vec![
                        Instruction::statement(axcbpf::opcode::LD_W_ABS, field.encoded_offset(),),
                        Instruction::statement(axcbpf::opcode::ALU_AND_K, 0xffff),
                        Instruction::statement(axcbpf::opcode::RET_A, 0),
                    ],
                    ExecutorPolicy::Interpreter,
                )
                .unwrap();
                assert_eq!(
                    PacketFilter::filter(metadata_filter.as_ref(), &packet, context).unwrap(),
                    expected as usize,
                    "{field:?} interpreter for TPID {tpid:#x}"
                );

                // The kernel JIT arena is initialized during the real kernel
                // boot path, not by the host unit-test harness. Native
                // packet-context execution is covered by the axcbpf host
                // differential suite; this branch runs when the integration
                // test is built for the kernel target.
                #[cfg(all(feature = "bpf", target_os = "none"))]
                {
                    let metadata_filter = PacketCbpfFilter::try_new_with_policy(
                        alloc::vec![
                            Instruction::statement(
                                axcbpf::opcode::LD_W_ABS,
                                field.encoded_offset(),
                            ),
                            Instruction::statement(axcbpf::opcode::ALU_AND_K, 0xffff),
                            Instruction::statement(axcbpf::opcode::RET_A, 0),
                        ],
                        ExecutorPolicy::Jit,
                    )
                    .unwrap();
                    assert_eq!(
                        PacketFilter::filter(metadata_filter.as_ref(), &packet, context).unwrap(),
                        expected as usize,
                        "{field:?} JIT for TPID {tpid:#x}"
                    );
                }
            }

            // A byte load at offset 12/13 sees the inner EtherType, proving
            // the cBPF byte view and the ancillary view are separate.
            let bytes_filter = PacketCbpfFilter::try_new_with_policy(
                alloc::vec![
                    Instruction::statement(axcbpf::opcode::LD_H_ABS, 12),
                    Instruction::statement(axcbpf::opcode::RET_A, 0),
                ],
                ExecutorPolicy::Interpreter,
            )
            .unwrap();
            assert_eq!(
                PacketFilter::filter(bytes_filter.as_ref(), &packet, context).unwrap(),
                0x0800
            );
            #[cfg(all(feature = "bpf", target_os = "none"))]
            {
                let bytes_filter = PacketCbpfFilter::try_new_with_policy(
                    alloc::vec![
                        Instruction::statement(axcbpf::opcode::LD_H_ABS, 12),
                        Instruction::statement(axcbpf::opcode::RET_A, 0),
                    ],
                    ExecutorPolicy::Jit,
                )
                .unwrap();
                assert_eq!(
                    PacketFilter::filter(bytes_filter.as_ref(), &packet, context).unwrap(),
                    0x0800
                );
            }
        }
    }

    #[test]
    fn interpreter_executor_is_fixed_before_packet_filter_publication() {
        let before = counters();
        let filter = PacketCbpfFilter::try_new_with_policy(
            alloc::vec![Instruction::statement(axcbpf::opcode::RET_K, u32::MAX)],
            ExecutorPolicy::Interpreter,
        )
        .unwrap();
        let before_execution = counters();
        assert_eq!(before_execution.published, before.published);
        assert_eq!(filter.filter(&[1, 2, 3]).unwrap(), 3);
        let after = counters();
        assert!(after.interpreter_executed > before_execution.interpreter_executed);
    }

    #[test]
    fn failed_publication_reservation_is_not_observable() {
        let before = counters();
        let reservation = try_reserve_published().unwrap();
        assert!(load_counters().published > before.published);
        drop(reservation);
        assert_eq!(counters().published, before.published);
    }

    #[test]
    fn publication_is_reserved_before_execution_can_be_counted() {
        let before = counters();
        let reservation = try_reserve_published().unwrap();
        assert!(load_counters().published >= before.published.saturating_add(1));
        record_interpreter_executed();
        reservation.commit();
        let after = counters();
        assert!(after.published >= before.published.saturating_add(1));
        assert!(after.interpreter_executed >= before.interpreter_executed.saturating_add(1));
    }

    #[cfg(feature = "test-io-control")]
    #[test]
    fn control_policy_changes_only_future_packet_admissions() {
        let old = crate::seccomp_jit::executor_policies();
        crate::seccomp_jit::set_executor_policies_for_control(
            None,
            Some(ExecutorPolicy::Interpreter),
        );
        let old_filter = PacketCbpfFilter::try_new(alloc::vec![Instruction::statement(
            axcbpf::opcode::RET_K,
            u32::MAX,
        )])
        .unwrap();
        crate::seccomp_jit::set_executor_policies_for_control(None, Some(ExecutorPolicy::Jit));
        let new_filter = PacketCbpfFilter::try_new(alloc::vec![Instruction::statement(
            axcbpf::opcode::RET_K,
            u32::MAX,
        )]);

        let before = counters();
        old_filter.filter(&[1, 2, 3]).unwrap();
        let after = counters();
        assert_eq!(
            after.interpreter_executed,
            before.interpreter_executed.saturating_add(1)
        );
        assert_eq!(after.native_executed, before.native_executed);
        #[cfg(feature = "bpf")]
        if let Ok(filter) = new_filter {
            let before = counters();
            filter.filter(&[1, 2, 3]).unwrap();
            assert_eq!(
                counters().native_executed,
                before.native_executed.saturating_add(1)
            );
        }
        #[cfg(not(feature = "bpf"))]
        match new_filter {
            Err(error) => assert_eq!(error, LinuxError::EOPNOTSUPP.into()),
            Ok(_) => panic!("force-jit admitted an interpreter packet filter"),
        }

        crate::seccomp_jit::set_executor_policies_for_control(Some(old.0), Some(old.1));
    }

    #[cfg(not(feature = "bpf"))]
    #[test]
    fn force_jit_rejects_instead_of_using_the_interpreter() {
        let result = PacketCbpfFilter::try_new_with_policy(
            alloc::vec![Instruction::statement(axcbpf::opcode::RET_K, u32::MAX)],
            ExecutorPolicy::Jit,
        );
        match result {
            Err(error) => assert_eq!(error, LinuxError::EOPNOTSUPP.into()),
            Ok(_) => panic!("force-jit admitted an interpreter packet filter"),
        }
    }
}
