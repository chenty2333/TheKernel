use alloc::{string::String, vec::Vec};
use core::{ffi::c_char, mem::size_of};

use axerrno::{AxError, AxResult, LinuxError};
use axtask::current;
use linux_raw_sys::general::{CAP_SETUID, CAP_SYS_ADMIN};
use thekernel_linux_cred::KeyPermissionMask;
use thekernel_linux_keyring::uapi::{
    KEY_CALLOUT_STRING_MAX, KEY_DESCRIPTION_STRING_MAX, KEY_TYPE_STRING_MAX, KeyctlPlan,
    KeyctlUapiError, RawKeyctlArgs, UserBuffer, UserString, capabilities_bytes, decode_keyctl,
};
use thekernel_linux_usercopy::{
    UserMemory, UserMemoryContext, vm_load, vm_load_until_nul_bounded, vm_write_slice,
};

use crate::{
    keyring::{self, KeyActor, KeyTypeKind, KeyctlCommand, KeyctlOutput, ReqKeyDefault},
    mm::map_usercopy_error,
    task::{AsThread, Cred},
};

fn load_user_string<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const c_char,
    max_bytes: usize,
) -> AxResult<String> {
    String::from_utf8(
        vm_load_until_nul_bounded(memory, ptr.cast::<u8>(), max_bytes)
            .map_err(map_usercopy_error)?,
    )
    .map_err(|_| AxError::IllegalBytes)
}

fn load_planned_string<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    plan: UserString,
) -> AxResult<String> {
    load_user_string(memory, plan.address as *const c_char, plan.max_bytes)
}

fn load_planned_buffer<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    plan: UserBuffer,
) -> AxResult<Vec<u8>> {
    load_payload(memory, plan.address as *const u8, plan.len)
}

fn load_keyctl_iov_payload<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    plan: UserBuffer,
) -> AxResult<Vec<u8>> {
    use crate::mm::IoVec;

    let iovs =
        vm_load(memory, plan.address as *const IoVec, plan.len).map_err(map_usercopy_error)?;
    let total = iovs.iter().try_fold(0usize, |total, iov| {
        if iov.iov_len < 0 {
            return Err(AxError::InvalidInput);
        }
        total
            .checked_add(iov.iov_len as usize)
            .ok_or(AxError::InvalidInput)
    })?;
    if total > thekernel_linux_keyring::uapi::KEYCTL_UPDATE_PAYLOAD_MAX {
        return Err(AxError::InvalidInput);
    }
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(total)
        .map_err(|_| AxError::NoMemory)?;
    for iov in iovs {
        let len = iov.iov_len as usize;
        if len == 0 {
            continue;
        }
        let bytes = vm_load(memory, iov.iov_base as *const u8, len).map_err(map_usercopy_error)?;
        payload.extend_from_slice(&bytes);
    }
    Ok(payload)
}

fn map_keyctl_uapi_error(error: KeyctlUapiError) -> AxError {
    match error {
        KeyctlUapiError::Invalid => AxError::InvalidInput,
        KeyctlUapiError::Unsupported => LinuxError::EOPNOTSUPP.into(),
    }
}

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

fn validate_key_payload<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
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
            load_payload(memory, payload, plen)
        }
        KeyTypeKind::BigKey => {
            if plen == 0 || plen > kind.payload_limit() {
                return Err(AxError::InvalidInput);
            }
            load_payload(memory, payload, plen)
        }
    }
}

fn load_payload<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    payload: *const u8,
    plen: usize,
) -> AxResult<Vec<u8>> {
    if plen == 0 {
        return Ok(Vec::new());
    }
    if payload.is_null() {
        return Err(AxError::BadAddress);
    }
    vm_load(memory, payload, plen).map_err(map_usercopy_error)
}

fn write_keyring_ids<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    buf: *mut u8,
    size: usize,
    ids: &[i32],
) -> AxResult<isize> {
    let full_size = core::mem::size_of_val(ids);
    if size != 0 && !buf.is_null() {
        let mut bytes = Vec::new();
        for id in ids.iter().take(size / size_of::<i32>()) {
            bytes.extend_from_slice(&id.to_ne_bytes());
        }
        vm_write_slice(memory, buf, &bytes[..bytes.len().min(size)]).map_err(map_usercopy_error)?;
    }
    Ok(full_size as isize)
}

fn write_counted_bytes_if_fits<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    buf: *mut u8,
    size: usize,
    bytes: &[u8],
) -> AxResult<isize> {
    if !buf.is_null() && size >= bytes.len() {
        vm_write_slice(memory, buf, bytes).map_err(map_usercopy_error)?;
    }
    Ok(bytes.len() as isize)
}

fn write_keyctl_capabilities<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    buf: *mut u8,
    size: usize,
) -> AxResult<isize> {
    if size == 0 {
        return Ok(capabilities_bytes().len() as isize);
    }
    if buf.is_null() {
        return Err(AxError::BadAddress);
    }

    let copy_len = capabilities_bytes().len().min(size);
    vm_write_slice(memory, buf, &capabilities_bytes()[..copy_len]).map_err(map_usercopy_error)?;

    const ZERO_CHUNK: [u8; 64] = [0; 64];
    let mut zeroed = copy_len;
    while zeroed < size {
        let chunk_len = (size - zeroed).min(ZERO_CHUNK.len());
        vm_write_slice(memory, buf.wrapping_add(zeroed), &ZERO_CHUNK[..chunk_len])
            .map_err(map_usercopy_error)?;
        zeroed += chunk_len;
    }
    Ok(capabilities_bytes().len() as isize)
}

pub fn sys_add_key<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    type_name: *const c_char,
    description: *const c_char,
    payload: *const u8,
    plen: usize,
    keyring: i32,
) -> AxResult<isize> {
    let type_name = load_user_string(memory, type_name, KEY_TYPE_STRING_MAX)?;
    let description = load_user_string(memory, description, KEY_DESCRIPTION_STRING_MAX)?;
    let kind = parse_add_key_kind(&type_name, &description)?;
    let payload = validate_key_payload(memory, kind, &description, payload, plen)?;
    keyring::add_key(&current_key_actor(), kind, description, payload, keyring)
}

pub fn sys_request_key<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    type_name: *const c_char,
    description: *const c_char,
    callout_info: *const c_char,
    dest_keyring: i32,
) -> AxResult<isize> {
    let type_name = load_user_string(memory, type_name, KEY_TYPE_STRING_MAX)?;
    let kind = KeyTypeKind::from_name(&type_name).ok_or(AxError::NoSuchDevice)?;
    let description = load_user_string(memory, description, KEY_DESCRIPTION_STRING_MAX)?;
    let callout = if callout_info.is_null() {
        None
    } else {
        Some(load_user_string(
            memory,
            callout_info,
            KEY_CALLOUT_STRING_MAX,
        )?)
    };
    keyring::request_key(
        &current_key_actor(),
        kind,
        &description,
        callout.as_deref(),
        dest_keyring,
    )
}

pub fn sys_keyctl<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    option: i32,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
) -> AxResult<isize> {
    let plan = decode_keyctl(RawKeyctlArgs {
        option,
        arg2,
        arg3,
        arg4,
        arg5,
    })
    .map_err(map_keyctl_uapi_error)?;
    let command = match plan {
        KeyctlPlan::Capabilities { output } => {
            return write_keyctl_capabilities(memory, output.address as *mut u8, output.len);
        }
        KeyctlPlan::GetKeyringId { keyring, create } => {
            KeyctlCommand::GetKeyringId { keyring, create }
        }
        KeyctlPlan::JoinSession { name } => KeyctlCommand::JoinSession {
            name: name
                .map(|plan| load_planned_string(memory, plan))
                .transpose()?,
        },
        KeyctlPlan::Update { key, payload } => KeyctlCommand::Update {
            key,
            payload: load_planned_buffer(memory, payload)?,
        },
        KeyctlPlan::Revoke { key } => KeyctlCommand::Revoke { key },
        KeyctlPlan::Chown { key, uid, gid } => KeyctlCommand::Chown { key, uid, gid },
        KeyctlPlan::SetPerm { key, permissions } => KeyctlCommand::SetPerm {
            key,
            permissions: KeyPermissionMask::try_from_raw(permissions)
                .ok_or(AxError::InvalidInput)?,
        },
        KeyctlPlan::Describe { key, .. } => KeyctlCommand::Describe { key },
        KeyctlPlan::Clear { keyring } => KeyctlCommand::Clear { keyring },
        KeyctlPlan::Link { key, keyring } => KeyctlCommand::Link { key, keyring },
        KeyctlPlan::Unlink { serial, keyring } => KeyctlCommand::Unlink { serial, keyring },
        KeyctlPlan::Search {
            keyring,
            type_name,
            description,
            destination,
        } => KeyctlCommand::Search {
            keyring,
            type_name: load_planned_string(memory, type_name)?,
            description: load_planned_string(memory, description)?,
            destination,
        },
        KeyctlPlan::Read { key, output } => KeyctlCommand::Read {
            key,
            copy_limit: (output.address != 0 && output.len != 0).then_some(output.len),
        },
        KeyctlPlan::SetReqKeyring { setting } => KeyctlCommand::SetReqKeyring {
            setting: ReqKeyDefault::from_raw(setting).ok_or(AxError::InvalidInput)?,
        },
        KeyctlPlan::SetTimeout { key, seconds } => KeyctlCommand::SetTimeout { key, seconds },
        KeyctlPlan::Invalidate { key } => KeyctlCommand::Invalidate { key },
        KeyctlPlan::Instantiate {
            key,
            payload,
            destination,
        } => KeyctlCommand::Instantiate {
            key,
            payload: load_planned_buffer(memory, payload)?,
            destination,
        },
        KeyctlPlan::InstantiateIov {
            key,
            iov,
            destination,
        } => KeyctlCommand::Instantiate {
            key,
            payload: load_keyctl_iov_payload(memory, iov)?,
            destination,
        },
        KeyctlPlan::Negate {
            key,
            timeout,
            destination,
        } => KeyctlCommand::Negate {
            key,
            timeout,
            destination,
        },
        KeyctlPlan::AssumeAuthority { key } => KeyctlCommand::AssumeAuthority { key },
        KeyctlPlan::Reject {
            key,
            timeout,
            error,
            destination,
        } => KeyctlCommand::Reject {
            key,
            timeout,
            error,
            destination,
        },
        KeyctlPlan::GetPersistent { uid, destination } => {
            KeyctlCommand::GetPersistent { uid, destination }
        }
        KeyctlPlan::Restrict {
            keyring,
            type_name,
            restriction,
        } => {
            let kind = match (type_name, restriction) {
                (None, None) => None,
                (Some(type_name), Some(restriction)) => {
                    let type_name = load_planned_string(memory, type_name)?;
                    let restriction = load_planned_string(memory, restriction)?;
                    // This kernel's supported restriction is deliberately
                    // typed and exact: `keyring:<key-type>`. Unknown Linux
                    // restriction backends are rejected, never recorded as a
                    // generic “restricted” bit that accepts the wrong keys.
                    if restriction != "type" {
                        return Err(LinuxError::EOPNOTSUPP.into());
                    }
                    Some(KeyTypeKind::from_name(&type_name).ok_or(AxError::NoSuchDevice)?)
                }
                _ => return Err(AxError::InvalidInput),
            };
            KeyctlCommand::Restrict { keyring, kind }
        }
        KeyctlPlan::Move {
            key,
            from,
            to,
            exclusive,
        } => KeyctlCommand::Move {
            key,
            from,
            to,
            exclusive,
        },
    };

    match keyring::keyctl(&current_key_actor(), command)? {
        KeyctlOutput::Value(value) => Ok(value),
        KeyctlOutput::CountedBytes(bytes) => {
            let KeyctlPlan::Describe { output, .. } = plan else {
                unreachable!()
            };
            write_counted_bytes_if_fits(memory, output.address as *mut u8, output.len, &bytes)
        }
        KeyctlOutput::KeyringIds(ids) => {
            let KeyctlPlan::Read { output, .. } = plan else {
                unreachable!()
            };
            write_keyring_ids(memory, output.address as *mut u8, output.len, &ids)
        }
        KeyctlOutput::Payload { full_len, bytes } => {
            let KeyctlPlan::Read { output, .. } = plan else {
                unreachable!()
            };
            if output.address != 0 && output.len != 0 {
                vm_write_slice(memory, output.address as *mut u8, &bytes)
                    .map_err(map_usercopy_error)?;
            }
            Ok(full_len as isize)
        }
    }
}

#[cfg(test)]
mod tests {
    use core::{mem::MaybeUninit, ptr};

    use thekernel_linux_usercopy::{UserCopyError, VmResult};

    use super::*;
    use crate::task::{Kgid, Kuid, UserNamespace, ns_capable};

    struct NoMemory;

    // SAFETY: this fixture never reports a successful read or write.
    unsafe impl UserMemory for NoMemory {
        fn read(&mut self, _start: usize, _dst: &mut [MaybeUninit<u8>]) -> VmResult {
            Err(UserCopyError::BadAddress)
        }

        fn write(&mut self, _start: usize, _src: &[u8]) -> VmResult {
            Err(UserCopyError::BadAddress)
        }
    }

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
        let mut provider = NoMemory;
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            write_keyctl_capabilities(&mut memory, ptr::null_mut(), 1),
            Err(AxError::BadAddress)
        );
        assert_eq!(
            write_keyctl_capabilities(&mut memory, ptr::null_mut(), 0),
            Ok(capabilities_bytes().len() as isize)
        );
    }

    #[test]
    fn big_key_payload_must_be_nonempty() {
        let mut provider = NoMemory;
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            validate_key_payload(&mut memory, KeyTypeKind::BigKey, "key", ptr::null(), 0),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn user_and_logon_payloads_must_be_nonempty() {
        for kind in [KeyTypeKind::User, KeyTypeKind::Logon] {
            let mut provider = NoMemory;
            let mut memory = UserMemoryContext::new(&mut provider);
            assert_eq!(
                validate_key_payload(&mut memory, kind, "name:field", ptr::null(), 0),
                Err(AxError::InvalidInput)
            );
        }
    }

    #[test]
    fn logon_description_requires_a_nonempty_prefix() {
        let mut provider = NoMemory;
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            validate_key_payload(
                &mut memory,
                KeyTypeKind::Logon,
                ":secret",
                [1_u8].as_ptr(),
                1
            ),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn keyctl_update_rejects_more_than_one_page_before_copying() {
        let mut provider = NoMemory;
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            sys_keyctl(
                &mut memory,
                2,
                1,
                ptr::null::<u8>() as usize,
                thekernel_linux_keyring::uapi::KEYCTL_UPDATE_PAYLOAD_MAX + 1,
                0,
            ),
            Err(AxError::InvalidInput)
        );
    }
}
