extern crate std;

use alloc::sync::{Arc, Weak};
use core::{
    any::Any,
    cell::Cell,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Context,
    time::Duration,
};
use std::{
    sync::{Mutex as StdMutex, mpsc},
    thread,
};

use axfs_ng_vfs::{
    DirEntry, FileNode, FileNodeOps, Filesystem, FilesystemOps, Metadata, MetadataUpdate,
    Mountpoint, NodeOps, NodePermission, NodeType, Reference, StatFs, VfsError, VfsResult,
    XattrProvider, XattrSetMode,
};
use axio::{IoBuf, Read};
use tk_linux_packet::{
    PacketSendAddress, PacketSocketType, ProtocolSelector, ReceiveFlags, SetPacketOption,
};

use super::*;
use crate::{
    file::{PacketSocket, packet_socket::packet_test_context},
    task::{NetworkNamespace, UserNamespace},
};

struct CountingPacketSource<'a> {
    bytes: &'a [u8],
    offset: usize,
    reads: &'a Cell<usize>,
}

impl Read for CountingPacketSource<'_> {
    fn read(&mut self, output: &mut [u8]) -> AxResult<usize> {
        self.reads.set(self.reads.get() + 1);
        let source = &self.bytes[self.offset..];
        let copied = source.len().min(output.len());
        output[..copied].copy_from_slice(&source[..copied]);
        self.offset += copied;
        Ok(copied)
    }
}

impl IoBuf for CountingPacketSource<'_> {
    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }
}

fn packet_test_namespace() -> Arc<NetworkNamespace> {
    NetworkNamespace::try_new_loopback_only(UserNamespace::try_new_root().unwrap()).unwrap()
}

fn raw_ipv4_packet() -> [u8; 34] {
    let mut frame = [0_u8; 34];
    frame[12..14].copy_from_slice(&0x0800_u16.to_be_bytes());
    frame[14] = 0x45;
    frame[16..18].copy_from_slice(&20_u16.to_be_bytes());
    frame[22] = 64;
    frame
}

fn loopback_packet_destination() -> PacketSendAddress {
    PacketSendAddress::try_from_network_order_fields(0x0800_u16.to_be(), 1, 6, [0; 8]).unwrap()
}

fn pinned_packet(socket: Arc<PacketSocket>) -> PinnedSocketDescription {
    let file: Arc<dyn FileLike> = socket;
    let description = FileDescription::new(file).unwrap();
    let handle = FileHandle::<dyn FileLike>::from_description_for_test(description);
    let status = handle.io_status_snapshot();
    let pinned = PinnedSocketDescription::from_file_handle(&handle, status)
        .unwrap()
        .unwrap();
    assert_eq!(
        pinned.security_ref().unwrap().ofd_identity(),
        handle.open_file_description_key()
    );
    pinned
}

fn enqueue_outgoing_packet(sender: &PacketSocket, frame: &[u8]) {
    let plan = sender
        .prepare_send(frame.len(), Some(loopback_packet_destination()))
        .unwrap();
    assert_eq!(sender.send_prepared(plan, frame), Ok(frame.len()));
}

#[test]
fn denied_generic_packet_read_preserves_payload_and_queue_ownership() {
    let _context = packet_test_context();
    let namespace = packet_test_namespace();
    let receiver = PacketSocket::try_new(
        PacketSocketType::Raw,
        ProtocolSelector::All,
        namespace.clone(),
    )
    .unwrap();
    receiver
        .set_packet_option(SetPacketOption::IgnoreOutgoing(true))
        .unwrap();
    let sender =
        PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, namespace).unwrap();
    let pinned = pinned_packet(receiver.clone());
    let frame = raw_ipv4_packet();
    enqueue_outgoing_packet(&sender, &frame);
    assert!(receiver.poll().contains(IoEvents::READABLE));

    let authorize_calls = Cell::new(0);
    let backend_calls = Cell::new(0);
    let mut output = [0xa5_u8; 34];
    let output_len = output.len();
    let result = generic_read_after_socket_policy(
        Some(&pinned),
        output_len,
        |_| {
            authorize_calls.set(authorize_calls.get() + 1);
            Err(AxError::PermissionDenied)
        },
        || {
            backend_calls.set(backend_calls.get() + 1);
            let mut destination = &mut output[..];
            receiver
                .recv_with_nonblocking(&mut destination, ReceiveFlags::EMPTY, true)
                .map(|outcome| outcome.returned_len())
        },
    );

    assert_eq!(result, Err(AxError::PermissionDenied));
    assert_eq!(authorize_calls.get(), 1);
    assert_eq!(backend_calls.get(), 0);
    assert_eq!(output, [0xa5; 34]);
    assert!(receiver.poll().contains(IoEvents::READABLE));

    let mut drained = [0_u8; 34];
    let mut destination = &mut drained[..];
    let outcome = receiver
        .recv_with_nonblocking(&mut destination, ReceiveFlags::EMPTY, true)
        .unwrap();
    assert_eq!(outcome.returned_len(), frame.len());
    assert_eq!(drained, frame);
    assert!(!receiver.poll().contains(IoEvents::READABLE));
}

#[test]
fn generic_packet_read_uses_the_frozen_ofd_nonblocking_state() {
    let _context = packet_test_context();
    let namespace = packet_test_namespace();
    let receiver = PacketSocket::try_new(
        PacketSocketType::Raw,
        ProtocolSelector::All,
        namespace.clone(),
    )
    .unwrap();
    receiver
        .set_packet_option(SetPacketOption::IgnoreOutgoing(true))
        .unwrap();
    let sender =
        PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, namespace).unwrap();
    let file: Arc<dyn FileLike> = receiver.clone();
    let description = FileDescription::new(file).unwrap();
    let handle = FileHandle::<dyn FileLike>::from_description_for_test(description);
    let frozen_nonblocking = handle.set_nonblocking_status(true).unwrap();
    assert!(frozen_nonblocking.nonblocking());
    assert!(!handle.set_nonblocking_status(false).unwrap().nonblocking());

    let worker_handle = handle.clone();
    let (result_tx, result_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut output = [0_u8; 34];
        let result =
            read_file_like_with_status(&worker_handle, frozen_nonblocking, &mut &mut output[..]);
        result_tx.send(result).unwrap();
    });

    match result_rx.recv_timeout(Duration::from_millis(200)) {
        Ok(result) => assert_eq!(result, Err(AxError::WouldBlock)),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            enqueue_outgoing_packet(&sender, &raw_ipv4_packet());
            let _ = result_rx.recv_timeout(Duration::from_secs(1));
            worker.join().unwrap();
            panic!("packet read resampled the live blocking flag");
        }
        Err(error) => panic!("packet read worker failed: {error:?}"),
    }
    worker.join().unwrap();
}

#[test]
fn denied_generic_packet_write_reads_no_payload_and_submits_no_frame() {
    let _context = packet_test_context();
    let namespace = packet_test_namespace();
    let observer = PacketSocket::try_new(
        PacketSocketType::Raw,
        ProtocolSelector::All,
        namespace.clone(),
    )
    .unwrap();
    let sender =
        PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, namespace).unwrap();
    let pinned = pinned_packet(sender.clone());
    let frame = raw_ipv4_packet();
    let reads = Cell::new(0);
    let backend_calls = Cell::new(0);
    let authorize_calls = Cell::new(0);
    let mut source = CountingPacketSource {
        bytes: &frame,
        offset: 0,
        reads: &reads,
    };

    let result = generic_write_after_socket_policy(
        Some(&pinned),
        |_| {
            authorize_calls.set(authorize_calls.get() + 1);
            Err(AxError::PermissionDenied)
        },
        || {
            backend_calls.set(backend_calls.get() + 1);
            sender.write(&mut source)
        },
    );

    assert_eq!(result, Err(AxError::PermissionDenied));
    assert_eq!(authorize_calls.get(), 1);
    assert_eq!(backend_calls.get(), 0);
    assert_eq!(reads.get(), 0);
    assert!(!observer.poll().contains(IoEvents::READABLE));
}

#[test]
fn zero_length_packet_read_skips_hook_but_write_still_dispatches() {
    let _context = packet_test_context();
    let namespace = packet_test_namespace();
    let receiver = PacketSocket::try_new(
        PacketSocketType::Raw,
        ProtocolSelector::All,
        namespace.clone(),
    )
    .unwrap();
    let sender =
        PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, namespace).unwrap();
    let pinned = pinned_packet(receiver.clone());
    enqueue_outgoing_packet(&sender, &raw_ipv4_packet());

    let receive_hooks = Cell::new(0);
    let receive_calls = Cell::new(0);
    let result = generic_read_after_socket_policy(
        Some(&pinned),
        0,
        |_| {
            receive_hooks.set(receive_hooks.get() + 1);
            Ok(())
        },
        || {
            receive_calls.set(receive_calls.get() + 1);
            Ok(0usize)
        },
    );
    assert_eq!(result, Ok(None));
    assert_eq!(receive_hooks.get(), 0);
    assert_eq!(receive_calls.get(), 0);
    assert!(receiver.poll().contains(IoEvents::READABLE));

    let send_hooks = Cell::new(0);
    let send_calls = Cell::new(0);
    let empty_reads = Cell::new(0);
    let mut empty = CountingPacketSource {
        bytes: &[],
        offset: 0,
        reads: &empty_reads,
    };
    let result = generic_write_after_socket_policy(
        Some(&pinned),
        |_| {
            send_hooks.set(send_hooks.get() + 1);
            Err(AxError::PermissionDenied)
        },
        || {
            send_calls.set(send_calls.get() + 1);
            receiver.write(&mut empty)
        },
    );
    assert_eq!(result, Err(AxError::PermissionDenied));
    assert_eq!(send_hooks.get(), 1);
    assert_eq!(send_calls.get(), 0);
    assert_eq!(empty_reads.get(), 0);
    assert!(receiver.poll().contains(IoEvents::READABLE));
}

struct IoContractFs {
    this: Weak<Self>,
    flags: NodeFlags,
    node_type: NodeType,
    size: u64,
    fail_open: AtomicBool,
    fail_remove_xattr: AtomicBool,
    open_calls: AtomicUsize,
    remove_xattr_calls: AtomicUsize,
    set_len_calls: AtomicUsize,
    write_offsets: StdMutex<Vec<u64>>,
}

impl IoContractFs {
    fn new(flags: NodeFlags, size: u64) -> Arc<Self> {
        Self::new_with_type(flags, size, NodeType::RegularFile)
    }

    fn new_with_type(flags: NodeFlags, size: u64, node_type: NodeType) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            this: this.clone(),
            flags,
            node_type,
            size,
            fail_open: AtomicBool::new(false),
            fail_remove_xattr: AtomicBool::new(false),
            open_calls: AtomicUsize::new(0),
            remove_xattr_calls: AtomicUsize::new(0),
            set_len_calls: AtomicUsize::new(0),
            write_offsets: StdMutex::new(Vec::new()),
        })
    }

    fn location(self: &Arc<Self>) -> Location {
        let filesystem = Filesystem::new(self.clone());
        Mountpoint::new_root(&filesystem).root_location()
    }
}

impl FilesystemOps for IoContractFs {
    fn name(&self) -> &str {
        "io-contract-test"
    }

    fn root_dir(&self) -> DirEntry {
        let fs = self.this.upgrade().expect("test filesystem is live");
        DirEntry::new_file(
            FileNode::new(Arc::new(IoContractNode {
                fs,
                user_data: axfs_ng_vfs::NodeUserData::new(),
            })),
            self.node_type,
            Reference::root(),
        )
    }

    fn stat(&self) -> VfsResult<StatFs> {
        Ok(StatFs {
            fs_type: 0,
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            file_count: 1,
            free_file_count: 0,
            name_length: 255,
            fragment_size: 4096,
            mount_flags: 0,
        })
    }
}

struct IoContractNode {
    fs: Arc<IoContractFs>,
    user_data: axfs_ng_vfs::NodeUserData,
}

impl NodeOps for IoContractNode {
    fn inode(&self) -> u64 {
        1
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        Ok(Metadata {
            device: 0,
            inode: 1,
            nlink: 1,
            mode: NodePermission::from_bits_truncate(0o600),
            node_type: self.fs.node_type,
            uid: 0,
            gid: 0,
            project_id: 0,
            size: self.fs.size,
            block_size: 4096,
            blocks: 0,
            rdev: Default::default(),
            atime: axfs_ng_vfs::Timestamp::ZERO,
            btime: axfs_ng_vfs::Timestamp::ZERO,
            mtime: axfs_ng_vfs::Timestamp::ZERO,
            ctime: axfs_ng_vfs::Timestamp::ZERO,
        })
    }

    fn update_metadata(&self, _update: MetadataUpdate) -> VfsResult<()> {
        Ok(())
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        &*self.fs
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn flags(&self) -> NodeFlags {
        self.fs.flags
    }

    fn open(&self, _read: bool, _write: bool) -> VfsResult<()> {
        self.fs.open_calls.fetch_add(1, Ordering::AcqRel);
        if self.fs.fail_open.load(Ordering::Acquire) {
            Err(VfsError::PermissionDenied)
        } else {
            Ok(())
        }
    }

    fn persistent_user_data(&self) -> Option<&axfs_ng_vfs::NodeUserData> {
        Some(&self.user_data)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn xattr_provider(&self) -> Option<&dyn XattrProvider> {
        Some(self)
    }
}

impl XattrProvider for IoContractNode {
    fn get_xattr(&self, _name: &[u8]) -> VfsResult<Vec<u8>> {
        Err(LinuxError::ENODATA.into())
    }

    fn list_xattrs(&self) -> VfsResult<Vec<u8>> {
        Ok(Vec::new())
    }

    fn set_xattr(&self, _name: &[u8], _value: &[u8], _mode: XattrSetMode) -> VfsResult<()> {
        Ok(())
    }

    fn remove_xattr(&self, _name: &[u8]) -> VfsResult<()> {
        self.fs.remove_xattr_calls.fetch_add(1, Ordering::AcqRel);
        if self.fs.fail_remove_xattr.load(Ordering::Acquire) {
            Err(VfsError::Io)
        } else {
            Err(LinuxError::ENODATA.into())
        }
    }
}

impl Pollable for IoContractNode {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

impl FileNodeOps for IoContractNode {
    fn supports_nowait_read(&self) -> bool {
        true
    }

    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Ok(0)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.fs.write_offsets.lock().unwrap().push(offset);
        Ok(buf.len())
    }

    fn write_at_vectored(&self, bufs: &[&[u8]], offset: u64) -> VfsResult<usize> {
        let _ = offset;
        bufs.iter().try_fold(0usize, |total, buf| {
            total.checked_add(buf.len()).ok_or(VfsError::InvalidInput)
        })
    }

    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        Ok((buf.len(), self.fs.size + buf.len() as u64))
    }

    fn set_len(&self, _len: u64) -> VfsResult<()> {
        self.fs.set_len_calls.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn set_symlink(&self, _target: &axfs_ng_vfs::FsPath) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }
}

#[test]
fn axfs_pinned_segment_adapter_is_fixed_and_bounded() {
    let segments: [UserIoPinSegment; USER_IOV_FAST_MAX_SEGMENTS] =
        core::array::from_fn(|index| UserIoPinSegment {
            paddr: 0x1000 + index * 0x1000,
            len: 0x1000,
        });
    let physical = axfs_pinned_segments(&segments).unwrap();
    assert_eq!(physical.as_slice().len(), USER_IOV_FAST_MAX_SEGMENTS);
    assert_eq!(physical.as_slice()[3].paddr(), 0x4000);

    let overflow: [UserIoPinSegment; USER_IOV_FAST_MAX_SEGMENTS + 1] =
        core::array::from_fn(|index| UserIoPinSegment {
            paddr: 0x1000 + index * 0x1000,
            len: 0x1000,
        });
    assert_eq!(
        axfs_pinned_segments(&overflow).err(),
        Some(AxError::InvalidInput)
    );
}

#[test]
fn io_uring_dma_clip_keeps_exact_subrange_without_allocating() {
    let segments = [
        UserIoPinSegment {
            paddr: 0x10_000,
            len: 0x800,
        },
        UserIoPinSegment {
            paddr: 0x20_000,
            len: 0x800,
        },
        UserIoPinSegment {
            paddr: 0x30_000,
            len: 0x800,
        },
    ];
    let mut physical = [PhysicalIoSegment::new(0, 0); IO_URING_DMA_MAX_SEGMENTS];
    let count = clip_io_uring_dma_segments(&segments, 0x200, 0x1_000, &mut physical);
    assert_eq!(count, Some(3));
    assert_eq!(
        &physical[..3],
        &[
            PhysicalIoSegment::new(0x10_200, 0x600),
            PhysicalIoSegment::new(0x20_000, 0x800),
            PhysicalIoSegment::new(0x30_000, 0x200),
        ]
    );
}

#[test]
fn io_uring_dma_clip_rejects_more_than_four_physical_ranges() {
    let segments: [UserIoPinSegment; IO_URING_DMA_MAX_SEGMENTS + 1] =
        core::array::from_fn(|index| UserIoPinSegment {
            paddr: 0x10_000 + index * 0x2_000,
            len: 0x1_000,
        });
    let mut physical = [PhysicalIoSegment::new(0, 0); IO_URING_DMA_MAX_SEGMENTS];
    assert_eq!(
        clip_io_uring_dma_segments(&segments, 0, segments.len() * 0x1_000, &mut physical),
        None
    );
}

#[test]
fn io_uring_dma_clip_accepts_one_through_four_sg_ranges() {
    let segments: [UserIoPinSegment; IO_URING_DMA_MAX_SEGMENTS] =
        core::array::from_fn(|index| UserIoPinSegment {
            paddr: 0x60_000 + index * 0x2_000,
            len: 0x1_000,
        });
    for count in 1..=IO_URING_DMA_MAX_SEGMENTS {
        let mut physical = [PhysicalIoSegment::new(0, 0); IO_URING_DMA_MAX_SEGMENTS];
        assert_eq!(
            clip_io_uring_dma_segments(&segments[..count], 0, count * 0x1_000, &mut physical),
            Some(count)
        );
    }
}

#[test]
fn io_uring_dma_clip_merges_adjacent_physical_pages_for_256k_request() {
    let segments: [UserIoPinSegment; 64] = core::array::from_fn(|index| UserIoPinSegment {
        paddr: 0x40_000 + index * 0x1_000,
        len: 0x1_000,
    });
    let mut physical = [PhysicalIoSegment::new(0, 0); IO_URING_DMA_MAX_SEGMENTS];
    let count = clip_io_uring_dma_segments(&segments, 0, IO_URING_DMA_MAX_BYTES, &mut physical);
    assert_eq!(count, Some(1));
    assert_eq!(
        physical[0],
        PhysicalIoSegment::new(0x40_000, IO_URING_DMA_MAX_BYTES)
    );
}

#[test]
fn io_uring_dma_clip_reports_sg_cap_only_for_nonadjacent_ranges() {
    let segments: [UserIoPinSegment; IO_URING_DMA_MAX_SEGMENTS + 1] =
        core::array::from_fn(|index| UserIoPinSegment {
            paddr: 0x80_000 + index * 0x2_000,
            len: 0x1_000,
        });
    let mut physical = [PhysicalIoSegment::new(0, 0); IO_URING_DMA_MAX_SEGMENTS];
    assert_eq!(
        clip_io_uring_dma_segments_with_reason(
            &segments,
            0,
            segments.len() * 0x1_000,
            &mut physical,
        ),
        Err(crate::file::io_uring::IoUringDmaFallbackReason::SgCap)
    );
}

#[test]
fn io_uring_dma_geometry_requires_private_aligned_nonzero_range() {
    assert!(fixed_dma_geometry_eligible(
        0x2000,
        0x1000,
        0x4000,
        true,
        UserIoPinProvenance::PrivateAnonymous,
    ));
    assert!(!fixed_dma_geometry_eligible(
        0x2000,
        0x1000,
        0x4000,
        true,
        UserIoPinProvenance::Ineligible,
    ));
    assert!(!fixed_dma_geometry_eligible(
        0x2201,
        0x1000,
        0x4000,
        true,
        UserIoPinProvenance::PrivateAnonymous,
    ));
    assert!(!fixed_dma_geometry_eligible(
        0x2000,
        0,
        0x4000,
        true,
        UserIoPinProvenance::PrivateAnonymous,
    ));
    assert!(!fixed_dma_geometry_eligible(
        0x2000,
        0x1000,
        0x4000,
        false,
        UserIoPinProvenance::PrivateAnonymous,
    ));
}

#[test]
fn io_uring_dma_result_requires_full_completion_and_never_bounces_errors() {
    assert_eq!(
        classify_fixed_dma_result(Some(0x1000), 0x1000),
        Ok(FixedDmaOutcome::Completed(0x1000))
    );
    assert_eq!(
        classify_fixed_dma_result(None, 0x1000),
        Ok(FixedDmaOutcome::Fallback)
    );
    assert_eq!(
        classify_fixed_dma_result(Some(0x200), 0x1000),
        Err(AxError::Io)
    );
}

#[test]
fn explicit_write_marker_maps_to_espipe_only() {
    assert!(check_positioned_write_flags(NodeFlags::empty()).is_ok());
    assert_eq!(
        check_positioned_write_flags(NodeFlags::NO_POSITIONED_WRITE),
        Err(AxError::from(LinuxError::ESPIPE))
    );
}

#[test]
fn stream_cursor_and_explicit_io_capabilities_are_independent() {
    let stream = NodeFlags::STREAM;
    assert!(check_positioned_read_flags(stream).is_ok());
    assert!(check_positioned_write_flags(stream).is_ok());

    assert_eq!(
        check_positioned_read_flags(stream | NodeFlags::NO_POSITIONED_READ),
        Err(AxError::from(LinuxError::ESPIPE))
    );
    assert_eq!(
        check_positioned_write_flags(stream | NodeFlags::NO_POSITIONED_WRITE),
        Err(AxError::from(LinuxError::ESPIPE))
    );
}

#[test]
fn inode_append_classification_excludes_stream_and_positioned_nodes() {
    let open = |flags| {
        let fs = IoContractFs::new(flags, 4096);
        let mut options = OpenOptions::new();
        options.write(true);
        File::new(
            options
                .open_loc(fs.location())
                .unwrap()
                .into_file()
                .unwrap(),
        )
    };
    let append = OfdIoStatus::new(O_APPEND);

    let regular = open(NodeFlags::NON_CACHEABLE);
    assert!(write_uses_inode_append(regular.inner(), append));
    assert!(!write_uses_current_position(regular.inner(), append));

    let stream = open(NodeFlags::NON_CACHEABLE | NodeFlags::STREAM);
    assert!(!write_uses_inode_append(stream.inner(), append));
    assert!(!write_uses_current_position(stream.inner(), append));

    let positioned = open(NodeFlags::NON_CACHEABLE | NodeFlags::POSITIONED_APPEND);
    assert!(!write_uses_inode_append(positioned.inner(), append));
    assert!(write_uses_current_position(positioned.inner(), append));
}

#[test]
fn io_uring_nowait_direct_read_requires_residency_before_copy() {
    let fs = IoContractFs::new(NodeFlags::NON_CACHEABLE, 4096);
    let mut options = OpenOptions::new();
    options.read(true).direct(true);
    let file = File::new(
        options
            .open_loc(fs.location())
            .unwrap()
            .into_file()
            .unwrap(),
    );
    let status = OfdIoStatus::new(0).with_rwf_nowait(true);
    let mut bytes = [0xa5; 512];
    let mut destination = bytes.as_mut_slice();
    // The provider advertises NOWAIT, but has no resident direct range.
    // This is EAGAIN before its read_at (which would report EOF), not a
    // queued effect or a successful zero-length read.
    assert_eq!(
        file.read_at_with_status(status, &mut destination, 0),
        Err(AxError::WouldBlock)
    );
    assert_eq!(bytes, [0xa5; 512]);
}

#[test]
fn generic_regular_file_is_not_worker_safe_even_with_fixed_direct_plan() {
    let fs = IoContractFs::new(NodeFlags::NON_CACHEABLE, 4096);
    let mut options = OpenOptions::new();
    options.read(true).direct(true);
    let file = Arc::new(File::new(
        options
            .open_loc(fs.location())
            .unwrap()
            .into_file()
            .unwrap(),
    ));
    let segments = [UserIoPinSegment {
        paddr: 0x2000,
        len: 0x1000,
    }];
    let fixed = Some((
        segments.as_slice(),
        0,
        0x1000,
        true,
        UserIoPinProvenance::PrivateAnonymous,
    ));

    assert!(file_uses_direct_io(file.as_ref()));
    assert!(!regular_ext4_physical_worker_plan(
        file.as_ref(),
        OfdIoStatus::new(0),
        PreparedPhysicalIoOperation::Read,
        0x2000,
        0x1000,
        0,
        fixed,
    ));
}

#[test]
fn zero_offset_io_uses_stream_dispatch_for_tty_like_files_only() {
    let _context = crate::test_support::scheduler_test_context();
    let stream_fs = IoContractFs::new_with_type(
        NodeFlags::NON_CACHEABLE
            | NodeFlags::STREAM
            | NodeFlags::NO_POSITIONED_READ
            | NodeFlags::NO_POSITIONED_WRITE,
        0,
        NodeType::CharacterDevice,
    );
    let mut stream_options = OpenOptions::new();
    stream_options.read(true).write(true);
    let stream_file = File::new(
        stream_options
            .open_loc(stream_fs.location())
            .unwrap()
            .into_file()
            .unwrap(),
    );
    let stream: Arc<dyn FileLike> = Arc::new(stream_file);
    let stream_description = FileDescription::new(stream).unwrap();
    let stream_handle = FileHandle::<dyn FileLike>::from_description_for_test(stream_description);
    assert!(zero_offset_stream_file_like(
        &stream_handle,
        NodeFlags::NO_POSITIONED_READ
    ));
    assert!(zero_offset_stream_file_like(
        &stream_handle,
        NodeFlags::NO_POSITIONED_WRITE
    ));

    // An anon-inode FileLike has no positioned operation at all. It must
    // still use its direct read/write methods at offset zero.
    let event: Arc<dyn FileLike> = crate::file::event::EventFd::new(0, false);
    let event_description = FileDescription::new(event).unwrap();
    let event_handle = FileHandle::<dyn FileLike>::from_description_for_test(event_description);
    assert!(zero_offset_stream_file_like(
        &event_handle,
        NodeFlags::NO_POSITIONED_READ
    ));
    assert!(zero_offset_stream_file_like(
        &event_handle,
        NodeFlags::NO_POSITIONED_WRITE
    ));

    // A regular file remains on the positioned path even if a malformed
    // test backend happens to advertise the stream prohibition flags.
    let regular_fs = IoContractFs::new(
        NodeFlags::NON_CACHEABLE
            | NodeFlags::STREAM
            | NodeFlags::NO_POSITIONED_READ
            | NodeFlags::NO_POSITIONED_WRITE,
        0,
    );
    let mut regular_options = OpenOptions::new();
    regular_options.read(true).write(true);
    let regular_file = File::new(
        regular_options
            .open_loc(regular_fs.location())
            .unwrap()
            .into_file()
            .unwrap(),
    );
    let regular: Arc<dyn FileLike> = Arc::new(regular_file);
    let regular_description = FileDescription::new(regular).unwrap();
    let regular_handle = FileHandle::<dyn FileLike>::from_description_for_test(regular_description);
    assert!(!zero_offset_stream_file_like(
        &regular_handle,
        NodeFlags::NO_POSITIONED_READ
    ));
    assert!(!zero_offset_stream_file_like(
        &regular_handle,
        NodeFlags::NO_POSITIONED_WRITE
    ));
}

#[test]
fn stream_write_admission_rejects_read_only_mount_for_nonzero_io() {
    let fs = IoContractFs::new(NodeFlags::NON_CACHEABLE, 0);
    let filesystem = Filesystem::new(fs.clone());
    let mountpoint = Mountpoint::new_root(&filesystem);
    mounts::initialize_test_mount(&mountpoint, 1).unwrap();
    let mut options = OpenOptions::new();
    options.write(true);
    let file = File::new(
        options
            .open_loc(mountpoint.root_location())
            .unwrap()
            .into_file()
            .unwrap(),
    );

    assert_eq!(
        check_file_write_admission(&file, 1),
        Err(AxError::ReadOnlyFilesystem)
    );
    // Ordinary sys_write permits a zero-length request after access
    // admission, so the io_uring stream path must preserve that rule.
    assert!(check_file_write_admission(&file, 0).is_ok());
}

#[repr(align(512))]
struct AlignedDirectBuffer([u8; DIRECT_IO_ALIGNMENT]);

#[test]
fn current_position_admission_and_write_exclude_shared_ofd_seek() {
    let fs = IoContractFs::new(NodeFlags::NON_CACHEABLE, 4096);
    let mut options = OpenOptions::new();
    options.write(true).direct(true);
    let file = Arc::new(File::new(
        options
            .open_loc(fs.location())
            .unwrap()
            .into_file()
            .unwrap(),
    ));
    let buffer = Arc::new(AlignedDirectBuffer([0x5a; DIRECT_IO_ALIGNMENT]));
    let (admitted_tx, admitted_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();

    let writer_file = file.clone();
    let writer_buffer = buffer.clone();
    let writer_fs = fs.clone();
    let writer = thread::spawn(move || {
        with_current_position_io(writer_file.as_ref(), DIRECT_IO_ALIGNMENT, |offset| {
            validate_direct_io(
                writer_file.as_ref(),
                writer_buffer.0.as_ptr() as usize,
                writer_buffer.0.len(),
                offset,
            )?;
            admitted_tx.send(offset).unwrap();
            release_rx.recv().unwrap();
            // Model the positioned backend callback used by both fast and
            // fallback paths without requiring a host kernel task.
            writer_fs.write_offsets.lock().unwrap().push(offset);
            let written = writer_buffer.0.len();
            Ok((written, written))
        })
    });
    assert_eq!(admitted_rx.recv().unwrap(), 0);

    let (seek_started_tx, seek_started_rx) = mpsc::channel();
    let (seek_done_tx, seek_done_rx) = mpsc::channel();
    let seeker_file = file.clone();
    let seeker = thread::spawn(move || {
        seek_started_tx.send(()).unwrap();
        let mut inner = seeker_file.inner();
        let position = inner
            .seek(SeekFrom::Current(DIRECT_IO_ALIGNMENT as i64))
            .unwrap();
        seek_done_tx.send(position).unwrap();
    });
    seek_started_rx.recv().unwrap();
    assert_eq!(
        seek_done_rx.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );

    release_tx.send(()).unwrap();
    assert_eq!(writer.join().unwrap(), Ok(DIRECT_IO_ALIGNMENT));
    assert_eq!(
        seek_done_rx.recv_timeout(Duration::from_secs(1)),
        Ok((DIRECT_IO_ALIGNMENT * 2) as u64)
    );
    seeker.join().unwrap();
    assert_eq!(&*fs.write_offsets.lock().unwrap(), &[0]);
}

#[test]
fn open_hook_rejects_before_truncate_side_effects() {
    let fs = IoContractFs::new(NodeFlags::NON_CACHEABLE, 17);
    fs.fail_open.store(true, Ordering::Release);
    let mut options = OpenOptions::new();
    options.write(true).truncate(true);
    assert!(matches!(
        options.open_loc(fs.location()),
        Err(VfsError::PermissionDenied)
    ));
    assert_eq!(fs.open_calls.load(Ordering::Acquire), 1);
    assert_eq!(fs.set_len_calls.load(Ordering::Acquire), 0);
}

#[test]
fn privilege_cleanup_failure_rejects_before_truncate_side_effects() {
    executable::init().unwrap();
    let fs = IoContractFs::new(NodeFlags::NON_CACHEABLE, 17);
    fs.fail_remove_xattr.store(true, Ordering::Release);
    let mut options = OpenOptions::new();
    options.write(true);
    let file = options
        .open_loc(fs.location())
        .unwrap()
        .into_file()
        .unwrap();
    let namespace = crate::task::UserNamespace::try_new_root().unwrap();
    let security = VfsSecurityContext::new(crate::task::Cred::try_root(namespace).unwrap());

    let result = (|| {
        let location = file.backend()?.location();
        let _privilege_guard = begin_inode_content_write(location, &security)?;
        file.set_len(0)
    })();
    assert_eq!(result, Err(AxError::Io));
    assert_eq!(fs.remove_xattr_calls.load(Ordering::Acquire), 1);
    assert_eq!(fs.set_len_calls.load(Ordering::Acquire), 0);
}

#[test]
fn sendfile_destination_rejects_authoritative_ofd_append_status() {
    assert!(check_sendfile_destination_status(0).is_ok());
    assert_eq!(
        check_sendfile_destination_status(O_APPEND),
        Err(AxError::InvalidInput)
    );
}

#[test]
fn copy_file_range_effective_count_uses_eof_short_source_and_write_limit() {
    let eof = copy_file_range_source_count(MAX_FILE_OFFSET - 1, 4, 0).unwrap();
    assert_eq!(eof, 0);
    assert_eq!(
        copy_file_range_effective_count(MAX_FILE_OFFSET - 1, MAX_FILE_OFFSET - 1, eof, eof, true,),
        Ok(0)
    );

    assert_eq!(
        copy_file_range_effective_count(0, MAX_FILE_OFFSET - 1, 4, 4, false),
        Ok(1)
    );
    assert_eq!(
        copy_file_range_effective_count(0, MAX_FILE_OFFSET, 1, 1, false),
        Err(AxError::from(LinuxError::EFBIG))
    );

    // The requested ranges overlap (0..8 and 5..13), but only four
    // source bytes exist, so the effective ranges do not.
    let short = copy_file_range_source_count(0, 8, 4).unwrap();
    assert_eq!(short, 4);
    assert_eq!(
        copy_file_range_effective_count(0, 5, short, short, true),
        Ok(4)
    );

    // A destination limit can make otherwise overlapping requested
    // ranges disjoint before the overlap and destination-end checks.
    assert_eq!(copy_file_range_effective_count(0, 8, 10, 3, true), Ok(3));
    assert_eq!(
        copy_file_range_effective_count(0, 2, 4, 4, true),
        Err(AxError::InvalidInput)
    );
}

#[test]
fn copy_file_range_source_wrap_precedes_eof_clamping() {
    let error = copy_file_range_source_count(u64::MAX - 1, 4, 0).unwrap_err();
    assert_eq!(LinuxError::from(error), LinuxError::EOVERFLOW);
}

#[test]
fn splice_nonblocking_is_derived_only_from_explicit_or_pipe_status() {
    assert!(splice_operation_nonblocking(
        SPLICE_F_NONBLOCK,
        false,
        false,
        true,
        false,
    ));
    assert!(splice_operation_nonblocking(0, true, true, false, true));
    assert!(splice_operation_nonblocking(0, true, false, true, true));
    assert!(!splice_operation_nonblocking(0, false, true, true, false,));
    assert!(!splice_operation_nonblocking(0, true, false, false, true,));

    // The buffered path freezes each endpoint separately. A source socket
    // keeps its own O_NONBLOCK, an output pipe can make source admission
    // nonblocking, and SPLICE_F_NONBLOCK does not override a socket
    // destination's own blocking mode.
    assert_eq!(
        splice_endpoint_nonblocking(0, true, true, false),
        (true, false)
    );
    assert_eq!(
        splice_endpoint_nonblocking(0, false, true, true),
        (true, true)
    );
    assert_eq!(
        splice_endpoint_nonblocking(SPLICE_F_NONBLOCK, false, false, false),
        (true, false)
    );
    assert_eq!(
        splice_endpoint_nonblocking(0, false, false, true),
        (false, true)
    );
}

#[test]
fn reciprocal_transfers_use_the_same_bounded_attempt_lock_order() {
    let forward = ordered_transfer_attempt_lock_indices(0x1234, 0xfeed_beef);
    let reverse = ordered_transfer_attempt_lock_indices(0xfeed_beef, 0x1234);
    assert_eq!(forward, reverse);
    assert!(forward.0 <= forward.1);
    assert!(forward.1 < TRANSFER_ATTEMPT_LOCK_COUNT);

    let same = ordered_transfer_attempt_lock_indices(0x1234, 0x1234);
    assert_eq!(same.0, same.1);
}

#[test]
fn transfer_eagain_waits_on_the_endpoint_that_was_attempted() {
    assert_eq!(transfer_wait_endpoint(false), TransferWaitEndpoint::Source);
    assert_eq!(
        transfer_wait_endpoint(true),
        TransferWaitEndpoint::Destination
    );
}

#[test]
fn transfer_driver_counts_short_destination_prefix_before_stopping() {
    let mut calls = 0;
    let transferred = drive_send_with(8, |_buf, _total| {
        calls += 1;
        Ok(Some(SendStep {
            written: 3,
            destination_short: true,
        }))
    })
    .unwrap();
    assert_eq!(transferred, 3);
    assert_eq!(calls, 1);
}

#[test]
fn transfer_driver_returns_progress_instead_of_later_error() {
    let mut calls = 0;
    let transferred = drive_send_with(8, |_buf, total| {
        calls += 1;
        if total == 0 {
            Ok(Some(SendStep {
                written: 4,
                destination_short: false,
            }))
        } else {
            Err(AxError::InvalidInput)
        }
    })
    .unwrap();
    assert_eq!(transferred, 4);
    assert_eq!(calls, 2);

    assert_eq!(
        drive_send_with(8, |_buf, _total| Err(AxError::InvalidInput)),
        Err(AxError::InvalidInput)
    );
}

#[test]
fn transfer_writer_commits_a_stream_prefix_before_a_later_error() {
    let mut calls = 0;
    let mut destination = |buf: &[u8]| {
        calls += 1;
        if calls == 1 {
            Ok(buf.len())
        } else {
            Err(AxError::WouldBlock)
        }
    };
    let mut writer = TransferWriter::new(4, &mut destination);

    assert_eq!(writer.write(b"ab"), Ok(2));
    assert_eq!(writer.write(b"cd"), Ok(0));
    assert_eq!(writer.written, 2);
    assert!(writer.destination_short);
}

#[test]
fn sync_file_range_checks_signed_loff_t_overflow_but_keeps_zero_to_eof() {
    assert_eq!(sync_file_range_end(i64::MAX, 1), Err(AxError::InvalidInput));
    assert_eq!(sync_file_range_end(i64::MAX, 0), Ok(0));
    assert_eq!(sync_file_range_end(i64::MAX - 1, 1), Ok(i64::MAX as u64));
}

#[test]
fn sync_file_range_validates_a_pipe_range_before_the_type_error() {
    // `sys_sync_file_range` calls this helper after successful fd lookup
    // and before its NodeType::Pipe ESPIPE branch.
    assert_eq!(
        validate_sync_file_range_args(i64::MAX, 1, 0),
        Err(AxError::InvalidInput)
    );
}

#[test]
fn ftruncate_admission_matches_linux_fdget_and_do_ftruncate() {
    let cases = [
        (
            false,
            FileLikeKind::Regular,
            false,
            true,
            AxError::BadFileDescriptor,
        ),
        (
            true,
            FileLikeKind::Regular,
            true,
            true,
            AxError::BadFileDescriptor,
        ),
        (
            true,
            FileLikeKind::Directory,
            false,
            true,
            AxError::InvalidInput,
        ),
        (true, FileLikeKind::Fifo, false, true, AxError::InvalidInput),
        (
            true,
            FileLikeKind::Socket,
            false,
            true,
            AxError::InvalidInput,
        ),
        (
            true,
            FileLikeKind::Other,
            false,
            true,
            AxError::InvalidInput,
        ),
        (
            true,
            FileLikeKind::Regular,
            false,
            false,
            AxError::InvalidInput,
        ),
    ];
    for (fd_found, kind, path_only, writable, expected) in cases {
        assert_eq!(
            ftruncate_admission_errno(fd_found, kind, path_only, writable),
            Err(expected)
        );
    }
}

#[test]
fn ftruncate_negative_length_precedes_invalid_and_opath_fd_admission() {
    for (fd_found, path_only) in [(false, false), (true, true)] {
        let result = ftruncate_length_errno(-1).and_then(|()| {
            ftruncate_admission_errno(fd_found, FileLikeKind::Regular, path_only, true)
        });
        assert_eq!(result, Err(AxError::InvalidInput));
    }
}
