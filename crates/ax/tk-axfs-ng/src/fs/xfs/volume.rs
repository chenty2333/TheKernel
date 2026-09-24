//! The XFS volume: its data, external-log and realtime devices.

use super::*;

/// A read-only view of a data device plus optional external log and realtime
/// devices.  Device membership is explicit: an XFS external log/realtime
/// device is never guessed from a pathname or a device number.
pub struct XfsVolume {
    pub(super) data: BlockVolume,
    pub(super) external_log: Option<BlockVolume>,
    pub(super) realtime: Option<BlockVolume>,
    pub(super) rtgroup_inodes: Vec<(u64, u64)>,
    pub(super) superblock: XfsSuperblock,
    /// Serializes durable home-block replay.  The log itself supplies the
    /// transaction order; this lock only prevents two recovery/teardown
    /// callers from observing and advancing the same home LSN concurrently.
    pub(super) replay_lock: SpinMutex<()>,
    // Keeps the mount claim alive for `probe`.  Generic `open` takes already
    // owned volumes and intentionally leaves claim ownership with its caller.
    pub(super) _data_claim: Option<MountedBlockDevice>,
}
