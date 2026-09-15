use alloc::{string::String, vec::Vec};
use core::{ffi::c_char, mem::size_of};

use axerrno::{AxError, AxResult, LinuxError};
use axtask::current;
use linux_raw_sys::general::{CAP_SETUID, CAP_SYS_ADMIN};
use tk_linux_cred::KeyPermissionMask;
use tk_linux_keyring::uapi::{
    KEY_CALLOUT_STRING_MAX, KEY_DESCRIPTION_STRING_MAX, KEY_TYPE_STRING_MAX, KeyctlPlan,
    KeyctlUapiError, RawKeyctlArgs, UserBuffer, UserString, capabilities_bytes, decode_keyctl,
};
use tk_linux_usercopy::{
    UserCopyError, UserMemory, UserMemoryContext, vm_load, vm_load_until_nul_bounded,
    vm_write_slice,
};

use crate::{
    keyring::{self, KeyActor, KeyTypeKind, KeyctlCommand, KeyctlOutput, ReqKeyDefault},
    mm::map_usercopy_error,
    task::{AsThread, Cred},
};

/// `add_key()`'s payload ceiling from `keyctl.c`: `plen > 1024 * 1024 - 1` is
/// rejected before any user pointer is touched.
const ADD_KEY_PAYLOAD_MAX: usize = 1024 * 1024 - 1;

/// Loads a NUL-terminated user string with Linux's `strndup_user()` error
/// contract.
///
/// `strndup_user()` reports a string that does not fit in its budget as
/// `-EINVAL`, not as a distinct length error, and every keyring string is read
/// that way (`key_get_type_from_user`, `strndup_user`).
fn load_user_string<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const c_char,
    max_bytes: usize,
) -> AxResult<String> {
    String::from_utf8(
        vm_load_until_nul_bounded(memory, ptr.cast::<u8>(), max_bytes)
            .map_err(|error| match error {
                UserCopyError::TooLong => AxError::InvalidInput,
                other => map_usercopy_error(other),
            })?,
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
    if total > tk_linux_keyring::uapi::KEYCTL_INSTANTIATE_PAYLOAD_MAX {
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

/// `key_get_type_from_user()`'s syntax rules for an already-copied name.
///
/// The 32-byte ceiling is enforced by the load itself; an empty name is
/// `-EINVAL` and a leading dot is `-EPERM`.
fn validate_key_type_encoding(type_name: &str) -> AxResult<()> {
    if type_name.is_empty() {
        return Err(AxError::InvalidInput);
    }
    if type_name.starts_with('.') {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(())
}

/// `key_type_lookup()` as every other entry point sees it.
///
/// `request_key(2)`, `KEYCTL_SEARCH` and `KEYCTL_RESTRICT_KEYRING` propagate
/// its `-ENOKEY` for an unregistered type unchanged.
fn registered_key_type(type_name: &str) -> AxResult<KeyTypeKind> {
    KeyTypeKind::from_name(type_name).ok_or(AxError::from(LinuxError::ENOKEY))
}

/// `add_key()`'s extra rule: a description-private keyring name is `-EPERM`.
///
/// Linux tests `strncmp(type, "keyring", 7) == 0`, so every `keyring`-prefixed
/// type name takes part.
fn parse_add_key_kind(type_name: &str, description: Option<&str>) -> AxResult<KeyTypeKind> {
    validate_key_type_encoding(type_name)?;
    // `add_key()` runs the private-name check before `key_create_or_update()`
    // resolves the type, so it wins even for an unregistered name.
    if type_name.starts_with("keyring") && description.is_some_and(|desc| desc.starts_with('.')) {
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
    // `keyctl_read_key()` stages the read method's output in a kernel buffer
    // and only calls `copy_to_user()` when the whole result fits: a short
    // buffer reports the length and transfers nothing at all.
    if size >= full_size && !buf.is_null() && full_size != 0 {
        let mut bytes = Vec::with_capacity(full_size);
        for id in ids {
            bytes.extend_from_slice(&id.to_ne_bytes());
        }
        vm_write_slice(memory, buf, &bytes).map_err(map_usercopy_error)?;
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
    // `add_key()` order: the payload ceiling is checked before the type name is
    // even read, so it outranks every EFAULT below it.
    if plen > ADD_KEY_PAYLOAD_MAX {
        return Err(AxError::InvalidInput);
    }
    let type_name = load_user_string(memory, type_name, KEY_TYPE_STRING_MAX)?;
    let description = if description.is_null() {
        None
    } else {
        Some(load_user_string(
            memory,
            description,
            KEY_DESCRIPTION_STRING_MAX,
        )?)
    };
    let kind = parse_add_key_kind(&type_name, description.as_deref())?;
    // Linux normalizes an empty description to NULL and then reaches
    // `key_alloc()`, which rejects a missing description with -EINVAL. Every
    // key type this kernel implements needs one.
    let description = description
        .filter(|description| !description.is_empty())
        .ok_or(AxError::InvalidInput)?;
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
    // `request_key()` reads all three strings before it resolves the key type,
    // so a bad description or callout outranks the unknown-type -ENODEV.
    let type_name = load_user_string(memory, type_name, KEY_TYPE_STRING_MAX)?;
    validate_key_type_encoding(&type_name)?;
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
    let kind = registered_key_type(&type_name)?;
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
        } => {
            let type_name = load_planned_string(memory, type_name)?;
            validate_key_type_encoding(&type_name)?;
            KeyctlCommand::Search {
                keyring,
                type_name,
                description: load_planned_string(memory, description)?,
                destination,
            }
        }
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
                // `keyring_restrict(key_ref, NULL, NULL)` installs
                // `restrict_link_reject`, which refuses every later link.
                (None, None) => None,
                // A typed restriction can only come from a key type that
                // exports `lookup_restriction`.  No type this kernel
                // implements does, and `keyring_restrict()` reports a missing
                // backend as -ENOENT rather than -EOPNOTSUPP.
                (Some(type_name), Some(restriction)) => {
                    let type_name = load_planned_string(memory, type_name)?;
                    let _restriction = load_planned_string(memory, restriction)?;
                    // The key type is resolved first, and only a registered
                    // type can reach the missing-backend -ENOENT.
                    registered_key_type(&type_name)?;
                    return Err(AxError::NotFound);
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
            // `keyring_read()` rejects a byte count that is not a whole number
            // of key serials before it copies anything.  A NULL output or a
            // zero length is the size query and always succeeds.
            if output.address != 0 && output.len != 0 && output.len % size_of::<i32>() != 0 {
                return Err(AxError::InvalidInput);
            }
            write_keyring_ids(memory, output.address as *mut u8, output.len, &ids)
        }
        KeyctlOutput::Payload { full_len, bytes } => {
            let KeyctlPlan::Read { output, .. } = plan else {
                unreachable!()
            };
            if !bytes.is_empty() && output.address != 0 {
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

    use tk_linux_usercopy::{UserCopyError, VmResult};

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
            parse_add_key_kind("keyring", Some(".private")),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            parse_add_key_kind("keyring.invalid", Some(".private")),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            parse_add_key_kind("user", Some(".public-to-keyring-core")),
            Ok(KeyTypeKind::User)
        );
        assert_eq!(parse_add_key_kind("keyring", Some("")), Ok(KeyTypeKind::Keyring));
        // A missing description cannot carry the private-name rule.
        assert_eq!(parse_add_key_kind("keyring", None), Ok(KeyTypeKind::Keyring));
    }

    #[test]
    fn key_type_names_follow_key_get_type_from_user() {
        // `key_get_type_from_user()`: empty is -EINVAL, a leading dot is
        // -EPERM, and the 32-byte ceiling is applied by the string load.
        assert_eq!(validate_key_type_encoding(""), Err(AxError::InvalidInput));
        assert_eq!(
            validate_key_type_encoding(".x"),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(validate_key_type_encoding("logon"), Ok(()));
        assert_eq!(parse_add_key_kind("", None), Err(AxError::InvalidInput));
        assert_eq!(
            parse_add_key_kind(".hidden", None),
            Err(AxError::OperationNotPermitted)
        );
        // `add_key(2)` is the one entry point that rewrites the registry's
        // -ENOKEY into -ENODEV.
        assert_eq!(
            parse_add_key_kind("bogus", None),
            Err(AxError::NoSuchDevice)
        );
        assert_eq!(parse_add_key_kind("user", None), Ok(KeyTypeKind::User));
        assert_eq!(
            registered_key_type("bogus"),
            Err(AxError::from(LinuxError::ENOKEY))
        );
        assert_eq!(registered_key_type("user"), Ok(KeyTypeKind::User));
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
                tk_linux_keyring::uapi::KEYCTL_UPDATE_PAYLOAD_MAX + 1,
                0,
            ),
            Err(AxError::InvalidInput)
        );
    }
}
