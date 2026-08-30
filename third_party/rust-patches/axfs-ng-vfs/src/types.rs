use core::{fmt::Debug, time::Duration};

/// A filesystem timestamp with a signed Unix epoch second.
///
/// Unlike [`Duration`], this can represent pre-1970 inode times.  Nanoseconds
/// are normalized to the conventional `0..1_000_000_000` range.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct Timestamp {
    seconds: i64,
    nanoseconds: u32,
}

impl Timestamp {
    /// Unix epoch.
    pub const ZERO: Self = Self::new(0, 0);

    /// Constructs a normalized timestamp, returning `None` for invalid ns.
    pub const fn try_new(seconds: i64, nanoseconds: u32) -> Option<Self> {
        if nanoseconds < 1_000_000_000 {
            Some(Self {
                seconds,
                nanoseconds,
            })
        } else {
            None
        }
    }

    /// Constructs a timestamp from already validated parts.
    pub const fn new(seconds: i64, nanoseconds: u32) -> Self {
        assert!(nanoseconds < 1_000_000_000);
        Self {
            seconds,
            nanoseconds,
        }
    }

    /// Returns signed seconds from the Unix epoch.
    pub const fn seconds(self) -> i64 {
        self.seconds
    }

    /// Returns the normalized nanosecond component.
    pub const fn subsec_nanos(self) -> u32 {
        self.nanoseconds
    }

    /// Converts to the legacy unsigned representation when possible.
    pub const fn try_into_duration(self) -> Option<Duration> {
        if self.seconds < 0 {
            None
        } else {
            Some(Duration::new(self.seconds as u64, self.nanoseconds))
        }
    }
}

impl From<Duration> for Timestamp {
    fn from(value: Duration) -> Self {
        Self::new(
            value.as_secs().min(i64::MAX as u64) as i64,
            value.subsec_nanos(),
        )
    }
}

/// Filesystem node type.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NodeType {
    Unknown         = 0,
    Fifo            = 0o1,
    CharacterDevice = 0o2,
    Directory       = 0o4,
    BlockDevice     = 0o6,
    RegularFile     = 0o10,
    Symlink         = 0o12,
    Socket          = 0o14,
}

impl From<u8> for NodeType {
    fn from(value: u8) -> Self {
        match value {
            0o1 => Self::Fifo,
            0o2 => Self::CharacterDevice,
            0o4 => Self::Directory,
            0o6 => Self::BlockDevice,
            0o10 => Self::RegularFile,
            0o12 => Self::Symlink,
            0o14 => Self::Socket,
            _ => Self::Unknown,
        }
    }
}

bitflags::bitflags! {
    /// Inode permission mode.
    #[derive(Debug, Clone, Copy)]
    pub struct NodePermission: u16 {
        /// Set user ID on execution.
        const SET_UID = 0o4000;
        /// Set group ID on execution.
        const SET_GID = 0o2000;
        /// Sticky bit.
        const STICKY = 0o1000;

        /// Owner has read permission.
        const OWNER_READ = 0o400;
        /// Owner has write permission.
        const OWNER_WRITE = 0o200;
        /// Owner has execute permission.
        const OWNER_EXEC = 0o100;

        /// Group has read permission.
        const GROUP_READ = 0o40;
        /// Group has write permission.
        const GROUP_WRITE = 0o20;
        /// Group has execute permission.
        const GROUP_EXEC = 0o10;

        /// Others have read permission.
        const OTHER_READ = 0o4;
        /// Others have write permission.
        const OTHER_WRITE = 0o2;
        /// Others have execute permission.
        const OTHER_EXEC = 0o1;
    }
}

impl Default for NodePermission {
    fn default() -> Self {
        Self::from_bits_truncate(0o666)
    }
}

bitflags::bitflags! {
    /// Metadata fields that a filesystem can persist for an inode.
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct MetadataUpdateCapabilities: u8 {
        const MODE  = 1 << 0;
        const OWNER = 1 << 1;
        const RDEV  = 1 << 2;
        const ATIME = 1 << 3;
        const MTIME = 1 << 4;
        const CTIME = 1 << 5;

        const ALL = Self::MODE.bits()
            | Self::OWNER.bits()
            | Self::RDEV.bits()
            | Self::ATIME.bits()
            | Self::MTIME.bits()
            | Self::CTIME.bits();
    }
}

/// Filesystem node metadata.
#[derive(Clone, Debug)]
pub struct Metadata {
    /// ID of device containing file
    pub device: u64,
    /// Inode number
    pub inode: u64,
    /// Number of hard links
    pub nlink: u64,
    /// Permission mode
    pub mode: NodePermission,
    /// Node type
    pub node_type: NodeType,
    /// User ID of owner
    pub uid: u32,
    /// Group ID of owner
    pub gid: u32,
    /// Total size in bytes
    pub size: u64,
    /// Block size for filesystem I/O
    pub block_size: u64,
    /// Number of 512B blocks allocated
    pub blocks: u64,
    /// Device ID (if special file)
    pub rdev: DeviceId,

    /// Time of last access
    pub atime: Timestamp,
    /// Time of creation
    pub btime: Timestamp,
    /// Time of last modification
    pub mtime: Timestamp,
    /// Time of last status change
    pub ctime: Timestamp,
}

/// Filesystem node metadata update.
#[derive(Default, Clone, Debug)]
pub struct MetadataUpdate {
    /// Permission mode
    pub mode: Option<NodePermission>,
    /// The owner (uid, gid)
    pub owner: Option<(u32, u32)>,
    /// Device ID for special files
    pub rdev: Option<DeviceId>,

    /// Time of last access
    pub atime: Option<Timestamp>,
    /// Time of last modification
    pub mtime: Option<Timestamp>,
    /// Time of last status change
    pub ctime: Option<Timestamp>,
}

impl MetadataUpdate {
    pub fn retain_supported(&mut self, capabilities: MetadataUpdateCapabilities) {
        if !capabilities.contains(MetadataUpdateCapabilities::MODE) {
            self.mode = None;
        }
        if !capabilities.contains(MetadataUpdateCapabilities::OWNER) {
            self.owner = None;
        }
        if !capabilities.contains(MetadataUpdateCapabilities::RDEV) {
            self.rdev = None;
        }
        if !capabilities.contains(MetadataUpdateCapabilities::ATIME) {
            self.atime = None;
        }
        if !capabilities.contains(MetadataUpdateCapabilities::MTIME) {
            self.mtime = None;
        }
        if !capabilities.contains(MetadataUpdateCapabilities::CTIME) {
            self.ctime = None;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.mode.is_none()
            && self.owner.is_none()
            && self.rdev.is_none()
            && self.atime.is_none()
            && self.mtime.is_none()
            && self.ctime.is_none()
    }
}

/// Device Id
#[derive(Default, Clone, PartialEq, Eq, Copy)]
pub struct DeviceId(pub u64);

impl DeviceId {
    pub const fn new(major: u32, minor: u32) -> Self {
        let major = major as u64;
        let minor = minor as u64;
        Self(
            (major & 0xffff_f000) << 32
                | (major & 0x0000_0fff) << 8
                | (minor & 0xffff_ff00) << 12
                | (minor & 0x0000_00ff),
        )
    }

    pub const fn major(&self) -> u32 {
        ((self.0 >> 32) & 0xffff_f000 | (self.0 >> 8) & 0x0000_0fff) as u32
    }

    pub const fn minor(&self) -> u32 {
        ((self.0 >> 12) & 0xffff_ff00 | self.0 & 0x0000_00ff) as u32
    }
}

impl Debug for DeviceId {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("DeviceId")
            .field("major", &self.major())
            .field("minor", &self.minor())
            .finish()
    }
}
