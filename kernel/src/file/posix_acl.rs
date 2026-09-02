//! Linux POSIX.1e ACL xattr handling.
//!
//! The on-disk/userspace representation is deliberately kept as the Linux
//! little-endian `posix_acl_xattr_header` plus `posix_acl_xattr_entry` layout.
//! Keeping validation here prevents filesystems from accidentally accepting an
//! opaque `system.posix_acl_*` record without also providing its DAC meaning.

use alloc::vec::Vec;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{
    Location, Metadata, MetadataUpdate, NodePermission, NodeType, PreparedPosixAcl, XattrSetMode,
};
use linux_raw_sys::general::{R_OK, W_OK, X_OK};

use crate::task::DacCredentialView;

pub(crate) const ACCESS_XATTR: &[u8] = b"system.posix_acl_access";
pub(crate) const DEFAULT_XATTR: &[u8] = b"system.posix_acl_default";

const VERSION: u32 = 0x0002;
const HEADER_LEN: usize = 4;
const ENTRY_LEN: usize = 8;
const ACL_USER_OBJ: u16 = 0x01;
const ACL_USER: u16 = 0x02;
const ACL_GROUP_OBJ: u16 = 0x04;
const ACL_GROUP: u16 = 0x08;
const ACL_MASK: u16 = 0x10;
const ACL_OTHER: u16 = 0x20;
const ACL_PERM_MASK: u16 = 0x07;

#[derive(Clone, Copy)]
struct Entry {
    tag: u16,
    perm: u16,
    id: u32,
}

#[derive(Clone)]
struct Acl {
    entries: Vec<Entry>,
    extended: bool,
}

fn invalid() -> AxError {
    AxError::InvalidInput
}
fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}
fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

impl Acl {
    fn parse(bytes: &[u8]) -> AxResult<Self> {
        if bytes.len() < HEADER_LEN
            || (bytes.len() - HEADER_LEN) % ENTRY_LEN != 0
            || le_u32(&bytes[..HEADER_LEN]) != VERSION
        {
            return Err(invalid());
        }
        let count = (bytes.len() - HEADER_LEN) / ENTRY_LEN;
        if !(3..=0x10000).contains(&count) {
            return Err(invalid());
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(count)
            .map_err(|_| AxError::NoMemory)?;
        for raw in bytes[HEADER_LEN..].chunks_exact(ENTRY_LEN) {
            let tag = le_u16(&raw[..2]);
            let perm = le_u16(&raw[2..4]);
            let id = le_u32(&raw[4..8]);
            if perm & !ACL_PERM_MASK != 0 {
                return Err(invalid());
            }
            match tag {
                ACL_USER_OBJ | ACL_GROUP_OBJ | ACL_MASK | ACL_OTHER if id != u32::MAX => {
                    return Err(invalid());
                }
                ACL_USER | ACL_GROUP if id != u32::MAX => {}
                _ => return Err(invalid()),
            }
            entries.push(Entry { tag, perm, id });
        }
        if entries[0].tag != ACL_USER_OBJ || entries.last().is_none_or(|e| e.tag != ACL_OTHER) {
            return Err(invalid());
        }
        let mut cursor = 1;
        let mut last_id = None;
        while cursor < entries.len() && entries[cursor].tag == ACL_USER {
            if last_id.is_some_and(|last| last >= entries[cursor].id) {
                return Err(invalid());
            }
            last_id = Some(entries[cursor].id);
            cursor += 1;
        }
        if cursor >= entries.len() || entries[cursor].tag != ACL_GROUP_OBJ {
            return Err(invalid());
        }
        cursor += 1;
        last_id = None;
        while cursor < entries.len() && entries[cursor].tag == ACL_GROUP {
            if last_id.is_some_and(|last| last >= entries[cursor].id) {
                return Err(invalid());
            }
            last_id = Some(entries[cursor].id);
            cursor += 1;
        }
        let extended = entries
            .iter()
            .any(|e| matches!(e.tag, ACL_USER | ACL_GROUP));
        if extended {
            if cursor >= entries.len() || entries[cursor].tag != ACL_MASK {
                return Err(invalid());
            }
            cursor += 1;
        }
        if cursor + 1 != entries.len() || entries[cursor].tag != ACL_OTHER {
            return Err(invalid());
        }
        Ok(Self { entries, extended })
    }

    fn encode(&self) -> AxResult<Vec<u8>> {
        let size = HEADER_LEN
            .checked_add(
                self.entries
                    .len()
                    .checked_mul(ENTRY_LEN)
                    .ok_or(AxError::NoMemory)?,
            )
            .ok_or(AxError::NoMemory)?;
        let mut raw = Vec::new();
        raw.try_reserve_exact(size).map_err(|_| AxError::NoMemory)?;
        raw.extend_from_slice(&VERSION.to_le_bytes());
        for entry in &self.entries {
            raw.extend_from_slice(&entry.tag.to_le_bytes());
            raw.extend_from_slice(&entry.perm.to_le_bytes());
            raw.extend_from_slice(&entry.id.to_le_bytes());
        }
        Ok(raw)
    }
    fn entry(&self, tag: u16) -> Entry {
        self.entries
            .iter()
            .copied()
            .find(|entry| entry.tag == tag)
            .unwrap()
    }
    fn mode(&self, old: NodePermission) -> NodePermission {
        let group = if self.extended {
            self.entry(ACL_MASK).perm
        } else {
            self.entry(ACL_GROUP_OBJ).perm
        };
        NodePermission::from_bits_truncate(
            (old.bits() & !0o777)
                | (self.entry(ACL_USER_OBJ).perm << 6)
                | (group << 3)
                | self.entry(ACL_OTHER).perm,
        )
    }
    /// Restrict inherited entries by a requested create mode.
    fn clip_to_mode(&mut self, mode: NodePermission) {
        let owner = ((mode.bits() >> 6) & 7) as u16;
        let group = ((mode.bits() >> 3) & 7) as u16;
        let other = (mode.bits() & 7) as u16;
        for entry in &mut self.entries {
            match entry.tag {
                ACL_USER_OBJ => entry.perm &= owner,
                ACL_MASK if self.extended => entry.perm &= group,
                ACL_GROUP_OBJ if !self.extended => entry.perm &= group,
                ACL_OTHER => entry.perm &= other,
                _ => {}
            }
        }
    }
    /// chmod assigns the three mode-backed ACL entries; it does not preserve
    /// their old bits by intersecting them with the new mode.
    fn assign_mode(&mut self, mode: NodePermission) {
        let owner = ((mode.bits() >> 6) & 7) as u16;
        let group = ((mode.bits() >> 3) & 7) as u16;
        let other = (mode.bits() & 7) as u16;
        for entry in &mut self.entries {
            match entry.tag {
                ACL_USER_OBJ => entry.perm = owner,
                ACL_MASK if self.extended => entry.perm = group,
                ACL_GROUP_OBJ if !self.extended => entry.perm = group,
                ACL_OTHER => entry.perm = other,
                _ => {}
            }
        }
    }
    fn allows(&self, metadata: &Metadata, requested: u32, credentials: &DacCredentialView) -> bool {
        let wanted = ((if requested & R_OK != 0 { 4 } else { 0 })
            | (if requested & W_OK != 0 { 2 } else { 0 })
            | (if requested & X_OK != 0 { 1 } else { 0 })) as u16;
        if credentials.uid().into_raw() == metadata.uid {
            return self.entry(ACL_USER_OBJ).perm & wanted == wanted;
        }
        if let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.tag == ACL_USER && entry.id == credentials.uid().into_raw())
        {
            return (entry.perm & self.entry(ACL_MASK).perm) & wanted == wanted;
        }
        let in_group = |id| {
            credentials.gid().into_raw() == id
                || crate::task::Kgid::from_raw(id)
                    .is_some_and(|group| credentials.supplementary_groups().contains(&group))
        };
        let mut group_perm = if in_group(metadata.gid) {
            self.entry(ACL_GROUP_OBJ).perm
        } else {
            0
        };
        let mut matched = in_group(metadata.gid);
        for entry in self
            .entries
            .iter()
            .filter(|entry| entry.tag == ACL_GROUP && in_group(entry.id))
        {
            group_perm |= entry.perm;
            matched = true;
        }
        if matched {
            let mask = if self.extended {
                self.entry(ACL_MASK).perm
            } else {
                7
            };
            return (group_perm & mask) & wanted == wanted;
        }
        self.entry(ACL_OTHER).perm & wanted == wanted
    }

    fn from_mode(mode: NodePermission) -> AxResult<Self> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(3)
            .map_err(|_| AxError::NoMemory)?;
        entries.push(Entry {
            tag: ACL_USER_OBJ,
            perm: ((mode.bits() >> 6) & 7) as u16,
            id: u32::MAX,
        });
        entries.push(Entry {
            tag: ACL_GROUP_OBJ,
            perm: ((mode.bits() >> 3) & 7) as u16,
            id: u32::MAX,
        });
        entries.push(Entry {
            tag: ACL_OTHER,
            perm: (mode.bits() & 7) as u16,
            id: u32::MAX,
        });
        Ok(Self {
            entries,
            extended: false,
        })
    }

    fn grant_user(&mut self, uid: u32, permissions: u16) -> AxResult<()> {
        if !self.extended {
            let group = self.entry(ACL_GROUP_OBJ).perm;
            let other = self.entries.pop().ok_or(AxError::BadState)?;
            self.entries.try_reserve(2).map_err(|_| AxError::NoMemory)?;
            self.entries.push(Entry {
                tag: ACL_USER,
                perm: permissions,
                id: uid,
            });
            self.entries.push(Entry {
                tag: ACL_MASK,
                perm: group,
                id: u32::MAX,
            });
            self.entries.push(other);
            self.extended = true;
            return Ok(());
        }
        let user_start = 1;
        let user_end = self
            .entries
            .iter()
            .position(|entry| entry.tag == ACL_GROUP_OBJ)
            .ok_or(AxError::BadState)?;
        match self.entries[user_start..user_end].binary_search_by_key(&uid, |entry| entry.id) {
            Ok(index) => self.entries[user_start + index].perm = permissions,
            Err(index) => {
                self.entries.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                self.entries.insert(
                    user_start + index,
                    Entry {
                        tag: ACL_USER,
                        perm: permissions,
                        id: uid,
                    },
                );
            }
        }
        Ok(())
    }

    fn revoke_user(&mut self, uid: u32) -> bool {
        if !self.extended {
            return false;
        }
        let user_start = 1;
        let Some(user_end) = self
            .entries
            .iter()
            .position(|entry| entry.tag == ACL_GROUP_OBJ)
        else {
            return false;
        };
        let Ok(index) =
            self.entries[user_start..user_end].binary_search_by_key(&uid, |entry| entry.id)
        else {
            return false;
        };
        self.entries.remove(user_start + index);
        let named_groups = self.entries.iter().any(|entry| entry.tag == ACL_GROUP);
        let named_users = self.entries.iter().any(|entry| entry.tag == ACL_USER);
        if !named_users && !named_groups {
            self.entries.retain(|entry| entry.tag != ACL_MASK);
            self.extended = false;
        }
        true
    }
}

pub(crate) fn is_acl_xattr(name: &[u8]) -> bool {
    name == ACCESS_XATTR || name == DEFAULT_XATTR
}
fn validate_set(metadata: &Metadata, name: &[u8], value: &[u8]) -> AxResult<Acl> {
    if name == DEFAULT_XATTR && metadata.node_type != NodeType::Directory {
        return Err(AxError::PermissionDenied);
    }
    Acl::parse(value)
}
pub(crate) fn set(
    location: &Location,
    metadata: &Metadata,
    name: &[u8],
    value: &[u8],
    mode: XattrSetMode,
) -> AxResult<()> {
    let acl = validate_set(metadata, name, value)?;
    let previous = match location.get_xattr(name) {
        Ok(previous) => Some(previous),
        Err(error) if matches!(LinuxError::from(error), LinuxError::ENODATA) => None,
        Err(error) => return Err(error),
    };
    location.set_xattr(name, value, mode)?;
    if name == ACCESS_XATTR
        && let Err(error) = location.update_metadata(MetadataUpdate {
            mode: Some(acl.mode(metadata.mode)),
            ..Default::default()
        })
    {
        let rollback = match previous {
            Some(previous) => location.set_xattr(name, &previous, XattrSetMode::Upsert),
            None => location.remove_xattr(name),
        };
        if rollback.is_err() {
            return Err(AxError::Io);
        }
        return Err(error);
    }
    Ok(())
}
pub(crate) fn remove(location: &Location, metadata: &Metadata, name: &[u8]) -> AxResult<()> {
    if name == DEFAULT_XATTR && metadata.node_type != NodeType::Directory {
        return Err(AxError::PermissionDenied);
    }
    location.remove_xattr(name)
}
pub(crate) fn check_access(
    location: &Location,
    metadata: &Metadata,
    requested: u32,
    credentials: &DacCredentialView,
) -> AxResult<Option<bool>> {
    match location.get_xattr(ACCESS_XATTR) {
        Ok(bytes) => Ok(Some(Acl::parse(&bytes)?.allows(
            metadata,
            requested,
            credentials,
        ))),
        Err(error)
            if matches!(
                LinuxError::from(error),
                LinuxError::ENODATA | LinuxError::EOPNOTSUPP
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
/// Linux ignores umask when a parent default ACL exists. Its entries are
/// instead restricted by the mode requested by the creator.
pub(crate) fn initial_mode(
    parent: &Location,
    requested: NodePermission,
) -> AxResult<Option<NodePermission>> {
    let bytes = match parent.get_xattr(DEFAULT_XATTR) {
        Ok(bytes) => bytes,
        Err(error)
            if matches!(
                LinuxError::from(error),
                LinuxError::ENODATA | LinuxError::EOPNOTSUPP
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let mut acl = Acl::parse(&bytes)?;
    acl.clip_to_mode(requested);
    Ok(Some(acl.mode(requested)))
}
pub(crate) fn inherit_default(
    parent: &Location,
    child: &Location,
    node_type: NodeType,
) -> AxResult<()> {
    if node_type == NodeType::Symlink {
        return Ok(());
    }
    let bytes = match parent.get_xattr(DEFAULT_XATTR) {
        Ok(bytes) => bytes,
        Err(error)
            if matches!(
                LinuxError::from(error),
                LinuxError::ENODATA | LinuxError::EOPNOTSUPP
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let mut acl = Acl::parse(&bytes)?;
    let metadata = child.metadata()?;
    acl.clip_to_mode(metadata.mode);
    child.update_metadata(MetadataUpdate {
        mode: Some(acl.mode(metadata.mode)),
        ..Default::default()
    })?;
    child.set_xattr(ACCESS_XATTR, &acl.encode()?, XattrSetMode::Upsert)?;
    if node_type == NodeType::Directory {
        child.set_xattr(DEFAULT_XATTR, &bytes, XattrSetMode::Upsert)?;
    }
    Ok(())
}

/// Prepares inherited ACL xattrs without touching a child inode.  The bytes
/// are immutable create input and must be installed by the provider before it
/// inserts the new directory entry.
pub(crate) fn prepare_inherited_default(
    parent: &Location,
    node_type: NodeType,
    requested: NodePermission,
) -> AxResult<(Option<PreparedPosixAcl>, Option<PreparedPosixAcl>)> {
    if node_type == NodeType::Symlink {
        return Ok((None, None));
    }
    let bytes = match parent.get_xattr(DEFAULT_XATTR) {
        Ok(bytes) => bytes,
        Err(error)
            if matches!(
                LinuxError::from(error),
                LinuxError::ENODATA | LinuxError::EOPNOTSUPP
            ) =>
        {
            return Ok((None, None));
        }
        Err(error) => return Err(error),
    };
    let mut acl = Acl::parse(&bytes)?;
    acl.clip_to_mode(requested);
    let access = PreparedPosixAcl::parse(acl.encode()?)?;
    let default = if node_type == NodeType::Directory {
        Some(PreparedPosixAcl::parse(bytes)?)
    } else {
        None
    };
    Ok((Some(access), default))
}

/// Proof used by the standard FUSE create path.  FUSE has no atomic
/// post-create xattr operation, so the daemon may own ACL inheritance only
/// when the prepared attributes are exactly what Linux derives from this
/// parent default ACL and the create mode.  This compares canonical typed
/// records rather than trusting a capability bit as a blanket promise.
pub(crate) fn fuse_daemon_owns_inheritance(
    parent_default: &[u8],
    node_type: NodeType,
    requested: NodePermission,
    access: Option<&PreparedPosixAcl>,
    default: Option<&PreparedPosixAcl>,
) -> AxResult<bool> {
    let mut acl = Acl::parse(parent_default)?;
    acl.clip_to_mode(requested);
    let expected_access = acl.encode()?;
    if access.map(PreparedPosixAcl::as_bytes) != Some(expected_access.as_slice()) {
        return Ok(false);
    }
    let expected_default = (node_type == NodeType::Directory).then_some(parent_default);
    Ok(default.map(PreparedPosixAcl::as_bytes) == expected_default)
}
pub(crate) struct PreparedChmod {
    previous: Vec<u8>,
    replacement: Vec<u8>,
}

impl PreparedChmod {
    pub(crate) fn stage(&self, location: &Location) -> AxResult<()> {
        location.set_xattr(ACCESS_XATTR, &self.replacement, XattrSetMode::Replace)
    }
    pub(crate) fn rollback(&self, location: &Location) -> AxResult<()> {
        location.set_xattr(ACCESS_XATTR, &self.previous, XattrSetMode::Replace)
    }
}

pub(crate) fn prepare_chmod(
    location: &Location,
    mode: NodePermission,
) -> AxResult<Option<PreparedChmod>> {
    let previous = match location.get_xattr(ACCESS_XATTR) {
        Ok(bytes) => bytes,
        Err(error)
            if matches!(
                LinuxError::from(error),
                LinuxError::ENODATA | LinuxError::EOPNOTSUPP
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let mut acl = Acl::parse(&previous)?;
    acl.assign_mode(mode);
    Ok(Some(PreparedChmod {
        previous,
        replacement: acl.encode()?,
    }))
}

/// Privileged device/session ACL operation. Callers in kernel device/session
/// management bypass userspace xattr authorization, but still get exactly the
/// same parser, canonical ordering, mode-mask synchronization and storage
/// representation as `setfacl`.
pub(crate) fn grant_user(location: &Location, uid: u32, permissions: u16) -> AxResult<()> {
    if permissions & !ACL_PERM_MASK != 0 {
        return Err(AxError::InvalidInput);
    }
    let metadata = location.metadata()?;
    let previous = match location.get_xattr(ACCESS_XATTR) {
        Ok(bytes) => Some(bytes),
        Err(error)
            if matches!(
                LinuxError::from(error),
                LinuxError::ENODATA | LinuxError::EOPNOTSUPP
            ) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    let bytes = match previous.as_ref() {
        Some(bytes) => bytes.clone(),
        None => Acl::from_mode(metadata.mode)?.encode()?,
    };
    let mut acl = Acl::parse(&bytes)?;
    acl.grant_user(uid, permissions)?;
    let encoded = acl.encode()?;
    location.set_xattr(ACCESS_XATTR, &encoded, XattrSetMode::Upsert)?;
    if let Err(error) = location.update_metadata(MetadataUpdate {
        mode: Some(acl.mode(metadata.mode)),
        ..Default::default()
    }) {
        let _ = match previous {
            Some(previous) => location.set_xattr(ACCESS_XATTR, &previous, XattrSetMode::Upsert),
            None => location.remove_xattr(ACCESS_XATTR),
        };
        return Err(error);
    }
    Ok(())
}

pub(crate) fn revoke_user(location: &Location, uid: u32) -> AxResult<()> {
    let metadata = location.metadata()?;
    let bytes = match location.get_xattr(ACCESS_XATTR) {
        Ok(bytes) => bytes,
        Err(error)
            if matches!(
                LinuxError::from(error),
                LinuxError::ENODATA | LinuxError::EOPNOTSUPP
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let mut acl = Acl::parse(&bytes)?;
    if !acl.revoke_user(uid) {
        return Ok(());
    }
    if !acl.extended {
        location.remove_xattr(ACCESS_XATTR)?;
        if let Err(error) = location.update_metadata(MetadataUpdate {
            mode: Some(acl.mode(metadata.mode)),
            ..Default::default()
        }) {
            let _ = location.set_xattr(ACCESS_XATTR, &bytes, XattrSetMode::Upsert);
            return Err(error);
        }
        return Ok(());
    }
    let encoded = acl.encode()?;
    location.set_xattr(ACCESS_XATTR, &encoded, XattrSetMode::Replace)?;
    if let Err(error) = location.update_metadata(MetadataUpdate {
        mode: Some(acl.mode(metadata.mode)),
        ..Default::default()
    }) {
        let _ = location.set_xattr(ACCESS_XATTR, &bytes, XattrSetMode::Replace);
        return Err(error);
    }
    Ok(())
}
