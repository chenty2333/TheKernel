//! `getcwd(2)`'s raw-syscall result rule.
//!
//! Linux's `SYSCALL_DEFINE2(getcwd, ...)` in `fs/d_path.c` renders the
//! current directory into a fixed `kmalloc(PATH_MAX)` scratch buffer
//! (`__getname()`), prepends the literal `"(unreachable)"` when
//! `prepend_path()` cannot reach the caller's `fs->root`, and then decides
//! between `-ENAMETOOLONG`, `-ERANGE`, `-EFAULT` and success purely from the
//! rendered length and the user-supplied size:
//!
//! ```text
//! 	len = PATH_MAX - b.len;
//! 	if (unlikely(len > PATH_MAX))
//! 		error = -ENAMETOOLONG;
//! 	else if (unlikely(len > size))
//! 		error = -ERANGE;
//! 	else if (copy_to_user(buf, b.buf, len))
//! 		error = -EFAULT;
//! 	else
//! 		error = len;
//! ```
//!
//! `len` counts the trailing NUL, so a successful `getcwd` reports
//! `strlen(path) + 1`. The two length verdicts are scalar rules with no
//! address-space dependency, so they live here and are host-tested.

/// `PATH_MAX` from `include/uapi/linux/limits.h`, which is also the size of
/// the kernel's fixed `getcwd` scratch buffer (`__getname()`).
pub const PATH_MAX: usize = 4096;

/// The literal Linux prepends when the current directory is not reachable
/// from the caller's root (a chroot or detached-mount walk in
/// `__prepend_path()` returns 1, 2 or 3).
pub const GETCWD_UNREACHABLE_PREFIX: &[u8] = b"(unreachable)";

/// Rejection of a rendered current-directory path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GetcwdError {
    /// `len > PATH_MAX`: the rendered path does not fit the kernel's fixed
    /// scratch buffer, so Linux reports `-ENAMETOOLONG` before it ever looks
    /// at the caller's size.
    NameTooLong,
    /// `len > size`: the rendered path fits the kernel but not the caller's
    /// buffer. A zero-sized buffer lands here, so `getcwd(buf, 0)` is
    /// `-ERANGE`; `getcwd(NULL, 0)` is `-ERANGE` too because Linux has no
    /// explicit NULL test before this point.
    Range,
}

/// Applies Linux's length rule to a rendered current directory.
///
/// `path_len` is the number of bytes the kernel would copy to userspace,
/// *including* the trailing NUL. The returned value is that same length, so
/// callers must transfer exactly `Ok(len)` bytes and return `Ok(len)` as the
/// syscall result.
///
/// The `-EFAULT` case is not modelled here: it is the outcome of the
/// `copy_to_user()` that follows a successful verdict, and it is the only
/// place Linux ever inspects the user pointer.
pub const fn getcwd_user_len(path_len: usize, size: usize) -> Result<usize, GetcwdError> {
    if path_len > PATH_MAX {
        Err(GetcwdError::NameTooLong)
    } else if path_len > size {
        Err(GetcwdError::Range)
    } else {
        Ok(path_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_verdict_precedes_the_user_pointer_test() {
        // `getcwd(NULL, 0)` and `getcwd(buf, 0)` are both -ERANGE on Linux:
        // the size comparison happens before copy_to_user, which is the only
        // place a NULL pointer is ever dereferenced.
        assert_eq!(getcwd_user_len(2, 0), Err(GetcwdError::Range));
        assert_eq!(getcwd_user_len(1, 0), Err(GetcwdError::Range));
        // Exactly fitting buffers succeed; one byte short is ERANGE.
        assert_eq!(getcwd_user_len(2, 2), Ok(2));
        assert_eq!(getcwd_user_len(2, 1), Err(GetcwdError::Range));
    }

    #[test]
    fn path_max_is_checked_before_the_caller_size() {
        assert_eq!(getcwd_user_len(PATH_MAX, usize::MAX), Ok(PATH_MAX));
        assert_eq!(
            getcwd_user_len(PATH_MAX + 1, usize::MAX),
            Err(GetcwdError::NameTooLong)
        );
        // A buffer larger than PATH_MAX cannot rescue an over-long path.
        assert_eq!(
            getcwd_user_len(PATH_MAX + 1, PATH_MAX + 1),
            Err(GetcwdError::NameTooLong)
        );
        // ...and the caller's size never upgrades ENAMETOOLONG to ERANGE.
        assert_eq!(getcwd_user_len(PATH_MAX + 1, 0), Err(GetcwdError::NameTooLong));
    }

    #[test]
    fn unreachable_prefix_lengths_are_representable() {
        // Linux renders "(unreachable)" with no separator; the leading '/'
        // comes from `prepend_name()`. The shortest unreachable result is
        // therefore "(unreachable)/" plus NUL.
        let shortest = GETCWD_UNREACHABLE_PREFIX.len() + 1 + 1;
        assert_eq!(shortest, 15);
        assert_eq!(getcwd_user_len(shortest, shortest), Ok(shortest));
    }
}
