use alloc::{
    alloc::{Layout, alloc},
    boxed::Box,
    sync::Arc,
    vec::Vec,
};
use core::{
    mem::{MaybeUninit, size_of},
    sync::atomic::{AtomicUsize, Ordering},
};

use axerrno::{AxError, AxResult, LinuxError};
use axnet::CMsgData;
use linux_raw_sys::net::{SCM_RIGHTS, SOL_SOCKET, cmsghdr};
use starry_vm::{VmMutPtr, vm_read_slice};

use crate::{
    file::{FileDescription, Socket, epoll::Epoll, get_file_description, try_reserve_fd},
    mm::UserPtr,
};

/// Linux's per-message hard limit from `net/core/scm.c`.
pub const SCM_MAX_FD: usize = 253;

// Conservatively charge more than just the Arc slot. This represents the
// retained OFD reference plus queue/control metadata and prevents empty Unix
// datagrams from turning fd references into an unmetered resource.
const SCM_RIGHTS_FD_QUEUE_CHARGE: usize = 64;

// Until Credential v2 can key this ledger by user namespace and real kuid,
// impose a hard system-wide ceiling. This is deliberately independent of a
// destination socket's byte budget: blocking sendmsg callers may hold their
// owned SCM_RIGHTS snapshot while waiting to obtain that socket admission.
const SCM_RIGHTS_INFLIGHT_LIMIT: usize = 16_384;
static SCM_RIGHTS_INFLIGHT: AtomicUsize = AtomicUsize::new(0);

fn try_acquire_scm_rights(count: usize) -> AxResult<()> {
    SCM_RIGHTS_INFLIGHT
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current
                .checked_add(count)
                .filter(|next| *next <= SCM_RIGHTS_INFLIGHT_LIMIT)
        })
        .map_err(|_| AxError::from(LinuxError::ENOBUFS))?;
    Ok(())
}

const fn cmsg_align(len: usize) -> Option<usize> {
    match len.checked_add(size_of::<usize>() - 1) {
        Some(len) => Some(len & !(size_of::<usize>() - 1)),
        None => None,
    }
}

const fn cmsg_header_len() -> usize {
    // `cmsghdr` has native size/alignment on every supported Linux ABI.
    size_of::<cmsghdr>()
}

const fn cmsg_len(body_len: usize) -> Option<usize> {
    match cmsg_align(cmsg_header_len()) {
        Some(header_len) => header_len.checked_add(body_len),
        None => None,
    }
}

const fn cmsg_space(body_len: usize) -> Option<usize> {
    match (cmsg_align(cmsg_header_len()), cmsg_align(body_len)) {
        (Some(header_len), Some(body_len)) => header_len.checked_add(body_len),
        _ => None,
    }
}

fn try_box<T>(value: T) -> AxResult<Box<T>> {
    let raw = unsafe { alloc(Layout::new::<T>()) }.cast::<T>();
    if raw.is_null() {
        return Err(AxError::NoMemory);
    }
    unsafe {
        raw.write(value);
        Ok(Box::from_raw(raw))
    }
}

pub enum CMsg {
    Rights {
        fds: Vec<Arc<FileDescription>>,
        inflight_count: usize,
    },
}

impl Drop for CMsg {
    fn drop(&mut self) {
        let Self::Rights { inflight_count, .. } = self;
        SCM_RIGHTS_INFLIGHT.fetch_sub(*inflight_count, Ordering::AcqRel);
    }
}

impl CMsg {
    /// Appends one SCM_RIGHTS header to a pre-reserved aggregate list.
    /// Multiple user headers are coalesced into one queued object, matching
    /// Linux's single `scm_fp_list` and keeping metadata bounded by
    /// `SCM_MAX_FD` rather than by arbitrary header fragmentation.
    pub fn append_rights(
        hdr_addr: usize,
        hdr: &cmsghdr,
        fds: &mut Vec<Arc<FileDescription>>,
    ) -> AxResult<()> {
        let header_len = cmsg_header_len();
        if hdr.cmsg_len < header_len {
            return Err(AxError::InvalidInput);
        }
        if (hdr.cmsg_level as u32, hdr.cmsg_type as u32) != (SOL_SOCKET, SCM_RIGHTS) {
            return Err(AxError::InvalidInput);
        }

        let data_len = hdr.cmsg_len - header_len;
        // Linux's scm_fp_copy consumes the complete fd integers and ignores a
        // final 1-3 data bytes.
        let fd_count = data_len / size_of::<i32>();
        if fds
            .len()
            .checked_add(fd_count)
            .is_none_or(|count| count > SCM_MAX_FD)
        {
            return Err(AxError::InvalidInput);
        }

        let data_addr = hdr_addr
            .checked_add(header_len)
            .ok_or(AxError::InvalidInput)?;
        let data_bytes = fd_count
            .checked_mul(size_of::<i32>())
            .ok_or(AxError::InvalidInput)?;
        let mut raw_fds = Vec::new();
        raw_fds
            .try_reserve_exact(fd_count)
            .map_err(|_| AxError::NoMemory)?;
        raw_fds.resize(fd_count, 0_i32);
        if data_bytes != 0 {
            // Never expose userspace as a Rust slice. A sibling sharing this
            // address space may mutate or unmap the control buffer while the
            // syscall runs; take one bounded owned snapshot, then parse only
            // kernel memory.
            vm_read_slice(data_addr as *const u8, unsafe {
                core::slice::from_raw_parts_mut(
                    raw_fds.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                    data_bytes,
                )
            })?;
        }
        for fd in raw_fds {
            if fd < 0 {
                return Err(AxError::BadFileDescriptor);
            }
            let description = get_file_description(fd)?;

            // Queued SCM_RIGHTS references require Linux's Unix-socket cycle
            // collector. Until that collector exists, reject the two object
            // kinds that can retain Unix sockets instead of leaking an
            // unreachable OFD cycle forever.
            if description.inner.downcast_ref::<Socket>().is_some()
                || description.inner.downcast_ref::<Epoll>().is_some()
            {
                return Err(AxError::OperationNotSupported);
            }
            fds.push(description);
        }
        Ok(())
    }

    pub fn from_rights(fds: Vec<Arc<FileDescription>>) -> AxResult<Option<CMsgData>> {
        if fds.is_empty() {
            return Ok(None);
        }
        let charge = size_of::<Self>()
            .checked_add(
                fds.len()
                    .checked_mul(SCM_RIGHTS_FD_QUEUE_CHARGE)
                    .ok_or(AxError::NoMemory)?,
            )
            .and_then(|charge| {
                fds.capacity()
                    .checked_mul(size_of::<Arc<FileDescription>>())
                    .and_then(|storage| charge.checked_add(storage))
            })
            .ok_or(AxError::NoMemory)?;
        try_acquire_scm_rights(fds.len())?;
        let inflight_count = fds.len();
        Ok(Some(CMsgData::new(
            try_box(Self::Rights {
                fds,
                inflight_count,
            })?,
            charge,
        )))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RightsPushResult {
    pub installed: usize,
    pub published: bool,
}

pub struct CMsgBuilder<'a> {
    hdr: UserPtr<cmsghdr>,
    len: &'a mut usize,
    capacity: usize,
}

impl<'a> CMsgBuilder<'a> {
    pub fn new(msg: UserPtr<cmsghdr>, len: &'a mut usize) -> Self {
        let capacity = *len;
        *len = 0;
        Self {
            hdr: msg,
            len,
            capacity,
        }
    }

    /// Publishes the longest SCM_RIGHTS prefix that fits both the control
    /// buffer and the receiver's fd-number limit.
    ///
    /// Once the socket payload has been consumed, Linux treats user-control
    /// faults, fd exhaustion, and ancillary allocation failure as control
    /// truncation. Accordingly this method never propagates those failures;
    /// it either publishes a fully described non-empty cmsg or publishes
    /// nothing. The returned count tells the caller when to set MSG_CTRUNC.
    pub fn push_rights(&mut self, fds: &[Arc<FileDescription>], cloexec: bool) -> RightsPushResult {
        let Some(remaining) = self.capacity.checked_sub(*self.len) else {
            return RightsPushResult::default();
        };
        let Some(header_len) = cmsg_align(cmsg_header_len()) else {
            return RightsPushResult::default();
        };
        let Some(body_capacity) = remaining.checked_sub(header_len) else {
            return RightsPushResult::default();
        };
        let count = fds.len().min(body_capacity / size_of::<i32>());
        if count == 0 {
            return RightsPushResult::default();
        }

        let base = self.hdr.address().as_usize();
        let Some(data_addr) = base.checked_add(header_len) else {
            return RightsPushResult::default();
        };

        // Linux copies each number before fd_install. Publish one alias at a
        // time so a fault in a later integer preserves the visible prefix,
        // while CLONE_FILES siblings cannot guess any not-yet-copied slot.
        let mut installed = 0usize;
        for (index, description) in fds[..count].iter().enumerate() {
            let Ok(Some(reservation)) = try_reserve_fd(cloexec) else {
                break;
            };
            let fd = reservation.fd();
            let Some(offset) = index.checked_mul(size_of::<i32>()) else {
                break;
            };
            let Some(dst) = data_addr.checked_add(offset) else {
                break;
            };
            if (dst as *mut i32).vm_write(fd).is_err()
                || reservation.publish(description.clone()).is_err()
            {
                break;
            }
            installed += 1;
        }
        if installed == 0 {
            return RightsPushResult::default();
        }

        let body_len = installed * size_of::<i32>();
        let Some(message_len) = cmsg_len(body_len) else {
            return RightsPushResult {
                installed,
                published: false,
            };
        };
        let Some(message_space) = cmsg_space(body_len) else {
            return RightsPushResult {
                installed,
                published: false,
            };
        };
        // Linux may publish a final cmsg in CMSG_LEN bytes when the caller did
        // not provide the trailing CMSG_SPACE padding.
        let used = message_space.min(remaining);
        let Some(next) = base.checked_add(used) else {
            return RightsPushResult {
                installed,
                published: false,
            };
        };

        let hdr = cmsghdr {
            cmsg_len: message_len,
            cmsg_level: SOL_SOCKET as _,
            cmsg_type: SCM_RIGHTS as _,
        };
        if (base as *mut cmsghdr).vm_write(hdr).is_err() {
            // Linux has already installed the fd prefix at this point. Keep it
            // even though the control header itself could not be published;
            // msg_controllen remains zero.
            return RightsPushResult {
                installed,
                published: false,
            };
        }

        self.hdr = UserPtr::from(next);
        *self.len += used;
        RightsPushResult {
            installed,
            published: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static SCM_ACCOUNT_TEST_LOCK: spin::Mutex<()> = spin::Mutex::new(());

    #[test]
    fn cmsg_lengths_match_linux_native_alignment() {
        assert_eq!(cmsg_len(0), Some(size_of::<cmsghdr>()));
        assert_eq!(cmsg_len(size_of::<i32>()), Some(size_of::<cmsghdr>() + 4));
        assert_eq!(
            cmsg_space(size_of::<i32>()),
            cmsg_align(size_of::<cmsghdr>()).and_then(|header| header.checked_add(8))
        );
    }

    #[test]
    fn rights_limit_is_linux_scm_max_fd() {
        assert_eq!(SCM_MAX_FD, 253);
    }

    #[test]
    fn inflight_rights_admission_saturates_and_drop_restores_the_ledger() {
        let _guard = SCM_ACCOUNT_TEST_LOCK.lock();
        let baseline = SCM_RIGHTS_INFLIGHT.load(Ordering::Acquire);
        let available = SCM_RIGHTS_INFLIGHT_LIMIT - baseline;
        assert!(available > 0);
        try_acquire_scm_rights(available).unwrap();
        let admitted = CMsg::Rights {
            fds: Vec::new(),
            inflight_count: available,
        };
        assert_eq!(
            try_acquire_scm_rights(1),
            Err(AxError::from(LinuxError::ENOBUFS))
        );
        drop(admitted);
        assert_eq!(SCM_RIGHTS_INFLIGHT.load(Ordering::Acquire), baseline);
    }
}
