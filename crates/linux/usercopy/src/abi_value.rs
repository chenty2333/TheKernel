//! Foreign Linux ABI types that may cross the user boundary by value.

/// A type for which every bit pattern is a valid value, so a completed
/// usercopy can be interpreted without `unsafe`.
///
/// This is `bytemuck::AnyBitPattern` for types this workspace does not own.
/// Types a kernel crate defines derive `bytemuck::AnyBitPattern` (or `Pod`)
/// and use [`VmPtr::vm_read`](crate::VmPtr::vm_read). The orphan rule keeps
/// that derive off `linux_raw_sys` types, so the audited ones are listed
/// below and read with [`VmPtr::vm_read_abi`](crate::VmPtr::vm_read_abi). A
/// type that is not listed does not compile there, rather than being assumed
/// valid at an `assume_init` call site.
///
/// Generic readers that may be handed either kind bound on this trait; a
/// workspace type that reaches one implements it next to its definition.
///
/// # Safety
///
/// Every field, recursively, must be an integer, an array of integers, a raw
/// pointer, or a union of those: no `bool`, `char`, enum, reference or
/// `NonNull`.
pub unsafe trait UserAbiValue: Copy + 'static {
    /// The all-zero value, which every such type has.
    fn abi_zeroed() -> Self {
        // SAFETY: every bit pattern, including all zeroes, is a valid value.
        unsafe { core::mem::zeroed() }
    }
}

/// A [`UserAbiValue`] with no padding bytes, so its complete object
/// representation is initialized and may be copied out to userspace.
///
/// This is `bytemuck::Pod` for types this workspace does not own; workspace
/// types derive `bytemuck::Pod`, which checks for padding at compile time, and
/// use [`VmMutPtr::vm_write`](crate::VmMutPtr::vm_write). The listed foreign
/// types use [`VmMutPtr::vm_write_abi`](crate::VmMutPtr::vm_write_abi), and
/// each entry carries a compile-time proof that its fields cover the object.
///
/// # Safety
///
/// The type must have no padding bytes, including tail padding.
pub unsafe trait UserAbiPod: UserAbiValue {}

/// The object representation of a [`UserAbiPod`] value, for copy-out paths
/// that assemble a byte image (the `bytemuck::bytes_of` equivalent).
pub fn abi_bytes<T: UserAbiPod>(value: &T) -> &[u8] {
    // SAFETY: `UserAbiPod` guarantees every one of the `size_of::<T>()` bytes
    // behind `value` is initialized, and the slice borrows `value`.
    unsafe { core::slice::from_raw_parts(core::ptr::from_ref(value).cast::<u8>(), size_of::<T>()) }
}

/// The mutable object representation of a [`UserAbiPod`] value, for filling
/// it from a byte source (the `bytemuck::bytes_of_mut` equivalent).
pub fn abi_bytes_mut<T: UserAbiPod>(value: &mut T) -> &mut [u8] {
    // SAFETY: `UserAbiPod` guarantees every byte behind `value` is initialized
    // and that every resulting bit pattern is a valid `T`, so arbitrary byte
    // writes through the slice keep `value` valid; the slice borrows `value`.
    unsafe {
        core::slice::from_raw_parts_mut(core::ptr::from_mut(value).cast::<u8>(), size_of::<T>())
    }
}

/// Decodes a [`UserAbiValue`] from the start of a byte buffer that may not be
/// aligned for `T`, such as a snapshot copied from userspace.
///
/// # Panics
///
/// Panics if `bytes` is shorter than `T`.
pub fn abi_read_unaligned<T: UserAbiValue>(bytes: &[u8]) -> T {
    assert!(bytes.len() >= size_of::<T>(), "short buffer for a UserAbiValue");
    // SAFETY: the assertion keeps the read inside `bytes`, whose bytes are all
    // initialized; `read_unaligned` has no alignment requirement, and every
    // bit pattern is a valid `T`.
    unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) }
}

#[doc(hidden)]
pub const fn field_size<T, F>(_field: fn(&T) -> &F) -> usize {
    core::mem::size_of::<F>()
}

macro_rules! user_abi_pods {
    ($($ty:ty { $($field:ident),+ $(,)? }),* $(,)?) => {
        $(
            // SAFETY: audited against the `linux_raw_sys` 0.12 x86_64
            // definition, whose fields are all integers, integer arrays or
            // records of those.
            unsafe impl UserAbiValue for $ty {}
            // SAFETY: the assertion below proves the listed fields cover the
            // whole object, so there is no padding.
            unsafe impl UserAbiPod for $ty {}
            const _: () = assert!(
                core::mem::size_of::<$ty>() == 0 $(+ field_size(|value: &$ty| &value.$field))+,
                "padding or an unlisted field in a UserAbiPod type",
            );
        )*
    };
}

// SAFETY: an array of valid-for-any-bits elements has no other bytes.
unsafe impl<T: UserAbiValue, const N: usize> UserAbiValue for [T; N] {}
// SAFETY: array elements are contiguous, so padding-free elements leave none.
unsafe impl<T: UserAbiPod, const N: usize> UserAbiPod for [T; N] {}
// SAFETY: a raw pointer is valid for every address bit pattern; reading one
// asserts nothing about what it points to.
unsafe impl<T: 'static> UserAbiValue for *const T {}
// SAFETY: as for `*const T`.
unsafe impl<T: 'static> UserAbiValue for *mut T {}

macro_rules! primitive_user_abi_pods {
    ($($ty:ty),* $(,)?) => {
        $(
            // SAFETY: every bit pattern of a primitive integer is a value.
            unsafe impl UserAbiValue for $ty {}
            // SAFETY: a primitive integer has no padding.
            unsafe impl UserAbiPod for $ty {}
        )*
    };
}

primitive_user_abi_pods!(u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize);

user_abi_pods!(
    linux_raw_sys::general::__kernel_fd_set { fds_bits },
    linux_raw_sys::general::__kernel_fsid_t { val },
    linux_raw_sys::general::__kernel_old_timeval { tv_sec, tv_usec },
    linux_raw_sys::general::__user_cap_data_struct { effective, permitted, inheritable },
    linux_raw_sys::general::__user_cap_header_struct { version, pid },
    linux_raw_sys::general::f_owner_ex { type_, pid },
    linux_raw_sys::general::futex_waitv { val, uaddr, flags, __reserved },
    linux_raw_sys::general::itimerspec { it_interval, it_value },
    linux_raw_sys::general::itimerval { it_interval, it_value },
    linux_raw_sys::general::open_how { flags, mode, resolve },
    linux_raw_sys::general::rlimit { rlim_cur, rlim_max },
    linux_raw_sys::general::rlimit64 { rlim_cur, rlim_max },
    linux_raw_sys::general::rusage {
        ru_utime, ru_stime, ru_maxrss, ru_ixrss, ru_idrss, ru_isrss, ru_minflt, ru_majflt,
        ru_nswap, ru_inblock, ru_oublock, ru_msgsnd, ru_msgrcv, ru_nsignals, ru_nvcsw, ru_nivcsw,
    },
    linux_raw_sys::general::stat {
        st_dev, st_ino, st_nlink, st_mode, st_uid, st_gid, __pad0, st_rdev, st_size, st_blksize,
        st_blocks, st_atime, st_atime_nsec, st_mtime, st_mtime_nsec, st_ctime, st_ctime_nsec,
        __unused,
    },
    linux_raw_sys::general::statfs {
        f_type, f_bsize, f_blocks, f_bfree, f_bavail, f_files, f_ffree, f_fsid, f_namelen,
        f_frsize, f_flags, f_spare,
    },
    linux_raw_sys::general::statx {
        stx_mask, stx_blksize, stx_attributes, stx_nlink, stx_uid, stx_gid, stx_mode, __spare0,
        stx_ino, stx_size, stx_blocks, stx_attributes_mask, stx_atime, stx_btime, stx_ctime,
        stx_mtime, stx_rdev_major, stx_rdev_minor, stx_dev_major, stx_dev_minor, stx_mnt_id,
        stx_dio_mem_align, stx_dio_offset_align, stx_subvol, stx_atomic_write_unit_min,
        stx_atomic_write_unit_max, stx_atomic_write_segments_max, stx_dio_read_offset_align,
        stx_atomic_write_unit_max_opt, __spare2, __spare3,
    },
    linux_raw_sys::general::statx_timestamp { tv_sec, tv_nsec, __reserved },
    linux_raw_sys::general::timespec { tv_sec, tv_nsec },
    linux_raw_sys::general::timeval { tv_sec, tv_usec },
    linux_raw_sys::general::timezone { tz_minuteswest, tz_dsttime },
    linux_raw_sys::net::cmsghdr { cmsg_len, cmsg_level, cmsg_type },
    linux_raw_sys::net::in_addr { s_addr },
    // `in6_u` is a union whose three members are all 16 bytes of integers, so
    // writing any one of them initializes the whole union.
    linux_raw_sys::net::in6_addr { in6_u },
    linux_raw_sys::net::sockaddr_in { sin_family, sin_port, sin_addr, __pad },
    linux_raw_sys::net::sockaddr_in6 {
        sin6_family, sin6_port, sin6_flowinfo, sin6_addr, sin6_scope_id,
    },
    linux_raw_sys::net::ucred { pid, uid, gid },
    linux_raw_sys::system::new_utsname { sysname, nodename, release, version, machine, domainname },
);

// SAFETY: audited as above. `epoll_event` is `repr(C, packed)` on x86_64, so
// it has no padding by construction (its packed fields cannot be borrowed for
// the field-size proof, so the size is pinned instead).
unsafe impl UserAbiValue for linux_raw_sys::general::epoll_event {}
// SAFETY: packed, as above.
unsafe impl UserAbiPod for linux_raw_sys::general::epoll_event {}
const _: () = assert!(core::mem::size_of::<linux_raw_sys::general::epoll_event>() == 12);

// SAFETY: audited as above; every field is an integer or a raw pointer. These
// have alignment holes, so they may be read but are deliberately not
// `UserAbiPod`.
unsafe impl UserAbiValue for linux_raw_sys::net::msghdr {}
// SAFETY: as above; integers with a hole after `l_whence` and tail padding.
unsafe impl UserAbiValue for linux_raw_sys::general::flock64 {}
