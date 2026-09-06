use core::num::NonZeroU64;

use crate::IoUringError;

/// Linux's maximum setup submission entries.
pub const IORING_MAX_ENTRIES: u32 = 32_768;
/// Linux's maximum setup completion entries.
pub const IORING_MAX_CQ_ENTRIES: u32 = 65_536;
/// Linux's maximum registered-file slots before caller resource limits.
pub const IORING_MAX_FIXED_FILES: u32 = 1 << 20;

/// Shared SQ/CQ ring mmap offset.
pub const IORING_OFF_SQ_RING: u64 = 0;
/// Shared CQ ring mmap offset. `SINGLE_MMAP` aliases this to the SQ mapping.
pub const IORING_OFF_CQ_RING: u64 = 0x0800_0000;
/// Submission-entry array mmap offset.
pub const IORING_OFF_SQES: u64 = 0x1000_0000;

/// Bytes in one Linux ABI SQE supported by the first slice.
pub const SQE_BYTES: u32 = 64;
/// Bytes in one Linux ABI CQE supported by the first slice.
pub const CQE_BYTES: u32 = 16;
/// Bytes occupied by the cacheline-aligned shared ring header.
pub const RING_HEADER_BYTES: u32 = 64;

const RING_ALIGNMENT: u32 = 64;

/// Caller-allocated stable identity for one ring instance.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RingId(NonZeroU64);

impl RingId {
    /// Builds a ring identity, rejecting the reserved zero value.
    pub const fn new(raw: u64) -> Result<Self, IoUringError> {
        match NonZeroU64::new(raw) {
            Some(raw) => Ok(Self(raw)),
            None => Err(IoUringError::InvalidIdentity),
        }
    }

    /// Returns the consumer-visible identity value.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Strictly decoded setup flags supported by the initial core.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SetupFlags(u32);

impl SetupFlags {
    /// Userspace supplied the requested CQ size in `cq_entries`.
    pub const CQSIZE: Self = Self(1 << 3);
    /// Excessive SQ/CQ sizes are clamped to Linux's limits.
    pub const CLAMP: Self = Self(1 << 4);
    /// Submission indexes directly address SQEs without an SQ array.
    pub const NO_SQARRAY: Self = Self(1 << 16);
    /// Submitters may ask the kernel SQ polling worker to consume SQEs.
    pub const SQPOLL: Self = Self(1 << 1);
    /// Bind the SQPOLL worker to `sq_thread_cpu`.
    pub const SQ_AFF: Self = Self(1 << 2);
    /// Completion is driven by provider/device polling rather than IRQ wake.
    pub const IOPOLL: Self = Self(1 << 0);
    /// Completion work is deferred to task-work delivery.
    pub const DEFER_TASKRUN: Self = Self(1 << 13);
    /// Cooperatively run queued task work at syscall boundaries.
    pub const COOP_TASKRUN: Self = Self(1 << 8);
    /// Only the creating issuer may enter the ring.
    pub const SINGLE_ISSUER: Self = Self(1 << 12);
    /// Start disabled so resources may be registered before submissions are
    /// admitted; `REGISTER_ENABLE_RINGS` performs the one-way transition.
    pub const R_DISABLED: Self = Self(1 << 6);
    /// Every flag implemented by this version.
    pub const SUPPORTED: Self = Self(
        Self::CQSIZE.0
            | Self::CLAMP.0
            | Self::NO_SQARRAY.0
            | Self::SQPOLL.0
            | Self::SQ_AFF.0
            | Self::IOPOLL.0
            | Self::DEFER_TASKRUN.0
            | Self::COOP_TASKRUN.0
            | Self::SINGLE_ISSUER.0
            | Self::R_DISABLED.0,
    );

    /// Strictly decodes setup bits before any allocation occurs.
    pub const fn from_bits(bits: u32) -> Result<Self, IoUringError> {
        if bits & !Self::SUPPORTED.0 == 0 {
            Ok(Self(bits))
        } else {
            Err(IoUringError::UnsupportedSetupFlags)
        }
    }

    /// Returns Linux-compatible raw setup bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Returns whether all flags in `other` are present.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Adapter-proven feature bits carried by a resolved layout.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FeatureFlags(u32);

impl FeatureFlags {
    /// No feature has yet been proved by the embedding adapter.
    pub const EMPTY: Self = Self(0);
    /// SQ and CQ offsets name the same backing mapping.
    pub const SINGLE_MMAP: Self = Self(1 << 0);
    /// Every accepted one-shot request owns terminal CQ capacity until reaped.
    pub const NODROP: Self = Self(1 << 1);
    /// The adapter consumes no SQE field after its stable private copy.
    pub const SUBMIT_STABLE: Self = Self(1 << 2);
    /// One-shot poll consumes the complete 32-bit `poll32_events` field.
    pub const POLL_32BITS: Self = Self(1 << 6);

    /// Complete feature vocabulary supported by the initial core contract.
    ///
    /// This is an upper bound, not an advertisement. An embedding adapter must
    /// pass the exact subset it has proved to [`SetupRequest::resolve`].
    pub const SUPPORTED: Self =
        Self(Self::SINGLE_MMAP.0 | Self::NODROP.0 | Self::SUBMIT_STABLE.0 | Self::POLL_32BITS.0);

    /// Strictly decodes only features implemented by this core contract.
    pub const fn from_bits(bits: u32) -> Result<Self, IoUringError> {
        if bits & !Self::SUPPORTED.0 == 0 {
            Ok(Self(bits))
        } else {
            Err(IoUringError::UnsupportedFeatureFlags)
        }
    }

    /// Returns Linux-compatible raw feature bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Returns whether all feature bits in `other` are present.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Combines independently proved initial features.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Raw setup inputs after syscall copyin but before ring allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetupRequest {
    entries: u32,
    cq_entries: u32,
    flags: SetupFlags,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
}

impl SetupRequest {
    /// Builds a request from the fields used by the supported initial profile.
    pub const fn new(entries: u32, cq_entries: u32, flags: SetupFlags) -> Self {
        Self {
            entries,
            cq_entries,
            flags,
            sq_thread_cpu: 0,
            sq_thread_idle: 0,
        }
    }

    /// Strictly decodes a copied Linux `io_uring_params` input.
    ///
    /// Polling and affinity fields require their corresponding setup flags.
    /// Attached workqueues are unsupported; their descriptor and Linux's three
    /// reserved words must be zero before any allocation.
    #[allow(clippy::too_many_arguments)]
    pub const fn from_raw(
        entries: u32,
        cq_entries: u32,
        flags: u32,
        sq_thread_cpu: u32,
        sq_thread_idle: u32,
        wq_fd: u32,
        reserved: [u32; 3],
    ) -> Result<Self, IoUringError> {
        let flags = match SetupFlags::from_bits(flags) {
            Ok(flags) => flags,
            Err(error) => return Err(error),
        };
        if (!flags.contains(SetupFlags::SQPOLL) && (sq_thread_cpu != 0 || sq_thread_idle != 0))
            || (flags.contains(SetupFlags::SQ_AFF) && !flags.contains(SetupFlags::SQPOLL))
            || (flags.contains(SetupFlags::DEFER_TASKRUN)
                && !flags.contains(SetupFlags::SINGLE_ISSUER))
            || (!flags.contains(SetupFlags::SQ_AFF) && sq_thread_cpu != 0)
            || wq_fd != 0
            || reserved[0] != 0
            || reserved[1] != 0
            || reserved[2] != 0
        {
            return Err(IoUringError::ReservedFieldNonZero);
        }
        Ok(Self {
            entries,
            cq_entries,
            flags,
            sq_thread_cpu,
            sq_thread_idle,
        })
    }

    /// Requested SQ entries before clamp and power-of-two rounding.
    pub const fn entries(self) -> u32 {
        self.entries
    }

    /// Requested CQ entries. This is ignored unless `CQSIZE` is set.
    pub const fn cq_entries(self) -> u32 {
        self.cq_entries
    }

    /// Strictly decoded setup flags.
    pub const fn flags(self) -> SetupFlags {
        self.flags
    }

    /// Requested SQ polling CPU. It is meaningful only with `SQPOLL` and is
    /// retained through layout resolution so the kernel worker lifecycle can
    /// validate and bind the requested target instead of losing it at setup.
    pub const fn sq_thread_cpu(self) -> u32 {
        self.sq_thread_cpu
    }

    /// SQ polling idle interval in milliseconds, retained verbatim for the
    /// ring-local worker rather than being silently discarded at setup.
    pub const fn sq_thread_idle(self) -> u32 {
        self.sq_thread_idle
    }

    /// Validates setup geometry and produces exact shared-memory offsets.
    ///
    /// `proven_features` is the exact subset whose adapter-owned mmap,
    /// publication, stable-copy, and readiness premises the caller has proved.
    /// The policy core validates the vocabulary but never advertises an
    /// adapter-dependent feature implicitly.
    pub fn resolve(self, proven_features: FeatureFlags) -> Result<RingLayout, IoUringError> {
        let sq_entries = resolve_sq_entries(self.entries, self.flags)?;
        let cq_entries = resolve_cq_entries(sq_entries, self.cq_entries, self.flags)?;

        let cqe_end = RING_HEADER_BYTES
            .checked_add(
                cq_entries
                    .checked_mul(CQE_BYTES)
                    .ok_or(IoUringError::Overflow)?,
            )
            .ok_or(IoUringError::Overflow)?;
        let sq_array = if self.flags.contains(SetupFlags::NO_SQARRAY) {
            None
        } else {
            Some(align_up(cqe_end, RING_ALIGNMENT)?)
        };
        let ring_bytes = match sq_array {
            Some(offset) => offset
                .checked_add(
                    sq_entries
                        .checked_mul(size_of_u32())
                        .ok_or(IoUringError::Overflow)?,
                )
                .ok_or(IoUringError::Overflow)?,
            None => cqe_end,
        };
        let sqe_bytes = sq_entries
            .checked_mul(SQE_BYTES)
            .ok_or(IoUringError::Overflow)?;

        Ok(RingLayout {
            sq_entries,
            cq_entries,
            setup_flags: self.flags,
            sq_thread_cpu: self.sq_thread_cpu,
            sq_thread_idle: self.sq_thread_idle,
            features: proven_features,
            sq_offsets: SubmissionQueueOffsets {
                head: 0,
                tail: 4,
                ring_mask: 16,
                ring_entries: 24,
                flags: 36,
                dropped: 32,
                array: sq_array,
            },
            cq_offsets: CompletionQueueOffsets {
                head: 8,
                tail: 12,
                ring_mask: 20,
                ring_entries: 28,
                overflow: 44,
                cqes: RING_HEADER_BYTES,
                flags: 40,
            },
            ring_bytes,
            sqe_bytes,
        })
    }
}

/// Published submission-ring byte offsets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubmissionQueueOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: Option<u32>,
}

impl SubmissionQueueOffsets {
    /// Kernel-owned SQ head offset.
    pub const fn head(self) -> u32 {
        self.head
    }
    /// Userspace-owned SQ tail offset.
    pub const fn tail(self) -> u32 {
        self.tail
    }
    /// Constant SQ mask offset.
    pub const fn ring_mask(self) -> u32 {
        self.ring_mask
    }
    /// Constant SQ entry count offset.
    pub const fn ring_entries(self) -> u32 {
        self.ring_entries
    }
    /// Kernel-owned runtime SQ flags offset.
    pub const fn flags(self) -> u32 {
        self.flags
    }
    /// Kernel-owned invalid-index counter offset.
    pub const fn dropped(self) -> u32 {
        self.dropped
    }
    /// Optional SQ index-array offset. `None` means `NO_SQARRAY`.
    pub const fn array(self) -> Option<u32> {
        self.array
    }
}

/// Published completion-ring byte offsets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompletionQueueOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
}

impl CompletionQueueOffsets {
    /// Userspace-owned CQ head offset.
    pub const fn head(self) -> u32 {
        self.head
    }
    /// Kernel-owned CQ tail offset.
    pub const fn tail(self) -> u32 {
        self.tail
    }
    /// Constant CQ mask offset.
    pub const fn ring_mask(self) -> u32 {
        self.ring_mask
    }
    /// Constant CQ entry count offset.
    pub const fn ring_entries(self) -> u32 {
        self.ring_entries
    }
    /// Kernel-owned overflow counter offset.
    pub const fn overflow(self) -> u32 {
        self.overflow
    }
    /// First CQE byte offset.
    pub const fn cqes(self) -> u32 {
        self.cqes
    }
    /// Userspace-owned CQ runtime flags offset.
    pub const fn flags(self) -> u32 {
        self.flags
    }
}

/// One mmap-visible backing region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingRegion {
    /// The single shared SQ/CQ header, CQEs, and optional SQ array.
    Rings,
    /// The separately mapped array of 64-byte SQEs.
    SubmissionEntries,
}

/// Fully resolved, Linux-compatible initial ring geometry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingLayout {
    sq_entries: u32,
    cq_entries: u32,
    setup_flags: SetupFlags,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: FeatureFlags,
    sq_offsets: SubmissionQueueOffsets,
    cq_offsets: CompletionQueueOffsets,
    ring_bytes: u32,
    sqe_bytes: u32,
}

impl RingLayout {
    /// Actual power-of-two submission entries.
    pub const fn sq_entries(self) -> u32 {
        self.sq_entries
    }
    /// Actual power-of-two completion entries.
    pub const fn cq_entries(self) -> u32 {
        self.cq_entries
    }
    /// Actual SQ ring mask.
    pub const fn sq_mask(self) -> u32 {
        self.sq_entries - 1
    }
    /// Actual CQ ring mask.
    pub const fn cq_mask(self) -> u32 {
        self.cq_entries - 1
    }
    /// Setup flags preserved in returned params.
    pub const fn setup_flags(self) -> SetupFlags {
        self.setup_flags
    }
    /// Requested SQPOLL target CPU.
    pub const fn sq_thread_cpu(self) -> u32 {
        self.sq_thread_cpu
    }
    /// Requested SQPOLL idle interval in milliseconds.
    pub const fn sq_thread_idle(self) -> u32 {
        self.sq_thread_idle
    }
    /// Features proved by this geometry and lifecycle contract.
    pub const fn features(self) -> FeatureFlags {
        self.features
    }
    /// Published SQ offsets.
    pub const fn sq_offsets(self) -> SubmissionQueueOffsets {
        self.sq_offsets
    }
    /// Published CQ offsets.
    pub const fn cq_offsets(self) -> CompletionQueueOffsets {
        self.cq_offsets
    }
    /// Exact bytes needed by the shared ring backing before page rounding.
    pub const fn ring_bytes(self) -> u32 {
        self.ring_bytes
    }
    /// Exact bytes needed by the SQE backing before page rounding.
    pub const fn sqe_bytes(self) -> u32 {
        self.sqe_bytes
    }

    /// Resolves one Linux io_uring mmap offset to its backing region.
    pub const fn mapping_region(self, offset: u64) -> Result<MappingRegion, IoUringError> {
        match offset {
            IORING_OFF_SQ_RING | IORING_OFF_CQ_RING => Ok(MappingRegion::Rings),
            IORING_OFF_SQES => Ok(MappingRegion::SubmissionEntries),
            _ => Err(IoUringError::InvalidMappingOffset),
        }
    }

    /// Returns exact unrounded bytes for one mmap region.
    pub const fn mapping_bytes(self, region: MappingRegion) -> u32 {
        match region {
            MappingRegion::Rings => self.ring_bytes,
            MappingRegion::SubmissionEntries => self.sqe_bytes,
        }
    }

    /// Validates a copied SQ head/tail pair and returns pending submissions.
    pub const fn pending_submissions(self, head: u32, tail: u32) -> Result<u32, IoUringError> {
        let pending = tail.wrapping_sub(head);
        if pending <= self.sq_entries {
            Ok(pending)
        } else {
            Err(IoUringError::InvalidQueueGeometry)
        }
    }

    /// Maps a monotonic SQ counter to its shared ring slot.
    pub const fn submission_slot(self, counter: u32) -> u32 {
        counter & self.sq_mask()
    }

    /// Validates an SQ-array value before using it as an SQE index.
    pub const fn validate_sqe_index(self, index: u32) -> Result<u32, IoUringError> {
        if index < self.sq_entries {
            Ok(index)
        } else {
            Err(IoUringError::InvalidSubmission)
        }
    }
}

fn resolve_sq_entries(entries: u32, flags: SetupFlags) -> Result<u32, IoUringError> {
    if entries == 0 {
        return Err(IoUringError::ZeroEntries);
    }
    let entries = if entries > IORING_MAX_ENTRIES {
        if flags.contains(SetupFlags::CLAMP) {
            IORING_MAX_ENTRIES
        } else {
            return Err(IoUringError::SubmissionEntriesTooLarge);
        }
    } else {
        entries
    };
    entries
        .checked_next_power_of_two()
        .ok_or(IoUringError::Overflow)
}

fn resolve_cq_entries(
    sq_entries: u32,
    requested: u32,
    flags: SetupFlags,
) -> Result<u32, IoUringError> {
    if !flags.contains(SetupFlags::CQSIZE) {
        return sq_entries.checked_mul(2).ok_or(IoUringError::Overflow);
    }
    if requested == 0 {
        return Err(IoUringError::ZeroCompletionEntries);
    }
    let requested = if requested > IORING_MAX_CQ_ENTRIES {
        if flags.contains(SetupFlags::CLAMP) {
            IORING_MAX_CQ_ENTRIES
        } else {
            return Err(IoUringError::CompletionEntriesTooLarge);
        }
    } else {
        requested
    };
    let entries = requested
        .checked_next_power_of_two()
        .ok_or(IoUringError::Overflow)?;
    if entries < sq_entries {
        return Err(IoUringError::CompletionEntriesTooSmall);
    }
    Ok(entries)
}

const fn align_up(value: u32, alignment: u32) -> Result<u32, IoUringError> {
    let mask = alignment - 1;
    match value.checked_add(mask) {
        Some(value) => Ok(value & !mask),
        None => Err(IoUringError::Overflow),
    }
}

const fn size_of_u32() -> u32 {
    4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_advertises_exactly_the_adapter_proved_features() {
        let partial = FeatureFlags::SINGLE_MMAP.union(FeatureFlags::SUBMIT_STABLE);
        let layout = SetupRequest::new(3, 0, SetupFlags::default())
            .resolve(partial)
            .unwrap();
        assert_eq!(layout.sq_entries(), 4);
        assert_eq!(layout.cq_entries(), 8);
        assert_eq!(layout.features(), partial);
        assert!(layout.features().contains(FeatureFlags::SINGLE_MMAP));
        assert!(layout.features().contains(FeatureFlags::SUBMIT_STABLE));
        assert!(!layout.features().contains(FeatureFlags::NODROP));
        assert!(!layout.features().contains(FeatureFlags::POLL_32BITS));
        assert_eq!(layout.features().bits(), 0x5);

        let empty = SetupRequest::new(3, 0, SetupFlags::default())
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        assert_eq!(empty.features(), FeatureFlags::EMPTY);

        let complete = SetupRequest::new(3, 0, SetupFlags::default())
            .resolve(FeatureFlags::SUPPORTED)
            .unwrap();
        assert_eq!(complete.features(), FeatureFlags::SUPPORTED);
        assert_eq!(complete.features().bits(), 0x47);
    }

    #[test]
    fn setup_clamp_cqsize_and_no_sqarray_are_checked() {
        assert_eq!(
            SetupFlags::from_bits(SetupFlags::CQSIZE.bits()),
            Ok(SetupFlags::CQSIZE)
        );
        assert_eq!(
            SetupFlags::from_bits(SetupFlags::CLAMP.bits()),
            Ok(SetupFlags::CLAMP)
        );
        assert_eq!(
            SetupFlags::from_bits(SetupFlags::NO_SQARRAY.bits()),
            Ok(SetupFlags::NO_SQARRAY)
        );
        let flags = SetupFlags::from_bits(
            SetupFlags::CLAMP.bits() | SetupFlags::CQSIZE.bits() | SetupFlags::NO_SQARRAY.bits(),
        )
        .unwrap();
        let layout = SetupRequest::new(u32::MAX, u32::MAX, flags)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        assert_eq!(layout.sq_entries(), IORING_MAX_ENTRIES);
        assert_eq!(layout.cq_entries(), IORING_MAX_CQ_ENTRIES);
        assert_eq!(layout.sq_offsets().array(), None);
        assert_eq!(
            layout.mapping_region(IORING_OFF_CQ_RING),
            Ok(MappingRegion::Rings)
        );
        assert_eq!(
            layout.mapping_region(IORING_OFF_SQES),
            Ok(MappingRegion::SubmissionEntries)
        );
    }

    #[test]
    fn queue_counters_and_sq_array_indexes_reject_forged_progress() {
        let layout = SetupRequest::new(4, 0, SetupFlags::default())
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        assert_eq!(layout.pending_submissions(10, 14), Ok(4));
        assert_eq!(
            layout.pending_submissions(10, 15),
            Err(IoUringError::InvalidQueueGeometry)
        );
        assert_eq!(layout.validate_sqe_index(3), Ok(3));
        assert_eq!(
            layout.validate_sqe_index(4),
            Err(IoUringError::InvalidSubmission)
        );
    }

    #[test]
    fn reserved_setup_fields_and_unknown_features_are_rejected() {
        assert_eq!(
            SetupRequest::from_raw(1, 0, 0, 0, 0, 0, [0, 1, 0]),
            Err(IoUringError::ReservedFieldNonZero)
        );
        assert_eq!(
            FeatureFlags::from_bits(1 << 3),
            Err(IoUringError::UnsupportedFeatureFlags)
        );
        assert_eq!(
            SetupFlags::from_bits(SetupFlags::SQ_AFF.bits()),
            Ok(SetupFlags::SQ_AFF)
        );
        assert_eq!(
            SetupRequest::from_raw(1, 0, SetupFlags::SQ_AFF.bits(), 0, 0, 0, [0; 3]),
            Err(IoUringError::ReservedFieldNonZero)
        );
        let polling = SetupRequest::from_raw(
            1,
            0,
            SetupFlags::SQ_AFF.bits() | SetupFlags::SQPOLL.bits(),
            2,
            100,
            0,
            [0; 3],
        )
        .unwrap();
        assert_eq!(polling.sq_thread_cpu(), 2);
        assert_eq!(polling.sq_thread_idle(), 100);
        assert_eq!(
            SetupFlags::from_bits(1 << 7),
            Err(IoUringError::UnsupportedSetupFlags)
        );
    }
}
