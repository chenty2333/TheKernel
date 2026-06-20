use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::{
    ffi::c_char,
    mem::size_of,
    str,
    sync::atomic::{AtomicUsize, Ordering},
};

use axerrno::{AxError, AxResult, LinuxError};
use axsync::Mutex;
use axtask::current;
use starry_vm::{vm_load, vm_write_slice};

use crate::{mm::vm_load_string, task::AsThread, time::wall_time};

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

const KEYCTL_GET_KEYRING_ID: i32 = 0;
const KEYCTL_JOIN_SESSION_KEYRING: i32 = 1;
const KEYCTL_UPDATE: i32 = 2;
const KEYCTL_REVOKE: i32 = 3;
const KEYCTL_CHOWN: i32 = 4;
const KEYCTL_SETPERM: i32 = 5;
const KEYCTL_DESCRIBE: i32 = 6;
const KEYCTL_CLEAR: i32 = 7;
const KEYCTL_LINK: i32 = 8;
const KEYCTL_UNLINK: i32 = 9;
const KEYCTL_SEARCH: i32 = 10;
const KEYCTL_READ: i32 = 11;
const KEYCTL_INSTANTIATE: i32 = 12;
const KEYCTL_NEGATE: i32 = 13;
const KEYCTL_SET_REQKEY_KEYRING: i32 = 14;
const KEYCTL_SET_TIMEOUT: i32 = 15;
const KEYCTL_GET_SECURITY: i32 = 17;
const KEYCTL_REJECT: i32 = 19;
const KEYCTL_INVALIDATE: i32 = 21;
const KEYCTL_GET_PERSISTENT: i32 = 22;
const KEYCTL_RESTRICT_KEYRING: i32 = 29;
const KEYCTL_MOVE: i32 = 30;
const KEYCTL_CAPABILITIES: i32 = 31;

const KEY_POS_VIEW: u32 = 0x0100_0000;
const KEY_POS_READ: u32 = 0x0200_0000;
const KEY_POS_WRITE: u32 = 0x0400_0000;
const KEY_POS_SEARCH: u32 = 0x0800_0000;
const KEY_POS_LINK: u32 = 0x1000_0000;
const KEY_POS_SETATTR: u32 = 0x2000_0000;
const KEY_POS_ALL: u32 = 0x3f00_0000;
const KEY_USR_ALL: u32 = 0x003f_0000;
const KEY_GRP_ALL: u32 = 0x0000_3f00;
const KEY_OTH_ALL: u32 = 0x0000_003f;
const KEY_PERM_MASK: u32 = KEY_POS_ALL | KEY_USR_ALL | KEY_GRP_ALL | KEY_OTH_ALL;
const KEY_DEFAULT_PERM: u32 = KEY_POS_ALL | KEY_USR_ALL;
const KEYCTL_MOVE_EXCL: u32 = 0x0000_0001;
const KEYCTL_CAPS0_CAPABILITIES: u8 = 0x01;
const KEYCTL_CAPS0_PERSISTENT_KEYRINGS: u8 = 0x02;
const KEYCTL_CAPS0_BIG_KEY: u8 = 0x10;
const KEYCTL_CAPS0_INVALIDATE: u8 = 0x20;
const KEYCTL_CAPS0_RESTRICT_KEYRING: u8 = 0x40;
const KEYCTL_CAPS0_MOVE: u8 = 0x80;
const KEYCTL_CAPABILITIES_BYTES: [u8; 2] = [
    KEYCTL_CAPS0_CAPABILITIES
        | KEYCTL_CAPS0_PERSISTENT_KEYRINGS
        | KEYCTL_CAPS0_BIG_KEY
        | KEYCTL_CAPS0_INVALIDATE
        | KEYCTL_CAPS0_RESTRICT_KEYRING
        | KEYCTL_CAPS0_MOVE,
    0,
];

const USER_KEY_PAYLOAD_MAX: usize = 32_767;
const BIG_KEY_PAYLOAD_MAX: usize = (1 << 20) - 1;
const KEY_CHARGE_OVERHEAD: usize = 32;
const KEY_MAXKEYS_DEFAULT: usize = 200;
const KEY_MAXBYTES_DEFAULT: usize = 20_000;
const KEY_ROOT_MAXKEYS_DEFAULT: usize = 1_000_000;
const KEY_ROOT_MAXBYTES_DEFAULT: usize = 25_000_000;

static KEY_MANAGER: Mutex<KeyManager> = Mutex::new(KeyManager::new());
static KEY_MAXKEYS: AtomicUsize = AtomicUsize::new(KEY_MAXKEYS_DEFAULT);
static KEY_MAXBYTES: AtomicUsize = AtomicUsize::new(KEY_MAXBYTES_DEFAULT);
static KEY_ROOT_MAXKEYS: AtomicUsize = AtomicUsize::new(KEY_ROOT_MAXKEYS_DEFAULT);
static KEY_ROOT_MAXBYTES: AtomicUsize = AtomicUsize::new(KEY_ROOT_MAXBYTES_DEFAULT);
static KEY_GC_DELAY: AtomicUsize = AtomicUsize::new(300);

#[derive(Clone, Copy, Eq, PartialEq)]
enum KeyState {
    Positive,
    Negative,
    Revoked,
}

struct Key {
    type_name: String,
    description: String,
    payload: Vec<u8>,
    links: Vec<i32>,
    uid: u32,
    gid: u32,
    perm: u32,
    state: KeyState,
    expires_at: Option<u64>,
    restricted: bool,
}

impl Key {
    fn keyring(description: String, uid: u32, gid: u32) -> Self {
        Self {
            type_name: "keyring".to_string(),
            description,
            payload: Vec::new(),
            links: Vec::new(),
            uid,
            gid,
            perm: KEY_DEFAULT_PERM,
            state: KeyState::Positive,
            expires_at: None,
            restricted: false,
        }
    }

    fn positive(
        type_name: String,
        description: String,
        payload: Vec<u8>,
        uid: u32,
        gid: u32,
    ) -> Self {
        Self {
            type_name,
            description,
            payload,
            links: Vec::new(),
            uid,
            gid,
            perm: KEY_DEFAULT_PERM,
            state: KeyState::Positive,
            expires_at: None,
            restricted: false,
        }
    }

    fn negative(type_name: String, description: String, uid: u32, gid: u32) -> Self {
        Self {
            type_name,
            description,
            payload: Vec::new(),
            links: Vec::new(),
            uid,
            gid,
            perm: KEY_DEFAULT_PERM,
            state: KeyState::Negative,
            expires_at: None,
            restricted: false,
        }
    }

    fn is_keyring(&self) -> bool {
        self.type_name == "keyring"
    }

    fn charge(&self) -> usize {
        KEY_CHARGE_OVERHEAD + self.description.len() + 1 + self.payload.len()
    }
}

#[derive(Clone, Copy)]
struct CurrentKeyIds {
    tid: u32,
    pid: u32,
    uid: u32,
    gid: u32,
}

struct KeyManager {
    next_serial: i32,
    keys: BTreeMap<i32, Key>,
    thread_keyrings: BTreeMap<u32, i32>,
    process_keyrings: BTreeMap<u32, i32>,
    session_keyrings: BTreeMap<u32, i32>,
    user_keyrings: BTreeMap<u32, i32>,
    user_session_keyrings: BTreeMap<u32, i32>,
    persistent_keyrings: BTreeMap<u32, i32>,
    reqkey_defaults: BTreeMap<u32, i32>,
}

impl KeyManager {
    const fn new() -> Self {
        Self {
            next_serial: 1,
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

    fn alloc_serial(&mut self) -> i32 {
        let serial = self.next_serial;
        self.next_serial = self.next_serial.saturating_add(1).max(1);
        serial
    }

    fn insert_key(&mut self, key: Key) -> i32 {
        let serial = self.alloc_serial();
        self.keys.insert(serial, key);
        serial
    }

    fn create_keyring(&mut self, description: String, uid: u32, gid: u32) -> i32 {
        self.insert_key(Key::keyring(description, uid, gid))
    }

    fn special_keyring(&mut self, spec: i32, ids: CurrentKeyIds, create: bool) -> AxResult<i32> {
        match spec {
            KEY_SPEC_THREAD_KEYRING => {
                if let Some(id) = self.thread_keyrings.get(&ids.tid) {
                    return Ok(*id);
                }
                if !create {
                    return Err(LinuxError::ENOKEY.into());
                }
                let id = self.create_keyring(format!("_tid.{}", ids.tid), ids.uid, ids.gid);
                self.thread_keyrings.insert(ids.tid, id);
                Ok(id)
            }
            KEY_SPEC_PROCESS_KEYRING => {
                if let Some(id) = self.process_keyrings.get(&ids.pid) {
                    return Ok(*id);
                }
                if !create {
                    return Err(LinuxError::ENOKEY.into());
                }
                let id = self.create_keyring(format!("_pid.{}", ids.pid), ids.uid, ids.gid);
                self.process_keyrings.insert(ids.pid, id);
                Ok(id)
            }
            KEY_SPEC_SESSION_KEYRING => {
                if let Some(id) = self.session_keyrings.get(&ids.pid) {
                    return Ok(*id);
                }
                if !create {
                    return Err(LinuxError::ENOKEY.into());
                }
                let id = self.create_keyring(format!("_ses.{}", ids.pid), ids.uid, ids.gid);
                self.session_keyrings.insert(ids.pid, id);
                Ok(id)
            }
            KEY_SPEC_USER_KEYRING => {
                if let Some(id) = self.user_keyrings.get(&ids.uid) {
                    return Ok(*id);
                }
                let id = self.create_keyring(format!("_uid.{}", ids.uid), ids.uid, ids.gid);
                self.user_keyrings.insert(ids.uid, id);
                Ok(id)
            }
            KEY_SPEC_USER_SESSION_KEYRING => {
                if let Some(id) = self.user_session_keyrings.get(&ids.uid) {
                    return Ok(*id);
                }
                let id = self.create_keyring(format!("_uid_ses.{}", ids.uid), ids.uid, ids.gid);
                self.user_session_keyrings.insert(ids.uid, id);
                Ok(id)
            }
            _ => Err(LinuxError::ENOKEY.into()),
        }
    }

    fn resolve_keyring(&mut self, id: i32, ids: CurrentKeyIds, create: bool) -> AxResult<i32> {
        if id < 0 {
            return self.special_keyring(id, ids, create);
        }
        let key = self
            .keys
            .get(&id)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        self.check_key_available(key, true)?;
        if key.is_keyring() {
            Ok(id)
        } else {
            Err(AxError::InvalidInput)
        }
    }

    fn resolve_key(&mut self, id: i32, ids: CurrentKeyIds, create_special: bool) -> AxResult<i32> {
        let serial = if id < 0 {
            self.special_keyring(id, ids, create_special)?
        } else {
            id
        };
        let key = self
            .keys
            .get(&serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        self.check_key_available(key, true)?;
        Ok(serial)
    }

    fn check_key_available(&self, key: &Key, allow_keyring: bool) -> AxResult<()> {
        match key.state {
            KeyState::Revoked => return Err(LinuxError::EKEYREVOKED.into()),
            KeyState::Negative => return Err(LinuxError::ENOKEY.into()),
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

    fn has_perm(&self, key: &Key, perm: u32) -> bool {
        key.perm & perm != 0
    }

    fn keyring_has_write(&self, keyring: i32) -> AxResult<bool> {
        self.keyring_has_perm(keyring, KEY_POS_WRITE)
    }

    fn keyring_has_perm(&self, keyring: i32, perm: u32) -> AxResult<bool> {
        let key = self
            .keys
            .get(&keyring)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        self.check_key_available(key, true)?;
        Ok(key.is_keyring() && self.has_perm(key, perm))
    }

    fn find_linked_key(&self, keyring: i32, type_name: &str, description: &str) -> Option<i32> {
        let keyring = self.keys.get(&keyring)?;
        keyring.links.iter().copied().find(|serial| {
            self.keys
                .get(serial)
                .is_some_and(|key| key.type_name == type_name && key.description == description)
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
        let (type_name, description) = {
            let key = self
                .keys
                .get(&serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            (key.type_name.clone(), key.description.clone())
        };
        if let Some(existing) = self.find_linked_key(keyring, &type_name, &description) {
            self.unlink_key_from_keyring(keyring, existing)?;
        }
        let keyring_key = self
            .keys
            .get_mut(&keyring)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if !keyring_key.links.contains(&serial) {
            keyring_key.links.push(serial);
        }
        Ok(())
    }

    fn link_existing_key(&mut self, keyring: i32, serial: i32, exclusive: bool) -> AxResult<()> {
        let key = self
            .keys
            .get(&serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        self.check_key_available(key, true)?;
        if !self.has_perm(key, KEY_POS_LINK) {
            return Err(LinuxError::EACCES.into());
        }
        let (type_name, description) = (key.type_name.clone(), key.description.clone());
        if !self.keyring_has_write(keyring)? {
            return Err(LinuxError::EACCES.into());
        }
        if exclusive
            && self
                .find_linked_key(keyring, &type_name, &description)
                .is_some()
        {
            return Err(LinuxError::EEXIST.into());
        }
        self.link_key_replace(keyring, serial)
    }

    fn search_keyring(
        &self,
        keyring: i32,
        type_name: &str,
        description: &str,
        visited: &mut Vec<i32>,
    ) -> AxResult<Option<i32>> {
        if visited.contains(&keyring) {
            return Ok(None);
        }
        visited.push(keyring);

        let ring = self
            .keys
            .get(&keyring)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        self.check_key_available(ring, true)?;
        if !ring.is_keyring() {
            return Err(AxError::InvalidInput);
        }
        if !self.has_perm(ring, KEY_POS_SEARCH) {
            return Err(LinuxError::EACCES.into());
        }

        for serial in &ring.links {
            if let Some(key) = self.keys.get(serial)
                && key.type_name == type_name
                && key.description == description
            {
                self.check_key_available(key, true)?;
                if !self.has_perm(key, KEY_POS_SEARCH) {
                    return Err(LinuxError::EACCES.into());
                }
                return Ok(Some(*serial));
            }
        }

        for serial in &ring.links {
            if let Some(key) = self.keys.get(serial)
                && key.is_keyring()
                && let Some(found) =
                    self.search_keyring(*serial, type_name, description, visited)?
            {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    fn get_persistent_keyring(&mut self, uid: u32, ids: CurrentKeyIds) -> i32 {
        if let Some(id) = self.persistent_keyrings.get(&uid) {
            return *id;
        }
        let id = self.create_keyring(format!("_persistent.{}", uid), uid, ids.gid);
        self.persistent_keyrings.insert(uid, id);
        id
    }

    fn current_search_keyrings(&mut self, ids: CurrentKeyIds) -> Vec<i32> {
        let mut keyrings = Vec::new();
        if let Some(id) = self.thread_keyrings.get(&ids.tid) {
            keyrings.push(*id);
        }
        if let Some(id) = self.process_keyrings.get(&ids.pid) {
            keyrings.push(*id);
        }
        if let Some(id) = self.session_keyrings.get(&ids.pid) {
            keyrings.push(*id);
        }
        if let Ok(id) = self.special_keyring(KEY_SPEC_USER_KEYRING, ids, true) {
            keyrings.push(id);
        }
        if let Ok(id) = self.special_keyring(KEY_SPEC_USER_SESSION_KEYRING, ids, true) {
            keyrings.push(id);
        }
        keyrings
    }

    fn search_current(
        &mut self,
        ids: CurrentKeyIds,
        type_name: &str,
        description: &str,
    ) -> Option<i32> {
        for keyring in self.current_search_keyrings(ids) {
            if let Some(serial) = self.find_linked_key(keyring, type_name, description) {
                return Some(serial);
            }
        }
        None
    }

    fn resolve_reqkey_default_destination(
        &mut self,
        target: i32,
        ids: CurrentKeyIds,
    ) -> AxResult<i32> {
        match target {
            KEY_REQKEY_DEFL_DEFAULT | KEY_REQKEY_DEFL_THREAD_KEYRING => {
                if let Some(id) = self.thread_keyrings.get(&ids.tid) {
                    return Ok(*id);
                }
                if let Some(id) = self.process_keyrings.get(&ids.pid) {
                    return Ok(*id);
                }
                if let Some(id) = self.session_keyrings.get(&ids.pid) {
                    return Ok(*id);
                }
                self.special_keyring(KEY_SPEC_USER_SESSION_KEYRING, ids, true)
            }
            KEY_REQKEY_DEFL_PROCESS_KEYRING => {
                if let Some(id) = self.process_keyrings.get(&ids.pid) {
                    return Ok(*id);
                }
                if let Some(id) = self.session_keyrings.get(&ids.pid) {
                    return Ok(*id);
                }
                self.special_keyring(KEY_SPEC_USER_SESSION_KEYRING, ids, true)
            }
            KEY_REQKEY_DEFL_SESSION_KEYRING => {
                if let Some(id) = self.session_keyrings.get(&ids.pid) {
                    return Ok(*id);
                }
                self.special_keyring(KEY_SPEC_USER_SESSION_KEYRING, ids, true)
            }
            KEY_REQKEY_DEFL_USER_KEYRING => self.special_keyring(KEY_SPEC_USER_KEYRING, ids, true),
            KEY_REQKEY_DEFL_USER_SESSION_KEYRING => {
                self.special_keyring(KEY_SPEC_USER_SESSION_KEYRING, ids, true)
            }
            _ => Err(AxError::InvalidInput),
        }
    }

    fn resolve_reqkey_destination(&mut self, dest: i32, ids: CurrentKeyIds) -> AxResult<i32> {
        if dest == KEY_REQKEY_DEFL_DEFAULT {
            let target = self
                .reqkey_defaults
                .get(&ids.pid)
                .copied()
                .unwrap_or(KEY_REQKEY_DEFL_DEFAULT);
            return self.resolve_reqkey_default_destination(target, ids);
        }
        self.resolve_keyring(dest, ids, true)
    }

    fn usage_for_uid(&self, uid: u32) -> KeyUsage {
        let mut usage = KeyUsage::default();
        for key in self.keys.values().filter(|key| key.uid == uid) {
            usage.keys += 1;
            usage.bytes += key.charge();
        }
        usage
    }

    fn quota_allows(&self, uid: u32, charge: usize) -> bool {
        let usage = self.usage_for_uid(uid);
        usage.keys < user_maxkeys(uid) && usage.bytes.saturating_add(charge) <= user_maxbytes(uid)
    }
}

#[derive(Default)]
struct KeyUsage {
    keys: usize,
    bytes: usize,
}

fn current_key_ids() -> CurrentKeyIds {
    let curr = current();
    let thread = curr.as_thread();
    CurrentKeyIds {
        tid: thread.tid(),
        pid: thread.proc_data.proc.pid(),
        uid: thread.proc_data.euid(),
        gid: thread.proc_data.egid(),
    }
}

fn validate_key_payload(
    type_name: &str,
    description: &str,
    payload: *const u8,
    plen: usize,
) -> AxResult<Vec<u8>> {
    match type_name {
        "keyring" => {
            if plen != 0 {
                return Err(AxError::InvalidInput);
            }
            Ok(Vec::new())
        }
        "user" | "logon" => {
            if plen > USER_KEY_PAYLOAD_MAX {
                return Err(AxError::InvalidInput);
            }
            if type_name == "logon" && !description.contains(':') {
                return Err(AxError::InvalidInput);
            }
            load_payload(payload, plen)
        }
        "big_key" => {
            if plen > BIG_KEY_PAYLOAD_MAX {
                return Err(AxError::InvalidInput);
            }
            load_payload(payload, plen)
        }
        "encrypted" => validate_encrypted_payload(payload, plen),
        _ => Err(AxError::NoSuchDevice),
    }
}

fn validate_encrypted_payload(payload: *const u8, plen: usize) -> AxResult<Vec<u8>> {
    let payload = load_payload(payload, plen)?;
    let text = str::from_utf8(&payload).map_err(|_| AxError::InvalidInput)?;
    let parts = text.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 5 || parts[0] != "new" || !parts[2].starts_with("user:") {
        return Err(AxError::InvalidInput);
    }
    let data_len = parts[3]
        .parse::<usize>()
        .map_err(|_| AxError::InvalidInput)?;
    let hex = parts[4].as_bytes();
    if hex.len() != data_len.saturating_mul(2) || !hex.iter().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AxError::InvalidInput);
    }
    Ok(payload)
}

fn load_payload(payload: *const u8, plen: usize) -> AxResult<Vec<u8>> {
    if plen == 0 {
        return Ok(Vec::new());
    }
    if payload.is_null() {
        return Err(AxError::BadAddress);
    }
    Ok(vm_load(payload, plen)?)
}

fn user_maxkeys(uid: u32) -> usize {
    if uid == 0 {
        KEY_ROOT_MAXKEYS.load(Ordering::Relaxed)
    } else {
        KEY_MAXKEYS.load(Ordering::Relaxed)
    }
}

fn user_maxbytes(uid: u32) -> usize {
    if uid == 0 {
        KEY_ROOT_MAXBYTES.load(Ordering::Relaxed)
    } else {
        KEY_MAXBYTES.load(Ordering::Relaxed)
    }
}

fn write_keyring_ids(buf: *mut u8, size: usize, ids: &[i32]) -> AxResult<isize> {
    let full_size = ids.len() * size_of::<i32>();
    if size != 0 && !buf.is_null() {
        let mut bytes = Vec::new();
        for id in ids.iter().take(size / size_of::<i32>()) {
            bytes.extend_from_slice(&id.to_ne_bytes());
        }
        vm_write_slice(buf, &bytes[..bytes.len().min(size)])?;
    }
    Ok(full_size as isize)
}

fn write_counted_bytes_if_fits(buf: *mut u8, size: usize, bytes: &[u8]) -> AxResult<isize> {
    if !buf.is_null() && size >= bytes.len() {
        vm_write_slice(buf, bytes)?;
    }
    Ok(bytes.len() as isize)
}

fn write_counted_bytes_prefix(buf: *mut u8, size: usize, bytes: &[u8]) -> AxResult<isize> {
    if !buf.is_null() && size != 0 {
        vm_write_slice(buf, &bytes[..bytes.len().min(size)])?;
    }
    Ok(bytes.len() as isize)
}

fn key_security_label(key: &Key) -> Vec<u8> {
    let mut label = format!("key:{}:{}", key.type_name, key.uid).into_bytes();
    label.push(0);
    label
}

fn should_instantiate_request(type_name: &str, description: &str, callout: Option<&str>) -> bool {
    type_name == "user"
        && description.starts_with("debug:")
        && callout.is_some_and(|callout| !callout.is_empty() && callout != "negate")
}

pub fn sys_add_key(
    type_name: *const c_char,
    description: *const c_char,
    payload: *const u8,
    plen: usize,
    keyring: i32,
) -> AxResult<isize> {
    let type_name = vm_load_string(type_name)?;
    let description = vm_load_string(description)?;
    let payload = validate_key_payload(&type_name, &description, payload, plen)?;
    let ids = current_key_ids();
    let key = Key::positive(type_name, description, payload, ids.uid, ids.gid);
    let charge = key.charge();

    let mut manager = KEY_MANAGER.lock();
    let keyring = manager.resolve_keyring(keyring, ids, true)?;
    if !manager.keyring_has_write(keyring)? {
        return Err(LinuxError::EACCES.into());
    }
    if !manager.quota_allows(ids.uid, charge) {
        return Err(LinuxError::EDQUOT.into());
    }
    let serial = manager.insert_key(key);
    manager.link_key_replace(keyring, serial)?;
    Ok(serial as isize)
}

pub fn sys_request_key(
    type_name: *const c_char,
    description: *const c_char,
    callout_info: *const c_char,
    dest_keyring: i32,
) -> AxResult<isize> {
    let type_name = vm_load_string(type_name)?;
    let description = vm_load_string(description)?;
    let callout = if callout_info.is_null() {
        None
    } else {
        Some(vm_load_string(callout_info)?)
    };
    if !matches!(
        type_name.as_str(),
        "keyring" | "user" | "logon" | "big_key" | "encrypted"
    ) {
        return Err(AxError::NoSuchDevice);
    }

    let ids = current_key_ids();
    let mut manager = KEY_MANAGER.lock();
    if let Some(serial) = manager.search_current(ids, &type_name, &description) {
        let key = manager
            .keys
            .get(&serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        manager.check_key_available(key, true)?;
        return Ok(serial as isize);
    }

    let dest = manager.resolve_reqkey_destination(dest_keyring, ids)?;
    if !manager.keyring_has_write(dest)? {
        return Err(LinuxError::EACCES.into());
    }
    if should_instantiate_request(&type_name, &description, callout.as_deref()) {
        let callout = callout.ok_or(AxError::InvalidInput)?;
        let payload = callout.into_bytes();
        if payload.len() > USER_KEY_PAYLOAD_MAX {
            return Err(AxError::InvalidInput);
        }
        let key = Key::positive(type_name, description, payload, ids.uid, ids.gid);
        let charge = key.charge();
        if !manager.quota_allows(ids.uid, charge) {
            return Err(LinuxError::EDQUOT.into());
        }
        let serial = manager.insert_key(key);
        manager.link_key_replace(dest, serial)?;
        return Ok(serial as isize);
    }
    let key = Key::negative(type_name, description, ids.uid, ids.gid);
    let charge = key.charge();
    if !manager.quota_allows(ids.uid, charge) {
        return Err(LinuxError::EDQUOT.into());
    }
    let serial = manager.insert_key(key);
    manager.link_key_replace(dest, serial)?;
    Err(LinuxError::ENOKEY.into())
}

pub fn sys_keyctl(
    option: i32,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
) -> AxResult<isize> {
    let ids = current_key_ids();
    let mut manager = KEY_MANAGER.lock();
    match option {
        KEYCTL_GET_KEYRING_ID => {
            let serial = manager.resolve_keyring(arg2 as i32, ids, arg3 != 0)?;
            Ok(serial as isize)
        }
        KEYCTL_JOIN_SESSION_KEYRING => {
            if arg2 != 0 {
                let name = vm_load_string(arg2 as *const c_char)?;
                if name.starts_with('.') {
                    return Err(AxError::OperationNotPermitted);
                }
            }
            let serial = manager.create_keyring(format!("_ses.{}", ids.pid), ids.uid, ids.gid);
            manager.session_keyrings.insert(ids.pid, serial);
            Ok(serial as isize)
        }
        KEYCTL_UPDATE => {
            let serial = arg2 as i32;
            let payload = load_payload(arg3 as *const u8, arg4)?;
            let key = manager
                .keys
                .get_mut(&serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            if key.state == KeyState::Revoked {
                return Err(LinuxError::EKEYREVOKED.into());
            }
            if key.state == KeyState::Negative {
                return Err(LinuxError::ENOKEY.into());
            }
            if key
                .expires_at
                .is_some_and(|expires_at| wall_time().as_secs() >= expires_at)
            {
                return Err(LinuxError::EKEYEXPIRED.into());
            }
            if key.is_keyring() || key.type_name != "user" {
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            if key.perm & KEY_POS_WRITE == 0 {
                return Err(LinuxError::EACCES.into());
            }
            if payload.len() > USER_KEY_PAYLOAD_MAX {
                return Err(AxError::InvalidInput);
            }
            key.payload = payload;
            Ok(0)
        }
        KEYCTL_REVOKE => {
            let key = manager
                .keys
                .get_mut(&(arg2 as i32))
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            key.state = KeyState::Revoked;
            Ok(0)
        }
        KEYCTL_CHOWN => {
            let serial = manager.resolve_key(arg2 as i32, ids, false)?;
            let key = manager
                .keys
                .get_mut(&serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            if key.perm & KEY_POS_SETATTR == 0 {
                return Err(LinuxError::EACCES.into());
            }
            let uid = arg3 as u32;
            let gid = arg4 as u32;
            if uid != u32::MAX {
                key.uid = uid;
            }
            if gid != u32::MAX {
                key.gid = gid;
            }
            Ok(0)
        }
        KEYCTL_SETPERM => {
            if (arg3 as u32) & !KEY_PERM_MASK != 0 {
                return Err(AxError::InvalidInput);
            }
            let serial = if (arg2 as i32) < 0 {
                manager.resolve_keyring(arg2 as i32, ids, false)?
            } else {
                arg2 as i32
            };
            let key = manager
                .keys
                .get_mut(&serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            key.perm = arg3 as u32;
            Ok(0)
        }
        KEYCTL_DESCRIBE => {
            let serial = manager.resolve_key(arg2 as i32, ids, false)?;
            let key = manager
                .keys
                .get(&serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            if !manager.has_perm(key, KEY_POS_VIEW) {
                return Err(LinuxError::EACCES.into());
            }
            let description = format!(
                "{};{};{};{:08x};{}\0",
                key.type_name, key.uid, key.gid, key.perm, key.description
            );
            write_counted_bytes_if_fits(arg3 as *mut u8, arg4, description.as_bytes())
        }
        KEYCTL_CLEAR => {
            let keyring = manager.resolve_keyring(arg2 as i32, ids, false)?;
            manager
                .keys
                .get_mut(&keyring)
                .ok_or(AxError::from(LinuxError::ENOKEY))?
                .links
                .clear();
            Ok(0)
        }
        KEYCTL_LINK => {
            let serial = manager.resolve_key(arg2 as i32, ids, false)?;
            let keyring = manager.resolve_keyring(arg3 as i32, ids, true)?;
            manager.link_existing_key(keyring, serial, false)?;
            Ok(0)
        }
        KEYCTL_UNLINK => {
            let serial = arg2 as i32;
            let keyring = manager.resolve_keyring(arg3 as i32, ids, false)?;
            manager.unlink_key_from_keyring(keyring, serial)?;
            Ok(0)
        }
        KEYCTL_SEARCH => {
            let keyring = manager.resolve_keyring(arg2 as i32, ids, false)?;
            let type_name = vm_load_string(arg3 as *const c_char)?;
            let description = vm_load_string(arg4 as *const c_char)?;
            let serial = manager
                .search_keyring(keyring, &type_name, &description, &mut Vec::new())?
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            if arg5 != 0 {
                let dest = manager.resolve_keyring(arg5 as i32, ids, true)?;
                manager.link_existing_key(dest, serial, false)?;
            }
            Ok(serial as isize)
        }
        KEYCTL_READ => {
            let serial = if (arg2 as i32) < 0 {
                manager.resolve_keyring(arg2 as i32, ids, false)?
            } else {
                arg2 as i32
            };
            let key = manager
                .keys
                .get(&serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            manager.check_key_available(key, true)?;
            if !manager.has_perm(key, KEY_POS_READ) {
                return Err(LinuxError::EACCES.into());
            }
            if key.is_keyring() {
                return write_keyring_ids(arg3 as *mut u8, arg4, &key.links);
            }
            if arg4 != 0 && arg3 != 0 {
                let copy_len = key.payload.len().min(arg4);
                vm_write_slice(arg3 as *mut u8, &key.payload[..copy_len])?;
            }
            Ok(key.payload.len() as isize)
        }
        KEYCTL_INSTANTIATE => {
            let serial = arg2 as i32;
            let payload = load_payload(arg3 as *const u8, arg4)?;
            if payload.len() > BIG_KEY_PAYLOAD_MAX {
                return Err(AxError::InvalidInput);
            }
            let key = manager
                .keys
                .get_mut(&serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            if key.state == KeyState::Revoked {
                return Err(LinuxError::EKEYREVOKED.into());
            }
            key.payload = payload;
            key.state = KeyState::Positive;
            if arg5 != 0 {
                let dest = manager.resolve_keyring(arg5 as i32, ids, true)?;
                manager.link_key_replace(dest, serial)?;
            }
            Ok(0)
        }
        KEYCTL_NEGATE | KEYCTL_REJECT => {
            if option == KEYCTL_REJECT {
                let error = arg4 as i32;
                if error <= 0 || LinuxError::try_from(error).is_err() {
                    return Err(AxError::InvalidInput);
                }
            }
            let serial = arg2 as i32;
            let key = manager
                .keys
                .get_mut(&serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            if key.state == KeyState::Revoked {
                return Err(LinuxError::EKEYREVOKED.into());
            }
            key.payload.clear();
            key.state = KeyState::Negative;
            let timeout = arg3 as u64;
            key.expires_at = (timeout != 0).then(|| wall_time().as_secs().saturating_add(timeout));
            let dest_arg = if option == KEYCTL_NEGATE { arg4 } else { arg5 };
            if dest_arg != 0 {
                let dest = manager.resolve_keyring(dest_arg as i32, ids, true)?;
                manager.link_key_replace(dest, serial)?;
            }
            Ok(0)
        }
        KEYCTL_SET_REQKEY_KEYRING => {
            let old_setting = manager
                .reqkey_defaults
                .get(&ids.pid)
                .copied()
                .unwrap_or(KEY_REQKEY_DEFL_DEFAULT);
            match arg2 as i32 {
                KEY_REQKEY_DEFL_NO_CHANGE => Ok(old_setting as isize),
                KEY_REQKEY_DEFL_THREAD_KEYRING => {
                    manager.special_keyring(KEY_SPEC_THREAD_KEYRING, ids, true)?;
                    manager.reqkey_defaults.insert(ids.pid, arg2 as i32);
                    Ok(old_setting as isize)
                }
                KEY_REQKEY_DEFL_PROCESS_KEYRING => {
                    manager.special_keyring(KEY_SPEC_PROCESS_KEYRING, ids, true)?;
                    manager.reqkey_defaults.insert(ids.pid, arg2 as i32);
                    Ok(old_setting as isize)
                }
                KEY_REQKEY_DEFL_DEFAULT => {
                    manager.reqkey_defaults.remove(&ids.pid);
                    Ok(old_setting as isize)
                }
                KEY_REQKEY_DEFL_SESSION_KEYRING
                | KEY_REQKEY_DEFL_USER_KEYRING
                | KEY_REQKEY_DEFL_USER_SESSION_KEYRING => {
                    manager.reqkey_defaults.insert(ids.pid, arg2 as i32);
                    Ok(old_setting as isize)
                }
                _ => Err(AxError::InvalidInput),
            }
        }
        KEYCTL_SET_TIMEOUT => {
            let key = manager
                .keys
                .get_mut(&(arg2 as i32))
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            let secs = arg3 as u64;
            key.expires_at = (secs != 0).then(|| wall_time().as_secs().saturating_add(secs));
            Ok(0)
        }
        KEYCTL_GET_SECURITY => {
            let serial = manager.resolve_key(arg2 as i32, ids, false)?;
            let key = manager
                .keys
                .get(&serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            if !manager.has_perm(key, KEY_POS_VIEW) {
                return Err(LinuxError::EACCES.into());
            }
            let label = key_security_label(key);
            write_counted_bytes_prefix(arg3 as *mut u8, arg4, &label)
        }
        KEYCTL_INVALIDATE => {
            let serial = arg2 as i32;
            if !manager.keys.contains_key(&serial) {
                return Err(LinuxError::ENOKEY.into());
            }
            manager.remove_key_everywhere(serial);
            Ok(0)
        }
        KEYCTL_GET_PERSISTENT => {
            let uid = if arg2 == u32::MAX as usize {
                ids.uid
            } else {
                arg2 as u32
            };
            let serial = manager.get_persistent_keyring(uid, ids);
            if arg3 != 0 {
                let dest = manager.resolve_keyring(arg3 as i32, ids, true)?;
                manager.link_existing_key(dest, serial, false)?;
            }
            Ok(serial as isize)
        }
        KEYCTL_RESTRICT_KEYRING => {
            let serial = manager.resolve_keyring(arg2 as i32, ids, false)?;
            if arg3 == 0 && arg4 != 0 || arg3 != 0 && arg4 == 0 {
                return Err(AxError::InvalidInput);
            }
            if arg3 != 0 {
                let _ = vm_load_string(arg3 as *const c_char)?;
                let _ = vm_load_string(arg4 as *const c_char)?;
            }
            let key = manager
                .keys
                .get_mut(&serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            if key.perm & KEY_POS_SETATTR == 0 {
                return Err(LinuxError::EACCES.into());
            }
            key.restricted = true;
            Ok(0)
        }
        KEYCTL_MOVE => {
            let serial = manager.resolve_key(arg2 as i32, ids, false)?;
            let from = manager.resolve_keyring(arg3 as i32, ids, false)?;
            let to = manager.resolve_keyring(arg4 as i32, ids, true)?;
            let flags = arg5 as u32;
            if flags & !KEYCTL_MOVE_EXCL != 0 {
                return Err(AxError::InvalidInput);
            }
            {
                let from_key = manager
                    .keys
                    .get_mut(&from)
                    .ok_or(AxError::from(LinuxError::ENOKEY))?;
                let before = from_key.links.len();
                from_key.links.retain(|linked| *linked != serial);
                if before == from_key.links.len() {
                    return Err(LinuxError::ENOKEY.into());
                }
            }
            manager.link_existing_key(to, serial, flags & KEYCTL_MOVE_EXCL != 0)?;
            Ok(0)
        }
        KEYCTL_CAPABILITIES => {
            if arg2 != 0 && arg3 != 0 {
                let copy_len = KEYCTL_CAPABILITIES_BYTES.len().min(arg3);
                vm_write_slice(arg2 as *mut u8, &KEYCTL_CAPABILITIES_BYTES[..copy_len])?;
                if arg3 > copy_len {
                    let mut zeros = Vec::new();
                    zeros.resize(arg3 - copy_len, 0);
                    vm_write_slice((arg2 as *mut u8).wrapping_add(copy_len), &zeros)?;
                }
            }
            Ok(KEYCTL_CAPABILITIES_BYTES.len() as isize)
        }
        _ => Err(LinuxError::EOPNOTSUPP.into()),
    }
}

pub(crate) fn key_users_snapshot() -> String {
    let manager = KEY_MANAGER.lock();
    let mut users = BTreeMap::<u32, KeyUsage>::new();
    for key in manager.keys.values() {
        let usage = users.entry(key.uid).or_default();
        usage.keys += 1;
        usage.bytes += key.charge();
    }

    let mut out = String::new();
    for (uid, usage) in users {
        out.push_str(&format!(
            "{uid:5}: {usage_ref:5} 0/0 {keys}/{max_keys} {bytes}/{max_bytes}\n",
            usage_ref = usage.keys,
            keys = usage.keys,
            max_keys = user_maxkeys(uid),
            bytes = usage.bytes,
            max_bytes = user_maxbytes(uid),
        ));
    }
    out
}

pub(crate) fn key_gc_delay() -> usize {
    KEY_GC_DELAY.load(Ordering::Relaxed)
}

pub(crate) fn set_key_gc_delay(value: usize) {
    KEY_GC_DELAY.store(value, Ordering::Relaxed);
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
