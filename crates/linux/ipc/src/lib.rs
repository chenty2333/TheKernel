//! Pure System V IPC and POSIX mqueue policy. It owns neither managers, queues, nor VM.
#![no_std]
#![forbid(unsafe_code)]

pub const IPC_NOWAIT: u16 = 0o4000;
pub const SEM_UNDO: u16 = 0x1000;
pub const SHM_RDONLY: u32 = 0o10000;
pub const SHM_RND: u32 = 0o20000;
pub const SHMLBA: usize = 4096;
pub const MQ_PRIO_MAX: u32 = 32768;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpcError {
    PermissionDenied,
    InvalidMode,
    InvalidSelection,
    InvalidOperation,
    WouldBlock,
    InvalidAddress,
    InvalidQueueName,
    InvalidAttributes,
    InvalidPriority,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Credentials {
    pub euid: u32,
    pub egid: u32,
    pub privileged: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IpcPermission {
    pub uid: u32,
    pub gid: u32,
    pub mode: u16,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    Read,
    Write,
    Alter,
}
pub const fn authorize(c: Credentials, p: IpcPermission, a: Access) -> Result<(), IpcError> {
    if c.privileged {
        return Ok(());
    }
    let shift = if c.euid == p.uid {
        6
    } else if c.egid == p.gid {
        3
    } else {
        0
    };
    let need = match a {
        Access::Read => 4,
        Access::Write => 2,
        Access::Alter => 2,
    };
    if ((p.mode >> shift) & need) != 0 {
        Ok(())
    } else {
        Err(IpcError::PermissionDenied)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageSelection {
    First,
    Exact(i64),
    LowestAtLeast(i64),
    LowestType,
}
pub const fn select_message(request: i64) -> Result<MessageSelection, IpcError> {
    if request == 0 {
        Ok(MessageSelection::First)
    } else if request > 0 {
        Ok(MessageSelection::Exact(request))
    } else if request == i64::MIN {
        // Linux treats the unrepresentable absolute value as the largest
        // admissible type bound, so this still selects the lowest message
        // type rather than rejecting the receive request.
        Ok(MessageSelection::LowestType)
    } else if request == -0x7fff_ffff_ffff_ffff {
        Ok(MessageSelection::LowestType)
    } else {
        Ok(MessageSelection::LowestAtLeast(-request))
    }
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemBuf {
    pub num: u16,
    pub op: i16,
    pub flags: i16,
}
const _: () = {
    assert!(core::mem::size_of::<SemBuf>() == 6);
    assert!(core::mem::align_of::<SemBuf>() == 2);
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemPlan {
    Adjust {
        index: u16,
        delta: i16,
        undo: bool,
    },
    WaitZero {
        index: u16,
        nowait: bool,
    },
    WaitDecrease {
        index: u16,
        amount: u16,
        nowait: bool,
        undo: bool,
    },
}
pub const fn plan_sem_op(op: SemBuf) -> Result<SemPlan, IpcError> {
    if op.flags & !(IPC_NOWAIT as i16 | SEM_UNDO as i16) != 0 {
        return Err(IpcError::InvalidOperation);
    }
    let nowait = op.flags & IPC_NOWAIT as i16 != 0;
    let undo = op.flags & SEM_UNDO as i16 != 0;
    if op.op > 0 {
        Ok(SemPlan::Adjust {
            index: op.num,
            delta: op.op,
            undo,
        })
    } else if op.op == 0 {
        if undo {
            Err(IpcError::InvalidOperation)
        } else {
            Ok(SemPlan::WaitZero {
                index: op.num,
                nowait,
            })
        }
    } else {
        Ok(SemPlan::WaitDecrease {
            index: op.num,
            amount: op.op.unsigned_abs(),
            nowait,
            undo,
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShmSnapshot {
    pub length: usize,
    pub min_address: usize,
    pub max_address: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShmAttachPlan {
    pub address: usize,
    pub length: usize,
    pub readonly: bool,
}
pub const fn plan_shmat(
    snapshot: ShmSnapshot,
    requested: usize,
    flags: u32,
) -> Result<ShmAttachPlan, IpcError> {
    if flags & !(SHM_RDONLY | SHM_RND) != 0 {
        return Err(IpcError::InvalidOperation);
    }
    let address = if requested == 0 {
        snapshot.min_address
    } else if flags & SHM_RND != 0 {
        requested & !(SHMLBA - 1)
    } else if requested & (SHMLBA - 1) == 0 {
        requested
    } else {
        return Err(IpcError::InvalidAddress);
    };
    let end = match address.checked_add(snapshot.length) {
        Some(v) => v,
        None => return Err(IpcError::InvalidAddress),
    };
    if address < snapshot.min_address || end > snapshot.max_address {
        return Err(IpcError::InvalidAddress);
    }
    Ok(ShmAttachPlan {
        address,
        length: snapshot.length,
        readonly: flags & SHM_RDONLY != 0,
    })
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MqAttributes {
    pub max_messages: usize,
    pub message_size: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MqLimits {
    pub queues: usize,
    pub max_messages: usize,
    pub max_message_size: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MqPlan {
    Create { attributes: MqAttributes },
    Open,
}
pub fn plan_mq_open(
    name: &[u8],
    create: bool,
    attr: Option<MqAttributes>,
    limits: MqLimits,
) -> Result<MqPlan, IpcError> {
    if name.len() < 2 || name[0] != b'/' || name[1..].contains(&0) || name[1..].contains(&b'/') {
        return Err(IpcError::InvalidQueueName);
    }
    if !create {
        return Ok(MqPlan::Open);
    }
    let a = attr.ok_or(IpcError::InvalidAttributes)?;
    if a.max_messages == 0
        || a.max_messages > limits.max_messages
        || a.message_size == 0
        || a.message_size > limits.max_message_size
    {
        return Err(IpcError::InvalidAttributes);
    }
    Ok(MqPlan::Create { attributes: a })
}
pub const fn validate_priority(priority: u32) -> Result<(), IpcError> {
    if priority < MQ_PRIO_MAX {
        Ok(())
    } else {
        Err(IpcError::InvalidPriority)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permissions_and_selection() {
        assert_eq!(
            authorize(
                Credentials {
                    euid: 2,
                    egid: 2,
                    privileged: false
                },
                IpcPermission {
                    uid: 1,
                    gid: 2,
                    mode: 0o060
                },
                Access::Write
            ),
            Ok(())
        );
        assert_eq!(select_message(i64::MIN), Ok(MessageSelection::LowestType));
    }
    #[test]
    fn sem_validation_order() {
        assert_eq!(
            plan_sem_op(SemBuf {
                num: 0,
                op: 0,
                flags: SEM_UNDO as i16
            }),
            Err(IpcError::InvalidOperation)
        );
    }
}
