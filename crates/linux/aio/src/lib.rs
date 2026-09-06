//! Pure native Linux AIO ABI decoding and lifecycle plans.
//!
//! Mechanism adapters own opaque request handles, user-memory copying, VFS
//! execution, completion delivery, waiting, and owner lifetime.  This crate
//! only admits copied requests and returns explicit state transitions.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

pub const IOCB_BYTES: usize = 64;
pub const IO_EVENT_BYTES: usize = 32;
pub const IOCB_FLAG_RESFD: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AioError {
    InvalidId,
    InvalidEntries,
    InvalidOpcode,
    InvalidFlags,
    InvalidPriority,
    InvalidFd,
    Limit,
    NotFound,
    Busy,
    Overflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct AioContextId(u64);
impl AioContextId {
    pub const fn new(raw: u64) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }
    pub const fn get(self) -> u64 {
        self.0
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct AioRequestId(u64);
impl AioRequestId {
    pub const fn new(raw: u64) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoEvent {
    pub data: u64,
    pub object: u64,
    pub result: i64,
    pub result2: i64,
}
const _: () = assert!(core::mem::size_of::<IoEvent>() == IO_EVENT_BYTES);
const _: () = assert!(core::mem::align_of::<IoEvent>() == 8);

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Iocb {
    pub data: u64,
    pub key: u32,
    pub rw_flags: u32,
    pub opcode: u16,
    pub reqprio: i16,
    pub fd: u32,
    pub buffer: u64,
    pub nbytes: u64,
    pub offset: i64,
    pub reserved2: u64,
    pub flags: u32,
    pub resfd: u32,
}
const _: () = assert!(core::mem::size_of::<Iocb>() == IOCB_BYTES);
const _: () = assert!(core::mem::align_of::<Iocb>() == 8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AioOpcode {
    Pread,
    Pwrite,
    Fsync,
    Fdatasync,
    Poll,
    Noop,
    Preadv,
    Pwritev,
}
impl TryFrom<u16> for AioOpcode {
    type Error = AioError;
    fn try_from(raw: u16) -> Result<Self, Self::Error> {
        match raw {
            0 => Ok(Self::Pread),
            1 => Ok(Self::Pwrite),
            2 => Ok(Self::Fsync),
            3 => Ok(Self::Fdatasync),
            5 => Ok(Self::Poll),
            6 => Ok(Self::Noop),
            7 => Ok(Self::Preadv),
            8 => Ok(Self::Pwritev),
            _ => Err(AioError::InvalidOpcode),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AioRequest {
    pub id: AioRequestId,
    pub data: u64,
    pub opcode: AioOpcode,
    pub fd: u32,
    pub buffer: u64,
    pub nbytes: u64,
    pub offset: i64,
    pub rw_flags: u32,
}
impl AioRequest {
    pub fn decode(id: AioRequestId, raw: Iocb) -> Result<Self, AioError> {
        if raw.key != 0 || raw.reqprio != 0 || raw.reserved2 != 0 {
            return Err(AioError::InvalidPriority);
        }
        if raw.flags != 0 || raw.resfd != 0 {
            return Err(AioError::InvalidFlags);
        }
        let opcode = AioOpcode::try_from(raw.opcode)?;
        if matches!(opcode, AioOpcode::Noop) {
            if raw.fd != 0
                || raw.buffer != 0
                || raw.nbytes != 0
                || raw.offset != 0
                || raw.rw_flags != 0
            {
                return Err(AioError::InvalidFd);
            }
        } else if raw.fd == u32::MAX {
            return Err(AioError::InvalidFd);
        }
        Ok(Self {
            id,
            data: raw.data,
            opcode,
            fd: raw.fd,
            buffer: raw.buffer,
            nbytes: raw.nbytes,
            offset: raw.offset,
            rw_flags: raw.rw_flags,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AioContextSnapshot {
    pub owner: u64,
    pub capacity: u32,
    pub submitted: u32,
    pub completed: u32,
    pub destroying: bool,
}
impl AioContextSnapshot {
    pub const fn new(owner: u64, capacity: u32) -> Result<Self, AioError> {
        if owner == 0 || capacity == 0 {
            Err(AioError::InvalidEntries)
        } else {
            Ok(Self {
                owner,
                capacity,
                submitted: 0,
                completed: 0,
                destroying: false,
            })
        }
    }
    pub const fn outstanding(self) -> u32 {
        self.submitted - self.completed
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AioPlan<H> {
    Submit {
        context: AioContextId,
        requests: Vec<(AioRequest, Option<H>)>,
        after: AioContextSnapshot,
    },
    GetEvents {
        context: AioContextId,
        min: u32,
        max: u32,
        snapshot: AioContextSnapshot,
    },
    Destroy {
        context: AioContextId,
        snapshot: AioContextSnapshot,
    },
    OwnerCleanup {
        context: AioContextId,
        owner: u64,
        snapshot: AioContextSnapshot,
    },
}
pub trait AioHandle: Copy + Eq {}
impl<T: Copy + Eq> AioHandle for T {}
pub trait AioResolver {
    type Handle: AioHandle;
    fn resolve(&self, fd: u32) -> Result<Self::Handle, AioError>;
}

pub fn plan_submit<R: AioResolver>(
    context: AioContextId,
    snapshot: AioContextSnapshot,
    requests: &[AioRequest],
    resolver: &R,
) -> Result<AioPlan<R::Handle>, AioError> {
    if snapshot.destroying {
        return Err(AioError::Busy);
    }
    let count = u32::try_from(requests.len()).map_err(|_| AioError::Limit)?;
    let outstanding = snapshot
        .outstanding()
        .checked_add(count)
        .ok_or(AioError::Overflow)?;
    if count == 0 || outstanding > snapshot.capacity {
        return Err(AioError::Limit);
    }
    let mut resolved = Vec::new();
    resolved
        .try_reserve(requests.len())
        .map_err(|_| AioError::Limit)?;
    for &request in requests {
        let handle = if request.opcode == AioOpcode::Noop {
            None
        } else {
            Some(resolver.resolve(request.fd)?)
        };
        resolved.push((request, handle));
    }
    Ok(AioPlan::Submit {
        context,
        requests: resolved,
        after: AioContextSnapshot {
            submitted: snapshot.submitted + count,
            ..snapshot
        },
    })
}
pub fn plan_getevents(
    context: AioContextId,
    snapshot: AioContextSnapshot,
    min: u32,
    max: u32,
) -> Result<AioPlan<()>, AioError> {
    if snapshot.destroying || max == 0 || min > max || max > snapshot.capacity {
        return Err(AioError::InvalidEntries);
    }
    Ok(AioPlan::GetEvents {
        context,
        min,
        max,
        snapshot,
    })
}
pub fn plan_destroy(
    context: AioContextId,
    snapshot: AioContextSnapshot,
) -> Result<AioPlan<()>, AioError> {
    if snapshot.destroying {
        Err(AioError::Busy)
    } else {
        Ok(AioPlan::Destroy {
            context,
            snapshot: AioContextSnapshot {
                destroying: true,
                ..snapshot
            },
        })
    }
}
pub fn plan_owner_cleanup(
    context: AioContextId,
    snapshot: AioContextSnapshot,
    owner: u64,
) -> Result<AioPlan<()>, AioError> {
    if owner == 0 || owner != snapshot.owner {
        Err(AioError::NotFound)
    } else {
        Ok(AioPlan::OwnerCleanup {
            context,
            owner,
            snapshot: AioContextSnapshot {
                destroying: true,
                ..snapshot
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Clone, Copy, Eq, PartialEq)]
    struct Resolver;
    impl AioResolver for Resolver {
        type Handle = u32;
        fn resolve(&self, fd: u32) -> Result<u32, AioError> {
            Ok(fd)
        }
    }
    fn ctx() -> AioContextId {
        AioContextId::new(1).unwrap()
    }
    #[test]
    fn layout_and_decode_are_linux_sized() {
        let request = AioRequest::decode(
            AioRequestId::new(2).unwrap(),
            Iocb {
                data: 3,
                key: 0,
                rw_flags: 0,
                opcode: 0,
                reqprio: 0,
                fd: 4,
                buffer: 5,
                nbytes: 6,
                offset: 7,
                reserved2: 0,
                flags: 0,
                resfd: 0,
            },
        )
        .unwrap();
        assert_eq!(request.fd, 4);
        assert!(
            AioRequest::decode(
                AioRequestId::new(2).unwrap(),
                Iocb {
                    flags: IOCB_FLAG_RESFD,
                    ..Iocb {
                        data: 0,
                        key: 0,
                        rw_flags: 0,
                        opcode: 6,
                        reqprio: 0,
                        fd: 0,
                        buffer: 0,
                        nbytes: 0,
                        offset: 0,
                        reserved2: 0,
                        flags: 0,
                        resfd: 0
                    }
                }
            )
            .is_err()
        );
    }
    #[test]
    fn submit_snapshots_capacity_and_handles() {
        let state = AioContextSnapshot::new(8, 2).unwrap();
        let request = AioRequest::decode(
            AioRequestId::new(9).unwrap(),
            Iocb {
                data: 0,
                key: 0,
                rw_flags: 0,
                opcode: 6,
                reqprio: 0,
                fd: 0,
                buffer: 0,
                nbytes: 0,
                offset: 0,
                reserved2: 0,
                flags: 0,
                resfd: 0,
            },
        )
        .unwrap();
        let plan = plan_submit(ctx(), state, &[request], &Resolver).unwrap();
        match plan {
            AioPlan::Submit {
                requests, after, ..
            } => {
                assert_eq!(requests.len(), 1);
                assert_eq!(requests[0].1, None);
                assert_eq!(after.outstanding(), 1);
            }
            _ => panic!(),
        }
    }
}
