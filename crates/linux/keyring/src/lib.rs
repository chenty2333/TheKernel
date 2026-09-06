//! Pure Linux keyring graph, quota, permission, search, GC and lifecycle policy.
#![no_std]
#![forbid(unsafe_code)]
#![allow(missing_docs)]
extern crate alloc;
use alloc::vec::Vec;
use core::num::NonZeroU32;

/// Linux `keyctl(2)` raw argument decoding and copy plans.
///
/// Addresses are deliberately opaque machine words: an embedding kernel owns
/// user-memory access and turns these plans into object operations.
pub mod uapi {
    pub const KEY_TYPE_STRING_MAX: usize = 32;
    pub const KEY_DESCRIPTION_STRING_MAX: usize = 4096;
    pub const KEY_CALLOUT_STRING_MAX: usize = 4096;
    pub const KEYCTL_UPDATE_PAYLOAD_MAX: usize = 4096;
    pub const KEYCTL_INSTANTIATE_IOV_MAX: usize = 1024;

    const MOVE_EXCL: u32 = 1;
    const CAPABILITIES: [u8; 2] = [0xf3, 0];

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum KeyctlUapiError {
        Invalid,
        Unsupported,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct UserString {
        pub address: usize,
        pub max_bytes: usize,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct UserBuffer {
        pub address: usize,
        pub len: usize,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct RawKeyctlArgs {
        pub option: i32,
        pub arg2: usize,
        pub arg3: usize,
        pub arg4: usize,
        pub arg5: usize,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum KeyctlPlan {
        Capabilities {
            output: UserBuffer,
        },
        GetKeyringId {
            keyring: i32,
            create: bool,
        },
        JoinSession {
            name: Option<UserString>,
        },
        Update {
            key: i32,
            payload: UserBuffer,
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
            permissions: u32,
        },
        Describe {
            key: i32,
            output: UserBuffer,
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
            type_name: UserString,
            description: UserString,
            destination: Option<i32>,
        },
        Read {
            key: i32,
            output: UserBuffer,
        },
        SetReqKeyring {
            setting: i32,
        },
        SetTimeout {
            key: i32,
            seconds: u64,
        },
        Invalidate {
            key: i32,
        },
        Instantiate {
            key: i32,
            payload: UserBuffer,
            destination: i32,
        },
        InstantiateIov {
            key: i32,
            iov: UserBuffer,
            destination: i32,
        },
        Negate {
            key: i32,
            timeout: u64,
            destination: i32,
        },
        AssumeAuthority {
            key: i32,
        },
        Reject {
            key: i32,
            timeout: u64,
            error: i32,
            destination: i32,
        },
        GetPersistent {
            uid: Option<u32>,
            destination: i32,
        },
        Restrict {
            keyring: i32,
            type_name: Option<UserString>,
            restriction: Option<UserString>,
        },
        Move {
            key: i32,
            from: i32,
            to: i32,
            exclusive: bool,
        },
    }

    pub const fn capabilities_bytes() -> &'static [u8] {
        &CAPABILITIES
    }

    pub fn decode_keyctl(raw: RawKeyctlArgs) -> Result<KeyctlPlan, KeyctlUapiError> {
        let a2 = raw.arg2;
        let a3 = raw.arg3;
        let a4 = raw.arg4;
        let a5 = raw.arg5;
        match raw.option {
            0 => Ok(KeyctlPlan::GetKeyringId {
                keyring: a2 as i32,
                create: a3 != 0,
            }),
            1 => Ok(KeyctlPlan::JoinSession {
                name: (a2 != 0).then_some(UserString {
                    address: a2,
                    max_bytes: KEY_DESCRIPTION_STRING_MAX,
                }),
            }),
            2 if a4 <= KEYCTL_UPDATE_PAYLOAD_MAX => Ok(KeyctlPlan::Update {
                key: a2 as i32,
                payload: UserBuffer {
                    address: a3,
                    len: a4,
                },
            }),
            2 => Err(KeyctlUapiError::Invalid),
            3 => Ok(KeyctlPlan::Revoke { key: a2 as i32 }),
            4 => Ok(KeyctlPlan::Chown {
                key: a2 as i32,
                uid: (a3 as u32 != u32::MAX).then_some(a3 as u32),
                gid: (a4 as u32 != u32::MAX).then_some(a4 as u32),
            }),
            5 => Ok(KeyctlPlan::SetPerm {
                key: a2 as i32,
                permissions: a3 as u32,
            }),
            6 => Ok(KeyctlPlan::Describe {
                key: a2 as i32,
                output: UserBuffer {
                    address: a3,
                    len: a4,
                },
            }),
            7 => Ok(KeyctlPlan::Clear { keyring: a2 as i32 }),
            8 => Ok(KeyctlPlan::Link {
                key: a2 as i32,
                keyring: a3 as i32,
            }),
            9 => Ok(KeyctlPlan::Unlink {
                serial: a2 as i32,
                keyring: a3 as i32,
            }),
            10 => Ok(KeyctlPlan::Search {
                keyring: a2 as i32,
                type_name: UserString {
                    address: a3,
                    max_bytes: KEY_TYPE_STRING_MAX,
                },
                description: UserString {
                    address: a4,
                    max_bytes: KEY_DESCRIPTION_STRING_MAX,
                },
                destination: (a5 != 0).then_some(a5 as i32),
            }),
            11 => Ok(KeyctlPlan::Read {
                key: a2 as i32,
                output: UserBuffer {
                    address: a3,
                    len: a4,
                },
            }),
            12 if a4 <= KEYCTL_UPDATE_PAYLOAD_MAX => Ok(KeyctlPlan::Instantiate {
                key: a2 as i32,
                payload: UserBuffer {
                    address: a3,
                    len: a4,
                },
                destination: a5 as i32,
            }),
            12 => Err(KeyctlUapiError::Invalid),
            13 => Ok(KeyctlPlan::Negate {
                key: a2 as i32,
                timeout: a3 as u64,
                destination: a4 as i32,
            }),
            16 => Ok(KeyctlPlan::AssumeAuthority { key: a2 as i32 }),
            17 => Err(KeyctlUapiError::Unsupported),
            19 => Ok(KeyctlPlan::Reject {
                key: a2 as i32,
                timeout: a3 as u64,
                error: a4 as i32,
                destination: a5 as i32,
            }),
            20 if a4 <= KEYCTL_INSTANTIATE_IOV_MAX => Ok(KeyctlPlan::InstantiateIov {
                key: a2 as i32,
                iov: UserBuffer {
                    address: a3,
                    len: a4,
                },
                destination: a5 as i32,
            }),
            20 => Err(KeyctlUapiError::Invalid),
            14 => Ok(KeyctlPlan::SetReqKeyring { setting: a2 as i32 }),
            15 => Ok(KeyctlPlan::SetTimeout {
                key: a2 as i32,
                seconds: a3 as u64,
            }),
            21 => Ok(KeyctlPlan::Invalidate { key: a2 as i32 }),
            22 => Ok(KeyctlPlan::GetPersistent {
                uid: (a2 != u32::MAX as usize).then_some(a2 as u32),
                destination: a3 as i32,
            }),
            29 if (a3 == 0) == (a4 == 0) => Ok(KeyctlPlan::Restrict {
                keyring: a2 as i32,
                type_name: (a3 != 0).then_some(UserString {
                    address: a3,
                    max_bytes: KEY_TYPE_STRING_MAX,
                }),
                restriction: (a4 != 0).then_some(UserString {
                    address: a4,
                    max_bytes: KEY_DESCRIPTION_STRING_MAX,
                }),
            }),
            29 => Err(KeyctlUapiError::Invalid),
            30 if (a5 as u32 & !MOVE_EXCL) == 0 => Ok(KeyctlPlan::Move {
                key: a2 as i32,
                from: a3 as i32,
                to: a4 as i32,
                exclusive: a5 as u32 & MOVE_EXCL != 0,
            }),
            30 => Err(KeyctlUapiError::Invalid),
            31 => Ok(KeyctlPlan::Capabilities {
                output: UserBuffer {
                    address: a2,
                    len: a3,
                },
            }),
            _ => Err(KeyctlUapiError::Unsupported),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyError {
    Invalid,
    NotFound,
    Exists,
    Permission,
    Quota,
    Limit,
    Overflow,
    State,
    Cycle,
}
/// Linux's negative key serial selectors, normalized before an embedding
/// kernel consults its task and namespace-owned root slots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpecialKeyring {
    Thread,
    Process,
    Session,
    User,
    UserSession,
}
pub const fn resolve_special_keyring(raw: i32) -> Result<SpecialKeyring, KeyError> {
    match raw {
        -1 => Ok(SpecialKeyring::Thread),
        -2 => Ok(SpecialKeyring::Process),
        -3 => Ok(SpecialKeyring::Session),
        -4 => Ok(SpecialKeyring::User),
        -5 => Ok(SpecialKeyring::UserSession),
        _ => Err(KeyError::NotFound),
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissingSessionPlan {
    CreateAnonymous,
    InstallUserSession,
}
pub const fn plan_missing_session(create: bool) -> MissingSessionPlan {
    if create {
        MissingSessionPlan::CreateAnonymous
    } else {
        MissingSessionPlan::InstallUserSession
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct KeyId(NonZeroU32);
impl KeyId {
    pub const fn new(raw: u32) -> Option<Self> {
        match NonZeroU32::new(raw) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct TaskOwnerId(NonZeroU32);
impl TaskOwnerId {
    pub const fn new(raw: u32) -> Option<Self> {
        match NonZeroU32::new(raw) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct ProcessOwnerId(NonZeroU32);
impl ProcessOwnerId {
    pub const fn new(raw: u32) -> Option<Self> {
        match NonZeroU32::new(raw) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyTaskOwner {
    pub thread: TaskOwnerId,
    pub process: ProcessOwnerId,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyKind {
    Keyring,
    User,
    Logon,
    BigKey,
}
impl KeyKind {
    pub const fn payload_limit(self) -> usize {
        match self {
            Self::Keyring => 0,
            Self::User | Self::Logon => 32767,
            Self::BigKey => 1 << 20,
        }
    }
    pub const fn readable(self) -> bool {
        !matches!(self, Self::Logon)
    }
    pub const fn supports_payload_update(self) -> bool {
        !matches!(self, Self::Keyring)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyPermissions(pub u32);
impl KeyPermissions {
    pub const VIEW: Self = Self(1);
    pub const READ: Self = Self(2);
    pub const WRITE: Self = Self(4);
    pub const SEARCH: Self = Self(8);
    pub const LINK: Self = Self(16);
    pub const SETATTR: Self = Self(32);
    pub const fn contains(self, r: Self) -> bool {
        self.0 & r.0 == r.0
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionLane {
    Possessor,
    Owner,
    Group,
    Other,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PermissionLanes {
    pub possessor: Option<KeyPermissions>,
    pub owner: Option<KeyPermissions>,
    pub group: Option<KeyPermissions>,
    pub other: Option<KeyPermissions>,
}
impl PermissionLanes {
    pub const fn select(self, lane: PermissionLane) -> KeyPermissions {
        match lane {
            PermissionLane::Possessor => match self.possessor {
                Some(p) => p,
                None => KeyPermissions(0),
            },
            PermissionLane::Owner => match self.owner {
                Some(p) => p,
                None => KeyPermissions(0),
            },
            PermissionLane::Group => match self.group {
                Some(p) => p,
                None => KeyPermissions(0),
            },
            PermissionLane::Other => match self.other {
                Some(p) => p,
                None => KeyPermissions(0),
            },
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PermissionInput {
    pub possessed: bool,
    pub owner: bool,
    pub group: bool,
}
pub const fn permission_lane(input: PermissionInput) -> PermissionLane {
    if input.possessed {
        PermissionLane::Possessor
    } else if input.owner {
        PermissionLane::Owner
    } else if input.group {
        PermissionLane::Group
    } else {
        PermissionLane::Other
    }
}
pub const fn permits(lanes: PermissionLanes, input: PermissionInput, want: KeyPermissions) -> bool {
    let identity = if input.owner {
        PermissionLane::Owner
    } else if input.group {
        PermissionLane::Group
    } else {
        PermissionLane::Other
    };
    let mut granted = lanes.select(identity);
    if input.possessed {
        granted = KeyPermissions(granted.0 | lanes.select(PermissionLane::Possessor).0);
    }
    granted.contains(want)
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyAvailability {
    Available,
    Revoked,
    Expired,
    WrongKind,
}
pub const fn availability(
    revoked: bool,
    expired: bool,
    is_keyring: bool,
    allow_keyring: bool,
) -> KeyAvailability {
    if revoked {
        KeyAvailability::Revoked
    } else if expired {
        KeyAvailability::Expired
    } else if is_keyring && !allow_keyring {
        KeyAvailability::WrongKind
    } else {
        KeyAvailability::Available
    }
}
pub const fn check_available(
    revoked: bool,
    expired: bool,
    is_keyring: bool,
    allow_keyring: bool,
) -> Result<KeyAvailability, KeyError> {
    match availability(revoked, expired, is_keyring, allow_keyring) {
        KeyAvailability::Available => Ok(KeyAvailability::Available),
        KeyAvailability::Revoked => Err(KeyError::State),
        KeyAvailability::Expired => Err(KeyError::NotFound),
        KeyAvailability::WrongKind => Err(KeyError::Invalid),
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QuotaCharge {
    pub keys: usize,
    pub bytes: usize,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QuotaUsage {
    pub keys: usize,
    pub bytes: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuotaLimit {
    pub keys: usize,
    pub bytes: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuotaPlan {
    pub after: QuotaUsage,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaAdmission {
    Enforced,
    AllowOverrun,
    Exempt,
}
impl QuotaPlan {
    pub fn replace(
        usage: QuotaUsage,
        old: QuotaCharge,
        new: QuotaCharge,
        limit: QuotaLimit,
    ) -> Result<Self, KeyError> {
        let keys = usage
            .keys
            .checked_sub(old.keys)
            .ok_or(KeyError::State)?
            .checked_add(new.keys)
            .ok_or(KeyError::Quota)?;
        let bytes = usage
            .bytes
            .checked_sub(old.bytes)
            .ok_or(KeyError::State)?
            .checked_add(new.bytes)
            .ok_or(KeyError::Quota)?;
        if keys > limit.keys || bytes > limit.bytes {
            Err(KeyError::Quota)
        } else {
            Ok(Self {
                after: QuotaUsage { keys, bytes },
            })
        }
    }

    pub fn admit_replace(
        usage: QuotaUsage,
        old: QuotaCharge,
        new: QuotaCharge,
        limit: QuotaLimit,
        admission: QuotaAdmission,
    ) -> Result<Option<Self>, KeyError> {
        if admission == QuotaAdmission::Exempt {
            return Ok(None);
        }
        let keys = usage
            .keys
            .checked_sub(old.keys)
            .ok_or(KeyError::State)?
            .checked_add(new.keys)
            .ok_or(KeyError::Quota)?;
        let bytes = usage
            .bytes
            .checked_sub(old.bytes)
            .ok_or(KeyError::State)?
            .checked_add(new.bytes)
            .ok_or(KeyError::Quota)?;
        if admission == QuotaAdmission::Enforced
            && ((new.keys > old.keys && keys > limit.keys)
                || (new.bytes > old.bytes && bytes > limit.bytes))
        {
            return Err(KeyError::Quota);
        }
        let plan = Self {
            after: QuotaUsage { keys, bytes },
        };
        Ok(Some(plan))
    }

    pub fn transfer(
        from: QuotaUsage,
        to: QuotaUsage,
        charge: QuotaCharge,
        to_limit: QuotaLimit,
        admission: QuotaAdmission,
        same_owner: bool,
    ) -> Result<Option<(Self, Self)>, KeyError> {
        if admission == QuotaAdmission::Exempt || same_owner {
            return Ok(None);
        }
        let empty = QuotaCharge::default();
        let from_plan = Self::replace(
            from,
            charge,
            empty,
            QuotaLimit {
                keys: usize::MAX,
                bytes: usize::MAX,
            },
        )?;
        let to_plan =
            Self::admit_replace(to, empty, charge, to_limit, admission)?.ok_or(KeyError::State)?;
        Ok(Some((from_plan, to_plan)))
    }

    /// Like [`Self::transfer`], but treats an intra-owner move as a no-op.
    pub fn transfer_for<O: Copy + Eq>(
        from_owner: O,
        to_owner: O,
        from: QuotaUsage,
        to: QuotaUsage,
        charge: QuotaCharge,
        to_limit: QuotaLimit,
        admission: QuotaAdmission,
    ) -> Result<Option<(Self, Self)>, KeyError> {
        if from_owner == to_owner {
            Ok(None)
        } else {
            Self::transfer(from, to, charge, to_limit, admission, false)
        }
    }

    pub fn retire(usage: QuotaUsage, charge: QuotaCharge) -> Result<Self, KeyError> {
        Self::replace(
            usage,
            charge,
            QuotaCharge::default(),
            QuotaLimit {
                keys: usize::MAX,
                bytes: usize::MAX,
            },
        )
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyMeta<O: Copy + Eq> {
    pub kind: KeyKind,
    pub owner: O,
    pub permissions: KeyPermissions,
    pub payload_len: usize,
    pub revoked: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphPlan {
    Link { from: KeyId, to: KeyId },
    Unlink { from: KeyId, to: KeyId },
    Retire { key: KeyId },
}
pub trait KeyGraph {
    type Owner: Copy + Eq;
    fn meta(&self, key: KeyId) -> Option<KeyMeta<Self::Owner>>;
    fn links(&self, key: KeyId, out: &mut Vec<KeyId>) -> Result<(), KeyError>;
}
pub fn plan_link<G: KeyGraph>(
    g: &G,
    from: KeyId,
    to: KeyId,
    max: usize,
) -> Result<GraphPlan, KeyError> {
    let a = g.meta(from).ok_or(KeyError::NotFound)?;
    let b = g.meta(to).ok_or(KeyError::NotFound)?;
    if a.kind != KeyKind::Keyring || a.revoked || b.revoked {
        return Err(KeyError::Invalid);
    }
    let mut q = Vec::new();
    q.try_reserve(max).map_err(|_| KeyError::Limit)?;
    q.push((to, 0usize));
    while let Some((id, depth)) = q.pop() {
        if id == from {
            return Err(KeyError::Cycle);
        }
        if g.meta(id).ok_or(KeyError::NotFound)?.kind != KeyKind::Keyring {
            continue;
        }
        let mut children = Vec::new();
        g.links(id, &mut children)?;
        let mut nested = Vec::new();
        for child in children {
            if g.meta(child).ok_or(KeyError::NotFound)?.kind == KeyKind::Keyring {
                nested.push(child);
            }
        }
        if depth >= max && !nested.is_empty() {
            return Err(KeyError::Limit);
        }
        for child in nested {
            q.push((child, depth.checked_add(1).ok_or(KeyError::Limit)?));
        }
    }
    Ok(GraphPlan::Link { from, to })
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchRequest {
    pub root: KeyId,
    pub kind: KeyKind,
    pub include_revoked: bool,
    pub max_visits: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchPlan {
    Found(KeyId),
    Missing,
}
pub fn plan_search<G: KeyGraph>(
    g: &G,
    r: SearchRequest,
    mut f: impl FnMut(KeyId) -> bool,
) -> Result<SearchPlan, KeyError> {
    let mut q = Vec::new();
    q.try_reserve(r.max_visits).map_err(|_| KeyError::Limit)?;
    q.push(r.root);
    let mut i = 0;
    while i < q.len() {
        if i >= r.max_visits {
            return Err(KeyError::Limit);
        }
        let id = q[i];
        i += 1;
        let m = g.meta(id).ok_or(KeyError::NotFound)?;
        if m.kind == r.kind && (r.include_revoked || !m.revoked) && f(id) {
            return Ok(SearchPlan::Found(id));
        }
        g.links(id, &mut q)?;
    }
    Ok(SearchPlan::Missing)
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TraversalNode {
    pub available: bool,
    pub searchable: bool,
    pub keyring: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BfsRequest<'a> {
    pub roots: &'a [KeyId],
    pub max_visits: usize,
    pub max_depth: usize,
}
/// Bounded breadth-first traversal which visits each key at most once.
/// A node must be available and searchable to be visited; children are then
/// considered only for searchable keyrings.
pub fn bfs<G: KeyGraph>(
    g: &G,
    request: BfsRequest<'_>,
    mut node: impl FnMut(KeyId) -> Result<TraversalNode, KeyError>,
    mut visit: impl FnMut(KeyId, usize) -> bool,
) -> Result<Option<KeyId>, KeyError> {
    let mut queue = Vec::new();
    queue
        .try_reserve(request.max_visits)
        .map_err(|_| KeyError::Limit)?;
    for &root in request.roots {
        if queue.len() >= request.max_visits {
            return Err(KeyError::Limit);
        }
        queue.push((root, 0usize));
    }
    let mut seen = Vec::new();
    seen.try_reserve(request.max_visits)
        .map_err(|_| KeyError::Limit)?;
    let mut index = 0;
    while index < queue.len() {
        if seen.len() >= request.max_visits {
            return Err(KeyError::Limit);
        }
        let (key, depth) = queue[index];
        index += 1;
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let state = node(key)?;
        if !state.available || !state.searchable {
            continue;
        }
        if visit(key, depth) {
            return Ok(Some(key));
        }
        if depth <= request.max_depth && state.keyring {
            let mut children = Vec::new();
            g.links(key, &mut children)?;
            for child in children {
                // Linux bounds nesting, not a final ordinary key reached from
                // the last permitted keyring level.  One terminal keyring is
                // therefore visitable at the boundary, while a data key below
                // it is not.
                if depth == request.max_depth
                    && g.meta(child).ok_or(KeyError::NotFound)?.kind != KeyKind::Keyring
                {
                    continue;
                }
                if queue.iter().any(|(queued, _)| *queued == child) {
                    continue;
                }
                if queue.len() >= request.max_visits {
                    return Err(KeyError::Limit);
                }
                queue.push((child, depth + 1));
            }
        }
    }
    Ok(None)
}
/// Tests whether `target` is reachable from any possession root under the
/// same availability and SEARCH gates used to traverse keyrings.
pub fn is_possessed<G: KeyGraph>(
    g: &G,
    roots: &[KeyId],
    target: KeyId,
    max_visits: usize,
    max_depth: usize,
    node: impl FnMut(KeyId) -> Result<TraversalNode, KeyError>,
) -> Result<bool, KeyError> {
    Ok(bfs(
        g,
        BfsRequest {
            roots,
            max_visits,
            max_depth,
        },
        node,
        |key, _| key == target,
    )?
    .is_some())
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GcCandidate {
    pub key: KeyId,
    pub roots: usize,
    pub links: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GcPlan {
    Retire(KeyId),
    Keep(KeyId),
}
pub const fn plan_gc(c: GcCandidate) -> GcPlan {
    if c.roots == 0 && c.links == 0 {
        GcPlan::Retire(c.key)
    } else {
        GcPlan::Keep(c.key)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GcScratchState {
    Idle,
    Touched,
    Queued,
    Retire,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GcScratch {
    pub epoch: u64,
    pub root_drops: usize,
    pub link_drops: usize,
    pub state: GcScratchState,
}
impl GcScratch {
    pub const IDLE: Self = Self {
        epoch: 0,
        root_drops: 0,
        link_drops: 0,
        state: GcScratchState::Idle,
    };
    pub fn touch(epoch: u64) -> Result<Self, KeyError> {
        if epoch == 0 {
            Err(KeyError::State)
        } else {
            Ok(Self {
                epoch,
                root_drops: 0,
                link_drops: 0,
                state: GcScratchState::Touched,
            })
        }
    }
    pub fn add_root_drop(&mut self, roots: usize) -> Result<(), KeyError> {
        self.root_drops = self.root_drops.checked_add(1).ok_or(KeyError::Overflow)?;
        if self.root_drops > roots {
            Err(KeyError::State)
        } else {
            Ok(())
        }
    }
    pub fn add_link_drop(&mut self, links: usize) -> Result<(), KeyError> {
        self.link_drops = self.link_drops.checked_add(1).ok_or(KeyError::Overflow)?;
        if self.link_drops > links {
            Err(KeyError::State)
        } else {
            Ok(())
        }
    }
    pub fn queue_if_unreferenced(&mut self, roots: usize, links: usize) -> Result<bool, KeyError> {
        if roots.checked_sub(self.root_drops).ok_or(KeyError::State)? != 0
            || links.checked_sub(self.link_drops).ok_or(KeyError::State)? != 0
        {
            return Ok(false);
        }
        match self.state {
            GcScratchState::Touched => {
                self.state = GcScratchState::Queued;
                Ok(true)
            }
            GcScratchState::Queued | GcScratchState::Retire => Ok(false),
            GcScratchState::Idle => Err(KeyError::State),
        }
    }
    pub fn retire(&mut self, roots: usize, links: usize) -> Result<(), KeyError> {
        if self.state != GcScratchState::Queued
            || roots != self.root_drops
            || links != self.link_drops
        {
            return Err(KeyError::State);
        }
        self.state = GcScratchState::Retire;
        Ok(())
    }
    pub fn clear(&mut self) {
        *self = Self::IDLE;
    }
}
/// Per-owner, allocation-free GC quota scratch.  The caller owns the epoch
/// and links touched owners externally; this value only validates aggregation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GcQuotaScratch {
    pub epoch: u64,
    pub retire: QuotaCharge,
    pub after: QuotaUsage,
}
impl GcQuotaScratch {
    pub const IDLE: Self = Self {
        epoch: 0,
        retire: QuotaCharge { keys: 0, bytes: 0 },
        after: QuotaUsage { keys: 0, bytes: 0 },
    };
    pub fn add_retire(
        &mut self,
        epoch: u64,
        usage: QuotaUsage,
        charge: QuotaCharge,
    ) -> Result<(), KeyError> {
        if epoch == 0 || (self.epoch != 0 && self.epoch != epoch) {
            return Err(KeyError::State);
        }
        let retire = QuotaCharge {
            keys: self
                .retire
                .keys
                .checked_add(charge.keys)
                .ok_or(KeyError::Overflow)?,
            bytes: self
                .retire
                .bytes
                .checked_add(charge.bytes)
                .ok_or(KeyError::Overflow)?,
        };
        self.after = QuotaUsage {
            keys: usage.keys.checked_sub(retire.keys).ok_or(KeyError::State)?,
            bytes: usage
                .bytes
                .checked_sub(retire.bytes)
                .ok_or(KeyError::State)?,
        };
        self.epoch = epoch;
        self.retire = retire;
        Ok(())
    }
    pub fn clear(&mut self) {
        *self = Self::IDLE;
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForkState {
    Prepared,
    Committed,
    Cancelled,
    RolledBack,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForkPlan {
    parent: KeyTaskOwner,
    child: KeyTaskOwner,
    clone_thread: bool,
    state: ForkState,
}
impl ForkPlan {
    pub fn prepare(
        parent: KeyTaskOwner,
        child: KeyTaskOwner,
        clone_thread: bool,
    ) -> Result<Self, KeyError> {
        if parent.thread == child.thread
            || (clone_thread && parent.process != child.process)
            || (!clone_thread && child.thread.0.get() != child.process.0.get())
        {
            return Err(KeyError::State);
        }
        Ok(Self {
            parent,
            child,
            clone_thread,
            state: ForkState::Prepared,
        })
    }
    pub const fn parent(&self) -> KeyTaskOwner {
        self.parent
    }
    pub const fn child(&self) -> KeyTaskOwner {
        self.child
    }
    pub const fn clone_thread(&self) -> bool {
        self.clone_thread
    }
    pub fn commit(&mut self) -> Result<(), KeyError> {
        if self.state != ForkState::Prepared {
            return Err(KeyError::State);
        }
        self.state = ForkState::Committed;
        Ok(())
    }
    pub fn cancel(&mut self) -> Result<(), KeyError> {
        if self.state != ForkState::Prepared {
            return Err(KeyError::State);
        }
        self.state = ForkState::Cancelled;
        Ok(())
    }
    pub fn rollback(&mut self) -> Result<(), KeyError> {
        if self.state != ForkState::Prepared && self.state != ForkState::Committed {
            return Err(KeyError::State);
        }
        self.state = ForkState::RolledBack;
        Ok(())
    }
    pub const fn is_terminal(&self) -> bool {
        !matches!(self.state, ForkState::Prepared)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecyclePlan {
    Exec {
        owner: KeyTaskOwner,
    },
    Exit {
        owner: KeyTaskOwner,
        final_thread: bool,
    },
}
pub const fn plan_exec(owner: KeyTaskOwner) -> LifecyclePlan {
    LifecyclePlan::Exec { owner }
}
pub const fn plan_exit(owner: KeyTaskOwner, final_thread: bool) -> LifecyclePlan {
    LifecyclePlan::Exit {
        owner,
        final_thread,
    }
}
#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests {
    use super::*;

    struct Graph {
        links: &'static [(u32, &'static [u32])],
        leaf_is_user: bool,
    }
    impl KeyGraph for Graph {
        type Owner = ();
        fn meta(&self, key: KeyId) -> Option<KeyMeta<Self::Owner>> {
            self.links
                .iter()
                .any(|(id, _)| *id == key.get())
                .then_some(KeyMeta {
                    kind: if self.leaf_is_user && key.get() == 4 {
                        KeyKind::User
                    } else {
                        KeyKind::Keyring
                    },
                    owner: (),
                    permissions: KeyPermissions::SEARCH,
                    payload_len: 0,
                    revoked: false,
                })
        }
        fn links(&self, key: KeyId, out: &mut Vec<KeyId>) -> Result<(), KeyError> {
            let (_, links) = self
                .links
                .iter()
                .find(|(id, _)| *id == key.get())
                .ok_or(KeyError::NotFound)?;
            for &id in *links {
                out.push(KeyId::new(id).unwrap());
            }
            Ok(())
        }
    }
    #[test]
    fn quota() {
        assert_eq!(
            QuotaPlan::replace(
                QuotaUsage { keys: 1, bytes: 1 },
                QuotaCharge { keys: 1, bytes: 1 },
                QuotaCharge { keys: 2, bytes: 3 },
                QuotaLimit { keys: 2, bytes: 3 }
            )
            .unwrap()
            .after,
            QuotaUsage { keys: 2, bytes: 3 }
        )
    }
    #[test]
    fn fork() {
        let mut f = ForkPlan::prepare(
            KeyTaskOwner {
                thread: TaskOwnerId::new(1).unwrap(),
                process: ProcessOwnerId::new(1).unwrap(),
            },
            KeyTaskOwner {
                thread: TaskOwnerId::new(2).unwrap(),
                process: ProcessOwnerId::new(2).unwrap(),
            },
            false,
        )
        .unwrap();
        f.commit().unwrap();
        assert!(f.is_terminal())
    }
    #[test]
    fn quota_admission_transfer_and_retire() {
        let use_ = QuotaUsage { keys: 3, bytes: 30 };
        let charge = QuotaCharge { keys: 1, bytes: 10 };
        assert_eq!(
            QuotaPlan::retire(use_, charge).unwrap().after,
            QuotaUsage { keys: 2, bytes: 20 }
        );
        assert_eq!(
            QuotaPlan::transfer(
                use_,
                QuotaUsage::default(),
                charge,
                QuotaLimit { keys: 1, bytes: 10 },
                QuotaAdmission::Enforced,
                false,
            )
            .unwrap()
            .unwrap()
            .1
            .after,
            QuotaUsage { keys: 1, bytes: 10 },
        );
        assert_eq!(
            QuotaPlan::admit_replace(
                QuotaUsage { keys: 2, bytes: 2 },
                QuotaCharge { keys: 1, bytes: 1 },
                QuotaCharge::default(),
                QuotaLimit { keys: 0, bytes: 0 },
                QuotaAdmission::Enforced
            )
            .unwrap()
            .unwrap()
            .after,
            QuotaUsage { keys: 1, bytes: 1 },
        );
    }
    #[test]
    fn quota_counter_overflow_is_quota_exhaustion() {
        assert_eq!(
            QuotaPlan::admit_replace(
                QuotaUsage {
                    keys: usize::MAX,
                    bytes: 0,
                },
                QuotaCharge::default(),
                QuotaCharge { keys: 1, bytes: 0 },
                QuotaLimit {
                    keys: usize::MAX,
                    bytes: usize::MAX,
                },
                QuotaAdmission::Enforced,
            ),
            Err(KeyError::Quota),
        );
    }
    #[test]
    fn permissions_select_possessor_then_identity_lane() {
        let lanes = PermissionLanes {
            possessor: Some(KeyPermissions::READ),
            owner: Some(KeyPermissions::WRITE),
            group: None,
            other: Some(KeyPermissions::VIEW),
        };
        assert!(permits(
            lanes,
            PermissionInput {
                possessed: true,
                owner: false,
                group: false
            },
            KeyPermissions::READ
        ));
        assert!(permits(
            lanes,
            PermissionInput {
                possessed: false,
                owner: true,
                group: false
            },
            KeyPermissions::WRITE
        ));
        assert!(!permits(
            lanes,
            PermissionInput {
                possessed: false,
                owner: false,
                group: true
            },
            KeyPermissions::VIEW
        ));
        assert_eq!(
            check_available(false, false, true, false),
            Err(KeyError::Invalid)
        );
    }
    #[test]
    fn key_kind_payload_and_update_policy_are_centralized() {
        assert_eq!(KeyKind::User.payload_limit(), 32_767);
        assert_eq!(KeyKind::BigKey.payload_limit(), 1 << 20);
        assert!(!KeyKind::Keyring.supports_payload_update());
        assert!(!KeyKind::Logon.readable());
    }
    #[test]
    fn special_keyring_resolution_and_session_fallback_are_explicit() {
        assert_eq!(resolve_special_keyring(-1), Ok(SpecialKeyring::Thread));
        assert_eq!(resolve_special_keyring(-5), Ok(SpecialKeyring::UserSession));
        assert_eq!(resolve_special_keyring(-99), Err(KeyError::NotFound));
        assert_eq!(
            plan_missing_session(false),
            MissingSessionPlan::InstallUserSession
        );
    }

    #[test]
    fn keyctl_uapi_decodes_output_and_move_policy() {
        use uapi::{KeyctlPlan, KeyctlUapiError, RawKeyctlArgs, decode_keyctl};

        assert_eq!(
            decode_keyctl(RawKeyctlArgs {
                option: 31,
                arg2: 0x1000,
                arg3: 64,
                arg4: 0,
                arg5: 0
            }),
            Ok(KeyctlPlan::Capabilities {
                output: uapi::UserBuffer {
                    address: 0x1000,
                    len: 64
                }
            })
        );
        assert_eq!(
            decode_keyctl(RawKeyctlArgs {
                option: 30,
                arg2: 1,
                arg3: 2,
                arg4: 3,
                arg5: 2
            }),
            Err(KeyctlUapiError::Invalid)
        );
    }

    #[test]
    fn keyctl_uapi_rejects_oversized_update_before_usercopy() {
        use uapi::{KEYCTL_UPDATE_PAYLOAD_MAX, KeyctlUapiError, RawKeyctlArgs, decode_keyctl};

        assert_eq!(
            decode_keyctl(RawKeyctlArgs {
                option: 2,
                arg2: 1,
                arg3: 0,
                arg4: KEYCTL_UPDATE_PAYLOAD_MAX + 1,
                arg5: 0,
            }),
            Err(KeyctlUapiError::Invalid)
        );
    }
    #[test]
    fn bfs_possession_deduplicates_and_honors_search_gate() {
        let graph = Graph {
            links: &[(1, &[2, 3]), (2, &[4]), (3, &[4]), (4, &[])],
            leaf_is_user: true,
        };
        let id = |n| KeyId::new(n).unwrap();
        assert!(
            is_possessed(&graph, &[id(1)], id(4), 4, 3, |key| Ok(TraversalNode {
                available: true,
                searchable: key != id(3),
                keyring: true,
            }))
            .unwrap()
        );
        assert!(
            !is_possessed(&graph, &[id(1)], id(4), 4, 1, |_| Ok(TraversalNode {
                available: true,
                searchable: true,
                keyring: true,
            }))
            .unwrap()
        );
        assert!(
            !is_possessed(&graph, &[id(1)], id(2), 4, 3, |key| Ok(TraversalNode {
                available: true,
                searchable: key != id(2),
                keyring: true,
            }))
            .unwrap()
        );
        assert_eq!(
            bfs(
                &graph,
                BfsRequest {
                    roots: &[id(1)],
                    max_visits: 1,
                    max_depth: 3
                },
                |_| Ok(TraversalNode {
                    available: true,
                    searchable: true,
                    keyring: true
                }),
                |_, _| false,
            ),
            Err(KeyError::Limit),
        );
    }
    #[test]
    fn link_limit_is_depth_not_total_nodes() {
        let graph = Graph {
            links: &[
                (1, &[2]),
                (2, &[3]),
                (3, &[4]),
                (4, &[5]),
                (5, &[6]),
                (6, &[7]),
                (7, &[]),
                (8, &[]),
            ],
            leaf_is_user: false,
        };
        assert_eq!(
            plan_link(&graph, KeyId::new(8).unwrap(), KeyId::new(1).unwrap(), 6),
            Ok(GraphPlan::Link {
                from: KeyId::new(8).unwrap(),
                to: KeyId::new(1).unwrap()
            }),
        );
    }
    #[test]
    fn gc_scratch_requires_ordered_transitions() {
        let mut scratch = GcScratch::touch(1).unwrap();
        scratch.add_root_drop(1).unwrap();
        scratch.add_link_drop(1).unwrap();
        assert!(scratch.queue_if_unreferenced(1, 1).unwrap());
        scratch.retire(1, 1).unwrap();
        scratch.clear();
        assert_eq!(scratch, GcScratch::IDLE);
    }
}
