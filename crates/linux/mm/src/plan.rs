use crate::{MmError, PageRange, PageSize, UserRange};

/// Result of translating an affine virtual origin during remap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AffineRelocation {
    origin: usize,
    backing_advance: usize,
}

impl AffineRelocation {
    /// New representable virtual origin.
    pub const fn origin(self) -> usize {
        self.origin
    }

    /// Backing prefix consumed when a negative virtual origin is unrepresentable.
    pub const fn backing_advance(self) -> usize {
        self.backing_advance
    }
}

/// Relocates one affine origin while preserving its backing cursor.
///
/// If translating an origin below `old_start` would underflow at `new_start`,
/// the origin is canonically rebased to `new_start` and the unrepresentable
/// prefix is returned as `backing_advance`.
pub const fn relocate_affine_origin(
    origin: usize,
    old_start: usize,
    new_start: usize,
) -> Result<AffineRelocation, MmError> {
    if origin >= old_start {
        let leading_gap = origin - old_start;
        return match new_start.checked_add(leading_gap) {
            Some(origin) => Ok(AffineRelocation {
                origin,
                backing_advance: 0,
            }),
            None => Err(MmError::Overflow),
        };
    }

    let consumed_prefix = old_start - origin;
    match new_start.checked_sub(consumed_prefix) {
        Some(origin) => Ok(AffineRelocation {
            origin,
            backing_advance: 0,
        }),
        None => Ok(AffineRelocation {
            origin: new_start,
            backing_advance: consumed_prefix,
        }),
    }
}

/// Page-covering arithmetic for an arbitrary nonempty userspace byte range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageCoveringPlan {
    requested: UserRange,
    pages: PageRange,
    leading_bytes: usize,
    trailing_bytes: usize,
}

impl PageCoveringPlan {
    /// Computes the smallest covering page range.
    pub fn new(requested: UserRange, page_size: usize) -> Result<Self, MmError> {
        let page_size = PageSize::new(page_size)?;
        let pages = PageRange::covering(requested, page_size)?;
        Ok(Self {
            requested,
            pages,
            leading_bytes: requested.start() - pages.start(),
            trailing_bytes: pages.end() - requested.end(),
        })
    }

    /// Original byte range.
    pub const fn requested(self) -> UserRange {
        self.requested
    }

    /// Covering page range.
    pub const fn pages(self) -> PageRange {
        self.pages
    }

    /// Bytes before the requested start in the first page.
    pub const fn leading_bytes(self) -> usize {
        self.leading_bytes
    }

    /// Bytes after the requested end in the final page.
    pub const fn trailing_bytes(self) -> usize {
        self.trailing_bytes
    }
}

/// Whole-operation geometry for one page-aligned remap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemapGeometry {
    old_range: PageRange,
    new_range: PageRange,
    moved_source: PageRange,
}

impl RemapGeometry {
    /// Builds remap geometry. `new_start` and `new_length` must be page aligned.
    pub fn new(old_range: PageRange, new_start: usize, new_length: usize) -> Result<Self, MmError> {
        let page_size = old_range.page_size();
        let new_range = PageRange::with_page_size(new_start, new_length, page_size)?;
        let moved_length = old_range.len().min(new_range.len());
        let moved_source = PageRange::with_page_size(old_range.start(), moved_length, page_size)?;
        Ok(Self {
            old_range,
            new_range,
            moved_source,
        })
    }

    /// Original mapping range.
    pub const fn old_range(self) -> PageRange {
        self.old_range
    }

    /// Destination range, including any growth tail.
    pub const fn new_range(self) -> PageRange {
        self.new_range
    }

    /// Source prefix whose existing backing/pages are relocated.
    pub const fn moved_source(self) -> PageRange {
        self.moved_source
    }

    /// Whether the remap grows beyond the source length.
    pub const fn grows(self) -> bool {
        self.new_range.len() > self.old_range.len()
    }

    /// Whether the remap truncates the source tail.
    pub const fn shrinks(self) -> bool {
        self.new_range.len() < self.old_range.len()
    }

    /// Growth-only destination tail, if any.
    pub fn growth_tail(self) -> Result<Option<PageRange>, MmError> {
        if !self.grows() {
            return Ok(None);
        }
        Ok(Some(PageRange::with_page_size(
            self.new_range
                .start()
                .checked_add(self.old_range.len())
                .ok_or(MmError::Overflow)?,
            self.new_range.len() - self.old_range.len(),
            self.new_range.page_size(),
        )?))
    }

    /// Plans one source fragment while keeping a single whole-remap backend pair.
    pub fn segment(self, source_segment: PageRange) -> Result<RemapSegmentGeometry, MmError> {
        if !self.moved_source.contains(source_segment) {
            return Err(MmError::InvalidRemap);
        }
        let offset = source_segment
            .start()
            .checked_sub(self.old_range.start())
            .ok_or(MmError::InvalidRemap)?;
        let destination_start = self
            .new_range
            .start()
            .checked_add(offset)
            .ok_or(MmError::Overflow)?;
        let destination = PageRange::with_page_size(
            destination_start,
            source_segment.len(),
            self.new_range.page_size(),
        )?;
        if !self.new_range.contains(destination) {
            return Err(MmError::InvalidRemap);
        }
        Ok(RemapSegmentGeometry {
            source: source_segment,
            destination,
            backend_old_start: self.old_range.start(),
            backend_new_start: self.new_range.start(),
        })
    }
}

/// One remap fragment plus the shared backend relocation anchor pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemapSegmentGeometry {
    source: PageRange,
    destination: PageRange,
    backend_old_start: usize,
    backend_new_start: usize,
}

impl RemapSegmentGeometry {
    /// Source fragment.
    pub const fn source(self) -> PageRange {
        self.source
    }

    /// Destination fragment.
    pub const fn destination(self) -> PageRange {
        self.destination
    }

    /// Whole remap's old anchor passed to every backend fragment.
    pub const fn backend_old_start(self) -> usize {
        self.backend_old_start
    }

    /// Whole remap's new anchor passed to every backend fragment.
    pub const fn backend_new_start(self) -> usize {
        self.backend_new_start
    }
}

/// Caller-specific `RLIMIT_MEMLOCK` policy after capability selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemlockLimit {
    /// `CAP_IPC_LOCK` or equivalent makes the byte limit inapplicable.
    Unlimited,
    /// Finite byte limit.
    Limited(u64),
    /// Zero/disabled policy which reports permission denial.
    Disabled,
}

/// Checked incremental memlock accounting plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemlockPlan {
    requested_bytes: u64,
    already_locked_bytes: u64,
    additional_bytes: u64,
    total_locked_bytes: u64,
}

impl MemlockPlan {
    /// Plans incremental charging for a range.
    pub fn new(
        current_locked_bytes: u64,
        already_locked_bytes: u64,
        requested_bytes: u64,
        limit: MemlockLimit,
    ) -> Result<Self, MmError> {
        if requested_bytes == 0 {
            return Err(MmError::ZeroLength);
        }
        if already_locked_bytes > requested_bytes {
            return Err(MmError::InconsistentAccounting);
        }
        if limit == MemlockLimit::Disabled {
            return Err(MmError::MemlockDenied);
        }
        let additional_bytes = requested_bytes - already_locked_bytes;
        let total_locked_bytes = current_locked_bytes
            .checked_add(additional_bytes)
            .ok_or(MmError::Overflow)?;
        if let MemlockLimit::Limited(limit) = limit {
            if total_locked_bytes > limit {
                return Err(MmError::QuotaExceeded);
            }
        }
        Ok(Self {
            requested_bytes,
            already_locked_bytes,
            additional_bytes,
            total_locked_bytes,
        })
    }

    /// Convenience planner using a page range's byte size.
    pub fn for_range(
        current_locked_bytes: u64,
        already_locked_bytes: u64,
        range: PageRange,
        limit: MemlockLimit,
    ) -> Result<Self, MmError> {
        Self::new(
            current_locked_bytes,
            already_locked_bytes,
            u64::try_from(range.len()).map_err(|_| MmError::Overflow)?,
            limit,
        )
    }

    /// Requested covered bytes.
    pub const fn requested_bytes(self) -> u64 {
        self.requested_bytes
    }

    /// Bytes in the request already charged before the operation.
    pub const fn already_locked_bytes(self) -> u64 {
        self.already_locked_bytes
    }

    /// New charge needed by this operation.
    pub const fn additional_bytes(self) -> u64 {
        self.additional_bytes
    }

    /// Total locked bytes after successful publication.
    pub const fn total_locked_bytes(self) -> u64 {
        self.total_locked_bytes
    }
}
