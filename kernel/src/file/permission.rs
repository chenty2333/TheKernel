use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::FsContext;
use axfs_ng_vfs::{
    FsName, FsPath, Location, Metadata, MetadataUpdate, NodePermission, NodeType, Timestamp,
    path::{FinalComponent, FinalComponentKind},
};
use linux_raw_sys::general::{
    CAP_CHOWN, CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH, CAP_FOWNER, CAP_FSETID, R_OK, W_OK, X_OK,
};
use linux_vfs::{
    Access, ChmodRequest as LinuxChmodRequest, ChmodSetattrPlan as LinuxChmodSetattrPlan,
    ChownRequest as LinuxChownRequest, ChownSetattrPlan as LinuxChownSetattrPlan, DacCapability,
    DacCredentials, DacError, HardlinkCredentials, NodeKind as LinuxNodeKind,
    NodeMetadata as LinuxNodeMetadata, PreparedSetattr as LinuxPreparedSetattr, SetattrError,
    check_dac as check_linux_dac, check_hardlink_source as check_linux_hardlink_source,
    check_sticky_mutation as check_linux_sticky_mutation,
    initial_create_attributes as linux_initial_create_attributes, plan_chmod as linux_plan_chmod,
    plan_chown as linux_plan_chown,
};
use thekernel_linux_cred::{
    FileOpenOperation, InodeChmodIntent, InodeChownIntent, InodeCreateMode, InodeMknodKind,
    InodeMknodOperation, InodePermissionAccess, InodeSetattrMode, InodeSetattrProposal,
    InodeTimestampIntent,
};

use crate::{
    file::{posix_acl, privilege_metadata::InodePrivilegeCleanup},
    pseudofs::check_proc_pid_dir_search,
    task::{
        AsThread, Cred, DacCredentialView, Kgid, Kuid, UserNamespace, ns_capable,
        security::{
            ExistingInodeSecurityRef, FileOpenSecurityContext, InodeCreateSecurityContext,
            InodeLinkSecurityContext, InodeMkdirSecurityContext, InodeMknodSecurityContext,
            InodePermissionOperation, InodePermissionSecurityContext, InodeRenameSecurityContext,
            InodeRmdirSecurityContext, InodeSecurityRef, InodeSetattrCommittedSecurityRef,
            InodeSetattrSecurityAdmission, InodeSetattrSecurityContext,
            InodeSymlinkSecurityContext, InodeUnlinkSecurityContext, LANDLOCK_ACCESS_FS_EXECUTE,
            LANDLOCK_ACCESS_FS_IOCTL_DEV, LANDLOCK_ACCESS_FS_MAKE_BLOCK,
            LANDLOCK_ACCESS_FS_MAKE_CHAR, LANDLOCK_ACCESS_FS_MAKE_DIR,
            LANDLOCK_ACCESS_FS_MAKE_FIFO, LANDLOCK_ACCESS_FS_MAKE_REG,
            LANDLOCK_ACCESS_FS_MAKE_SOCK, LANDLOCK_ACCESS_FS_MAKE_SYM, LANDLOCK_ACCESS_FS_READ_DIR,
            LANDLOCK_ACCESS_FS_READ_FILE, LANDLOCK_ACCESS_FS_REFER, LANDLOCK_ACCESS_FS_REMOVE_DIR,
            LANDLOCK_ACCESS_FS_REMOVE_FILE, LANDLOCK_ACCESS_FS_TRUNCATE,
            LANDLOCK_ACCESS_FS_WRITE_FILE, PlannedInodeSecurityRef, RenameDestinationSecurityRef,
            dispatch_file_open, dispatch_inode_create, dispatch_inode_link, dispatch_inode_mkdir,
            dispatch_inode_mknod, dispatch_inode_permission, dispatch_inode_rename,
            dispatch_inode_rmdir, dispatch_inode_setattr, dispatch_inode_symlink,
            dispatch_inode_unlink, initial_user_namespace,
        },
    },
    time::wall_time,
};

static INITIAL_USER_NAMESPACE_DAC_DOMAIN: () = ();

/// Projects on-disk ids through the immutable map selected for this exact
/// mount.  VFS stores filesystem ids; DAC must instead see the ids visible to
/// the task in an idmapped mount.  An actor outside the map's user namespace
/// deliberately receives overflow ids rather than accidental ownership.
fn idmapped_metadata_for_actor(
    loc: &Location,
    metadata: &Metadata,
    security: &VfsSecurityContext,
) -> AxResult<Metadata> {
    let Some(idmap) = security.idmap_for(loc)? else {
        return Ok(metadata.clone());
    };
    Ok(metadata_for_idmap(metadata, security.actor(), Some(&idmap)))
}

/// Projects metadata through an already selected mount idmap.  Descriptor
/// operations pass the OFD-pinned snapshot here instead of resolving the
/// caller's current mount namespace after `setns()`.
fn metadata_for_idmap(
    metadata: &Metadata,
    actor: &Cred,
    idmap: Option<&crate::mounts::MountIdmap>,
) -> Metadata {
    let Some(idmap) = idmap else {
        return metadata.clone();
    };
    let mut projected = metadata.clone();
    let map = |id: u32, rows: &[crate::mounts::MountIdmapRange]| {
        rows.iter().find_map(|row| {
            let end = row.outside.checked_add(row.length)?;
            (id >= row.outside && id < end)
                .then_some(row.inside.checked_add(id - row.outside))
                .flatten()
        })
    };
    if actor.user_ns().identity().into_raw() != idmap.user_namespace().identity().into_raw() {
        projected.uid = u32::MAX;
        projected.gid = u32::MAX;
    } else {
        projected.uid = map(metadata.uid, &idmap.uid).unwrap_or(u32::MAX);
        projected.gid = map(metadata.gid, &idmap.gid).unwrap_or(u32::MAX);
    }
    projected
}

/// One immutable actor/DAC/owner-namespace snapshot shared by a complete VFS
/// operation.
///
/// Pathwalk, final discretionary admission, and typed security hooks must all
/// observe the same composite credential. Keeping the actor `Arc` alongside
/// the derived DAC view also pins its module-state vector across a concurrent
/// credential replacement; callers never need to resample `current()` after
/// pathname traversal has started.
#[derive(Clone)]
pub(crate) struct VfsSecurityContext {
    actor: Arc<Cred>,
    credentials: DacCredentialView,
    filesystem_owner_user_ns: Arc<UserNamespace>,
    mount_topology: Option<Arc<crate::mounts::MountTopology>>,
    /// Detached mount FDs are not yet members of a namespace topology, but
    /// their retained idmaps still govern every pathname walk in that tree.
    detached_mount_idmaps: Option<Arc<Vec<(u64, Arc<crate::mounts::MountIdmap>)>>>,
    landlock_domain: Option<crate::task::security::LandlockDomain>,
}

/// Captures the current task's immutable VFS authority for one synchronous
/// operation.  MM advice paths use this after retaining a mapped file, where
/// no fd-table lookup is available but DAC/LSM checks must still observe the
/// same actor snapshot as ordinary VFS syscalls.
pub(crate) fn current_vfs_security() -> VfsSecurityContext {
    let current = axtask::current();
    VfsSecurityContext::new(current.as_thread().current_cred())
}

/// Landlock is task-local rather than credential-local.  Fetching its
/// immutable domain here is safe for this syscall execution context; a task
/// can only append a new layer by returning to userspace and issuing another
/// syscall.  Denials deliberately use EACCES, matching Linux path rules.
pub(crate) fn check_landlock_access(location: &Location, access: u64) -> AxResult {
    // Landlock restrictions belong to a Linux thread.  Host capability tests
    // and kernel-internal VFS operations can run without one; in that case
    // there is no task-local ruleset to enforce.
    if let Some(current) = axtask::current_may_uninit()
        && let Some(thread) = current.try_as_thread()
    {
        let domain = thread.landlock_domain();
        if !domain.allows_path(location, access) {
            domain.report_path_denial(location, access);
            return Err(AxError::PermissionDenied);
        }
    }
    Ok(())
}

pub(crate) fn landlock_allows_access(location: &Location, access: u64) -> bool {
    axtask::current_may_uninit().is_none_or(|current| {
        current
            .try_as_thread()
            .is_none_or(|thread| thread.landlock_domain().allows_path(location, access))
    })
}

/// A file description can cache the Landlock decision made at open time for
/// operations whose object identity cannot change.  The later rejection still
/// represents an access attempt and must be accounted/audited at that point.
pub(crate) fn report_cached_landlock_denial(location: &Location, access: u64) {
    if let Some(current) = axtask::current_may_uninit()
        && let Some(thread) = current.try_as_thread()
    {
        thread
            .landlock_domain()
            .report_path_denial(location, access);
    }
}
fn check_landlock_refer_transition(source: &Location, destination: &Location) -> AxResult {
    let compared_access = if source.is_dir() {
        u64::MAX
    } else {
        LANDLOCK_ACCESS_FS_EXECUTE
            | LANDLOCK_ACCESS_FS_WRITE_FILE
            | LANDLOCK_ACCESS_FS_READ_FILE
            | LANDLOCK_ACCESS_FS_REFER
            | LANDLOCK_ACCESS_FS_TRUNCATE
            | LANDLOCK_ACCESS_FS_IOCTL_DEV
    };
    let Some(current) = axtask::current_may_uninit() else {
        return Ok(());
    };
    let Some(thread) = current.try_as_thread() else {
        return Ok(());
    };
    let domain = thread.landlock_domain();
    let source_allowed = domain.allows_path(source, LANDLOCK_ACCESS_FS_REFER);
    let destination_allowed = domain.allows_path(destination, LANDLOCK_ACCESS_FS_REFER);
    if !source_allowed {
        domain.report_path_denial(source, LANDLOCK_ACCESS_FS_REFER);
    }
    if !destination_allowed {
        domain.report_path_denial(destination, LANDLOCK_ACCESS_FS_REFER);
    }
    // MAKE_*/REMOVE_* admission is checked by the caller before this helper,
    // which gives those ordinary access denials Linux's EACCES precedence.
    // REFER itself is the cross-hierarchy constraint: it is denied by default
    // even when a layer did not declare the bit, and both a missing REFER grant
    // and a destination which would gain rights are reported as EXDEV.
    if !source_allowed || !destination_allowed {
        return Err(LinuxError::EXDEV.into());
    }
    if domain.destination_is_no_less_restrictive(source, destination, compared_access) {
        Ok(())
    } else {
        Err(LinuxError::EXDEV.into())
    }
}

pub(crate) fn check_landlock_truncate(location: &Location) -> AxResult {
    check_landlock_access(location, LANDLOCK_ACCESS_FS_TRUNCATE)
}

fn landlock_make_access(node_type: NodeType) -> AxResult<u64> {
    match node_type {
        NodeType::RegularFile => Ok(LANDLOCK_ACCESS_FS_MAKE_REG),
        NodeType::Directory => Ok(LANDLOCK_ACCESS_FS_MAKE_DIR),
        NodeType::CharacterDevice => Ok(LANDLOCK_ACCESS_FS_MAKE_CHAR),
        NodeType::BlockDevice => Ok(LANDLOCK_ACCESS_FS_MAKE_BLOCK),
        NodeType::Fifo => Ok(LANDLOCK_ACCESS_FS_MAKE_FIFO),
        NodeType::Socket => Ok(LANDLOCK_ACCESS_FS_MAKE_SOCK),
        _ => Err(AxError::InvalidInput),
    }
}

impl VfsSecurityContext {
    pub(crate) fn new(actor: Arc<Cred>) -> Self {
        let credentials = actor.fs_dac_credentials();
        let filesystem_owner_user_ns = initial_user_namespace(actor.user_ns());
        Self {
            actor,
            credentials,
            filesystem_owner_user_ns,
            mount_topology: axtask::current_may_uninit().and_then(|task| {
                task.try_as_thread()
                    .map(|thread| thread.mount_ns().topology())
            }),
            detached_mount_idmaps: None,
            landlock_domain: axtask::current_may_uninit()
                .and_then(|task| task.try_as_thread().map(|thread| thread.landlock_domain())),
        }
    }

    /// Constructs an authority view for deferred pathname work.  No later
    /// permission or idmap lookup is allowed to consult the executor task.
    pub(crate) fn with_execution_authority(
        actor: Arc<Cred>,
        mount_topology: Arc<crate::mounts::MountTopology>,
        landlock_domain: crate::task::security::LandlockDomain,
    ) -> Self {
        let credentials = actor.fs_dac_credentials();
        let filesystem_owner_user_ns = initial_user_namespace(actor.user_ns());
        Self {
            actor,
            credentials,
            filesystem_owner_user_ns,
            mount_topology: Some(mount_topology),
            detached_mount_idmaps: None,
            landlock_domain: Some(landlock_domain),
        }
    }

    pub(crate) fn with_detached_mount_authority(
        actor: Arc<Cred>,
        detached_mount_idmaps: Vec<(u64, Arc<crate::mounts::MountIdmap>)>,
        landlock_domain: crate::task::security::LandlockDomain,
    ) -> AxResult<Self> {
        let credentials = actor.fs_dac_credentials();
        let filesystem_owner_user_ns = initial_user_namespace(actor.user_ns());
        let detached_mount_idmaps =
            Arc::try_new(detached_mount_idmaps).map_err(|_| AxError::NoMemory)?;
        Ok(Self {
            actor,
            credentials,
            filesystem_owner_user_ns,
            mount_topology: None,
            detached_mount_idmaps: Some(detached_mount_idmaps),
            landlock_domain: Some(landlock_domain),
        })
    }

    fn idmap_for(&self, location: &Location) -> AxResult<Option<Arc<crate::mounts::MountIdmap>>> {
        let mount_id = location.mountpoint().mount_id();
        if let Some(topology) = self.mount_topology.as_ref() {
            match topology.idmap_for_mount(mount_id) {
                Ok(idmap) => return Ok(idmap),
                Err(AxError::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(self.detached_mount_idmaps.as_ref().and_then(|idmaps| {
            idmaps
                .iter()
                .find(|(candidate, _)| *candidate == mount_id)
                .map(|(_, idmap)| idmap.clone())
        }))
    }

    pub(crate) fn check_landlock_access(&self, location: &Location, access: u64) -> AxResult {
        if let Some(domain) = self.landlock_domain.as_ref()
            && !domain.allows_path(location, access)
        {
            domain.report_path_denial(location, access);
            return Err(AxError::PermissionDenied);
        }
        Ok(())
    }

    pub(crate) fn landlock_allows_access(&self, location: &Location, access: u64) -> bool {
        self.landlock_domain
            .as_ref()
            .is_none_or(|domain| domain.allows_path(location, access))
    }

    pub(crate) fn mount_topology(&self) -> Option<Arc<crate::mounts::MountTopology>> {
        self.mount_topology.clone()
    }

    pub(crate) fn actor(&self) -> &Cred {
        &self.actor
    }

    pub(crate) fn actor_arc(&self) -> &Arc<Cred> {
        &self.actor
    }

    pub(crate) const fn credentials(&self) -> &DacCredentialView {
        &self.credentials
    }

    /// Tests one initial-user-namespace capability against the exact actor and
    /// DAC projection frozen for this VFS operation. The snapshot check keeps
    /// synthetic or future non-effective projections from being silently
    /// rebound to the actor's effective set; the actor check performs commoncap
    /// plus ordered stacked-policy dispatch.
    pub(crate) fn has_capability(&self, capability: u32) -> bool {
        self.credentials.has_capability(capability)
            && self.actor.has_effective_capability(capability)
    }

    pub(crate) const fn filesystem_owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.filesystem_owner_user_ns
    }

    /// Begins one inode-setattr admission over the exact metadata snapshot the
    /// typed setattr adapter will use to prepare its backend update.
    ///
    /// The returned linear admission owns the old object facts and retains this
    /// context's frozen actor/DAC/owner-namespace tuple. The caller must keep the
    /// inode metadata writer gate held through backend publication and either
    /// drop the admission on failure or carry it into the sealed published
    /// state after successful backend mutation.
    pub(crate) fn begin_inode_setattr<'context, 'location>(
        &'context self,
        location: &'location Location,
        fresh_metadata: &Metadata,
        proposal: InodeSetattrProposal,
    ) -> AxResult<InodeSetattrSecurityAdmission<'context, 'location>> {
        let old_object = InodeSecurityRef::new(location, fresh_metadata);
        dispatch_inode_setattr(InodeSetattrSecurityContext::new(
            self.actor(),
            self.credentials(),
            self.filesystem_owner_user_ns(),
            old_object,
            proposal,
        ))
    }

    /// Runs the dedicated `security_inode_file_getattr` pre-hook.  The hook
    /// intentionally has no DAC access decision attached: file attributes are
    /// metadata selected after pathname traversal, not a content read.
    pub(crate) fn inode_file_getattr(
        &self,
        location: &Location,
        metadata: &Metadata,
    ) -> AxResult<()> {
        let object = InodeSecurityRef::new(location, metadata);
        dispatch_inode_permission(&InodePermissionSecurityContext::new_for_operation(
            self.actor(),
            self.credentials(),
            self.filesystem_owner_user_ns(),
            &object,
            InodePermissionAccess::READ,
            InodePermissionOperation::FileGetattr,
        ))
    }

    pub(crate) fn begin_pseudo_inode_setattr<'context>(
        &'context self,
        metadata: &Metadata,
        proposal: InodeSetattrProposal,
    ) -> AxResult<InodeSetattrSecurityAdmission<'context, 'static>> {
        dispatch_inode_setattr(InodeSetattrSecurityContext::new(
            self.actor(),
            self.credentials(),
            self.filesystem_owner_user_ns(),
            InodeSecurityRef::new_pseudo(metadata),
            proposal,
        ))
    }
}

/// Backend update and successful committed facts derived from one consumed
/// Linux setattr policy plan.
pub(crate) struct PreparedMetadataSetattr {
    update: MetadataUpdate,
    committed: Metadata,
}

impl PreparedMetadataSetattr {
    #[cfg(test)]
    pub(crate) fn into_parts(self) -> (MetadataUpdate, Metadata) {
        (self.update, self.committed)
    }
}

/// One admitted and prepared setattr operation which has not yet reached its
/// generic VFS backend.
pub(crate) struct PreparedInodeSetattr<'context, 'location> {
    admission: InodeSetattrSecurityAdmission<'context, 'location>,
    location: &'location Location,
    prepared: PreparedMetadataSetattr,
    privilege_cleanup: Option<InodePrivilegeCleanup<'location>>,
}

impl<'context, 'location> PreparedInodeSetattr<'context, 'location> {
    /// The exact mode which the already-admitted Linux setattr plan will
    /// publish. Side metadata that must stay coherent with chmod (POSIX ACL
    /// masks) can be staged against this value before publication.
    pub(crate) const fn committed_mode(&self) -> NodePermission {
        self.prepared.committed.mode
    }

    /// Publishes the backend update against the exact location bound during
    /// pre-hook admission. Chown publication first consumes its location-bound
    /// privilege-cleanup token; a later backend failure deliberately does not
    /// roll that cleanup back. Any failure drops the admission and cannot
    /// construct a post-hook-capable state.
    pub(crate) fn publish(self) -> AxResult<PublishedInodeSetattr<'context, 'location>> {
        let Self {
            admission,
            location,
            prepared,
            privilege_cleanup,
        } = self;
        if let Some(privilege_cleanup) = privilege_cleanup {
            privilege_cleanup.apply()?;
        }
        location.update_metadata(prepared.update)?;
        Ok(PublishedInodeSetattr {
            admission: Some(admission),
            location,
            committed: prepared.committed,
        })
    }

    /// Stage a reversible companion update before publishing inode metadata.
    /// If the VFS metadata write fails, the staged state is rolled back before
    /// the security admission is dropped, so no post-hook token is leaked.
    pub(crate) fn publish_with_staged<T>(
        self,
        stage: impl FnOnce() -> AxResult<T>,
        rollback: impl FnOnce(T) -> AxResult<()>,
    ) -> AxResult<(PublishedInodeSetattr<'context, 'location>, T)> {
        let staged = stage()?;
        let Self {
            admission,
            location,
            prepared,
            privilege_cleanup,
        } = self;
        if let Some(privilege_cleanup) = privilege_cleanup
            && let Err(error) = privilege_cleanup.apply()
        {
            let _ = rollback(staged);
            return Err(error);
        }
        if let Err(error) = location.update_metadata(prepared.update) {
            let _ = rollback(staged);
            return Err(error);
        }
        Ok((
            PublishedInodeSetattr {
                admission: Some(admission),
                location,
                committed: prepared.committed,
            },
            staged,
        ))
    }
}

/// Timestamp setattr policy bound to one old-inode snapshot and one security
/// admission.  The caller keeps the metadata writer transaction across this
/// complete sequence so owner checks, the pre-hook, publication, and post-hook
/// cannot observe a concurrent chown in between.
pub(crate) struct TimestampSetattrPolicy<'a> {
    location: &'a Location,
    metadata: Metadata,
    atime: Option<Timestamp>,
    mtime: Option<Timestamp>,
    ctime: Timestamp,
    intent: InodeTimestampIntent,
}

impl<'a> TimestampSetattrPolicy<'a> {
    pub(crate) fn new(
        location: &'a Location,
        atime: Option<Timestamp>,
        mtime: Option<Timestamp>,
        intent: InodeTimestampIntent,
    ) -> AxResult<Self> {
        Ok(Self {
            location,
            metadata: location.metadata()?,
            atime,
            mtime,
            ctime: wall_time().into(),
            intent,
        })
    }

    pub(crate) fn admit<'context>(
        self,
        security: &'context VfsSecurityContext,
    ) -> AxResult<PreparedInodeSetattr<'context, 'a>> {
        let credentials = security.credentials();
        if Kuid::from_raw(self.metadata.uid) != Some(credentials.uid())
            && !security.has_capability(CAP_FOWNER)
        {
            use thekernel_linux_cred::InodeTimestampValue;
            if (self.intent.atime(), self.intent.mtime())
                != (InodeTimestampValue::Now, InodeTimestampValue::Now)
            {
                return Err(AxError::OperationNotPermitted);
            }
            check_open_permissions_with_security(
                self.location,
                W_OK,
                security.actor(),
                credentials,
                security.filesystem_owner_user_ns(),
            )?;
        }
        let admission = security.begin_inode_setattr(
            self.location,
            &self.metadata,
            InodeSetattrProposal::timestamps(self.intent),
        )?;
        let update = MetadataUpdate {
            atime: self.atime,
            mtime: self.mtime,
            ctime: Some(self.ctime),
            ..Default::default()
        };
        let mut committed = self.metadata;
        if let Some(atime) = self.atime {
            committed.atime = atime;
        }
        if let Some(mtime) = self.mtime {
            committed.mtime = mtime;
        }
        committed.ctime = self.ctime;
        Ok(PreparedInodeSetattr {
            admission,
            location: self.location,
            prepared: PreparedMetadataSetattr { update, committed },
            privilege_cleanup: None,
        })
    }
}

/// Backend-success token bound to the exact pre-hook inode and committed
/// attribute outcome.
///
/// Once this state exists, the successful-only post hook is mandatory. A
/// caller may run infallible/best-effort fsnotify first, then must consume the
/// token with [`Self::commit`].
pub(crate) struct PublishedInodeSetattr<'context, 'location> {
    admission: Option<InodeSetattrSecurityAdmission<'context, 'location>>,
    location: &'location Location,
    committed: Metadata,
}

impl PublishedInodeSetattr<'_, '_> {
    pub(crate) fn commit(mut self) {
        let admission = self
            .admission
            .take()
            .expect("published inode setattr admission already consumed");
        admission.committed(InodeSetattrCommittedSecurityRef::new(
            self.location,
            &self.committed,
        ));
    }
}

impl Drop for PublishedInodeSetattr<'_, '_> {
    fn drop(&mut self) {
        assert!(
            self.admission.is_none(),
            "successful inode setattr dropped before its post hook"
        );
    }
}

/// Kernel adapter joining one exact generic Linux-VFS chmod plan to the typed
/// credential hook contract.
///
/// The generic crate owns Linux owner/FOWNER/FSETID policy. This adapter owns
/// `Metadata` conversion, filesystem-ID mapping, errno selection, timestamps,
/// and construction of the independent credential hook proposal. The only
/// production transition is [`Self::admit`], which validates owner mapping,
/// dispatches the pre-hook, and returns a continuation bound to that success.
pub(crate) struct ChmodSetattrPolicy<'a> {
    location: &'a Location,
    metadata: Metadata,
    ctime: Timestamp,
    actor: &'a Cred,
    credentials: &'a DacCredentialView,
    plan: LinuxChmodSetattrPlan<'static, KernelDacCredentials<'a>>,
}

impl<'a> ChmodSetattrPolicy<'a> {
    pub(crate) fn new(
        location: &'a Location,
        requested_mode: u32,
        security: &'a VfsSecurityContext,
    ) -> AxResult<Self> {
        let metadata = location.metadata()?;
        let ctime = wall_time().into();
        let node = linux_metadata_snapshot(&metadata);
        let request = LinuxChmodRequest::new(requested_mode as u16);
        let actor = security.actor();
        let credentials = security.credentials();
        Ok(Self {
            location,
            metadata,
            ctime,
            actor,
            credentials,
            plan: linux_plan_chmod(
                &node,
                request,
                KernelDacCredentials::actor_bound(actor, credentials),
            ),
        })
    }

    pub(crate) const fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    fn validate_owner_mapping(&self, target_owner_user_ns: &UserNamespace) -> AxResult<()> {
        validate_setattr_owner_pair(&self.metadata, None, None, target_owner_user_ns)
    }

    fn into_hook_proposal(self) -> AxResult<(InodeSetattrProposal, ChmodSetattrAfterHook<'a>)> {
        let intent = InodeChmodIntent::new(inode_setattr_mode(self.plan.request().mode())?);
        Ok((
            InodeSetattrProposal::chmod(intent),
            ChmodSetattrAfterHook(self),
        ))
    }

    pub(crate) fn admit<'context>(
        self,
        security: &'context VfsSecurityContext,
    ) -> AxResult<AdmittedChmodSetattr<'a, 'context, 'a>> {
        if !core::ptr::eq(self.actor, security.actor())
            || !core::ptr::eq(self.credentials, security.credentials())
        {
            return Err(AxError::BadState);
        }
        self.validate_owner_mapping(security.filesystem_owner_user_ns())?;
        let location = self.location;
        let (proposal, after_hook) = self.into_hook_proposal()?;
        let admission = security.begin_inode_setattr(location, &after_hook.0.metadata, proposal)?;
        Ok(AdmittedChmodSetattr {
            admission,
            location,
            after_hook,
        })
    }
}

/// Consuming continuation proving that the chmod hook proposal has already
/// been separated from the exact policy plan.
struct ChmodSetattrAfterHook<'a>(ChmodSetattrPolicy<'a>);

impl ChmodSetattrAfterHook<'_> {
    fn prepare(self) -> AxResult<PreparedMetadataSetattr> {
        let prepared = self.0.plan.prepare().map_err(map_setattr_error)?;
        Ok(prepare_metadata_setattr(
            &self.0.metadata,
            prepared,
            self.0.ctime,
        ))
    }
}

pub(crate) struct AdmittedChmodSetattr<'policy, 'context, 'location> {
    admission: InodeSetattrSecurityAdmission<'context, 'location>,
    location: &'location Location,
    after_hook: ChmodSetattrAfterHook<'policy>,
}

impl<'context, 'location> AdmittedChmodSetattr<'_, 'context, 'location> {
    pub(crate) fn prepare(self) -> AxResult<PreparedInodeSetattr<'context, 'location>> {
        let prepared = self.after_hook.prepare()?;
        Ok(PreparedInodeSetattr {
            admission: self.admission,
            location: self.location,
            prepared,
            privilege_cleanup: None,
        })
    }
}

/// Kernel adapter joining one omission-aware generic Linux-VFS chown plan to
/// the typed credential hook contract.
pub(crate) struct ChownSetattrPolicy<'a> {
    location: &'a Location,
    metadata: Metadata,
    ctime: Timestamp,
    requested_user: Option<Kuid>,
    requested_group: Option<Kgid>,
    actor: &'a Cred,
    credentials: &'a DacCredentialView,
    plan: LinuxChownSetattrPlan<'static, KernelDacCredentials<'a>>,
}

impl<'a> ChownSetattrPolicy<'a> {
    pub(crate) fn new(
        location: &'a Location,
        requested_user: Option<Kuid>,
        requested_group: Option<Kgid>,
        security: &'a VfsSecurityContext,
    ) -> AxResult<Self> {
        let metadata = location.metadata()?;
        let ctime = wall_time().into();
        let node = linux_metadata_snapshot(&metadata);
        let request = LinuxChownRequest::new(
            requested_user.map(Kuid::into_raw),
            requested_group.map(Kgid::into_raw),
        );
        let actor = security.actor();
        let credentials = security.credentials();
        Ok(Self {
            location,
            metadata,
            ctime,
            requested_user,
            requested_group,
            actor,
            credentials,
            plan: linux_plan_chown(
                &node,
                request,
                KernelDacCredentials::actor_bound(actor, credentials),
            ),
        })
    }

    pub(crate) const fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    fn validate_owner_mapping(&self, target_owner_user_ns: &UserNamespace) -> AxResult<()> {
        validate_setattr_owner_pair(
            &self.metadata,
            self.requested_user,
            self.requested_group,
            target_owner_user_ns,
        )
    }

    fn into_hook_proposal(
        self,
        privilege_cleanup: InodePrivilegeCleanup<'a>,
    ) -> AxResult<(InodeSetattrProposal, ChownSetattrAfterHook<'a>)> {
        let intent = InodeChownIntent::new(self.requested_user, self.requested_group);
        let proposal = InodeSetattrProposal::chown(
            intent,
            self.plan.hook_mode().map(inode_setattr_mode).transpose()?,
            privilege_cleanup.intent(),
        );
        Ok((
            proposal,
            ChownSetattrAfterHook {
                policy: self,
                privilege_cleanup,
            },
        ))
    }

    pub(crate) fn admit<'context>(
        self,
        security: &'context VfsSecurityContext,
        privilege_cleanup: InodePrivilegeCleanup<'a>,
    ) -> AxResult<AdmittedChownSetattr<'a, 'context, 'a>> {
        if !core::ptr::eq(self.actor, security.actor())
            || !core::ptr::eq(self.credentials, security.credentials())
        {
            return Err(AxError::BadState);
        }
        privilege_cleanup.validate_location(self.location)?;
        self.validate_owner_mapping(security.filesystem_owner_user_ns())?;
        let location = self.location;
        let (proposal, after_hook) = self.into_hook_proposal(privilege_cleanup)?;
        let admission =
            security.begin_inode_setattr(location, &after_hook.policy.metadata, proposal)?;
        Ok(AdmittedChownSetattr {
            admission,
            location,
            after_hook,
        })
    }
}

/// Consuming continuation proving that the chown hook proposal has already
/// been separated from the exact omission-aware policy plan.
struct ChownSetattrAfterHook<'a> {
    policy: ChownSetattrPolicy<'a>,
    privilege_cleanup: InodePrivilegeCleanup<'a>,
}

impl<'a> ChownSetattrAfterHook<'a> {
    fn prepare(self) -> AxResult<(PreparedMetadataSetattr, InodePrivilegeCleanup<'a>)> {
        let Self {
            policy,
            privilege_cleanup,
        } = self;
        let prepared = policy.plan.prepare().map_err(map_setattr_error)?;
        Ok((
            prepare_metadata_setattr(&policy.metadata, prepared, policy.ctime),
            privilege_cleanup,
        ))
    }
}

pub(crate) struct AdmittedChownSetattr<'policy, 'context, 'location> {
    admission: InodeSetattrSecurityAdmission<'context, 'location>,
    location: &'location Location,
    after_hook: ChownSetattrAfterHook<'policy>,
}

impl<'context, 'location> AdmittedChownSetattr<'location, 'context, 'location> {
    pub(crate) fn prepare(self) -> AxResult<PreparedInodeSetattr<'context, 'location>> {
        let (prepared, privilege_cleanup) = self.after_hook.prepare()?;
        Ok(PreparedInodeSetattr {
            admission: self.admission,
            location: self.location,
            prepared,
            privilege_cleanup: Some(privilege_cleanup),
        })
    }
}

fn inode_setattr_mode(mode: u16) -> AxResult<InodeSetattrMode> {
    InodeSetattrMode::try_from_bits(mode).ok_or(AxError::BadState)
}

fn map_setattr_error(error: SetattrError) -> AxError {
    match error {
        SetattrError::ChownDenied | SetattrError::ChmodDenied => AxError::OperationNotPermitted,
        _ => AxError::OperationNotPermitted,
    }
}

/// Pure generic-policy adapter for unit tests which exercise Linux setattr
/// arithmetic without constructing a VFS location or bypassing a production
/// security-hook transition.
#[cfg(test)]
fn chown_plan_for_test<'a>(
    metadata: &Metadata,
    requested_user: Option<Kuid>,
    requested_group: Option<Kgid>,
    credentials: &'a DacCredentialView,
) -> LinuxChownSetattrPlan<'static, KernelDacCredentials<'a>> {
    let node = linux_metadata_snapshot(metadata);
    let request = LinuxChownRequest::new(
        requested_user.map(Kuid::into_raw),
        requested_group.map(Kgid::into_raw),
    );
    linux_plan_chown(&node, request, KernelDacCredentials::snapshot(credentials))
}

#[cfg(test)]
pub(crate) fn chown_hook_mode_for_test(
    metadata: &Metadata,
    credentials: &DacCredentialView,
) -> Option<NodePermission> {
    chown_plan_for_test(metadata, None, None, credentials)
        .hook_mode()
        .map(NodePermission::from_bits_truncate)
}

#[cfg(test)]
pub(crate) fn prepare_chown_metadata_setattr_for_test(
    metadata: &Metadata,
    requested_user: Option<Kuid>,
    requested_group: Option<Kgid>,
    credentials: &DacCredentialView,
    ctime: Timestamp,
) -> AxResult<PreparedMetadataSetattr> {
    let prepared = chown_plan_for_test(metadata, requested_user, requested_group, credentials)
        .prepare()
        .map_err(map_setattr_error)?;
    Ok(prepare_metadata_setattr(metadata, prepared, ctime))
}

#[cfg(test)]
pub(crate) fn prepare_chmod_metadata_setattr_for_test(
    metadata: &Metadata,
    requested_mode: u32,
    credentials: &DacCredentialView,
    ctime: Timestamp,
) -> AxResult<PreparedMetadataSetattr> {
    let node = linux_metadata_snapshot(metadata);
    let request = LinuxChmodRequest::new(requested_mode as u16);
    let prepared = linux_plan_chmod(&node, request, KernelDacCredentials::snapshot(credentials))
        .prepare()
        .map_err(map_setattr_error)?;
    Ok(prepare_metadata_setattr(metadata, prepared, ctime))
}

/// Validates the owner pair which will remain after the request. Linux rejects
/// an unmapped omitted field before the inode-setattr hook, while a present
/// valid field replaces that stale value.
fn validate_setattr_owner_pair(
    metadata: &Metadata,
    requested_user: Option<Kuid>,
    requested_group: Option<Kgid>,
    target_owner_user_ns: &UserNamespace,
) -> AxResult<()> {
    let user = match requested_user {
        Some(user) => user,
        None => Kuid::from_raw(metadata.uid).ok_or(LinuxError::EOVERFLOW)?,
    };
    let group = match requested_group {
        Some(group) => group,
        None => Kgid::from_raw(metadata.gid).ok_or(LinuxError::EOVERFLOW)?,
    };
    if target_owner_user_ns.kernel_uid_to_user(user).is_none()
        || target_owner_user_ns.kernel_gid_to_user(group).is_none()
    {
        return Err(LinuxError::EOVERFLOW.into());
    }
    Ok(())
}

fn prepare_metadata_setattr(
    metadata: &Metadata,
    prepared: LinuxPreparedSetattr<u32, u32>,
    ctime: Timestamp,
) -> PreparedMetadataSetattr {
    let update = MetadataUpdate {
        owner: prepared.owner(),
        mode: prepared.mode().map(NodePermission::from_bits_truncate),
        ctime: Some(ctime),
        ..Default::default()
    };
    let mut committed = metadata.clone();
    committed.uid = prepared.committed_user();
    committed.gid = prepared.committed_group();
    committed.mode = NodePermission::from_bits_truncate(prepared.committed_mode());
    committed.ctime = ctime;
    PreparedMetadataSetattr { update, committed }
}

#[derive(Clone, Copy)]
enum DacCapabilityDispatch<'a> {
    /// Pure Linux-DAC projection used by `access(2)` real-ID credentials and
    /// policy-arithmetic tests. It must never be rebound to a live actor whose
    /// effective set can differ from this snapshot.
    SnapshotOnly,
    /// Normal live VFS operation over one exact pinned composite actor.
    Actor(&'a Cred),
    /// Descriptor operation whose mount idmap pins the namespace relative to
    /// which VFS override capabilities are evaluated.
    ActorInNamespace(&'a Cred, &'a Arc<UserNamespace>),
}

#[derive(Clone, Copy)]
struct KernelDacCredentials<'a> {
    credentials: &'a DacCredentialView,
    capability_dispatch: DacCapabilityDispatch<'a>,
}

impl<'a> KernelDacCredentials<'a> {
    const fn snapshot(credentials: &'a DacCredentialView) -> Self {
        Self {
            credentials,
            capability_dispatch: DacCapabilityDispatch::SnapshotOnly,
        }
    }

    const fn actor_bound(actor: &'a Cred, credentials: &'a DacCredentialView) -> Self {
        Self {
            credentials,
            capability_dispatch: DacCapabilityDispatch::Actor(actor),
        }
    }

    const fn actor_bound_in_namespace(
        actor: &'a Cred,
        credentials: &'a DacCredentialView,
        namespace: &'a Arc<UserNamespace>,
    ) -> Self {
        Self {
            credentials,
            capability_dispatch: DacCapabilityDispatch::ActorInNamespace(actor, namespace),
        }
    }

    fn has_raw_capability(&self, capability: u32) -> bool {
        if !self.credentials.has_capability(capability) {
            return false;
        }
        match self.capability_dispatch {
            DacCapabilityDispatch::SnapshotOnly => true,
            DacCapabilityDispatch::Actor(actor) => actor.has_effective_capability(capability),
            DacCapabilityDispatch::ActorInNamespace(actor, namespace) => {
                ns_capable(actor, namespace, capability)
            }
        }
    }
}

impl DacCredentials for KernelDacCredentials<'_> {
    type UserId = u32;
    type GroupId = u32;
    type UserNamespace = ();

    fn fs_user_id(&self) -> Self::UserId {
        self.credentials.uid().into_raw()
    }

    fn fs_group_id(&self) -> Self::GroupId {
        self.credentials.gid().into_raw()
    }

    fn is_in_group(&self, group: Self::GroupId) -> bool {
        Kgid::from_raw(group)
            .is_some_and(|group| self.credentials.supplementary_groups().contains(&group))
    }

    fn has_capability(&self, _owner: &Self::UserNamespace, capability: DacCapability) -> bool {
        self.has_raw_capability(match capability {
            DacCapability::Override => CAP_DAC_OVERRIDE,
            DacCapability::ReadSearch => CAP_DAC_READ_SEARCH,
            DacCapability::Fowner => CAP_FOWNER,
            DacCapability::Fsetid => CAP_FSETID,
            DacCapability::Chown => CAP_CHOWN,
            _ => return false,
        })
    }
}

struct KernelHardlinkCredentials<'a> {
    actor: &'a Cred,
    dac: &'a DacCredentialView,
}

impl DacCredentials for KernelHardlinkCredentials<'_> {
    type UserId = u32;
    type GroupId = u32;
    type UserNamespace = ();

    fn fs_user_id(&self) -> Self::UserId {
        self.dac.uid().into_raw()
    }

    fn fs_group_id(&self) -> Self::GroupId {
        self.dac.gid().into_raw()
    }

    fn is_in_group(&self, group: Self::GroupId) -> bool {
        Kgid::from_raw(group).is_some_and(|group| self.dac.supplementary_groups().contains(&group))
    }

    fn has_capability(&self, _owner: &Self::UserNamespace, capability: DacCapability) -> bool {
        KernelDacCredentials::actor_bound(self.actor, self.dac).has_raw_capability(match capability
        {
            DacCapability::Override => CAP_DAC_OVERRIDE,
            DacCapability::ReadSearch => CAP_DAC_READ_SEARCH,
            DacCapability::Fowner => CAP_FOWNER,
            DacCapability::Fsetid => CAP_FSETID,
            DacCapability::Chown => CAP_CHOWN,
            _ => return false,
        })
    }
}

impl HardlinkCredentials for KernelHardlinkCredentials<'_> {
    fn user_id_is_mapped_in_own_namespace(&self, user: Self::UserId) -> bool {
        Kuid::from_raw(user)
            .is_some_and(|user| self.actor.user_ns().kernel_uid_to_user(user).is_some())
    }

    fn has_capability_in_own_namespace(&self, capability: DacCapability) -> bool {
        let capability = match capability {
            DacCapability::Fowner => CAP_FOWNER,
            _ => return false,
        };
        self.actor
            .has_effective_capability_in_own_user_ns(capability)
    }
}

fn linux_node_kind(node_type: NodeType) -> LinuxNodeKind {
    match node_type {
        NodeType::Unknown => LinuxNodeKind::Unknown,
        NodeType::Fifo => LinuxNodeKind::Fifo,
        NodeType::CharacterDevice => LinuxNodeKind::CharacterDevice,
        NodeType::Directory => LinuxNodeKind::Directory,
        NodeType::BlockDevice => LinuxNodeKind::BlockDevice,
        NodeType::RegularFile => LinuxNodeKind::Regular,
        NodeType::Symlink => LinuxNodeKind::Symlink,
        NodeType::Socket => LinuxNodeKind::Socket,
    }
}

fn linux_access(requested: u32) -> Access {
    let mut access = Access::NONE;
    if requested & R_OK != 0 {
        access |= Access::READ;
    }
    if requested & W_OK != 0 {
        access |= Access::WRITE;
    }
    if requested & X_OK != 0 {
        access |= Access::EXECUTE;
    }
    access
}

fn linux_node_metadata(
    mode: u32,
    owner_uid: u32,
    owner_gid: u32,
    node_type: NodeType,
) -> LinuxNodeMetadata<'static, u32, u32, ()> {
    LinuxNodeMetadata {
        mode: mode as u16,
        owner_user: owner_uid,
        owner_group: owner_gid,
        kind: linux_node_kind(node_type),
        owner_user_namespace: &INITIAL_USER_NAMESPACE_DAC_DOMAIN,
        ids_mapped: true,
    }
}

fn linux_metadata_snapshot(metadata: &Metadata) -> LinuxNodeMetadata<'static, u32, u32, ()> {
    linux_node_metadata(
        metadata.mode.bits() as u32,
        metadata.uid,
        metadata.gid,
        metadata.node_type,
    )
}

fn map_dac_error(error: DacError) -> AxError {
    match error {
        DacError::AccessDenied => AxError::PermissionDenied,
        DacError::StickyDenied => AxError::OperationNotPermitted,
        DacError::UnmappedId => LinuxError::EOVERFLOW.into(),
        DacError::HardlinkDenied => AxError::OperationNotPermitted,
        _ => AxError::PermissionDenied,
    }
}

pub(crate) fn check_writable_mount(dir: &Location) -> AxResult {
    if crate::mounts::is_readonly(dir)? {
        Err(AxError::ReadOnlyFilesystem)
    } else {
        Ok(())
    }
}

fn dac_access_allowed_with(
    perm: u32,
    owner_uid: u32,
    owner_gid: u32,
    node_type: NodeType,
    requested: u32,
    credentials: KernelDacCredentials<'_>,
) -> bool {
    check_linux_dac(
        &linux_node_metadata(perm, owner_uid, owner_gid, node_type),
        linux_access(requested),
        &credentials,
    )
    .is_ok()
}

pub(crate) fn dac_access_allowed(
    perm: u32,
    owner_uid: u32,
    owner_gid: u32,
    node_type: NodeType,
    requested: u32,
    credentials: &DacCredentialView,
) -> bool {
    dac_access_allowed_with(
        perm,
        owner_uid,
        owner_gid,
        node_type,
        requested,
        KernelDacCredentials::snapshot(credentials),
    )
}

pub(crate) fn check_dac_permissions(
    perm: u32,
    owner_uid: u32,
    owner_gid: u32,
    node_type: NodeType,
    requested: u32,
    credentials: &DacCredentialView,
) -> AxResult {
    if dac_access_allowed(
        perm,
        owner_uid,
        owner_gid,
        node_type,
        requested,
        credentials,
    ) {
        Ok(())
    } else {
        Err(AxError::PermissionDenied)
    }
}

fn check_dac_permissions_with_actor(
    perm: u32,
    owner_uid: u32,
    owner_gid: u32,
    node_type: NodeType,
    requested: u32,
    actor: &Cred,
    credentials: &DacCredentialView,
) -> AxResult {
    if dac_access_allowed_with(
        perm,
        owner_uid,
        owner_gid,
        node_type,
        requested,
        KernelDacCredentials::actor_bound(actor, credentials),
    ) {
        Ok(())
    } else {
        Err(AxError::PermissionDenied)
    }
}

fn check_dac_permissions_with_actor_in_namespace(
    perm: u32,
    owner_uid: u32,
    owner_gid: u32,
    node_type: NodeType,
    requested: u32,
    actor: &Cred,
    credentials: &DacCredentialView,
    namespace: &Arc<UserNamespace>,
) -> AxResult {
    if dac_access_allowed_with(
        perm,
        owner_uid,
        owner_gid,
        node_type,
        requested,
        KernelDacCredentials::actor_bound_in_namespace(actor, credentials, namespace),
    ) {
        Ok(())
    } else {
        Err(AxError::PermissionDenied)
    }
}

pub(crate) fn check_dac_permissions_with_security(
    perm: u32,
    owner_uid: u32,
    owner_gid: u32,
    node_type: NodeType,
    requested: u32,
    security: &VfsSecurityContext,
) -> AxResult {
    check_dac_permissions_with_actor(
        perm,
        owner_uid,
        owner_gid,
        node_type,
        requested,
        security.actor(),
        security.credentials(),
    )
}

fn inode_permission_access(requested: u32) -> AxResult<Option<InodePermissionAccess>> {
    if requested == 0 {
        return Ok(None);
    }
    if requested & !(R_OK | W_OK | X_OK) != 0 {
        return Err(AxError::InvalidInput);
    }
    let mut bits = 0;
    if requested & R_OK != 0 {
        bits |= InodePermissionAccess::READ.bits();
    }
    if requested & W_OK != 0 {
        bits |= InodePermissionAccess::WRITE.bits();
    }
    if requested & X_OK != 0 {
        bits |= InodePermissionAccess::EXECUTE.bits();
    }
    InodePermissionAccess::try_from_bits(bits)
        .map(Some)
        .ok_or(AxError::InvalidInput)
}

/// Runs Linux DAC and the frozen typed inode hook against one metadata
/// snapshot. Callers must supply the effective filesystem projection derived
/// from this exact actor. Real-ID `access(2)` projections deliberately stay on
/// the snapshot-only path until the ABI layer has a distinct typed context for
/// synthetic credentials; binding them here would apply the wrong effective
/// set to commoncap and stacked policy.
pub(crate) fn check_inode_permissions(
    loc: &Location,
    requested: u32,
    actor: &Cred,
    credentials: &DacCredentialView,
    filesystem_owner_user_ns: &Arc<UserNamespace>,
) -> AxResult {
    let Some(access) = inode_permission_access(requested)? else {
        return Ok(());
    };
    let metadata = loc.metadata()?;
    check_inode_permissions_with_metadata(
        loc,
        &metadata,
        requested,
        access,
        actor,
        credentials,
        filesystem_owner_user_ns,
    )
}

/// Runs final-inode DAC and the generic inode-permission hook against a
/// metadata snapshot already frozen by the operation-specific caller.
///
/// Xattr and setattr-style operations need their DAC decision and typed
/// operation hook to name the same inode facts. Accepting the snapshot here
/// avoids a second metadata read between those two policy stages.
pub(crate) fn check_inode_permissions_with_security(
    loc: &Location,
    metadata: &Metadata,
    requested: u32,
    security: &VfsSecurityContext,
) -> AxResult {
    let Some(access) = inode_permission_access(requested)? else {
        return Ok(());
    };
    let projected = idmapped_metadata_for_actor(loc, metadata, security)?;
    check_inode_permissions_with_projected_metadata(
        loc,
        &projected,
        requested,
        access,
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
        None,
        Some(security),
    )
}

/// Computes the two facts used by Linux's `inode_owner_or_capable()`.
///
/// This is deliberately distinct from [`metadata_for_idmap`], whose overflow
/// projection is a DAC view for one actor.  `inode_owner_or_capable()` first
/// forms a mount-relative vfsuid, compares it with the current fsuid, then
/// permits `CAP_FOWNER` only when that vfsuid maps into the caller's current
/// user namespace.
pub(crate) fn inode_owner_and_fowner_with_idmap(
    metadata: &Metadata,
    security: &VfsSecurityContext,
    idmap: Option<&crate::mounts::MountIdmap>,
) -> (bool, bool) {
    let vfsuid = idmap.map_or_else(
        || Kuid::from_raw(metadata.uid),
        |idmap| {
            idmap
                .uid
                .iter()
                .find_map(|row| {
                    let end = row.outside.checked_add(row.length)?;
                    (metadata.uid >= row.outside && metadata.uid < end)
                        .then_some(row.inside.checked_add(metadata.uid - row.outside))
                        .flatten()
                })
                // Linux's make_vfsuid() wraps map_id_down()'s mount-relative raw
                // value directly as a vfsuid/kuid. Mapping it back through the
                // idmap namespace would undo the mount shift and grant ownership
                // to the wrong fsuid.
                .and_then(Kuid::from_raw)
        },
    );
    let actor = security.actor();
    let owner = vfsuid == Some(actor.ids().fsuid);
    let fowner_capable = vfsuid
        .is_some_and(|uid| actor.user_ns().kernel_uid_to_user(uid).is_some())
        && ns_capable(actor, actor.user_ns(), CAP_FOWNER);
    (owner, fowner_capable)
}

/// Descriptor counterpart to [`check_inode_permissions_with_security`].
/// The selected idmap is part of the open file description and therefore
/// remains stable across mount-namespace changes by the calling task.
pub(crate) fn check_inode_permissions_with_security_and_idmap(
    loc: &Location,
    metadata: &Metadata,
    requested: u32,
    security: &VfsSecurityContext,
    idmap: Option<&crate::mounts::MountIdmap>,
) -> AxResult {
    let Some(access) = inode_permission_access(requested)? else {
        return Ok(());
    };
    let projected = metadata_for_idmap(metadata, security.actor(), idmap);
    check_inode_permissions_with_projected_metadata(
        loc,
        &projected,
        requested,
        access,
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
        idmap.map(crate::mounts::MountIdmap::user_namespace),
        Some(security),
    )
}

/// Descriptor-only pseudo inodes use the same actor-bound DAC projection and
/// inode-permission hook as a path-backed inode, but have no mount location.
pub(crate) fn check_pseudo_inode_permissions_with_security(
    metadata: &Metadata,
    requested: u32,
    security: &VfsSecurityContext,
) -> AxResult {
    let Some(access) = inode_permission_access(requested)? else {
        return Ok(());
    };
    check_dac_permissions_with_actor(
        metadata.mode.bits() as u32,
        metadata.uid,
        metadata.gid,
        metadata.node_type,
        requested,
        security.actor(),
        security.credentials(),
    )?;
    let object = InodeSecurityRef::new_pseudo(metadata);
    dispatch_inode_permission(&InodePermissionSecurityContext::new(
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
        &object,
        access,
    ))
}

fn check_inode_permissions_with_metadata(
    loc: &Location,
    metadata: &Metadata,
    requested: u32,
    access: InodePermissionAccess,
    actor: &Cred,
    credentials: &DacCredentialView,
    filesystem_owner_user_ns: &Arc<UserNamespace>,
) -> AxResult {
    // Non-deferred callers retain their established current-task semantics.
    // Deferred OPENAT2 uses `check_inode_permissions_with_security` above,
    // which supplies its captured topology explicitly.
    let topology = axtask::current_may_uninit().and_then(|task| {
        task.try_as_thread()
            .map(|thread| thread.mount_ns().topology())
    });
    let idmap = topology
        .map(|topology| topology.idmap_for_mount(loc.mountpoint().mount_id()))
        .transpose()?
        .flatten();
    let projected_metadata = metadata_for_idmap(metadata, actor, idmap.as_deref());
    check_inode_permissions_with_projected_metadata(
        loc,
        &projected_metadata,
        requested,
        access,
        actor,
        credentials,
        filesystem_owner_user_ns,
        None,
        None,
    )
}

fn check_inode_permissions_with_projected_metadata(
    loc: &Location,
    metadata: &Metadata,
    requested: u32,
    access: InodePermissionAccess,
    actor: &Cred,
    credentials: &DacCredentialView,
    filesystem_owner_user_ns: &Arc<UserNamespace>,
    capability_user_ns: Option<&Arc<UserNamespace>>,
    landlock_security: Option<&VfsSecurityContext>,
) -> AxResult {
    if let Some(allowed) = posix_acl::check_access(loc, metadata, requested, credentials)? {
        // An ACL named entry can grant access which the group mode cannot
        // express. Conversely, a matching named entry must deny even when a
        // broad group-mode bit would otherwise allow it. The two DAC override
        // capabilities retain their normal Linux meaning.
        let capable = |capability| {
            capability_user_ns.map_or_else(
                || actor.has_effective_capability(capability),
                |namespace| ns_capable(actor, namespace, capability),
            )
        };
        let override_allowed =
            capable(CAP_DAC_OVERRIDE) || requested & W_OK == 0 && capable(CAP_DAC_READ_SEARCH);
        if !allowed && !override_allowed {
            return Err(AxError::PermissionDenied);
        }
        if allowed {
            return check_inode_permission_post_dac(
                loc,
                metadata,
                requested,
                access,
                actor,
                credentials,
                filesystem_owner_user_ns,
                landlock_security,
            );
        }
    }
    if let Some(namespace) = capability_user_ns {
        check_dac_permissions_with_actor_in_namespace(
            metadata.mode.bits() as u32,
            metadata.uid,
            metadata.gid,
            metadata.node_type,
            requested,
            actor,
            credentials,
            namespace,
        )?;
    } else {
        check_dac_permissions_with_actor(
            metadata.mode.bits() as u32,
            metadata.uid,
            metadata.gid,
            metadata.node_type,
            requested,
            actor,
            credentials,
        )?;
    }
    check_inode_permission_post_dac(
        loc,
        metadata,
        requested,
        access,
        actor,
        credentials,
        filesystem_owner_user_ns,
        landlock_security,
    )
}

fn check_inode_permission_post_dac(
    loc: &Location,
    metadata: &Metadata,
    requested: u32,
    access: InodePermissionAccess,
    actor: &Cred,
    credentials: &DacCredentialView,
    filesystem_owner_user_ns: &Arc<UserNamespace>,
    landlock_security: Option<&VfsSecurityContext>,
) -> AxResult {
    let requested = requested & (R_OK | W_OK);
    let landlock_access = match metadata.node_type {
        NodeType::Directory if requested & R_OK != 0 => LANDLOCK_ACCESS_FS_READ_DIR,
        NodeType::Directory => 0,
        _ => {
            (if requested & R_OK != 0 {
                LANDLOCK_ACCESS_FS_READ_FILE
            } else {
                0
            }) | (if requested & W_OK != 0 {
                LANDLOCK_ACCESS_FS_WRITE_FILE
            } else {
                0
            })
        }
    };
    if landlock_access != 0 {
        if let Some(security) = landlock_security {
            security.check_landlock_access(loc, landlock_access)?;
        } else {
            check_landlock_access(loc, landlock_access)?;
        }
    }
    let object = InodeSecurityRef::new(loc, metadata);
    dispatch_inode_permission(&InodePermissionSecurityContext::new(
        actor,
        credentials,
        filesystem_owner_user_ns,
        &object,
        access,
    ))
}

/// Runs the typed file-open hook for the exact pre-open location. Callers
/// invoke this before `OpenOptions::open_loc`, so a denial cannot leave an
/// `O_TRUNC` side effect or a visible file description behind.
pub(crate) fn authorize_file_open(
    loc: &Location,
    actor: &Cred,
    credentials: &DacCredentialView,
    filesystem_owner_user_ns: &Arc<UserNamespace>,
    operation: FileOpenOperation,
) -> AxResult {
    let metadata = loc.metadata()?;
    let object = InodeSecurityRef::new(loc, &metadata);
    dispatch_file_open(&FileOpenSecurityContext::new(
        actor,
        credentials,
        filesystem_owner_user_ns,
        &object,
        operation,
    ))
}

/// Runs the Linux hook family selected by one named inode kind against the
/// exact parent metadata and final component which will be published.
///
/// The caller has already completed pathwalk and directory DAC admission and
/// computed `mode` from this same parent snapshot. Dispatch is the last
/// policy failure before the generic VFS mutation; symlink, hard-link, and
/// unnamed temporary-file creation are intentionally not representable here.
pub(crate) fn authorize_named_inode_create(
    parent: &Location,
    parent_metadata: &Metadata,
    name: &FsName,
    node_type: NodeType,
    mode: NodePermission,
    rdev: Option<axfs_ng_vfs::DeviceId>,
    security: &VfsSecurityContext,
) -> AxResult {
    security.check_landlock_access(parent, landlock_make_access(node_type)?)?;
    let mode = InodeCreateMode::try_from_bits(mode.bits()).ok_or(AxError::BadState)?;
    let parent_object = InodeSecurityRef::new(parent, parent_metadata);
    let planned = PlannedInodeSecurityRef::new(parent_object, name);
    match node_type {
        NodeType::RegularFile if rdev.is_none() => {
            dispatch_inode_create(&InodeCreateSecurityContext::new(
                security.actor(),
                security.credentials(),
                security.filesystem_owner_user_ns(),
                &planned,
                mode,
            ))
        }
        NodeType::Directory if rdev.is_none() => {
            dispatch_inode_mkdir(&InodeMkdirSecurityContext::new(
                security.actor(),
                security.credentials(),
                security.filesystem_owner_user_ns(),
                &planned,
                mode,
            ))
        }
        NodeType::Fifo | NodeType::CharacterDevice | NodeType::BlockDevice | NodeType::Socket => {
            let kind = match node_type {
                NodeType::Fifo => InodeMknodKind::Fifo,
                NodeType::CharacterDevice => InodeMknodKind::CharacterDevice,
                NodeType::BlockDevice => InodeMknodKind::BlockDevice,
                NodeType::Socket => InodeMknodKind::Socket,
                _ => return Err(AxError::InvalidInput),
            };
            let operation = InodeMknodOperation::new(kind, mode, rdev.map(|device| device.0))
                .ok_or(AxError::BadState)?;
            dispatch_inode_mknod(&InodeMknodSecurityContext::new(
                security.actor(),
                security.credentials(),
                security.filesystem_owner_user_ns(),
                &planned,
                operation,
            ))
        }
        _ => Err(AxError::InvalidInput),
    }
}

/// Runs the Linux-style symbolic-link hook against the exact parent snapshot,
/// final component, and target which the transaction will publish.
///
/// Destination DAC and absence revalidation have already completed. Dispatch
/// is the final policy failure before the generic VFS creates the symlink.
pub(crate) fn authorize_symlink_create(
    parent: &Location,
    parent_metadata: &Metadata,
    name: &FsName,
    target: &FsPath,
    security: &VfsSecurityContext,
) -> AxResult {
    check_landlock_access(parent, LANDLOCK_ACCESS_FS_MAKE_SYM)?;
    let parent_object = InodeSecurityRef::new(parent, parent_metadata);
    let planned = PlannedInodeSecurityRef::new(parent_object, name);
    dispatch_inode_symlink(&InodeSymlinkSecurityContext::new(
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
        &planned,
        target,
    ))
}

/// Applies Linux protected-hardlink source policy to one stable metadata
/// snapshot. The source `READ | WRITE` inode-permission hook runs only for a
/// safe-shaped regular file; its denial marks the source unsafe but owner or
/// mapped own-namespace `CAP_FOWNER` may still admit the link.
pub(crate) fn authorize_hardlink_source(
    source: &Location,
    source_metadata: &Metadata,
    security: &VfsSecurityContext,
    protected_hardlinks: bool,
) -> AxResult {
    let credentials = KernelHardlinkCredentials {
        actor: security.actor(),
        dac: security.credentials(),
    };
    check_linux_hardlink_source(
        &linux_node_metadata(
            source_metadata.mode.bits() as u32,
            source_metadata.uid,
            source_metadata.gid,
            source_metadata.node_type,
        ),
        &credentials,
        protected_hardlinks,
        |_| {
            check_inode_permissions_with_metadata(
                source,
                source_metadata,
                R_OK | W_OK,
                InodePermissionAccess::READ | InodePermissionAccess::WRITE,
                security.actor(),
                security.credentials(),
                security.filesystem_owner_user_ns(),
            )
            .is_ok()
        },
    )
    .map_err(map_dac_error)
}

/// Runs the Linux-style `inode_link` hook against the exact source snapshot
/// and prospective destination which the transaction will publish.
pub(crate) fn authorize_hardlink_create(
    source: &Location,
    source_metadata: &Metadata,
    parent: &Location,
    parent_metadata: &Metadata,
    name: &FsName,
    security: &VfsSecurityContext,
) -> AxResult {
    check_landlock_access(parent, LANDLOCK_ACCESS_FS_MAKE_REG)?;
    if let Some(source_parent) = source.parent()
        && !source_parent.same_node(parent)
    {
        check_landlock_refer_transition(&source_parent, parent)?;
    }
    let source_object = InodeSecurityRef::new(source, source_metadata);
    let parent_object = InodeSecurityRef::new(parent, parent_metadata);
    let planned = PlannedInodeSecurityRef::new(parent_object, name);
    dispatch_inode_link(&InodeLinkSecurityContext::new(
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
        &source_object,
        &planned,
    ))
}

/// Runs the Linux-style `inode_unlink` hook against the exact parent, final
/// component, and victim snapshot retained by the namespace transaction.
///
/// The caller completes the currently supported `may_delete` admission,
/// backend-capability check, and mountpoint rejection before reaching this
/// helper. Directory emptiness and the actual namespace mutation remain in
/// the backend after the hook.
pub(crate) fn authorize_inode_unlink(
    parent: &Location,
    parent_metadata: &Metadata,
    target: &Location,
    target_metadata: &Metadata,
    name: &FsName,
    security: &VfsSecurityContext,
) -> AxResult {
    check_landlock_access(parent, LANDLOCK_ACCESS_FS_REMOVE_FILE)?;
    let parent_object = InodeSecurityRef::new(parent, parent_metadata);
    let target_object = InodeSecurityRef::new(target, target_metadata);
    let existing = ExistingInodeSecurityRef::new(parent_object, target_object, name);
    dispatch_inode_unlink(&InodeUnlinkSecurityContext::new(
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
        &existing,
    ))
}

/// Runs the distinct Linux-style `inode_rmdir` hook against the exact parent,
/// final component, and victim-directory snapshot retained by the namespace
/// transaction.
///
/// Backend directory-emptiness admission deliberately remains after this
/// hook, matching Linux `vfs_rmdir()` ordering.
pub(crate) fn authorize_inode_rmdir(
    parent: &Location,
    parent_metadata: &Metadata,
    target: &Location,
    target_metadata: &Metadata,
    name: &FsName,
    security: &VfsSecurityContext,
) -> AxResult {
    check_landlock_access(parent, LANDLOCK_ACCESS_FS_REMOVE_DIR)?;
    let parent_object = InodeSecurityRef::new(parent, parent_metadata);
    let target_object = InodeSecurityRef::new(target, target_metadata);
    let existing = ExistingInodeSecurityRef::new(parent_object, target_object, name);
    dispatch_inode_rmdir(&InodeRmdirSecurityContext::new(
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
        &existing,
    ))
}

/// Runs one forward Linux-style `inode_rename` leaf hook over the exact source
/// and destination snapshots retained by the namespace transaction.
///
/// Ordinary and `RENAME_NOREPLACE` both use this one forward dispatch. A
/// future exchange transaction must explicitly construct and dispatch the
/// reverse context first; flags never enter the leaf contract.
#[allow(clippy::too_many_arguments)]
pub(crate) fn authorize_inode_rename(
    old_parent: &Location,
    old_parent_metadata: &Metadata,
    source: &Location,
    source_metadata: &Metadata,
    old_name: &FsName,
    new_parent: &Location,
    new_parent_metadata: &Metadata,
    replaced: Option<(&Location, &Metadata)>,
    new_name: &FsName,
    security: &VfsSecurityContext,
) -> AxResult<()> {
    check_landlock_access(
        old_parent,
        if source_metadata.node_type == NodeType::Directory {
            LANDLOCK_ACCESS_FS_REMOVE_DIR
        } else {
            LANDLOCK_ACCESS_FS_REMOVE_FILE
        },
    )?;
    check_landlock_access(
        new_parent,
        match replaced {
            Some((target, _)) if target.is_dir() => LANDLOCK_ACCESS_FS_REMOVE_DIR,
            Some(_) => LANDLOCK_ACCESS_FS_REMOVE_FILE,
            None => landlock_make_access(source_metadata.node_type)?,
        },
    )?;
    if !old_parent.same_node(new_parent) {
        check_landlock_refer_transition(old_parent, new_parent)?;
    }
    let old_parent_object = InodeSecurityRef::new(old_parent, old_parent_metadata);
    let source_object = InodeSecurityRef::new(source, source_metadata);
    let old_entry = ExistingInodeSecurityRef::new(old_parent_object, source_object, old_name);
    let new_parent_object = InodeSecurityRef::new(new_parent, new_parent_metadata);
    let new_entry = match replaced {
        Some((target, metadata)) => RenameDestinationSecurityRef::existing(
            new_parent_object,
            InodeSecurityRef::new(target, metadata),
            new_name,
        ),
        None => RenameDestinationSecurityRef::absent(new_parent_object, new_name),
    };
    dispatch_inode_rename(&InodeRenameSecurityContext::new(
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
        &old_entry,
        &new_entry,
    ))
}

pub(crate) fn check_pathwalk_search_permission_with_security(
    dir: &Location,
    actor: &Cred,
    credentials: &DacCredentialView,
    filesystem_owner_user_ns: &Arc<UserNamespace>,
) -> AxResult {
    check_proc_pid_dir_search(dir)?;
    check_inode_permissions(dir, X_OK, actor, credentials, filesystem_owner_user_ns)
}

pub(crate) fn check_pathwalk_search_permission_with_vfs_security(
    dir: &Location,
    security: &VfsSecurityContext,
) -> AxResult {
    check_proc_pid_dir_search(dir)?;
    check_inode_permissions_with_security(dir, &dir.metadata()?, X_OK, security)
}

pub(crate) fn check_create_permissions_with_security(
    dir: &Location,
    actor: &Cred,
    credentials: &DacCredentialView,
    filesystem_owner_user_ns: &Arc<UserNamespace>,
) -> AxResult {
    check_writable_mount(dir)?;
    check_inode_permissions(
        dir,
        W_OK | X_OK,
        actor,
        credentials,
        filesystem_owner_user_ns,
    )
}

pub(crate) fn check_create_permissions_with_vfs_security(
    dir: &Location,
    security: &VfsSecurityContext,
) -> AxResult {
    check_writable_mount(dir)?;
    check_inode_permissions_with_security(dir, &dir.metadata()?, W_OK | X_OK, security)
}

/// Applies named-create mount and parent admission to one caller-frozen
/// metadata snapshot. Namespace transactions use this after exact negative
/// destination revalidation so an existing final component wins over
/// writable/DAC errors and the parent inode-permission hook runs only once.
pub(crate) fn check_create_permissions_with_frozen_metadata(
    dir: &Location,
    dir_metadata: &Metadata,
    security: &VfsSecurityContext,
) -> AxResult {
    check_writable_mount(dir)?;
    check_inode_permissions_with_metadata(
        dir,
        dir_metadata,
        W_OK | X_OK,
        InodePermissionAccess::WRITE | InodePermissionAccess::EXECUTE,
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
    )
}

pub(crate) fn check_open_permissions_with_security(
    loc: &Location,
    mask: u32,
    actor: &Cred,
    credentials: &DacCredentialView,
    filesystem_owner_user_ns: &Arc<UserNamespace>,
) -> AxResult {
    check_inode_permissions(loc, mask, actor, credentials, filesystem_owner_user_ns)
}

pub(crate) fn check_open_permissions_with_vfs_security(
    loc: &Location,
    mask: u32,
    security: &VfsSecurityContext,
) -> AxResult {
    check_inode_permissions_with_security(loc, &loc.metadata()?, mask, security)
}

pub(crate) fn check_landlock_truncate_with_vfs_security(
    location: &Location,
    security: &VfsSecurityContext,
) -> AxResult {
    security.check_landlock_access(location, LANDLOCK_ACCESS_FS_TRUNCATE)
}

pub(crate) fn check_execute_permissions_with_security(
    loc: &Location,
    actor: &Cred,
    credentials: &DacCredentialView,
    filesystem_owner_user_ns: &Arc<UserNamespace>,
) -> AxResult {
    if crate::mounts::is_noexec(loc)? {
        return Err(AxError::PermissionDenied);
    }

    let metadata = loc.metadata()?;
    if metadata.node_type != NodeType::RegularFile {
        return Err(AxError::PermissionDenied);
    }
    check_landlock_access(loc, LANDLOCK_ACCESS_FS_EXECUTE)?;
    check_inode_permissions_with_metadata(
        loc,
        &metadata,
        X_OK,
        InodePermissionAccess::EXECUTE,
        actor,
        credentials,
        filesystem_owner_user_ns,
    )
}

pub(crate) fn check_execute_permissions_with_vfs_security(
    loc: &Location,
    security: &VfsSecurityContext,
) -> AxResult {
    if crate::mounts::is_noexec(loc)? {
        return Err(AxError::PermissionDenied);
    }

    let metadata = loc.metadata()?;
    if metadata.node_type != NodeType::RegularFile {
        return Err(AxError::PermissionDenied);
    }
    security.check_landlock_access(loc, LANDLOCK_ACCESS_FS_EXECUTE)?;
    check_inode_permissions_with_security(loc, &metadata, X_OK, security)
}

pub(crate) fn check_pathwalk_search_permission(
    dir: &Location,
    credentials: &DacCredentialView,
) -> AxResult {
    check_proc_pid_dir_search(dir)?;
    let stat = dir.metadata()?;
    check_dac_permissions(
        stat.mode.bits() as u32,
        stat.uid,
        stat.gid,
        stat.node_type,
        X_OK,
        credentials,
    )
}

/// Linux DAC admission over the generic axfs path resolver.
///
/// The generic resolver owns component and symlink traversal. This adapter
/// injects one immutable-per-operation credential view without teaching axfs
/// about Linux identities or capabilities.
pub(crate) trait DacFsContextExt {
    fn resolve_dac(
        &self,
        path: impl AsRef<FsPath>,
        credentials: &DacCredentialView,
    ) -> AxResult<Location>;
    fn resolve_no_follow_dac(
        &self,
        path: impl AsRef<FsPath>,
        credentials: &DacCredentialView,
    ) -> AxResult<Location>;
}

impl DacFsContextExt for FsContext {
    fn resolve_dac(
        &self,
        path: impl AsRef<FsPath>,
        credentials: &DacCredentialView,
    ) -> AxResult<Location> {
        self.resolve_with_admission(path, &mut |dir| {
            check_pathwalk_search_permission(dir, credentials)
        })
    }

    fn resolve_no_follow_dac(
        &self,
        path: impl AsRef<FsPath>,
        credentials: &DacCredentialView,
    ) -> AxResult<Location> {
        self.resolve_no_follow_with_admission(path, &mut |dir| {
            check_pathwalk_search_permission(dir, credentials)
        })
    }
}

/// Frozen-credential pathwalk admission for namespace mutations.
///
/// The DAC-only resolver remains available to paths that have not yet joined
/// the typed security vertical slice. New mutation hooks use these methods so
/// every traversed directory, including directories reached through symlink
/// targets, is admitted with the same actor and module-state vector that will
/// authorize the final namespace change.
pub(crate) trait SecurityFsContextExt {
    fn resolve_security(
        &self,
        path: impl AsRef<FsPath>,
        security: &VfsSecurityContext,
    ) -> AxResult<Location>;

    fn resolve_no_follow_security(
        &self,
        path: impl AsRef<FsPath>,
        security: &VfsSecurityContext,
    ) -> AxResult<Location>;

    fn resolve_security_unobserved(
        &self,
        path: impl AsRef<FsPath>,
        security: &VfsSecurityContext,
    ) -> AxResult<Location>;

    fn resolve_no_follow_security_unobserved(
        &self,
        path: impl AsRef<FsPath>,
        security: &VfsSecurityContext,
    ) -> AxResult<Location>;

    fn resolve_named_create_security<'a>(
        &self,
        path: &'a FsPath,
        security: &VfsSecurityContext,
        terminal_type: NamedCreateTerminalType,
    ) -> AxResult<(Location, &'a FsName)>;

    fn resolve_parent_preserving_final_security<'a>(
        &self,
        path: &'a FsPath,
        security: &VfsSecurityContext,
    ) -> AxResult<(Location, FinalComponent<'a>)>;
}

/// Linux terminal-component policy for a named create operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NamedCreateTerminalType {
    /// A missing normal component may carry trailing-directory intent, as in
    /// `mkdir("new/")`.
    Directory,
    /// Trailing-directory intent is a lookup, not a creatable name. A missing
    /// component is `ENOENT`; any existing covered entry is `EEXIST`.
    NonDirectory,
}

impl SecurityFsContextExt for FsContext {
    fn resolve_security(
        &self,
        path: impl AsRef<FsPath>,
        security: &VfsSecurityContext,
    ) -> AxResult<Location> {
        self.resolve_with_admission(path, &mut |dir| {
            check_pathwalk_search_permission_with_security(
                dir,
                security.actor(),
                security.credentials(),
                security.filesystem_owner_user_ns(),
            )
        })
    }

    fn resolve_no_follow_security(
        &self,
        path: impl AsRef<FsPath>,
        security: &VfsSecurityContext,
    ) -> AxResult<Location> {
        self.resolve_no_follow_with_admission(path, &mut |dir| {
            check_pathwalk_search_permission_with_security(
                dir,
                security.actor(),
                security.credentials(),
                security.filesystem_owner_user_ns(),
            )
        })
    }

    fn resolve_security_unobserved(
        &self,
        path: impl AsRef<FsPath>,
        security: &VfsSecurityContext,
    ) -> AxResult<Location> {
        self.resolve_with_admission_unobserved(path, &mut |dir| {
            check_pathwalk_search_permission_with_security(
                dir,
                security.actor(),
                security.credentials(),
                security.filesystem_owner_user_ns(),
            )
        })
    }

    fn resolve_no_follow_security_unobserved(
        &self,
        path: impl AsRef<FsPath>,
        security: &VfsSecurityContext,
    ) -> AxResult<Location> {
        self.resolve_no_follow_with_admission_unobserved(path, &mut |dir| {
            check_pathwalk_search_permission_with_security(
                dir,
                security.actor(),
                security.credentials(),
                security.filesystem_owner_user_ns(),
            )
        })
    }

    fn resolve_named_create_security<'a>(
        &self,
        path: &'a FsPath,
        security: &VfsSecurityContext,
        terminal_type: NamedCreateTerminalType,
    ) -> AxResult<(Location, &'a FsName)> {
        let (parent, final_component) =
            self.resolve_parent_preserving_final_security(path, security)?;
        let FinalComponentKind::Normal(name) = final_component.kind() else {
            return Err(AxError::AlreadyExists);
        };
        if final_component.requires_directory()
            && terminal_type == NamedCreateTerminalType::NonDirectory
        {
            return match parent.lookup_no_follow_in_mount(name) {
                Ok(_) => Err(AxError::AlreadyExists),
                Err(AxError::NotFound) => Err(AxError::NotFound),
                Err(error) => Err(error),
            };
        }
        Ok((parent, name))
    }

    fn resolve_parent_preserving_final_security<'a>(
        &self,
        path: &'a FsPath,
        security: &VfsSecurityContext,
    ) -> AxResult<(Location, FinalComponent<'a>)> {
        let (_, final_component) = path.split_final_component().ok_or(AxError::NotFound)?;
        if matches!(
            final_component.kind(),
            axfs_ng_vfs::path::FinalComponentKind::Root
        ) {
            // Linux's pure-root parent walk returns without may_lookup on the
            // root itself. Keep the mount identity for rename EXDEV ordering,
            // but do not invent an inode_permission/search hook.
            return Ok((self.root_dir().clone(), final_component));
        }
        self.resolve_parent_preserving_final_with_admission(path, &mut |dir| {
            check_pathwalk_search_permission_with_security(
                dir,
                security.actor(),
                security.credentials(),
                security.filesystem_owner_user_ns(),
            )
        })
    }
}

pub(crate) fn check_search_permissions_with_security(
    loc: &Location,
    security: &VfsSecurityContext,
) -> AxResult {
    check_pathwalk_search_permission_with_security(
        loc,
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
    )
}

/// Authorizes a directory selected by `fchdir(2)`.
///
/// The Linux request is `MAY_EXEC | MAY_CHDIR`: DAC receives execute/search
/// access and the typed security hook receives the additional `MAY_CHDIR`
/// operation intent.  `VfsSecurityContext` carries the filesystem owner
/// namespace exposed by this kernel's mounts; idmapped mounts are not
/// implemented, so there is no synthetic idmap to apply here.
pub(crate) fn check_fchdir_permissions_with_security(
    loc: &Location,
    security: &VfsSecurityContext,
) -> AxResult {
    let metadata = idmapped_metadata_for_actor(loc, &loc.metadata()?, security)?;
    check_dac_permissions_with_actor(
        metadata.mode.bits() as u32,
        metadata.uid,
        metadata.gid,
        metadata.node_type,
        X_OK,
        security.actor(),
        security.credentials(),
    )?;
    let object = InodeSecurityRef::new(loc, &metadata);
    dispatch_inode_permission(&InodePermissionSecurityContext::new_for_operation(
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
        &object,
        InodePermissionAccess::EXECUTE,
        InodePermissionOperation::FchdirMayChdir,
    ))
}

/// Computes Linux `vfs_prepare_mode()` plus `inode_init_owner()` attributes
/// for one named inode before the generic VFS publishes its name.
///
/// The SGID permission check intentionally precedes umask, as it does in
/// Linux. Directories under an SGID parent always inherit that bit; regular
/// and special files only lose an executable SGID request when the caller is
/// neither in the parent group nor capable of preserving set-id bits.
pub(crate) fn initial_named_create_owner_mode(
    parent: &Metadata,
    credentials: &DacCredentialView,
    node_type: NodeType,
    requested_mode: NodePermission,
    umask: u32,
) -> (NodePermission, (u32, u32)) {
    initial_named_create_owner_mode_with(
        parent,
        KernelDacCredentials::snapshot(credentials),
        node_type,
        requested_mode,
        umask,
    )
}

pub(crate) fn initial_named_create_owner_mode_with_security(
    parent: &Metadata,
    security: &VfsSecurityContext,
    node_type: NodeType,
    requested_mode: NodePermission,
    umask: u32,
) -> (NodePermission, (u32, u32)) {
    initial_named_create_owner_mode_with(
        parent,
        KernelDacCredentials::actor_bound(security.actor(), security.credentials()),
        node_type,
        requested_mode,
        umask,
    )
}

/// Computes create ownership in the mount's visible id space and translates
/// the resulting uid/gid back to filesystem ids before the backend sees it.
/// This is the counterpart to DAC/stat's outside-to-inside projection: an
/// idmapped mount must never persist the caller-visible ids verbatim.
pub(crate) fn initial_named_create_owner_mode_with_security_at(
    parent_location: &Location,
    parent: &Metadata,
    security: &VfsSecurityContext,
    node_type: NodeType,
    requested_mode: NodePermission,
    umask: u32,
) -> AxResult<(NodePermission, (u32, u32))> {
    let visible_parent = idmapped_metadata_for_actor(parent_location, parent, security)?;
    let (mode, (uid, gid)) = initial_named_create_owner_mode_with_security(
        &visible_parent,
        security,
        node_type,
        requested_mode,
        umask,
    );
    let Some(idmap) = security.idmap_for(parent_location)? else {
        return Ok((mode, (uid, gid)));
    };
    if security.actor().user_ns().identity().into_raw()
        != idmap.user_namespace().identity().into_raw()
    {
        return Err(AxError::PermissionDenied);
    }
    let outside = |id: u32, rows: &[crate::mounts::MountIdmapRange]| {
        rows.iter()
            .find_map(|row| {
                let end = row.inside.checked_add(row.length)?;
                (id >= row.inside && id < end)
                    .then_some(row.outside.checked_add(id - row.inside))
                    .flatten()
            })
            .ok_or(AxError::InvalidInput)
    };
    Ok((mode, (outside(uid, &idmap.uid)?, outside(gid, &idmap.gid)?)))
}

/// Converts a chown request expressed in the caller-visible idmapped mount
/// space into the filesystem's persistent ids.  `None` deliberately remains
/// omitted, preserving chown(-1, ...) semantics.
pub(crate) fn idmapped_chown_ids(
    location: &Location,
    security: &VfsSecurityContext,
    uid: Option<Kuid>,
    gid: Option<Kgid>,
) -> AxResult<(Option<Kuid>, Option<Kgid>)> {
    let Some(idmap) = security.idmap_for(location)? else {
        return Ok((uid, gid));
    };
    if security.actor().user_ns().identity().into_raw()
        != idmap.user_namespace().identity().into_raw()
    {
        return Err(AxError::PermissionDenied);
    }
    let outside = |id: u32, rows: &[crate::mounts::MountIdmapRange]| {
        rows.iter()
            .find_map(|row| {
                let end = row.inside.checked_add(row.length)?;
                (id >= row.inside && id < end)
                    .then_some(row.outside.checked_add(id - row.inside))
                    .flatten()
            })
            .ok_or(AxError::InvalidInput)
    };
    let uid = uid
        .map(|value| {
            Kuid::from_raw(outside(value.into_raw(), &idmap.uid)?).ok_or(AxError::InvalidInput)
        })
        .transpose()?;
    let gid = gid
        .map(|value| {
            Kgid::from_raw(outside(value.into_raw(), &idmap.gid)?).ok_or(AxError::InvalidInput)
        })
        .transpose()?;
    Ok((uid, gid))
}

fn initial_named_create_owner_mode_with(
    parent: &Metadata,
    credentials: KernelDacCredentials<'_>,
    node_type: NodeType,
    requested_mode: NodePermission,
    umask: u32,
) -> (NodePermission, (u32, u32)) {
    let attributes = linux_initial_create_attributes(
        &linux_node_metadata(
            parent.mode.bits() as u32,
            parent.uid,
            parent.gid,
            parent.node_type,
        ),
        linux_node_kind(node_type),
        requested_mode.bits(),
        umask as u16,
        &credentials,
    );
    (
        NodePermission::from_bits_truncate(attributes.mode),
        (attributes.user, attributes.group),
    )
}

fn check_sticky_delete_permissions_with_metadata(
    dir_stat: &Metadata,
    target_stat: &Metadata,
    security: &VfsSecurityContext,
) -> AxResult {
    check_linux_sticky_mutation(
        &linux_node_metadata(
            dir_stat.mode.bits() as u32,
            dir_stat.uid,
            dir_stat.gid,
            dir_stat.node_type,
        ),
        &linux_node_metadata(
            target_stat.mode.bits() as u32,
            target_stat.uid,
            target_stat.gid,
            target_stat.node_type,
        ),
        &KernelDacCredentials::actor_bound(security.actor(), security.credentials()),
    )
    .map_err(map_dac_error)
}

/// Applies the currently representable Linux `may_delete` parent admission
/// with one frozen operation credential. This includes writable-mount,
/// parent write/search DAC plus `inode_permission`, and sticky-directory
/// policy. Victim type and mechanism capability are checked by the namespace
/// transaction before the distinct unlink/rmdir inode hook.
pub(crate) fn check_remove_permissions_with_security(
    dir: &Location,
    dir_metadata: &Metadata,
    target_metadata: &Metadata,
    security: &VfsSecurityContext,
) -> AxResult {
    check_writable_mount(dir)?;
    check_inode_permissions_with_metadata(
        dir,
        dir_metadata,
        W_OK | X_OK,
        InodePermissionAccess::WRITE | InodePermissionAccess::EXECUTE,
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
    )?;
    check_sticky_delete_permissions_with_metadata(dir_metadata, target_metadata, security)
}

/// Applies the old and new parent portions of Linux `vfs_rename()` admission
/// with one frozen actor and one set of metadata snapshots.
///
/// The old side is always `may_delete`. The destination side is `may_create`
/// when absent and `may_delete` when replacing an existing entry. Type
/// compatibility is intentionally left to the transaction after this helper,
/// because Linux performs destination parent DAC/sticky checks before
/// returning `EISDIR`/`ENOTDIR`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_rename_parent_permissions_with_security(
    old_dir: &Location,
    old_dir_metadata: &Metadata,
    source_metadata: &Metadata,
    new_dir: &Location,
    new_dir_metadata: &Metadata,
    replaced_metadata: Option<&Metadata>,
    security: &VfsSecurityContext,
) -> AxResult {
    check_remove_permissions_with_security(old_dir, old_dir_metadata, source_metadata, security)?;
    if let Some(replaced_metadata) = replaced_metadata {
        check_remove_permissions_with_security(
            new_dir,
            new_dir_metadata,
            replaced_metadata,
            security,
        )?;
    } else {
        check_writable_mount(new_dir)?;
        check_inode_permissions_with_metadata(
            new_dir,
            new_dir_metadata,
            W_OK | X_OK,
            InodePermissionAccess::WRITE | InodePermissionAccess::EXECUTE,
            security.actor(),
            security.credentials(),
            security.filesystem_owner_user_ns(),
        )?;
    }
    Ok(())
}

/// Checks the moved directory inode itself when rename changes its parent.
///
/// Linux performs this after confirming the backend has a rename operation
/// and before `security_inode_rename()` so the transaction owns that ordering.
pub(crate) fn check_cross_parent_rename_source_permissions_with_security(
    old_dir: &Location,
    new_dir: &Location,
    source: &Location,
    source_metadata: &Metadata,
    security: &VfsSecurityContext,
) -> AxResult<()> {
    if source_metadata.node_type != NodeType::Directory || old_dir.same_node(new_dir) {
        return Ok(());
    }
    check_inode_permissions_with_metadata(
        source,
        source_metadata,
        W_OK,
        InodePermissionAccess::WRITE,
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
    )
}

pub(crate) fn check_open_permissions(
    loc: &Location,
    mask: u32,
    credentials: &DacCredentialView,
) -> AxResult {
    if mask == 0 {
        return Ok(());
    }

    let stat = loc.metadata()?;
    check_dac_permissions(
        stat.mode.bits() as u32,
        stat.uid,
        stat.gid,
        stat.node_type,
        mask,
        credentials,
    )
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use thekernel_linux_cred::{FsCredentialSnapshot, GroupInfo, Kgid, Kuid};

    use super::*;
    use crate::task::{Cred, UserNamespace};

    fn credentials(uid: u32, gid: u32, groups: &[u32], capabilities: &[u32]) -> DacCredentialView {
        let mut effective = [0; 2];
        for &capability in capabilities {
            let word = capability as usize / u32::BITS as usize;
            effective[word] |= 1 << (capability % u32::BITS);
        }
        let mut supplementary_groups = Vec::new();
        supplementary_groups
            .try_reserve_exact(groups.len())
            .unwrap();
        for &group in groups {
            supplementary_groups.push(Kgid::from_raw(group).unwrap());
        }
        FsCredentialSnapshot::new(
            Kuid::from_raw(uid).unwrap(),
            Kgid::from_raw(gid).unwrap(),
            GroupInfo::try_new(supplementary_groups).unwrap(),
            effective,
            true,
        )
    }

    fn directory_metadata(mode: u16, uid: u32, gid: u32) -> Metadata {
        Metadata {
            device: 1,
            inode: 1,
            nlink: 1,
            mode: NodePermission::from_bits_truncate(mode),
            node_type: NodeType::Directory,
            uid,
            gid,
            project_id: 0,
            size: 0,
            block_size: 4096,
            blocks: 0,
            rdev: axfs_ng_vfs::DeviceId(0),
            atime: Timestamp::ZERO,
            btime: Timestamp::ZERO,
            mtime: Timestamp::ZERO,
            ctime: Timestamp::ZERO,
        }
    }

    #[test]
    fn owner_class_does_not_inherit_other_permissions() {
        let credentials = credentials(1000, 100, &[], &[]);
        assert!(!dac_access_allowed(
            0o004,
            1000,
            200,
            NodeType::RegularFile,
            R_OK,
            &credentials,
        ));
    }

    #[test]
    fn group_class_does_not_inherit_other_permissions() {
        let credentials = credentials(1000, 200, &[], &[]);
        assert!(!dac_access_allowed(
            0o004,
            3000,
            200,
            NodeType::RegularFile,
            R_OK,
            &credentials,
        ));
    }

    #[test]
    fn uid_zero_without_effective_dac_capabilities_is_not_privileged() {
        let credentials = credentials(0, 0, &[], &[]);
        assert!(!dac_access_allowed(
            0,
            1000,
            100,
            NodeType::RegularFile,
            R_OK,
            &credentials,
        ));
        assert!(!dac_access_allowed(
            0,
            1000,
            100,
            NodeType::Directory,
            X_OK,
            &credentials,
        ));
    }

    #[test]
    fn read_search_does_not_override_write_permissions() {
        let credentials = credentials(0, 0, &[], &[CAP_DAC_READ_SEARCH]);
        assert!(dac_access_allowed(
            0,
            1000,
            100,
            NodeType::RegularFile,
            R_OK,
            &credentials,
        ));
        assert!(dac_access_allowed(
            0,
            1000,
            100,
            NodeType::Directory,
            X_OK,
            &credentials,
        ));
        assert!(!dac_access_allowed(
            0,
            1000,
            100,
            NodeType::RegularFile,
            W_OK,
            &credentials,
        ));
    }

    #[test]
    fn override_requires_an_execute_bit_for_regular_files() {
        let credentials = credentials(0, 0, &[], &[CAP_DAC_OVERRIDE]);
        assert!(dac_access_allowed(
            0,
            1000,
            100,
            NodeType::RegularFile,
            R_OK | W_OK,
            &credentials,
        ));
        assert!(!dac_access_allowed(
            0,
            1000,
            100,
            NodeType::RegularFile,
            X_OK,
            &credentials,
        ));
        assert!(dac_access_allowed(
            0o001,
            1000,
            100,
            NodeType::RegularFile,
            X_OK,
            &credentials,
        ));
    }

    #[test]
    fn synthetic_dac_projection_is_not_rebound_to_live_actor_effective_state() {
        let root_namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(root_namespace.clone()).unwrap();
        let child_namespace = root_namespace
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let child = Cred::try_with_user_namespace(&root, child_namespace).unwrap();
        let synthetic = credentials(0, 0, &[], &[CAP_DAC_OVERRIDE]);

        assert!(
            KernelDacCredentials::snapshot(&synthetic).has_capability(&(), DacCapability::Override)
        );
        assert!(
            !KernelDacCredentials::actor_bound(&child, &synthetic)
                .has_capability(&(), DacCapability::Override)
        );
    }

    #[test]
    fn sgid_parent_attributes_follow_linux_prepare_then_owner_order() {
        let parent = directory_metadata(0o2770, 10, 200);
        let unprivileged = credentials(1000, 100, &[], &[]);

        let (regular_mode, regular_owner) = initial_named_create_owner_mode(
            &parent,
            &unprivileged,
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o2670),
            0o020,
        );
        assert_eq!(regular_owner, (1000, 200));
        assert_eq!(regular_mode.bits(), 0o650);

        let (directory_mode, directory_owner) = initial_named_create_owner_mode(
            &parent,
            &unprivileged,
            NodeType::Directory,
            NodePermission::from_bits_truncate(0o770),
            0o027,
        );
        assert_eq!(directory_owner, (1000, 200));
        assert_eq!(directory_mode.bits(), 0o2750);
    }

    #[test]
    fn cap_fsetid_preserves_executable_sgid_request() {
        let parent = directory_metadata(0o2770, 10, 200);
        let capable = credentials(1000, 100, &[], &[CAP_FSETID]);
        let (mode, owner) = initial_named_create_owner_mode(
            &parent,
            &capable,
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o2670),
            0,
        );
        assert_eq!(owner, (1000, 200));
        assert_eq!(mode.bits(), 0o2670);
    }

    #[test]
    fn unix_socket_mode_uses_umask_and_sgid_parent_group() {
        let parent = directory_metadata(0o2770, 10, 200);
        let caller = credentials(1000, 100, &[300], &[]);
        let (mode, owner) = initial_named_create_owner_mode(
            &parent,
            &caller,
            NodeType::Socket,
            NodePermission::from_bits_truncate(0o777),
            0o027,
        );
        assert_eq!(mode.bits(), 0o750);
        assert_eq!(owner, (1000, 200));
    }
}
