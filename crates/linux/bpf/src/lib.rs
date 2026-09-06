//! Pure Linux eBPF admission and lifecycle policy.
#![no_std]
#![forbid(unsafe_code)]
#![allow(missing_docs)]
extern crate alloc;
pub mod uapi;
use alloc::vec::Vec;
use core::num::NonZeroU32;
pub use uapi::*;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BpfError {
    Invalid,
    NotFound,
    Exists,
    Frozen,
    Limit,
    Overflow,
    Stale,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum BpfCommand {
    MapCreate = 0,
    MapLookupElem = 1,
    MapUpdateElem = 2,
    MapDeleteElem = 3,
    MapGetNextKey = 4,
    ProgLoad = 5,
    ObjPin = 6,
    ObjGet = 7,
    ProgAttach = 8,
    ProgDetach = 9,
    ProgTestRun = 10,
    ProgGetNextId = 11,
    MapGetNextId = 12,
    ProgGetFdById = 13,
    MapGetFdById = 14,
    ObjGetInfoByFd = 15,
    ProgQuery = 16,
    RawTracepointOpen = 17,
    BtfLoad = 18,
    BtfGetFdById = 19,
    TaskFdQuery = 20,
    MapLookupAndDeleteElem = 21,
    MapFreeze = 22,
    BtfGetNextId = 23,
    MapLookupBatch = 24,
    MapLookupAndDeleteBatch = 25,
    MapUpdateBatch = 26,
    MapDeleteBatch = 27,
    LinkCreate = 28,
    LinkUpdate = 29,
    LinkGetFdById = 30,
    LinkGetNextId = 31,
    EnableStats = 32,
    IterCreate = 33,
    LinkDetach = 34,
    ProgBindMap = 35,
    TokenCreate = 36,
    ProgStreamReadByFd = 37,
}
impl TryFrom<u32> for BpfCommand {
    type Error = BpfError;
    fn try_from(v: u32) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(Self::MapCreate),
            1 => Ok(Self::MapLookupElem),
            2 => Ok(Self::MapUpdateElem),
            3 => Ok(Self::MapDeleteElem),
            4 => Ok(Self::MapGetNextKey),
            5 => Ok(Self::ProgLoad),
            6 => Ok(Self::ObjPin),
            7 => Ok(Self::ObjGet),
            8 => Ok(Self::ProgAttach),
            9 => Ok(Self::ProgDetach),
            10 => Ok(Self::ProgTestRun),
            11 => Ok(Self::ProgGetNextId),
            12 => Ok(Self::MapGetNextId),
            13 => Ok(Self::ProgGetFdById),
            14 => Ok(Self::MapGetFdById),
            15 => Ok(Self::ObjGetInfoByFd),
            16 => Ok(Self::ProgQuery),
            17 => Ok(Self::RawTracepointOpen),
            18 => Ok(Self::BtfLoad),
            19 => Ok(Self::BtfGetFdById),
            20 => Ok(Self::TaskFdQuery),
            21 => Ok(Self::MapLookupAndDeleteElem),
            22 => Ok(Self::MapFreeze),
            23 => Ok(Self::BtfGetNextId),
            24 => Ok(Self::MapLookupBatch),
            25 => Ok(Self::MapLookupAndDeleteBatch),
            26 => Ok(Self::MapUpdateBatch),
            27 => Ok(Self::MapDeleteBatch),
            28 => Ok(Self::LinkCreate),
            29 => Ok(Self::LinkUpdate),
            30 => Ok(Self::LinkGetFdById),
            31 => Ok(Self::LinkGetNextId),
            32 => Ok(Self::EnableStats),
            33 => Ok(Self::IterCreate),
            34 => Ok(Self::LinkDetach),
            35 => Ok(Self::ProgBindMap),
            36 => Ok(Self::TokenCreate),
            37 => Ok(Self::ProgStreamReadByFd),
            _ => Err(BpfError::Invalid),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapProfile {
    Array,
    Hash,
    /// An array of retained program references used by BPF tail-call sites.
    ProgArray,
    PerfEventArray,
    /// A hash map which evicts its least-recently-used entry at capacity.
    LruHash,
    PerCpuHash,
    PerCpuArray,
    LpmTrie,
    Queue,
    Stack,
    SockMap,
    SockHash,
    RingBuf,
    DevMap,
    CpuMap,
    XskMap,
}
impl TryFrom<u32> for MapProfile {
    type Error = BpfError;
    fn try_from(v: u32) -> Result<Self, Self::Error> {
        match v {
            1 => Ok(Self::Hash),
            2 => Ok(Self::Array),
            3 => Ok(Self::ProgArray),
            4 => Ok(Self::PerfEventArray),
            9 => Ok(Self::LruHash),
            5 => Ok(Self::PerCpuHash),
            6 => Ok(Self::PerCpuArray),
            11 => Ok(Self::LpmTrie),
            22 => Ok(Self::Queue),
            23 => Ok(Self::Stack),
            15 => Ok(Self::SockMap),
            18 => Ok(Self::SockHash),
            27 => Ok(Self::RingBuf),
            14 => Ok(Self::DevMap),
            16 => Ok(Self::CpuMap),
            17 => Ok(Self::XskMap),
            _ => Err(BpfError::Invalid),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgramProfile {
    SocketFilter,
    Tracepoint,
    PerfEvent,
    RawTracepoint,
}
impl TryFrom<u32> for ProgramProfile {
    type Error = BpfError;
    fn try_from(v: u32) -> Result<Self, Self::Error> {
        match v {
            1 => Ok(Self::SocketFilter),
            5 => Ok(Self::Tracepoint),
            7 => Ok(Self::PerfEvent),
            17 => Ok(Self::RawTracepoint),
            _ => Err(BpfError::Invalid),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelperProfile {
    MapLookup,
    MapUpdate,
    MapDelete,
    KtimeGetNs,
    GetCurrentPidTgid,
    GetCurrentUidGid,
    GetCurrentComm,
    RingBuf,
}
impl TryFrom<u32> for HelperProfile {
    type Error = BpfError;
    fn try_from(v: u32) -> Result<Self, Self::Error> {
        match v {
            1 => Ok(Self::MapLookup),
            2 => Ok(Self::MapUpdate),
            3 => Ok(Self::MapDelete),
            5 => Ok(Self::KtimeGetNs),
            14 => Ok(Self::GetCurrentPidTgid),
            15 => Ok(Self::GetCurrentUidGid),
            16 => Ok(Self::GetCurrentComm),
            130..=133 => Ok(Self::RingBuf),
            _ => Err(BpfError::Invalid),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct MapId(NonZeroU32);
impl MapId {
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
pub struct ProgramId(NonZeroU32);
impl ProgramId {
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BpfAttr {
    pub command: BpfCommand,
    pub object_type: u32,
    pub key_size: u32,
    pub value_size: u32,
    pub max_entries: u32,
    pub flags: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MapCreateRequest {
    pub profile: MapProfile,
    pub key_size: u32,
    pub value_size: u32,
    pub max_entries: u32,
}
impl MapCreateRequest {
    pub fn from_attr(a: BpfAttr) -> Result<Self, BpfError> {
        if a.command != BpfCommand::MapCreate
            || (a.flags != 0
                && !(a.object_type == BPF_MAP_TYPE_LPM_TRIE && a.flags == BPF_F_NO_PREALLOC))
            || a.max_entries == 0
        {
            return Err(BpfError::Invalid);
        }
        let p = MapProfile::try_from(a.object_type)?;
        if p != MapProfile::RingBuf && a.value_size == 0 {
            return Err(BpfError::Invalid);
        }
        if ((p == MapProfile::Array
            || p == MapProfile::ProgArray
            || p == MapProfile::PerfEventArray
            || p == MapProfile::PerCpuArray
            || p == MapProfile::SockMap)
            && a.key_size != 4)
            || (p == MapProfile::ProgArray && a.value_size != 4)
            || (p == MapProfile::PerfEventArray && a.value_size != 4)
            || (p == MapProfile::RingBuf && (a.key_size != 0 || a.value_size != 0))
            || ((p == MapProfile::DevMap || p == MapProfile::CpuMap || p == MapProfile::XskMap)
                && a.key_size != 4)
            || (p == MapProfile::DevMap && a.value_size != 8)
            || (p == MapProfile::CpuMap && a.value_size != 8)
            || (p == MapProfile::XskMap && a.value_size != 4)
            || ((p == MapProfile::Queue || p == MapProfile::Stack) && a.key_size != 0)
            || (p == MapProfile::LpmTrie && a.key_size < 4)
            || ((p == MapProfile::SockMap || p == MapProfile::SockHash) && a.value_size != 4)
        {
            return Err(BpfError::Invalid);
        }
        Ok(Self {
            profile: p,
            key_size: a.key_size,
            value_size: a.value_size,
            max_entries: a.max_entries,
        })
    }
    pub fn reservation_bytes(self) -> Result<usize, BpfError> {
        match self.profile {
            MapProfile::RingBuf => {
                usize::try_from(self.max_entries).map_err(|_| BpfError::Overflow)
            }
            MapProfile::ProgArray | MapProfile::PerfEventArray => usize::try_from(self.max_entries)
                .map_err(|_| BpfError::Overflow)?
                .checked_mul(core::mem::size_of::<u32>())
                .ok_or(BpfError::Overflow),
            _ => usize::try_from(self.value_size)
                .map_err(|_| BpfError::Overflow)?
                .checked_mul(usize::try_from(self.max_entries).map_err(|_| BpfError::Overflow)?)
                .ok_or(BpfError::Overflow),
        }
    }
}
pub trait MapToken: Copy + Eq {}
impl<T: Copy + Eq> MapToken for T {}
pub trait MapResolver {
    type Token: MapToken;
    fn resolve(&self, handle: u32) -> Result<(MapId, Self::Token), BpfError>;
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramLoadRequest {
    pub profile: ProgramProfile,
    pub instruction_count: u32,
    pub map_handles: Vec<u32>,
    pub helpers: Vec<HelperProfile>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedProgram<T: MapToken> {
    pub profile: ProgramProfile,
    pub instruction_count: u32,
    pub maps: Vec<(MapId, T)>,
    pub helpers: Vec<HelperProfile>,
}
impl ProgramLoadRequest {
    pub fn verify<R: MapResolver>(
        &self,
        r: &R,
        max: u32,
    ) -> Result<VerifiedProgram<R::Token>, BpfError> {
        if self.instruction_count == 0
            || self.instruction_count > max
            || self.map_handles.len() > 64
            || self.helpers.len() > 64
        {
            return Err(BpfError::Limit);
        }
        let mut maps = Vec::new();
        maps.try_reserve(self.map_handles.len())
            .map_err(|_| BpfError::Limit)?;
        for &h in &self.map_handles {
            maps.push(r.resolve(h)?)
        }
        Ok(VerifiedProgram {
            profile: self.profile,
            instruction_count: self.instruction_count,
            maps,
            helpers: self.helpers.clone(),
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MapState {
    pub profile: MapProfile,
    pub frozen: bool,
    pub attachments: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecyclePlan {
    CreateMap { id: MapId, state: MapState },
    Freeze { id: MapId },
    Attach { program: ProgramId, map: MapId },
    Detach { program: ProgramId, map: MapId },
}
impl MapState {
    pub const fn new(profile: MapProfile) -> Self {
        Self {
            profile,
            frozen: false,
            attachments: 0,
        }
    }
    pub fn plan_freeze(self, id: MapId) -> Result<LifecyclePlan, BpfError> {
        if self.frozen {
            Err(BpfError::Frozen)
        } else {
            Ok(LifecyclePlan::Freeze { id })
        }
    }
}
#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests {
    use super::*;
    struct R;
    impl MapResolver for R {
        type Token = u8;
        fn resolve(&self, h: u32) -> Result<(MapId, u8), BpfError> {
            Ok((MapId::new(h).ok_or(BpfError::Invalid)?, 7))
        }
    }
    #[test]
    fn bounded() {
        let r = MapCreateRequest::from_attr(BpfAttr {
            command: BpfCommand::MapCreate,
            object_type: 2,
            key_size: 4,
            value_size: 8,
            max_entries: 2,
            flags: 0,
        })
        .unwrap();
        assert_eq!(r.reservation_bytes(), Ok(16));
    }
    #[test]
    fn resolver() {
        let p = ProgramLoadRequest {
            profile: ProgramProfile::SocketFilter,
            instruction_count: 1,
            map_handles: alloc::vec![3],
            helpers: alloc::vec![],
        };
        assert_eq!(p.verify(&R, 1).unwrap().maps[0].0.get(), 3)
    }
}
