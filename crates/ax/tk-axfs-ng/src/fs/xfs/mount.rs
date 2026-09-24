//! The live XFS mount: transaction commit, writeback and log state.

use super::*;

pub(super) struct XfsLiveLogState {
    pub(super) ring: XfsLogRing,
    pub(super) ail: XfsAil,
    pub(super) next_transaction: u32,
    pub(super) failed: bool,
}

/// Writable-mount coordinator. It is constructed only with an explicitly
/// recovered log ring; no VFS caller can guess a journal head/tail from a
/// pathname or silently start a second log stream.
pub struct XfsMount {
    pub(super) volume: Arc<XfsVolume>,
    pub(super) live: SpinMutex<XfsLiveLogState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RangeRewrite {
    Punch { offset: u64, length: u64 },
    Collapse { offset: u64, length: u64 },
    Insert { offset: u64, length: u64 },
}

impl XfsMount {
    pub(crate) fn new(volume: Arc<XfsVolume>, ring: XfsLogRing) -> XfsResult<Self> {
        if !volume.superblock.is_v5() || ring.blocks() != volume.log_region_blocks()? {
            return Err(XfsError::UnsupportedFeature);
        }
        Ok(Self {
            volume,
            live: SpinMutex::new(XfsLiveLogState {
                ring,
                ail: XfsAil::default(),
                next_transaction: 1,
                failed: false,
            }),
        })
    }

    pub(crate) fn volume(&self) -> &Arc<XfsVolume> {
        &self.volume
    }

    pub(super) fn stage_inode_quota_delta(
        &self,
        inode: &XfsInode,
        block_delta: i64,
        inode_delta: i64,
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        self.stage_quota_identity_delta(
            inode.number,
            inode.uid,
            inode.gid,
            inode.project_id,
            block_delta,
            inode_delta,
            metadata,
        )
    }

    pub(super) fn stage_quota_identity_delta(
        &self,
        inode_number: u64,
        uid: u32,
        gid: u32,
        project_id: u32,
        block_delta: i64,
        inode_delta: i64,
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if !self.volume.has_quota_accounting() {
            return Ok(());
        }
        let roots = self.volume.quota_state()?.roots;
        // Quota files and the root metadata objects are accounting
        // infrastructure, not quota-owned user objects.
        if [roots.user, roots.group, roots.project]
            .into_iter()
            .flatten()
            .any(|root| root == inode_number)
        {
            return Ok(());
        }
        if roots.user.is_some() && self.volume.quota_accounting_enabled(1) {
            self.volume
                .stage_dquot_delta(1, uid, block_delta, inode_delta, metadata)?;
        }
        if roots.group.is_some() && self.volume.quota_accounting_enabled(4) {
            self.volume
                .stage_dquot_delta(4, gid, block_delta, inode_delta, metadata)?;
        }
        if roots.project.is_some() && self.volume.quota_accounting_enabled(2) {
            self.volume
                .stage_dquot_delta(2, project_id, block_delta, inode_delta, metadata)?;
        }
        Ok(())
    }

    pub(super) fn stage_quota_owner_transfer(
        &self,
        inode: &XfsInode,
        uid: u32,
        gid: u32,
        project_id: u32,
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if !self.volume.has_quota_accounting() {
            return Ok(());
        }
        let roots = self.volume.quota_state()?.roots;
        // `d_bcount` and `di_nblocks` are both 512-byte basic blocks.  The
        // transfer must move the complete on-disk ownership charge, including
        // data, attribute, and external-BMBT allocations.
        let blocks = i64::try_from(inode.blocks).map_err(|_| XfsError::AddressOutOfRange)?;
        for (kind, root, old, new) in [
            (1u8, roots.user, inode.uid, uid),
            (4, roots.group, inode.gid, gid),
            (2, roots.project, inode.project_id, project_id),
        ] {
            if root.is_some() && self.volume.quota_accounting_enabled(kind) && old != new {
                self.volume
                    .stage_dquot_delta(kind, old, -blocks, -1, metadata)?;
                self.volume
                    .stage_dquot_delta(kind, new, blocks, 1, metadata)?;
            }
        }
        Ok(())
    }

    pub(super) fn staged_inode_block_delta(
        &self,
        inode: u64,
        metadata: &XfsMetadataTransaction,
    ) -> XfsResult<i64> {
        let inode_size = usize::from(self.volume.superblock.inode_size);
        for buffer in &metadata.buffers {
            if buffer.metadata_type != XfsMetadataBufferType::Inode
                || buffer.before.len() != buffer.after.len()
                || buffer.before.len() % inode_size != 0
            {
                continue;
            }
            for (before, after) in buffer
                .before
                .chunks_exact(inode_size)
                .zip(buffer.after.chunks_exact(inode_size))
            {
                if be16(before, 0)? == XFS_DINODE_MAGIC && be64(before, 152)? == inode {
                    let old = be64(before, 64)?;
                    let new = be64(after, 64)?;
                    return if new >= old {
                        i64::try_from(new - old).map_err(|_| XfsError::AddressOutOfRange)
                    } else {
                        i64::try_from(old - new)
                            .map_err(|_| XfsError::AddressOutOfRange)
                            .map(|value| -value)
                    };
                }
            }
        }
        Err(XfsError::CorruptMetadata)
    }

    pub(super) fn restage_prepared_inode_size(
        &self,
        inode: u64,
        size: u64,
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let inode_size = usize::from(self.volume.superblock.inode_size);
        for buffer in &mut metadata.buffers {
            if buffer.metadata_type != XfsMetadataBufferType::Inode
                || buffer.before.len() != buffer.after.len()
                || buffer.before.len() % inode_size != 0
            {
                continue;
            }
            for (before, after) in buffer
                .before
                .chunks_exact(inode_size)
                .zip(buffer.after.chunks_exact_mut(inode_size))
            {
                if be16(before, 0)? != XFS_DINODE_MAGIC || be64(before, 152)? != inode {
                    continue;
                }
                put_be64(after, 56, size)?;
                rewrite_crc32c(after, 100)?;
                return Ok(());
            }
        }
        Err(XfsError::CorruptMetadata)
    }

    /// Runs one VFS read while excluding publication of every home image in a
    /// live transaction.  The lock deliberately spans decoding as well as the
    /// initial inode lookup: an external directory/attribute tree can require
    /// several home blocks whose individually FUA-complete writes are not a
    /// coherent namespace until the coordinator releases this guard.
    pub(crate) fn read_coherent<T>(
        &self,
        read: impl FnOnce(&XfsVolume) -> XfsResult<T>,
    ) -> XfsResult<T> {
        let _live = self.live.lock();
        read(&self.volume)
    }

    pub(super) fn push_ail_locked(&self, live: &mut XfsLiveLogState) -> XfsResult<()> {
        let Some(through_lsn) = live.ail.entries().last().map(|entry| entry.lsn) else {
            return Ok(());
        };
        self.volume
            .checkpoint_live_log(&mut live.ring, &mut live.ail, through_lsn, |entry| {
                if entry.checkpoint_homes.is_empty() {
                    return Err(XfsError::CorruptMetadata);
                }
                for (block, image) in &entry.checkpoint_homes {
                    self.volume
                        .write_basic_blocks_fua(&self.volume.data, *block, image)?;
                }
                Ok(())
            })
    }

    /// Forces all ordinary AIL work to durable home blocks and then fences
    /// every member device.  This is the sync path; it intentionally does not
    /// manufacture a clean-unmount marker while callers remain active.
    pub(crate) fn flush_live(&self) -> XfsResult<()> {
        let mut live = self.live.lock();
        if live.failed {
            return Err(XfsError::Io);
        }
        let result = self
            .push_ail_locked(&mut live)
            .and_then(|()| self.volume.flush());
        if let Err(error) = result {
            live.failed = true;
            return Err(error);
        }
        Ok(())
    }

    /// Writes a terminal XFS unmount record only after every preceding AIL
    /// item has reached its home block and every member is durable.  The
    /// marker itself is log FUA/forced by `persist_live_log_commit`; it carries
    /// no home image, so its ring space becomes reclaimable immediately.
    pub(crate) fn clean_unmount(&self) -> XfsResult<()> {
        let mut live = self.live.lock();
        if live.failed {
            return Err(XfsError::Io);
        }
        let result = (|| {
            // Force the preceding log before asking the AIL to make any
            // record reclaimable; an external log is intentionally distinct
            // from the data member here.
            self.volume.log_volume()?.flush().map_err(XfsError::from)?;
            self.push_ail_locked(&mut live)?;
            if !live.ail.entries().is_empty() {
                return Err(XfsError::CorruptMetadata);
            }
            self.volume.flush()?;
            let transaction = live.next_transaction;
            let operations = [XfsLogOperation {
                transaction_id: transaction,
                // Linux XFS_LOG client and xfs_unmount_log_format magic.
                client_id: 0xaa,
                flags: XLOG_UNMOUNT_TRANS,
                payload: vec![0x55, 0x6e, 0, 0, 0, 0, 0, 0],
            }];
            let tail_lsn = live.ring.tail_lsn();
            let prepared = self.volume.prepare_live_log_commit(
                &mut live.ring,
                transaction,
                tail_lsn,
                &operations,
            )?;
            let end_lsn = prepared.reservation.end_lsn(live.ring.blocks())?;
            self.volume.persist_clean_unmount_record(&prepared)?;
            live.ring.checkpoint_tail(end_lsn)?;
            live.next_transaction = live
                .next_transaction
                .checked_add(1)
                .ok_or(XfsError::AddressOutOfRange)?;
            Ok(())
        })();
        if let Err(error) = result {
            live.failed = true;
            return Err(error);
        }
        Ok(())
    }

    /// Atomically replaces the persistent file-attribute portion of a v3
    /// inode.  Attribute callers cannot publish an inode core without going
    /// through the same recovered live-log coordinator as data and namespace
    /// mutations.
    pub(crate) fn set_file_attr(
        &self,
        inode: u64,
        attr: XfsFileAttr,
        ctime_seconds: i64,
        ctime_nanoseconds: u32,
    ) -> XfsResult<()> {
        let mut live = self.live.lock();
        let current = self.volume.inode(inode)?;
        if current.version < 3 || !self.volume.superblock.is_v5() {
            return Err(XfsError::UnsupportedFeature);
        }

        let mut metadata = XfsMetadataTransaction::default();
        self.stage_quota_owner_transfer(
            &current,
            current.uid,
            current.gid,
            attr.project_id,
            &mut metadata,
        )?;
        self.volume.stage_file_attr(
            inode,
            attr,
            ctime_seconds,
            ctime_nanoseconds,
            &mut metadata,
        )?;
        self.commit_locked(&mut live, &metadata)
    }

    /// Replaces the supported fixed dinode-core fields in one live XFS
    /// transaction.  The live lock covers sampling the old raw inode, CRC
    /// construction, log commit, and AIL publication; no caller can make a
    /// mode/owner/time subset durable independently of the rest.
    pub(crate) fn update_inode_core(
        &self,
        inode: u64,
        update: XfsInodeCoreUpdate,
    ) -> XfsResult<()> {
        if update.is_empty() {
            return Ok(());
        }
        let mut live = self.live.lock();
        let mut metadata = XfsMetadataTransaction::default();
        let current = self.volume.inode(inode)?;
        if let Some((uid, gid)) = update.owner {
            self.stage_quota_owner_transfer(&current, uid, gid, current.project_id, &mut metadata)?;
        }
        self.volume
            .stage_inode_core_update(inode, update, &mut metadata)?;
        self.commit_locked(&mut live, &metadata)
    }

    /// Replaces a supported symlink's target under the one live-log lock.
    /// The replacement data is staged in newly allocated blocks before the
    /// log can switch the dinode mapping, so an I/O/log failure cannot expose
    /// a partially overwritten old target.
    pub(crate) fn replace_symlink(
        &self,
        inode: u64,
        target: &[u8],
        seconds: i64,
        nanoseconds: u32,
    ) -> XfsResult<()> {
        let mut live = self.live.lock();
        let metadata =
            self.volume
                .stage_symlink_replacement(inode, target, seconds, nanoseconds)?;
        self.commit_locked(&mut live, &metadata)
    }

    /// Publishes a caller-composed metadata set under the mount's sole log
    /// coordinator.  This is deliberately the only public composition point:
    /// inode, AG and every affected directory image share one transaction id,
    /// one durable log record and one home-write checkpoint.
    pub fn commit_staged(&self, metadata: XfsMetadataTransaction) -> XfsResult<()> {
        let mut live = self.live.lock();
        self.commit_locked(&mut live, &metadata)
    }

    /// Allocates, initializes, and names one regular or directory inode under
    /// the mount log lock.  The inobt/finobt transition, new inode core,
    /// parent namespace image and parent directory link count form one log
    /// record; callers cannot observe an allocated-but-unnamed inode.
    pub fn create_named_inode(
        &self,
        parent: u64,
        name: &[u8],
        initial: XfsNewInode,
        exclusive: bool,
    ) -> XfsResult<XfsNamedInodeOutcome> {
        if name.is_empty()
            || name == b"."
            || name == b".."
            || name.iter().any(|byte| *byte == 0 || *byte == b'/')
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut live = self.live.lock();
        let parent_inode = self.volume.inode(parent)?;
        if parent_inode.mode & 0o170000 != 0o040000 {
            return Err(XfsError::UnsupportedFeature);
        }
        let mut entries = self.volume.directory_entries(parent)?;
        if let Some(entry) = entries.iter().find(|entry| entry.name == name) {
            if exclusive {
                return Err(XfsError::AddressOutOfRange);
            }
            return Ok(XfsNamedInodeOutcome::Existing(entry.inode));
        }
        let mode = initial.mode;
        let is_directory = mode & 0o170000 == 0o040000;
        let is_symlink = mode & 0o170000 == 0o120000;
        if is_directory != initial.parent.is_some()
            || is_symlink != initial.symlink_target.is_some()
            || (!is_symlink && initial.symlink_target.is_some())
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let (ag, _) = self.volume.split_inode_number(parent)?;
        let remote_blocks = initial
            .symlink_target
            .as_ref()
            .and_then(|target| {
                (target.len() > self.volume.superblock.inode_size as usize - 176).then(|| {
                    u32::try_from(
                        target
                            .len()
                            .div_ceil(self.volume.superblock.block_size as usize),
                    )
                    .ok()
                })
            })
            .flatten();
        let allocation = match remote_blocks {
            Some(blocks) => self
                .volume
                .prepare_inode_allocation_with_remote(ag, blocks)?,
            None => self.volume.prepare_inode_allocation(ag)?,
        };
        let mut metadata = XfsMetadataTransaction::default();
        let (uid, gid, project_id) = (initial.uid, initial.gid, initial.project_id);
        self.volume
            .stage_new_inode(&allocation, initial, &mut metadata)?;
        self.stage_quota_identity_delta(
            allocation.inode,
            uid,
            gid,
            project_id,
            0,
            1,
            &mut metadata,
        )?;
        entries.push(XfsDirectoryEntry {
            name: name.to_vec(),
            inode: allocation.inode,
            file_type: Some(directory_type_for_inode(mode)),
        });
        self.volume.stage_directory_entries_with_parent(
            parent,
            self.volume.directory_parent(parent)?,
            &entries,
            &mut metadata,
        )?;
        if is_directory {
            self.volume.stage_inode_link_count(
                parent,
                parent_inode
                    .nlink
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?,
                &mut metadata,
            )?;
        }
        self.commit_locked(&mut live, &metadata)?;
        Ok(XfsNamedInodeOutcome::Created(allocation.inode))
    }

    /// Commits a set of complete directory images atomically.  Shortform
    /// promotions are preflighted first: each needs an AG free-space delta,
    /// and two candidates in one AG cannot be independently selected from
    /// the same on-disk snapshot.  The future leaf/node allocator replaces
    /// that precise conflict with one combined AG planner; until then it is
    /// rejected before any metadata is staged or logged.
    pub fn replace_directories(&self, updates: &[XfsDirectoryUpdate]) -> XfsResult<()> {
        let mut live = self.live.lock();
        let mut metadata = XfsMetadataTransaction::default();
        self.stage_directory_updates(updates, &[], &[], &mut metadata)?;
        self.commit_locked(&mut live, &metadata)
    }

    /// Adds a second hard link for a non-directory inode.  The directory
    /// image and inode-core link count are deliberately staged from the same
    /// locked snapshot, so the name is never durable without its reference.
    pub fn link_named(
        &self,
        directory: u64,
        name: &[u8],
        target: u64,
        expected_generation: u32,
    ) -> XfsResult<()> {
        Self::validate_namespace_name(name)?;
        let mut live = self.live.lock();
        let target_inode = self.volume.inode(target)?;
        if target_inode.generation != expected_generation {
            return Err(XfsError::AddressOutOfRange);
        }
        if target_inode.mode & 0o170000 == 0o040000 || target_inode.nlink == 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let links = target_inode
            .nlink
            .checked_add(1)
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut entries = self.volume.directory_entries(directory)?;
        if entries.iter().any(|entry| entry.name == name) {
            return Err(XfsError::AddressOutOfRange);
        }
        entries.push(XfsDirectoryEntry {
            name: name.to_vec(),
            inode: target,
            file_type: Some(directory_type_for_inode(target_inode.mode)),
        });
        let parent = self.volume.directory_parent(directory)?;
        let mut metadata = XfsMetadataTransaction::default();
        self.stage_directory_updates(
            &[XfsDirectoryUpdate {
                directory,
                parent,
                entries,
            }],
            &[],
            &[],
            &mut metadata,
        )?;
        self.volume
            .stage_inode_link_count(target, links, &mut metadata)?;
        self.commit_locked(&mut live, &metadata)
    }

    /// Removes one non-directory name.  A final regular-file link also
    /// truncates and returns its data extents and inode bit in this exact log
    /// transaction; special inode reclamation is not guessed here.
    pub fn unlink_named(
        &self,
        directory: u64,
        name: &[u8],
        expected: Option<(u64, u32)>,
    ) -> XfsResult<()> {
        Self::validate_namespace_name(name)?;
        let mut live = self.live.lock();
        let mut entries = self.volume.directory_entries(directory)?;
        let index = entries
            .iter()
            .position(|entry| entry.name == name)
            .ok_or(XfsError::AddressOutOfRange)?;
        let target = entries[index].inode;
        let inode = self.volume.inode(target)?;
        if expected
            .is_some_and(|(number, generation)| number != target || generation != inode.generation)
        {
            return Err(XfsError::AddressOutOfRange);
        }
        if inode.mode & 0o170000 == 0o040000 {
            return Err(XfsError::UnsupportedFeature);
        }
        let links = inode
            .nlink
            .checked_sub(1)
            .ok_or(XfsError::CorruptMetadata)?;
        entries.remove(index);
        let parent = self.volume.directory_parent(directory)?;
        let mut metadata = XfsMetadataTransaction::default();
        self.stage_directory_updates(
            &[XfsDirectoryUpdate {
                directory,
                parent,
                entries,
            }],
            &[],
            &[],
            &mut metadata,
        )?;
        self.stage_last_unlink(target, &inode, links, &mut metadata)?;
        self.commit_locked(&mut live, &metadata)
    }

    /// Removes an empty directory.  The child `..`, parent image, parent
    /// nlink, child nlink and inobt/finobt transition share one log record.
    /// External directory trees are intentionally refused until their data,
    /// leaf/node and free-space teardown is implemented as one planner.
    pub fn rmdir_named(
        &self,
        directory: u64,
        name: &[u8],
        expected: Option<(u64, u32)>,
    ) -> XfsResult<()> {
        Self::validate_namespace_name(name)?;
        let mut live = self.live.lock();
        let mut entries = self.volume.directory_entries(directory)?;
        let index = entries
            .iter()
            .position(|entry| entry.name == name)
            .ok_or(XfsError::AddressOutOfRange)?;
        let target = entries[index].inode;
        let child = self.volume.inode(target)?;
        if expected
            .is_some_and(|(number, generation)| number != target || generation != child.generation)
        {
            return Err(XfsError::AddressOutOfRange);
        }
        if child.mode & 0o170000 != 0o040000 || child.nlink != 2 {
            return Err(XfsError::UnsupportedFeature);
        }
        if !self.volume.directory_entries(target)?.is_empty() {
            return Err(XfsError::NotEmpty);
        }
        let parent_inode = self.volume.inode(directory)?;
        let parent_links = parent_inode
            .nlink
            .checked_sub(1)
            .ok_or(XfsError::CorruptMetadata)?;
        entries.remove(index);
        let parent = self.volume.directory_parent(directory)?;
        let mut metadata = XfsMetadataTransaction::default();
        let teardown = (matches!(
            child.data_format,
            XfsForkFormat::Extents | XfsForkFormat::Btree
        ) || matches!(
            child.attr_format,
            XfsForkFormat::Extents | XfsForkFormat::Btree
        ))
        .then_some(target);
        self.stage_directory_updates(
            &[XfsDirectoryUpdate {
                directory,
                parent,
                entries,
            }],
            &[target],
            teardown.as_slice(),
            &mut metadata,
        )?;
        self.volume
            .stage_inode_link_count(directory, parent_links, &mut metadata)?;
        self.volume
            .stage_inode_link_count(target, 0, &mut metadata)?;
        self.volume
            .stage_directory_reclaim_inode(target, &mut metadata)?;
        self.commit_locked(&mut live, &metadata)
    }

    /// Ordinary (non-exchange, non-whiteout) rename, including replacement.
    /// Every changed directory representation, a moved directory's native
    /// `..`, parent link deltas, and a replaced inode's final reclaim are
    /// composed before the sole log commit.
    pub fn rename_named(
        &self,
        old_parent: u64,
        old_name: &[u8],
        source_expected: (u64, u32),
        new_parent: u64,
        new_name: &[u8],
        destination_expected: Option<(u64, u32)>,
    ) -> XfsResult<()> {
        Self::validate_namespace_name(old_name)?;
        Self::validate_namespace_name(new_name)?;
        let mut live = self.live.lock();
        let mut old_entries = self.volume.directory_entries(old_parent)?;
        let old_index = old_entries
            .iter()
            .position(|entry| entry.name == old_name)
            .ok_or(XfsError::AddressOutOfRange)?;
        let source_entry = old_entries[old_index].clone();
        let source = self.volume.inode(source_entry.inode)?;
        if source.number != source_expected.0 || source.generation != source_expected.1 {
            return Err(XfsError::AddressOutOfRange);
        }
        let source_directory = source.mode & 0o170000 == 0o040000;
        let same_parent = old_parent == new_parent;
        if same_parent && old_name == new_name {
            return Ok(());
        }
        let mut new_entries = if same_parent {
            old_entries.clone()
        } else {
            self.volume.directory_entries(new_parent)?
        };
        let destination = new_entries
            .iter()
            .position(|entry| entry.name == new_name)
            .map(|index| self.volume.inode(new_entries[index].inode))
            .transpose()?;
        match (destination_expected, destination.as_ref()) {
            (None, None) => {}
            (Some((number, generation)), Some(inode))
                if inode.number == number && inode.generation == generation => {}
            _ => return Err(XfsError::AddressOutOfRange),
        }
        if let Some(destination) = &destination {
            let destination_directory = destination.mode & 0o170000 == 0o040000;
            if source_directory != destination_directory {
                return Err(XfsError::UnsupportedFeature);
            }
            if destination_directory
                && (destination.nlink != 2
                    || !self
                        .volume
                        .directory_entries(destination.number)?
                        .is_empty()
                    || !matches!(
                        destination.data_format,
                        XfsForkFormat::Local | XfsForkFormat::Extents | XfsForkFormat::Btree
                    ))
            {
                return Err(XfsError::UnsupportedFeature);
            }
        }
        if source_directory && !same_parent {
            self.reject_directory_cycle(source.number, new_parent)?;
        }

        // Remove the old name first.  For a same-parent rename use one image
        // so an old/new collision cannot create two copies of the same slot.
        old_entries.remove(old_index);
        if same_parent {
            new_entries = old_entries.clone();
        }
        if let Some(index) = new_entries.iter().position(|entry| entry.name == new_name) {
            new_entries[index] = XfsDirectoryEntry {
                name: new_name.to_vec(),
                inode: source.number,
                file_type: Some(directory_type_for_inode(source.mode)),
            };
        } else {
            new_entries.push(XfsDirectoryEntry {
                name: new_name.to_vec(),
                inode: source.number,
                file_type: Some(directory_type_for_inode(source.mode)),
            });
        }

        let old_parent_of_directory = self.volume.directory_parent(old_parent)?;
        let new_parent_of_directory = if same_parent {
            old_parent_of_directory
        } else {
            self.volume.directory_parent(new_parent)?
        };
        let mut updates = Vec::new();
        updates.push(XfsDirectoryUpdate {
            directory: old_parent,
            parent: old_parent_of_directory,
            entries: old_entries,
        });
        if !same_parent {
            updates.push(XfsDirectoryUpdate {
                directory: new_parent,
                parent: new_parent_of_directory,
                entries: new_entries,
            });
        } else {
            updates[0].entries = new_entries;
        }
        if source_directory && !same_parent {
            updates.push(XfsDirectoryUpdate {
                directory: source.number,
                parent: new_parent,
                entries: self.volume.directory_entries(source.number)?,
            });
        }

        let old_parent_inode = self.volume.inode(old_parent)?;
        let new_parent_inode = if same_parent {
            old_parent_inode.clone()
        } else {
            self.volume.inode(new_parent)?
        };
        let mut old_links = old_parent_inode.nlink;
        let mut new_links = new_parent_inode.nlink;
        if source_directory && !same_parent {
            old_links = old_links.checked_sub(1).ok_or(XfsError::CorruptMetadata)?;
            new_links = new_links
                .checked_add(1)
                .ok_or(XfsError::AddressOutOfRange)?;
        }
        let mut metadata = XfsMetadataTransaction::default();
        let freed = destination
            .as_ref()
            .filter(|inode| inode.mode & 0o170000 == 0o040000)
            .map(|inode| vec![inode.number])
            .unwrap_or_default();
        let teardown = destination
            .as_ref()
            .filter(|inode| {
                inode.mode & 0o170000 == 0o040000
                    && (matches!(
                        inode.data_format,
                        XfsForkFormat::Extents | XfsForkFormat::Btree
                    ) || matches!(
                        inode.attr_format,
                        XfsForkFormat::Extents | XfsForkFormat::Btree
                    ))
            })
            .map(|inode| inode.number);
        self.stage_directory_updates(&updates, &freed, teardown.as_slice(), &mut metadata)?;
        if let Some(destination) = destination {
            if destination.mode & 0o170000 == 0o040000 {
                // Removing the replaced directory removes one subdirectory
                // from new_parent; the source move above adds its replacement.
                new_links = new_links.checked_sub(1).ok_or(XfsError::CorruptMetadata)?;
                self.volume
                    .stage_inode_link_count(destination.number, 0, &mut metadata)?;
                self.volume
                    .stage_directory_reclaim_inode(destination.number, &mut metadata)?;
            } else {
                let links = destination
                    .nlink
                    .checked_sub(1)
                    .ok_or(XfsError::CorruptMetadata)?;
                self.stage_last_unlink(destination.number, &destination, links, &mut metadata)?;
            }
        }
        if same_parent {
            if old_links != old_parent_inode.nlink || new_links != old_parent_inode.nlink {
                self.volume
                    .stage_inode_link_count(old_parent, new_links, &mut metadata)?;
            }
        } else {
            if old_links != old_parent_inode.nlink {
                self.volume
                    .stage_inode_link_count(old_parent, old_links, &mut metadata)?;
            }
            if new_links != new_parent_inode.nlink {
                self.volume
                    .stage_inode_link_count(new_parent, new_links, &mut metadata)?;
            }
        }
        self.commit_locked(&mut live, &metadata)
    }

    pub(super) fn validate_namespace_name(name: &[u8]) -> XfsResult<()> {
        if name.is_empty()
            || name.len() > 255
            || name == b"."
            || name == b".."
            || name.iter().any(|byte| *byte == 0 || *byte == b'/')
        {
            return Err(XfsError::AddressOutOfRange);
        }
        Ok(())
    }

    pub(super) fn reject_directory_cycle(&self, source: u64, mut parent: u64) -> XfsResult<()> {
        for _ in 0..=self.volume.superblock.ag_count {
            if parent == source {
                return Err(XfsError::AddressOutOfRange);
            }
            let next = self.volume.directory_parent(parent)?;
            if next == parent {
                return Ok(());
            }
            parent = next;
        }
        Err(XfsError::CorruptMetadata)
    }

    pub(super) fn stage_last_unlink(
        &self,
        inode_number: u64,
        inode: &XfsInode,
        links: u32,
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if links != 0 {
            return self
                .volume
                .stage_inode_link_count(inode_number, links, metadata);
        }
        if inode.mode & 0o170000 != 0o100000 {
            return Err(XfsError::UnsupportedFeature);
        }
        let reclaim = self.volume.prepare_regular_truncate(inode_number, 0)?;
        metadata.buffers.extend(reclaim.buffers);
        self.volume
            .stage_inode_link_count(inode_number, 0, metadata)?;
        metadata
            .buffers
            .extend(self.volume.prepare_inode_free(inode_number)?.buffers);
        self.stage_inode_quota_delta(
            inode,
            -i64::try_from(inode.blocks).map_err(|_| XfsError::AddressOutOfRange)?,
            -1,
            metadata,
        )?;
        Ok(())
    }

    pub(super) fn stage_directory_updates(
        &self,
        updates: &[XfsDirectoryUpdate],
        free_inodes: &[u64],
        teardown: &[u64],
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if updates.is_empty()
            || updates.iter().enumerate().any(|(index, update)| {
                update.directory == 0
                    || updates[..index]
                        .iter()
                        .any(|prior| prior.directory == update.directory)
            })
        {
            return Err(XfsError::AddressOutOfRange);
        }
        // Each tuple is (update index, inode AG, requested blocks, external).
        // A single AG snapshot must serve every directory changed by one
        // rename: independently staged AGF/AGFL images cannot compose.
        let mut reserved = Vec::<(usize, u32, u32, bool)>::new();
        let mut releases = Vec::<u64>::new();
        for (index, update) in updates.iter().enumerate() {
            let inode = self.volume.inode(update.directory)?;
            if inode.data_format == XfsForkFormat::Local {
                let mut probe = XfsMetadataTransaction::default();
                match self.volume.stage_shortform_directory(
                    update.directory,
                    update.parent,
                    &update.entries,
                    &mut probe,
                ) {
                    Ok(()) => {}
                    Err(XfsError::AddressOutOfRange) => {
                        let (ag, _) = self.volume.split_inode_number(update.directory)?;
                        let blocks = u32::try_from(
                            self.volume.directory_block_size()?
                                / self.volume.superblock.block_size as usize,
                        )
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                        reserved.push((index, ag, blocks, false));
                    }
                    Err(error) => return Err(error),
                }
            } else if matches!(
                inode.data_format,
                XfsForkFormat::Extents | XfsForkFormat::Btree
            ) {
                let (ag, _) = self.volume.split_inode_number(update.directory)?;
                let blocks = self.volume.directory_rebuild_blocks(
                    update.directory,
                    update.parent,
                    &update.entries,
                )?;
                releases.extend(self.volume.directory_rebuild_releases(update.directory)?);
                reserved.push((index, ag, blocks, true));
            } else {
                return Err(XfsError::UnsupportedFeature);
            }
        }
        for inode in teardown {
            releases.extend(self.volume.directory_teardown_releases(*inode)?);
        }
        let mut batches = Vec::<(u32, Vec<XfsExtentAllocation>)>::new();
        for (_, ag, ..) in &reserved {
            if batches.iter().any(|(present, _)| present == ag) {
                continue;
            }
            let requests = reserved
                .iter()
                .filter(|(_, candidate, ..)| candidate == ag)
                .map(|(_, _, blocks, _)| *blocks)
                .collect::<Vec<_>>();
            let batch = self.volume.prepare_extent_allocations(*ag, &requests)?;
            let allocations = batch.allocations;
            let pairs = allocations
                .iter()
                .map(|allocation| (allocation.start_block, allocation.block_count))
                .collect::<Vec<_>>();
            self.volume.stage_directory_rebuild_allocator_delta(
                *ag,
                &pairs,
                &releases,
                free_inodes,
                metadata,
            )?;
            batches.push((*ag, allocations));
        }
        // Return old external blocks belonging to AGs without a replacement
        // allocation (a cross-AG move can make that happen) through their
        // own single canonical free-space delta.
        for physical in &releases {
            let ag = u32::try_from(*physical / self.volume.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            if !batches.iter().any(|(present, _)| *present == ag) {
                self.volume.stage_directory_rebuild_allocator_delta(
                    ag,
                    &[],
                    &releases,
                    free_inodes,
                    metadata,
                )?;
                batches.push((ag, Vec::new()));
            }
        }
        for inode in free_inodes {
            let (ag, _) = self.volume.split_inode_number(*inode)?;
            if !batches.iter().any(|(present, _)| *present == ag) {
                self.volume.stage_directory_rebuild_allocator_delta(
                    ag,
                    &[],
                    &releases,
                    free_inodes,
                    metadata,
                )?;
                batches.push((ag, Vec::new()));
            }
        }
        for (index, update) in updates.iter().enumerate() {
            if let Some((_, ag, _, external)) =
                reserved.iter().find(|(candidate, ..)| *candidate == index)
            {
                let ordinal = reserved
                    .iter()
                    .take_while(|(candidate, ..)| *candidate != index)
                    .filter(|(_, candidate, ..)| candidate == ag)
                    .count();
                let allocation = batches
                    .iter()
                    .find(|(candidate, _)| candidate == ag)
                    .and_then(|(_, allocations)| allocations.get(ordinal))
                    .ok_or(XfsError::CorruptMetadata)?;
                if *external {
                    self.volume.stage_directory_block_with_reservation(
                        update.directory,
                        update.parent,
                        &update.entries,
                        allocation,
                        metadata,
                    )?;
                } else {
                    self.volume.stage_shortform_directory_promotion(
                        update.directory,
                        update.parent,
                        &update.entries,
                        allocation,
                        metadata,
                    )?;
                }
            } else {
                self.volume.stage_directory_entries_with_parent(
                    update.directory,
                    update.parent,
                    &update.entries,
                    metadata,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn commit_locked(
        &self,
        live: &mut XfsLiveLogState,
        metadata: &XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if live.failed {
            return Err(XfsError::Io);
        }
        let transaction = live.next_transaction;
        let result = self
            .volume
            .commit_metadata_transaction(&mut live.ring, &mut live.ail, transaction, metadata)
            .map(|_| ());
        match result {
            Ok(()) => {
                live.next_transaction = live
                    .next_transaction
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?;
                Ok(())
            }
            Err(error) => {
                live.failed = true;
                Err(error)
            }
        }
    }

    pub fn read_at(&self, inode: u64, offset: u64, output: &mut [u8]) -> XfsResult<usize> {
        self.read_coherent(|volume| volume.read_inode_at(inode, offset, output))
    }

    pub fn shortform_xattrs(&self, inode: u64) -> XfsResult<Vec<XfsShortformXattr>> {
        let _live = self.live.lock();
        self.volume.shortform_xattrs(inode)
    }

    /// Takes a coherent xattr snapshot irrespective of the fork format.  The
    /// live-log lock makes lookup/list and a following mutation see the same
    /// committed image, rather than mixing a leaf before a journal commit
    /// with an inode after it.
    pub fn xattrs(&self, inode: u64) -> XfsResult<Vec<XfsShortformXattr>> {
        let _live = self.live.lock();
        self.volume.xattrs(inode)
    }

    pub fn write_at(&self, inode: u64, offset: u64, data: &[u8]) -> XfsResult<usize> {
        let mut live = self.live.lock();
        if live.failed {
            return Err(XfsError::Io);
        }
        let transaction = live.next_transaction;
        let owner = self.volume.inode(inode)?;
        let mut prepared = self
            .volume
            .prepare_regular_write(inode, offset, data.len())?;
        self.rewrite_shared_write_as_cow(&owner, &mut prepared)?;
        let blocks = self.staged_inode_block_delta(inode, &prepared.metadata)?;
        self.stage_inode_quota_delta(&owner, blocks, 0, &mut prepared.metadata)?;
        let result = {
            let XfsLiveLogState { ring, ail, .. } = &mut *live;
            self.volume.write_prepared_regular_at_live(
                ring,
                ail,
                transaction,
                prepared,
                offset,
                data,
            )
        };
        match result {
            Ok(written) => {
                live.next_transaction = live
                    .next_transaction
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?;
                Ok(written)
            }
            Err(error) => {
                live.failed = true;
                Err(error)
            }
        }
    }

    /// Samples EOF and commits the write under one live-log critical section;
    /// two concurrent O_APPEND writers therefore receive distinct offsets.
    pub fn append(&self, inode: u64, data: &[u8]) -> XfsResult<(usize, u64)> {
        let mut live = self.live.lock();
        if live.failed {
            return Err(XfsError::Io);
        }
        let owner = self.volume.inode(inode)?;
        let offset = owner.size;
        let transaction = live.next_transaction;
        let mut prepared = self
            .volume
            .prepare_regular_write(inode, offset, data.len())?;
        self.rewrite_shared_write_as_cow(&owner, &mut prepared)?;
        let blocks = self.staged_inode_block_delta(inode, &prepared.metadata)?;
        self.stage_inode_quota_delta(&owner, blocks, 0, &mut prepared.metadata)?;
        let result = {
            let XfsLiveLogState { ring, ail, .. } = &mut *live;
            self.volume.write_prepared_regular_at_live(
                ring,
                ail,
                transaction,
                prepared,
                offset,
                data,
            )
        };
        match result {
            Ok(written) => {
                live.next_transaction = live
                    .next_transaction
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?;
                Ok((written, offset))
            }
            Err(error) => {
                live.failed = true;
                Err(error)
            }
        }
    }

    /// Replaces the normal allocator image for a write which touches a
    /// reflink-shared block.  A shared extent is never modified in place:
    /// every new data/BMBT home, rmap owner transition, refcount transition,
    /// free-space transition and final inode mapping is derived from the same
    /// per-AG planner and committed in one log transaction.
    pub(super) fn rewrite_shared_write_as_cow(
        &self,
        inode: &XfsInode,
        prepared: &mut XfsRegularWrite,
    ) -> XfsResult<()> {
        if !self.volume.superblock.features.has_rmapbt()
            || !self.volume.superblock.features.has_reflink()
        {
            return Ok(());
        }
        let block_size = u64::from(self.volume.superblock.block_size);
        let first = prepared.offset / block_size;
        let last = prepared
            .offset
            .checked_add(prepared.length as u64)
            .ok_or(XfsError::AddressOutOfRange)?
            .checked_sub(1)
            .ok_or(XfsError::AddressOutOfRange)?
            / block_size;
        let old = match inode.data_format {
            XfsForkFormat::Extents => self.volume.inode_data_extents(inode.number)?,
            XfsForkFormat::Btree => self.volume.inode_bmbt_extents(inode.number)?,
            _ => return Err(XfsError::UnsupportedFeature),
        };
        let shared = (first..=last).any(|file_block| {
            xfs_extent_at(&old, file_block)
                .ok()
                .flatten()
                .is_some_and(|extent| {
                    let ag =
                        (extent.start_block / u64::from(self.volume.superblock.ag_blocks)) as u32;
                    self.volume
                        .refcount_records(ag)
                        .ok()
                        .is_some_and(|records| {
                            records.iter().any(|record| {
                                let local = (extent.start_block
                                    % u64::from(self.volume.superblock.ag_blocks))
                                    as u32;
                                local >= record.start_block
                                    && local < record.start_block.saturating_add(record.block_count)
                                    && record.refcount > 1
                            })
                        })
                })
        });
        let has_hole = (first..=last)
            .any(|file_block| xfs_extent_at(&old, file_block).ok().flatten().is_none());
        if !shared && !has_hole {
            return Ok(());
        }

        let mut planners = Vec::<XfsAgMutationPlanner>::new();
        let mut mappings = prepared.mappings.clone();
        let mut copies = Vec::new();
        for file_block in first..=last {
            let original = xfs_extent_at(&old, file_block)?;
            let current = xfs_extent_at(&mappings, file_block)?.ok_or(XfsError::CorruptMetadata)?;
            if let Some(source) = original {
                let old_plan = xfs_planner_for(&self.volume, &mut planners, source.start_block)?;
                let old_local =
                    u32::try_from(source.start_block % u64::from(self.volume.superblock.ag_blocks))
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                // Refcount and rmap must agree that this inode owns the old
                // block before it can be retired.  This rejects corrupt
                // one-sided sharing metadata rather than silently losing an
                // owner during COW.
                let count = old_plan.refcount_at(old_local)?;
                if count <= 1 {
                    continue;
                }
                old_plan.remove_owner(old_local, inode.number, file_block)?;
                old_plan.set_refcount(old_local, count - 1)?;
                let new_local = old_plan.claim_free_block()?;
                old_plan.add_owner(new_local, inode.number, file_block)?;
                let new_physical = u64::from(old_plan.ag)
                    .checked_mul(u64::from(self.volume.superblock.ag_blocks))
                    .and_then(|base| base.checked_add(u64::from(new_local)))
                    .ok_or(XfsError::AddressOutOfRange)?;
                xfs_replace_one_mapping(
                    &mut mappings,
                    file_block,
                    Some(XfsExtent {
                        unwritten: false,
                        file_block,
                        start_block: new_physical,
                        block_count: 1,
                    }),
                )?;
                copies.push((source.start_block, new_physical));
            } else {
                // A hole was selected by the ordinary allocator while
                // preparing this write.  Claim that exact home in the same
                // planner and publish its rmap alongside the COW changes.
                let plan = xfs_planner_for(&self.volume, &mut planners, current.start_block)?;
                let local = u32::try_from(
                    current.start_block % u64::from(self.volume.superblock.ag_blocks),
                )
                .map_err(|_| XfsError::AddressOutOfRange)?;
                plan.claim_specific_free_block(local)?;
                plan.add_owner(local, inode.number, file_block)?;
            }
        }

        let mut all = old.clone();
        for mapping in &mappings {
            xfs_replace_one_mapping(&mut all, mapping.file_block, Some(*mapping))?;
        }
        let old_bmap = if inode.data_format == XfsForkFormat::Btree {
            self.volume.inode_bmbt_blocks(inode.number)?
        } else {
            Vec::new()
        };
        let (raw_inode, inode_bytes) = self.volume.inode_and_bytes(inode.number)?;
        let required = bmap_external_blocks(
            self.volume.superblock,
            raw_inode.data_fork(&inode_bytes)?.len(),
            all.len(),
        )?;
        let reused = required.min(old_bmap.len());
        let inode_ag = self.volume.split_inode_number(inode.number)?.0;
        let mut bmap = old_bmap[..reused].to_vec();
        let mut excluded = copies
            .iter()
            .map(|(_, new)| {
                (
                    u32::try_from(*new / u64::from(self.volume.superblock.ag_blocks)).unwrap_or(0),
                    u32::try_from(*new % u64::from(self.volume.superblock.ag_blocks)).unwrap_or(0),
                    1,
                )
            })
            .collect::<Vec<_>>();
        for extent in &prepared.allocated {
            excluded.push((
                u32::try_from(extent.start_block / u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?,
                u32::try_from(extent.start_block % u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?,
                extent.block_count,
            ));
        }
        let fresh = self.volume.reserve_bmap_metadata_blocks(
            inode_ag,
            required.saturating_sub(reused),
            &excluded,
        )?;
        for physical in &fresh {
            let plan = xfs_planner_for(&self.volume, &mut planners, *physical)?;
            let local = u32::try_from(*physical % u64::from(self.volume.superblock.ag_blocks))
                .map_err(|_| XfsError::AddressOutOfRange)?;
            plan.claim_specific_free_block(local)?;
            plan.add_owner(local, inode.number, XFS_RMAP_OFF_BMBT)?;
        }
        bmap.extend_from_slice(&fresh);
        for physical in old_bmap.iter().copied().skip(reused) {
            let plan = xfs_planner_for(&self.volume, &mut planners, physical)?;
            let local = u32::try_from(physical % u64::from(self.volume.superblock.ag_blocks))
                .map_err(|_| XfsError::AddressOutOfRange)?;
            plan.remove_owner(local, inode.number, XFS_RMAP_OFF_BMBT)?;
            plan.release_free_block(local)?;
        }
        let mut metadata = XfsMetadataTransaction::default();
        for plan in &planners {
            metadata
                .buffers
                .extend(self.volume.stage_reflink_ag_plan(plan)?.buffers);
        }
        let end = prepared
            .offset
            .checked_add(prepared.length as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        if required == 0 {
            self.volume.stage_regular_inode_extents(
                inode.number,
                all,
                inode.size.max(end),
                &mut metadata,
            )?;
        } else {
            self.volume.stage_regular_inode_bmap(
                inode.number,
                all,
                inode.size.max(end),
                &bmap,
                &mut metadata,
            )?;
        }
        prepared.mappings = mappings;
        prepared.copy_before_write = copies;
        prepared.metadata = metadata;
        Ok(())
    }

    pub fn truncate(&self, inode: u64, size: u64) -> XfsResult<()> {
        let mut live = self.live.lock();
        let owner = self.volume.inode(inode)?;
        let mut transaction_data = self.volume.prepare_regular_truncate(inode, size)?;
        let blocks = self.staged_inode_block_delta(inode, &transaction_data)?;
        self.stage_inode_quota_delta(&owner, blocks, 0, &mut transaction_data)?;
        self.commit_locked(&mut live, &transaction_data)
    }

    pub fn fallocate(
        &self,
        inode: u64,
        offset: u64,
        length: u64,
        keep_size: bool,
    ) -> XfsResult<()> {
        let mut live = self.live.lock();
        let owner = self.volume.inode(inode)?;
        let mut prepared = self
            .volume
            .prepare_regular_fallocate(inode, offset, length, keep_size)?;
        if prepared.metadata.buffers.is_empty() {
            return Ok(());
        }
        let blocks = self.staged_inode_block_delta(inode, &prepared.metadata)?;
        self.stage_inode_quota_delta(&owner, blocks, 0, &mut prepared.metadata)?;
        self.commit_locked(&mut live, &prepared.metadata)
    }

    /// Makes every shared block in a range private without changing file
    /// contents or logical allocation.  The prepared write supplies the
    /// exact extent/BMBT shape; `rewrite_shared_write_as_cow` replaces only
    /// shared physical homes and retains the copied data ordering invariant.
    pub fn unshare_range(&self, inode: u64, offset: u64, length: u64) -> XfsResult<()> {
        let block = u64::from(self.volume.superblock.block_size);
        if length == 0 || offset % block != 0 || length % block != 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut live = self.live.lock();
        if live.failed {
            return Err(XfsError::Io);
        }
        let owner = self.volume.inode(inode)?;
        let end = offset
            .checked_add(length)
            .ok_or(XfsError::AddressOutOfRange)?;
        if end > owner.size {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut prepared = self.volume.prepare_regular_write(
            inode,
            offset,
            usize::try_from(length).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        self.rewrite_shared_write_as_cow(&owner, &mut prepared)?;
        if prepared.copy_before_write.is_empty() {
            return Ok(());
        }
        // Unlike an ordinary write, this has no user payload.  The COW data
        // homes must still be copied and flushed before their mapping is
        // logged, but `commit_metadata_transaction` owns that ordering for
        // staged data writes.
        for (old, new) in &prepared.copy_before_write {
            prepared.metadata.data_writes.push(XfsStagedDataWrite {
                fs_block: *new,
                before: self.volume.read_data_fs_block(*new)?,
                after: self.volume.read_data_fs_block(*old)?,
            });
        }
        let blocks = self.staged_inode_block_delta(inode, &prepared.metadata)?;
        self.stage_inode_quota_delta(&owner, blocks, 0, &mut prepared.metadata)?;
        self.commit_locked(&mut live, &prepared.metadata)
    }

    /// Zeroes the complete request in one prepared write transaction.  In
    /// particular, do not turn this into a loop of ordinary writes: a caller
    /// must never observe the first half zeroed while the second half still
    /// carries old data after a failed range operation.
    pub fn zero_range(
        &self,
        inode: u64,
        offset: u64,
        length: u64,
        keep_size: bool,
    ) -> XfsResult<()> {
        if length == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let end = offset
            .checked_add(length)
            .ok_or(XfsError::AddressOutOfRange)?;
        let length = usize::try_from(length).map_err(|_| XfsError::AddressOutOfRange)?;
        let mut live = self.live.lock();
        if live.failed {
            return Err(XfsError::Io);
        }
        let owner = self.volume.inode(inode)?;
        let mut prepared = self.volume.prepare_regular_write(inode, offset, length)?;
        self.rewrite_shared_write_as_cow(&owner, &mut prepared)?;
        // `prepare_regular_write` grows EOF for normal writes.  KEEP_SIZE
        // keeps its already-prepared extent/BMBT images, replacing only the
        // final dinode EOF in that same transaction.
        if keep_size && end > owner.size {
            self.restage_prepared_inode_size(inode, owner.size, &mut prepared.metadata)?;
        }
        let blocks = self.staged_inode_block_delta(inode, &prepared.metadata)?;
        self.stage_inode_quota_delta(&owner, blocks, 0, &mut prepared.metadata)?;
        let transaction = live.next_transaction;
        let result = {
            let XfsLiveLogState { ring, ail, .. } = &mut *live;
            self.volume
                .zero_prepared_regular_at_live(ring, ail, transaction, prepared, offset)
        };
        match result {
            Ok(_) => {
                live.next_transaction = live
                    .next_transaction
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?;
                Ok(())
            }
            Err(error) => {
                live.failed = true;
                Err(error)
            }
        }
    }

    /// Punches whole interior blocks from the fork and frees (or unshares)
    /// their physical homes in the very same transaction.  Unaligned edge
    /// blocks retain their mapping and are zeroed through the write/COW path.
    pub fn punch_hole(&self, inode: u64, offset: u64, length: u64) -> XfsResult<()> {
        self.rewrite_block_range(inode, RangeRewrite::Punch { offset, length })
    }

    /// Collapse is a logical extent-map translation.  Block-aligned suffixes
    /// retain their physical blocks; only rmap logical offsets move.
    pub fn collapse_range(&self, inode: u64, offset: u64, length: u64) -> XfsResult<()> {
        self.rewrite_block_range(inode, RangeRewrite::Collapse { offset, length })
    }

    /// Insert is likewise a logical mapping translation; the inserted range
    /// is sparse, so it reads as zero without fabricating data blocks.
    pub fn insert_range(&self, inode: u64, offset: u64, length: u64) -> XfsResult<()> {
        self.rewrite_block_range(inode, RangeRewrite::Insert { offset, length })
    }

    /// Builds the entire result fork before touching the log.  This is the
    /// range-operation counterpart to the reflink planner: the allocator,
    /// rmap/refcount ownership view, BMBT homes and dinode image are all
    /// derived from one locked snapshot and published by one commit.
    pub(super) fn rewrite_block_range(
        &self,
        inode_number: u64,
        operation: RangeRewrite,
    ) -> XfsResult<()> {
        let block_size = u64::from(self.volume.superblock.block_size);
        let mut live = self.live.lock();
        if live.failed {
            return Err(XfsError::Io);
        }
        let inode = self.volume.inode(inode_number)?;
        if inode.mode & 0o170000 != 0o100000 {
            return Err(XfsError::UnsupportedFeature);
        }
        // Every validation which depends on EOF is deliberately after the
        // coordinator lock.  A concurrent truncate cannot turn a previously
        // checked collapse/insert/punch into an out-of-range map mutation.
        let (first, end, operation) = match operation {
            RangeRewrite::Punch { offset, length } => {
                if length == 0 || offset >= inode.size {
                    return Err(XfsError::AddressOutOfRange);
                }
                let end = offset
                    .checked_add(length)
                    .ok_or(XfsError::AddressOutOfRange)?
                    .min(inode.size);
                (
                    offset.div_ceil(block_size),
                    end / block_size,
                    RangeRewrite::Punch {
                        offset,
                        length: end - offset,
                    },
                )
            }
            RangeRewrite::Collapse { offset, length } => {
                if length == 0 || offset % block_size != 0 || length % block_size != 0 {
                    return Err(XfsError::AddressOutOfRange);
                }
                let end = offset
                    .checked_add(length)
                    .ok_or(XfsError::AddressOutOfRange)?;
                if end > inode.size {
                    return Err(XfsError::AddressOutOfRange);
                }
                (
                    offset / block_size,
                    end / block_size,
                    RangeRewrite::Collapse { offset, length },
                )
            }
            RangeRewrite::Insert { offset, length } => {
                if length == 0
                    || offset % block_size != 0
                    || length % block_size != 0
                    || offset >= inode.size
                {
                    return Err(XfsError::AddressOutOfRange);
                }
                inode
                    .size
                    .checked_add(length)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let end = offset
                    .checked_add(length)
                    .ok_or(XfsError::AddressOutOfRange)?;
                (
                    offset / block_size,
                    end / block_size,
                    RangeRewrite::Insert { offset, length },
                )
            }
        };
        let old = match inode.data_format {
            XfsForkFormat::Extents => self.volume.inode_data_extents(inode_number)?,
            XfsForkFormat::Btree => self.volume.inode_bmbt_extents(inode_number)?,
            _ => return Err(XfsError::UnsupportedFeature),
        };
        let old_size_blocks = inode.size.div_ceil(block_size);
        if !matches!(operation, RangeRewrite::Insert { .. }) && end > old_size_blocks {
            return Err(XfsError::AddressOutOfRange);
        }
        let delta = match operation {
            RangeRewrite::Punch { .. } => end.saturating_sub(first),
            RangeRewrite::Collapse { .. } | RangeRewrite::Insert { .. } => {
                end.checked_sub(first).ok_or(XfsError::AddressOutOfRange)?
            }
        };
        let mut rewritten = Vec::new();
        let mut released = Vec::<XfsExtent>::new();
        let mut moved = Vec::<(u64, u64, u64)>::new();
        for extent in old.iter().copied() {
            for index in 0..u64::from(extent.block_count) {
                let old_file = extent
                    .file_block
                    .checked_add(index)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let physical = extent
                    .start_block
                    .checked_add(index)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let next_file = match operation {
                    RangeRewrite::Punch { .. } if old_file >= first && old_file < end => {
                        released.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                        released.push(XfsExtent {
                            unwritten: extent.unwritten,
                            file_block: old_file,
                            start_block: physical,
                            block_count: 1,
                        });
                        continue;
                    }
                    RangeRewrite::Punch { .. } => old_file,
                    RangeRewrite::Collapse { .. } if old_file >= first && old_file < end => {
                        released.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                        released.push(XfsExtent {
                            unwritten: extent.unwritten,
                            file_block: old_file,
                            start_block: physical,
                            block_count: 1,
                        });
                        continue;
                    }
                    RangeRewrite::Collapse { .. } if old_file >= end => old_file
                        .checked_sub(delta)
                        .ok_or(XfsError::AddressOutOfRange)?,
                    RangeRewrite::Collapse { .. } => old_file,
                    RangeRewrite::Insert { .. } if old_file >= first => old_file
                        .checked_add(delta)
                        .ok_or(XfsError::AddressOutOfRange)?,
                    RangeRewrite::Insert { .. } => old_file,
                };
                if next_file != old_file {
                    moved.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    moved.push((physical, old_file, next_file));
                }
                rewritten.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                push_merged_extent(
                    &mut rewritten,
                    XfsExtent {
                        unwritten: extent.unwritten,
                        file_block: next_file,
                        start_block: physical,
                        block_count: 1,
                    },
                )?;
            }
        }
        let new_size = match operation {
            RangeRewrite::Punch { .. } => inode.size,
            RangeRewrite::Collapse { .. } => inode
                .size
                .checked_sub(
                    delta
                        .checked_mul(block_size)
                        .ok_or(XfsError::AddressOutOfRange)?,
                )
                .ok_or(XfsError::AddressOutOfRange)?,
            RangeRewrite::Insert { .. } => inode
                .size
                .checked_add(
                    delta
                        .checked_mul(block_size)
                        .ok_or(XfsError::AddressOutOfRange)?,
                )
                .ok_or(XfsError::AddressOutOfRange)?,
        };
        let old_bmap = if inode.data_format == XfsForkFormat::Btree {
            self.volume.inode_bmbt_blocks(inode_number)?
        } else {
            Vec::new()
        };
        let (_, raw_inode) = self.volume.inode_and_bytes(inode_number)?;
        let required = bmap_external_blocks(
            self.volume.superblock,
            inode.data_fork(&raw_inode)?.len(),
            rewritten.len(),
        )?;
        let reused = required.min(old_bmap.len());
        let inode_ag = self.volume.split_inode_number(inode_number)?.0;
        let new_bmap = self.volume.reserve_bmap_metadata_blocks(
            inode_ag,
            required.saturating_sub(reused),
            &[],
        )?;
        let mut bmap = Vec::new();
        bmap.try_reserve_exact(
            reused
                .checked_add(new_bmap.len())
                .ok_or(XfsError::AddressOutOfRange)?,
        )
        .map_err(|_| XfsError::NoMemory)?;
        bmap.extend_from_slice(&old_bmap[..reused]);
        bmap.extend_from_slice(&new_bmap);
        let mut metadata = XfsMetadataTransaction::default();
        let mut boundary_data = Vec::<XfsStagedDataWrite>::new();
        if self.volume.superblock.features.has_rmapbt()
            && self.volume.superblock.features.has_reflink()
        {
            let mut planners = Vec::<XfsAgMutationPlanner>::new();
            for extent in &released {
                let planner = xfs_planner_for(&self.volume, &mut planners, extent.start_block)?;
                let local =
                    u32::try_from(extent.start_block % u64::from(self.volume.superblock.ag_blocks))
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                let count = planner.refcount_at(local)?;
                planner.remove_owner(local, inode_number, extent.file_block)?;
                if count > 1 {
                    planner.set_refcount(local, count - 1)?;
                } else {
                    planner.release_free_block(local)?;
                }
            }
            for (physical, old_file, new_file) in &moved {
                let planner = xfs_planner_for(&self.volume, &mut planners, *physical)?;
                let local = u32::try_from(*physical % u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                planner.remove_owner(local, inode_number, *old_file)?;
                planner.add_owner(local, inode_number, *new_file)?;
            }
            if let RangeRewrite::Punch { offset, length } = operation {
                let end = offset
                    .checked_add(length)
                    .ok_or(XfsError::AddressOutOfRange)?;
                self.stage_punch_boundary_data(
                    inode_number,
                    &old,
                    &mut rewritten,
                    offset,
                    end,
                    &mut planners,
                    &mut boundary_data,
                )?;
            }
            for physical in &new_bmap {
                let planner = xfs_planner_for(&self.volume, &mut planners, *physical)?;
                let local = u32::try_from(*physical % u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                planner.claim_specific_free_block(local)?;
                planner.add_owner(local, inode_number, XFS_RMAP_OFF_BMBT)?;
            }
            for physical in old_bmap.iter().copied().skip(reused) {
                let planner = xfs_planner_for(&self.volume, &mut planners, physical)?;
                let local = u32::try_from(physical % u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                planner.remove_owner(local, inode_number, XFS_RMAP_OFF_BMBT)?;
                planner.release_free_block(local)?;
            }
            for planner in &planners {
                let staged = self.volume.stage_reflink_ag_plan(planner)?;
                metadata
                    .buffers
                    .try_reserve(staged.buffers.len())
                    .map_err(|_| XfsError::NoMemory)?;
                metadata.buffers.extend(staged.buffers);
            }
        } else {
            let mut groups = Vec::<(u32, Vec<(u32, u32)>, Vec<u32>)>::new();
            for physical in &new_bmap {
                let ag = u32::try_from(*physical / u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                let local = u32::try_from(*physical % u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                if let Some((_, allocated, _)) = groups.iter_mut().find(|(item, ..)| *item == ag) {
                    allocated.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    allocated.push((local, 1));
                } else {
                    let mut allocated = Vec::new();
                    allocated.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    allocated.push((local, 1));
                    groups.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    groups.push((ag, allocated, Vec::new()));
                }
            }
            for extent in &released {
                let ag =
                    u32::try_from(extent.start_block / u64::from(self.volume.superblock.ag_blocks))
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                let local =
                    u32::try_from(extent.start_block % u64::from(self.volume.superblock.ag_blocks))
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                if let Some((_, _, free)) = groups.iter_mut().find(|(item, ..)| *item == ag) {
                    free.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    free.push(local);
                } else {
                    let mut free = Vec::new();
                    free.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    free.push(local);
                    groups.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    groups.push((ag, Vec::new(), free));
                }
            }
            for physical in old_bmap.iter().copied().skip(reused) {
                let ag = u32::try_from(physical / u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                let local = u32::try_from(physical % u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                if let Some((_, _, free)) = groups.iter_mut().find(|(item, ..)| *item == ag) {
                    free.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    free.push(local);
                } else {
                    let mut free = Vec::new();
                    free.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    free.push(local);
                    groups.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    groups.push((ag, Vec::new(), free));
                }
            }
            for (ag, allocated, free) in groups {
                let staged = self.volume.stage_extent_delta(ag, &allocated, &free)?;
                metadata
                    .buffers
                    .try_reserve(staged.buffers.len())
                    .map_err(|_| XfsError::NoMemory)?;
                metadata.buffers.extend(staged.buffers);
            }
            if let RangeRewrite::Punch { offset, length } = operation {
                let end = offset
                    .checked_add(length)
                    .ok_or(XfsError::AddressOutOfRange)?;
                self.stage_punch_boundary_data_unshared(&old, offset, end, &mut boundary_data)?;
            }
        }
        if required == 0 {
            self.volume.stage_regular_inode_extents(
                inode_number,
                rewritten,
                new_size,
                &mut metadata,
            )?;
        } else {
            self.volume.stage_regular_inode_bmap(
                inode_number,
                rewritten,
                new_size,
                &bmap,
                &mut metadata,
            )?;
        }
        metadata.data_writes = boundary_data;
        let quota = self.staged_inode_block_delta(inode_number, &metadata)?;
        self.stage_inode_quota_delta(&inode, quota, 0, &mut metadata)?;
        self.commit_locked(&mut live, &metadata)
    }

    pub(super) fn punch_boundary_blocks(&self, offset: u64, end: u64) -> XfsResult<Vec<u64>> {
        let block = u64::from(self.volume.superblock.block_size);
        let mut blocks = Vec::new();
        if offset % block != 0 {
            blocks.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            blocks.push(offset / block);
        }
        if end % block != 0 {
            let last = end.checked_sub(1).ok_or(XfsError::AddressOutOfRange)? / block;
            if !blocks.contains(&last) {
                blocks.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                blocks.push(last);
            }
        }
        Ok(blocks)
    }

    pub(super) fn zero_punch_image(
        &self,
        image: &mut [u8],
        file_block: u64,
        offset: u64,
        end: u64,
    ) -> XfsResult<()> {
        let block = u64::from(self.volume.superblock.block_size);
        let start = file_block
            .checked_mul(block)
            .ok_or(XfsError::AddressOutOfRange)?;
        let finish = start
            .checked_add(block)
            .ok_or(XfsError::AddressOutOfRange)?;
        let begin = offset.max(start);
        let limit = end.min(finish);
        if begin >= limit {
            return Err(XfsError::CorruptMetadata);
        }
        let from = usize::try_from(begin - start).map_err(|_| XfsError::AddressOutOfRange)?;
        let to = usize::try_from(limit - start).map_err(|_| XfsError::AddressOutOfRange)?;
        image
            .get_mut(from..to)
            .ok_or(XfsError::CorruptMetadata)?
            .fill(0);
        Ok(())
    }

    /// Produces boundary RMW images while the reflink planner still owns the
    /// old/new physical homes.  A shared boundary is copied before its
    /// mapping is published, exactly like a normal COW write.
    pub(super) fn stage_punch_boundary_data(
        &self,
        inode: u64,
        old: &[XfsExtent],
        rewritten: &mut Vec<XfsExtent>,
        offset: u64,
        end: u64,
        planners: &mut Vec<XfsAgMutationPlanner>,
        output: &mut Vec<XfsStagedDataWrite>,
    ) -> XfsResult<()> {
        for file_block in self.punch_boundary_blocks(offset, end)? {
            let Some(mapping) = xfs_extent_at(old, file_block)? else {
                continue;
            };
            let old_physical = mapping.start_block;
            let old_local =
                u32::try_from(old_physical % u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
            let shared =
                xfs_planner_for(&self.volume, planners, old_physical)?.refcount_at(old_local)? > 1;
            let physical = if shared {
                {
                    let planner = xfs_planner_for(&self.volume, planners, old_physical)?;
                    planner.remove_owner(old_local, inode, file_block)?;
                    let count = planner.refcount_at(old_local)?;
                    planner.set_refcount(
                        old_local,
                        count.checked_sub(1).ok_or(XfsError::CorruptMetadata)?,
                    )?;
                }
                let (ag, local) = {
                    let planner = xfs_planner_for(&self.volume, planners, old_physical)?;
                    (planner.ag, planner.claim_free_block()?)
                };
                let new_physical = u64::from(ag)
                    .checked_mul(u64::from(self.volume.superblock.ag_blocks))
                    .and_then(|base| base.checked_add(u64::from(local)))
                    .ok_or(XfsError::AddressOutOfRange)?;
                xfs_planner_for(&self.volume, planners, new_physical)?
                    .add_owner(local, inode, file_block)?;
                xfs_replace_one_mapping(
                    rewritten,
                    file_block,
                    Some(XfsExtent {
                        unwritten: false,
                        file_block,
                        start_block: new_physical,
                        block_count: 1,
                    }),
                )?;
                new_physical
            } else {
                old_physical
            };
            let before = self.volume.read_data_fs_block(physical)?;
            let mut after = if shared {
                self.volume.read_data_fs_block(old_physical)?
            } else {
                before.clone()
            };
            self.zero_punch_image(&mut after, file_block, offset, end)?;
            output.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            output.push(XfsStagedDataWrite {
                fs_block: physical,
                before,
                after,
            });
        }
        Ok(())
    }

    pub(super) fn stage_punch_boundary_data_unshared(
        &self,
        old: &[XfsExtent],
        offset: u64,
        end: u64,
        output: &mut Vec<XfsStagedDataWrite>,
    ) -> XfsResult<()> {
        for file_block in self.punch_boundary_blocks(offset, end)? {
            let Some(mapping) = xfs_extent_at(old, file_block)? else {
                continue;
            };
            let before = self.volume.read_data_fs_block(mapping.start_block)?;
            let mut after = before.clone();
            self.zero_punch_image(&mut after, file_block, offset, end)?;
            output.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            output.push(XfsStagedDataWrite {
                fs_block: mapping.start_block,
                before,
                after,
            });
        }
        Ok(())
    }

    pub(super) fn reflink_range(
        &self,
        source: u64,
        source_offset: u64,
        destination: u64,
        destination_offset: u64,
        length: u64,
        dedupe: bool,
        seconds: i64,
        nanoseconds: u32,
    ) -> XfsResult<bool> {
        let block_size = u64::from(self.volume.superblock.block_size);
        if length == 0
            || source_offset % block_size != 0
            || destination_offset % block_size != 0
            || length % block_size != 0
        {
            return Err(XfsError::AddressOutOfRange);
        }
        if !self.volume.superblock.features.has_rmapbt()
            || !self.volume.superblock.features.has_reflink()
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let mut live = self.live.lock();
        if live.failed {
            return Err(XfsError::Io);
        }
        let source_inode = self.volume.inode(source)?;
        let destination_inode = self.volume.inode(destination)?;
        if source_inode.mode & 0o170000 != 0o100000 || destination_inode.mode & 0o170000 != 0o100000
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let source_end = source_offset
            .checked_add(length)
            .ok_or(XfsError::AddressOutOfRange)?;
        let destination_end = destination_offset
            .checked_add(length)
            .ok_or(XfsError::AddressOutOfRange)?;
        if source_end > source_inode.size {
            return Err(XfsError::AddressOutOfRange);
        }
        // Linux rejects overlapping self-reflinks because the source mapping
        // must remain an immutable snapshot for the entire transaction.
        if source == destination
            && source_offset < destination_end
            && destination_offset < source_end
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let source_extents = match source_inode.data_format {
            XfsForkFormat::Extents => self.volume.inode_data_extents(source)?,
            XfsForkFormat::Btree => self.volume.inode_bmbt_extents(source)?,
            _ => return Err(XfsError::UnsupportedFeature),
        };
        let mut destination_extents = match destination_inode.data_format {
            XfsForkFormat::Extents => self.volume.inode_data_extents(destination)?,
            XfsForkFormat::Btree => self.volume.inode_bmbt_extents(destination)?,
            _ => return Err(XfsError::UnsupportedFeature),
        };
        let blocks = length / block_size;
        if dedupe {
            let mut left =
                vec![0u8; usize::try_from(block_size).map_err(|_| XfsError::AddressOutOfRange)?];
            let mut right = left.clone();
            for block in 0..blocks {
                self.volume
                    .read_inode_at(source, source_offset + block * block_size, &mut left)?;
                self.volume.read_inode_at(
                    destination,
                    destination_offset + block * block_size,
                    &mut right,
                )?;
                if left != right {
                    return Ok(false);
                }
            }
        }
        let mut planners = Vec::<XfsAgMutationPlanner>::new();
        let source_block = source_offset / block_size;
        let destination_block = destination_offset / block_size;
        for relative in 0..blocks {
            let source_mapping = xfs_extent_at(&source_extents, source_block + relative)?;
            let old_destination =
                xfs_extent_at(&destination_extents, destination_block + relative)?;
            if let Some(old) = old_destination {
                let old_planner = xfs_planner_for(&self.volume, &mut planners, old.start_block)?;
                let local =
                    u32::try_from(old.start_block % u64::from(self.volume.superblock.ag_blocks))
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                old_planner.remove_owner(local, destination, destination_block + relative)?;
                let count = old_planner.refcount_at(local)?;
                if count == 1 {
                    old_planner.release_free_block(local)?;
                } else {
                    old_planner.set_refcount(local, count - 1)?;
                }
            }
            if let Some(source_mapping) = source_mapping {
                if source_mapping.unwritten {
                    return Err(XfsError::UnsupportedFeature);
                }
                let source_planner =
                    xfs_planner_for(&self.volume, &mut planners, source_mapping.start_block)?;
                let local = u32::try_from(
                    source_mapping.start_block % u64::from(self.volume.superblock.ag_blocks),
                )
                .map_err(|_| XfsError::AddressOutOfRange)?;
                let count = source_planner.refcount_at(local)?;
                source_planner.set_refcount(
                    local,
                    count.checked_add(1).ok_or(XfsError::AddressOutOfRange)?,
                )?;
                source_planner.add_owner(local, destination, destination_block + relative)?;
            }
            xfs_replace_one_mapping(
                &mut destination_extents,
                destination_block + relative,
                source_mapping,
            )?;
        }
        let new_size = destination_inode.size.max(destination_end);
        // Rebuild the external bmap tree from the final mapping.  New node
        // homes are claimed from the same AG snapshots and receive BMBT rmap
        // ownership before the AG roots are staged, so root growth cannot
        // leak an unowned metadata block across a crash.
        let (raw_destination, destination_bytes) = self.volume.inode_and_bytes(destination)?;
        let fork_bytes = raw_destination.data_fork(&destination_bytes)?.len();
        let required_bmap = bmap_external_blocks(
            self.volume.superblock,
            fork_bytes,
            destination_extents.len(),
        )?;
        let old_bmap = if destination_inode.data_format == XfsForkFormat::Btree {
            self.volume.inode_bmbt_blocks(destination)?
        } else {
            Vec::new()
        };
        let destination_ag = self.volume.split_inode_number(destination)?.0;
        let reused_bmap = required_bmap.min(old_bmap.len());
        let mut bmap_blocks = Vec::new();
        bmap_blocks
            .try_reserve_exact(required_bmap)
            .map_err(|_| XfsError::NoMemory)?;
        bmap_blocks.extend_from_slice(&old_bmap[..reused_bmap]);
        while bmap_blocks.len() < required_bmap {
            let mut selected = None;
            for step in 0..self.volume.superblock.ag_count {
                let ag = u32::try_from(
                    (u64::from(destination_ag) + u64::from(step))
                        % u64::from(self.volume.superblock.ag_count),
                )
                .map_err(|_| XfsError::AddressOutOfRange)?;
                let physical = u64::from(ag)
                    .checked_mul(u64::from(self.volume.superblock.ag_blocks))
                    .ok_or(XfsError::AddressOutOfRange)?;
                let candidate = xfs_planner_for(&self.volume, &mut planners, physical)?;
                if let Ok(local) = candidate.claim_free_block() {
                    selected = Some((ag, local));
                    break;
                }
            }
            let (ag, local) = selected.ok_or(XfsError::AddressOutOfRange)?;
            let candidate = xfs_planner_for(
                &self.volume,
                &mut planners,
                u64::from(ag) * u64::from(self.volume.superblock.ag_blocks),
            )?;
            candidate.add_owner(local, destination, XFS_RMAP_OFF_BMBT)?;
            bmap_blocks.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            bmap_blocks.push(
                u64::from(ag)
                    .checked_mul(u64::from(self.volume.superblock.ag_blocks))
                    .and_then(|base| base.checked_add(u64::from(local)))
                    .ok_or(XfsError::AddressOutOfRange)?,
            );
        }
        for physical in old_bmap.iter().copied().skip(required_bmap) {
            let candidate = xfs_planner_for(&self.volume, &mut planners, physical)?;
            let local = u32::try_from(physical % u64::from(self.volume.superblock.ag_blocks))
                .map_err(|_| XfsError::AddressOutOfRange)?;
            candidate.remove_owner(local, destination, XFS_RMAP_OFF_BMBT)?;
            candidate.release_free_block(local)?;
        }
        let mut metadata = XfsMetadataTransaction::default();
        for planner in &planners {
            let staged = self.volume.stage_reflink_ag_plan(planner)?;
            metadata
                .buffers
                .try_reserve(staged.buffers.len())
                .map_err(|_| XfsError::NoMemory)?;
            metadata.buffers.extend(staged.buffers);
        }
        match destination_inode.data_format {
            XfsForkFormat::Extents if required_bmap == 0 => {
                self.volume.stage_regular_inode_extents(
                    destination,
                    destination_extents,
                    new_size,
                    &mut metadata,
                )?
            }
            XfsForkFormat::Extents | XfsForkFormat::Btree => self.volume.stage_regular_inode_bmap(
                destination,
                destination_extents,
                new_size,
                &bmap_blocks,
                &mut metadata,
            )?,
            _ => return Err(XfsError::UnsupportedFeature),
        }
        // The final `di_nblocks` image includes both the replaced data
        // mappings and any BMBT growth/shrink.  Charge its exact basic-block
        // delta rather than attempting a per-data-block approximation above.
        let quota_delta = self.staged_inode_block_delta(destination, &metadata)?;
        self.stage_inode_quota_delta(&destination_inode, quota_delta, 0, &mut metadata)?;
        self.volume.stage_inode_core_update(
            destination,
            XfsInodeCoreUpdate {
                mtime: Some((seconds, nanoseconds)),
                ctime: Some((seconds, nanoseconds)),
                ..XfsInodeCoreUpdate::default()
            },
            &mut metadata,
        )?;
        self.commit_locked(&mut live, &metadata)?;
        Ok(true)
    }

    pub fn clone_range(
        &self,
        source: u64,
        source_offset: u64,
        destination: u64,
        destination_offset: u64,
        length: u64,
        seconds: i64,
        nanoseconds: u32,
    ) -> XfsResult<()> {
        self.reflink_range(
            source,
            source_offset,
            destination,
            destination_offset,
            length,
            false,
            seconds,
            nanoseconds,
        )
        .map(|_| ())
    }

    pub fn dedupe_range(
        &self,
        source: u64,
        source_offset: u64,
        destination: u64,
        destination_offset: u64,
        length: u64,
        seconds: i64,
        nanoseconds: u32,
    ) -> XfsResult<bool> {
        self.reflink_range(
            source,
            source_offset,
            destination,
            destination_offset,
            length,
            true,
            seconds,
            nanoseconds,
        )
    }

    /// Commits a local attribute-fork replacement under the same live-log
    /// serialization as data-fork mutations.  The caller performs any
    /// namespace policy before this method; existence and replacement state
    /// are sampled only while this lock is held.
    pub fn replace_shortform_xattrs(
        &self,
        inode: u64,
        attrs: &[XfsShortformXattr],
    ) -> XfsResult<()> {
        let mut live = self.live.lock();
        let mut metadata = XfsMetadataTransaction::default();
        self.volume
            .stage_shortform_xattrs(inode, attrs, &mut metadata)?;
        self.commit_locked(&mut live, &metadata)
    }

    pub fn mutate_shortform_xattr(
        &self,
        inode: u64,
        flags: u8,
        name: &[u8],
        value: Option<&[u8]>,
        mode: XfsShortformXattrMode,
    ) -> XfsResult<XfsShortformXattrOutcome> {
        if name.is_empty() || name.iter().any(|byte| *byte == 0) {
            return Err(XfsError::AddressOutOfRange);
        }
        if flags & XFS_ATTR_LOCAL == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let mut live = self.live.lock();
        let mut attrs = self.volume.shortform_xattrs(inode)?;
        let at = attrs
            .iter()
            .position(|attribute| attribute.flags == flags && attribute.name == name);
        match (value, mode, at) {
            (None, _, None) => return Ok(XfsShortformXattrOutcome::Missing),
            (None, _, Some(index)) => {
                attrs.remove(index);
            }
            (
                Some(_),
                XfsShortformXattrMode::Create | XfsShortformXattrMode::CreateAndReplace,
                Some(_),
            ) => return Ok(XfsShortformXattrOutcome::Exists),
            (
                Some(_),
                XfsShortformXattrMode::Replace | XfsShortformXattrMode::CreateAndReplace,
                None,
            ) => return Ok(XfsShortformXattrOutcome::Missing),
            (Some(data), _, Some(index)) => attrs[index].value = data.to_vec(),
            (Some(data), _, None) => attrs.push(XfsShortformXattr {
                flags,
                name: name.to_vec(),
                value: data.to_vec(),
            }),
        }
        let mut metadata = XfsMetadataTransaction::default();
        self.volume
            .stage_shortform_xattrs(inode, &attrs, &mut metadata)?;
        self.commit_locked(&mut live, &metadata)?;
        Ok(XfsShortformXattrOutcome::Applied)
    }

    pub fn mutate_xattr(
        &self,
        inode: u64,
        flags: u8,
        name: &[u8],
        value: Option<&[u8]>,
        mode: XfsShortformXattrMode,
    ) -> XfsResult<XfsShortformXattrOutcome> {
        // LOCAL is a leaf storage bit.  Namespace callers always use the
        // normalized identity so moving a value to remote blocks does not
        // make it disappear from getxattr/listxattr.
        if name.is_empty()
            || name.iter().any(|byte| *byte == 0)
            || flags & !(XFS_ATTR_LOCAL | XFS_ATTR_ROOT | XFS_ATTR_SECURE) != 0
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let flags = flags | XFS_ATTR_LOCAL;
        let mut live = self.live.lock();
        let mut attrs = self.volume.xattrs(inode)?;
        let at = attrs
            .iter()
            .position(|attribute| attribute.flags == flags && attribute.name == name);
        match (value, mode, at) {
            (None, _, None) => return Ok(XfsShortformXattrOutcome::Missing),
            (None, _, Some(index)) => {
                attrs.remove(index);
            }
            (
                Some(_),
                XfsShortformXattrMode::Create | XfsShortformXattrMode::CreateAndReplace,
                Some(_),
            ) => return Ok(XfsShortformXattrOutcome::Exists),
            (
                Some(_),
                XfsShortformXattrMode::Replace | XfsShortformXattrMode::CreateAndReplace,
                None,
            ) => return Ok(XfsShortformXattrOutcome::Missing),
            (Some(data), _, Some(index)) => attrs[index].value = data.to_vec(),
            (Some(data), _, None) => attrs.push(XfsShortformXattr {
                flags,
                name: name.to_vec(),
                value: data.to_vec(),
            }),
        }
        let mut metadata = XfsMetadataTransaction::default();
        if self.volume.inode(inode)?.attr_format == XfsForkFormat::Local {
            match self
                .volume
                .stage_shortform_xattrs(inode, &attrs, &mut metadata)
            {
                Ok(()) => {}
                Err(XfsError::AddressOutOfRange) => {
                    self.volume
                        .stage_attribute_values(inode, &attrs, &mut metadata)?
                }
                Err(error) => return Err(error),
            }
        } else {
            self.volume
                .stage_attribute_values(inode, &attrs, &mut metadata)?;
        }
        self.commit_locked(&mut live, &metadata)?;
        Ok(XfsShortformXattrOutcome::Applied)
    }

    /// Applies a raw-name directory mutation to shortform or one-block
    /// dir2/dir3 storage.  Promotion reserves data blocks and updates the AG,
    /// inode and directory image under this one live-log lock; duplicate or
    /// missing names therefore never leak a partly committed namespace.
    pub fn mutate_directory(
        &self,
        directory: u64,
        mutation: XfsDirectoryMutation,
    ) -> XfsResult<()> {
        let mut live = self.live.lock();
        let mut entries = self.volume.directory_entries(directory)?;
        let name = match &mutation {
            XfsDirectoryMutation::Insert(entry) => entry.name.clone(),
            XfsDirectoryMutation::Remove(name) => name.clone(),
            XfsDirectoryMutation::Replace { name, .. } => name.clone(),
        };
        if name.is_empty()
            || name == b"."
            || name == b".."
            || name.iter().any(|byte| *byte == 0 || *byte == b'/')
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let at = entries.iter().position(|entry| entry.name == name);
        match mutation {
            XfsDirectoryMutation::Insert(entry) => {
                if at.is_some() {
                    return Err(XfsError::AddressOutOfRange);
                }
                if entry.inode == 0 {
                    return Err(XfsError::CorruptMetadata);
                }
                entries.push(entry);
            }
            XfsDirectoryMutation::Remove(_) => {
                entries.remove(at.ok_or(XfsError::AddressOutOfRange)?);
            }
            XfsDirectoryMutation::Replace { entry, .. } => {
                let index = at.ok_or(XfsError::AddressOutOfRange)?;
                if entry.inode == 0 || entry.name != entries[index].name {
                    return Err(XfsError::CorruptMetadata);
                }
                entries[index] = entry;
            }
        }
        let parent = self.volume.directory_parent(directory)?;
        let mut metadata = XfsMetadataTransaction::default();
        self.volume.stage_directory_entries_with_parent(
            directory,
            parent,
            &entries,
            &mut metadata,
        )?;
        self.commit_locked(&mut live, &metadata)
    }
}
