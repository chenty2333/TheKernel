//! Linux-compatible key and keyring state.
//!
//! This module owns the Linux-visible key policy and state machine. Syscall
//! entry code is responsible only for UAPI decoding and userspace copies.

mod service;

pub(crate) use service::{
    KeyActor, KeyTypeKind, KeyctlCommand, KeyctlOutput, ReqKeyDefault, add_key, key_maxbytes,
    key_maxkeys, key_root_maxbytes, key_root_maxkeys, key_users_snapshot, keyctl, request_key,
    set_key_maxbytes, set_key_maxkeys, set_key_root_maxbytes, set_key_root_maxkeys,
};
