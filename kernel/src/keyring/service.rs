use alloc::{string::String, vec::Vec};

use axerrno::AxResult;
use axsync::Mutex;

use super::manager::KeyManager;
pub(crate) use super::{
    accounting::{
        key_maxbytes, key_maxkeys, key_root_maxbytes, key_root_maxkeys, set_key_maxbytes,
        set_key_maxkeys, set_key_root_maxbytes, set_key_root_maxkeys,
    },
    manager::{KeyActor, KeyTypeKind, KeyUserRecord, KeyctlCommand, KeyctlOutput, ReqKeyDefault},
};

static KEY_MANAGER: Mutex<KeyManager> = Mutex::new(KeyManager::new());

pub(crate) fn add_key(
    actor: &KeyActor,
    kind: KeyTypeKind,
    description: String,
    payload: Vec<u8>,
    keyring: i32,
) -> AxResult<isize> {
    KEY_MANAGER
        .lock()
        .add_key(actor, kind, description, payload, keyring)
}

pub(crate) fn request_key(
    actor: &KeyActor,
    kind: KeyTypeKind,
    description: &str,
    callout_present: bool,
    dest_keyring: i32,
) -> AxResult<isize> {
    KEY_MANAGER
        .lock()
        .request_key(actor, kind, description, callout_present, dest_keyring)
}

pub(crate) fn keyctl(actor: &KeyActor, command: KeyctlCommand) -> AxResult<KeyctlOutput> {
    KEY_MANAGER.lock().keyctl(actor, command)
}

pub(crate) fn key_user_records() -> Vec<KeyUserRecord> {
    KEY_MANAGER.lock().key_user_records()
}
