use crate::{MmError, PageSize};

/// Pure Linux `mincore(2)` request geometry after address-limit validation.
///
/// The caller supplies the architecture's inclusive `USER_PTR_MAX`.  This
/// deliberately models Linux's `access_ok(addr, len)` rather than mapping
/// presence: a zero-length request may name an unmapped address, but not one
/// above the user pointer limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MincorePlan {
    start: usize,
    page_count: usize,
    rounded_len: usize,
}

impl MincorePlan {
    /// Validates the Linux-visible input range and derives output geometry.
    ///
    /// Validation order is part of the ABI: alignment first, then the input
    /// `access_ok` range, then output-page arithmetic.  In particular, a
    /// zero-length request has zero output bytes and therefore requires no
    /// output-vector access, while its start address is still range checked.
    pub const fn new(
        start: usize,
        length: usize,
        page_size: usize,
        user_pointer_limit: usize,
    ) -> Result<Self, MmError> {
        let page_size = match PageSize::new(page_size) {
            Ok(page_size) => page_size,
            Err(error) => return Err(error),
        };
        if !page_size.is_aligned(start) {
            return Err(MmError::Unaligned);
        }

        let end = match start.checked_add(length) {
            Some(end) => end,
            None => return Err(MmError::AddressOutOfRange),
        };
        if start > user_pointer_limit || end > user_pointer_limit {
            return Err(MmError::AddressOutOfRange);
        }

        let page_count = length / page_size.bytes()
            + if length % page_size.bytes() == 0 {
                0
            } else {
                1
            };
        let rounded_len = match page_count.checked_mul(page_size.bytes()) {
            Some(length) => length,
            None => return Err(MmError::Overflow),
        };
        Ok(Self {
            start,
            page_count,
            rounded_len,
        })
    }

    /// First inspected virtual address.
    pub const fn start(self) -> usize {
        self.start
    }

    /// Number of one-byte residency values required in the output vector.
    pub const fn page_count(self) -> usize {
        self.page_count
    }

    /// Page-aligned number of bytes to inspect in the address space.
    pub const fn rounded_len(self) -> usize {
        self.rounded_len
    }

    /// Whether the request has no output and requires no VMA walk or copyout.
    pub const fn is_empty(self) -> bool {
        self.page_count == 0
    }
}
