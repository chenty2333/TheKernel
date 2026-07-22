//! Linux-compatible key and keyring state.
//!
//! This module owns the Linux-visible key policy and state machine. Syscall
//! entry code is responsible only for UAPI decoding and userspace copies.

mod accounting;
mod contract;
mod manager;
mod object;
mod service;

pub(crate) use service::{
    KeyActor, KeyTaskOwner, KeyTypeKind, KeyUserRecord, KeyctlCommand, KeyctlOutput, ReqKeyDefault,
    add_key, credential_fsids_precommit, exec_committed, exit_committed, key_maxbytes, key_maxkeys,
    key_root_maxbytes, key_root_maxkeys, key_user_records, keyctl, prepare_fork, request_key,
    set_key_maxbytes, set_key_maxkeys, set_key_root_maxbytes, set_key_root_maxkeys,
};
