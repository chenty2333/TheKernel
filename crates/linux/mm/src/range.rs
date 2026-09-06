use core::ops::Range;

use crate::MmError;

/// Checked nonzero half-open userspace byte range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct UserRange {
    start: usize,
    end: usize,
}

impl UserRange {
    /// Builds `[start, start + length)`, rejecting zero and overflow.
    pub const fn new(start: usize, length: usize) -> Result<Self, MmError> {
        if length == 0 {
            return Err(MmError::ZeroLength);
        }
        match start.checked_add(length) {
            Some(end) => Ok(Self { start, end }),
            None => Err(MmError::Overflow),
        }
    }

    /// Builds a range and validates it against an exclusive userspace limit.
    pub const fn new_bounded(
        start: usize,
        length: usize,
        user_end: usize,
    ) -> Result<Self, MmError> {
        let range = match Self::new(start, length) {
            Ok(range) => range,
            Err(error) => return Err(error),
        };
        range.within(user_end)
    }

    /// Builds a nonempty half-open range from explicit bounds.
    pub const fn from_bounds(start: usize, end: usize) -> Result<Self, MmError> {
        match end.checked_sub(start) {
            Some(0) => Err(MmError::ZeroLength),
            Some(_) => Ok(Self { start, end }),
            None => Err(MmError::Overflow),
        }
    }

    /// First included byte address.
    pub const fn start(self) -> usize {
        self.start
    }

    /// First excluded byte address.
    pub const fn end(self) -> usize {
        self.end
    }

    /// Number of bytes in the range.
    pub const fn len(self) -> usize {
        self.end - self.start
    }

    /// Always false because construction rejects empty ranges.
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// Returns a standard half-open range for consumer iteration.
    pub const fn as_range(self) -> Range<usize> {
        self.start..self.end
    }

    /// Whether this range fully contains `other`.
    pub const fn contains(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    /// Whether two half-open ranges intersect.
    pub const fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// Whether one byte address belongs to the range.
    pub const fn contains_address(self, address: usize) -> bool {
        self.start <= address && address < self.end
    }

    /// Validates that the entire range lies below an exclusive userspace end.
    pub const fn within(self, user_end: usize) -> Result<Self, MmError> {
        if self.end > user_end {
            return Err(MmError::AddressOutOfRange);
        }
        Ok(self)
    }
}

/// Checked power-of-two page size, independent of any HAL constant.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PageSize(usize);

impl PageSize {
    /// Validates a nonzero power-of-two page size.
    pub const fn new(bytes: usize) -> Result<Self, MmError> {
        if bytes == 0 {
            return Err(MmError::ZeroLength);
        }
        if !bytes.is_power_of_two() {
            return Err(MmError::InvalidPageSize);
        }
        Ok(Self(bytes))
    }

    /// Page size in bytes.
    pub const fn bytes(self) -> usize {
        self.0
    }

    /// Whether an address or length is page aligned.
    pub const fn is_aligned(self, value: usize) -> bool {
        value & (self.0 - 1) == 0
    }

    pub(crate) const fn align_down(self, value: usize) -> usize {
        value & !(self.0 - 1)
    }

    pub(crate) const fn align_up(self, value: usize) -> Result<usize, MmError> {
        let mask = self.0 - 1;
        match value.checked_add(mask) {
            Some(value) => Ok(value & !mask),
            None => Err(MmError::Overflow),
        }
    }
}

/// Checked nonzero page-aligned byte range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PageRange {
    range: UserRange,
    page_size: PageSize,
}

impl PageRange {
    /// Builds an exactly page-aligned range from raw byte values.
    pub const fn new(start: usize, length: usize, page_size: usize) -> Result<Self, MmError> {
        let page_size = match PageSize::new(page_size) {
            Ok(page_size) => page_size,
            Err(error) => return Err(error),
        };
        Self::with_page_size(start, length, page_size)
    }

    /// Builds an exactly page-aligned range using a validated page size.
    pub const fn with_page_size(
        start: usize,
        length: usize,
        page_size: PageSize,
    ) -> Result<Self, MmError> {
        if !page_size.is_aligned(start) || !page_size.is_aligned(length) {
            return Err(MmError::Unaligned);
        }
        let range = match UserRange::new(start, length) {
            Ok(range) => range,
            Err(error) => return Err(error),
        };
        Ok(Self { range, page_size })
    }

    /// Expands a byte range to the smallest page-aligned covering range.
    pub const fn covering(range: UserRange, page_size: PageSize) -> Result<Self, MmError> {
        let start = page_size.align_down(range.start());
        let end = match page_size.align_up(range.end()) {
            Ok(end) => end,
            Err(error) => return Err(error),
        };
        Self::with_page_size(start, end - start, page_size)
    }

    /// Underlying byte range.
    pub const fn user_range(self) -> UserRange {
        self.range
    }

    /// First included address.
    pub const fn start(self) -> usize {
        self.range.start()
    }

    /// First excluded address.
    pub const fn end(self) -> usize {
        self.range.end()
    }

    /// Byte length.
    pub const fn len(self) -> usize {
        self.range.len()
    }

    /// Always false because construction rejects empty ranges.
    pub const fn is_empty(self) -> bool {
        self.range.is_empty()
    }

    /// Page size used by the range.
    pub const fn page_size(self) -> PageSize {
        self.page_size
    }

    /// Number of pages in the range.
    pub const fn page_count(self) -> usize {
        self.len() / self.page_size.bytes()
    }

    /// Whether this range fully contains `other` with the same page size.
    pub const fn contains(self, other: Self) -> bool {
        self.page_size.bytes() == other.page_size.bytes() && self.range.contains(other.range)
    }

    /// Whether two ranges with the same page size intersect.
    pub const fn overlaps(self, other: Self) -> bool {
        self.page_size.bytes() == other.page_size.bytes() && self.range.overlaps(other.range)
    }

    /// Returns an aligned subrange, relative to this range's first byte.
    pub const fn subrange(self, offset: usize, length: usize) -> Result<Self, MmError> {
        if !self.page_size.is_aligned(offset) || !self.page_size.is_aligned(length) {
            return Err(MmError::Unaligned);
        }
        let start = match self.start().checked_add(offset) {
            Some(start) => start,
            None => return Err(MmError::Overflow),
        };
        let subrange = match Self::with_page_size(start, length, self.page_size) {
            Ok(range) => range,
            Err(error) => return Err(error),
        };
        if !self.contains(subrange) {
            return Err(MmError::RangeNotMapped);
        }
        Ok(subrange)
    }
}
