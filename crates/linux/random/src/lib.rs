//! Pure Linux `getrandom(2)` argument, length and readiness policy.
//!
//! Everything here mirrors `SYSCALL_DEFINE3(getrandom, ...)` in
//! `drivers/char/random.c` plus the `import_ubuf()` clamp it inherits from
//! `lib/iov_iter.c`. The crate owns no entropy, copies no user memory and
//! never decides *how* to wait, so the whole flag table is host-testable.

#![no_std]
#![forbid(unsafe_code)]

/// Do not block waiting for the entropy pool to be seeded.
pub const GRND_NONBLOCK: u32 = 0x0001;
/// Accepted for ABI compatibility only. `include/uapi/linux/random.h`
/// documents this bit as "No effect" and `getrandom(2)` ignores it.
pub const GRND_RANDOM: u32 = 0x0002;
/// Return bytes even when the pool is not seeded, skipping the readiness gate.
pub const GRND_INSECURE: u32 = 0x0004;

/// `MAX_RW_COUNT` from `include/linux/fs.h`: `INT_MAX & PAGE_MASK`.
///
/// `import_ubuf()` silently clamps the request to this boundary instead of
/// rejecting it, so an oversized `getrandom(2)` request is truncated rather
/// than failing with `-EINVAL`.
pub const MAX_RW_COUNT: usize = (i32::MAX as usize) & !(4096 - 1);

/// A rejected `getrandom(2)` argument. Both variants are `-EINVAL`; they are
/// distinguished so the rejection reason is testable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RandomArgumentError {
    /// A bit outside `GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE` was set.
    UnknownFlag,
    /// `GRND_INSECURE` and `GRND_RANDOM` were requested together.
    InsecureAndRandom,
}

/// What Linux's readiness gate does before the buffer is touched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadinessPlan {
    /// `GRND_INSECURE` bypasses `crng_ready()` completely.
    Skip,
    /// Block in `wait_for_random_bytes()` until the pool is seeded.
    Block,
    /// Fail with `-EAGAIN` because `GRND_NONBLOCK` was given.
    FailWouldBlock,
}

/// One admitted `getrandom(2)` call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GetRandomRequest {
    flags: u32,
    len: usize,
}

impl GetRandomRequest {
    /// Applies Linux's flag validation and `import_ubuf()` length clamp.
    ///
    /// `drivers/char/random.c`:
    ///
    /// ```text
    /// if (flags & ~(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE))
    ///         return -EINVAL;
    /// if ((flags & (GRND_INSECURE | GRND_RANDOM)) == (GRND_INSECURE | GRND_RANDOM))
    ///         return -EINVAL;
    /// ```
    ///
    /// Flag validation is therefore complete before the readiness gate, before
    /// `import_ubuf()` and long before any userspace access.
    pub const fn new(flags: u32, len: usize) -> Result<Self, RandomArgumentError> {
        if flags & !(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE) != 0 {
            return Err(RandomArgumentError::UnknownFlag);
        }
        if flags & (GRND_INSECURE | GRND_RANDOM) == (GRND_INSECURE | GRND_RANDOM) {
            return Err(RandomArgumentError::InsecureAndRandom);
        }
        Ok(Self {
            flags,
            len: if len > MAX_RW_COUNT { MAX_RW_COUNT } else { len },
        })
    }

    /// The request length after the `MAX_RW_COUNT` clamp.
    pub const fn len(self) -> usize {
        self.len
    }

    /// Whether this request transfers nothing, which Linux answers with a
    /// successful zero-length read instead of touching the buffer.
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// The validated raw flag word.
    pub const fn flags(self) -> u32 {
        self.flags
    }

    /// Whether the pool readiness gate is skipped.
    pub const fn insecure(self) -> bool {
        self.flags & GRND_INSECURE != 0
    }

    /// Which branch of Linux's `crng_ready()` gate this call takes.
    ///
    /// ```text
    /// if (!crng_ready() && !(flags & GRND_INSECURE)) {
    ///         if (flags & GRND_NONBLOCK)
    ///                 return -EAGAIN;
    ///         ret = wait_for_random_bytes();
    ///         if (unlikely(ret))
    ///                 return ret;
    /// }
    /// ```
    ///
    /// The gate runs before `import_ubuf()`, so a zero-length blocking call
    /// still waits for the pool.
    pub const fn readiness(self) -> ReadinessPlan {
        if self.insecure() {
            ReadinessPlan::Skip
        } else if self.flags & GRND_NONBLOCK != 0 {
            ReadinessPlan::FailWouldBlock
        } else {
            ReadinessPlan::Block
        }
    }

    /// Whether the empty request still needs the pool before returning 0.
    ///
    /// Linux only reaches `get_random_bytes_user()`'s `!iov_iter_count()`
    /// early return after the readiness gate and after `import_ubuf()`.
    pub const fn gates_zero_length(self) -> bool {
        !self.insecure()
    }
}

/// Linux's `access_ok()` test as `import_ubuf()` reaches it
/// (`lib/iov_iter.c:1445-1453`).
///
/// `import_ubuf()` clamps the request to `MAX_RW_COUNT` and then validates the
/// **whole** clamped range with one `access_ok()` before a single byte is
/// copied:
///
/// ```text
/// int import_ubuf(int rw, void __user *buf, size_t len, struct iov_iter *i)
/// {
///         if (len > MAX_RW_COUNT)
///                 len = MAX_RW_COUNT;
///         if (unlikely(!access_ok(buf, len)))
///                 return -EFAULT;
///         ...
/// ```
///
/// `arch/x86/include/asm/uaccess_64.h:98-107` implements the runtime-size case
/// as `sum = size + (unsigned long)ptr; valid_user_address(sum) && sum >= ptr`
/// with `valid_user_address(x)` meaning `x <= USER_PTR_MAX`, so a range is
/// accepted exactly when it neither wraps nor ends above `user_ptr_max`.
///
/// A zero-length request reduces to `address <= user_ptr_max`, which is what
/// [`zero_length_address_is_user`] spells out.
pub const fn range_address_is_user(address: usize, len: usize, user_ptr_max: usize) -> bool {
    match address.checked_add(len) {
        Some(end) => end <= user_ptr_max,
        // `access_ok()`'s `sum >= ptr` re-test rejects a wrapped sum.
        None => false,
    }
}

/// Linux's `access_ok()` test for a **zero-length** request, as reached
/// through `import_ubuf()` (`lib/iov_iter.c`).
///
/// `arch/x86/include/asm/uaccess_64.h` defines
/// `valid_user_address(x)` as `x <= USER_PTR_MAX`, where `USER_PTR_MAX` is
/// `TASK_SIZE_MAX`. `access_ok()` adds the size and re-tests the sum, so a
/// zero-length request reduces to `address <= user_ptr_max` — the first page
/// (and therefore `NULL`) is a valid user address, while an address at or above
/// the user-space ceiling is `-EFAULT` even though nothing is copied.
pub const fn zero_length_address_is_user(address: usize, user_ptr_max: usize) -> bool {
    range_address_is_user(address, 0, user_ptr_max)
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_flag_bits_are_einval() {
        assert_eq!(
            GetRandomRequest::new(0x0008, 1),
            Err(RandomArgumentError::UnknownFlag)
        );
        assert_eq!(
            GetRandomRequest::new(0x8000_0000, 1),
            Err(RandomArgumentError::UnknownFlag)
        );
        // Every documented bit is accepted on its own.
        for flags in [0, GRND_NONBLOCK, GRND_RANDOM, GRND_INSECURE] {
            assert!(GetRandomRequest::new(flags, 1).is_ok(), "flags={flags:#x}");
        }
    }

    #[test]
    fn insecure_and_random_together_are_einval() {
        assert_eq!(
            GetRandomRequest::new(GRND_INSECURE | GRND_RANDOM, 1),
            Err(RandomArgumentError::InsecureAndRandom)
        );
        assert_eq!(
            GetRandomRequest::new(GRND_NONBLOCK | GRND_INSECURE | GRND_RANDOM, 1),
            Err(RandomArgumentError::InsecureAndRandom)
        );
        // The flag word is rejected before the length is even considered.
        assert_eq!(
            GetRandomRequest::new(GRND_INSECURE | GRND_RANDOM, usize::MAX),
            Err(RandomArgumentError::InsecureAndRandom)
        );
    }

    #[test]
    fn readiness_gate_is_the_linux_table() {
        let plan = |flags| GetRandomRequest::new(flags, 8).unwrap().readiness();
        assert_eq!(plan(0), ReadinessPlan::Block);
        assert_eq!(plan(GRND_RANDOM), ReadinessPlan::Block);
        assert_eq!(plan(GRND_NONBLOCK), ReadinessPlan::FailWouldBlock);
        assert_eq!(
            plan(GRND_NONBLOCK | GRND_RANDOM),
            ReadinessPlan::FailWouldBlock
        );
        assert_eq!(plan(GRND_INSECURE), ReadinessPlan::Skip);
        assert_eq!(
            plan(GRND_INSECURE | GRND_NONBLOCK),
            ReadinessPlan::Skip
        );
    }

    #[test]
    fn is_empty_matches_the_clamped_length() {
        let empty = GetRandomRequest::new(0, 0).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        let one = GetRandomRequest::new(0, 1).unwrap();
        assert!(!one.is_empty());
        // The clamp still leaves a non-empty request non-empty.
        let clamped = GetRandomRequest::new(0, usize::MAX).unwrap();
        assert_eq!(clamped.len(), MAX_RW_COUNT);
        assert!(!clamped.is_empty());
    }

    #[test]
    fn length_is_clamped_not_rejected() {
        for flags in [0, GRND_INSECURE, GRND_NONBLOCK] {
            assert_eq!(
                GetRandomRequest::new(flags, MAX_RW_COUNT + 1)
                    .unwrap()
                    .len(),
                MAX_RW_COUNT
            );
            assert_eq!(
                GetRandomRequest::new(flags, usize::MAX).unwrap().len(),
                MAX_RW_COUNT
            );
        }
        assert_eq!(MAX_RW_COUNT, 0x7fff_f000);
        assert_eq!(GetRandomRequest::new(0, 0).unwrap().len(), 0);
    }

    #[test]
    fn zero_length_still_obeys_the_readiness_gate() {
        assert!(GetRandomRequest::new(0, 0).unwrap().gates_zero_length());
        assert!(GetRandomRequest::new(GRND_NONBLOCK, 0).unwrap().gates_zero_length());
        assert!(!GetRandomRequest::new(GRND_INSECURE, 0).unwrap().gates_zero_length());
    }

    #[test]
    fn zero_length_access_ok_accepts_the_low_page_only() {
        const USER_PTR_MAX: usize = 0x0000_7fff_ffff_f000;
        assert!(zero_length_address_is_user(0, USER_PTR_MAX));
        assert!(zero_length_address_is_user(0x1000, USER_PTR_MAX));
        assert!(zero_length_address_is_user(USER_PTR_MAX, USER_PTR_MAX));
        assert!(!zero_length_address_is_user(USER_PTR_MAX + 1, USER_PTR_MAX));
        assert!(!zero_length_address_is_user(usize::MAX, USER_PTR_MAX));
    }

    #[test]
    fn whole_range_access_ok_uses_the_exclusive_end() {
        const USER_PTR_MAX: usize = 0x0000_7fff_ffff_f000;
        // The end is inclusive of `USER_PTR_MAX` itself, because the test is on
        // the sum: `sum <= USER_PTR_MAX`.
        assert!(range_address_is_user(0x1000, 0x1000, USER_PTR_MAX));
        assert!(range_address_is_user(USER_PTR_MAX - 0x1000, 0x1000, USER_PTR_MAX));
        assert!(range_address_is_user(USER_PTR_MAX, 0, USER_PTR_MAX));
        // One byte past the ceiling is rejected, and so is a range that starts
        // inside the user window and only crosses the ceiling at its end.
        assert!(!range_address_is_user(
            USER_PTR_MAX - 0x1000,
            0x1001,
            USER_PTR_MAX
        ));
        assert!(!range_address_is_user(USER_PTR_MAX, 1, USER_PTR_MAX));
        assert!(!range_address_is_user(0x7fff_ffff_f000, 16, USER_PTR_MAX));
        // `access_ok()`'s `sum >= ptr` re-test: a wrapping sum is rejected.
        assert!(!range_address_is_user(usize::MAX, 2, USER_PTR_MAX));
        assert!(!range_address_is_user(USER_PTR_MAX, usize::MAX, USER_PTR_MAX));
        // The empty request is the address test alone, first page included.
        for address in [0usize, 1, 0x1000, USER_PTR_MAX] {
            assert_eq!(
                range_address_is_user(address, 0, USER_PTR_MAX),
                zero_length_address_is_user(address, USER_PTR_MAX)
            );
        }
    }
}
