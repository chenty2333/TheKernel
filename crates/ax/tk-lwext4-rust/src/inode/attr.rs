use core::time::Duration;

use super::{InodeRef, InodeType};
use crate::{SystemHal, ffi::*, util::get_block_size};

/// Filesystem node metadata.
#[derive(Clone, Debug, Default)]
pub struct FileAttr {
    /// ID of device containing file
    pub device: u64,
    /// Inode number
    pub ino: u32,
    /// Number of hard links
    pub nlink: u64,
    /// Permission mode
    pub mode: u32,
    /// Type of file
    pub node_type: InodeType,
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
    /// Device ID for special files
    pub rdev: u64,
    /// Native ext4 inode flags.
    pub flags: u32,
    /// Native ext4 project identifier.
    pub project_id: u32,

    /// Time of last access
    pub atime: Timestamp,
    /// Time of creation
    pub btime: Timestamp,
    /// Time of last modification
    pub mtime: Timestamp,
    /// Time of last status change
    pub ctime: Timestamp,
}

/// ext4's signed 34-bit timestamp representation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Timestamp {
    seconds: i64,
    nanoseconds: u32,
}

impl Timestamp {
    pub const ZERO: Self = Self::new(0, 0);
    pub const MIN_SECONDS: i64 = i32::MIN as i64;
    pub const MAX_SECONDS: i64 = (3_i64 << 32) + i32::MAX as i64;

    pub const fn new(seconds: i64, nanoseconds: u32) -> Self {
        Self {
            seconds,
            nanoseconds,
        }
    }

    pub const fn seconds(self) -> i64 {
        self.seconds
    }

    pub const fn subsec_nanos(self) -> u32 {
        self.nanoseconds
    }

    pub const fn is_ext4_representable(self) -> bool {
        self.nanoseconds < 1_000_000_000
            && self.seconds >= Self::MIN_SECONDS
            && self.seconds <= Self::MAX_SECONDS
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

impl From<&Duration> for Timestamp {
    fn from(value: &Duration) -> Self {
        (*value).into()
    }
}

impl PartialEq<Duration> for Timestamp {
    fn eq(&self, other: &Duration) -> bool {
        self.seconds >= 0
            && self.seconds as u64 == other.as_secs()
            && self.nanoseconds == other.subsec_nanos()
    }
}

impl PartialEq<Timestamp> for Duration {
    fn eq(&self, other: &Timestamp) -> bool {
        other == self
    }
}

fn encode_time(time: Timestamp) -> (u32, u32) {
    debug_assert!(time.is_ext4_representable());
    let sec = time.seconds();
    let nsec = time.subsec_nanos();
    let low = sec as i32;
    let epoch = (sec - low as i64) >> 32;
    let time = u32::to_le(low as u32);
    let extra = u32::to_le((nsec << 2) | epoch as u32);
    (time, extra)
}
fn decode_time(time: u32, extra: u32) -> Timestamp {
    let low = u32::from_le(time) as i32;
    let extra = u32::from_le(extra);
    let epoch = extra & 3;
    let nsec = extra >> 2;

    Timestamp::new(low as i64 + ((epoch as i64) << 32), nsec)
}

impl<Hal: SystemHal> InodeRef<Hal> {
    pub fn inode_type(&self) -> InodeType {
        ((self.mode() >> 12) as u8).into()
    }

    pub fn is_dir(&self) -> bool {
        self.inode_type() == InodeType::Directory
    }

    pub fn size(&self) -> u64 {
        unsafe { ext4_inode_get_size(self.superblock() as *const _ as _, self.inner.inode) }
    }

    pub fn mode(&self) -> u32 {
        unsafe { ext4_inode_get_mode(self.superblock() as *const _ as _, self.inner.inode) }
    }
    pub fn set_mode(&mut self, mode: u32) {
        unsafe {
            ext4_inode_set_mode(self.superblock_mut(), self.inner.inode, mode);
            self.mark_dirty();
        }
    }

    pub fn nlink(&self) -> u16 {
        u16::from_le(self.raw_inode().links_count)
    }

    pub fn uid(&self) -> u32 {
        unsafe { ext4_inode_get_uid(self.inner.inode) }
    }
    pub fn gid(&self) -> u32 {
        unsafe { ext4_inode_get_gid(self.inner.inode) }
    }

    pub fn set_owner(&mut self, uid: u32, gid: u32) {
        unsafe {
            ext4_inode_set_uid(self.inner.inode, uid);
            ext4_inode_set_gid(self.inner.inode, gid);
            self.mark_dirty();
        }
    }

    pub fn rdev(&self) -> u64 {
        unsafe { ext4_inode_get_dev(self.inner.inode) as u64 }
    }

    /// Native ext4 inode flags (`i_flags`).
    pub fn flags(&self) -> u32 {
        unsafe { ext4_inode_get_flags(self.inner.inode) }
    }

    pub fn set_flags(&mut self, flags: u32) {
        unsafe { ext4_inode_set_flags(self.inner.inode, flags) }
        self.mark_dirty();
    }

    pub fn is_immutable(&self) -> bool {
        self.flags() & 0x0000_0010 != 0
    }

    pub fn is_append_only(&self) -> bool {
        self.flags() & 0x0000_0020 != 0
    }

    /// Native ext4 project identifier (`i_projid`).
    pub fn project_id(&self) -> u32 {
        u32::from_le(self.raw_inode().projid)
    }

    pub fn set_project_id(&mut self, project_id: u32) {
        self.raw_inode_mut().projid = u32::to_le(project_id);
        self.mark_dirty();
    }

    pub fn set_rdev(&mut self, rdev: u64) {
        unsafe {
            ext4_inode_set_dev(self.inner.inode, rdev as u32);
            self.mark_dirty();
        }
    }

    pub fn set_atime(&mut self, time: impl Into<Timestamp>) {
        let (time, extra) = encode_time(time.into());
        let inode = self.raw_inode_mut();
        inode.access_time = time;
        inode.atime_extra = extra;
        self.mark_dirty();
    }
    pub fn set_mtime(&mut self, time: impl Into<Timestamp>) {
        let (time, extra) = encode_time(time.into());
        let inode = self.raw_inode_mut();
        inode.modification_time = time;
        inode.mtime_extra = extra;
        self.mark_dirty();
    }
    pub fn set_ctime(&mut self, time: impl Into<Timestamp>) {
        let (time, extra) = encode_time(time.into());
        let inode = self.raw_inode_mut();
        inode.change_inode_time = time;
        inode.ctime_extra = extra;
        self.mark_dirty();
    }
    pub fn set_btime(&mut self, time: impl Into<Timestamp>) {
        let (time, extra) = encode_time(time.into());
        let inode = self.raw_inode_mut();
        inode.crtime = time;
        inode.crtime_extra = extra;
        self.mark_dirty();
    }

    pub fn update_atime(&mut self) {
        if let Some(dur) = Hal::now() {
            self.set_atime(dur);
        }
    }
    pub fn update_mtime(&mut self) {
        if let Some(dur) = Hal::now() {
            self.set_mtime(dur);
        }
    }
    pub fn update_ctime(&mut self) {
        if let Some(dur) = Hal::now() {
            self.set_ctime(dur);
        }
    }

    pub fn get_attr(&self, attr: &mut FileAttr) {
        attr.device = 0;
        attr.ino = u32::from_le(self.inner.index);
        attr.nlink = self.nlink() as _;
        attr.mode = self.mode();
        attr.node_type = self.inode_type();
        attr.uid = self.uid();
        attr.gid = self.gid();
        attr.size = self.size();
        attr.block_size = get_block_size(self.superblock()) as _;
        attr.blocks = unsafe {
            ext4_inode_get_blocks_count(self.superblock() as *const _ as _, self.inner.inode)
        };
        attr.rdev = self.rdev();
        attr.flags = self.flags();
        attr.project_id = self.project_id();

        let inode = self.raw_inode();
        attr.atime = decode_time(inode.access_time, inode.atime_extra);
        attr.btime = decode_time(inode.crtime, inode.crtime_extra);
        attr.mtime = decode_time(inode.modification_time, inode.mtime_extra);
        attr.ctime = decode_time(inode.change_inode_time, inode.ctime_extra);
    }
}

#[cfg(test)]
mod tests {
    use super::{Timestamp, decode_time, encode_time};

    #[test]
    fn ext4_signed_34_bit_time_round_trips_pre_epoch_values() {
        for time in [
            Timestamp::new(-1, 123),
            Timestamp::new(Timestamp::MIN_SECONDS, 999_999_999),
            Timestamp::new(0, 0),
            Timestamp::new(Timestamp::MAX_SECONDS, 1),
        ] {
            let (seconds, extra) = encode_time(time);
            assert_eq!(decode_time(seconds, extra), time);
        }
    }
}
