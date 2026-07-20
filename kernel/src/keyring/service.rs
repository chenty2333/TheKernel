use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::{
    mem::size_of,
    sync::atomic::{AtomicUsize, Ordering},
};

use axerrno::{AxError, AxResult, LinuxError};
use axsync::Mutex;
use thekernel_linux_cred::{KeyPermission, KeyPermissionMask};

use crate::{
    task::{Credentials, DacCredentialView, Kgid, Kuid, UserGid, UserNamespace, UserUid},
    time::wall_time,
};

const KEY_SPEC_THREAD_KEYRING: i32 = -1;
const KEY_SPEC_PROCESS_KEYRING: i32 = -2;
const KEY_SPEC_SESSION_KEYRING: i32 = -3;
const KEY_SPEC_USER_KEYRING: i32 = -4;
const KEY_SPEC_USER_SESSION_KEYRING: i32 = -5;

const KEY_REQKEY_DEFL_DEFAULT: i32 = 0;
const KEY_REQKEY_DEFL_NO_CHANGE: i32 = -1;
const KEY_REQKEY_DEFL_THREAD_KEYRING: i32 = 1;
const KEY_REQKEY_DEFL_PROCESS_KEYRING: i32 = 2;
const KEY_REQKEY_DEFL_SESSION_KEYRING: i32 = 3;
const KEY_REQKEY_DEFL_USER_KEYRING: i32 = 4;
const KEY_REQKEY_DEFL_USER_SESSION_KEYRING: i32 = 5;

const KEYRING_SEARCH_MAX_DEPTH: usize = 6;

const USER_KEY_PAYLOAD_MAX: usize = 32_767;
const BIG_KEY_PAYLOAD_MAX: usize = (1 << 20) - 1;
const KEY_CHARGE_OVERHEAD: usize = 32;
const KEY_LINK_CHARGE: usize = size_of::<i32>();
const KEY_MAXKEYS_DEFAULT: usize = 200;
const KEY_MAXBYTES_DEFAULT: usize = 20_000;
const KEY_ROOT_MAXKEYS_DEFAULT: usize = 1_000_000;
const KEY_ROOT_MAXBYTES_DEFAULT: usize = 25_000_000;

static KEY_MANAGER: Mutex<KeyManager> = Mutex::new(KeyManager::new());
static KEY_MAXKEYS: AtomicUsize = AtomicUsize::new(KEY_MAXKEYS_DEFAULT);
static KEY_MAXBYTES: AtomicUsize = AtomicUsize::new(KEY_MAXBYTES_DEFAULT);
static KEY_ROOT_MAXKEYS: AtomicUsize = AtomicUsize::new(KEY_ROOT_MAXKEYS_DEFAULT);
static KEY_ROOT_MAXBYTES: AtomicUsize = AtomicUsize::new(KEY_ROOT_MAXBYTES_DEFAULT);

#[derive(Clone, Copy, Eq, PartialEq)]
enum KeyState {
    Positive,
    Revoked,
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

    const fn name(self) -> &'static str {
        match self {
            Self::Keyring => "keyring",
            Self::User => "user",
            Self::Logon => "logon",
            Self::BigKey => "big_key",
        }
    }

    const fn userspace_readable(self) -> bool {
        !matches!(self, Self::Logon)
    }

    const fn supports_payload_update(self) -> bool {
        matches!(self, Self::User | Self::Logon | Self::BigKey)
    }

    pub(crate) const fn payload_limit(self) -> usize {
        match self {
            Self::User | Self::Logon => USER_KEY_PAYLOAD_MAX,
            Self::BigKey => BIG_KEY_PAYLOAD_MAX,
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

fn permission_mask(possessor: KeyPermission, user: KeyPermission) -> KeyPermissionMask {
    KeyPermissionMask::from_lanes(Some(possessor), Some(user), None, None)
}

fn thread_process_keyring_permissions() -> KeyPermissionMask {
    permission_mask(KeyPermission::ALL, KeyPermission::VIEW)
}

fn anonymous_session_keyring_permissions() -> KeyPermissionMask {
    permission_mask(
        KeyPermission::ALL,
        KeyPermission::VIEW | KeyPermission::READ,
    )
}

fn named_session_keyring_permissions() -> KeyPermissionMask {
    permission_mask(
        KeyPermission::ALL,
        KeyPermission::VIEW | KeyPermission::READ | KeyPermission::LINK,
    )
}

fn uid_keyring_permissions() -> KeyPermissionMask {
    permission_mask(
        KeyPermission::VIEW
            | KeyPermission::READ
            | KeyPermission::WRITE
            | KeyPermission::SEARCH
            | KeyPermission::LINK,
        KeyPermission::ALL,
    )
}

fn persistent_keyring_permissions() -> KeyPermissionMask {
    permission_mask(
        KeyPermission::VIEW
            | KeyPermission::READ
            | KeyPermission::WRITE
            | KeyPermission::SEARCH
            | KeyPermission::LINK,
        KeyPermission::VIEW | KeyPermission::READ,
    )
}

struct Key {
    kind: KeyTypeKind,
    description: String,
    payload: Vec<u8>,
    links: Vec<i32>,
    uid: Kuid,
    gid: Kgid,
    perm: KeyPermissionMask,
    state: KeyState,
    expires_at: Option<u64>,
    restricted: bool,
}

impl Key {
    fn keyring(description: String, uid: Kuid, gid: Kgid, perm: KeyPermissionMask) -> Self {
        Self {
            kind: KeyTypeKind::Keyring,
            description,
            payload: Vec::new(),
            links: Vec::new(),
            uid,
            gid,
            perm,
            state: KeyState::Positive,
            expires_at: None,
            restricted: false,
        }
    }

    fn positive(
        kind: KeyTypeKind,
        description: String,
        payload: Vec<u8>,
        uid: Kuid,
        gid: Kgid,
    ) -> Self {
        Self {
            kind,
            description,
            payload,
            links: Vec::new(),
            uid,
            gid,
            perm: kind.default_permissions(),
            state: KeyState::Positive,
            expires_at: None,
            restricted: false,
        }
    }

    fn is_keyring(&self) -> bool {
        self.kind == KeyTypeKind::Keyring
    }

    fn charge(&self) -> usize {
        KEY_CHARGE_OVERHEAD
            + self.description.len()
            + 1
            + self.payload.len()
            + self.links.len() * KEY_LINK_CHARGE
    }
}

#[derive(Clone)]
pub(crate) struct KeyActor {
    tid: u32,
    pid: u32,
    /// Immutable scheduler-derived TID. The finite allocator never reuses it.
    thread_owner: u32,
    /// Immutable process ID. The process registry retires rather than reuses it.
    process_owner: u32,
    ids: Credentials,
    dac: DacCredentialView,
    user_ns: Arc<UserNamespace>,
    has_sys_admin: bool,
    has_setuid: bool,
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

    fn owner_uid(&self) -> Kuid {
        self.dac.uid()
    }

    fn owner_gid(&self) -> Kgid {
        self.dac.gid()
    }

    fn user_uid(&self) -> Kuid {
        self.ids.ruid
    }

    fn in_group(&self, gid: Kgid) -> bool {
        self.dac.gid() == gid || self.dac.supplementary_groups().contains(&gid)
    }

    fn map_user_uid(&self, raw: u32) -> AxResult<Kuid> {
        UserUid::from_raw(raw)
            .and_then(|uid| self.user_ns.user_uid_to_kernel(uid))
            .ok_or(AxError::InvalidInput)
    }

    fn map_user_gid(&self, raw: u32) -> AxResult<Kgid> {
        UserGid::from_raw(raw)
            .and_then(|gid| self.user_ns.user_gid_to_kernel(gid))
            .ok_or(AxError::InvalidInput)
    }

    fn display_uid(&self, uid: Kuid) -> u32 {
        self.user_ns
            .kernel_uid_to_user(uid)
            .map(UserUid::into_raw)
            .unwrap_or(65_534)
    }

    fn display_gid(&self, gid: Kgid) -> u32 {
        self.user_ns
            .kernel_gid_to_user(gid)
            .map(UserGid::into_raw)
            .unwrap_or(65_534)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PossessionContext {
    Recompute,
    Fixed(bool),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResolvedKey {
    serial: i32,
    possession: PossessionContext,
}

impl ResolvedKey {
    const fn numeric(serial: i32) -> Self {
        Self {
            serial,
            possession: PossessionContext::Recompute,
        }
    }

    const fn possessed(serial: i32) -> Self {
        Self {
            serial,
            possession: PossessionContext::Fixed(true),
        }
    }

    const fn with_possession(serial: i32, possessed: bool) -> Self {
        Self {
            serial,
            possession: PossessionContext::Fixed(possessed),
        }
    }
}

impl From<i32> for ResolvedKey {
    fn from(serial: i32) -> Self {
        Self::numeric(serial)
    }
}

struct KeyManager {
    next_serial: Option<i32>,
    keys: BTreeMap<i32, Key>,
    thread_keyrings: BTreeMap<u32, i32>,
    process_keyrings: BTreeMap<u32, i32>,
    session_keyrings: BTreeMap<u32, i32>,
    user_keyrings: BTreeMap<Kuid, i32>,
    user_session_keyrings: BTreeMap<Kuid, i32>,
    persistent_keyrings: BTreeMap<Kuid, i32>,
    reqkey_defaults: BTreeMap<u32, i32>,
}

impl KeyManager {
    const fn new() -> Self {
        Self {
            next_serial: Some(1),
            keys: BTreeMap::new(),
            thread_keyrings: BTreeMap::new(),
            process_keyrings: BTreeMap::new(),
            session_keyrings: BTreeMap::new(),
            user_keyrings: BTreeMap::new(),
            user_session_keyrings: BTreeMap::new(),
            persistent_keyrings: BTreeMap::new(),
            reqkey_defaults: BTreeMap::new(),
        }
    }

    fn alloc_serial(&mut self) -> AxResult<i32> {
        let serial = self.next_serial.ok_or(AxError::from(LinuxError::ENOSPC))?;
        self.next_serial = serial.checked_add(1);
        Ok(serial)
    }

    #[cfg(test)]
    fn insert_key(&mut self, key: Key) -> i32 {
        let serial = self.alloc_serial().expect("test key serial space");
        self.keys.insert(serial, key);
        serial
    }

    fn try_insert_key(&mut self, key: Key, check_owner_quota: bool) -> AxResult<i32> {
        if self.keys.len() >= KEY_ROOT_MAXKEYS.load(Ordering::Relaxed)
            || check_owner_quota && !self.quota_allows_new(key.uid, key.charge())
        {
            return Err(LinuxError::EDQUOT.into());
        }
        let serial = self.alloc_serial()?;
        debug_assert!(!self.keys.contains_key(&serial));
        self.keys.insert(serial, key);
        Ok(serial)
    }

    #[cfg(test)]
    fn create_keyring(
        &mut self,
        description: String,
        uid: Kuid,
        gid: Kgid,
        permissions: KeyPermissionMask,
    ) -> i32 {
        self.insert_key(Key::keyring(description, uid, gid, permissions))
    }

    fn try_create_keyring(
        &mut self,
        description: String,
        uid: Kuid,
        gid: Kgid,
        permissions: KeyPermissionMask,
        check_owner_quota: bool,
    ) -> AxResult<i32> {
        self.try_insert_key(
            Key::keyring(description, uid, gid, permissions),
            check_owner_quota,
        )
    }

    fn special_keyring(&mut self, spec: i32, actor: &KeyActor, create: bool) -> AxResult<i32> {
        match spec {
            KEY_SPEC_THREAD_KEYRING => {
                if let Some(id) = self.thread_keyrings.get(&actor.thread_owner) {
                    return Ok(*id);
                }
                if !create {
                    return Err(LinuxError::ENOKEY.into());
                }
                let id = self.try_create_keyring(
                    format!("_tid.{}", actor.tid),
                    actor.owner_uid(),
                    actor.owner_gid(),
                    thread_process_keyring_permissions(),
                    false,
                )?;
                self.thread_keyrings.insert(actor.thread_owner, id);
                Ok(id)
            }
            KEY_SPEC_PROCESS_KEYRING => {
                if let Some(id) = self.process_keyrings.get(&actor.process_owner) {
                    return Ok(*id);
                }
                if !create {
                    return Err(LinuxError::ENOKEY.into());
                }
                let id = self.try_create_keyring(
                    format!("_pid.{}", actor.pid),
                    actor.owner_uid(),
                    actor.owner_gid(),
                    thread_process_keyring_permissions(),
                    false,
                )?;
                self.process_keyrings.insert(actor.process_owner, id);
                Ok(id)
            }
            KEY_SPEC_SESSION_KEYRING => {
                if let Some(id) = self.session_keyrings.get(&actor.process_owner) {
                    return Ok(*id);
                }
                if !create {
                    return Err(LinuxError::ENOKEY.into());
                }
                let id = self.try_create_keyring(
                    format!("_ses.{}", actor.pid),
                    actor.owner_uid(),
                    actor.owner_gid(),
                    anonymous_session_keyring_permissions(),
                    false,
                )?;
                self.session_keyrings.insert(actor.process_owner, id);
                Ok(id)
            }
            KEY_SPEC_USER_KEYRING => {
                let uid = actor.user_uid();
                if let Some(id) = self.user_keyrings.get(&uid) {
                    return Ok(*id);
                }
                let id = self.try_create_keyring(
                    format!("_uid.{}", uid.into_raw()),
                    uid,
                    actor.owner_gid(),
                    uid_keyring_permissions(),
                    true,
                )?;
                self.user_keyrings.insert(uid, id);
                Ok(id)
            }
            KEY_SPEC_USER_SESSION_KEYRING => {
                let uid = actor.user_uid();
                if let Some(id) = self.user_session_keyrings.get(&uid) {
                    return Ok(*id);
                }
                let user_keyring = self.special_keyring(KEY_SPEC_USER_KEYRING, actor, true)?;
                let id = self.try_create_keyring(
                    format!("_uid_ses.{}", uid.into_raw()),
                    uid,
                    actor.owner_gid(),
                    uid_keyring_permissions(),
                    true,
                )?;
                if let Err(error) = self.link_key_replace(id, user_keyring) {
                    self.keys.remove(&id);
                    return Err(error);
                }
                self.user_session_keyrings.insert(uid, id);
                Ok(id)
            }
            _ => Err(LinuxError::ENOKEY.into()),
        }
    }

    fn resolve_keyring(
        &mut self,
        id: i32,
        actor: &KeyActor,
        create: bool,
    ) -> AxResult<ResolvedKey> {
        let resolved = self.resolve_key(id, actor, create)?;
        let key = self
            .keys
            .get(&resolved.serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if key.is_keyring() {
            Ok(resolved)
        } else {
            Err(AxError::InvalidInput)
        }
    }

    fn resolve_key(
        &mut self,
        id: i32,
        actor: &KeyActor,
        create_special: bool,
    ) -> AxResult<ResolvedKey> {
        let resolved = if id < 0 {
            ResolvedKey::possessed(self.special_keyring(id, actor, create_special)?)
        } else {
            ResolvedKey::numeric(id)
        };
        let key = self
            .keys
            .get(&resolved.serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        self.check_key_available(key, true)?;
        Ok(resolved)
    }

    fn check_key_available(&self, key: &Key, allow_keyring: bool) -> AxResult<()> {
        match key.state {
            KeyState::Revoked => return Err(LinuxError::EKEYREVOKED.into()),
            KeyState::Positive => {}
        }
        if key
            .expires_at
            .is_some_and(|expires_at| wall_time().as_secs() >= expires_at)
        {
            return Err(LinuxError::EKEYEXPIRED.into());
        }
        if !allow_keyring && key.is_keyring() {
            return Err(AxError::InvalidInput);
        }
        Ok(())
    }

    fn possession_roots(&self, actor: &KeyActor) -> Vec<i32> {
        let mut roots = Vec::new();
        if let Some(id) = self.thread_keyrings.get(&actor.thread_owner) {
            roots.push(*id);
        }
        if let Some(id) = self.process_keyrings.get(&actor.process_owner) {
            roots.push(*id);
        }
        if let Some(id) = self.session_keyrings.get(&actor.process_owner) {
            roots.push(*id);
        } else if let Some(id) = self.user_session_keyrings.get(&actor.user_uid()) {
            roots.push(*id);
        }
        roots
    }

    fn is_possessed(&self, target: i32, actor: &KeyActor) -> bool {
        let mut pending = self
            .possession_roots(actor)
            .into_iter()
            .map(|serial| (serial, 0))
            .collect::<Vec<_>>();
        let mut visited = BTreeSet::new();
        while let Some((serial, depth)) = pending.pop() {
            if !visited.insert(serial) {
                continue;
            }

            let Some(key) = self.keys.get(&serial) else {
                continue;
            };
            if serial == target {
                return self.check_key_available(key, true).is_ok()
                    && key
                        .perm
                        .allows(key.uid, key.gid, &actor.dac, true, KeyPermission::SEARCH);
            }
            if depth > KEYRING_SEARCH_MAX_DEPTH {
                continue;
            }
            if self.check_key_available(key, true).is_err()
                || !key
                    .perm
                    .allows(key.uid, key.gid, &actor.dac, true, KeyPermission::SEARCH)
            {
                continue;
            }
            if key.is_keyring() {
                pending.extend(key.links.iter().copied().map(|serial| (serial, depth + 1)));
            }
        }
        false
    }

    fn key_has_perm(
        &self,
        key: impl Into<ResolvedKey>,
        actor: &KeyActor,
        permission: KeyPermission,
    ) -> AxResult<bool> {
        let resolved = key.into();
        let key = self
            .keys
            .get(&resolved.serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        self.check_key_available(key, true)?;
        let possessed = match resolved.possession {
            PossessionContext::Recompute => self.is_possessed(resolved.serial, actor),
            PossessionContext::Fixed(possessed) => possessed,
        };
        Ok(key
            .perm
            .allows(key.uid, key.gid, &actor.dac, possessed, permission))
    }

    fn keyring_has_write(
        &self,
        keyring: impl Into<ResolvedKey>,
        actor: &KeyActor,
    ) -> AxResult<bool> {
        self.keyring_has_perm(keyring, actor, KeyPermission::WRITE)
    }

    fn keyring_has_perm(
        &self,
        keyring: impl Into<ResolvedKey>,
        actor: &KeyActor,
        permission: KeyPermission,
    ) -> AxResult<bool> {
        let resolved = keyring.into();
        let key = self
            .keys
            .get(&resolved.serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        self.check_key_available(key, true)?;
        Ok(key.is_keyring() && self.key_has_perm(resolved, actor, permission)?)
    }

    fn find_linked_key(&self, keyring: i32, kind: KeyTypeKind, description: &str) -> Option<i32> {
        let keyring = self.keys.get(&keyring)?;
        keyring.links.iter().copied().find(|serial| {
            self.keys
                .get(serial)
                .is_some_and(|key| key.kind == kind && key.description == description)
        })
    }

    fn remove_key_everywhere(&mut self, serial: i32) {
        self.keys.remove(&serial);
        for key in self.keys.values_mut() {
            if key.is_keyring() {
                key.links.retain(|linked| *linked != serial);
            }
        }
        self.thread_keyrings.retain(|_, id| *id != serial);
        self.process_keyrings.retain(|_, id| *id != serial);
        self.session_keyrings.retain(|_, id| *id != serial);
        self.user_keyrings.retain(|_, id| *id != serial);
        self.user_session_keyrings.retain(|_, id| *id != serial);
        self.persistent_keyrings.retain(|_, id| *id != serial);
    }

    fn unlink_key_from_keyring(&mut self, keyring: i32, serial: i32) -> AxResult<()> {
        let keyring_key = self
            .keys
            .get_mut(&keyring)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if !keyring_key.is_keyring() {
            return Err(AxError::InvalidInput);
        }
        let before = keyring_key.links.len();
        keyring_key.links.retain(|linked| *linked != serial);
        if keyring_key.links.len() == before {
            return Err(LinuxError::ENOKEY.into());
        }
        Ok(())
    }

    fn link_key_replace(&mut self, keyring: i32, serial: i32) -> AxResult<()> {
        self.validate_keyring_link(keyring, serial)?;
        let (kind, description) = {
            let key = self
                .keys
                .get(&serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            (key.kind, key.description.clone())
        };
        let existing = self.find_linked_key(keyring, kind, &description);
        if existing == Some(serial) {
            return Ok(());
        }
        if existing.is_none() {
            let destination = self
                .keys
                .get(&keyring)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            let new_charge = destination
                .charge()
                .checked_add(KEY_LINK_CHARGE)
                .ok_or(AxError::NoMemory)?;
            if !self.quota_allows_resize(destination.uid, destination.charge(), new_charge) {
                return Err(LinuxError::EDQUOT.into());
            }
            self.keys
                .get_mut(&keyring)
                .ok_or(AxError::from(LinuxError::ENOKEY))?
                .links
                .try_reserve(1)
                .map_err(|_| AxError::NoMemory)?;
        }
        let keyring_key = self
            .keys
            .get_mut(&keyring)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if let Some(existing) = existing {
            let slot = keyring_key
                .links
                .iter_mut()
                .find(|linked| **linked == existing)
                .ok_or(AxError::BadState)?;
            *slot = serial;
        } else {
            keyring_key.links.push(serial);
        }
        Ok(())
    }

    fn validate_keyring_link(&self, destination: i32, serial: i32) -> AxResult<()> {
        self.check_link_destination(destination)?;

        let Some(key) = self.keys.get(&serial) else {
            return Err(LinuxError::ENOKEY.into());
        };
        if !key.is_keyring() {
            return Ok(());
        }

        let mut pending = Vec::from([(serial, 0)]);
        let mut visited = BTreeSet::new();
        while let Some((candidate, depth)) = pending.pop() {
            if candidate == destination {
                return Err(LinuxError::EDEADLK.into());
            }
            if !visited.insert(candidate) {
                continue;
            }
            let Some(candidate_key) = self.keys.get(&candidate) else {
                continue;
            };
            if !candidate_key.is_keyring() {
                continue;
            }
            if depth >= KEYRING_SEARCH_MAX_DEPTH
                && candidate_key
                    .links
                    .iter()
                    .any(|linked| self.keys.get(linked).is_some_and(Key::is_keyring))
            {
                return Err(LinuxError::ELOOP.into());
            }
            pending.extend(
                candidate_key
                    .links
                    .iter()
                    .copied()
                    .filter(|linked| self.keys.get(linked).is_some_and(Key::is_keyring))
                    .map(|linked| (linked, depth + 1)),
            );
        }
        Ok(())
    }

    fn check_link_destination(&self, destination: i32) -> AxResult<()> {
        let destination_key = self
            .keys
            .get(&destination)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if !destination_key.is_keyring() {
            return Err(AxError::InvalidInput);
        }
        if destination_key.restricted {
            return Err(AxError::OperationNotPermitted);
        }
        Ok(())
    }

    fn link_existing_key(
        &mut self,
        keyring: impl Into<ResolvedKey>,
        key: impl Into<ResolvedKey>,
        actor: &KeyActor,
        exclusive: bool,
    ) -> AxResult<()> {
        let keyring = keyring.into();
        let resolved = key.into();
        let key = self
            .keys
            .get(&resolved.serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        self.check_key_available(key, true)?;
        if !self.key_has_perm(resolved, actor, KeyPermission::LINK)? {
            return Err(LinuxError::EACCES.into());
        }
        let (kind, description) = (key.kind, key.description.clone());
        if !self.keyring_has_write(keyring, actor)? {
            return Err(LinuxError::EACCES.into());
        }
        if exclusive
            && self
                .find_linked_key(keyring.serial, kind, &description)
                .is_some()
        {
            return Err(LinuxError::EEXIST.into());
        }
        self.link_key_replace(keyring.serial, resolved.serial)
    }

    fn move_key_link(&mut self, from: i32, to: i32, serial: i32, exclusive: bool) -> AxResult<()> {
        if from == to {
            return Ok(());
        }
        let (kind, description) = self
            .keys
            .get(&serial)
            .map(|key| (key.kind, key.description.clone()))
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if !self
            .keys
            .get(&from)
            .is_some_and(|keyring| keyring.links.contains(&serial))
        {
            return Err(LinuxError::ENOKEY.into());
        }
        let replaced = self.find_linked_key(to, kind, &description);
        if exclusive && replaced.is_some() {
            return Err(LinuxError::EEXIST.into());
        }
        let replaced_index = replaced.and_then(|replaced| {
            self.keys
                .get(&to)
                .and_then(|keyring| keyring.links.iter().position(|linked| *linked == replaced))
        });
        if replaced.is_some() && replaced_index.is_none() {
            return Err(AxError::BadState);
        }
        self.validate_keyring_link(to, serial)?;

        let destination_needs_slot = !self
            .keys
            .get(&to)
            .is_some_and(|keyring| keyring.links.contains(&serial));
        if destination_needs_slot && replaced.is_none() {
            let source_uid = self
                .keys
                .get(&from)
                .map(|keyring| keyring.uid)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            let destination = self
                .keys
                .get(&to)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            let new_charge = destination
                .charge()
                .checked_add(KEY_LINK_CHARGE)
                .ok_or(AxError::NoMemory)?;
            if source_uid != destination.uid
                && !self.quota_allows_resize(destination.uid, destination.charge(), new_charge)
            {
                return Err(LinuxError::EDQUOT.into());
            }
        }
        if destination_needs_slot && replaced.is_none() {
            self.keys
                .get_mut(&to)
                .ok_or(AxError::from(LinuxError::ENOKEY))?
                .links
                .try_reserve(1)
                .map_err(|_| AxError::NoMemory)?;
        }

        self.keys
            .get_mut(&from)
            .ok_or(AxError::from(LinuxError::ENOKEY))?
            .links
            .retain(|linked| *linked != serial);
        let destination = self
            .keys
            .get_mut(&to)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if destination.links.contains(&serial) {
            return Ok(());
        }
        if let Some(replaced_index) = replaced_index {
            destination.links[replaced_index] = serial;
        } else {
            destination.links.push(serial);
        }
        Ok(())
    }

    fn search_keyring(
        &self,
        keyring: impl Into<ResolvedKey>,
        actor: &KeyActor,
        kind: KeyTypeKind,
        description: &str,
        visited: &mut BTreeSet<i32>,
    ) -> AxResult<Option<ResolvedKey>> {
        let keyring = keyring.into();
        let search_possessed = match keyring.possession {
            PossessionContext::Recompute => self.is_possessed(keyring.serial, actor),
            PossessionContext::Fixed(possessed) => possessed,
        };
        let mut pending = VecDeque::from([(keyring.serial, 0)]);
        let mut first_error = None;

        while let Some((ring_serial, depth)) = pending.pop_front() {
            if !visited.insert(ring_serial) {
                continue;
            }
            let Some(ring) = self.keys.get(&ring_serial) else {
                if depth == 0 {
                    return Err(LinuxError::ENOKEY.into());
                }
                continue;
            };
            if let Err(error) = self.check_key_available(ring, true) {
                if depth == 0 {
                    return Err(error);
                }
                continue;
            }
            if !ring.is_keyring() {
                if depth == 0 {
                    return Err(AxError::InvalidInput);
                }
                continue;
            }
            let ring_ref = ResolvedKey::with_possession(ring_serial, search_possessed);
            if !self.key_has_perm(ring_ref, actor, KeyPermission::SEARCH)? {
                if depth == 0 {
                    return Err(LinuxError::EACCES.into());
                }
                continue;
            }
            if depth == 0 && ring.kind == kind && ring.description == description {
                return Ok(Some(ResolvedKey::with_possession(
                    ring_serial,
                    search_possessed,
                )));
            }

            for serial in &ring.links {
                let Some(key) = self.keys.get(serial) else {
                    continue;
                };
                if key.kind != kind || key.description != description {
                    continue;
                }
                if let Err(error) = self.check_key_available(key, true) {
                    first_error.get_or_insert(error);
                    continue;
                }
                match self.key_has_perm(
                    ResolvedKey::with_possession(*serial, search_possessed),
                    actor,
                    KeyPermission::SEARCH,
                ) {
                    Ok(true) => {
                        return Ok(Some(ResolvedKey::with_possession(
                            *serial,
                            search_possessed,
                        )));
                    }
                    Ok(false) => {
                        first_error.get_or_insert(AxError::from(LinuxError::EACCES));
                    }
                    Err(error) => {
                        first_error.get_or_insert(error);
                    }
                }
            }

            for serial in &ring.links {
                if !self.keys.get(serial).is_some_and(Key::is_keyring) {
                    continue;
                }
                if depth >= KEYRING_SEARCH_MAX_DEPTH {
                    continue;
                }
                pending.push_back((*serial, depth + 1));
            }
        }

        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(None)
        }
    }

    fn get_persistent_keyring(&mut self, uid: Kuid, actor: &KeyActor) -> AxResult<i32> {
        if let Some(id) = self.persistent_keyrings.get(&uid) {
            return Ok(*id);
        }
        let id = self.try_create_keyring(
            format!("_persistent.{}", uid.into_raw()),
            uid,
            actor.owner_gid(),
            persistent_keyring_permissions(),
            false,
        )?;
        self.persistent_keyrings.insert(uid, id);
        Ok(id)
    }

    fn current_search_keyrings(&self, actor: &KeyActor) -> Vec<ResolvedKey> {
        let mut keyrings = Vec::new();
        if let Some(id) = self.thread_keyrings.get(&actor.thread_owner) {
            keyrings.push(ResolvedKey::possessed(*id));
        }
        if let Some(id) = self.process_keyrings.get(&actor.process_owner) {
            keyrings.push(ResolvedKey::possessed(*id));
        }
        if let Some(id) = self.session_keyrings.get(&actor.process_owner) {
            keyrings.push(ResolvedKey::possessed(*id));
        } else if let Some(id) = self.user_session_keyrings.get(&actor.user_uid()) {
            keyrings.push(ResolvedKey::possessed(*id));
        }
        keyrings
    }

    fn search_current(
        &self,
        actor: &KeyActor,
        kind: KeyTypeKind,
        description: &str,
    ) -> AxResult<Option<ResolvedKey>> {
        let mut first_error = None;
        for keyring in self.current_search_keyrings(actor) {
            match self.search_keyring(keyring, actor, kind, description, &mut BTreeSet::new()) {
                Ok(Some(serial)) => return Ok(Some(serial)),
                Ok(None) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(None)
        }
    }

    fn link_existing_request_result(
        &mut self,
        dest: i32,
        key: impl Into<ResolvedKey>,
        actor: &KeyActor,
    ) -> AxResult<()> {
        if dest == 0 {
            return Ok(());
        }
        let destination = self.resolve_keyring(dest, actor, true)?;
        self.link_existing_key(destination, key, actor, false)
    }

    fn usage_for_uid(&self, uid: Kuid) -> KeyUsage {
        let mut usage = KeyUsage::default();
        for key in self.keys.values().filter(|key| key.uid == uid) {
            usage.keys += 1;
            usage.bytes += key.charge();
        }
        usage
    }

    fn quota_allows_new(&self, uid: Kuid, charge: usize) -> bool {
        let usage = self.usage_for_uid(uid);
        usage.keys < user_maxkeys(uid) && usage.bytes.saturating_add(charge) <= user_maxbytes(uid)
    }

    fn quota_allows_resize(&self, uid: Kuid, old_charge: usize, new_charge: usize) -> bool {
        if new_charge <= old_charge {
            return true;
        }
        let usage = self.usage_for_uid(uid);
        usage.bytes.saturating_add(new_charge - old_charge) <= user_maxbytes(uid)
    }

    fn replace_payload(&mut self, serial: i32, payload: Vec<u8>) -> AxResult<()> {
        let (kind, uid, old_charge, old_payload_len) = self
            .keys
            .get(&serial)
            .map(|key| (key.kind, key.uid, key.charge(), key.payload.len()))
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if !kind.supports_payload_update() {
            return Err(LinuxError::EOPNOTSUPP.into());
        }
        if payload.len() > kind.payload_limit() {
            return Err(AxError::InvalidInput);
        }
        let new_charge = old_charge
            .checked_sub(old_payload_len)
            .and_then(|charge| charge.checked_add(payload.len()))
            .ok_or(AxError::NoMemory)?;
        if !self.quota_allows_resize(uid, old_charge, new_charge) {
            return Err(LinuxError::EDQUOT.into());
        }
        self.keys
            .get_mut(&serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?
            .payload = payload;
        Ok(())
    }
}

#[derive(Default)]
struct KeyUsage {
    keys: usize,
    bytes: usize,
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
        destination: Option<i32>,
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

pub(crate) fn add_key(
    actor: &KeyActor,
    kind: KeyTypeKind,
    description: String,
    payload: Vec<u8>,
    keyring: i32,
) -> AxResult<isize> {
    let mut manager = KEY_MANAGER.lock();
    let keyring = manager.resolve_keyring(keyring, actor, true)?;
    if !manager.keyring_has_write(keyring, actor)? {
        return Err(LinuxError::EACCES.into());
    }
    if kind.supports_payload_update()
        && let Some(serial) = manager.find_linked_key(keyring.serial, kind, &description)
    {
        if !manager.key_has_perm(serial, actor, KeyPermission::WRITE)? {
            return Err(LinuxError::EACCES.into());
        }
        manager.replace_payload(serial, payload)?;
        return Ok(serial as isize);
    }

    manager.check_link_destination(keyring.serial)?;
    let key = Key::positive(
        kind,
        description,
        payload,
        actor.owner_uid(),
        actor.owner_gid(),
    );
    let serial = manager.try_insert_key(key, true)?;
    if let Err(error) = manager.link_key_replace(keyring.serial, serial) {
        manager.remove_key_everywhere(serial);
        return Err(error);
    }
    Ok(serial as isize)
}

pub(crate) fn request_key(
    actor: &KeyActor,
    kind: KeyTypeKind,
    description: &str,
    callout_present: bool,
    dest_keyring: i32,
) -> AxResult<isize> {
    let mut manager = KEY_MANAGER.lock();
    if let Some(resolved) = manager.search_current(actor, kind, description)? {
        let key = manager
            .keys
            .get(&resolved.serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        manager.check_key_available(key, true)?;
        manager.link_existing_request_result(dest_keyring, resolved, actor)?;
        return Ok(resolved.serial as isize);
    }

    if callout_present {
        // Construction requires a target-bound, one-shot request authority and
        // a userspace upcall. Neither exists yet, so do not fabricate a key or
        // a permanent negative cache entry.
        Err(LinuxError::EOPNOTSUPP.into())
    } else {
        Err(LinuxError::ENOKEY.into())
    }
}

pub(crate) fn keyctl(actor: &KeyActor, command: KeyctlCommand) -> AxResult<KeyctlOutput> {
    let mut manager = KEY_MANAGER.lock();
    let value = match command {
        KeyctlCommand::GetKeyringId { keyring, create } => {
            let serial = manager.resolve_keyring(keyring, actor, create)?;
            if !manager.key_has_perm(serial, actor, KeyPermission::SEARCH)? {
                return Err(LinuxError::EACCES.into());
            }
            serial.serial as isize
        }
        KeyctlCommand::JoinSession { name } => {
            let serial = if let Some(name) = name.as_deref() {
                if name.starts_with('.') {
                    return Err(AxError::OperationNotPermitted);
                }
                let existing = manager.keys.iter().find_map(|(serial, key)| {
                    (key.is_keyring()
                        && key.description == name
                        && manager
                            .key_has_perm(*serial, actor, KeyPermission::SEARCH)
                            .unwrap_or(false))
                    .then_some(*serial)
                });
                if let Some(serial) = existing {
                    serial
                } else {
                    manager.try_create_keyring(
                        name.to_string(),
                        actor.owner_uid(),
                        actor.owner_gid(),
                        named_session_keyring_permissions(),
                        true,
                    )?
                }
            } else {
                manager.try_create_keyring(
                    format!("_ses.{}", actor.pid),
                    actor.owner_uid(),
                    actor.owner_gid(),
                    anonymous_session_keyring_permissions(),
                    true,
                )?
            };
            manager.session_keyrings.insert(actor.process_owner, serial);
            serial as isize
        }
        KeyctlCommand::Update { key, payload } => {
            let serial = manager.resolve_key(key, actor, false)?;
            if !manager.key_has_perm(serial, actor, KeyPermission::WRITE)? {
                return Err(LinuxError::EACCES.into());
            }
            manager.replace_payload(serial.serial, payload)?;
            0
        }
        KeyctlCommand::Revoke { key } => {
            let serial = manager.resolve_key(key, actor, false)?;
            let can_write = manager.key_has_perm(serial, actor, KeyPermission::WRITE)?;
            let can_setattr = manager.key_has_perm(serial, actor, KeyPermission::SETATTR)?;
            if !can_write && !can_setattr {
                return Err(LinuxError::EACCES.into());
            }
            manager
                .keys
                .get_mut(&serial.serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?
                .state = KeyState::Revoked;
            0
        }
        KeyctlCommand::Chown { key, uid, gid } => {
            let serial = manager.resolve_key(key, actor, false)?;
            if !manager.key_has_perm(serial, actor, KeyPermission::SETATTR)? {
                return Err(LinuxError::EACCES.into());
            }
            let (old_uid, old_gid, charge) = manager
                .keys
                .get(&serial.serial)
                .map(|key| (key.uid, key.gid, key.charge()))
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            let uid = uid.map(|uid| actor.map_user_uid(uid)).transpose()?;
            let gid = gid.map(|gid| actor.map_user_gid(gid)).transpose()?;
            if uid.is_some_and(|uid| uid != old_uid) && !actor.has_sys_admin {
                return Err(AxError::OperationNotPermitted);
            }
            if gid.is_some_and(|gid| gid != old_gid)
                && !actor.has_sys_admin
                && !gid.is_some_and(|gid| actor.in_group(gid))
            {
                return Err(AxError::OperationNotPermitted);
            }
            if let Some(uid) = uid
                && uid != old_uid
                && !manager.quota_allows_new(uid, charge)
            {
                return Err(LinuxError::EDQUOT.into());
            }
            let key = manager
                .keys
                .get_mut(&serial.serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            if let Some(uid) = uid {
                key.uid = uid;
            }
            if let Some(gid) = gid {
                key.gid = gid;
            }
            0
        }
        KeyctlCommand::SetPerm { key, permissions } => {
            let serial = manager.resolve_key(key, actor, false)?;
            if !manager.key_has_perm(serial, actor, KeyPermission::SETATTR)? {
                return Err(LinuxError::EACCES.into());
            }
            let owner = manager
                .keys
                .get(&serial.serial)
                .map(|key| key.uid)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            if !actor.has_sys_admin && owner != actor.owner_uid() {
                return Err(AxError::OperationNotPermitted);
            }
            manager
                .keys
                .get_mut(&serial.serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?
                .perm = permissions;
            0
        }
        KeyctlCommand::Describe { key } => {
            let serial = manager.resolve_key(key, actor, false)?;
            if !manager.key_has_perm(serial, actor, KeyPermission::VIEW)? {
                return Err(LinuxError::EACCES.into());
            }
            let key = manager
                .keys
                .get(&serial.serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            let description = format!(
                "{};{};{};{:08x};{}\0",
                key.kind.name(),
                actor.display_uid(key.uid),
                actor.display_gid(key.gid),
                key.perm.into_raw(),
                key.description
            )
            .into_bytes();
            return Ok(KeyctlOutput::CountedBytes(description));
        }
        KeyctlCommand::Clear { keyring } => {
            let keyring = manager.resolve_keyring(keyring, actor, false)?;
            if !manager.keyring_has_write(keyring, actor)? {
                return Err(LinuxError::EACCES.into());
            }
            manager
                .keys
                .get_mut(&keyring.serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?
                .links
                .clear();
            0
        }
        KeyctlCommand::Link { key, keyring } => {
            let serial = manager.resolve_key(key, actor, false)?;
            let keyring = manager.resolve_keyring(keyring, actor, true)?;
            manager.link_existing_key(keyring, serial, actor, false)?;
            0
        }
        KeyctlCommand::Unlink { serial, keyring } => {
            let keyring = manager.resolve_keyring(keyring, actor, false)?;
            if !manager.keyring_has_write(keyring, actor)? {
                return Err(LinuxError::EACCES.into());
            }
            manager.unlink_key_from_keyring(keyring.serial, serial)?;
            0
        }
        KeyctlCommand::Search {
            keyring,
            type_name,
            description,
            destination,
        } => {
            let keyring = manager.resolve_keyring(keyring, actor, false)?;
            let kind = KeyTypeKind::from_name(&type_name).ok_or(AxError::NoSuchDevice)?;
            let serial = manager
                .search_keyring(keyring, actor, kind, &description, &mut BTreeSet::new())?
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            if let Some(destination) = destination {
                let dest = manager.resolve_keyring(destination, actor, true)?;
                manager.link_existing_key(dest, serial, actor, false)?;
            }
            serial.serial as isize
        }
        KeyctlCommand::Read { key, copy_limit } => {
            let serial = manager.resolve_key(key, actor, false)?;
            if !manager.key_has_perm(serial, actor, KeyPermission::READ)? {
                return Err(LinuxError::EACCES.into());
            }
            let key = manager
                .keys
                .get(&serial.serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            manager.check_key_available(key, true)?;
            if key.is_keyring() {
                return Ok(KeyctlOutput::KeyringIds(key.links.clone()));
            }
            if !key.kind.userspace_readable() {
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            let full_len = key.payload.len();
            let bytes = copy_limit
                .map(|limit| key.payload[..full_len.min(limit)].to_vec())
                .unwrap_or_default();
            return Ok(KeyctlOutput::Payload { full_len, bytes });
        }
        KeyctlCommand::SetReqKeyring { setting } => {
            let old_setting = manager
                .reqkey_defaults
                .get(&actor.process_owner)
                .copied()
                .unwrap_or(KEY_REQKEY_DEFL_DEFAULT);
            match setting {
                ReqKeyDefault::NoChange => return Ok(KeyctlOutput::Value(old_setting as isize)),
                ReqKeyDefault::Thread => {
                    manager.special_keyring(KEY_SPEC_THREAD_KEYRING, actor, true)?;
                    manager
                        .reqkey_defaults
                        .insert(actor.process_owner, setting as i32);
                }
                ReqKeyDefault::Process => {
                    manager.special_keyring(KEY_SPEC_PROCESS_KEYRING, actor, true)?;
                    manager
                        .reqkey_defaults
                        .insert(actor.process_owner, setting as i32);
                }
                ReqKeyDefault::Default => {
                    manager.reqkey_defaults.remove(&actor.process_owner);
                }
                ReqKeyDefault::Session | ReqKeyDefault::User | ReqKeyDefault::UserSession => {
                    manager
                        .reqkey_defaults
                        .insert(actor.process_owner, setting as i32);
                }
            }
            old_setting as isize
        }
        KeyctlCommand::SetTimeout { key, seconds } => {
            let serial = manager.resolve_key(key, actor, false)?;
            if !manager.key_has_perm(serial, actor, KeyPermission::SETATTR)? {
                return Err(LinuxError::EACCES.into());
            }
            manager
                .keys
                .get_mut(&serial.serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?
                .expires_at = (seconds != 0).then(|| wall_time().as_secs().saturating_add(seconds));
            0
        }
        KeyctlCommand::Invalidate { key } => {
            let serial = manager.resolve_key(key, actor, false)?;
            if !manager.key_has_perm(serial, actor, KeyPermission::SEARCH)? {
                return Err(LinuxError::EACCES.into());
            }
            manager.remove_key_everywhere(serial.serial);
            0
        }
        KeyctlCommand::GetPersistent { uid, destination } => {
            let uid = uid
                .map(|uid| actor.map_user_uid(uid))
                .transpose()?
                .unwrap_or(actor.ids.ruid);
            if uid != actor.ids.ruid && uid != actor.ids.euid && !actor.has_setuid {
                return Err(AxError::OperationNotPermitted);
            }
            let serial = manager.get_persistent_keyring(uid, actor)?;
            if let Some(destination) = destination {
                let dest = manager.resolve_keyring(destination, actor, true)?;
                manager.link_existing_key(dest, serial, actor, false)?;
            }
            serial as isize
        }
        KeyctlCommand::Restrict { keyring } => {
            let serial = manager.resolve_keyring(keyring, actor, false)?;
            if !manager.key_has_perm(serial, actor, KeyPermission::SETATTR)? {
                return Err(LinuxError::EACCES.into());
            }
            let key = manager
                .keys
                .get_mut(&serial.serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            if key.restricted {
                return Err(LinuxError::EEXIST.into());
            }
            key.restricted = true;
            0
        }
        KeyctlCommand::Move {
            key,
            from,
            to,
            exclusive,
        } => {
            let serial = manager.resolve_key(key, actor, false)?;
            let from = manager.resolve_keyring(from, actor, false)?;
            let to = manager.resolve_keyring(to, actor, true)?;
            if !manager.keyring_has_write(from, actor)?
                || !manager.keyring_has_write(to, actor)?
                || !manager.key_has_perm(serial, actor, KeyPermission::LINK)?
            {
                return Err(LinuxError::EACCES.into());
            }
            if from.serial == to.serial {
                return Ok(KeyctlOutput::Value(0));
            }
            manager.move_key_link(from.serial, to.serial, serial.serial, exclusive)?;
            0
        }
    };
    Ok(KeyctlOutput::Value(value))
}

fn user_maxkeys(uid: Kuid) -> usize {
    if uid == Kuid::INITIAL_ROOT {
        KEY_ROOT_MAXKEYS.load(Ordering::Relaxed)
    } else {
        KEY_MAXKEYS.load(Ordering::Relaxed)
    }
}

fn user_maxbytes(uid: Kuid) -> usize {
    if uid == Kuid::INITIAL_ROOT {
        KEY_ROOT_MAXBYTES.load(Ordering::Relaxed)
    } else {
        KEY_MAXBYTES.load(Ordering::Relaxed)
    }
}

pub(crate) fn key_users_snapshot() -> String {
    let manager = KEY_MANAGER.lock();
    let mut users = BTreeMap::<Kuid, KeyUsage>::new();
    for key in manager.keys.values() {
        let usage = users.entry(key.uid).or_default();
        usage.keys += 1;
        usage.bytes += key.charge();
    }

    let mut out = String::new();
    for (uid, usage) in users {
        let raw_uid = uid.into_raw();
        out.push_str(&format!(
            "{raw_uid:5}: {usage_ref:5} 0/0 {keys}/{max_keys} {bytes}/{max_bytes}\n",
            usage_ref = usage.keys,
            keys = usage.keys,
            max_keys = user_maxkeys(uid),
            bytes = usage.bytes,
            max_bytes = user_maxbytes(uid),
        ));
    }
    out
}

pub(crate) fn key_maxkeys() -> usize {
    KEY_MAXKEYS.load(Ordering::Relaxed)
}

pub(crate) fn set_key_maxkeys(value: usize) {
    KEY_MAXKEYS.store(value.max(1), Ordering::Relaxed);
}

pub(crate) fn key_maxbytes() -> usize {
    KEY_MAXBYTES.load(Ordering::Relaxed)
}

pub(crate) fn set_key_maxbytes(value: usize) {
    KEY_MAXBYTES.store(value.max(1), Ordering::Relaxed);
}

pub(crate) fn key_root_maxkeys() -> usize {
    KEY_ROOT_MAXKEYS.load(Ordering::Relaxed)
}

pub(crate) fn set_key_root_maxkeys(value: usize) {
    KEY_ROOT_MAXKEYS.store(value.max(1), Ordering::Relaxed);
}

pub(crate) fn key_root_maxbytes() -> usize {
    KEY_ROOT_MAXBYTES.load(Ordering::Relaxed)
}

pub(crate) fn set_key_root_maxbytes(value: usize) {
    KEY_ROOT_MAXBYTES.store(value.max(1), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use thekernel_linux_cred::{CAPABILITY_WORDS, GroupInfo};

    use super::*;

    fn actor(tid: u32, pid: u32, uid: u32, gid: u32) -> KeyActor {
        let uid = Kuid::from_raw(uid).unwrap();
        let gid = Kgid::from_raw(gid).unwrap();
        let user_ns = UserNamespace::try_new_root().unwrap();
        let groups = GroupInfo::try_new(Vec::new()).unwrap();
        KeyActor {
            tid,
            pid,
            thread_owner: tid,
            process_owner: pid,
            ids: Credentials {
                ruid: uid,
                euid: uid,
                suid: uid,
                fsuid: uid,
                rgid: gid,
                egid: gid,
                sgid: gid,
                fsgid: gid,
            },
            dac: DacCredentialView::new(uid, gid, groups, [0; CAPABILITY_WORDS], true),
            user_ns,
            has_sys_admin: false,
            has_setuid: false,
        }
    }

    #[test]
    fn guessed_serial_does_not_receive_possessor_permissions() {
        let owner = actor(1, 1, 1000, 1000);
        let outsider = actor(2, 2, 2000, 2000);
        let same_uid = actor(3, 3, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let serial = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "secret".to_string(),
            Vec::from([1, 2, 3]),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(root, serial).unwrap();

        assert!(
            manager
                .key_has_perm(serial, &owner, KeyPermission::READ)
                .unwrap()
        );
        assert!(
            !manager
                .key_has_perm(serial, &same_uid, KeyPermission::READ)
                .unwrap()
        );
        assert!(
            !manager
                .key_has_perm(serial, &outsider, KeyPermission::READ)
                .unwrap()
        );
    }

    #[test]
    fn possessor_lane_requires_a_searchable_path_to_the_exact_key() {
        let owner = actor(11, 11, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let serial = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "possessor-only".to_string(),
            Vec::new(),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.keys.get_mut(&serial).unwrap().perm =
            KeyPermissionMask::try_from_raw(0x0a00_0000).unwrap();
        manager.link_key_replace(root, serial).unwrap();

        assert!(manager.is_possessed(serial, &owner));
        assert!(
            manager
                .key_has_perm(serial, &owner, KeyPermission::READ)
                .unwrap()
        );

        manager.keys.get_mut(&serial).unwrap().perm =
            KeyPermissionMask::try_from_raw(0x0200_0000).unwrap();
        assert!(!manager.is_possessed(serial, &owner));
        assert!(
            !manager
                .key_has_perm(serial, &owner, KeyPermission::READ)
                .unwrap()
        );
    }

    #[test]
    fn keyring_cycles_are_rejected_and_traversal_depth_is_bounded() {
        let owner = actor(21, 21, 1000, 1000);
        let mut manager = KeyManager::new();

        let cycle_a = manager.create_keyring(
            "cycle-a".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let cycle_b = manager.create_keyring(
            "cycle-b".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        manager.link_key_replace(cycle_a, cycle_b).unwrap();
        assert_eq!(
            manager.link_key_replace(cycle_b, cycle_a),
            Err(LinuxError::EDEADLK.into())
        );

        let mut rings = Vec::new();
        for index in 0..=KEYRING_SEARCH_MAX_DEPTH + 1 {
            rings.push(manager.create_keyring(
                format!("ring-{index}"),
                owner.owner_uid(),
                owner.owner_gid(),
                thread_process_keyring_permissions(),
            ));
        }
        manager.thread_keyrings.insert(owner.thread_owner, rings[0]);
        for pair in rings.windows(2) {
            manager.link_key_replace(pair[0], pair[1]).unwrap();
        }
        let beyond_search_depth = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "beyond-search-depth".to_string(),
            Vec::new(),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager
            .link_key_replace(*rings.last().unwrap(), beyond_search_depth)
            .unwrap();

        assert!(manager.is_possessed(*rings.last().unwrap(), &owner));
        assert!(!manager.is_possessed(beyond_search_depth, &owner));
        assert!(
            manager
                .search_keyring(
                    rings[0],
                    &owner,
                    KeyTypeKind::User,
                    "beyond-search-depth",
                    &mut BTreeSet::new(),
                )
                .unwrap()
                .is_none()
        );
        let destination = manager.create_keyring(
            "depth-destination".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        assert!(manager.link_key_replace(destination, rings[0]) == Err(LinuxError::ELOOP.into()));
    }

    #[test]
    fn link_depth_counts_only_nested_keyrings() {
        let owner = actor(26, 26, 1000, 1000);
        let mut manager = KeyManager::new();
        let mut rings = Vec::new();
        for index in 0..=KEYRING_SEARCH_MAX_DEPTH {
            rings.push(manager.create_keyring(
                format!("bounded-ring-{index}"),
                owner.owner_uid(),
                owner.owner_gid(),
                thread_process_keyring_permissions(),
            ));
        }
        for pair in rings.windows(2) {
            manager.link_key_replace(pair[0], pair[1]).unwrap();
        }
        let ordinary_key = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "ordinary-leaf".to_string(),
            Vec::new(),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager
            .link_key_replace(*rings.last().unwrap(), ordinary_key)
            .unwrap();

        let valid_destination = manager.create_keyring(
            "valid-depth-destination".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        manager
            .link_key_replace(valid_destination, rings[0])
            .unwrap();

        let nested_keyring = manager.create_keyring(
            "one-keyring-too-deep".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        manager
            .link_key_replace(*rings.last().unwrap(), nested_keyring)
            .unwrap();
        let invalid_destination = manager.create_keyring(
            "invalid-depth-destination".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        assert_eq!(
            manager.link_key_replace(invalid_destination, rings[0]),
            Err(LinuxError::ELOOP.into())
        );
    }

    #[test]
    fn an_existing_request_result_links_only_to_an_explicit_destination() {
        let owner = actor(31, 31, 1000, 1000);
        let mut manager = KeyManager::new();
        let search_root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let destination = manager.create_keyring(
            "request-destination".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let serial = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "existing".to_string(),
            Vec::new(),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(search_root, destination).unwrap();
        manager.link_key_replace(search_root, serial).unwrap();

        manager
            .link_existing_request_result(0, serial, &owner)
            .unwrap();
        assert!(!manager.keys[&destination].links.contains(&serial));

        manager
            .link_existing_request_result(destination, serial, &owner)
            .unwrap();
        assert!(manager.keys[&destination].links.contains(&serial));
    }

    #[test]
    fn logon_payload_is_not_userspace_readable_even_when_possessed() {
        let owner = actor(41, 41, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let serial = manager.insert_key(Key::positive(
            KeyTypeKind::Logon,
            "service:secret".to_string(),
            Vec::from([1, 2, 3]),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(root, serial).unwrap();

        assert!(!KeyTypeKind::Logon.userspace_readable());
        assert!(manager.is_possessed(serial, &owner));
        assert!(
            !manager
                .key_has_perm(serial, &owner, KeyPermission::READ)
                .unwrap()
        );
    }

    #[test]
    fn visible_tid_rebinding_keeps_the_immutable_thread_owner() {
        let first = actor(51, 51, 1000, 1000);
        let mut manager = KeyManager::new();
        let thread_ring = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &first, true)
            .unwrap();

        let mut rebound = first.clone();
        rebound.tid = 1;
        assert_eq!(
            manager.special_keyring(KEY_SPEC_THREAD_KEYRING, &rebound, false),
            Ok(thread_ring)
        );
        assert!(manager.is_possessed(thread_ring, &rebound));
    }

    #[test]
    fn restriction_and_failed_move_preserve_the_source_link() {
        let owner = actor(61, 61, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let source = manager.create_keyring(
            "source".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let destination = manager.create_keyring(
            "destination".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let serial = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "movable".to_string(),
            Vec::new(),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(root, source).unwrap();
        manager.link_key_replace(root, destination).unwrap();
        manager.link_key_replace(source, serial).unwrap();
        manager.keys.get_mut(&destination).unwrap().restricted = true;

        assert_eq!(
            manager.move_key_link(source, destination, serial, false),
            Err(AxError::OperationNotPermitted)
        );
        assert!(manager.keys[&source].links.contains(&serial));
        assert!(!manager.keys[&destination].links.contains(&serial));
    }

    #[test]
    fn move_replaces_an_existing_match_without_growing_the_destination() {
        let owner = actor(66, 66, 1000, 1000);
        let mut manager = KeyManager::new();
        let source = manager.create_keyring(
            "source".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let destination = manager.create_keyring(
            "destination".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let moved = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "same-description".to_string(),
            Vec::from([1]),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        let replaced = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "same-description".to_string(),
            Vec::from([2]),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(source, moved).unwrap();
        manager.link_key_replace(destination, replaced).unwrap();
        let destination_len = manager.keys[&destination].links.len();

        manager
            .move_key_link(source, destination, moved, false)
            .unwrap();

        assert!(!manager.keys[&source].links.contains(&moved));
        assert_eq!(manager.keys[&destination].links.len(), destination_len);
        assert!(manager.keys[&destination].links.contains(&moved));
        assert!(!manager.keys[&destination].links.contains(&replaced));
    }

    #[test]
    fn payload_quota_failure_is_non_mutating() {
        let owner = actor(71, 71, 1000, 1000);
        let mut manager = KeyManager::new();
        let fixed_charge = KEY_CHARGE_OVERHEAD + "quota".len() + 1;
        let original_len = KEY_MAXBYTES_DEFAULT - fixed_charge - 1;
        let original = vec![0x5a; original_len];
        let serial = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "quota".to_string(),
            original.clone(),
            owner.owner_uid(),
            owner.owner_gid(),
        ));

        assert_eq!(
            manager.replace_payload(serial, vec![0xa5; original_len + 2]),
            Err(LinuxError::EDQUOT.into())
        );
        assert_eq!(manager.keys[&serial].payload, original);
    }

    #[test]
    fn link_quota_failure_is_non_mutating() {
        let owner = actor(73, 73, 1000, 1000);
        let mut manager = KeyManager::new();
        let destination = manager.create_keyring(
            "quota-ring".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let serial = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "link-target".to_string(),
            Vec::new(),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        let filler_fixed_charge = KEY_CHARGE_OVERHEAD + "filler".len() + 1;
        let current_charge = manager.usage_for_uid(owner.owner_uid()).bytes;
        let filler_len =
            KEY_MAXBYTES_DEFAULT - current_charge - filler_fixed_charge - (KEY_LINK_CHARGE - 1);
        manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "filler".to_string(),
            vec![0; filler_len],
            owner.owner_uid(),
            owner.owner_gid(),
        ));

        assert_eq!(
            manager.link_key_replace(destination, serial),
            Err(LinuxError::EDQUOT.into())
        );
        assert!(manager.keys[&destination].links.is_empty());
    }

    #[test]
    fn serial_exhaustion_is_explicit_and_never_overwrites_a_key() {
        let owner = actor(76, 76, 1000, 1000);
        let mut manager = KeyManager::new();
        manager.next_serial = Some(i32::MAX);
        let first = manager
            .try_insert_key(
                Key::positive(
                    KeyTypeKind::User,
                    "last".to_string(),
                    Vec::new(),
                    owner.owner_uid(),
                    owner.owner_gid(),
                ),
                false,
            )
            .unwrap();
        assert_eq!(first, i32::MAX);

        let before = manager.keys.len();
        assert_eq!(
            manager.try_insert_key(
                Key::positive(
                    KeyTypeKind::User,
                    "overflow".to_string(),
                    Vec::new(),
                    owner.owner_uid(),
                    owner.owner_gid(),
                ),
                false,
            ),
            Err(LinuxError::ENOSPC.into())
        );
        assert_eq!(manager.keys.len(), before);
        assert_eq!(manager.keys[&first].description, "last");
    }

    #[test]
    fn search_is_breadth_first_across_keyring_links() {
        let owner = actor(81, 81, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let branch = manager.create_keyring(
            "deep-branch".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let shallow_branch = manager.create_keyring(
            "shallow-branch".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let deeper = manager.create_keyring(
            "deeper".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let deep_match = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "target".to_string(),
            Vec::from([1]),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        let shallow_match = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "target".to_string(),
            Vec::from([2]),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(root, branch).unwrap();
        manager.link_key_replace(root, shallow_branch).unwrap();
        manager.link_key_replace(branch, deeper).unwrap();
        manager.link_key_replace(deeper, deep_match).unwrap();
        manager
            .link_key_replace(shallow_branch, shallow_match)
            .unwrap();

        assert_eq!(
            manager
                .search_keyring(
                    root,
                    &owner,
                    KeyTypeKind::User,
                    "target",
                    &mut BTreeSet::new(),
                )
                .unwrap()
                .map(|key| key.serial),
            Some(shallow_match)
        );
    }

    #[test]
    fn revoked_nested_keyring_does_not_poison_an_unrelated_miss() {
        let owner = actor(91, 91, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let revoked_child = manager.create_keyring(
            "revoked-child".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        manager.link_key_replace(root, revoked_child).unwrap();
        manager.keys.get_mut(&revoked_child).unwrap().state = KeyState::Revoked;

        assert_eq!(
            manager.search_keyring(
                root,
                &owner,
                KeyTypeKind::User,
                "missing",
                &mut BTreeSet::new(),
            ),
            Ok(None)
        );
    }

    #[test]
    fn inaccessible_match_reports_eacces_but_does_not_hide_a_later_match() {
        let owner = actor(101, 101, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let denied = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "permission-target".to_string(),
            Vec::from([1]),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.keys.get_mut(&denied).unwrap().perm = KeyPermissionMask::try_from_raw(0).unwrap();
        manager.link_key_replace(root, denied).unwrap();

        assert_eq!(
            manager.search_keyring(
                root,
                &owner,
                KeyTypeKind::User,
                "permission-target",
                &mut BTreeSet::new(),
            ),
            Err(LinuxError::EACCES.into())
        );

        let branch = manager.create_keyring(
            "valid-branch".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let allowed = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "permission-target".to_string(),
            Vec::from([2]),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(root, branch).unwrap();
        manager.link_key_replace(branch, allowed).unwrap();

        assert_eq!(
            manager.search_keyring(
                root,
                &owner,
                KeyTypeKind::User,
                "permission-target",
                &mut BTreeSet::new(),
            ),
            Ok(Some(ResolvedKey::possessed(allowed)))
        );
    }

    #[test]
    fn basal_keyring_is_considered_before_its_children() {
        let owner = actor(111, 111, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();

        assert_eq!(
            manager.search_keyring(
                ResolvedKey::possessed(root),
                &owner,
                KeyTypeKind::Keyring,
                "_tid.111",
                &mut BTreeSet::new(),
            ),
            Ok(Some(ResolvedKey::possessed(root)))
        );
    }

    #[test]
    fn direct_special_lookup_does_not_grant_numeric_possession() {
        let mut shifted = actor(121, 121, 1000, 1000);
        let groups = GroupInfo::try_new(Vec::new()).unwrap();
        shifted.dac = DacCredentialView::new(
            Kuid::INITIAL_ROOT,
            Kgid::INITIAL_ROOT,
            groups,
            [0; CAPABILITY_WORDS],
            true,
        );
        let mut manager = KeyManager::new();
        let direct = manager
            .resolve_keyring(KEY_SPEC_USER_KEYRING, &shifted, true)
            .unwrap();

        assert!(
            manager
                .key_has_perm(direct, &shifted, KeyPermission::WRITE)
                .unwrap()
        );
        assert!(
            !manager
                .key_has_perm(
                    ResolvedKey::numeric(direct.serial),
                    &shifted,
                    KeyPermission::WRITE,
                )
                .unwrap()
        );
    }

    #[test]
    fn explicit_unpossessed_search_does_not_borrow_an_independent_possession() {
        let owner = actor(131, 131, 1000, 1000);
        let mut manager = KeyManager::new();
        let credential_root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let independently_possessed = manager.create_keyring(
            "independently-possessed".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            permission_mask(KeyPermission::SEARCH, KeyPermission::VIEW),
        );
        manager
            .link_key_replace(credential_root, independently_possessed)
            .unwrap();

        let explicit_source = manager.create_keyring(
            "explicit-source".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            permission_mask(KeyPermission::SEARCH, KeyPermission::SEARCH),
        );
        manager
            .link_key_replace(explicit_source, independently_possessed)
            .unwrap();

        assert_eq!(
            manager.search_keyring(
                ResolvedKey::with_possession(explicit_source, false),
                &owner,
                KeyTypeKind::Keyring,
                "independently-possessed",
                &mut BTreeSet::new(),
            ),
            Err(LinuxError::EACCES.into())
        );
    }

    #[test]
    fn encrypted_key_type_is_not_advertised_without_a_real_type_backend() {
        assert_eq!(KeyTypeKind::from_name("encrypted"), None);
    }
}
