//! Stable, multi-device block address spaces for filesystem mounts.
//!
//! A volume owns the shared queue objects rather than a mutable driver guard.
//! This keeps device identity and async-completion custody stable across a
//! mount's lifetime, while presenting one contiguous logical block space.

use alloc::{sync::Arc, vec::Vec};

use axdriver_block::{
    BlockCapabilities, BlockCompletion, BlockCompletionDrain, BlockDriverOps, BlockGeometry,
    BlockQueueRequest, BlockRange, BlockRequestHandle, BlockSubmitReport, DevError, DevResult,
};
use axsync::Mutex;

use crate::SharedBlockDevice;

/// Immutable identity and extent of one physical member of a volume.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct BlockVolumeDevice {
    /// Index in the volume's fixed routing table.
    pub index: usize,
    /// Opaque identity of the underlying shared queue owner.
    pub identity: usize,
    /// Member geometry, always using the volume block size.
    pub geometry: BlockGeometry,
    /// First volume block routed to this member.
    pub volume_start: u64,
}

/// Deterministic storage operation selected by a fault rule.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BlockFaultOperation {
    /// Read operation.
    Read,
    /// Ordinary write operation.
    Write,
    /// FUA write operation.
    WriteFua,
    /// Persistence flush.
    Flush,
    /// Non-persistent ordering fence.
    Fence,
    /// Discard/deallocation.
    Discard,
    /// Logical zero write.
    WriteZeroes,
}

/// Lifetime of a deterministic injected fault.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BlockFaultLifetime {
    /// Consume the rule when it fires once.
    Once,
    /// Continue failing every matching operation after the trigger point.
    Persistent,
}

/// A deterministic fault rule. `successful_matches` matching operations pass
/// before the rule starts failing; this makes transaction/recovery test traces
/// reproducible without wall-clock or scheduler coupling.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct BlockFaultRule {
    /// Operation to intercept.
    pub operation: BlockFaultOperation,
    /// Physical member to intercept, or every member when `None`.
    pub device: Option<usize>,
    /// Number of matching calls which pass before failure.
    pub successful_matches: u64,
    /// Whether the rule is consumed after its first injected error.
    pub lifetime: BlockFaultLifetime,
}

#[derive(Debug, Clone, Copy)]
struct ActiveFaultRule(BlockFaultRule);

/// A mount-owned contiguous block address space assembled from fixed physical
/// members. Members must share a logical sector size; heterogeneous devices
/// need an explicit translation layer rather than implicit lossy rounding.
#[derive(Clone)]
pub struct BlockVolume {
    inner: Arc<BlockVolumeInner>,
}

struct BlockVolumeInner {
    members: Mutex<Arc<MemberMap>>,
    faults: Mutex<Vec<ActiveFaultRule>>,
}

pub struct MemberMap {
    geometry: BlockGeometry,
    members: Vec<VolumeMember>,
}

struct VolumeMember {
    descriptor: BlockVolumeDevice,
    device: SharedBlockDevice,
}

/// Handle for one member's existing async completion broker. A filesystem may
/// retain this as its completion owner without creating a second used-ring
/// consumer; all completion draining continues through `SharedBlockDevice`.
#[derive(Clone)]
pub struct BlockVolumeCompletionOwner {
    /// Fixed member descriptor.
    pub device: BlockVolumeDevice,
    queue: SharedBlockDevice,
}

impl BlockVolumeCompletionOwner {
    /// Returns the member queue. Its shared broker remains the sole lower
    /// completion owner and preserves physical/ordinary completion routing.
    pub fn queue(&self) -> &SharedBlockDevice {
        &self.queue
    }

    /// Publishes asynchronous requests to this member's existing completion
    /// broker. Request block addresses are member-local; a caller that starts
    /// from a volume address must first use the routing table above.
    pub fn submit_async_batch(
        &self,
        requests: &mut [BlockQueueRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        let mut queue = self.queue.clone();
        queue.submit_async_batch(requests)
    }

    /// Drains concrete completions owned by this member. The underlying shared
    /// broker keeps ordinary and physical completion identities disjoint.
    pub fn drain_completions(
        &self,
        output: &mut [BlockCompletion],
    ) -> DevResult<BlockCompletionDrain> {
        let mut queue = self.queue.clone();
        queue.drain_async_completions(output)
    }

    /// Waits for this member's submitted requests and returns their buffers to
    /// their caller only after the lower queue has retired each handle.
    pub fn wait_all(&self, handles: &[BlockRequestHandle]) -> DevResult {
        let mut queue = self.queue.clone();
        queue.wait_async_all(handles)
    }
}

impl BlockVolume {
    /// Assembles a contiguous volume from physical queues in the supplied
    /// order. Geometry is captured exactly once and cannot silently drift.
    pub fn new(devices: Vec<SharedBlockDevice>) -> DevResult<Self> {
        if devices.is_empty() {
            return Err(DevError::InvalidParam);
        }
        let map = Self::stage_member_map(devices)?;
        Ok(Self {
            inner: Arc::new(BlockVolumeInner {
                members: Mutex::new(map),
                faults: Mutex::new(Vec::new()),
            }),
        })
    }

    /// Validates and stages a complete routing table without making it
    /// visible.  The returned Arc is immutable: admitted I/O can retain it
    /// while a later topology commit publishes a different member map.
    pub fn stage_member_map(devices: Vec<SharedBlockDevice>) -> DevResult<Arc<MemberMap>> {
        let mut members = Vec::with_capacity(devices.len());
        let mut block_size = None;
        let mut volume_start = 0u64;
        for (index, device) in devices.into_iter().enumerate() {
            if members
                .iter()
                .any(|member: &VolumeMember| member.descriptor.identity == device.identity_token())
            {
                return Err(DevError::InvalidParam);
            }
            let geometry = device.block_geometry()?;
            if geometry.blocks == 0 {
                return Err(DevError::InvalidParam);
            }
            match block_size {
                Some(size) if size != geometry.block_size => return Err(DevError::InvalidParam),
                None => block_size = Some(geometry.block_size),
                _ => {}
            }
            let descriptor = BlockVolumeDevice {
                index,
                identity: device.identity_token(),
                geometry,
                volume_start,
            };
            volume_start = volume_start
                .checked_add(geometry.blocks)
                .ok_or(DevError::InvalidParam)?;
            members.push(VolumeMember { descriptor, device });
        }
        let geometry = BlockGeometry {
            block_size: block_size.ok_or(DevError::InvalidParam)?,
            blocks: volume_start,
        };
        Arc::try_new(MemberMap { geometry, members }).map_err(|_| DevError::NoMemory)
    }

    /// Atomically publishes a previously validated routing table. Existing
    /// synchronous calls and completion owners retain their prior Arc until
    /// they finish; no request is retargeted half-way through a topology
    /// change.
    pub fn publish_member_map(&self, staged: Arc<MemberMap>) {
        *self.inner.members.lock() = staged;
    }

    /// Describes a staged routing table without publishing it.  Filesystems
    /// use this to validate their own stripe metadata before the table can
    /// become reachable by new I/O.
    pub fn staged_devices(staged: &Arc<MemberMap>) -> Vec<BlockVolumeDevice> {
        staged
            .members
            .iter()
            .map(|member| member.descriptor)
            .collect()
    }

    fn member_map(&self) -> Arc<MemberMap> {
        self.inner.members.lock().clone()
    }

    /// Stable volume geometry.
    pub fn geometry(&self) -> BlockGeometry {
        self.member_map().geometry
    }

    /// Fixed physical routing table.
    pub fn devices(&self) -> impl ExactSizeIterator<Item = BlockVolumeDevice> {
        self.member_map()
            .members
            .iter()
            .map(|member| member.descriptor)
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// Snapshots the queues which define the currently published member map.
    /// The queues are reference-counted, so a filesystem can stage a complete
    /// replacement map without borrowing this volume or retargeting in-flight
    /// I/O.  It must still validate and publish the staged map atomically.
    pub fn member_queues(&self) -> Vec<SharedBlockDevice> {
        self.member_map()
            .members
            .iter()
            .map(|member| member.device.clone())
            .collect()
    }

    /// Returns the sole async completion owner for one physical member.
    pub fn completion_owner(&self, index: usize) -> Option<BlockVolumeCompletionOwner> {
        self.member_map()
            .members
            .get(index)
            .map(|member| BlockVolumeCompletionOwner {
                device: member.descriptor,
                queue: member.device.clone(),
            })
    }

    /// Replaces the deterministic fault schedule. Rules are evaluated in
    /// order, giving recovery tests a precise, reproducible failure point.
    pub fn set_fault_rules(&self, rules: &[BlockFaultRule]) {
        let mut active = self.inner.faults.lock();
        active.clear();
        active.extend(rules.iter().copied().map(ActiveFaultRule));
    }

    /// Removes all deterministic faults.
    pub fn clear_fault_rules(&self) {
        self.inner.faults.lock().clear();
    }

    /// Reads complete volume blocks, routing a request over member boundaries.
    pub fn read_blocks(&self, start: u64, buf: &mut [u8]) -> DevResult {
        let map = self.member_map();
        let range = self.buffer_range_in(&map, start, buf.len())?;
        let mut done = 0usize;
        self.for_each_member_in(
            &map,
            range,
            BlockFaultOperation::Read,
            |member, local, blocks| {
                self.maybe_fail(BlockFaultOperation::Read, member.descriptor.index)?;
                let bytes = blocks as usize * self.geometry().block_size;
                let mut device = member.device.clone();
                device.read_block(local, &mut buf[done..done + bytes])?;
                done += bytes;
                Ok(())
            },
        )
    }

    /// Writes complete volume blocks, routing a request over member boundaries.
    pub fn write_blocks(&self, start: u64, buf: &[u8]) -> DevResult {
        let map = self.member_map();
        let range = self.buffer_range_in(&map, start, buf.len())?;
        let mut done = 0usize;
        self.for_each_member_in(
            &map,
            range,
            BlockFaultOperation::Write,
            |member, local, blocks| {
                self.maybe_fail(BlockFaultOperation::Write, member.descriptor.index)?;
                let bytes = blocks as usize * self.geometry().block_size;
                let mut device = member.device.clone();
                device.write_block(local, &buf[done..done + bytes])?;
                done += bytes;
                Ok(())
            },
        )
    }

    /// Writes blocks with FUA semantics. Every participating member must
    /// advertise the capability; no flush-based emulation is substituted.
    pub fn write_blocks_fua(&self, start: u64, buf: &[u8]) -> DevResult {
        let map = self.member_map();
        let range = self.buffer_range_in(&map, start, buf.len())?;
        self.require_capability_in(&map, range, |caps| caps.fua)?;
        let mut done = 0usize;
        self.for_each_member_in(
            &map,
            range,
            BlockFaultOperation::WriteFua,
            |member, local, blocks| {
                self.maybe_fail(BlockFaultOperation::WriteFua, member.descriptor.index)?;
                let bytes = blocks as usize * self.geometry().block_size;
                let mut device = member.device.clone();
                device.write_block_fua(local, &buf[done..done + bytes])?;
                done += bytes;
                Ok(())
            },
        )
    }

    /// Flushes every member in stable routing order.
    pub fn flush(&self) -> DevResult {
        let map = self.member_map();
        for member in &map.members {
            self.maybe_fail(BlockFaultOperation::Flush, member.descriptor.index)?;
            let mut device = member.device.clone();
            device.flush()?;
        }
        Ok(())
    }

    /// Establishes a non-persistent ordering fence on every member.
    pub fn fence(&self) -> DevResult {
        let map = self.member_map();
        for member in &map.members {
            self.maybe_fail(BlockFaultOperation::Fence, member.descriptor.index)?;
            let mut device = member.device.clone();
            device.fence()?;
        }
        Ok(())
    }

    /// Discards an exact logical range.
    pub fn discard(&self, range: BlockRange) -> DevResult {
        let map = self.member_map();
        self.require_range_in(&map, range)?;
        self.require_capability_in(&map, range, |caps| caps.discard)?;
        self.for_each_member_in(
            &map,
            range,
            BlockFaultOperation::Discard,
            |member, local, blocks| {
                self.maybe_fail(BlockFaultOperation::Discard, member.descriptor.index)?;
                let mut device = member.device.clone();
                device.discard_blocks(BlockRange {
                    start: local,
                    blocks,
                })
            },
        )
    }

    /// Writes logical zeroes to an exact volume range.
    pub fn write_zeroes(&self, range: BlockRange) -> DevResult {
        let map = self.member_map();
        self.require_range_in(&map, range)?;
        self.require_capability_in(&map, range, |caps| caps.write_zeroes)?;
        self.for_each_member_in(
            &map,
            range,
            BlockFaultOperation::WriteZeroes,
            |member, local, blocks| {
                self.maybe_fail(BlockFaultOperation::WriteZeroes, member.descriptor.index)?;
                let mut device = member.device.clone();
                device.write_zeroes(BlockRange {
                    start: local,
                    blocks,
                })
            },
        )
    }

    fn buffer_range_in(&self, map: &MemberMap, start: u64, len: usize) -> DevResult<BlockRange> {
        if len % map.geometry.block_size != 0 {
            return Err(DevError::InvalidParam);
        }
        let blocks = (len / map.geometry.block_size) as u64;
        let range = BlockRange { start, blocks };
        self.require_range_in(map, range)?;
        Ok(range)
    }

    fn require_range_in(&self, map: &MemberMap, range: BlockRange) -> DevResult {
        if map.geometry.contains(range) {
            Ok(())
        } else {
            Err(DevError::InvalidParam)
        }
    }

    fn require_capability_in(
        &self,
        map: &MemberMap,
        range: BlockRange,
        supported: impl Fn(BlockCapabilities) -> bool,
    ) -> DevResult {
        self.for_each_member_in(map, range, BlockFaultOperation::Read, |member, _, _| {
            if supported(member.device.block_capabilities()) {
                Ok(())
            } else {
                Err(DevError::Unsupported)
            }
        })
    }

    fn for_each_member_in(
        &self,
        map: &MemberMap,
        range: BlockRange,
        _operation: BlockFaultOperation,
        mut visit: impl FnMut(&VolumeMember, u64, u64) -> DevResult,
    ) -> DevResult {
        if range.blocks == 0 {
            return Ok(());
        }
        let mut cursor = range.start;
        let end = cursor
            .checked_add(range.blocks)
            .ok_or(DevError::InvalidParam)?;
        for member in &map.members {
            let member_start = member.descriptor.volume_start;
            let member_end = member_start + member.descriptor.geometry.blocks;
            if cursor >= member_end || end <= member_start {
                continue;
            }
            let start = cursor.max(member_start);
            let finish = end.min(member_end);
            visit(member, start - member_start, finish - start)?;
            cursor = finish;
            if cursor == end {
                break;
            }
        }
        if cursor == end {
            Ok(())
        } else {
            Err(DevError::BadState)
        }
    }

    fn maybe_fail(&self, operation: BlockFaultOperation, device: usize) -> DevResult {
        let mut rules = self.inner.faults.lock();
        let Some(index) = rules.iter().position(|active| {
            active.0.operation == operation
                && match active.0.device {
                    Some(expected) => expected == device,
                    None => true,
                }
        }) else {
            return Ok(());
        };
        let rule = &mut rules[index].0;
        if rule.successful_matches != 0 {
            rule.successful_matches -= 1;
            return Ok(());
        }
        if rule.lifetime == BlockFaultLifetime::Once {
            rules.remove(index);
        }
        Err(DevError::Io)
    }
}
