//! The first TheKernel/vISA continuation adapter.
//!
//! This module is deliberately a control-path seam.  The vISA coordinator
//! owns the portable continuation record while the parent `world` module owns
//! provider generations, native UTS namespaces, execution fences, and
//! ProcessData publication.  The adapter keeps only bounded, opaque tokens
//! between coordinator calls; it does not maintain a second provider ledger.

#![allow(clippy::large_enum_variant)]

#[cfg(test)]
extern crate std;

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::fmt;
#[cfg(test)]
use core::sync::atomic::{AtomicBool, Ordering};

use axerrno::{AxError, AxResult};
use axsync::spin::SpinNoIrq;
use spin::Once;
use visa_coordinator::{
    self as coordinator, AbortPreparationRequest, ActivateRequest, AuthorityBinding, AuthorityPort,
    CallOutcome, CaptureDurability, CaptureRequest, CapturedRuntime, CapturedSnapshot,
    CommitRequest, Coordinator, CoordinatorError, DriveResult, FreezeSourceRequest, FrozenRuntime,
    PrepareDestinationRequest, PrepareRequest, QueryAbortRequest, QueryActivationRequest,
    QueryCaptureRequest, QueryCommitRequest, QueryOutcome, QueryPrepareRequest, RecordStore,
    RestoreDestinationRequest, RestoreSourceRequest, RuntimePort,
};
use visa_core::{
    self, AbortPreparationReceipt, ActivationReceipt as VisaActivationReceipt,
    AuthorityCommitReceipt, BindingPreparationReceipt, CaptureReceipt, ContinuationId,
    ContinuationIntent, ContinuationPhase, Digest, ExternalCoordinate, ExternalOperationKind,
    LineagePoint, OperationId, PortableSnapshot, ProfileId, ProfileRef, ProfileVersion, Progress,
    RebindDisposition, ResourceRequirement, Rights, SafePointReceipt, SchemaId, SchemaRef, ScopeId,
    SnapshotEnvelope, SnapshotId, SourceRestorationReceipt,
};

use super::{
    ActivationReceipt as WorldActivationReceipt, Authority, AuthorityOperationId, AuthorityQuery,
    CapturedProviderState, CommittedBinding, GenerationHandle, OperationToken, PrepareOutcome,
    PreparedBinding, SemanticWorld, UtsProviderSnapshot,
};
use crate::task::{ProcessData, UTS_FIELD_LEN};

const WORKFLOW_CAPACITY: usize = 8;
const NATIVE_ROW_CAPACITY: usize = WORKFLOW_CAPACITY * 4;
const MAX_DRIVE_STEPS: usize = 32;
const UTS_CODEC_VERSION: u8 = 1;
const UTS_CODEC_MAX_BYTES: usize = 1 + 1 + UTS_FIELD_LEN + 1 + UTS_FIELD_LEN;
const UTS_CAPTURE_DURABILITY: CaptureDurability = CaptureDurability::ProcessLocal;

const VISA_AUTHORITY: visa_core::AuthorityId = visa_core::AuthorityId::from_u128(1);
const UTS_PROFILE: ProfileId = ProfileId::from_u128(0x5554_532d_5052_4f46_494c_4501);
const UTS_SCHEMA: SchemaId = SchemaId::from_u128(0x5554_532d_5354_4154_4501);
const UTS_SCOPE: ScopeId = ScopeId::from_u128(0x5554_532d_5343_4f50_4501);

static LOCAL_WORKFLOW: Once<SpinNoIrq<Option<BoundedWorkflowStore>>> = Once::new();
static LOCAL_NATIVE: Once<Arc<SpinNoIrq<UtsNativeState>>> = Once::new();

/// Canonical UTS logical state.  The version and both lengths are explicit;
/// no native integer layout, pointer, namespace owner, or provider handle is
/// included in these bytes.
pub(crate) fn encode_uts_state(snapshot: UtsProviderSnapshot) -> AxResult<Vec<u8>> {
    let node = snapshot.nodename();
    let domain = snapshot.domainname();
    let total = 1usize
        .checked_add(1 + node.len())
        .and_then(|size| size.checked_add(1 + domain.len()))
        .ok_or(AxError::OutOfRange)?;
    if total > UTS_CODEC_MAX_BYTES
        || node.len() > u8::MAX as usize
        || domain.len() > u8::MAX as usize
    {
        return Err(AxError::InvalidInput);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total)
        .map_err(|_| AxError::NoMemory)?;
    bytes.push(UTS_CODEC_VERSION);
    bytes.push(node.len() as u8);
    bytes.extend_from_slice(node);
    bytes.push(domain.len() as u8);
    bytes.extend_from_slice(domain);
    Ok(bytes)
}

pub(crate) fn decode_uts_state(bytes: &[u8]) -> AxResult<UtsProviderSnapshot> {
    if bytes.len() < 3 || bytes.len() > UTS_CODEC_MAX_BYTES || bytes[0] != UTS_CODEC_VERSION {
        return Err(AxError::InvalidInput);
    }
    let node_len = bytes[1] as usize;
    let node_end = 2usize.checked_add(node_len).ok_or(AxError::InvalidInput)?;
    let domain_len = *bytes.get(node_end).ok_or(AxError::InvalidInput)? as usize;
    let domain_start = node_end.checked_add(1).ok_or(AxError::InvalidInput)?;
    let end = domain_start
        .checked_add(domain_len)
        .ok_or(AxError::InvalidInput)?;
    if end != bytes.len() || node_len > UTS_FIELD_LEN || domain_len > UTS_FIELD_LEN {
        return Err(AxError::InvalidInput);
    }
    UtsProviderSnapshot::from_fields(&bytes[2..node_end], &bytes[domain_start..end])
}

fn contract_error(error: visa_core::ContractError) -> AxError {
    match error {
        visa_core::ContractError::Encoding => AxError::InvalidInput,
        visa_core::ContractError::InvalidLineageAdvance => AxError::InvalidInput,
        _ => AxError::BadState,
    }
}

fn profile() -> ProfileRef {
    ProfileRef {
        id: UTS_PROFILE,
        version: ProfileVersion { major: 1, minor: 0 },
        contract_digest: Digest::of_bytes(b"thekernel.uts.profile.v1"),
        state_schema: SchemaRef {
            id: UTS_SCHEMA,
            version: 1,
        },
    }
}

fn generation_coordinate(handle: &GenerationHandle) -> ExternalCoordinate {
    const PREFIX: &[u8] = b"thekernel.uts/";
    let mut value = Vec::new();
    // This is an opaque coordinate codec, not a native pointer or ABI.  The
    // fixed-width big-endian fields make equality and replay deterministic.
    value
        .try_reserve_exact(PREFIX.len() + 32)
        .expect("small coordinate reservation");
    value.extend_from_slice(PREFIX);
    value.extend_from_slice(&handle.authority.instance_id().0.get().to_be_bytes());
    value.extend_from_slice(&handle.coordinate.world.0.get().to_be_bytes());
    value.extend_from_slice(&handle.coordinate.provider.0.get().to_be_bytes());
    value.extend_from_slice(&handle.coordinate.generation.0.get().to_be_bytes());
    ExternalCoordinate {
        authority: VISA_AUTHORITY,
        value,
    }
}

fn destination_coordinate(source: &ExternalCoordinate) -> ExternalCoordinate {
    let mut value = Vec::new();
    value
        .try_reserve_exact(source.value.len() + 4)
        .expect("small coordinate reservation");
    value.extend_from_slice(&source.value);
    value.extend_from_slice(b"/new");
    ExternalCoordinate {
        authority: source.authority,
        value,
    }
}

fn logical_requirement() -> AxResult<ResourceRequirement> {
    let id = visa_core::RequirementId::from_u128(0x5554_532d_5553_4552_4e53_01);
    let mut kind = Vec::new();
    kind.try_reserve_exact(7).map_err(|_| AxError::NoMemory)?;
    kind.extend_from_slice(b"user-ns");
    let mut logical_name = Vec::new();
    logical_name
        .try_reserve_exact(9)
        .map_err(|_| AxError::NoMemory)?;
    logical_name.extend_from_slice(b"user-ns");
    Ok(ResourceRequirement {
        id,
        kind,
        logical_name,
        required_rights: Rights::default(),
        disposition: RebindDisposition::RetainOld,
        profile_data: Vec::new(),
    })
}

fn snapshot_id(operation: OperationId, state: &[u8]) -> SnapshotId {
    let mut material = Vec::new();
    material.extend_from_slice(&operation.0);
    material.extend_from_slice(state);
    let digest = Digest::of_bytes(&material);
    SnapshotId(
        digest.0[..16]
            .try_into()
            .expect("digest prefix has sixteen bytes"),
    )
}

fn make_logical_capture(
    request: &CaptureRequest,
    captured: &CapturedProviderState,
) -> AxResult<(SnapshotEnvelope, SafePointReceipt, CaptureReceipt)> {
    let state = encode_uts_state(captured.snapshot)?;
    let state_digest = Digest::of_bytes(&state);
    let cut_sequence = 1;
    let safe_point = SafePointReceipt {
        continuation: request.continuation,
        scope: request.scope,
        runtime: request.source.clone(),
        cut_sequence,
        portable_state_digest: state_digest,
        receipt_digest: Digest::ZERO,
    }
    .seal()
    .map_err(contract_error)?;
    let mut resources = Vec::new();
    resources
        .try_reserve_exact(1)
        .map_err(|_| AxError::NoMemory)?;
    resources.push(logical_requirement()?);
    let body = PortableSnapshot {
        snapshot: snapshot_id(request.operation, &state),
        continuation: request.continuation,
        scope: request.scope,
        lineage: request.lineage.clone(),
        profile: request.profile.clone(),
        source_cut: visa_core::SourceSemanticCut {
            runtime: request.source.clone(),
            cut_sequence,
            receipt_digest: safe_point.receipt_digest,
        },
        state,
        state_digest,
        resources,
        effects: Vec::new(),
    };
    let snapshot = SnapshotEnvelope::seal(body).map_err(contract_error)?;
    let receipt = CaptureReceipt {
        operation: request.operation,
        continuation: request.continuation,
        scope: request.scope,
        snapshot: snapshot.body.snapshot,
        source: request.source.clone(),
        profile: request.profile.clone(),
        lineage: request.lineage.clone(),
        state_digest: snapshot.body.state_digest,
        snapshot_digest: snapshot.body_digest,
        safe_point_digest: safe_point.receipt_digest,
        receipt_digest: Digest::ZERO,
    }
    .seal()
    .map_err(contract_error)?;
    Ok((snapshot, safe_point, receipt))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeOperationKey {
    authority: super::AuthorityInstanceId,
    continuation: ContinuationId,
    operation: OperationId,
    stage: ExternalOperationKind,
    digest: Digest,
    preparation_generation: Digest,
}

type NativeCaptureKey = NativeOperationKey;

#[derive(Clone)]
struct NativeCapture {
    key: NativeCaptureKey,
    snapshot: Arc<SnapshotEnvelope>,
    safe_point: Arc<SafePointReceipt>,
    receipt: Arc<CaptureReceipt>,
}

struct NativeCaptureRow {
    capture: NativeCapture,
    frozen: Option<CapturedProviderState>,
    restored: Option<Arc<SourceRestorationReceipt>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeDestinationKey {
    key: NativeOperationKey,
    continuation: ContinuationId,
    snapshot: SnapshotId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActivationKey {
    operation: OperationId,
    binding: Option<ExternalCoordinate>,
}

struct NativeOperation {
    key: NativeOperationKey,
    continuation: ContinuationId,
    /// The complete portable request bound to this external stage. Keeping
    /// this exact value lets retries reject a same-id/different-request
    /// conflict before touching the canonical world operation.
    binding: AuthorityBinding,
    canonical: Option<OperationToken>,
    preparation: Option<Arc<BindingPreparationReceipt>>,
    prepared: Option<PreparedBinding>,
    committed: Option<CommittedBinding>,
    world_receipt: Option<WorldActivationReceipt>,
    /// Keep only the returned destination consumer alive for a ProcessData
    /// that was constructed before Semantic World lease initialization.
    /// The destination handle itself is released at activation publication;
    /// normal processes retain their consumer lease in ProcessData.
    active_consumer: Option<super::UtsConsumerLease>,
    commit: Option<Arc<AuthorityCommitReceipt>>,
    abort: Option<Arc<AbortPreparationReceipt>>,
    activation_key: Option<Arc<ActivationKey>>,
    activation: Option<Arc<VisaActivationReceipt>>,
}

#[derive(Default)]
struct NativeFaults {
    capture_lost_ack: bool,
    prepare_post_take_failure: bool,
    commit_receipt_failure: bool,
    commit_capacity_failure: bool,
    commit_world_query_unknown: bool,
    commit_lost_ack: bool,
    activation_lost_ack: bool,
    #[cfg(test)]
    commit_force_failure: bool,
    #[cfg(test)]
    commit_failure_gate: Option<Arc<TestGate>>,
    #[cfg(test)]
    commit_publication_gate: Option<Arc<TestGate>>,
    #[cfg(test)]
    activation_publication_gate: Option<Arc<TestGate>>,
    #[cfg(test)]
    abort_lookup_gate: Option<Arc<TestGate>>,
}

#[cfg(test)]
struct TestGate {
    entered: AtomicBool,
    release: AtomicBool,
}

#[cfg(test)]
impl TestGate {
    fn new() -> Self {
        Self {
            entered: AtomicBool::new(false),
            release: AtomicBool::new(false),
        }
    }

    fn pause(&self) {
        self.entered.store(true, Ordering::Release);
        while !self.release.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
    }

    fn wait_until_entered(&self) {
        for _ in 0..1_000_000 {
            if self.entered.load(Ordering::Acquire) {
                return;
            }
            std::thread::yield_now();
        }
        panic!("commit failure gate was not reached");
    }

    fn release(&self) {
        self.release.store(true, Ordering::Release);
    }
}

struct UtsNativeState {
    captures: Vec<NativeCaptureRow>,
    operations: Vec<NativeOperation>,
    faults: NativeFaults,
}

impl UtsNativeState {
    fn try_new() -> AxResult<Self> {
        let mut captures = Vec::new();
        captures
            .try_reserve_exact(NATIVE_ROW_CAPACITY)
            .map_err(|_| AxError::NoMemory)?;
        let mut operations = Vec::new();
        operations
            .try_reserve_exact(NATIVE_ROW_CAPACITY)
            .map_err(|_| AxError::NoMemory)?;
        Ok(Self {
            captures,
            operations,
            faults: NativeFaults::default(),
        })
    }

    fn capture_index(&self, key: NativeCaptureKey) -> Option<usize> {
        self.captures.iter().position(|row| row.capture.key == key)
    }

    fn operation_index(&self, key: &NativeOperationKey) -> Option<usize> {
        self.operations.iter().position(|row| &row.key == key)
    }

    /// A caller-supplied external operation id is a conflict if it already
    /// names a row with any other authority instance, digest, or preparation
    /// generation. This is only a rejection check; all row selection still
    /// uses the complete key above.
    fn has_conflicting_operation(&self, key: &NativeOperationKey) -> bool {
        self.operations.iter().any(|row| {
            row.key.authority == key.authority
                && row.key.continuation == key.continuation
                && row.key.operation == key.operation
                && row.key.stage == key.stage
                && row.key != *key
        })
    }

    fn preparation_index_exact(
        &self,
        key: &NativeOperationKey,
        binding: &AuthorityBinding,
        preparation: &BindingPreparationReceipt,
    ) -> Option<usize> {
        self.operations.iter().position(|row| {
            &row.key == key
                && row.binding == *binding
                && row.preparation.as_deref() == Some(preparation)
        })
    }

    fn remove_operation_exact(&mut self, key: &NativeOperationKey) -> bool {
        let Some(index) = self.operations.iter().position(|row| &row.key == key) else {
            return false;
        };
        self.operations.remove(index);
        true
    }

    fn remove_preparation_exact(
        &mut self,
        key: &NativeOperationKey,
        binding: &AuthorityBinding,
        preparation: &BindingPreparationReceipt,
    ) -> bool {
        let Some(index) = self.preparation_index_exact(key, binding, preparation) else {
            return false;
        };
        self.operations.remove(index);
        true
    }

    fn ensure_capacity<T>(rows: &Vec<T>) -> Result<(), UtsAuthorityRejection> {
        (rows.len() < NATIVE_ROW_CAPACITY)
            .then_some(())
            .ok_or(UtsAuthorityRejection::Capacity)
    }
}

fn native_state() -> AxResult<Arc<SpinNoIrq<UtsNativeState>>> {
    LOCAL_NATIVE
        .try_call_once(|| UtsNativeState::try_new().map(|state| Arc::new(SpinNoIrq::new(state))))
        .map_err(|_| AxError::BadState)
        .cloned()
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum UtsAuthorityRejection {
    Invalid,
    Conflict,
    Busy,
    Missing,
    /// The process-local native projection no longer contains the opaque
    /// state needed to decide an operation. The coordinator must retain a
    /// recovery requirement instead of treating this as an authoritative
    /// absence.
    AuthorityUnavailable,
    Capacity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum UtsRuntimeError {
    Invalid,
    Busy,
    /// Local native state was lost before the runtime could establish the
    /// requested fact. This is an unknown outcome, not a confirmed absence.
    AuthorityUnavailable,
    Conflict,
}

fn rejection(error: &UtsAuthorityRejection) -> String {
    format!("{error:?}")
}

fn runtime_error(error: &UtsRuntimeError) -> String {
    format!("{error:?}")
}

/// A bounded in-memory record/lineage store for the embedded coordinator.
/// Slots are intentionally retained through terminal state so a repeated
/// external operation observes its original process-local result instead of
/// a recycled continuation id. This store is not a restart or crash-recovery
/// authority.
pub(crate) struct BoundedWorkflowStore {
    records: Vec<Option<visa_core::ContinuationRecord>>,
    lineages: Vec<Option<LineageSlot>>,
}

struct LineageSlot {
    lineage: visa_core::LineageId,
    head: LineagePoint,
    active: Option<ContinuationId>,
}

impl BoundedWorkflowStore {
    pub(crate) fn try_new() -> AxResult<Self> {
        let mut records = Vec::new();
        records
            .try_reserve_exact(WORKFLOW_CAPACITY)
            .map_err(|_| AxError::NoMemory)?;
        let mut lineages = Vec::new();
        lineages
            .try_reserve_exact(WORKFLOW_CAPACITY)
            .map_err(|_| AxError::NoMemory)?;
        Ok(Self { records, lineages })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkflowStoreError {
    AlreadyExists,
    NotFound,
    CasConflict,
    LineageFork,
    Capacity,
    NoMemory,
}

impl RecordStore for BoundedWorkflowStore {
    type Error = WorkflowStoreError;

    fn create(
        &mut self,
        request: coordinator::CreateRecord,
    ) -> Result<visa_core::ContinuationRecord, Self::Error> {
        let id = request.record.intent.id;
        if self
            .records
            .iter()
            .flatten()
            .any(|record| record.intent.id == id)
        {
            return Err(WorkflowStoreError::AlreadyExists);
        }
        if let Some(lineage) = self
            .lineages
            .iter()
            .flatten()
            .find(|lineage| lineage.lineage == request.lineage.parent.lineage)
            && (lineage.head != request.lineage.parent || lineage.active.is_some())
        {
            return Err(WorkflowStoreError::LineageFork);
        }
        let slot = self
            .records
            .iter()
            .position(Option::is_none)
            .unwrap_or(self.records.len());
        if slot >= WORKFLOW_CAPACITY {
            return Err(WorkflowStoreError::Capacity);
        }
        if slot == self.records.len() {
            self.records.push(None);
        }
        self.records[slot] = Some(request.record.clone());
        let lineage_slot = self
            .lineages
            .iter()
            .position(Option::is_none)
            .unwrap_or(self.lineages.len());
        if lineage_slot >= WORKFLOW_CAPACITY {
            self.records[slot] = None;
            return Err(WorkflowStoreError::Capacity);
        }
        if lineage_slot == self.lineages.len() {
            self.lineages.push(None);
        }
        self.lineages[lineage_slot] = Some(LineageSlot {
            lineage: request.lineage.parent.lineage,
            head: request.lineage.parent,
            active: Some(request.lineage.active_continuation),
        });
        Ok(request.record)
    }

    fn load(
        &self,
        continuation: &ContinuationId,
    ) -> Result<Option<visa_core::ContinuationRecord>, Self::Error> {
        Ok(self
            .records
            .iter()
            .flatten()
            .find(|record| &record.intent.id == continuation)
            .cloned())
    }

    fn cas(
        &mut self,
        continuation: &ContinuationId,
        expected: &visa_core::ContinuationRecord,
        next: visa_core::ContinuationRecord,
        lineage: Option<coordinator::LineageUpdate>,
    ) -> Result<visa_core::ContinuationRecord, Self::Error> {
        let record_index = self
            .records
            .iter()
            .position(|record| {
                record
                    .as_ref()
                    .is_some_and(|record| &record.intent.id == continuation)
            })
            .ok_or(WorkflowStoreError::NotFound)?;
        let current = self.records[record_index]
            .as_ref()
            .ok_or(WorkflowStoreError::NotFound)?;
        if current != expected || expected.revision.checked_add(1) != Some(next.revision) {
            return Err(WorkflowStoreError::CasConflict);
        }
        if let Some(update) = lineage {
            let line = self
                .lineages
                .iter_mut()
                .flatten()
                .find(|line| line.lineage == update.lineage)
                .ok_or(WorkflowStoreError::LineageFork)?;
            if line.head != update.expected_head || line.active != update.expected_active {
                return Err(WorkflowStoreError::LineageFork);
            }
            line.head = update.new_head;
            line.active = update.active_continuation;
        }
        self.records[record_index] = Some(next.clone());
        Ok(next)
    }

    fn discover_unfinished(&self) -> Result<Vec<ContinuationId>, Self::Error> {
        let mut result = Vec::new();
        result
            .try_reserve(self.records.len())
            .map_err(|_| WorkflowStoreError::NoMemory)?;
        for record in self.records.iter().flatten() {
            if !coordinator::record_is_terminal(record) {
                result.push(record.intent.id);
            }
        }
        Ok(result)
    }
}

struct UtsAuthorityAdapter {
    world: SemanticWorld,
    shared: Arc<SpinNoIrq<UtsNativeState>>,
}

impl UtsAuthorityAdapter {
    fn new(world: SemanticWorld, shared: Arc<SpinNoIrq<UtsNativeState>>) -> Self {
        Self { world, shared }
    }

    fn operation_digest(binding: &AuthorityBinding) -> Digest {
        binding.preparation_digest
    }

    fn operation_key(
        &self,
        continuation: ContinuationId,
        operation: OperationId,
        stage: ExternalOperationKind,
        digest: Digest,
        preparation_generation: Digest,
    ) -> NativeOperationKey {
        NativeOperationKey {
            authority: self.world.authority.instance_id(),
            continuation,
            operation,
            stage,
            digest,
            preparation_generation,
        }
    }

    fn lookup_exact(
        state: &UtsNativeState,
        key: &NativeOperationKey,
    ) -> Result<Option<usize>, UtsAuthorityRejection> {
        Ok(state.operation_index(key))
    }

    fn preparation_receipt(
        operation: OperationId,
        binding: &AuthorityBinding,
    ) -> Result<BindingPreparationReceipt, UtsAuthorityRejection> {
        BindingPreparationReceipt {
            operation,
            continuation: binding.continuation,
            snapshot: binding.snapshot,
            snapshot_digest: binding.preparation_digest,
            destination: binding.destination.clone(),
            grants: Vec::new(),
            receipt_digest: Digest::ZERO,
        }
        .seal()
        .map_err(|_| UtsAuthorityRejection::Invalid)
    }

    fn commit_receipt(
        operation: OperationId,
        binding: &AuthorityBinding,
        preparation: &BindingPreparationReceipt,
        source_fence_epoch: u64,
        execution_epoch: u64,
    ) -> Result<AuthorityCommitReceipt, UtsAuthorityRejection> {
        AuthorityCommitReceipt {
            operation,
            continuation: binding.continuation,
            snapshot: binding.snapshot,
            snapshot_digest: binding.preparation_digest,
            source: binding.source.clone(),
            source_fence_epoch,
            destination: binding.destination.clone(),
            binding_receipt_digest: preparation.receipt_digest,
            execution_epoch,
            receipt_digest: Digest::ZERO,
        }
        .seal()
        .map_err(|_| UtsAuthorityRejection::Invalid)
    }

    fn abort_receipt(
        operation: OperationId,
        binding: &AuthorityBinding,
        preparation: &BindingPreparationReceipt,
    ) -> Result<AbortPreparationReceipt, UtsAuthorityRejection> {
        AbortPreparationReceipt {
            operation,
            continuation: binding.continuation,
            snapshot: binding.snapshot,
            snapshot_digest: binding.preparation_digest,
            source: binding.source.clone(),
            destination: binding.destination.clone(),
            preparation_receipt_digest: preparation.receipt_digest,
            receipt_digest: Digest::ZERO,
        }
        .seal()
        .map_err(|_| UtsAuthorityRejection::Invalid)
    }

    fn same_capture(
        capture: &NativeCapture,
        binding: &AuthorityBinding,
        expected_destination: &ExternalCoordinate,
    ) -> bool {
        capture.snapshot.body.snapshot == binding.snapshot
            && capture.snapshot.body_digest == binding.preparation_digest
            && capture.snapshot.body.source_cut.runtime == binding.source
            && capture.snapshot.body.resources == binding.requirements
            && binding.destination == *expected_destination
            && binding
                .capture_receipt
                .as_ref()
                .map_or(true, |receipt| receipt == capture.receipt.as_ref())
    }

    fn capture_key(
        &self,
        binding: &AuthorityBinding,
    ) -> Result<NativeCaptureKey, UtsAuthorityRejection> {
        let authority = self.world.authority.instance_id();
        if let Some(receipt) = binding.capture_receipt.as_ref() {
            return Ok(NativeCaptureKey {
                authority,
                continuation: binding.continuation,
                operation: receipt.operation,
                stage: ExternalOperationKind::CaptureSource,
                digest: Digest::ZERO,
                preparation_generation: Digest::ZERO,
            });
        }

        // A process-local capture intentionally has no coordinator-visible
        // receipt. Resolve its opaque row only while this process still owns
        // the native projection; a missing row is an unknown authority fact.
        let expected_destination = destination_coordinate(&binding.source);
        let state = self.shared.lock();
        let mut candidate = None;
        for row in &state.captures {
            if row.capture.key.authority != authority
                || row.capture.key.continuation != binding.continuation
                || !Self::same_capture(&row.capture, binding, &expected_destination)
            {
                continue;
            }
            if candidate.replace(row.capture.key).is_some() {
                return Err(UtsAuthorityRejection::Conflict);
            }
        }
        candidate.ok_or(UtsAuthorityRejection::AuthorityUnavailable)
    }

    fn capture_generation(binding: &AuthorityBinding) -> Digest {
        binding
            .capture_receipt
            .as_ref()
            .map_or(Digest::ZERO, |receipt| receipt.receipt_digest)
    }

    fn put_captured(
        &self,
        key: NativeCaptureKey,
        captured: CapturedProviderState,
    ) -> Result<(), UtsAuthorityRejection> {
        let mut captured = Some(captured);
        let result = {
            let mut state = self.shared.lock();
            let Some(index) = state.capture_index(key) else {
                return {
                    drop(state);
                    drop(captured);
                    Err(UtsAuthorityRejection::Missing)
                };
            };
            let row = &mut state.captures[index];
            if row.restored.is_some() || row.frozen.is_some() {
                Err(UtsAuthorityRejection::Busy)
            } else {
                row.frozen = Some(captured.take().expect("capture token missing"));
                Ok(())
            }
        };
        // A failed publication retains and drops the opaque token only after
        // the native ledger lock is out of scope; dropping it under SpinNoIrq
        // would reopen the source fence while the row still says otherwise.
        drop(captured);
        result
    }

    /// Abort a canonical prepared binding and put its exact opaque source
    /// fence back in the process-local capture row. The authority binding owns the
    /// fence after preparation; simply dropping it would reopen the source
    /// while losing the token needed for an exact later restoration fact.
    fn rollback_prepared(
        &self,
        key: NativeCaptureKey,
        prepared: PreparedBinding,
    ) -> Result<(), UtsAuthorityRejection> {
        let captured = prepared.abort_prepared();
        self.put_captured(key, captured)
    }

    fn query_world_receipt(
        authority: &Arc<Authority>,
        operation: AuthorityOperationId,
        digest: Digest,
    ) -> Option<WorldActivationReceipt> {
        match authority
            .query_handle()
            .query_operation(operation, digest.0)
        {
            AuthorityQuery::Applied(receipt) => Some(receipt),
            AuthorityQuery::Pending
            | AuthorityQuery::Absent
            | AuthorityQuery::Rejected
            | AuthorityQuery::Conflict
            | AuthorityQuery::AuthorityGone => None,
        }
    }

    /// Reconciles the internal world activation receipt after a commit row
    /// was durably published with an unusual query view.  The native vISA
    /// commit receipt remains authoritative for external idempotency; this
    /// helper only fills the process-runtime edge once the exact canonical
    /// operation query says Applied.
    fn publish_world_receipt(
        authority: &Arc<Authority>,
        shared: &Arc<SpinNoIrq<UtsNativeState>>,
        continuation: ContinuationId,
        digest: Digest,
        external_operation: OperationId,
        preparation_generation: Digest,
        operation: AuthorityOperationId,
    ) -> Option<WorldActivationReceipt> {
        let receipt = Self::query_world_receipt(authority, operation, digest)?;
        let key = NativeOperationKey {
            authority: authority.instance_id(),
            continuation,
            operation: external_operation,
            stage: ExternalOperationKind::CommitAuthority,
            digest,
            preparation_generation,
        };
        let mut state = shared.lock();
        let index = state.operation_index(&key)?;
        let row = &mut state.operations[index];
        if row.world_receipt.is_none() {
            row.world_receipt = Some(receipt.clone());
        }
        row.world_receipt.clone()
    }

    fn ensure_preparation(
        &mut self,
        request: &PrepareRequest,
    ) -> Result<BindingPreparationReceipt, UtsAuthorityRejection> {
        let digest = Self::operation_digest(&request.binding);
        if let Some(capture_receipt) = request.binding.capture_receipt.as_ref() {
            if capture_receipt.verify().is_err() {
                return Err(UtsAuthorityRejection::Invalid);
            }
        }
        let expected_destination = destination_coordinate(&request.binding.source);
        let capture_key = self.capture_key(&request.binding)?;
        let prepare_key = self.operation_key(
            request.binding.continuation,
            request.operation,
            ExternalOperationKind::PrepareBindings,
            digest,
            Self::capture_generation(&request.binding),
        );
        let existing_preparation = {
            let state = self.shared.lock();
            if state.has_conflicting_operation(&prepare_key) {
                return Err(UtsAuthorityRejection::Conflict);
            }
            if let Some(index) = Self::lookup_exact(&state, &prepare_key)? {
                if state.operations[index].binding != request.binding {
                    return Err(UtsAuthorityRejection::Conflict);
                }
                Some(
                    state.operations[index]
                        .preparation
                        .clone()
                        .ok_or(UtsAuthorityRejection::Busy)?,
                )
            } else {
                None
            }
        };
        if let Some(receipt) = existing_preparation {
            return Ok(receipt.as_ref().clone());
        }
        {
            let state = self.shared.lock();
            UtsNativeState::ensure_capacity(&state.operations)?;
        }
        let canonical =
            Authority::allocate_operation_id().map_err(|_| UtsAuthorityRejection::Busy)?;
        let token = self
            .world
            .authority
            .reserve_operation_with_id(canonical, digest.0)
            .map_err(|_| UtsAuthorityRejection::Busy)?;
        let captured_result = {
            let mut state = self.shared.lock();
            match state.capture_index(capture_key) {
                None => Err(UtsAuthorityRejection::Missing),
                Some(capture_index)
                    if !Self::same_capture(
                        &state.captures[capture_index].capture,
                        &request.binding,
                        &expected_destination,
                    ) =>
                {
                    Err(UtsAuthorityRejection::Conflict)
                }
                Some(capture_index) => state.captures[capture_index]
                    .frozen
                    .take()
                    .ok_or(UtsAuthorityRejection::Busy),
            }
        };
        let captured = match captured_result {
            Ok(captured) => captured,
            Err(error) => {
                let _ = self.world.cancel_operation(&token, digest.0);
                return Err(error);
            }
        };
        let (prepare_result, remaining_capture) = self
            .world
            .prepare_uts_binding_preserving_capture(&token, digest.0, captured);
        let prepared = match (prepare_result, remaining_capture) {
            (Ok(PrepareOutcome::Prepared(binding)), None) => binding,
            (Ok(PrepareOutcome::AlreadyPending), Some(captured)) => {
                let _ = self.put_captured(capture_key, captured);
                let _ = self.world.cancel_operation(&token, digest.0);
                return Err(UtsAuthorityRejection::Busy);
            }
            (Ok(PrepareOutcome::Applied(_)), Some(captured)) => {
                let _ = self.put_captured(capture_key, captured);
                let _ = self.world.cancel_operation(&token, digest.0);
                return Err(UtsAuthorityRejection::Conflict);
            }
            (Ok(PrepareOutcome::Rejected | PrepareOutcome::Conflict), Some(captured)) => {
                let _ = self.put_captured(capture_key, captured);
                let _ = self.world.cancel_operation(&token, digest.0);
                return Err(UtsAuthorityRejection::Conflict);
            }
            (Err(_), Some(captured)) => {
                let _ = self.put_captured(capture_key, captured);
                let _ = self.world.cancel_operation(&token, digest.0);
                return Err(UtsAuthorityRejection::Busy);
            }
            (Ok(PrepareOutcome::Prepared(binding)), Some(captured)) => {
                let _ = self.rollback_prepared(capture_key, binding);
                let _ = self.put_captured(capture_key, captured);
                let _ = self.world.cancel_operation(&token, digest.0);
                return Err(UtsAuthorityRejection::Busy);
            }
            (Ok(_), None) | (Err(_), None) => {
                let _ = self.world.cancel_operation(&token, digest.0);
                return Err(UtsAuthorityRejection::Busy);
            }
        };
        // Fault injection models a receipt/publication failure after the
        // source token has already moved into the canonical PreparedBinding.
        // Every such path must run the same exact abort-and-restore guard.
        let fail_after_take = {
            let mut state = self.shared.lock();
            let failure = state.faults.prepare_post_take_failure;
            state.faults.prepare_post_take_failure = false;
            failure
        };
        if fail_after_take {
            self.rollback_prepared(capture_key, prepared)
                .map_err(|_| UtsAuthorityRejection::Busy)?;
            return Err(UtsAuthorityRejection::Busy);
        }
        let preparation = match Self::preparation_receipt(request.operation, &request.binding) {
            Ok(preparation) => preparation,
            Err(error) => {
                self.rollback_prepared(capture_key, prepared)
                    .map_err(|_| UtsAuthorityRejection::Busy)?;
                return Err(error);
            }
        };
        let row_binding = request.binding.clone();
        let row_preparation = match Arc::try_new(preparation.clone()) {
            Ok(receipt) => receipt,
            Err(_) => {
                self.rollback_prepared(capture_key, prepared)
                    .map_err(|_| UtsAuthorityRejection::Busy)?;
                return Err(UtsAuthorityRejection::Busy);
            }
        };
        let mut state = self.shared.lock();
        if let Err(error) = UtsNativeState::ensure_capacity(&state.operations) {
            drop(state);
            self.rollback_prepared(capture_key, prepared)
                .map_err(|_| UtsAuthorityRejection::Busy)?;
            return Err(error);
        }
        state.operations.push(NativeOperation {
            key: prepare_key,
            continuation: request.binding.continuation,
            binding: row_binding,
            canonical: Some(token),
            preparation: Some(row_preparation),
            prepared: Some(prepared),
            committed: None,
            world_receipt: None,
            active_consumer: None,
            commit: None,
            abort: None,
            activation_key: None,
            activation: None,
        });
        Ok(preparation)
    }

    fn commit_for(
        &mut self,
        request: &CommitRequest,
    ) -> Result<AuthorityCommitReceipt, UtsAuthorityRejection> {
        let digest = Self::operation_digest(&request.binding);
        if request.preparation.verify().is_err() {
            return Err(UtsAuthorityRejection::Invalid);
        }
        if let Some(capture_receipt) = request.binding.capture_receipt.as_ref() {
            if capture_receipt.verify().is_err() {
                return Err(UtsAuthorityRejection::Invalid);
            }
        }
        let prepare_key = self.operation_key(
            request.binding.continuation,
            request.preparation.operation,
            ExternalOperationKind::PrepareBindings,
            digest,
            Self::capture_generation(&request.binding),
        );
        let commit_key = self.operation_key(
            request.binding.continuation,
            request.operation,
            ExternalOperationKind::CommitAuthority,
            digest,
            request.preparation.receipt_digest,
        );
        release_cancelled_commit(&self.world.authority, &self.shared, &commit_key);
        let existing_commit = {
            let state = self.shared.lock();
            if state.has_conflicting_operation(&commit_key) {
                return Err(UtsAuthorityRejection::Conflict);
            }
            if let Some(index) = Self::lookup_exact(&state, &commit_key)? {
                if state.operations[index].binding != request.binding
                    || state.operations[index].preparation.as_deref() != Some(&request.preparation)
                {
                    return Err(UtsAuthorityRejection::Conflict);
                }
                Some(
                    state.operations[index]
                        .commit
                        .clone()
                        .ok_or(UtsAuthorityRejection::Busy)?,
                )
            } else {
                None
            }
        };
        if let Some(commit) = existing_commit {
            return Ok(commit.as_ref().clone());
        }
        let (canonical, preparation, source_fence_epoch, execution_epoch) = {
            let state = self.shared.lock();
            let prep_index = state
                .operation_index(&prepare_key)
                .ok_or(UtsAuthorityRejection::Missing)?;
            let row = &state.operations[prep_index];
            if row.binding != request.binding {
                return Err(UtsAuthorityRejection::Conflict);
            }
            let preparation = row
                .preparation
                .clone()
                .ok_or(UtsAuthorityRejection::Missing)?;
            if preparation.as_ref() != &request.preparation {
                return Err(UtsAuthorityRejection::Conflict);
            }
            let canonical = row.canonical.clone();
            let prepared = row.prepared.as_ref().ok_or(UtsAuthorityRejection::Busy)?;
            let source_fence_epoch = prepared
                .source_fence_epoch()
                .ok_or(UtsAuthorityRejection::Busy)?;
            let execution_epoch = prepared
                .execution_epoch()
                .ok_or(UtsAuthorityRejection::Busy)?;
            (canonical, preparation, source_fence_epoch, execution_epoch)
        };
        let canonical = canonical.ok_or(UtsAuthorityRejection::Busy)?;
        let preparation = preparation.as_ref().clone();

        // Receipt construction is deliberately before the irreversible world
        // transition.  All fields are stable on PreparedBinding, including
        // both fence epochs, so a receipt failure cannot consume the source
        // token or leave a fenced world operation without a native row.
        let receipt_failure = {
            let mut state = self.shared.lock();
            let failure = state.faults.commit_receipt_failure;
            state.faults.commit_receipt_failure = false;
            failure
        };
        if receipt_failure {
            return Err(UtsAuthorityRejection::Invalid);
        }
        let commit = Self::commit_receipt(
            request.operation,
            &request.binding,
            &preparation,
            source_fence_epoch,
            execution_epoch,
        )?;
        // Keep the row's owned receipt separate from the caller's return
        // value.  Cloning here is still pre-commit; publication below only
        // moves already-owned values after the world transition.
        let commit_for_row = match Arc::try_new(commit.clone()) {
            Ok(receipt) => receipt,
            Err(_) => {
                // The receipt allocation is still pre-commit. Keep the
                // prepared token intact and let the caller retry exactly.
                return Err(UtsAuthorityRejection::Busy);
            }
        };
        let commit_row_binding = request.binding.clone();
        let commit_row_preparation = match Arc::try_new(preparation.clone()) {
            Ok(receipt) => receipt,
            Err(_) => return Err(UtsAuthorityRejection::Busy),
        };
        let commit_row_canonical = canonical.clone();
        let capture_key = self.capture_key(&request.binding)?;

        // Reserve the native commit row and move the prepared token out of
        // the preparation row while holding the short native lock.  The
        // placeholder makes an in-flight exact commit query Pending/Busy,
        // never Absent, and the already-capacitated Vec makes publication
        // after world commit infallible.
        let mut prepared = {
            let mut state = self.shared.lock();
            UtsNativeState::ensure_capacity(&state.operations)?;
            let capacity_failure = state.faults.commit_capacity_failure;
            state.faults.commit_capacity_failure = false;
            if capacity_failure {
                return Err(UtsAuthorityRejection::Capacity);
            }
            let prep_index = state
                .operation_index(&prepare_key)
                .ok_or(UtsAuthorityRejection::Missing)?;
            let row = &state.operations[prep_index];
            if row.binding != request.binding {
                return Err(UtsAuthorityRejection::Conflict);
            }
            if row.preparation.as_deref() != Some(&request.preparation) {
                return Err(UtsAuthorityRejection::Conflict);
            }
            let prepared = state.operations[prep_index]
                .prepared
                .take()
                .ok_or(UtsAuthorityRejection::Busy)?;
            state.operations.push(NativeOperation {
                key: commit_key.clone(),
                continuation: request.binding.continuation,
                binding: commit_row_binding,
                canonical: Some(commit_row_canonical),
                preparation: Some(commit_row_preparation),
                prepared: None,
                committed: None,
                world_receipt: None,
                active_consumer: None,
                commit: None,
                abort: None,
                activation_key: None,
                activation: None,
            });
            prepared
        };

        #[cfg(test)]
        {
            let gate = {
                let mut state = self.shared.lock();
                state.faults.commit_failure_gate.take()
            };
            if let Some(gate) = gate {
                gate.pause();
            }
        }
        #[cfg(test)]
        let force_failure = {
            let mut state = self.shared.lock();
            let failure = state.faults.commit_force_failure;
            state.faults.commit_force_failure = false;
            failure
        };
        let commit_result = {
            #[cfg(test)]
            if force_failure {
                Err(AxError::ResourceBusy)
            } else {
                prepared.commit_in_place_pending()
            }
            #[cfg(not(test))]
            {
                prepared.commit_in_place_pending()
            }
        };
        let committed = match commit_result {
            Ok(committed) => committed,
            Err(_) => {
                // This is still a pre-commit failure.  Abort the exact
                // prepared binding, return its opaque fence to the capture
                // row, and remove both native rows so the same external
                // operation can retry from a coherent boundary.
                let captured = prepared.abort().ok_or(UtsAuthorityRejection::Busy)?;
                self.put_captured(capture_key, captured)?;
                let mut state = self.shared.lock();
                state.remove_operation_exact(&commit_key);
                state.remove_preparation_exact(
                    &prepare_key,
                    &request.binding,
                    &request.preparation,
                );
                return Err(UtsAuthorityRejection::Busy);
            }
        };

        // The world is now in its non-cancellable publication phase.  This
        // gate is test-only and deliberately sits outside both the native
        // and authority locks: cancellation may observe the phase, but must
        // return Busy until the native row owns the destination handle.
        #[cfg(test)]
        {
            let gate = {
                let mut state = self.shared.lock();
                state.faults.commit_publication_gate.take()
            };
            if let Some(gate) = gate {
                gate.pause();
            }
        }

        // The canonical authority transition is Applied at this point.  Its
        // exact query is non-failing bookkeeping: if an unusual query view is
        // Pending/Absent, keep the process-local commit row with a missing world
        // receipt and let commit_row reconcile it by the same canonical id.
        let query_unknown = {
            let mut state = self.shared.lock();
            let unknown = state.faults.commit_world_query_unknown;
            state.faults.commit_world_query_unknown = false;
            unknown
        };
        let world_receipt = if query_unknown {
            None
        } else {
            Self::query_world_receipt(&self.world.authority, committed.operation(), digest)
        };
        let committed_binding = committed.binding();
        let committed_operation = committed.operation();
        let mut state = self.shared.lock();
        let commit_index = state
            .operation_index(&commit_key)
            .expect("reserved native commit row disappeared");
        let row = &mut state.operations[commit_index];
        row.committed = Some(committed);
        row.world_receipt = world_receipt;
        row.commit = Some(commit_for_row);
        drop(state);

        // Native ownership is published before this infallible world-side
        // transition.  Cancellation cannot complete in the intervening
        // Committing phase, so it cannot release the source while missing a
        // destination handle that has not yet reached this row.
        self.world
            .authority
            .finish_commit_publication(committed_binding, committed_operation)
            .expect("validated commit publication state disappeared");
        Ok(commit)
    }
}

impl AuthorityPort for UtsAuthorityAdapter {
    type PrepareRejection = UtsAuthorityRejection;
    type CommitRejection = UtsAuthorityRejection;
    type AbortRejection = UtsAuthorityRejection;

    fn prepare(
        &mut self,
        request: PrepareRequest,
    ) -> CallOutcome<BindingPreparationReceipt, Self::PrepareRejection> {
        match self.ensure_preparation(&request) {
            Ok(receipt) => CallOutcome::Applied(receipt),
            Err(UtsAuthorityRejection::AuthorityUnavailable)
            | Err(UtsAuthorityRejection::Missing)
                if request.binding.capture_receipt.is_none() =>
            {
                CallOutcome::Indeterminate
            }
            Err(error) => CallOutcome::Rejected(error),
        }
    }

    fn query_prepare(
        &mut self,
        request: QueryPrepareRequest,
    ) -> QueryOutcome<BindingPreparationReceipt, Self::PrepareRejection> {
        let digest = Self::operation_digest(&request.binding);
        if let Some(capture_receipt) = request.binding.capture_receipt.as_ref() {
            if capture_receipt.verify().is_err() {
                return QueryOutcome::Rejected(UtsAuthorityRejection::Invalid);
            }
        }
        let key = self.operation_key(
            request.binding.continuation,
            request.operation,
            ExternalOperationKind::PrepareBindings,
            digest,
            Self::capture_generation(&request.binding),
        );
        let result = {
            let state = self.shared.lock();
            if state.has_conflicting_operation(&key) {
                QueryOutcome::Rejected(UtsAuthorityRejection::Conflict)
            } else {
                match Self::lookup_exact(&state, &key) {
                    Ok(Some(index)) => {
                        if state.operations[index].binding != request.binding {
                            QueryOutcome::Rejected(UtsAuthorityRejection::Conflict)
                        } else {
                            state.operations[index]
                                .preparation
                                .clone()
                                .map(QueryOutcome::Applied)
                                .unwrap_or(QueryOutcome::Indeterminate)
                        }
                    }
                    Ok(None) if request.binding.capture_receipt.is_none() => {
                        QueryOutcome::Indeterminate
                    }
                    Ok(None) => QueryOutcome::Absent,
                    Err(error) => QueryOutcome::Rejected(error),
                }
            }
        };
        match result {
            QueryOutcome::Applied(receipt) => QueryOutcome::Applied(receipt.as_ref().clone()),
            QueryOutcome::Rejected(error) => QueryOutcome::Rejected(error),
            QueryOutcome::Absent => QueryOutcome::Absent,
            QueryOutcome::Indeterminate => QueryOutcome::Indeterminate,
        }
    }

    fn commit(
        &mut self,
        request: CommitRequest,
    ) -> CallOutcome<AuthorityCommitReceipt, Self::CommitRejection> {
        match self.commit_for(&request) {
            Ok(receipt) => {
                let lost = {
                    let mut state = self.shared.lock();
                    let lost = state.faults.commit_lost_ack;
                    state.faults.commit_lost_ack = false;
                    lost
                };
                if lost {
                    CallOutcome::Indeterminate
                } else {
                    CallOutcome::Applied(receipt)
                }
            }
            Err(UtsAuthorityRejection::AuthorityUnavailable)
            | Err(UtsAuthorityRejection::Missing)
                if request.binding.capture_receipt.is_none() =>
            {
                CallOutcome::Indeterminate
            }
            Err(error) => CallOutcome::Rejected(error),
        }
    }

    fn query_commit(
        &mut self,
        request: QueryCommitRequest,
    ) -> QueryOutcome<AuthorityCommitReceipt, Self::CommitRejection> {
        let digest = Self::operation_digest(&request.binding);
        if request.preparation.verify().is_err() {
            return QueryOutcome::Rejected(UtsAuthorityRejection::Invalid);
        }
        if let Some(capture_receipt) = request.binding.capture_receipt.as_ref() {
            if capture_receipt.verify().is_err() {
                return QueryOutcome::Rejected(UtsAuthorityRejection::Invalid);
            }
        }
        let key = self.operation_key(
            request.binding.continuation,
            request.operation,
            ExternalOperationKind::CommitAuthority,
            digest,
            request.preparation.receipt_digest,
        );
        release_cancelled_commit(&self.world.authority, &self.shared, &key);
        // A process-local binding cannot treat a missing world receipt as an
        // authoritative commit. Try the exact authority query once more,
        // then surface an indeterminate outcome if authority state is gone
        // or otherwise unavailable. The coordinator maps that outcome to its
        // existing `ExternalOutcomeUnknown` recovery requirement.
        if request.binding.capture_receipt.is_none() {
            let canonical = {
                let state = self.shared.lock();
                state
                    .operation_index(&key)
                    .and_then(|index| {
                        state.operations[index]
                            .world_receipt
                            .is_none()
                            .then(|| state.operations[index].canonical.clone())
                    })
                    .flatten()
            };
            if let Some(canonical) = canonical {
                let _ = UtsAuthorityAdapter::publish_world_receipt(
                    &self.world.authority,
                    &self.shared,
                    request.binding.continuation,
                    digest,
                    request.operation,
                    request.preparation.receipt_digest,
                    canonical.operation(),
                );
            }
        }
        let result = {
            let state = self.shared.lock();
            if state.has_conflicting_operation(&key) {
                QueryOutcome::Rejected(UtsAuthorityRejection::Conflict)
            } else {
                match Self::lookup_exact(&state, &key) {
                    Ok(Some(index)) => {
                        if state.operations[index].binding != request.binding
                            || state.operations[index].preparation.as_deref()
                                != Some(&request.preparation)
                        {
                            QueryOutcome::Rejected(UtsAuthorityRejection::Conflict)
                        } else if request.binding.capture_receipt.is_none()
                            && state.operations[index].world_receipt.is_none()
                        {
                            QueryOutcome::Indeterminate
                        } else {
                            state.operations[index]
                                .commit
                                .clone()
                                .map(QueryOutcome::Applied)
                                .unwrap_or(QueryOutcome::Indeterminate)
                        }
                    }
                    Ok(None) if request.binding.capture_receipt.is_none() => {
                        QueryOutcome::Indeterminate
                    }
                    Ok(None) => QueryOutcome::Absent,
                    Err(error) => QueryOutcome::Rejected(error),
                }
            }
        };
        match result {
            QueryOutcome::Applied(receipt) => QueryOutcome::Applied(receipt.as_ref().clone()),
            QueryOutcome::Rejected(error) => QueryOutcome::Rejected(error),
            QueryOutcome::Absent => QueryOutcome::Absent,
            QueryOutcome::Indeterminate => QueryOutcome::Indeterminate,
        }
    }

    fn abort_preparation(
        &mut self,
        request: AbortPreparationRequest,
    ) -> CallOutcome<AbortPreparationReceipt, Self::AbortRejection> {
        let digest = Self::operation_digest(&request.binding);
        if request.preparation.verify().is_err() {
            return CallOutcome::Rejected(UtsAuthorityRejection::Invalid);
        }
        if let Some(capture_receipt) = request.binding.capture_receipt.as_ref() {
            if capture_receipt.verify().is_err() {
                return CallOutcome::Rejected(UtsAuthorityRejection::Invalid);
            }
        }
        let capture_key = match self.capture_key(&request.binding) {
            Ok(key) => key,
            Err(UtsAuthorityRejection::AuthorityUnavailable)
            | Err(UtsAuthorityRejection::Missing)
                if request.binding.capture_receipt.is_none() =>
            {
                return CallOutcome::Indeterminate;
            }
            Err(error) => return CallOutcome::Rejected(error),
        };
        let prepare_key = self.operation_key(
            request.binding.continuation,
            request.preparation.operation,
            ExternalOperationKind::PrepareBindings,
            digest,
            Self::capture_generation(&request.binding),
        );
        let abort_key = self.operation_key(
            request.binding.continuation,
            request.operation,
            ExternalOperationKind::AbortPreparation,
            digest,
            request.preparation.receipt_digest,
        );
        let abort =
            match Self::abort_receipt(request.operation, &request.binding, &request.preparation) {
                Ok(receipt) => receipt,
                Err(error) => return CallOutcome::Rejected(error),
            };
        let abort_for_row = match Arc::try_new(abort.clone()) {
            Ok(receipt) => receipt,
            Err(_) => return CallOutcome::Rejected(UtsAuthorityRejection::Busy),
        };
        let preparation = request.preparation.clone();
        let preparation_for_row = match Arc::try_new(preparation.clone()) {
            Ok(receipt) => receipt,
            Err(_) => return CallOutcome::Rejected(UtsAuthorityRejection::Busy),
        };
        let binding_for_row = request.binding.clone();
        if let Some(existing_abort) = {
            let state = self.shared.lock();
            if state.has_conflicting_operation(&abort_key) {
                return CallOutcome::Rejected(UtsAuthorityRejection::Conflict);
            }
            if let Some(index) = Self::lookup_exact(&state, &abort_key).ok().flatten() {
                if state.operations[index].binding != request.binding
                    || state.operations[index].preparation.as_deref() != Some(&request.preparation)
                {
                    return CallOutcome::Rejected(UtsAuthorityRejection::Conflict);
                }
                state.operations[index].abort.clone()
            } else {
                None
            }
        } {
            return CallOutcome::Applied(existing_abort.as_ref().clone());
        }
        {
            let state = self.shared.lock();
            if UtsNativeState::ensure_capacity(&state.operations).is_err() {
                return CallOutcome::Rejected(UtsAuthorityRejection::Capacity);
            }
            let Some(capture_index) = state.capture_index(capture_key) else {
                return CallOutcome::Rejected(UtsAuthorityRejection::Missing);
            };
            if state.captures[capture_index].restored.is_some()
                || state.captures[capture_index].frozen.is_some()
            {
                return CallOutcome::Rejected(UtsAuthorityRejection::Busy);
            }
            let Some(prep_index) = state.operation_index(&prepare_key) else {
                return CallOutcome::Rejected(UtsAuthorityRejection::Missing);
            };
            let row = &state.operations[prep_index];
            if row.binding != request.binding
                || row.preparation.as_deref() != Some(&request.preparation)
            {
                return CallOutcome::Rejected(UtsAuthorityRejection::Conflict);
            }
        }
        #[cfg(test)]
        {
            let gate = {
                let mut state = self.shared.lock();
                state.faults.abort_lookup_gate.take()
            };
            if let Some(gate) = gate {
                gate.pause();
            }
        }
        let prepared = {
            let mut state = self.shared.lock();
            if state.has_conflicting_operation(&abort_key) {
                return CallOutcome::Rejected(UtsAuthorityRejection::Conflict);
            }
            if UtsNativeState::ensure_capacity(&state.operations).is_err() {
                return CallOutcome::Rejected(UtsAuthorityRejection::Capacity);
            }
            let Some(capture_index) = state.capture_index(capture_key) else {
                return CallOutcome::Rejected(UtsAuthorityRejection::Missing);
            };
            if state.captures[capture_index].restored.is_some()
                || state.captures[capture_index].frozen.is_some()
            {
                return CallOutcome::Rejected(UtsAuthorityRejection::Busy);
            }
            if state.operation_index(&abort_key).is_some() {
                return CallOutcome::Rejected(UtsAuthorityRejection::Busy);
            }
            let Some(prep_index) = state.operation_index(&prepare_key) else {
                return CallOutcome::Rejected(UtsAuthorityRejection::Missing);
            };
            let (canonical, prepared) = {
                let row = &mut state.operations[prep_index];
                if row.binding != request.binding
                    || row.preparation.as_deref() != Some(&request.preparation)
                {
                    return CallOutcome::Rejected(UtsAuthorityRejection::Conflict);
                }
                let canonical = row.canonical.clone();
                let prepared = row.prepared.take().ok_or(UtsAuthorityRejection::Busy);
                (canonical, prepared)
            };
            match prepared {
                Ok(prepared) => {
                    state.operations.push(NativeOperation {
                        key: abort_key,
                        continuation: request.binding.continuation,
                        binding: binding_for_row,
                        canonical,
                        preparation: Some(preparation_for_row.clone()),
                        prepared: None,
                        committed: None,
                        world_receipt: None,
                        active_consumer: None,
                        commit: None,
                        abort: None,
                        activation_key: None,
                        activation: None,
                    });
                    prepared
                }
                Err(error) => return CallOutcome::Rejected(error),
            }
        };
        let captured = prepared.abort_prepared();
        let mut state = self.shared.lock();
        let capture_index = state
            .capture_index(capture_key)
            .expect("reserved abort capture row disappeared");
        {
            let capture_row = &mut state.captures[capture_index];
            assert!(capture_row.restored.is_none() && capture_row.frozen.is_none());
            capture_row.frozen = Some(captured);
        }
        let prep_index = state
            .operation_index(&prepare_key)
            .expect("reserved abort preparation row disappeared");
        state.operations.remove(prep_index);
        let abort_index = state
            .operation_index(&abort_key)
            .expect("reserved abort row disappeared");
        state.operations[abort_index].abort = Some(abort_for_row);
        CallOutcome::Applied(abort)
    }

    fn query_abort(
        &mut self,
        request: QueryAbortRequest,
    ) -> QueryOutcome<AbortPreparationReceipt, Self::AbortRejection> {
        let digest = Self::operation_digest(&request.binding);
        if request.preparation.verify().is_err() {
            return QueryOutcome::Rejected(UtsAuthorityRejection::Invalid);
        }
        if let Some(capture_receipt) = request.binding.capture_receipt.as_ref() {
            if capture_receipt.verify().is_err() {
                return QueryOutcome::Rejected(UtsAuthorityRejection::Invalid);
            }
        }
        let key = self.operation_key(
            request.binding.continuation,
            request.operation,
            ExternalOperationKind::AbortPreparation,
            digest,
            request.preparation.receipt_digest,
        );
        let result = {
            let state = self.shared.lock();
            if state.has_conflicting_operation(&key) {
                QueryOutcome::Rejected(UtsAuthorityRejection::Conflict)
            } else {
                match Self::lookup_exact(&state, &key) {
                    Ok(Some(index)) => {
                        if state.operations[index].binding != request.binding
                            || state.operations[index].preparation.as_deref()
                                != Some(&request.preparation)
                        {
                            QueryOutcome::Rejected(UtsAuthorityRejection::Conflict)
                        } else {
                            state.operations[index]
                                .abort
                                .clone()
                                .map(QueryOutcome::Applied)
                                .unwrap_or(QueryOutcome::Indeterminate)
                        }
                    }
                    Ok(None) if request.binding.capture_receipt.is_none() => {
                        QueryOutcome::Indeterminate
                    }
                    Ok(None) => QueryOutcome::Absent,
                    Err(error) => QueryOutcome::Rejected(error),
                }
            }
        };
        match result {
            QueryOutcome::Applied(receipt) => QueryOutcome::Applied(receipt.as_ref().clone()),
            QueryOutcome::Rejected(error) => QueryOutcome::Rejected(error),
            QueryOutcome::Absent => QueryOutcome::Absent,
            QueryOutcome::Indeterminate => QueryOutcome::Indeterminate,
        }
    }
}

struct UtsRuntimeAdapter<'a> {
    process: &'a ProcessData,
    /// The current process generation when recovery still needs to capture
    /// source state. Post-commit recovery deliberately leaves this empty:
    /// the source is permanently fenced/retired and activation recovery is
    /// reconciled from the process-local native row instead.
    source: Option<GenerationHandle>,
    world: SemanticWorld,
    authority: Arc<Authority>,
    shared: Arc<SpinNoIrq<UtsNativeState>>,
}

/// Release the non-serializable destination capability after the canonical
/// world operation has been cancelled, while retaining the native receipt as
/// process-local query data. The handle is moved out of the native ledger before
/// it is dropped so its authority callback never runs under SpinNoIrq.
fn release_cancelled_commit(
    authority: &Arc<Authority>,
    shared: &Arc<SpinNoIrq<UtsNativeState>>,
    key: &NativeOperationKey,
) -> bool {
    let canonical = {
        let state = shared.lock();
        let Some(index) = state.operation_index(key) else {
            return false;
        };
        state.operations[index].canonical.clone()
    };
    let Some(canonical) = canonical else {
        return false;
    };
    let cancelled = matches!(
        authority
            .query_handle()
            .query_operation(canonical.operation(), key.digest.0),
        AuthorityQuery::Absent | AuthorityQuery::Rejected | AuthorityQuery::Conflict
    );
    if !cancelled {
        return false;
    }
    let _released = release_cancelled_commit_in(shared, authority, canonical.operation());
    // The caller must still reconcile the terminal cancellation even when a
    // previous retry already moved the capability out of the row.
    true
}

fn release_cancelled_commit_in(
    shared: &Arc<SpinNoIrq<UtsNativeState>>,
    authority: &Authority,
    operation: AuthorityOperationId,
) -> bool {
    let committed = {
        let mut state = shared.lock();
        let Some(index) = state.operations.iter().position(|row| {
            row.key.authority == authority.instance_id()
                && row.key.stage == ExternalOperationKind::CommitAuthority
                && row
                    .canonical
                    .as_ref()
                    .is_some_and(|canonical| canonical.operation() == operation)
        }) else {
            return false;
        };
        state.operations[index].committed.take()
    };
    let released = committed.is_some();
    drop(committed);
    released
}

/// Called by the world authority immediately after it has durably cancelled
/// an Applied-but-not-active binding. The native row keeps its exact commit
/// receipt for idempotent queries, but this removes the opaque destination
/// handle at the cancellation linearization point.
pub(super) fn release_cancelled_commit_for_operation(
    authority: &Authority,
    operation: AuthorityOperationId,
) {
    let Some(shared) = LOCAL_NATIVE.get().cloned() else {
        return;
    };
    let _ = release_cancelled_commit_in(&shared, authority, operation);
}

impl<'a> UtsRuntimeAdapter<'a> {
    fn new(
        process: &'a ProcessData,
        source: Option<GenerationHandle>,
        world: SemanticWorld,
        authority: Arc<Authority>,
        shared: Arc<SpinNoIrq<UtsNativeState>>,
    ) -> Self {
        Self {
            process,
            source,
            world,
            authority,
            shared,
        }
    }

    fn capture_key(&self, request: &CaptureRequest) -> NativeCaptureKey {
        NativeCaptureKey {
            authority: self.authority.instance_id(),
            continuation: request.continuation,
            operation: request.operation,
            stage: ExternalOperationKind::CaptureSource,
            digest: Digest::ZERO,
            preparation_generation: Digest::ZERO,
        }
    }

    fn capture_matches_request(capture: &NativeCapture, request: &CaptureRequest) -> bool {
        capture.receipt.operation == request.operation
            && capture.receipt.continuation == request.continuation
            && capture.receipt.scope == request.scope
            && capture.receipt.source == request.source
            && capture.receipt.profile == request.profile
            && capture.receipt.lineage == request.lineage
    }

    fn destination_for(
        authority: &Arc<Authority>,
        shared: &Arc<SpinNoIrq<UtsNativeState>>,
        continuation: ContinuationId,
        snapshot: SnapshotId,
        destination: &ExternalCoordinate,
        requirements: &[ResourceRequirement],
        preparation_digest: Digest,
    ) -> Result<(NativeOperationKey, GenerationHandle), UtsRuntimeError> {
        let state = shared.lock();
        let mut candidate = None;
        for row in &state.operations {
            let Some(preparation) = row.preparation.as_ref() else {
                continue;
            };
            // A durable capture receipt contributes its generation to the
            // native key; a ProcessLocal binding deliberately uses the zero
            // generation and is still resolvable while this process owns the
            // exact native preparation row.
            let capture_generation = row
                .binding
                .capture_receipt
                .as_ref()
                .map_or(Digest::ZERO, |receipt| receipt.receipt_digest);
            let expected_key = NativeOperationKey {
                authority: authority.instance_id(),
                continuation,
                operation: preparation.operation,
                stage: ExternalOperationKind::PrepareBindings,
                digest: preparation_digest,
                preparation_generation: capture_generation,
            };
            if row.key != expected_key
                || preparation.snapshot != snapshot
                || preparation.destination != *destination
                || preparation.snapshot_digest != preparation_digest
                || row.binding.snapshot != snapshot
                || row.binding.destination != *destination
                || row.binding.preparation_digest != preparation_digest
                || row.binding.requirements != requirements
            {
                continue;
            }
            let coordinate = row
                .prepared
                .as_ref()
                .map(|prepared| prepared.destination)
                .or_else(|| {
                    row.committed
                        .as_ref()
                        .map(|committed| committed.destination().coordinate)
                });
            let Some(coordinate) = coordinate else {
                continue;
            };
            // The request does not carry the external prepare operation, so
            // an ambiguous semantic match is a conflict rather than permission
            // to select an arbitrary Vec row.
            if candidate.is_some() {
                return Err(UtsRuntimeError::Conflict);
            }
            candidate = Some((row.key, coordinate));
        }
        drop(state);
        let Some((key, coordinate)) = candidate else {
            return Err(UtsRuntimeError::AuthorityUnavailable);
        };
        let mut authority_state = authority.state.lock();
        let handle =
            match Authority::destination_handle(authority, &mut authority_state, coordinate) {
                Ok(handle) => handle,
                Err(AxError::BadState) => return Err(UtsRuntimeError::AuthorityUnavailable),
                Err(_) => return Err(UtsRuntimeError::Busy),
            };
        Ok((key, handle))
    }

    /// A native committed row owns a destination generation handle until
    /// activation consumes it. The world authority can also cancel an
    /// Applied-but-not-active binding after a lost acknowledgement; in that
    /// case the receipt remains process-local query data, but the handle is no longer a
    /// capability. Reconcile that exact world state without ever dropping
    /// the handle under the native SpinNoIrq lock.
    fn release_cancelled_commit(&self, key: &NativeOperationKey) -> bool {
        release_cancelled_commit(&self.authority, &self.shared, key)
    }

    fn commit_row(
        &self,
        continuation: ContinuationId,
        digest: Digest,
        operation: OperationId,
        preparation_generation: Digest,
    ) -> Option<(
        WorldActivationReceipt,
        bool,
        Option<ActivationKey>,
        Option<VisaActivationReceipt>,
    )> {
        Self::commit_row_for(
            &self.authority,
            &self.shared,
            continuation,
            digest,
            operation,
            preparation_generation,
        )
    }

    /// Reconcile a commit row without requiring a ProcessData owner.  The
    /// runtime adapter delegates here so a retry can safely observe a world
    /// binding that has already reached Activating/Active while the native
    /// activation acknowledgement is still in flight.
    fn commit_row_for(
        authority: &Arc<Authority>,
        shared: &Arc<SpinNoIrq<UtsNativeState>>,
        continuation: ContinuationId,
        digest: Digest,
        operation: OperationId,
        preparation_generation: Digest,
    ) -> Option<(
        WorldActivationReceipt,
        bool,
        Option<ActivationKey>,
        Option<VisaActivationReceipt>,
    )> {
        let key = NativeOperationKey {
            authority: authority.instance_id(),
            continuation,
            operation,
            stage: ExternalOperationKind::CommitAuthority,
            digest,
            preparation_generation,
        };
        if release_cancelled_commit(authority, shared, &key) {
            return None;
        }
        let (world_receipt, canonical, active, activation_key, activation, publication) = {
            let state = shared.lock();
            let index = state.operation_index(&key)?;
            let row = &state.operations[index];
            let publication = row
                .committed
                .as_ref()
                .map(|committed| (committed.binding(), committed.operation()));
            // An activated terminal row intentionally releases its
            // CommittedBinding capability.  It remains a valid commit row
            // because activation is the terminal proof that publication was
            // already completed; an unactivated row without that capability
            // is still in-flight and must not expose a world receipt.
            if publication.is_none() && row.activation.is_none() {
                return None;
            }
            (
                row.world_receipt.clone(),
                row.canonical.clone(),
                row.activation.is_some(),
                row.activation_key.clone(),
                row.activation.clone(),
                publication,
            )
        };
        if let Some((binding, operation)) = publication {
            // A concurrent activation may already have advanced the world
            // relation to Activating/Active (the native row still owns the
            // committed capability until that activation publishes).  The
            // world handshake is idempotent for those exact terminal states;
            // any other failure is a recoverable query miss, not a panic.
            if authority
                .finish_commit_publication(binding, operation)
                .is_err()
            {
                return None;
            }
        }
        let world_receipt = match world_receipt {
            Some(receipt) => receipt,
            None => {
                let canonical = canonical?;
                UtsAuthorityAdapter::publish_world_receipt(
                    authority,
                    shared,
                    continuation,
                    digest,
                    operation,
                    preparation_generation,
                    canonical.operation(),
                )?
            }
        };
        Some((
            world_receipt,
            active,
            activation_key.map(|key| key.as_ref().clone()),
            activation.map(|receipt| receipt.as_ref().clone()),
        ))
    }

    fn activation_binding(preparation: &BindingPreparationReceipt) -> Option<ExternalCoordinate> {
        preparation
            .grants
            .first()
            .map(|grant| grant.binding.clone())
    }

    fn exact_commit_row(
        &self,
        continuation: ContinuationId,
        digest: Digest,
        snapshot: SnapshotId,
        snapshot_digest: Digest,
        destination: &ExternalCoordinate,
        preparation: &BindingPreparationReceipt,
        commit: &AuthorityCommitReceipt,
    ) -> bool {
        let state = self.shared.lock();
        let key = NativeOperationKey {
            authority: self.authority.instance_id(),
            continuation,
            operation: commit.operation,
            stage: ExternalOperationKind::CommitAuthority,
            digest,
            preparation_generation: commit.binding_receipt_digest,
        };
        let Some(index) = state.operation_index(&key) else {
            return false;
        };
        let row = &state.operations[index];
        row.binding.snapshot == snapshot
            && row.binding.destination == *destination
            && row.binding.preparation_digest == snapshot_digest
            && row.preparation.as_deref() == Some(preparation)
            && row.commit.as_deref() == Some(commit)
    }

    fn activation_receipt(
        request: &ActivateRequest<NativeRestored>,
        commit: &AuthorityCommitReceipt,
    ) -> Result<VisaActivationReceipt, UtsRuntimeError> {
        VisaActivationReceipt {
            operation: request.operation,
            continuation: request.continuation,
            snapshot: request.snapshot,
            snapshot_digest: commit.snapshot_digest,
            destination: request.destination.clone(),
            authority_commit_digest: commit.receipt_digest,
            execution_epoch: commit.execution_epoch,
            receipt_digest: Digest::ZERO,
        }
        .seal()
        .map_err(|_| UtsRuntimeError::Invalid)
    }
}

#[derive(Clone, Copy, Debug)]
struct NativeRestored {
    continuation: ContinuationId,
    snapshot: SnapshotId,
}

fn restore_capture(
    shared: &Arc<SpinNoIrq<UtsNativeState>>,
    authority: super::AuthorityInstanceId,
    request: RestoreSourceRequest,
) -> CallOutcome<SourceRestorationReceipt, UtsRuntimeError> {
    let reservation = {
        let mut state = shared.lock();
        let mut matching_index = None;
        for (index, row) in state.captures.iter().enumerate() {
            let matches = row.capture.key.authority == authority
                && row.capture.key.stage == ExternalOperationKind::CaptureSource
                && row.capture.key.digest == Digest::ZERO
                && row.capture.key.preparation_generation == Digest::ZERO
                && row.capture.key.continuation == request.continuation
                && row.capture.snapshot.as_ref() == &request.snapshot
                && row.capture.receipt.source == request.source;
            if matches {
                if matching_index.replace(index).is_some() {
                    return CallOutcome::Rejected(UtsRuntimeError::Conflict);
                }
            }
        }
        let Some(index) = matching_index else {
            // The local native projection is not an authority of absence.
            // Once it is gone, source restoration has an unknown outcome and
            // the coordinator must retain recovery rather than replaying or
            // silently discarding the source cut.
            return CallOutcome::Indeterminate;
        };
        let row = &mut state.captures[index];
        if let Some(receipt) = row.restored.clone() {
            Err(receipt)
        } else {
            let Some(captured) = row.frozen.take() else {
                return CallOutcome::Rejected(UtsRuntimeError::Busy);
            };
            let Some(fence) = captured.fence.as_ref() else {
                row.frozen = Some(captured);
                return CallOutcome::Rejected(UtsRuntimeError::Busy);
            };
            let Some(execution_epoch) = fence.execution_epoch() else {
                row.frozen = Some(captured);
                return CallOutcome::Rejected(UtsRuntimeError::Busy);
            };
            if !fence.is_drained() {
                row.frozen = Some(captured);
                return CallOutcome::Rejected(UtsRuntimeError::Busy);
            }
            Ok((row.capture.key, captured, execution_epoch))
        }
    };
    let (capture_key, mut captured, execution_epoch) = match reservation {
        Err(receipt) => return CallOutcome::Applied(receipt.as_ref().clone()),
        Ok(reservation) => reservation,
    };

    // Seal and allocate the exact restoration fact before reopening the
    // source. Publication after rollback is then a no-allocation exact move;
    // if sealing/allocation fails the captured fence is returned unchanged.
    let restoration = match (SourceRestorationReceipt {
        continuation: request.continuation,
        snapshot: request.snapshot.body.snapshot,
        snapshot_digest: request.snapshot.body_digest,
        source: request.source.clone(),
        execution_epoch,
        receipt_digest: Digest::ZERO,
    })
    .seal()
    {
        Ok(receipt) => match Arc::try_new(receipt) {
            Ok(receipt) => receipt,
            Err(_) => {
                let mut state = shared.lock();
                let index = state
                    .capture_index(capture_key)
                    .expect("reserved capture row disappeared");
                assert!(state.captures[index].frozen.is_none());
                state.captures[index].frozen = Some(captured);
                return CallOutcome::Rejected(UtsRuntimeError::Busy);
            }
        },
        Err(_) => {
            let mut state = shared.lock();
            let index = state
                .capture_index(capture_key)
                .expect("reserved capture row disappeared");
            assert!(state.captures[index].frozen.is_none());
            state.captures[index].frozen = Some(captured);
            return CallOutcome::Rejected(UtsRuntimeError::Invalid);
        }
    };
    let mut fence = captured
        .fence
        .take()
        .expect("captured source lost its exact fence token");
    if fence.rollback().is_err() {
        captured.fence = Some(fence);
        let mut state = shared.lock();
        let index = state
            .capture_index(capture_key)
            .expect("reserved capture row disappeared");
        state.captures[index].frozen = Some(captured);
        return CallOutcome::Rejected(UtsRuntimeError::Busy);
    }
    let mut state = shared.lock();
    let index = state
        .capture_index(capture_key)
        .expect("reserved capture row disappeared");
    let row = &mut state.captures[index];
    assert!(row.frozen.is_none() && row.restored.is_none());
    row.restored = Some(restoration.clone());
    drop(state);
    CallOutcome::Applied(restoration.as_ref().clone())
}

impl<'a> RuntimePort for UtsRuntimeAdapter<'a> {
    type Frozen = NativeCaptureKey;
    type Prepared = NativeDestinationKey;
    type Restored = NativeRestored;
    type ActivationRejection = UtsRuntimeError;
    type Error = UtsRuntimeError;

    fn capture_durability(&self) -> CaptureDurability {
        // LOCAL_WORKFLOW and LOCAL_NATIVE are bounded process-local
        // projections. They do not survive a coordinator/source restart and
        // therefore cannot satisfy the authority-durable query contract.
        UTS_CAPTURE_DURABILITY
    }

    fn capture(
        &mut self,
        request: CaptureRequest,
    ) -> CallOutcome<CapturedRuntime<Self::Frozen>, Self::Error> {
        let key = self.capture_key(&request);
        let existing = {
            let state = self.shared.lock();
            if state.captures.len() >= NATIVE_ROW_CAPACITY && state.capture_index(key).is_none() {
                return CallOutcome::Rejected(UtsRuntimeError::Busy);
            }
            if let Some(index) = state.capture_index(key) {
                let row = &state.captures[index];
                if !Self::capture_matches_request(&row.capture, &request) {
                    return CallOutcome::Rejected(UtsRuntimeError::Conflict);
                }
                if row.frozen.is_some() {
                    Some((
                        row.capture.snapshot.clone(),
                        row.capture.safe_point.clone(),
                        row.capture.receipt.clone(),
                    ))
                } else {
                    return CallOutcome::Rejected(UtsRuntimeError::Busy);
                }
            } else {
                None
            }
        };
        if let Some((snapshot, safe_point, _receipt)) = existing {
            return CallOutcome::Applied(CapturedRuntime {
                snapshot: snapshot.as_ref().clone(),
                safe_point: safe_point.as_ref().clone(),
                // The receipt is retained only inside the native row for the
                // current process-local authority binding. It is not exposed
                // as a coordinator-durable capture receipt.
                receipt: None,
                frozen: key,
            });
        }
        // Existing process-local records are queried before this point. Resolve a
        // live source only when that query was Absent and the coordinator is
        // actually retrying capture; Applied/Pending recovery can reconcile
        // from the native row without a Runnable source handle.
        let source = match self.source.as_ref() {
            Some(source) => source.clone(),
            None => match self.world.generation_for_uts(&self.process.uts_ns()) {
                Ok(source) => source,
                // A process-local continuation cannot turn a missing source
                // generation into an authoritative rejection. The coordinator
                // must retain recovery for this unknown authority outcome.
                Err(_) => return CallOutcome::Indeterminate,
            },
        };
        if request.source != generation_coordinate(&source)
            || !Arc::ptr_eq(&self.process.uts_ns(), &source.uts)
        {
            return CallOutcome::Rejected(UtsRuntimeError::Conflict);
        }
        let fence = match source.begin_fence() {
            Ok(fence) => fence,
            Err(_) => return CallOutcome::Rejected(UtsRuntimeError::Busy),
        };
        let captured = match source.capture_after_fence(fence) {
            Ok(captured) => captured,
            Err(_) => return CallOutcome::Rejected(UtsRuntimeError::Busy),
        };
        let (snapshot, safe_point, receipt) = match make_logical_capture(&request, &captured) {
            Ok(values) => values,
            Err(error) => {
                return CallOutcome::Rejected(match error {
                    AxError::NoMemory => UtsRuntimeError::Busy,
                    _ => UtsRuntimeError::Invalid,
                });
            }
        };
        // All ref-counted process-local payloads are allocated before entering the
        // native row lock. The lock below only compares/moves these prepared
        // values and publishes the already-owned row.
        let snapshot_arc = match Arc::try_new(snapshot.clone()) {
            Ok(value) => value,
            Err(_) => return CallOutcome::Rejected(UtsRuntimeError::Busy),
        };
        let safe_point_arc = match Arc::try_new(safe_point.clone()) {
            Ok(value) => value,
            Err(_) => return CallOutcome::Rejected(UtsRuntimeError::Busy),
        };
        let receipt_arc = match Arc::try_new(receipt.clone()) {
            Ok(value) => value,
            Err(_) => return CallOutcome::Rejected(UtsRuntimeError::Busy),
        };
        let mut captured = Some(captured);
        let publication = {
            let mut state = self.shared.lock();
            if let Some(index) = state.capture_index(key) {
                if Self::capture_matches_request(&state.captures[index].capture, &request)
                    && state.captures[index].capture.receipt.as_ref() == &receipt
                {
                    Ok((true, false))
                } else {
                    Err(UtsRuntimeError::Conflict)
                }
            } else if state.captures.len() >= NATIVE_ROW_CAPACITY {
                Err(UtsRuntimeError::Busy)
            } else {
                state.captures.push(NativeCaptureRow {
                    capture: NativeCapture {
                        key,
                        snapshot: snapshot_arc,
                        safe_point: safe_point_arc,
                        receipt: receipt_arc,
                    },
                    frozen: Some(captured.take().expect("capture token missing")),
                    restored: None,
                });
                let lost = state.faults.capture_lost_ack;
                state.faults.capture_lost_ack = false;
                Ok((false, lost))
            }
        };
        let (duplicate, lost) = match publication {
            Ok(value) => value,
            Err(error) => {
                drop(captured);
                return CallOutcome::Rejected(error);
            }
        };
        if duplicate {
            drop(captured);
        }
        if lost {
            CallOutcome::Indeterminate
        } else {
            CallOutcome::Applied(CapturedRuntime {
                snapshot,
                safe_point,
                // See the duplicate path above: this local row is not a
                // durable capture authority.
                receipt: None,
                frozen: key,
            })
        }
    }

    fn query_capture(
        &mut self,
        _request: QueryCaptureRequest,
    ) -> QueryOutcome<CapturedSnapshot, Self::Error> {
        // A local row may still be present, but it is not a restart-safe
        // capture authority. Do not turn an in-process projection into a
        // durable query result; the existing coordinator maps this to
        // `ProcessLocalCaptureDualCrashRisk`.
        QueryOutcome::Indeterminate
    }

    fn retire_capture(&mut self, _receipt: &CaptureReceipt) -> Result<(), Self::Error> {
        // The source fence remains owned by the canonical PreparedBinding
        // until commit or pre-commit abort. Retiring the vISA capture row must
        // therefore not drop the native token early.
        Ok(())
    }

    fn freeze_source(
        &mut self,
        _request: FreezeSourceRequest,
    ) -> CallOutcome<FrozenRuntime<Self::Frozen>, Self::Error> {
        CallOutcome::Indeterminate
    }

    fn restore_source(
        &mut self,
        request: RestoreSourceRequest,
    ) -> CallOutcome<SourceRestorationReceipt, Self::Error> {
        restore_capture(&self.shared, self.authority.instance_id(), request)
    }

    fn prepare_destination(
        &mut self,
        request: PrepareDestinationRequest,
    ) -> CallOutcome<Self::Prepared, Self::Error> {
        if request.snapshot.verify().is_err() {
            return CallOutcome::Rejected(UtsRuntimeError::Invalid);
        }
        let (key, destination) = match Self::destination_for(
            &self.authority,
            &self.shared,
            request.continuation,
            request.snapshot.body.snapshot,
            &request.destination,
            &request.requirements,
            request.preparation_digest,
        ) {
            Ok(value) => value,
            Err(UtsRuntimeError::AuthorityUnavailable) => {
                return CallOutcome::Indeterminate;
            }
            Err(error) => return CallOutcome::Rejected(error),
        };
        if destination.enter().is_some() {
            return CallOutcome::Rejected(UtsRuntimeError::Conflict);
        }
        CallOutcome::Applied(NativeDestinationKey {
            key,
            continuation: request.continuation,
            snapshot: request.snapshot.body.snapshot,
        })
    }

    fn restore_destination(
        &mut self,
        request: RestoreDestinationRequest<Self::Prepared>,
    ) -> CallOutcome<Self::Restored, Self::Error> {
        if request.snapshot.verify().is_err() {
            return CallOutcome::Rejected(UtsRuntimeError::Invalid);
        }
        let Some((..)) = self.commit_row(
            request.continuation,
            request.commit.snapshot_digest,
            request.commit.operation,
            request.commit.binding_receipt_digest,
        ) else {
            return CallOutcome::Indeterminate;
        };
        if !self.exact_commit_row(
            request.continuation,
            request.commit.snapshot_digest,
            request.snapshot.body.snapshot,
            request.snapshot.body_digest,
            &request.destination,
            &request.preparation,
            &request.commit,
        ) {
            return CallOutcome::Rejected(UtsRuntimeError::Conflict);
        }
        if request.prepared.continuation != request.continuation
            || request.prepared.snapshot != request.snapshot.body.snapshot
        {
            return CallOutcome::Rejected(UtsRuntimeError::Conflict);
        }
        let state = self.shared.lock();
        let Some(index) = state.operation_index(&request.prepared.key) else {
            return CallOutcome::Rejected(UtsRuntimeError::Conflict);
        };
        let row = &state.operations[index];
        if row.binding.snapshot != request.snapshot.body.snapshot
            || row.binding.destination != request.destination
            || row.preparation.as_deref() != Some(&request.preparation)
        {
            return CallOutcome::Rejected(UtsRuntimeError::Conflict);
        }
        CallOutcome::Applied(NativeRestored {
            continuation: request.continuation,
            snapshot: request.snapshot.body.snapshot,
        })
    }

    fn activate(
        &mut self,
        request: ActivateRequest<Self::Restored>,
    ) -> CallOutcome<VisaActivationReceipt, Self::ActivationRejection> {
        let (world_receipt, active, activation_key, existing) = match self.commit_row(
            request.continuation,
            request.commit.snapshot_digest,
            request.commit.operation,
            request.commit.binding_receipt_digest,
        ) {
            Some(row) => row,
            None => return CallOutcome::Indeterminate,
        };
        if request.restored.continuation != request.continuation
            || request.restored.snapshot != request.snapshot
            || !self.exact_commit_row(
                request.continuation,
                request.commit.snapshot_digest,
                request.snapshot,
                request.commit.snapshot_digest,
                &request.destination,
                &request.preparation,
                &request.commit,
            )
        {
            return CallOutcome::Rejected(UtsRuntimeError::Conflict);
        }
        let requested_key = ActivationKey {
            operation: request.operation,
            binding: Self::activation_binding(&request.preparation),
        };
        if active {
            if activation_key.as_ref() != Some(&requested_key) {
                return CallOutcome::Rejected(UtsRuntimeError::Conflict);
            }
            return existing
                .map(CallOutcome::Applied)
                .unwrap_or(CallOutcome::Indeterminate);
        }
        // Seal/allocate the returned receipt before reserving the native
        // activation identity or publishing the destination pointer.
        let receipt = match Self::activation_receipt(&request, &request.commit) {
            Ok(receipt) => receipt,
            Err(error) => return CallOutcome::Rejected(error),
        };
        let activation_for_row = match Arc::try_new(receipt.clone()) {
            Ok(receipt) => receipt,
            Err(_) => return CallOutcome::Rejected(UtsRuntimeError::Busy),
        };
        let requested_key_for_row = match Arc::try_new(requested_key.clone()) {
            Ok(key) => key,
            Err(_) => return CallOutcome::Rejected(UtsRuntimeError::Busy),
        };
        // Reserve the exact activation identity before touching ProcessData.
        // The world activation publishes the destination pointer and its
        // consumer lease as one no-fail authority transaction; native
        // publication must therefore already have a process-local row/identity so
        // a concurrent activation cannot race us into a post-publication
        // conflict. A matching in-flight reservation is a lost-ack/query
        // case, while a different request is an exact conflict.
        {
            let mut state = self.shared.lock();
            let commit_key = NativeOperationKey {
                authority: self.authority.instance_id(),
                continuation: request.continuation,
                operation: request.commit.operation,
                stage: ExternalOperationKind::CommitAuthority,
                digest: request.commit.snapshot_digest,
                preparation_generation: request.commit.binding_receipt_digest,
            };
            let Some(index) = state.operation_index(&commit_key) else {
                return CallOutcome::Indeterminate;
            };
            let row = &mut state.operations[index];
            match row.activation_key.as_ref() {
                Some(existing) if existing.as_ref() != &requested_key => {
                    return CallOutcome::Rejected(UtsRuntimeError::Conflict);
                }
                Some(_) if row.activation.is_none() => {
                    return CallOutcome::Indeterminate;
                }
                // Another activation may have published between commit_row's
                // snapshot and this reservation lock.  Its exact receipt is
                // already recorded in the process-local row; leave ownership
                // untouched and let the query path recover it.
                Some(_) => return CallOutcome::Indeterminate,
                None => row.activation_key = Some(requested_key_for_row.clone()),
            }
        }
        let active_binding = match world_receipt.activate_for_process(self.process) {
            Ok(active_binding) => active_binding,
            Err(_) => {
                let mut state = self.shared.lock();
                let commit_key = NativeOperationKey {
                    authority: self.authority.instance_id(),
                    continuation: request.continuation,
                    operation: request.commit.operation,
                    stage: ExternalOperationKind::CommitAuthority,
                    digest: request.commit.snapshot_digest,
                    preparation_generation: request.commit.binding_receipt_digest,
                };
                if let Some(index) = state.operation_index(&commit_key) {
                    let row = &mut state.operations[index];
                    if row.activation.is_none()
                        && row.activation_key.as_deref() == Some(&requested_key)
                    {
                        row.activation_key = None;
                    }
                }
                return CallOutcome::Rejected(UtsRuntimeError::Busy);
            }
        };
        let active_consumer = active_binding.into_consumer();
        #[cfg(test)]
        {
            let gate = {
                let mut state = self.shared.lock();
                state.faults.activation_publication_gate.take()
            };
            if let Some(gate) = gate {
                // The world is already Active here; the native row still
                // owns the pre-activation committed capability until the
                // final short publication section below.
                gate.pause();
            }
        }
        let mut state = self.shared.lock();
        let commit_key = NativeOperationKey {
            authority: self.authority.instance_id(),
            continuation: request.continuation,
            operation: request.commit.operation,
            stage: ExternalOperationKind::CommitAuthority,
            digest: request.commit.snapshot_digest,
            preparation_generation: request.commit.binding_receipt_digest,
        };
        let Some(index) = state.operation_index(&commit_key) else {
            drop(state);
            drop(active_consumer);
            // The world is already Activating/Active, so a missing native
            // row is an indeterminate process-local publication rather than a
            // reason to manufacture a second capability or panic.
            return CallOutcome::Indeterminate;
        };
        if state.operations[index].activation_key.as_deref() != Some(&requested_key) {
            drop(state);
            drop(active_consumer);
            return CallOutcome::Rejected(UtsRuntimeError::Conflict);
        }
        if state.operations[index].activation.is_some() {
            drop(state);
            drop(active_consumer);
            // A concurrent publisher already owns the exact activation
            // receipt.  The caller can recover it through query_activation;
            // do not touch that terminal row or its capability owners here.
            return CallOutcome::Indeterminate;
        }
        let committed_to_drop = state.operations[index].committed.take();
        state.operations[index].active_consumer = active_consumer;
        state.operations[index].activation = Some(activation_for_row);
        let lost = state.faults.activation_lost_ack;
        state.faults.activation_lost_ack = false;
        drop(state);
        // Both the committed and activation handles are capability owners,
        // not receipt data. Drop the former only after releasing the native
        // SpinNoIrq ledger lock. A consumer fallback is retained only for a
        // process activation that predates lease installation; current
        // ProcessData activation installs the lease in its own slot.
        drop(committed_to_drop);
        if lost {
            CallOutcome::Indeterminate
        } else {
            CallOutcome::Applied(receipt)
        }
    }

    fn query_activation(
        &mut self,
        request: QueryActivationRequest,
    ) -> QueryOutcome<VisaActivationReceipt, Self::ActivationRejection> {
        let requested_key = ActivationKey {
            operation: request.operation,
            binding: request.binding,
        };
        let commit_key = NativeOperationKey {
            authority: self.authority.instance_id(),
            continuation: request.continuation,
            operation: request.commit.operation,
            stage: ExternalOperationKind::CommitAuthority,
            digest: request.commit.snapshot_digest,
            preparation_generation: request.commit.binding_receipt_digest,
        };
        self.release_cancelled_commit(&commit_key);
        let result = {
            let state = self.shared.lock();
            let Some(index) = state.operation_index(&commit_key) else {
                return QueryOutcome::Indeterminate;
            };
            if state.operations[index].binding.snapshot != request.snapshot
                || state.operations[index].binding.destination != request.destination
                || state.operations[index].commit.as_deref() != Some(&request.commit)
            {
                QueryOutcome::Rejected(UtsRuntimeError::Conflict)
            } else {
                match state.operations[index].activation_key.as_deref() {
                    None => QueryOutcome::Absent,
                    Some(existing) if existing != &requested_key => {
                        QueryOutcome::Rejected(UtsRuntimeError::Conflict)
                    }
                    Some(_) => state.operations[index]
                        .activation
                        .clone()
                        .map(QueryOutcome::Applied)
                        .unwrap_or(QueryOutcome::Indeterminate),
                }
            }
        };
        match result {
            QueryOutcome::Applied(receipt) => QueryOutcome::Applied(receipt.as_ref().clone()),
            QueryOutcome::Rejected(error) => QueryOutcome::Rejected(error),
            QueryOutcome::Absent => QueryOutcome::Absent,
            QueryOutcome::Indeterminate => QueryOutcome::Indeterminate,
        }
    }
}

impl fmt::Display for UtsAuthorityRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&rejection(self))
    }
}

impl fmt::Display for UtsRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&runtime_error(self))
    }
}

fn workflow_store() -> AxResult<&'static SpinNoIrq<Option<BoundedWorkflowStore>>> {
    LOCAL_WORKFLOW
        .try_call_once(|| BoundedWorkflowStore::try_new().map(|store| SpinNoIrq::new(Some(store))))
        .map_err(|_| AxError::BadState)
}

fn return_workflow_store(
    slot: &SpinNoIrq<Option<BoundedWorkflowStore>>,
    store: BoundedWorkflowStore,
) {
    *slot.lock() = Some(store);
}

fn map_coordinator_error<SE>(error: CoordinatorError<SE>) -> AxError {
    match error {
        CoordinatorError::NotFound => AxError::BadState,
        CoordinatorError::Core(_) => AxError::InvalidInput,
        CoordinatorError::Store(_) => AxError::ResourceBusy,
    }
}

fn run_coordinator(
    coordinator: &mut Coordinator<BoundedWorkflowStore, UtsAuthorityAdapter, UtsRuntimeAdapter<'_>>,
    id: &ContinuationId,
    recovering: bool,
) -> AxResult<()> {
    let mut first = true;
    for _ in 0..MAX_DRIVE_STEPS {
        let result = if first && recovering {
            first = false;
            coordinator.recover(id).map_err(map_coordinator_error)?
        } else {
            first = false;
            coordinator.drive(id).map_err(map_coordinator_error)?
        };
        match result {
            DriveResult::Activated => return Ok(()),
            DriveResult::Aborted | DriveResult::Fatal => return Err(AxError::BadState),
            DriveResult::DurableBoundary
            | DriveResult::ExternalPending(_)
            | DriveResult::Waiting
            | DriveResult::SourceRestored => {}
        }
    }
    Err(AxError::ResourceBusy)
}

/// Continue one exact external operation through capture, prepare, commit,
/// and ProcessData activation.  Re-entering with the same operation id reads
/// the bounded coordinator record and either recovers it by exact query or
/// returns the already activated terminal result.
pub(crate) fn continue_uts_provider_state(
    process: &ProcessData,
    operation: OperationId,
) -> AxResult<()> {
    let world = super::LOCAL_WORLD.get().ok_or(AxError::BadState)?.clone();
    let continuation = ContinuationId(operation.0);
    let shared = native_state()?;
    let store_slot = workflow_store()?;
    let mut store = {
        let mut guard = store_slot.lock();
        guard.take().ok_or(AxError::BadState)?
    };
    let existing = match store.load(&continuation) {
        Ok(existing) => existing,
        Err(_) => {
            return_workflow_store(store_slot, store);
            return Err(AxError::BadState);
        }
    };
    if let Some(record) = existing.as_ref() {
        if matches!(
            record.phase,
            ContinuationPhase::Progress(Progress::Activated)
        ) {
            return_workflow_store(store_slot, store);
            return Ok(());
        }
    }
    // Resolve the process-local continuation before requiring the process's
    // current UTS to be the intent source.  A committed source is
    // permanently Fenced/Retired, and an activation acknowledgement may be
    // lost after ProcessData has already switched to the destination.  Exact
    // recovery of those stages must not be rejected by a stale source check.
    let (source, intent) = if let Some(record) = existing.as_ref() {
        // The coordinator owns the exact stage query barrier.  In particular,
        // a pending capture/activation must be reconciled from its
        // process-local native row before we inspect the process's current UTS.
        // Runtime capture resolves a source lazily only after an exact query reports
        // Absent; post-commit activation never needs one at all.
        (None, record.intent.clone())
    } else {
        let current_uts = process.uts_ns();
        let source = match world.generation_for_uts(&current_uts) {
            Ok(source) => source,
            Err(error) => {
                return_workflow_store(store_slot, store);
                return Err(error);
            }
        };
        let source_coordinate = generation_coordinate(&source);
        let destination = destination_coordinate(&source_coordinate);
        let snapshot = match source.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return_workflow_store(store_slot, store);
                return Err(error);
            }
        };
        let state = match encode_uts_state(snapshot) {
            Ok(state) => state,
            Err(error) => {
                return_workflow_store(store_slot, store);
                return Err(error);
            }
        };
        let intent = ContinuationIntent {
            id: continuation,
            scope: UTS_SCOPE,
            source: source_coordinate.clone(),
            destination: destination.clone(),
            lineage_parent: LineagePoint {
                lineage: visa_core::LineageId::from_u128(0x5554_532d_4c49_4e45_4147_4501),
                generation: source.coordinate.generation.0.get(),
                state_digest: Digest::of_bytes(&state),
            },
            profile: profile(),
        };
        (Some(source), intent)
    };
    let recovering = existing.is_some();
    let authority = UtsAuthorityAdapter::new(world.clone(), shared.clone());
    let runtime = UtsRuntimeAdapter::new(
        process,
        source,
        world.clone(),
        world.authority.clone(),
        shared,
    );
    let mut coordinator = Coordinator::new(store, authority, runtime);
    if !recovering {
        if let Err(error) = coordinator.begin(intent) {
            let error = map_coordinator_error(error);
            return_workflow_store(store_slot, coordinator.store);
            return Err(error);
        }
    }
    let result = run_coordinator(&mut coordinator, &continuation, recovering);
    store = coordinator.store;
    return_workflow_store(store_slot, store);
    result
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use visa_core::Event;

    use super::{super::AuthorityCapacity, *};
    use crate::task::UtsNamespace;
    extern crate std;

    fn snapshot(node: &[u8], domain: &[u8]) -> UtsProviderSnapshot {
        UtsProviderSnapshot::from_fields(node, domain).unwrap()
    }

    #[test]
    fn canonical_codec_is_bounded_and_round_trips() {
        let logical = snapshot(b"node", b"domain");
        let bytes = encode_uts_state(logical).unwrap();
        assert_eq!(bytes[0], UTS_CODEC_VERSION);
        assert_eq!(bytes[1], 4);
        assert_eq!(decode_uts_state(&bytes).unwrap(), logical);
        assert!(decode_uts_state(&[UTS_CODEC_VERSION, 64, 0, 0]).is_err());
        assert!(decode_uts_state(&bytes[..bytes.len() - 1]).is_err());
        assert!(
            !bytes
                .windows(core::mem::size_of::<usize>())
                .any(|window| { window == &usize::to_ne_bytes(0x1234_5678usize) })
        );
    }

    #[test]
    fn logical_snapshot_has_retain_old_user_namespace_and_no_effects() {
        let world = Authority::try_new(AuthorityCapacity::default())
            .unwrap()
            .try_new_world()
            .unwrap();
        let owner = crate::task::UserNamespace::try_new_root().unwrap();
        let uts = UtsNamespace::try_new_root(owner).unwrap();
        let source = world.register_uts_provider(uts).unwrap();
        let request = CaptureRequest {
            operation: OperationId::from_u128(1),
            continuation: ContinuationId::from_u128(2),
            scope: UTS_SCOPE,
            source: generation_coordinate(&source),
            profile: profile(),
            lineage: visa_core::LineageAdvance {
                parent: LineagePoint {
                    lineage: visa_core::LineageId::from_u128(3),
                    generation: 0,
                    state_digest: Digest::ZERO,
                },
                successor_generation: 1,
            },
        };
        let captured = source
            .capture_after_fence(source.begin_fence().unwrap())
            .unwrap();
        let (envelope, ..) = make_logical_capture(&request, &captured).unwrap();
        assert_eq!(envelope.body.effects.len(), 0);
        assert_eq!(envelope.body.resources.len(), 1);
        assert_eq!(
            envelope.body.resources[0].disposition,
            RebindDisposition::RetainOld
        );
        assert_eq!(
            decode_uts_state(&envelope.body.state).unwrap(),
            captured.snapshot
        );
    }

    #[test]
    fn bounded_store_rejects_capacity_without_recycling_terminal_records() {
        let mut store = BoundedWorkflowStore::try_new().unwrap();
        let parent = LineagePoint {
            lineage: visa_core::LineageId::from_u128(1),
            generation: 0,
            state_digest: Digest::ZERO,
        };
        for value in 0..WORKFLOW_CAPACITY {
            let continuation = ContinuationId::from_u128(value as u128 + 1);
            let intent = ContinuationIntent {
                id: continuation,
                scope: UTS_SCOPE,
                source: ExternalCoordinate {
                    authority: VISA_AUTHORITY,
                    value: vec![1],
                },
                destination: ExternalCoordinate {
                    authority: VISA_AUTHORITY,
                    value: vec![2],
                },
                lineage_parent: LineagePoint {
                    lineage: visa_core::LineageId::from_u128(value as u128 + 10),
                    ..parent.clone()
                },
                profile: profile(),
            };
            let record = visa_core::apply(None, &Event::Begun(intent.clone())).unwrap();
            store
                .create(coordinator::CreateRecord {
                    record,
                    lineage: coordinator::LineageCreate {
                        parent: intent.lineage_parent,
                        active_continuation: continuation,
                    },
                })
                .unwrap();
        }
        let intent = ContinuationIntent {
            id: ContinuationId::from_u128(99),
            scope: UTS_SCOPE,
            source: ExternalCoordinate {
                authority: VISA_AUTHORITY,
                value: vec![1],
            },
            destination: ExternalCoordinate {
                authority: VISA_AUTHORITY,
                value: vec![2],
            },
            lineage_parent: parent,
            profile: profile(),
        };
        let record = visa_core::apply(None, &Event::Begun(intent.clone())).unwrap();
        assert_eq!(
            store.create(coordinator::CreateRecord {
                record,
                lineage: coordinator::LineageCreate {
                    parent: intent.lineage_parent,
                    active_continuation: intent.id,
                },
            }),
            Err(WorkflowStoreError::Capacity)
        );
    }

    #[test]
    fn uts_capture_contract_is_process_local() {
        assert_eq!(UTS_CAPTURE_DURABILITY, CaptureDurability::ProcessLocal);
        assert_ne!(
            UTS_CAPTURE_DURABILITY,
            CaptureDurability::AuthorityDurableQueryable
        );
    }

    struct AuthorityFixture {
        world: SemanticWorld,
        source: GenerationHandle,
        shared: Arc<SpinNoIrq<UtsNativeState>>,
        binding: AuthorityBinding,
    }

    fn authority_fixture() -> AuthorityFixture {
        let world = Authority::try_new(AuthorityCapacity::default())
            .unwrap()
            .try_new_world()
            .unwrap();
        let owner = crate::task::UserNamespace::try_new_root().unwrap();
        let uts = UtsNamespace::try_new_root(owner).unwrap();
        uts.set_nodename(b"uts-source").unwrap();
        let source = world.register_uts_provider(uts).unwrap();
        let shared = Arc::new(SpinNoIrq::new(UtsNativeState::try_new().unwrap()));
        let capture_operation = OperationId::from_u128(0x100);
        let continuation = ContinuationId::from_u128(0x101);
        let capture_request = CaptureRequest {
            operation: capture_operation,
            continuation,
            scope: UTS_SCOPE,
            source: generation_coordinate(&source),
            profile: profile(),
            lineage: visa_core::LineageAdvance {
                parent: LineagePoint {
                    lineage: visa_core::LineageId::from_u128(0x102),
                    generation: 0,
                    state_digest: Digest::ZERO,
                },
                successor_generation: 1,
            },
        };
        let captured = source
            .capture_after_fence(source.begin_fence().unwrap())
            .unwrap();
        let (snapshot, safe_point, receipt) =
            make_logical_capture(&capture_request, &captured).unwrap();
        let snapshot_arc = Arc::new(snapshot.clone());
        let safe_point_arc = Arc::new(safe_point);
        let receipt_arc = Arc::new(receipt.clone());
        shared.lock().captures.push(NativeCaptureRow {
            capture: NativeCapture {
                key: NativeCaptureKey {
                    authority: world.authority.instance_id(),
                    continuation,
                    operation: capture_operation,
                    stage: ExternalOperationKind::CaptureSource,
                    digest: Digest::ZERO,
                    preparation_generation: Digest::ZERO,
                },
                snapshot: snapshot_arc,
                safe_point: safe_point_arc,
                receipt: receipt_arc,
            },
            frozen: Some(captured),
            restored: None,
        });
        let source_coordinate = generation_coordinate(&source);
        let binding = AuthorityBinding {
            continuation,
            snapshot: snapshot.body.snapshot,
            source: source_coordinate.clone(),
            destination: destination_coordinate(&source_coordinate),
            requirements: snapshot.body.resources.clone(),
            capture_receipt: Some(receipt),
            preparation_digest: snapshot.body_digest,
        };
        AuthorityFixture {
            world,
            source,
            shared,
            binding,
        }
    }

    #[test]
    fn authority_adapter_is_idempotent_conflicting_and_postcommit_closed() {
        let fixture = authority_fixture();
        let mut adapter = UtsAuthorityAdapter::new(fixture.world.clone(), fixture.shared);
        let prepare_operation = OperationId::from_u128(0x103);
        let preparation = match adapter.prepare(PrepareRequest {
            operation: prepare_operation,
            binding: fixture.binding.clone(),
        }) {
            CallOutcome::Applied(receipt) => receipt,
            other => panic!("unexpected prepare outcome: {other:?}"),
        };
        assert_eq!(
            adapter.prepare(PrepareRequest {
                operation: prepare_operation,
                binding: fixture.binding.clone(),
            }),
            CallOutcome::Applied(preparation.clone())
        );
        let mut conflicting = fixture.binding.clone();
        conflicting.preparation_digest = Digest::of_bytes(b"wrong");
        assert!(matches!(
            adapter.prepare(PrepareRequest {
                operation: prepare_operation,
                binding: conflicting
            }),
            CallOutcome::Rejected(UtsAuthorityRejection::Conflict)
                | CallOutcome::Rejected(UtsAuthorityRejection::Invalid)
        ));
        assert!(fixture.source.enter().is_none());
        let commit_operation = OperationId::from_u128(0x104);
        let mut conflicting_preparation = preparation.clone();
        conflicting_preparation.operation = OperationId::from_u128(0x10a);
        assert!(matches!(
            adapter.commit(CommitRequest {
                operation: commit_operation,
                binding: fixture.binding.clone(),
                preparation: conflicting_preparation,
            }),
            CallOutcome::Rejected(UtsAuthorityRejection::Conflict)
                | CallOutcome::Rejected(UtsAuthorityRejection::Invalid)
        ));
        let commit = match adapter.commit(CommitRequest {
            operation: commit_operation,
            binding: fixture.binding.clone(),
            preparation: preparation.clone(),
        }) {
            CallOutcome::Applied(receipt) => receipt,
            other => panic!("unexpected commit outcome: {other:?}"),
        };
        assert_eq!(
            adapter.query_commit(QueryCommitRequest {
                operation: commit_operation,
                binding: fixture.binding.clone(),
                preparation: preparation.clone(),
            }),
            QueryOutcome::Applied(commit.clone())
        );
        let abort_operation = OperationId::from_u128(0x105);
        assert!(matches!(
            adapter.abort_preparation(AbortPreparationRequest {
                operation: abort_operation,
                binding: fixture.binding,
                preparation,
            }),
            CallOutcome::Rejected(UtsAuthorityRejection::Busy)
        ));
        assert!(fixture.source.enter().is_none());
        assert_ne!(commit.execution_epoch, 0);
    }

    #[test]
    fn process_local_authority_loss_is_indeterminate_for_recovery() {
        let fixture = authority_fixture();
        let shared = fixture.shared.clone();
        let mut binding = fixture.binding.clone();
        // A ProcessLocal capture deliberately has no coordinator-visible
        // receipt. Removing the native projection models authority loss
        // after that local cut; neither prepare nor its exact query may turn
        // the missing row into an authoritative rejection/absence.
        binding.capture_receipt = None;
        let frozen = {
            let mut state = shared.lock();
            let frozen = state.captures[0].frozen.take();
            state.captures.clear();
            frozen
        };
        drop(frozen);
        let mut adapter = UtsAuthorityAdapter::new(fixture.world, shared);
        let operation = OperationId::from_u128(0x116);
        assert_eq!(
            adapter.prepare(PrepareRequest {
                operation,
                binding: binding.clone(),
            }),
            CallOutcome::Indeterminate
        );
        assert_eq!(
            adapter.query_prepare(QueryPrepareRequest { operation, binding }),
            QueryOutcome::Indeterminate
        );
    }

    #[test]
    fn process_local_binding_keeps_native_commit_without_durable_capture_receipt() {
        let fixture = authority_fixture();
        let mut binding = fixture.binding.clone();
        binding.capture_receipt = None;
        let world = fixture.world.clone();
        let shared = fixture.shared.clone();
        let mut adapter = UtsAuthorityAdapter::new(fixture.world, fixture.shared);
        let preparation = match adapter.prepare(PrepareRequest {
            operation: OperationId::from_u128(0x117),
            binding: binding.clone(),
        }) {
            CallOutcome::Applied(receipt) => receipt,
            other => panic!("unexpected process-local prepare outcome: {other:?}"),
        };
        assert!(matches!(
            UtsRuntimeAdapter::destination_for(
                &world.authority,
                &shared,
                binding.continuation,
                binding.snapshot,
                &binding.destination,
                &binding.requirements,
                binding.preparation_digest,
            ),
            Ok(_)
        ));
        assert!(matches!(
            adapter.commit(CommitRequest {
                operation: OperationId::from_u128(0x118),
                binding,
                preparation,
            }),
            CallOutcome::Applied(_)
        ));
    }

    #[test]
    fn prepare_post_take_failure_restores_exact_capture_token() {
        let fixture = authority_fixture();
        let shared = fixture.shared.clone();
        shared.lock().faults.prepare_post_take_failure = true;
        let mut adapter = UtsAuthorityAdapter::new(fixture.world.clone(), fixture.shared);
        let request = PrepareRequest {
            operation: OperationId::from_u128(0x10b),
            binding: fixture.binding.clone(),
        };
        assert_eq!(
            adapter.prepare(request),
            CallOutcome::Rejected(UtsAuthorityRejection::Busy)
        );
        assert!(fixture.source.enter().is_none());
        assert!(shared.lock().captures[0].frozen.is_some());

        // The exact external request can retry because the failed canonical
        // preparation was aborted and its opaque source fence was returned to
        // the process-local capture row instead of being silently thawed.
        assert!(matches!(
            adapter.prepare(PrepareRequest {
                operation: OperationId::from_u128(0x10b),
                binding: fixture.binding.clone(),
            }),
            CallOutcome::Applied(_)
        ));
        assert!(shared.lock().captures[0].frozen.is_none());
    }

    #[test]
    fn authority_adapter_abort_records_exact_source_restoration() {
        let fixture = authority_fixture();
        let shared = fixture.shared.clone();
        let authority = fixture.world.authority.instance_id();
        let snapshot = shared.lock().captures[0].capture.snapshot.as_ref().clone();
        let source_coordinate = fixture.binding.source.clone();
        let mut adapter = UtsAuthorityAdapter::new(fixture.world, fixture.shared);
        let preparation = match adapter.prepare(PrepareRequest {
            operation: OperationId::from_u128(0x106),
            binding: fixture.binding.clone(),
        }) {
            CallOutcome::Applied(receipt) => receipt,
            other => panic!("unexpected prepare outcome: {other:?}"),
        };
        let abort_operation = OperationId::from_u128(0x107);
        let abort = match adapter.abort_preparation(AbortPreparationRequest {
            operation: abort_operation,
            binding: fixture.binding.clone(),
            preparation: preparation.clone(),
        }) {
            CallOutcome::Applied(receipt) => receipt,
            other => panic!("unexpected abort outcome: {other:?}"),
        };
        assert_eq!(
            adapter.query_abort(QueryAbortRequest {
                operation: abort_operation,
                binding: fixture.binding.clone(),
                preparation: preparation.clone(),
            }),
            QueryOutcome::Applied(abort)
        );
        assert!(fixture.source.enter().is_none());
        let restore = RestoreSourceRequest {
            continuation: fixture.binding.continuation,
            snapshot,
            source: source_coordinate,
        };
        let receipt = match restore_capture(&shared, authority, restore.clone()) {
            CallOutcome::Applied(receipt) => receipt,
            other => panic!("unexpected source restore outcome: {other:?}"),
        };
        assert!(receipt.verify().is_ok());
        assert!(fixture.source.enter().is_some());
        assert_eq!(
            restore_capture(&shared, authority, restore),
            CallOutcome::Applied(receipt)
        );
    }

    #[test]
    fn abort_rejects_same_external_operation_with_conflicting_preparation() {
        let fixture = authority_fixture();
        let mut adapter = UtsAuthorityAdapter::new(fixture.world.clone(), fixture.shared);
        let preparation = match adapter.prepare(PrepareRequest {
            operation: OperationId::from_u128(0x130),
            binding: fixture.binding.clone(),
        }) {
            CallOutcome::Applied(receipt) => receipt,
            other => panic!("unexpected prepare outcome: {other:?}"),
        };
        let abort_operation = OperationId::from_u128(0x131);
        assert!(matches!(
            adapter.abort_preparation(AbortPreparationRequest {
                operation: abort_operation,
                binding: fixture.binding.clone(),
                preparation: preparation.clone(),
            }),
            CallOutcome::Applied(_)
        ));

        let mut conflicting = preparation.clone();
        conflicting.operation = OperationId::from_u128(0x132);
        conflicting.receipt_digest = Digest::ZERO;
        let conflicting = conflicting.seal().unwrap();
        assert_eq!(
            adapter.abort_preparation(AbortPreparationRequest {
                operation: abort_operation,
                binding: fixture.binding,
                preparation: conflicting,
            }),
            CallOutcome::Rejected(UtsAuthorityRejection::Conflict)
        );
    }

    #[test]
    fn authority_commit_lost_ack_is_recovered_by_exact_query() {
        let fixture = authority_fixture();
        let shared = fixture.shared.clone();
        let mut adapter = UtsAuthorityAdapter::new(fixture.world, fixture.shared);
        let preparation = match adapter.prepare(PrepareRequest {
            operation: OperationId::from_u128(0x108),
            binding: fixture.binding.clone(),
        }) {
            CallOutcome::Applied(receipt) => receipt,
            other => panic!("unexpected prepare outcome: {other:?}"),
        };
        shared.lock().faults.commit_lost_ack = true;
        let operation = OperationId::from_u128(0x109);
        assert_eq!(
            adapter.commit(CommitRequest {
                operation,
                binding: fixture.binding.clone(),
                preparation: preparation.clone(),
            }),
            CallOutcome::Indeterminate
        );
        assert!(matches!(
            adapter.query_commit(QueryCommitRequest {
                operation,
                binding: fixture.binding,
                preparation,
            }),
            QueryOutcome::Applied(_)
        ));
        assert!(fixture.source.enter().is_none());
    }

    #[test]
    fn commit_row_reconciles_world_active_before_native_activation_ack() {
        let fixture = authority_fixture();
        let shared = fixture.shared.clone();
        let mut adapter = UtsAuthorityAdapter::new(fixture.world.clone(), shared.clone());
        let preparation = match adapter.prepare(PrepareRequest {
            operation: OperationId::from_u128(0x152),
            binding: fixture.binding.clone(),
        }) {
            CallOutcome::Applied(receipt) => receipt,
            other => panic!("unexpected prepare outcome: {other:?}"),
        };
        let operation = OperationId::from_u128(0x153);
        assert!(matches!(
            adapter.commit(CommitRequest {
                operation,
                binding: fixture.binding.clone(),
                preparation,
            }),
            CallOutcome::Applied(_)
        ));
        let (world_receipt, digest, preparation_generation) = {
            let state = shared.lock();
            let index = state
                .operations
                .iter()
                .position(|row| {
                    row.key.stage == ExternalOperationKind::CommitAuthority
                        && row.key.operation == operation
                })
                .unwrap();
            let row = &state.operations[index];
            (
                row.world_receipt.clone().unwrap(),
                row.key.digest,
                row.key.preparation_generation,
            )
        };

        // Publish world Active while the native row still owns the original
        // committed capability. A retry through commit_row must accept this
        // exact state and never panic on finish_commit_publication.
        let active = world_receipt.activate().unwrap();
        let reconciled = UtsRuntimeAdapter::commit_row_for(
            &fixture.world.authority,
            &shared,
            fixture.binding.continuation,
            digest,
            operation,
            preparation_generation,
        )
        .unwrap();
        assert!(!reconciled.1);
        drop(active);
    }

    #[test]
    fn cancellation_cannot_miss_pending_commit_handle_publication() {
        let fixture = authority_fixture();
        let shared = fixture.shared.clone();
        let preparation = {
            let mut adapter = UtsAuthorityAdapter::new(fixture.world.clone(), shared.clone());
            match adapter.prepare(PrepareRequest {
                operation: OperationId::from_u128(0x150),
                binding: fixture.binding.clone(),
            }) {
                CallOutcome::Applied(receipt) => receipt,
                other => panic!("unexpected prepare outcome: {other:?}"),
            }
        };
        let operation = OperationId::from_u128(0x151);
        let gate = Arc::new(TestGate::new());
        shared.lock().faults.commit_publication_gate = Some(gate.clone());

        let commit_world = fixture.world.clone();
        let commit_shared = shared.clone();
        let commit_binding = fixture.binding.clone();
        let commit_preparation = preparation.clone();
        let worker = std::thread::spawn(move || {
            let mut adapter = UtsAuthorityAdapter::new(commit_world, commit_shared);
            adapter.commit(CommitRequest {
                operation,
                binding: commit_binding,
                preparation: commit_preparation,
            })
        });

        // The authority has applied the operation and sealed the source, but
        // the native row is intentionally paused before it receives the
        // destination capability.  Cancellation must not win this window:
        // returning success here would leave the later handle owner orphaned.
        gate.wait_until_entered();
        let canonical = {
            let state = shared.lock();
            let index = state
                .operations
                .iter()
                .position(|row| {
                    row.key.stage == ExternalOperationKind::CommitAuthority
                        && row.key.operation == operation
                })
                .unwrap();
            assert!(state.operations[index].committed.is_none());
            state.operations[index].canonical.clone().unwrap()
        };
        assert_eq!(
            fixture
                .world
                .cancel_operation(&canonical, fixture.binding.preparation_digest.0),
            Err(AxError::ResourceBusy)
        );
        assert!(fixture.source.enter().is_none());

        gate.release();
        assert!(matches!(worker.join().unwrap(), CallOutcome::Applied(_)));
        let destination = {
            let state = shared.lock();
            let index = state
                .operations
                .iter()
                .position(|row| {
                    row.key.stage == ExternalOperationKind::CommitAuthority
                        && row.key.operation == operation
                })
                .unwrap();
            state.operations[index]
                .committed
                .as_ref()
                .unwrap()
                .destination()
                .coordinate
        };

        // Once the native row owns the handle, cancellation can linearize.
        // The isolated fixture does not install LOCAL_NATIVE, so invoke the
        // same exact-row reconciliation helper explicitly and prove that the
        // capability has one—and only one—terminal owner.
        fixture
            .world
            .cancel_operation(&canonical, fixture.binding.preparation_digest.0)
            .unwrap();
        assert!(release_cancelled_commit_in(
            &shared,
            &fixture.world.authority,
            canonical.operation(),
        ));
        assert!(!release_cancelled_commit_in(
            &shared,
            &fixture.world.authority,
            canonical.operation(),
        ));
        let state = shared.lock();
        let index = state
            .operations
            .iter()
            .position(|row| {
                row.key.stage == ExternalOperationKind::CommitAuthority
                    && row.key.operation == operation
            })
            .unwrap();
        assert!(state.operations[index].committed.is_none());
        assert!(state.operations[index].commit.is_some());
        drop(state);
        let authority_state = fixture.world.authority.state.lock();
        assert!(Authority::provider_index(&authority_state, destination).is_none());
    }

    #[test]
    fn cancellation_releases_native_commit_handle_but_keeps_receipt() {
        let fixture = authority_fixture();
        let shared = fixture.shared.clone();
        let mut adapter = UtsAuthorityAdapter::new(fixture.world.clone(), shared.clone());
        let preparation = match adapter.prepare(PrepareRequest {
            operation: OperationId::from_u128(0x140),
            binding: fixture.binding.clone(),
        }) {
            CallOutcome::Applied(receipt) => receipt,
            other => panic!("unexpected prepare outcome: {other:?}"),
        };
        let operation = OperationId::from_u128(0x141);
        assert!(matches!(
            adapter.commit(CommitRequest {
                operation,
                binding: fixture.binding.clone(),
                preparation,
            }),
            CallOutcome::Applied(_)
        ));
        let (canonical, destination) = {
            let state = shared.lock();
            let index = state
                .operations
                .iter()
                .position(|row| {
                    row.key.stage == ExternalOperationKind::CommitAuthority
                        && row.key.operation == operation
                })
                .unwrap();
            let row = &state.operations[index];
            (
                row.canonical.clone().unwrap(),
                row.committed.as_ref().unwrap().destination().coordinate,
            )
        };
        fixture
            .world
            .cancel_operation(&canonical, fixture.binding.preparation_digest.0)
            .unwrap();
        // The fixture owns an isolated native store rather than the process
        // singleton; exercise the same exact-operation cleanup helper used by
        // the world's cancellation callback.
        assert!(release_cancelled_commit_in(
            &shared,
            &fixture.world.authority,
            canonical.operation(),
        ));
        let state = shared.lock();
        let index = state
            .operations
            .iter()
            .position(|row| {
                row.key.stage == ExternalOperationKind::CommitAuthority
                    && row.key.operation == operation
            })
            .unwrap();
        assert!(state.operations[index].committed.is_none());
        assert!(state.operations[index].commit.is_some());
        drop(state);
        let authority_state = fixture.world.authority.state.lock();
        assert!(Authority::provider_index(&authority_state, destination).is_none());
    }

    #[test]
    fn concurrent_commit_failure_cleanup_and_abort_use_exact_rows() {
        let fixture = authority_fixture();
        let shared = fixture.shared.clone();
        let binding = fixture.binding.clone();
        let preparation = {
            let mut adapter = UtsAuthorityAdapter::new(fixture.world.clone(), shared.clone());
            match adapter.prepare(PrepareRequest {
                operation: OperationId::from_u128(0x110),
                binding: binding.clone(),
            }) {
                CallOutcome::Applied(receipt) => receipt,
                other => panic!("unexpected prepare outcome: {other:?}"),
            }
        };
        let gate = Arc::new(TestGate::new());
        let abort_gate = Arc::new(TestGate::new());
        {
            let mut state = shared.lock();
            state.faults.commit_force_failure = true;
            state.faults.commit_failure_gate = Some(gate.clone());
            state.faults.abort_lookup_gate = Some(abort_gate.clone());
        }
        let commit_world = fixture.world.clone();
        let commit_shared = shared.clone();
        let commit_binding = binding.clone();
        let commit_preparation = preparation.clone();
        let worker = std::thread::spawn(move || {
            let mut adapter = UtsAuthorityAdapter::new(commit_world, commit_shared);
            adapter.commit(CommitRequest {
                operation: OperationId::from_u128(0x111),
                binding: commit_binding,
                preparation: commit_preparation,
            })
        });

        // The worker has taken the prepared token and published its in-flight
        // commit row, but has not entered the forced pre-commit failure path.
        gate.wait_until_entered();
        let abort_request = AbortPreparationRequest {
            operation: OperationId::from_u128(0x112),
            binding: binding.clone(),
            preparation: preparation.clone(),
        };
        let abort_world = fixture.world.clone();
        let abort_shared = shared.clone();
        let abort_worker = std::thread::spawn(move || {
            let mut adapter = UtsAuthorityAdapter::new(abort_world, abort_shared);
            adapter.abort_preparation(abort_request)
        });
        // The abort worker is paused after its old cross-lock lookup point.
        // Commit cleanup can now remove and shift both rows before abort
        // performs its fresh exact identity lookup. The old implementation
        // retained a Vec index here and would panic after that removal.
        abort_gate.wait_until_entered();
        gate.release();
        assert_eq!(
            worker.join().unwrap(),
            CallOutcome::Rejected(UtsAuthorityRejection::Busy)
        );
        abort_gate.release();

        // Commit cleanup removed both rows by exact identity and restored the
        // opaque capture token. A later abort therefore reports Busy, rather
        // than indexing a shifted Vec row.
        assert_eq!(
            abort_worker.join().unwrap(),
            CallOutcome::Rejected(UtsAuthorityRejection::Busy)
        );
        let state = shared.lock();
        assert!(state.operations.iter().all(|row| {
            row.continuation != binding.continuation
                || !matches!(
                    row.key.stage,
                    ExternalOperationKind::PrepareBindings | ExternalOperationKind::CommitAuthority
                )
        }));
        assert!(state.captures[0].frozen.is_some());
    }

    #[test]
    fn commit_preflight_receipt_and_capacity_failures_keep_prepared_token() {
        let fixture = authority_fixture();
        let shared = fixture.shared.clone();
        let mut adapter = UtsAuthorityAdapter::new(fixture.world.clone(), fixture.shared);
        let preparation = match adapter.prepare(PrepareRequest {
            operation: OperationId::from_u128(0x10c),
            binding: fixture.binding.clone(),
        }) {
            CallOutcome::Applied(receipt) => receipt,
            other => panic!("unexpected prepare outcome: {other:?}"),
        };
        let request = || CommitRequest {
            operation: OperationId::from_u128(0x10d),
            binding: fixture.binding.clone(),
            preparation: preparation.clone(),
        };

        shared.lock().faults.commit_receipt_failure = true;
        assert_eq!(
            adapter.commit(request()),
            CallOutcome::Rejected(UtsAuthorityRejection::Invalid)
        );
        assert!(
            shared
                .lock()
                .operation_index(&NativeOperationKey {
                    authority: fixture.world.authority.instance_id(),
                    continuation: fixture.binding.continuation,
                    operation: OperationId::from_u128(0x10d),
                    stage: ExternalOperationKind::CommitAuthority,
                    digest: fixture.binding.preparation_digest,
                    preparation_generation: preparation.receipt_digest,
                })
                .is_none()
        );

        shared.lock().faults.commit_capacity_failure = true;
        assert_eq!(
            adapter.commit(request()),
            CallOutcome::Rejected(UtsAuthorityRejection::Capacity)
        );
        let state = shared.lock();
        let prep_index = state
            .operation_index(&NativeOperationKey {
                authority: fixture.world.authority.instance_id(),
                continuation: fixture.binding.continuation,
                operation: preparation.operation,
                stage: ExternalOperationKind::PrepareBindings,
                digest: fixture.binding.preparation_digest,
                preparation_generation: fixture
                    .binding
                    .capture_receipt
                    .as_ref()
                    .unwrap()
                    .receipt_digest,
            })
            .unwrap();
        assert!(state.operations[prep_index].prepared.is_some());
        drop(state);
        assert!(fixture.source.enter().is_none());

        assert!(matches!(adapter.commit(request()), CallOutcome::Applied(_)));
        assert!(fixture.source.enter().is_none());
    }

    #[test]
    fn abort_capacity_is_reserved_before_preparation_removal() {
        let fixture = authority_fixture();
        let shared = fixture.shared.clone();
        let mut adapter = UtsAuthorityAdapter::new(fixture.world.clone(), fixture.shared);
        let preparation = match adapter.prepare(PrepareRequest {
            operation: OperationId::from_u128(0x120),
            binding: fixture.binding.clone(),
        }) {
            CallOutcome::Applied(receipt) => receipt,
            other => panic!("unexpected prepare outcome: {other:?}"),
        };
        let filler = {
            let state = shared.lock();
            NATIVE_ROW_CAPACITY - state.operations.len()
        };
        {
            let mut state = shared.lock();
            for index in 0..filler {
                state.operations.push(NativeOperation {
                    key: NativeOperationKey {
                        authority: fixture.world.authority.instance_id(),
                        continuation: fixture.binding.continuation,
                        operation: OperationId::from_u128(0x200 + index as u128),
                        stage: ExternalOperationKind::CaptureSource,
                        digest: Digest::ZERO,
                        preparation_generation: Digest::ZERO,
                    },
                    continuation: fixture.binding.continuation,
                    binding: fixture.binding.clone(),
                    canonical: None,
                    preparation: None,
                    prepared: None,
                    committed: None,
                    world_receipt: None,
                    active_consumer: None,
                    commit: None,
                    abort: None,
                    activation_key: None,
                    activation: None,
                });
            }
        }
        assert_eq!(
            adapter.abort_preparation(AbortPreparationRequest {
                operation: OperationId::from_u128(0x121),
                binding: fixture.binding.clone(),
                preparation: preparation.clone(),
            }),
            CallOutcome::Rejected(UtsAuthorityRejection::Capacity)
        );
        let state = shared.lock();
        let prep_key = NativeOperationKey {
            authority: fixture.world.authority.instance_id(),
            continuation: fixture.binding.continuation,
            operation: preparation.operation,
            stage: ExternalOperationKind::PrepareBindings,
            digest: fixture.binding.preparation_digest,
            preparation_generation: fixture
                .binding
                .capture_receipt
                .as_ref()
                .unwrap()
                .receipt_digest,
        };
        let prep_index = state.operation_index(&prep_key).unwrap();
        assert!(state.operations[prep_index].prepared.is_some());
        assert_eq!(state.operations.len(), NATIVE_ROW_CAPACITY);
        assert!(fixture.source.enter().is_none());
    }

    #[test]
    fn prior_authority_instance_cannot_replay_native_capture() {
        let fixture = authority_fixture();
        let old_binding = fixture.binding.clone();
        let new_world = Authority::try_new(AuthorityCapacity::default())
            .unwrap()
            .try_new_world()
            .unwrap();
        let new_shared = Arc::new(SpinNoIrq::new(UtsNativeState::try_new().unwrap()));
        let mut adapter = UtsAuthorityAdapter::new(new_world, new_shared.clone());
        assert!(matches!(
            adapter.prepare(PrepareRequest {
                operation: OperationId::from_u128(0x122),
                binding: old_binding,
            }),
            CallOutcome::Rejected(UtsAuthorityRejection::Missing)
                | CallOutcome::Rejected(UtsAuthorityRejection::Conflict)
        ));
        assert!(new_shared.lock().operations.is_empty());
    }

    #[test]
    fn commit_world_query_unknown_publishes_commit_and_exact_query_rehydrates() {
        let fixture = authority_fixture();
        let shared = fixture.shared.clone();
        let mut adapter = UtsAuthorityAdapter::new(fixture.world.clone(), fixture.shared);
        let preparation = match adapter.prepare(PrepareRequest {
            operation: OperationId::from_u128(0x10e),
            binding: fixture.binding.clone(),
        }) {
            CallOutcome::Applied(receipt) => receipt,
            other => panic!("unexpected prepare outcome: {other:?}"),
        };
        let operation = OperationId::from_u128(0x10f);
        shared.lock().faults.commit_world_query_unknown = true;
        let commit = match adapter.commit(CommitRequest {
            operation,
            binding: fixture.binding.clone(),
            preparation: preparation.clone(),
        }) {
            CallOutcome::Applied(receipt) => receipt,
            other => panic!("unexpected commit outcome: {other:?}"),
        };
        {
            let state = shared.lock();
            let index = state
                .operation_index(&NativeOperationKey {
                    authority: fixture.world.authority.instance_id(),
                    continuation: fixture.binding.continuation,
                    operation,
                    stage: ExternalOperationKind::CommitAuthority,
                    digest: fixture.binding.preparation_digest,
                    preparation_generation: preparation.receipt_digest,
                })
                .unwrap();
            assert_eq!(state.operations[index].commit.as_deref(), Some(&commit));
            assert!(state.operations[index].world_receipt.is_none());
        }
        assert_eq!(
            adapter.query_commit(QueryCommitRequest {
                operation,
                binding: fixture.binding.clone(),
                preparation: preparation.clone(),
            }),
            QueryOutcome::Applied(commit.clone())
        );

        let canonical = {
            let state = shared.lock();
            let index = state
                .operation_index(&NativeOperationKey {
                    authority: fixture.world.authority.instance_id(),
                    continuation: fixture.binding.continuation,
                    operation,
                    stage: ExternalOperationKind::CommitAuthority,
                    digest: fixture.binding.preparation_digest,
                    preparation_generation: preparation.receipt_digest,
                })
                .unwrap();
            state.operations[index]
                .committed
                .as_ref()
                .unwrap()
                .operation()
        };
        assert!(
            UtsAuthorityAdapter::publish_world_receipt(
                &fixture.world.authority,
                &shared,
                fixture.binding.continuation,
                fixture.binding.preparation_digest,
                operation,
                preparation.receipt_digest,
                canonical,
            )
            .is_some()
        );
        assert!(shared.lock().operations.iter().any(|row| {
            row.key.stage == ExternalOperationKind::CommitAuthority && row.world_receipt.is_some()
        }));
        assert!(fixture.source.enter().is_none());
    }

    #[test]
    fn ambiguous_destination_match_is_conflict() {
        let fixture = authority_fixture();
        let shared = fixture.shared.clone();
        let preparation = {
            let mut authority = UtsAuthorityAdapter::new(fixture.world.clone(), shared.clone());
            match authority.prepare(PrepareRequest {
                operation: OperationId::from_u128(0x133),
                binding: fixture.binding.clone(),
            }) {
                CallOutcome::Applied(receipt) => receipt,
                other => panic!("unexpected prepare outcome: {other:?}"),
            }
        };
        let (
            row_key,
            row_continuation,
            row_binding,
            row_canonical,
            row_preparation,
            binding_id,
            operation,
            destination_coordinate,
            source_fence_epoch,
            execution_epoch,
        ) = {
            let state = shared.lock();
            let index = state
                .operation_index(&NativeOperationKey {
                    authority: fixture.world.authority.instance_id(),
                    continuation: fixture.binding.continuation,
                    operation: preparation.operation,
                    stage: ExternalOperationKind::PrepareBindings,
                    digest: fixture.binding.preparation_digest,
                    preparation_generation: fixture
                        .binding
                        .capture_receipt
                        .as_ref()
                        .unwrap()
                        .receipt_digest,
                })
                .unwrap();
            let row = &state.operations[index];
            let prepared = row.prepared.as_ref().unwrap();
            (
                row.key,
                row.continuation,
                row.binding.clone(),
                row.canonical.clone(),
                row.preparation.clone(),
                prepared.binding,
                prepared.operation,
                prepared.destination,
                prepared.source_fence_epoch().unwrap(),
                prepared.execution_epoch().unwrap(),
            )
        };
        let destination = {
            let mut state = fixture.world.authority.state.lock();
            Authority::destination_handle(
                &fixture.world.authority,
                &mut state,
                destination_coordinate,
            )
            .unwrap()
        };
        let duplicate = NativeOperation {
            key: row_key,
            continuation: row_continuation,
            binding: row_binding,
            canonical: row_canonical,
            preparation: row_preparation,
            prepared: None,
            committed: Some(CommittedBinding {
                authority: fixture.world.authority.clone(),
                binding: binding_id,
                destination,
                operation,
                source_fence_epoch: super::super::FenceEpoch(
                    core::num::NonZeroU64::new(source_fence_epoch).unwrap(),
                ),
                execution_epoch: super::super::ExecutionEpoch(
                    core::num::NonZeroU64::new(execution_epoch).unwrap(),
                ),
            }),
            world_receipt: None,
            active_consumer: None,
            commit: None,
            abort: None,
            activation_key: None,
            activation: None,
        };
        shared.lock().operations.push(duplicate);
        let capture = shared.lock().captures[0].capture.clone();
        assert!(matches!(
            UtsRuntimeAdapter::destination_for(
                &fixture.world.authority,
                &shared,
                fixture.binding.continuation,
                capture.snapshot.body.snapshot,
                &fixture.binding.destination,
                &fixture.binding.requirements,
                fixture.binding.preparation_digest,
            ),
            Err(UtsRuntimeError::Conflict)
        ));
    }
}
