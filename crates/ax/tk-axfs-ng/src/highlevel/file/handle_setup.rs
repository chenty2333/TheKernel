//! File: owned I/O admission, attributes, construction and fadvise.

use super::*;

impl File {
    /// Prepares an owned file-I/O request for this exact open description.
    ///
    /// The request's opcode and positioned offset are already immutable VFS
    /// geometry.  The kernel selects placement and broader LSM/DAC policy,
    /// while this OFD boundary repeats open-mode and persistent native
    /// immutable/append admission before a queue, cache, or provider can
    /// reserve observable state.
    pub fn prepare_owned_file_io(
        &self,
        request: FileIoRequest,
        completion: Box<dyn OwnedFileIoCompletion>,
    ) -> Result<PreparedOwnedFileIo, FileIoPrepareError> {
        let access = match request.opcode() {
            FileIoOpcode::Read => self.access(FileFlags::READ),
            FileIoOpcode::Write => self.access(FileFlags::WRITE),
        };
        if let Err(error) = access {
            return Err(FileIoPrepareError::new(error, request, completion));
        }
        // A zero-length operation is terminal before persistent-attribute,
        // provider, cache, or range-lease admission.
        if request.len() == 0 {
            let submission: Box<dyn PreparedFileIoSubmission> =
                match Box::try_new(ZeroLengthOwnedIoSubmission) {
                    Ok(submission) => submission,
                    Err(_) => {
                        return Err(FileIoPrepareError::new(
                            VfsError::NoMemory,
                            request,
                            completion,
                        ));
                    }
                };
            return Ok(PreparedOwnedFileIo::new(PreparedFileIo::new(
                request, completion, submission,
            )));
        }
        // Direct providers currently have no retained try-only NOWAIT permit.
        // Do this before native attributes, provider preparation, or cache
        // coherency can enter a blocking domain.
        if request.policy().nowait && matches!(&self.inner, FileBackend::Direct(_)) {
            return prepare_nowait_unavailable_owned_file_io(request, completion);
        }
        if request.policy().nowait
            && !nowait_terminal_plan_supported(
                request.opcode(),
                request.policy(),
                !self.flags.contains(FileFlags::NOATIME),
            )
        {
            return prepare_nowait_unavailable_owned_file_io(request, completion);
        }
        if request.opcode() == FileIoOpcode::Write {
            let append = request.placement() == FileIoWritePlacement::Append;
            let admission = if request.policy().nowait {
                // Early try-only rejection; the actual cached mutation takes
                // a fresh retained guard again below.
                try_begin_native_location_mutation_nowait(self.location(), append).map(|_| ())
            } else {
                match request.placement() {
                    FileIoWritePlacement::Positioned => {
                        self.admit_native_mutation(Some(request.offset()), false)
                    }
                    FileIoWritePlacement::Append => self.admit_native_mutation(None, true),
                }
            };
            if let Err(error) = admission {
                return Err(FileIoPrepareError::new(error, request, completion));
            }
        }
        match &self.inner {
            FileBackend::Cached(cache) => {
                let completion: Box<dyn OwnedFileIoCompletion> =
                    match self.try_prepare_owned_file_io_time_completion(&request, completion) {
                        Ok(completion) => completion,
                        Err(completion) => {
                            return Err(FileIoPrepareError::new(
                                VfsError::NoMemory,
                                request,
                                completion,
                            ));
                        }
                    };
                match CachedOwnedIoSubmission::prepare(
                    cache.clone(),
                    request,
                    completion,
                    !self.flags.contains(FileFlags::NOATIME),
                ) {
                    Ok(prepared) => Ok(PreparedOwnedFileIo::new(prepared)),
                    Err(FileIoPrepareError {
                        error,
                        request,
                        completion,
                    }) => Err(FileIoPrepareError::new(
                        error,
                        request,
                        completion.into_retry_completion(),
                    )),
                }
            }
            FileBackend::Direct(location) => {
                self.prepare_direct_owned_file_io(location, request, completion)
            }
        }
    }

    /// Allocate the metadata/sync wrapper before ownership moves into a
    /// prepared submission.  Allocation failure leaves the exact original
    /// completion sink available to the caller.
    pub(super) fn try_prepare_owned_file_io_time_completion(
        &self,
        request: &FileIoRequest,
        completion: Box<dyn OwnedFileIoCompletion>,
    ) -> Result<Box<OwnedFileIoTimeCompletion>, Box<dyn OwnedFileIoCompletion>> {
        match Box::try_new(OwnedFileIoTimeCompletion {
            completion: None,
            location: self.location().clone(),
            backend: self.inner.clone(),
            open_handle: self.open_handle.clone(),
            opcode: request.opcode(),
            policy: request.policy(),
            update_atime: !self.flags.contains(FileFlags::NOATIME),
        }) {
            Ok(mut time_completion) => {
                time_completion.completion = Some(completion);
                Ok(time_completion)
            }
            Err(_) => Err(completion),
        }
    }

    pub(super) fn prepare_direct_owned_file_io(
        &self,
        location: &Location,
        request: FileIoRequest,
        completion: Box<dyn OwnedFileIoCompletion>,
    ) -> Result<PreparedOwnedFileIo, FileIoPrepareError> {
        let opcode = request.opcode();
        let request_placement = request.placement();
        let offset = request.offset();
        let length = request.len();
        // Keep the location-owned node alive across provider preparation; a
        // `Location::entry()` temporary cannot lend its FileNode to an async
        // reservation call.
        let inode = if self.open_handle.is_none() {
            match location.entry().as_file() {
                Ok(inode) => Some(inode),
                Err(error) => return Err(FileIoPrepareError::new(error, request, completion)),
            }
        } else {
            None
        };
        let coherency = match Arc::try_new(SleepingMutex::new(None)) {
            Ok(coherency) => coherency,
            Err(_) => {
                return Err(FileIoPrepareError::new(
                    VfsError::NoMemory,
                    request,
                    completion,
                ));
            }
        };
        let time_completion =
            match self.try_prepare_owned_file_io_time_completion(&request, completion) {
                Ok(completion) => completion,
                Err(completion) => {
                    return Err(FileIoPrepareError::new(
                        VfsError::NoMemory,
                        request,
                        completion,
                    ));
                }
            };
        let routed_completion: Box<dyn OwnedFileIoCompletion> =
            match Box::try_new(DirectOwnedIoCompletion {
                time_completion: None,
                coherency: coherency.clone(),
            }) {
                Ok(mut routed_completion) => {
                    routed_completion.time_completion = Some(time_completion);
                    routed_completion
                }
                Err(_) => {
                    return Err(FileIoPrepareError::new(
                        VfsError::NoMemory,
                        request,
                        time_completion.into_original_completion(),
                    ));
                }
            };
        let prepared = match (self.open_handle.as_deref(), inode.as_ref()) {
            (Some(handle), _) => handle.prepare_file_io(request, routed_completion),
            (None, Some(inode)) => inode.prepare_file_io(request, routed_completion),
            (None, None) => unreachable!("direct file I/O missing both handle and inode"),
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(FileIoPrepareError {
                error,
                request,
                completion,
            }) => {
                return Err(FileIoPrepareError::new(
                    error,
                    request,
                    completion.into_retry_completion(),
                ));
            }
        };

        // Queue reservation is now held but unpublished.  Stage and commit
        // the exact cache range only at this point; a provider that declines
        // preparation has not forced a needless writeback/invalidation.
        let direct_coherency = match prepare_direct_owned_io_coherency(
            location,
            opcode,
            request_placement,
            offset,
            length,
        ) {
            Ok(coherency) => coherency,
            Err(error) => {
                let (request, completion) = prepared.abort();
                return Err(FileIoPrepareError::new(
                    error,
                    request,
                    completion.into_retry_completion(),
                ));
            }
        };
        *coherency.lock() = Some(direct_coherency);
        Ok(PreparedOwnedFileIo::with_direct_coherency(
            prepared, coherency,
        ))
    }

    /// Returns the explicit provider declaration required before Linux
    /// RWF_NOWAIT reaches cache residency or a lower I/O queue.  A regular
    /// inode is not enough: pseudo-files default to unsupported unless their
    /// concrete node/open handle opts in.
    pub fn supports_nowait_read(&self) -> VfsResult<bool> {
        match self.open_handle() {
            Some(handle) => Ok(handle.supports_nowait_read()),
            None => Ok(self.location().entry().as_file()?.supports_nowait_read()),
        }
    }

    /// Write-side counterpart of [`Self::supports_nowait_read`].
    pub fn supports_nowait_write(&self) -> VfsResult<bool> {
        match self.open_handle() {
            Some(handle) => Ok(handle.supports_nowait_write()),
            None => Ok(self.location().entry().as_file()?.supports_nowait_write()),
        }
    }

    /// Typed high-level page-cache admission used by Linux RWF_NOWAIT.
    pub fn nowait_read_admit(&self, offset: u64, length: usize) -> VfsResult<bool> {
        self.access(FileFlags::READ)?;
        if !self.inner.nowait_range_resident(offset, length) {
            return match &self.inner {
                // A cached mapping must never escape to a lower provider on
                // a miss: doing so would turn RWF_NOWAIT into hidden I/O.
                FileBackend::Cached(_) => Ok(false),
                // Direct/uncached OFDs have no high-level residency proof;
                // consult the provider's conservative typed fallback.
                FileBackend::Direct(location) => Ok(matches!(
                    self.open_handle().map_or_else(
                        || location
                            .entry()
                            .as_file()?
                            .nowait_read_admit(offset, length),
                        |handle| handle.nowait_read_admit(offset, length),
                    )?,
                    NowaitAdmission::Ready
                )),
            };
        }
        // Stateful remote backends bind queue admission to the exact OFD.
        // Local cached files have no per-open lower queue and are admitted by
        // complete, stable range residency alone.
        match self.open_handle() {
            Some(handle) => Ok(matches!(
                handle.nowait_read_admit(offset, length)?,
                NowaitAdmission::Ready
            )),
            None => Ok(true),
        }
    }

    /// A write may issue NOWAIT only when every affected cache page is
    /// resident and stable; extension/allocation and cache misses must defer.
    pub fn nowait_write_admit(&self, offset: u64, length: usize) -> VfsResult<bool> {
        self.access(FileFlags::WRITE)?;
        let end = offset
            .checked_add(length as u64)
            .ok_or(VfsError::InvalidInput)?;
        if let FileBackend::Cached(cache) = &self.inner {
            // Reading inode metadata here could sleep behind filesystem or
            // remote coherency work.  Cached NOWAIT instead relies solely on
            // the EOF snapshot produced by a prior cache-owned operation.
            return Ok(cache.shared.nowait_write_within_known_len(end)
                && cache.nowait_range_resident(offset, length));
        }
        if !self.inner.nowait_range_resident(offset, length) {
            return match &self.inner {
                FileBackend::Cached(_) => Ok(false),
                FileBackend::Direct(location) => Ok(matches!(
                    self.open_handle().map_or_else(
                        || location
                            .entry()
                            .as_file()?
                            .nowait_write_admit(offset, length),
                        |handle| handle.nowait_write_admit(offset, length),
                    )?,
                    NowaitAdmission::Ready
                )),
            };
        }
        match self.open_handle() {
            Some(handle) => Ok(matches!(
                handle.nowait_write_admit(offset, length)?,
                NowaitAdmission::Ready
            )),
            None => Ok(true),
        }
    }
    /// Returns the filesystem-native inode attribute set for this open object.
    /// No VFS-side shadow state is manufactured when the provider is absent.
    pub fn file_attr(&self) -> VfsResult<FileAttr> {
        self.location().get_file_attr()
    }

    /// Persists filesystem-native inode attributes for this open object.
    pub fn set_file_attr(&self, attr: FileAttr) -> VfsResult<()> {
        self.location().set_file_attr(attr)
    }

    /// Executes a filesystem-native range mutation through this exact open
    /// description while keeping the page cache coherent with the provider.
    /// Stateful filesystems use the per-open handle; ordinary local files use
    /// the inode node retained by the location.
    pub fn mutate_range(&self, request: FileRangeRequest) -> VfsResult<RangeMutation> {
        let native_mutation = begin_native_location_mutation(self.location(), false)?;
        self.mutate_range_with_held_native_mutation(request, &native_mutation)
    }

    /// Executes a range mutation while the caller retains the stable native
    /// mutation guard returned by [`Self::begin_native_mutation`].  This is
    /// for compound kernel operations whose sync/metadata completion must be
    /// in the same exclusion window; it intentionally does not reacquire it.
    pub fn mutate_range_with_held_native_mutation(
        &self,
        request: FileRangeRequest,
        native_mutation: &Option<FileAttrMutationGuard>,
    ) -> VfsResult<RangeMutation> {
        self.access(FileFlags::WRITE)?;
        self.admit_native_mutation(Some(request.offset), false)?;
        let location = self.location().clone();
        let open_handle = self.open_handle.clone();
        with_cache_invalidating_file_operation_with_held_native(
            &location,
            held_native_writeback_gate(native_mutation),
            move |_, inode| match open_handle.as_deref() {
                Some(handle) => handle.mutate_range(request),
                None => inode.mutate_range(request),
            },
        )
    }

    /// Shares a source extent through the destination file's normal mutation
    /// admission and cache-invalidation transaction.  ioctl reflink callers
    /// must use this rather than dispatching straight to `FileNodeOps`.
    pub fn clone_range_from(
        &self,
        source: &Location,
        source_offset: u64,
        destination_offset: u64,
        length: u64,
    ) -> VfsResult<()> {
        self.access(FileFlags::WRITE)?;
        self.admit_native_mutation(Some(destination_offset), false)?;
        let location = self.location().clone();
        let open_handle = self.open_handle.clone();
        with_source_coherent_destination_invalidated(
            source,
            &location,
            move |source, destination| match open_handle.as_deref() {
                Some(handle) => handle.clone_range_from(
                    source.as_node_ops(),
                    source_offset,
                    destination_offset,
                    length,
                ),
                None => destination.clone_range_from(
                    source.as_node_ops(),
                    source_offset,
                    destination_offset,
                    length,
                ),
            },
        )
    }

    /// Dedupe counterpart of [`Self::clone_range_from`].  A false provider
    /// result has no mutation, while a successful shared extent invalidates
    /// the destination cache under the same stable inode admission gate.
    pub fn dedupe_range_from(
        &self,
        source: &Location,
        source_offset: u64,
        destination_offset: u64,
        length: u64,
    ) -> VfsResult<bool> {
        self.access(FileFlags::WRITE)?;
        self.admit_native_mutation(Some(destination_offset), false)?;
        let location = self.location().clone();
        let open_handle = self.open_handle.clone();
        with_source_coherent_destination_invalidated(
            source,
            &location,
            move |source, destination| match open_handle.as_deref() {
                Some(handle) => handle.dedupe_range_from(
                    source.as_node_ops(),
                    source_offset,
                    destination_offset,
                    length,
                ),
                None => destination.dedupe_range_from(
                    source.as_node_ops(),
                    source_offset,
                    destination_offset,
                    length,
                ),
            },
        )
    }

    /// Enforce persistent Linux file attributes before a cached write can
    /// mutate VFS-visible state. Providers without native file attributes keep
    /// their existing behavior.
    pub(super) fn admit_native_mutation(&self, offset: Option<u64>, append: bool) -> VfsResult<()> {
        const FS_XFLAG_IMMUTABLE: u64 = 0x0000_0008;
        const FS_XFLAG_APPEND: u64 = 0x0000_0010;
        let attr = match self.location().get_file_attr() {
            Ok(attr) => attr,
            Err(VfsError::OperationNotSupported) => return Ok(()),
            Err(error) => return Err(error),
        };
        if attr.xflags & FS_XFLAG_IMMUTABLE != 0 {
            return Err(VfsError::OperationNotPermitted);
        }
        if attr.xflags & FS_XFLAG_APPEND != 0 && !append {
            // A positioned write at a coincidentally observed EOF is still
            // not an append transaction.  Only the typed Append placement
            // may enter the inode append domain.
            let _ = offset;
            return Err(VfsError::OperationNotPermitted);
        }
        Ok(())
    }

    #[cfg(feature = "times")]
    pub(super) fn record_time_flags(&self, flags: u8) {
        self.access_flags.fetch_or(flags, Ordering::AcqRel);
    }

    #[cfg(feature = "times")]
    pub(super) fn flush_times(&self) {
        let flags = self.access_flags.swap(0, Ordering::AcqRel);
        if flags == 0 {
            return;
        }

        // `wall_time` is the Unix-epoch wall clock used for inode metadata;
        // convert its unsigned legacy representation into a VFS timestamp.
        let now: Timestamp = axhal::time::wall_time().into();
        let mut update = MetadataUpdate::default();
        if flags & 1 != 0 {
            update.atime = Some(now);
        }
        if flags & 2 != 0 {
            update.mtime = Some(now);
            update.ctime = Some(now);
        }
        if let Err(err) = self.inner.location().update_supported_metadata(update) {
            debug!("Failed to update file times: {err:?}");
            self.access_flags.fetch_or(flags, Ordering::AcqRel);
        }
    }

    /// Creates a new [`File`] from a [`FileBackend`] and access flags.
    pub fn new(inner: FileBackend, flags: FileFlags) -> Self {
        Self::new_with_open_handle(inner, flags, None)
    }

    /// Constructs an OFD with its optional provider-owned operation handle.
    /// Only `OpenOptions` normally supplies one; this public constructor keeps
    /// ordinary stateless files free of a second representation.
    pub fn new_with_open_handle(
        inner: FileBackend,
        flags: FileFlags,
        open_handle: Option<Arc<dyn FileNodeOps>>,
    ) -> Self {
        Self::new_with_open_handle_and_pending_open_truncate(inner, flags, open_handle, false)
    }

    pub(super) fn new_with_open_handle_and_pending_open_truncate(
        inner: FileBackend,
        flags: FileFlags,
        open_handle: Option<Arc<dyn FileNodeOps>>,
        pending_open_truncate: bool,
    ) -> Self {
        let position = if inner.location().flags().contains(NodeFlags::STREAM) {
            None
        } else {
            // O_APPEND changes where each write commits; it does not seek the
            // newly opened description to EOF. Clearing append before the
            // first write must therefore expose the initial offset zero.
            Some(Mutex::new(0))
        };
        Self {
            inner,
            open_handle,
            append: AtomicBool::new(flags.contains(FileFlags::APPEND)),
            pending_open_truncate: AtomicBool::new(pending_open_truncate),
            flags: flags & !FileFlags::APPEND,
            position_transaction: Mutex::new(()),
            position,
            readahead: AtomicU8::new(FadviseReadahead::Normal as u8),
            #[cfg(feature = "times")]
            access_flags: AtomicU8::new(0),
        }
    }

    /// Opens an existing file for reading.
    pub fn open(context: &FsContext, path: impl AsRef<FsPath>) -> VfsResult<Self> {
        OpenOptions::new()
            .read(true)
            .open(context, path.as_ref())
            .and_then(OpenResult::into_file)
    }

    /// Opens a file for writing, creating it if it does not exist and
    /// truncating it if it does.
    pub fn create(context: &FsContext, path: impl AsRef<FsPath>) -> VfsResult<Self> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(context, path.as_ref())
            .and_then(OpenResult::into_file)
    }

    /// Checks that the file has the required `flags` and returns the backend.
    pub fn access(&self, flags: FileFlags) -> VfsResult<&FileBackend> {
        let requires_append = flags.contains(FileFlags::APPEND);
        let required_access = flags & !FileFlags::APPEND;
        if self.flags.contains(required_access)
            && (!requires_append
                || (self.flags.contains(FileFlags::WRITE) && self.append_enabled()))
            && !self.is_path()
        {
            Ok(&self.inner)
        } else {
            Err(VfsError::BadFileDescriptor)
        }
    }

    /// Returns `true` if this is a path-only handle (no I/O permitted).
    pub fn is_path(&self) -> bool {
        self.flags.contains(FileFlags::PATH)
    }

    /// Returns the access flags this file was opened with.
    pub fn flags(&self) -> FileFlags {
        let mut flags = self.flags;
        flags.set(FileFlags::APPEND, self.append_enabled());
        flags
    }

    /// Whether ordinary I/O on this open file description owns a cursor.
    pub fn has_current_position(&self) -> bool {
        self.position.is_some()
    }

    /// Whether the node accepts explicit-offset reads.
    pub fn supports_positioned_read(&self) -> bool {
        !self
            .location()
            .flags()
            .contains(NodeFlags::NO_POSITIONED_READ)
    }

    /// Whether the node accepts explicit-offset writes.
    pub fn supports_positioned_write(&self) -> bool {
        !self
            .location()
            .flags()
            .contains(NodeFlags::NO_POSITIONED_WRITE)
    }

    /// Whether the node accepts seek operations.
    pub fn supports_seek(&self) -> bool {
        !self.location().flags().contains(NodeFlags::NO_SEEK)
    }

    /// Updates append status for this open file description without changing
    /// its immutable read/write access mode.
    pub fn set_append(&self, append: bool) {
        self.append.store(append, Ordering::Release);
    }

    pub(super) fn append_enabled(&self) -> bool {
        self.append.load(Ordering::Acquire)
    }

    /// Returns a reference to the underlying [`FileBackend`].
    pub fn backend(&self) -> VfsResult<&FileBackend> {
        self.access(FileFlags::empty())?;
        Ok(&self.inner)
    }

    /// Retains the stable inode file-attribute admission gate for a caller
    /// which must perform a native mutation outside the normal `File`
    /// read/write/range helpers (for example truncate's backend `set_len`).
    ///
    /// The returned token owns no provider or cache lock and must cover the
    /// complete native commit window.  `append` selects the only mutation an
    /// append-only inode may admit.
    pub fn begin_native_mutation(&self, append: bool) -> VfsResult<Option<FileAttrMutationGuard>> {
        self.access(FileFlags::WRITE)?;
        begin_native_location_mutation(self.location(), append)
    }

    /// Page-cache statistics do not require data-I/O access, so O_PATH file
    /// descriptors may query their regular file's mapping as on Linux.
    pub fn cachestat(&self, first_page: u64, last_page: u64) -> CachedFileCacheStat {
        self.inner.cachestat(first_page, last_page)
    }

    /// Returns a reference to the underlying [`Location`].
    pub fn location(&self) -> &Location {
        self.inner.location()
    }

    /// Returns the provider-owned operation handle for this exact OFD.
    /// Protocol filesystems use it for operations whose semantics are bound
    /// to an open handle rather than only an inode.
    pub fn open_handle(&self) -> Option<&Arc<dyn FileNodeOps>> {
        self.open_handle.as_ref()
    }

    pub fn set_fadvise_readahead(&self, policy: FadviseReadahead) {
        match policy {
            FadviseReadahead::Normal => self.readahead.store(0, Ordering::Release),
            FadviseReadahead::Random => {
                self.update_fadvise_bits(FADVISE_RANDOM, FADVISE_SEQUENTIAL);
            }
            FadviseReadahead::Sequential => {
                self.update_fadvise_bits(FADVISE_SEQUENTIAL, FADVISE_RANDOM);
            }
            FadviseReadahead::NoReuse => {
                self.readahead.fetch_or(FADVISE_NOREUSE, Ordering::AcqRel);
            }
        }
    }

    pub(super) fn fadvise_has(&self, flag: u8) -> bool {
        self.readahead.load(Ordering::Acquire) & flag != 0
    }

    pub(super) fn update_fadvise_bits(&self, set: u8, clear: u8) {
        let mut previous = self.readahead.load(Ordering::Acquire);
        loop {
            let next = fadvise_next_bits(previous, set, clear);
            match self.readahead.compare_exchange_weak(
                previous,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => previous = observed,
            }
        }
    }

    pub fn fadvise_willneed(&self, offset: u64, len: u64) -> VfsResult<()> {
        self.access(FileFlags::empty())?
            .fadvise_willneed(offset, len)
    }

    pub fn fadvise_noreuse(&self, offset: u64, len: u64) -> VfsResult<()> {
        self.access(FileFlags::empty())?
            .fadvise_noreuse(offset, len)
    }

    pub fn fadvise_dontneed(&self, offset: u64, len: u64) -> VfsResult<()> {
        self.access(FileFlags::empty())?
            .fadvise_dontneed(offset, len)
    }
}
