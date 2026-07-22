use alloc::vec::Vec;
use core::{ffi::c_char, mem::size_of};

use axerrno::{AxError, AxResult, LinuxError};
use axtask::current;
use linux_raw_sys::general::{CAP_SETUID, CAP_SYS_ADMIN};
use memory_addr::PAGE_SIZE_4K;
use starry_vm::{vm_load, vm_write_slice};
use thekernel_linux_cred::KeyPermissionMask;

use crate::{
    keyring::{self, KeyActor, KeyTypeKind, KeyctlCommand, KeyctlOutput, ReqKeyDefault},
    mm::vm_load_string_bounded,
    task::{AsThread, Cred},
};

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

const KEY_TYPE_STRING_MAX: usize = 32;
const KEY_DESCRIPTION_STRING_MAX: usize = 4096;
const KEY_CALLOUT_STRING_MAX: usize = 4096;

fn key_actor_capabilities(credential: &Cred) -> (bool, bool) {
    (
        credential.has_effective_capability_in_own_user_ns(CAP_SYS_ADMIN),
        credential.has_effective_capability_in_own_user_ns(CAP_SETUID),
    )
}

fn parse_add_key_kind(type_name: &str, description: &str) -> AxResult<KeyTypeKind> {
    if type_name.starts_with("keyring") && description.starts_with('.') {
        return Err(AxError::OperationNotPermitted);
    }
    KeyTypeKind::from_name(type_name).ok_or(AxError::NoSuchDevice)
}

fn current_key_actor() -> KeyActor {
    let curr = current();
    let thread = curr.as_thread();
    let credential = thread.current_cred();
    let ids = credential.ids();
    let (has_sys_admin, has_setuid) = key_actor_capabilities(&credential);
    KeyActor::new(
        thread.tid(),
        thread.proc_data.proc.pid(),
        thread.kernel_tid(),
        thread.proc_data.proc.pid(),
        ids,
        credential.fs_dac_credentials(),
        credential.user_ns().clone(),
        has_sys_admin,
        has_setuid,
    )
}

fn validate_key_payload(
    kind: KeyTypeKind,
    description: &str,
    payload: *const u8,
    plen: usize,
) -> AxResult<Vec<u8>> {
    match kind {
        KeyTypeKind::Keyring => {
            if plen != 0 {
                return Err(AxError::InvalidInput);
            }
            Ok(Vec::new())
        }
        KeyTypeKind::User | KeyTypeKind::Logon => {
            if plen == 0 || plen > kind.payload_limit() {
                return Err(AxError::InvalidInput);
            }
            if kind == KeyTypeKind::Logon && description.find(':').is_none_or(|colon| colon == 0) {
                return Err(AxError::InvalidInput);
            }
            load_payload(payload, plen)
        }
        KeyTypeKind::BigKey => {
            if plen == 0 || plen > kind.payload_limit() {
                return Err(AxError::InvalidInput);
            }
            load_payload(payload, plen)
        }
    }
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

fn write_keyctl_capabilities(buf: *mut u8, size: usize) -> AxResult<isize> {
    if size == 0 {
        return Ok(KEYCTL_CAPABILITIES_BYTES.len() as isize);
    }
    if buf.is_null() {
        return Err(AxError::BadAddress);
    }

    let copy_len = KEYCTL_CAPABILITIES_BYTES.len().min(size);
    vm_write_slice(buf, &KEYCTL_CAPABILITIES_BYTES[..copy_len])?;

    const ZERO_CHUNK: [u8; 64] = [0; 64];
    let mut zeroed = copy_len;
    while zeroed < size {
        let chunk_len = (size - zeroed).min(ZERO_CHUNK.len());
        vm_write_slice(buf.wrapping_add(zeroed), &ZERO_CHUNK[..chunk_len])?;
        zeroed += chunk_len;
    }
    Ok(KEYCTL_CAPABILITIES_BYTES.len() as isize)
}

pub fn sys_add_key(
    type_name: *const c_char,
    description: *const c_char,
    payload: *const u8,
    plen: usize,
    keyring: i32,
) -> AxResult<isize> {
    let type_name = vm_load_string_bounded(type_name, KEY_TYPE_STRING_MAX)?;
    let description = vm_load_string_bounded(description, KEY_DESCRIPTION_STRING_MAX)?;
    let kind = parse_add_key_kind(&type_name, &description)?;
    let payload = validate_key_payload(kind, &description, payload, plen)?;
    keyring::add_key(&current_key_actor(), kind, description, payload, keyring)
}

pub fn sys_request_key(
    type_name: *const c_char,
    description: *const c_char,
    callout_info: *const c_char,
    dest_keyring: i32,
) -> AxResult<isize> {
    let type_name = vm_load_string_bounded(type_name, KEY_TYPE_STRING_MAX)?;
    let kind = KeyTypeKind::from_name(&type_name).ok_or(AxError::NoSuchDevice)?;
    let description = vm_load_string_bounded(description, KEY_DESCRIPTION_STRING_MAX)?;
    let callout = if callout_info.is_null() {
        None
    } else {
        Some(vm_load_string_bounded(
            callout_info,
            KEY_CALLOUT_STRING_MAX,
        )?)
    };
    keyring::request_key(
        &current_key_actor(),
        kind,
        &description,
        callout.is_some(),
        dest_keyring,
    )
}

pub fn sys_keyctl(
    option: i32,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
) -> AxResult<isize> {
    if option == KEYCTL_CAPABILITIES {
        return write_keyctl_capabilities(arg2 as *mut u8, arg3);
    }
    if matches!(option, KEYCTL_INSTANTIATE | KEYCTL_NEGATE | KEYCTL_REJECT)
        || option == KEYCTL_GET_SECURITY
    {
        return Err(LinuxError::EOPNOTSUPP.into());
    }

    let command = match option {
        KEYCTL_GET_KEYRING_ID => KeyctlCommand::GetKeyringId {
            keyring: arg2 as i32,
            create: arg3 != 0,
        },
        KEYCTL_JOIN_SESSION_KEYRING => KeyctlCommand::JoinSession {
            name: if arg2 == 0 {
                None
            } else {
                Some(vm_load_string_bounded(
                    arg2 as *const c_char,
                    KEY_DESCRIPTION_STRING_MAX,
                )?)
            },
        },
        KEYCTL_UPDATE => {
            if arg4 > PAGE_SIZE_4K {
                return Err(AxError::InvalidInput);
            }
            let payload = load_payload(arg3 as *const u8, arg4)?;
            KeyctlCommand::Update {
                key: arg2 as i32,
                payload,
            }
        }
        KEYCTL_REVOKE => KeyctlCommand::Revoke { key: arg2 as i32 },
        KEYCTL_CHOWN => KeyctlCommand::Chown {
            key: arg2 as i32,
            uid: (arg3 as u32 != u32::MAX).then_some(arg3 as u32),
            gid: (arg4 as u32 != u32::MAX).then_some(arg4 as u32),
        },
        KEYCTL_SETPERM => KeyctlCommand::SetPerm {
            key: arg2 as i32,
            permissions: KeyPermissionMask::try_from_raw(arg3 as u32)
                .ok_or(AxError::InvalidInput)?,
        },
        KEYCTL_DESCRIBE => KeyctlCommand::Describe { key: arg2 as i32 },
        KEYCTL_CLEAR => KeyctlCommand::Clear {
            keyring: arg2 as i32,
        },
        KEYCTL_LINK => KeyctlCommand::Link {
            key: arg2 as i32,
            keyring: arg3 as i32,
        },
        KEYCTL_UNLINK => KeyctlCommand::Unlink {
            serial: arg2 as i32,
            keyring: arg3 as i32,
        },
        KEYCTL_SEARCH => KeyctlCommand::Search {
            keyring: arg2 as i32,
            type_name: vm_load_string_bounded(arg3 as *const c_char, KEY_TYPE_STRING_MAX)?,
            description: vm_load_string_bounded(arg4 as *const c_char, KEY_DESCRIPTION_STRING_MAX)?,
            destination: (arg5 != 0).then_some(arg5 as i32),
        },
        KEYCTL_READ => KeyctlCommand::Read {
            key: arg2 as i32,
            copy_limit: (arg3 != 0 && arg4 != 0).then_some(arg4),
        },
        KEYCTL_SET_REQKEY_KEYRING => KeyctlCommand::SetReqKeyring {
            setting: ReqKeyDefault::from_raw(arg2 as i32).ok_or(AxError::InvalidInput)?,
        },
        KEYCTL_SET_TIMEOUT => KeyctlCommand::SetTimeout {
            key: arg2 as i32,
            seconds: arg3 as u64,
        },
        KEYCTL_INVALIDATE => KeyctlCommand::Invalidate { key: arg2 as i32 },
        KEYCTL_GET_PERSISTENT => KeyctlCommand::GetPersistent {
            uid: (arg2 != u32::MAX as usize).then_some(arg2 as u32),
            destination: arg3 as i32,
        },
        KEYCTL_RESTRICT_KEYRING => {
            if arg3 == 0 && arg4 != 0 || arg3 != 0 && arg4 == 0 {
                return Err(AxError::InvalidInput);
            }
            if arg3 != 0 {
                let _ = vm_load_string_bounded(arg3 as *const c_char, KEY_TYPE_STRING_MAX)?;
                let _ = vm_load_string_bounded(arg4 as *const c_char, KEY_DESCRIPTION_STRING_MAX)?;
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            KeyctlCommand::Restrict {
                keyring: arg2 as i32,
            }
        }
        KEYCTL_MOVE => {
            let flags = arg5 as u32;
            if flags & !KEYCTL_MOVE_EXCL != 0 {
                return Err(AxError::InvalidInput);
            }
            KeyctlCommand::Move {
                key: arg2 as i32,
                from: arg3 as i32,
                to: arg4 as i32,
                exclusive: flags & KEYCTL_MOVE_EXCL != 0,
            }
        }
        _ => return Err(LinuxError::EOPNOTSUPP.into()),
    };

    match keyring::keyctl(&current_key_actor(), command)? {
        KeyctlOutput::Value(value) => Ok(value),
        KeyctlOutput::CountedBytes(bytes) => {
            write_counted_bytes_if_fits(arg3 as *mut u8, arg4, &bytes)
        }
        KeyctlOutput::KeyringIds(ids) => write_keyring_ids(arg3 as *mut u8, arg4, &ids),
        KeyctlOutput::Payload { full_len, bytes } => {
            if arg3 != 0 && arg4 != 0 {
                vm_write_slice(arg3 as *mut u8, &bytes)?;
            }
            Ok(full_len as isize)
        }
    }
}

#[cfg(test)]
mod tests {
    use core::ptr;

    use super::*;
    use crate::task::{Kgid, Kuid, UserNamespace, ns_capable};

    #[test]
    fn key_actor_capabilities_are_relative_to_own_user_namespace_only() {
        let root_namespace = UserNamespace::try_new_root().unwrap();
        let root_credential = Cred::try_root(root_namespace.clone()).unwrap();
        let child_namespace = root_namespace
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let sibling_namespace = root_namespace
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let child_credential =
            Cred::try_with_user_namespace(&root_credential, child_namespace.clone()).unwrap();

        assert_eq!(key_actor_capabilities(&child_credential), (true, true));
        assert!(ns_capable(
            &child_credential,
            &child_namespace,
            CAP_SYS_ADMIN
        ));
        assert!(!ns_capable(
            &child_credential,
            &root_namespace,
            CAP_SYS_ADMIN
        ));
        assert!(!ns_capable(
            &child_credential,
            &sibling_namespace,
            CAP_SYS_ADMIN
        ));
    }

    #[test]
    fn add_key_adapter_rejects_private_keyring_prefix_before_type_lookup() {
        assert_eq!(
            parse_add_key_kind("keyring", ".private"),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            parse_add_key_kind("keyring.invalid", ".private"),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            parse_add_key_kind("user", ".public-to-keyring-core"),
            Ok(KeyTypeKind::User)
        );
        assert_eq!(parse_add_key_kind("keyring", ""), Ok(KeyTypeKind::Keyring));
    }

    #[test]
    fn keyctl_capabilities_requires_a_non_null_output_for_nonzero_size() {
        assert_eq!(
            write_keyctl_capabilities(ptr::null_mut(), 1),
            Err(AxError::BadAddress)
        );
        assert_eq!(
            write_keyctl_capabilities(ptr::null_mut(), 0),
            Ok(KEYCTL_CAPABILITIES_BYTES.len() as isize)
        );
    }

    #[test]
    fn big_key_payload_must_be_nonempty() {
        assert_eq!(
            validate_key_payload(KeyTypeKind::BigKey, "key", ptr::null(), 0),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn user_and_logon_payloads_must_be_nonempty() {
        for kind in [KeyTypeKind::User, KeyTypeKind::Logon] {
            assert_eq!(
                validate_key_payload(kind, "name:field", ptr::null(), 0),
                Err(AxError::InvalidInput)
            );
        }
    }

    #[test]
    fn logon_description_requires_a_nonempty_prefix() {
        assert_eq!(
            validate_key_payload(KeyTypeKind::Logon, ":secret", [1_u8].as_ptr(), 1),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn keyctl_update_rejects_more_than_one_page_before_copying() {
        assert_eq!(
            sys_keyctl(
                KEYCTL_UPDATE,
                1,
                ptr::null::<u8>() as usize,
                PAGE_SIZE_4K + 1,
                0,
            ),
            Err(AxError::InvalidInput)
        );
    }
}
