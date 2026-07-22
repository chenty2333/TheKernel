use alloc::{string::String, vec::Vec};
use core::{mem::size_of, sync::atomic::Ordering};

use axerrno::{AxError, AxResult};
use thekernel_linux_cred::{KeyPermission, KeyPermissionMask};

use super::accounting::{AbiQuotaCharge, QuotaAdmission, ResidentCharge};
use crate::task::{Kgid, Kuid, UserNamespaceId};

const USER_KEY_PAYLOAD_MAX: usize = 32_767;
const BIG_KEY_PAYLOAD_MAX: usize = 1 << 20;
pub(super) const BIG_KEY_ABI_PAYLOAD_CHARGE: usize = 16;
pub(super) const KEY_RESIDENT_NODE_OVERHEAD: usize = 64;
pub(super) const KEY_LINK_CHARGE: usize = size_of::<i32>();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum KeyState {
    Positive,
    Revoked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PublishedKeyringName {
    pub(super) namespace: UserNamespaceId,
    pub(super) order: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GcPlanState {
    Touched,
    Queued,
    Retire,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct GcPlanScratch {
    pub(super) epoch: u64,
    pub(super) root_drops: usize,
    pub(super) link_drops: usize,
    pub(super) state: Option<GcPlanState>,
    pub(super) touched_next: Option<i32>,
    pub(super) work_next: Option<i32>,
}

impl GcPlanScratch {
    pub(super) const IDLE: Self = Self {
        epoch: 0,
        root_drops: 0,
        link_drops: 0,
        state: None,
        touched_next: None,
        work_next: None,
    };

    pub(super) const fn is_idle(self) -> bool {
        self.epoch == 0
            && self.root_drops == 0
            && self.link_drops == 0
            && self.state.is_none()
            && self.touched_next.is_none()
            && self.work_next.is_none()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyTypeKind {
    Keyring,
    User,
    Logon,
    BigKey,
}

impl KeyTypeKind {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "keyring" => Some(Self::Keyring),
            "user" => Some(Self::User),
            "logon" => Some(Self::Logon),
            "big_key" => Some(Self::BigKey),
            _ => None,
        }
    }

    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Keyring => "keyring",
            Self::User => "user",
            Self::Logon => "logon",
            Self::BigKey => "big_key",
        }
    }

    pub(super) const fn userspace_readable(self) -> bool {
        !matches!(self, Self::Logon)
    }

    pub(super) const fn supports_payload_update(self) -> bool {
        matches!(self, Self::User | Self::Logon | Self::BigKey)
    }

    pub(crate) const fn payload_limit(self) -> usize {
        match self {
            Self::User | Self::Logon => USER_KEY_PAYLOAD_MAX,
            Self::BigKey => BIG_KEY_PAYLOAD_MAX,
            Self::Keyring => 0,
        }
    }

    pub(super) const fn abi_payload_charge(self, payload_len: usize) -> usize {
        match self {
            Self::BigKey => BIG_KEY_ABI_PAYLOAD_CHARGE,
            Self::User | Self::Logon => payload_len,
            Self::Keyring => 0,
        }
    }

    fn default_permissions(self) -> KeyPermissionMask {
        let mut possessor = KeyPermission::VIEW
            | KeyPermission::SEARCH
            | KeyPermission::LINK
            | KeyPermission::SETATTR;
        if self.userspace_readable() {
            possessor |= KeyPermission::READ;
        }
        if self == Self::Keyring || self.supports_payload_update() {
            possessor |= KeyPermission::WRITE;
        }
        permission_mask(possessor, KeyPermission::VIEW)
    }
}

pub(super) fn permission_mask(possessor: KeyPermission, user: KeyPermission) -> KeyPermissionMask {
    KeyPermissionMask::from_lanes(Some(possessor), Some(user), None, None)
}

pub(super) fn thread_process_keyring_permissions() -> KeyPermissionMask {
    permission_mask(KeyPermission::ALL, KeyPermission::VIEW)
}

pub(super) fn anonymous_session_keyring_permissions() -> KeyPermissionMask {
    permission_mask(
        KeyPermission::ALL,
        KeyPermission::VIEW | KeyPermission::READ,
    )
}

pub(super) fn named_session_keyring_permissions() -> KeyPermissionMask {
    permission_mask(
        KeyPermission::ALL,
        KeyPermission::VIEW | KeyPermission::READ | KeyPermission::LINK,
    )
}

pub(super) fn uid_keyring_permissions() -> KeyPermissionMask {
    permission_mask(
        KeyPermission::VIEW
            | KeyPermission::READ
            | KeyPermission::WRITE
            | KeyPermission::SEARCH
            | KeyPermission::LINK,
        KeyPermission::ALL,
    )
}

pub(super) fn persistent_keyring_permissions() -> KeyPermissionMask {
    permission_mask(
        KeyPermission::VIEW
            | KeyPermission::READ
            | KeyPermission::WRITE
            | KeyPermission::SEARCH
            | KeyPermission::LINK,
        KeyPermission::VIEW | KeyPermission::READ,
    )
}

pub(super) struct Key {
    pub(super) kind: KeyTypeKind,
    pub(super) description: String,
    pub(super) payload: Vec<u8>,
    pub(super) links: Vec<i32>,
    /// Linux-visible owner used by permission and describe operations.
    pub(super) uid: Kuid,
    /// Stable owner of the ABI quota charge.
    ///
    /// This is intentionally distinct from `uid`: credential-driven visible
    /// ownership changes may leave the quota owner unchanged, while
    /// `KEYCTL_CHOWN` transfers both identities transactionally.
    pub(super) quota_uid: Kuid,
    pub(super) gid: Kgid,
    pub(super) perm: KeyPermissionMask,
    pub(super) state: KeyState,
    pub(super) expires_at: Option<u64>,
    pub(super) restricted: bool,
    /// Namespace and stable publication order of this public keyring name.
    /// This is non-owning and allocation-free; duplicate names remain legal.
    pub(super) published_name: Option<PublishedKeyringName>,
    pub(super) in_owner_quota: bool,
    pub(super) abi_charge: AbiQuotaCharge,
    pub(super) resident_charge: ResidentCharge,
    pub(super) root_refs: usize,
    pub(super) link_refs: usize,
    /// Intrusive, allocation-free scratch for a manager-owned GC transaction.
    /// It must be idle whenever the manager mutex is released.
    pub(super) gc_plan: GcPlanScratch,
    /// Intrusive work link used by the legacy single-root collector. Prepared
    /// GC refuses to touch a key while this field is populated.
    pub(super) gc_next: Option<i32>,
}

pub(super) fn wipe_key_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        // Volatile stores keep secret retirement observable to the compiler.
        unsafe { core::ptr::write_volatile(byte, 0) };
    }
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
}

impl Drop for Key {
    fn drop(&mut self) {
        wipe_key_bytes(&mut self.payload);
    }
}

impl Key {
    pub(super) fn keyring(
        description: String,
        uid: Kuid,
        gid: Kgid,
        perm: KeyPermissionMask,
    ) -> AxResult<Self> {
        Self::new(
            KeyTypeKind::Keyring,
            description,
            Vec::new(),
            uid,
            gid,
            perm,
        )
    }

    pub(super) fn positive(
        kind: KeyTypeKind,
        description: String,
        payload: Vec<u8>,
        uid: Kuid,
        gid: Kgid,
    ) -> AxResult<Self> {
        Self::new(
            kind,
            description,
            payload,
            uid,
            gid,
            kind.default_permissions(),
        )
    }

    pub(super) fn new(
        kind: KeyTypeKind,
        description: String,
        payload: Vec<u8>,
        uid: Kuid,
        gid: Kgid,
        perm: KeyPermissionMask,
    ) -> AxResult<Self> {
        if payload.len() > kind.payload_limit()
            || kind == KeyTypeKind::Keyring && !payload.is_empty()
            || matches!(
                kind,
                KeyTypeKind::User | KeyTypeKind::Logon | KeyTypeKind::BigKey
            ) && payload.is_empty()
            || kind == KeyTypeKind::Logon && description.find(':').is_none_or(|colon| colon == 0)
        {
            return Err(AxError::InvalidInput);
        }
        let description_bytes = description.len().checked_add(1).ok_or(AxError::NoMemory)?;
        let abi_payload_bytes = kind.abi_payload_charge(payload.len());
        let abi_bytes = description_bytes
            .checked_add(abi_payload_bytes)
            .ok_or(AxError::NoMemory)?;
        let resident_bytes = size_of::<Self>()
            .checked_add(KEY_RESIDENT_NODE_OVERHEAD)
            .and_then(|bytes| bytes.checked_add(description.capacity()))
            .and_then(|bytes| bytes.checked_add(payload.capacity()))
            .ok_or(AxError::NoMemory)?;
        Ok(Self {
            kind,
            description,
            payload,
            links: Vec::new(),
            uid,
            quota_uid: uid,
            gid,
            perm,
            state: KeyState::Positive,
            expires_at: None,
            restricted: false,
            published_name: None,
            in_owner_quota: true,
            abi_charge: AbiQuotaCharge {
                keys: 1,
                bytes: abi_bytes,
            },
            resident_charge: ResidentCharge {
                objects: 1,
                bytes: resident_bytes,
                link_bytes: 0,
            },
            root_refs: 0,
            link_refs: 0,
            gc_plan: GcPlanScratch::IDLE,
            gc_next: None,
        })
    }

    pub(super) fn is_keyring(&self) -> bool {
        self.kind == KeyTypeKind::Keyring
    }

    pub(super) fn has_references(&self) -> bool {
        self.root_refs != 0 || self.link_refs != 0
    }

    pub(super) fn ongoing_quota_admission(&self) -> QuotaAdmission {
        if self.in_owner_quota {
            QuotaAdmission::Enforced
        } else {
            QuotaAdmission::Exempt
        }
    }

    pub(super) fn payload_charges(
        &self,
        payload: &Vec<u8>,
    ) -> AxResult<(AbiQuotaCharge, ResidentCharge)> {
        let old_payload_abi = self.kind.abi_payload_charge(self.payload.len());
        let new_payload_abi = self.kind.abi_payload_charge(payload.len());
        let abi_bytes = self
            .abi_charge
            .bytes
            .checked_sub(old_payload_abi)
            .and_then(|bytes| bytes.checked_add(new_payload_abi))
            .ok_or(AxError::BadState)?;
        let resident_bytes = self
            .resident_charge
            .bytes
            .checked_sub(self.payload.capacity())
            .and_then(|bytes| bytes.checked_add(payload.capacity()))
            .ok_or(AxError::BadState)?;
        Ok((
            AbiQuotaCharge {
                keys: self.abi_charge.keys,
                bytes: abi_bytes,
            },
            ResidentCharge {
                objects: self.resident_charge.objects,
                bytes: resident_bytes,
                link_bytes: self.resident_charge.link_bytes,
            },
        ))
    }

    pub(super) fn with_added_link_charges(
        &self,
        new_link_capacity: usize,
    ) -> AxResult<(AbiQuotaCharge, ResidentCharge)> {
        let new_link_bytes = new_link_capacity
            .checked_mul(KEY_LINK_CHARGE)
            .ok_or(AxError::NoMemory)?;
        Ok((
            AbiQuotaCharge {
                keys: self.abi_charge.keys,
                bytes: self
                    .abi_charge
                    .bytes
                    .checked_add(KEY_LINK_CHARGE)
                    .ok_or(AxError::NoMemory)?,
            },
            ResidentCharge {
                objects: self.resident_charge.objects,
                bytes: self.resident_charge.bytes,
                link_bytes: new_link_bytes,
            },
        ))
    }

    pub(super) fn with_removed_link_charges(
        &self,
        links: usize,
        new_link_capacity: usize,
    ) -> AxResult<(AbiQuotaCharge, ResidentCharge)> {
        let abi_bytes = links
            .checked_mul(KEY_LINK_CHARGE)
            .ok_or(AxError::BadState)?;
        let resident_link_bytes = new_link_capacity
            .checked_mul(KEY_LINK_CHARGE)
            .ok_or(AxError::BadState)?;
        Ok((
            AbiQuotaCharge {
                keys: self.abi_charge.keys,
                bytes: self
                    .abi_charge
                    .bytes
                    .checked_sub(abi_bytes)
                    .ok_or(AxError::BadState)?,
            },
            ResidentCharge {
                objects: self.resident_charge.objects,
                bytes: self.resident_charge.bytes,
                link_bytes: resident_link_bytes,
            },
        ))
    }

    pub(super) fn next_link_capacity(&self) -> AxResult<Option<usize>> {
        if self.links.len() < self.links.capacity() {
            return Ok(None);
        }
        let new_len = self.links.len().checked_add(1).ok_or(AxError::NoMemory)?;
        Ok(Some(
            self.links
                .capacity()
                .checked_mul(2)
                .unwrap_or(usize::MAX)
                .max(4)
                .max(new_len),
        ))
    }

    pub(super) fn stage_link_push(&self, serial: i32, new_capacity: usize) -> AxResult<Vec<i32>> {
        if new_capacity <= self.links.capacity() || new_capacity <= self.links.len() {
            return Err(AxError::BadState);
        }
        let mut staged = Vec::new();
        staged
            .try_reserve_exact(new_capacity)
            .map_err(|_| AxError::NoMemory)?;
        staged.extend_from_slice(&self.links);
        staged.push(serial);
        Ok(staged)
    }
}
