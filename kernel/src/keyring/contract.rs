use alloc::{string::String, sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult};
use thekernel_linux_cred::KeyPermissionMask;

use crate::task::{Credentials, DacCredentialView, Kgid, Kuid, UserGid, UserNamespace, UserUid};

const KEY_REQKEY_DEFL_DEFAULT: i32 = 0;
const KEY_REQKEY_DEFL_NO_CHANGE: i32 = -1;
const KEY_REQKEY_DEFL_THREAD_KEYRING: i32 = 1;
const KEY_REQKEY_DEFL_PROCESS_KEYRING: i32 = 2;
const KEY_REQKEY_DEFL_SESSION_KEYRING: i32 = 3;
const KEY_REQKEY_DEFL_USER_KEYRING: i32 = 4;
const KEY_REQKEY_DEFL_USER_SESSION_KEYRING: i32 = 5;

/// Immutable task/process identities used by keyring lifecycle transitions.
///
/// `thread_owner` is the scheduler-derived kernel TID and never follows the
/// visible-TID rebind performed by a non-leader exec. `process_owner` is the
/// process-domain PID shared by every `CLONE_THREAD` sibling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KeyTaskOwner {
    thread_owner: u32,
    process_owner: u32,
}

impl KeyTaskOwner {
    pub(crate) const fn new(thread_owner: u32, process_owner: u32) -> Self {
        Self {
            thread_owner,
            process_owner,
        }
    }

    pub(super) const fn thread_owner(self) -> u32 {
        self.thread_owner
    }

    pub(super) const fn process_owner(self) -> u32 {
        self.process_owner
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KeyUserRecord {
    pub(crate) uid: Kuid,
    pub(crate) usage: usize,
    pub(crate) keys: usize,
    pub(crate) instantiated_keys: usize,
    pub(crate) quota_keys: usize,
    pub(crate) max_keys: usize,
    pub(crate) quota_bytes: usize,
    pub(crate) max_bytes: usize,
}

#[derive(Clone)]
pub(crate) struct KeyActor {
    pub(super) tid: u32,
    pub(super) pid: u32,
    /// Immutable scheduler-derived TID. The finite allocator never reuses it.
    pub(super) thread_owner: u32,
    /// Immutable process ID. The process registry retires rather than reuses it.
    pub(super) process_owner: u32,
    pub(super) ids: Credentials,
    pub(super) dac: DacCredentialView,
    pub(super) user_ns: Arc<UserNamespace>,
    pub(super) has_sys_admin: bool,
    pub(super) has_setuid: bool,
}

impl KeyActor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        tid: u32,
        pid: u32,
        thread_owner: u32,
        process_owner: u32,
        ids: Credentials,
        dac: DacCredentialView,
        user_ns: Arc<UserNamespace>,
        has_sys_admin: bool,
        has_setuid: bool,
    ) -> Self {
        Self {
            tid,
            pid,
            thread_owner,
            process_owner,
            ids,
            dac,
            user_ns,
            has_sys_admin,
            has_setuid,
        }
    }

    pub(super) fn owner_uid(&self) -> Kuid {
        self.dac.uid()
    }

    pub(super) fn owner_gid(&self) -> Kgid {
        self.dac.gid()
    }

    pub(super) fn real_uid(&self) -> Kuid {
        self.ids.ruid
    }

    pub(super) fn real_gid(&self) -> Kgid {
        self.ids.rgid
    }

    pub(super) fn in_group(&self, gid: Kgid) -> bool {
        self.dac.gid() == gid || self.dac.supplementary_groups().contains(&gid)
    }

    pub(super) fn map_user_uid(&self, raw: u32) -> AxResult<Kuid> {
        UserUid::from_raw(raw)
            .and_then(|uid| self.user_ns.user_uid_to_kernel(uid))
            .ok_or(AxError::InvalidInput)
    }

    pub(super) fn map_user_gid(&self, raw: u32) -> AxResult<Kgid> {
        UserGid::from_raw(raw)
            .and_then(|gid| self.user_ns.user_gid_to_kernel(gid))
            .ok_or(AxError::InvalidInput)
    }

    pub(super) fn display_uid(&self, uid: Kuid) -> u32 {
        self.user_ns
            .kernel_uid_to_user(uid)
            .map(UserUid::into_raw)
            .unwrap_or(65_534)
    }

    pub(super) fn display_gid(&self, gid: Kgid) -> u32 {
        self.user_ns
            .kernel_gid_to_user(gid)
            .map(UserGid::into_raw)
            .unwrap_or(65_534)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub(crate) enum ReqKeyDefault {
    NoChange    = -1,
    Default     = 0,
    Thread      = 1,
    Process     = 2,
    Session     = 3,
    User        = 4,
    UserSession = 5,
}

impl ReqKeyDefault {
    pub(crate) fn from_raw(raw: i32) -> Option<Self> {
        match raw {
            KEY_REQKEY_DEFL_NO_CHANGE => Some(Self::NoChange),
            KEY_REQKEY_DEFL_DEFAULT => Some(Self::Default),
            KEY_REQKEY_DEFL_THREAD_KEYRING => Some(Self::Thread),
            KEY_REQKEY_DEFL_PROCESS_KEYRING => Some(Self::Process),
            KEY_REQKEY_DEFL_SESSION_KEYRING => Some(Self::Session),
            KEY_REQKEY_DEFL_USER_KEYRING => Some(Self::User),
            KEY_REQKEY_DEFL_USER_SESSION_KEYRING => Some(Self::UserSession),
            _ => None,
        }
    }
}

pub(crate) enum KeyctlCommand {
    GetKeyringId {
        keyring: i32,
        create: bool,
    },
    JoinSession {
        name: Option<String>,
    },
    Update {
        key: i32,
        payload: Vec<u8>,
    },
    Revoke {
        key: i32,
    },
    Chown {
        key: i32,
        uid: Option<u32>,
        gid: Option<u32>,
    },
    SetPerm {
        key: i32,
        permissions: KeyPermissionMask,
    },
    Describe {
        key: i32,
    },
    Clear {
        keyring: i32,
    },
    Link {
        key: i32,
        keyring: i32,
    },
    Unlink {
        serial: i32,
        keyring: i32,
    },
    Search {
        keyring: i32,
        type_name: String,
        description: String,
        destination: Option<i32>,
    },
    Read {
        key: i32,
        copy_limit: Option<usize>,
    },
    SetReqKeyring {
        setting: ReqKeyDefault,
    },
    SetTimeout {
        key: i32,
        seconds: u64,
    },
    Invalidate {
        key: i32,
    },
    GetPersistent {
        uid: Option<u32>,
        destination: i32,
    },
    Restrict {
        keyring: i32,
    },
    Move {
        key: i32,
        from: i32,
        to: i32,
        exclusive: bool,
    },
}

pub(crate) enum KeyctlOutput {
    Value(isize),
    CountedBytes(Vec<u8>),
    KeyringIds(Vec<i32>),
    Payload { full_len: usize, bytes: Vec<u8> },
}
