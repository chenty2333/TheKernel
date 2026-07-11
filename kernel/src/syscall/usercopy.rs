//! Linux ABI records that must remain raw until after user-copy validation.

use core::mem;

use linux_raw_sys::general::sigevent;
use starry_vm::{VmPtr, VmResult};

#[repr(C)]
#[derive(Clone, Copy)]
union RawSigval {
    bits: usize,
    int: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawSigeventThread {
    function: usize,
    attribute: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
union RawSigeventUnion {
    pad: [i32; 12],
    tid: i32,
    thread: RawSigeventThread,
}

/// All-integer mirror of Linux `struct sigevent`.
///
/// `linux-raw-sys` models the `SIGEV_THREAD` callback as
/// `Option<extern "C" fn>`. Arbitrary userspace bytes are not a valid value of
/// that Rust type, even though they are valid bytes at the syscall boundary.
/// This mirror preserves the exact ABI layout without creating a function
/// pointer or reference before the notify mode has been validated.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct RawSigevent {
    value: RawSigval,
    signo: i32,
    notify: i32,
    data: RawSigeventUnion,
}

const _: [(); mem::size_of::<sigevent>()] = [(); mem::size_of::<RawSigevent>()];
const _: [(); mem::align_of::<sigevent>()] = [(); mem::align_of::<RawSigevent>()];
const _: [(); mem::offset_of!(sigevent, sigev_value)] = [(); mem::offset_of!(RawSigevent, value)];
const _: [(); mem::offset_of!(sigevent, sigev_signo)] = [(); mem::offset_of!(RawSigevent, signo)];
const _: [(); mem::offset_of!(sigevent, sigev_notify)] = [(); mem::offset_of!(RawSigevent, notify)];
const _: [(); mem::offset_of!(sigevent, _sigev_un)] = [(); mem::offset_of!(RawSigevent, data)];

impl RawSigevent {
    pub(crate) fn read_from_user(ptr: *const Self) -> VmResult<Self> {
        let value = ptr.vm_read_uninit()?;
        // SAFETY: VmIo initialized every byte before returning `Ok`. This
        // repr(C) value and both unions contain only integer scalars/arrays;
        // there are no bools, Rust enums, references, NonZero values, or
        // function pointers. Every initialized bit pattern is therefore valid.
        Ok(unsafe { value.assume_init() })
    }

    pub(crate) const fn notify(&self) -> i32 {
        self.notify
    }

    pub(crate) const fn signo(&self) -> i32 {
        self.signo
    }

    pub(crate) fn value_ptr_address(&self) -> usize {
        // SAFETY: every RawSigval bit pattern is valid as usize.
        unsafe { self.value.bits }
    }

    pub(crate) fn thread_id(&self) -> i32 {
        // SAFETY: every RawSigeventUnion bit pattern is valid as i32 storage.
        unsafe { self.data.tid }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_callback_bits_remain_integer_storage() {
        let event = RawSigevent {
            value: RawSigval { bits: usize::MAX },
            signo: 64,
            notify: 2,
            data: RawSigeventUnion {
                thread: RawSigeventThread {
                    function: usize::MAX,
                    attribute: usize::MAX - 1,
                },
            },
        };

        assert_eq!(event.value_ptr_address(), usize::MAX);
        assert_eq!(event.signo(), 64);
        assert_eq!(event.notify(), 2);
    }
}
