use core::mem;

use linux_raw_sys::general::sigevent;

use crate::{UserMemory, UserMemoryContext, VmPtr, VmResult};

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

/// All-integer mirror of Linux `struct sigevent` for safe syscall copy-in.
///
/// `linux-raw-sys` models the `SIGEV_THREAD` callback as
/// `Option<extern "C" fn>`. Arbitrary userspace bytes are not a valid value of
/// that Rust type, even though they are valid bytes at the syscall boundary.
/// This mirror preserves the exact ABI layout without creating a function
/// pointer or reference before the notify mode has been validated.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawSigevent {
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
    /// Copies one complete `struct sigevent` from the supplied userspace
    /// context without constructing function pointers from raw bytes.
    pub fn read_from_user<M: UserMemory + ?Sized>(
        memory: &mut UserMemoryContext<'_, M>,
        ptr: *const Self,
    ) -> VmResult<Self> {
        let value = ptr.vm_read_uninit(memory)?;
        // SAFETY: UserMemory initialized every byte before returning `Ok`.
        // This repr(C) value and both unions contain only integer
        // scalars/arrays; every initialized bit pattern is therefore valid.
        Ok(unsafe { value.assume_init() })
    }

    /// Returns the raw `sigev_notify` mode.
    pub const fn notify(&self) -> i32 {
        self.notify
    }

    /// Returns the raw `sigev_signo` value.
    pub const fn signo(&self) -> i32 {
        self.signo
    }

    /// Returns the raw `sigev_value.sival_ptr` bits as a userspace address.
    pub fn value_ptr_address(&self) -> usize {
        // SAFETY: every RawSigval bit pattern is valid as usize storage.
        unsafe { self.value.bits }
    }

    /// Returns the raw `SIGEV_THREAD_ID` thread ID storage.
    pub fn thread_id(&self) -> i32 {
        // SAFETY: every RawSigeventUnion bit pattern is valid as i32 storage.
        unsafe { self.data.tid }
    }
}
