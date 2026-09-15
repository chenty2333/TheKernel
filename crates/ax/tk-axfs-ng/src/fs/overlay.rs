//! Overlayfs mount admission over byte-preserving VFS paths.
//!
//! Overlayfs is a composed filesystem, so accepting a text `lowerdir=` option
//! is not enough.  Before an overlay superblock is constructed the caller
//! must resolve every directory in the caller's mount namespace and pass the
//! resulting [`Location`]s here.  This keeps the validation on object
//! identities rather than lossy path strings: rename, bind mounts, and a
//! non-UTF-8 name cannot retarget a prepared upper/work transaction.
//!
//! The implementation is mounted through the regular fscontext registry.  Its
//! writable path never exposes a lower inode for mutation: metadata, xattrs,
//! file attributes and data all first materialize a durable upper object.

use alloc::{sync::Arc, vec::Vec};
use core::{
    any::Any,
    sync::atomic::{AtomicU64, Ordering},
    task::Context,
};

use axerrno::LinuxError;
use axfs_ng_vfs::{
    CreateDisposition, DirEntry, DirEntrySink, DirNode, DirNodeOps, ExportHandle,
    ExportHandleDecodeMode, ExportHandleMode, FileLock, FileNode, FileNodeOps, FileRangeRequest,
    Filesystem, FilesystemOps, FsName, FsNameBuf, FsPath, FsPathBuf, Location, LockOps, Metadata,
    MetadataUpdate, NodeFlags, NodeOps, NodePermission, NodeType, NodeUserData, ObjectKey,
    QuotaOps, QuotaUsage, Reference, RenameRequest, StatFs, UnlinkRequest, VfsError, VfsResult,
    XattrSetMode,
};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use axsync::Mutex;
use hashbrown::HashSet;

/// Linux bounds the number of lower layers to avoid unbounded recursive
/// lookup/copy-up state.  Keep the bound explicit in the provider rather than
/// relying on a caller-side allocation limit.
pub const OVERLAY_MAX_LAYERS: usize = 500;

/// Persistent overlay feature selection.  Each member has a real on-disk
/// consequence, so unknown options are rejected by the parser instead of
/// being silently retained for a future implementation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OverlayFeatures {
    pub redirect_dir: bool,
    pub index: bool,
    pub xino: bool,
    pub metacopy: bool,
    pub nfs_export: bool,
    pub volatile: bool,
}

/// Raw, unresolved fsconfig values.  Resolution is intentionally separate:
/// it has to run under the calling mount namespace and credential/idmap
/// context, which this no-std filesystem crate does not own.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverlayMountOptions {
    pub lowerdirs: Vec<FsPathBuf>,
    pub upperdir: Option<FsPathBuf>,
    pub workdir: Option<FsPathBuf>,
    pub features: OverlayFeatures,
}

impl OverlayMountOptions {
    pub fn empty() -> Self {
        Self {
            lowerdirs: Vec::new(),
            upperdir: None,
            workdir: None,
            features: OverlayFeatures::default(),
        }
    }

    /// Applies one raw fsconfig string option.  `lowerdir` retains Linux's
    /// colon-separated list syntax, including `\\:` and `\\\\` escapes; all
    /// other pathname options are one raw byte path.  Values containing NUL,
    /// relative paths, duplicate singleton keys, and unknown boolean forms
    /// fail before any namespace mutation.
    pub fn set_option(&mut self, key: &[u8], value: &[u8]) -> VfsResult<()> {
        match key {
            b"lowerdir" => {
                if !self.lowerdirs.is_empty() {
                    return Err(VfsError::InvalidInput);
                }
                self.lowerdirs = parse_lowerdirs(value)?;
            }
            b"upperdir" => set_unique_path(&mut self.upperdir, value)?,
            b"workdir" => set_unique_path(&mut self.workdir, value)?,
            b"redirect_dir" => self.features.redirect_dir = parse_on_off(value)?,
            b"index" => self.features.index = parse_on_off(value)?,
            b"xino" => self.features.xino = parse_on_off(value)?,
            b"metacopy" => self.features.metacopy = parse_on_off(value)?,
            b"nfs_export" => self.features.nfs_export = parse_on_off(value)?,
            b"volatile" => self.features.volatile = parse_on_off(value)?,
            _ => return Err(VfsError::InvalidInput),
        }
        Ok(())
    }

    /// Checks the configuration combinations independent of live dentries.
    pub fn validate_shape(&self) -> VfsResult<()> {
        if self.lowerdirs.is_empty() || self.lowerdirs.len() > OVERLAY_MAX_LAYERS {
            return Err(VfsError::InvalidInput);
        }
        if self.upperdir.is_some() != self.workdir.is_some() {
            return Err(VfsError::InvalidInput);
        }
        // metacopy needs an upper metadata inode; nfs_export depends on a
        // persistent index, and volatile explicitly cannot promise it.
        if self.features.metacopy && self.upperdir.is_none()
            || self.features.nfs_export && (!self.features.index || self.features.volatile)
            || self.features.volatile && self.features.nfs_export
        {
            return Err(VfsError::InvalidInput);
        }
        Ok(())
    }
}

/// Resolved directories retained while the overlay mount is live.  Validation
/// does not publish anything: the node/copy-up implementation receives this
/// only after all checks below pass, then performs its own prepared commit.
#[derive(Clone)]
pub struct OverlayTopology {
    pub lower: Vec<Location>,
    pub upper: Option<Location>,
    pub work: Option<Location>,
    pub features: OverlayFeatures,
    /// Maps filesystem IDs from the resolved lower mount view to the upper
    /// mount view.  Kept provider-local so axfs does not depend on the kernel
    /// namespace crate; mount admission supplies the real MountIdmap adapter.
    pub id_mapper: Arc<dyn OverlayIdMapper>,
}

pub trait OverlayIdMapper: Send + Sync {
    /// Projects an id as exposed by this exact lower mount view through the
    /// kernel id space and into the upper mount view.  The source location is
    /// part of the operation: an overlay may have many lower mounts, each
    /// with a different idmap, so choosing the first layer is not sound.
    fn lower_uid_to_upper(&self, lower: &Location, uid: u32) -> VfsResult<u32>;
    fn lower_gid_to_upper(&self, lower: &Location, gid: u32) -> VfsResult<u32>;
    fn lower_kernel_uid_to_visible(&self, lower: &Location, uid: u32) -> VfsResult<u32>;
    fn lower_kernel_gid_to_visible(&self, lower: &Location, gid: u32) -> VfsResult<u32>;
    fn upper_visible_uid_to_kernel(&self, uid: u32) -> VfsResult<u32>;
    fn upper_visible_gid_to_kernel(&self, gid: u32) -> VfsResult<u32>;
}

#[derive(Default)]
pub struct IdentityOverlayIdMapper;
impl OverlayIdMapper for IdentityOverlayIdMapper {
    fn lower_uid_to_upper(&self, _lower: &Location, uid: u32) -> VfsResult<u32> {
        Ok(uid)
    }
    fn lower_gid_to_upper(&self, _lower: &Location, gid: u32) -> VfsResult<u32> {
        Ok(gid)
    }
    fn lower_kernel_uid_to_visible(&self, _lower: &Location, uid: u32) -> VfsResult<u32> {
        Ok(uid)
    }
    fn lower_kernel_gid_to_visible(&self, _lower: &Location, gid: u32) -> VfsResult<u32> {
        Ok(gid)
    }
    fn upper_visible_uid_to_kernel(&self, uid: u32) -> VfsResult<u32> {
        Ok(uid)
    }
    fn upper_visible_gid_to_kernel(&self, gid: u32) -> VfsResult<u32> {
        Ok(gid)
    }
}

impl OverlayTopology {
    pub fn try_new(
        options: &OverlayMountOptions,
        lower: Vec<Location>,
        upper: Option<Location>,
        work: Option<Location>,
    ) -> VfsResult<Self> {
        Self::try_new_with_id_mapper(
            options,
            lower,
            upper,
            work,
            Arc::new(IdentityOverlayIdMapper),
        )
    }

    pub fn try_new_with_id_mapper(
        options: &OverlayMountOptions,
        lower: Vec<Location>,
        upper: Option<Location>,
        work: Option<Location>,
        id_mapper: Arc<dyn OverlayIdMapper>,
    ) -> VfsResult<Self> {
        options.validate_shape()?;
        if lower.len() != options.lowerdirs.len()
            || lower.is_empty()
            || lower.len() > OVERLAY_MAX_LAYERS
        {
            return Err(VfsError::InvalidInput);
        }
        if upper.is_some() != options.upperdir.is_some()
            || work.is_some() != options.workdir.is_some()
        {
            return Err(VfsError::InvalidInput);
        }
        for layer in &lower {
            layer.check_is_dir()?;
        }
        if let Some(upper) = &upper {
            upper.check_is_dir()?;
        }
        if let Some(work) = &work {
            work.check_is_dir()?;
        }

        // A lower layer must not be the same directory or lie below either
        // mutable staging directory.  Otherwise copy-up can recursively see
        // its own temporary names after a rename or bind mount change.
        for (index, layer) in lower.iter().enumerate() {
            for previous in &lower[..index] {
                if layer.same_mount(previous) && layer.same_node(previous) {
                    return Err(VfsError::InvalidInput);
                }
            }
            if let Some(upper) = &upper
                && overlaps(layer, upper)?
            {
                return Err(VfsError::InvalidInput);
            }
            if let Some(work) = &work
                && overlaps(layer, work)?
            {
                return Err(VfsError::InvalidInput);
            }
        }
        if let (Some(upper), Some(work)) = (&upper, &work) {
            // Linux requires upper/work to be distinct directories in the
            // same superblock; object identity is insufficient for bind views,
            // so compare mounted filesystem device as well.
            if overlaps(upper, work)? || upper.metadata()?.device != work.metadata()?.device {
                return Err(VfsError::InvalidInput);
            }
        }
        Ok(Self {
            lower,
            upper,
            work,
            features: options.features,
            id_mapper,
        })
    }

    pub fn is_read_only(&self) -> bool {
        self.upper.is_none()
    }
}

/// A backend-owned copy-up transaction.  The staging object is deliberately
/// opaque: callers can neither derive a temporary pathname nor publish it by
/// calling a generic rename after a failed copy.  That makes the only visible
/// transition the backend's checked `publish` below.
pub trait OverlayCopyUpBackend {
    type Staged;

    /// Creates a non-visible upper object with the lower object's type,
    /// metadata, idmapped ownership and native project/file attributes.  The
    /// returned object must not be reachable through the merged namespace.
    fn prepare(
        &self,
        upper_parent: &Location,
        name: &FsName,
        lower: &Location,
        origin: axfs_ng_vfs::ObjectKey,
    ) -> VfsResult<Self::Staged>;

    /// Copies data, symlink contents, and every eligible xattr into staging.
    /// Whiteout/opaque/internal overlay xattrs are backend-owned and must not
    /// be copied from an untrusted lower filesystem through this hook.
    fn copy_contents(&self, staged: &mut Self::Staged, lower: &Location) -> VfsResult<()>;

    /// Flushes both object data and the staging directory before publication.
    /// A successful return is the durability boundary for a later atomic
    /// publication, not merely a cache writeback request.
    fn sync_staging(&self, staged: &Self::Staged) -> VfsResult<()>;

    /// Atomically installs the staged object at `name`, after revalidating the
    /// expected lower/upper namespace generation supplied by the caller.  It
    /// must install origin/index metadata before the name becomes visible.
    fn publish(
        &self,
        staged: Self::Staged,
        upper_parent: &Location,
        name: &FsName,
        lower: &Location,
    ) -> VfsResult<Location>;

    /// Aborts an unpublished staging transaction.  Implementations must make
    /// it idempotent: copy or flush failures may race teardown/recovery.
    fn abort(&self, staged: Self::Staged);

    /// Flushes the directory transaction that made a copy-up or whiteout
    /// visible.  A failure is reported to the caller and retained by the
    /// provider's writeback-error sequence; it is never converted to success.
    fn sync_publication(&self, upper_parent: &Location) -> VfsResult<()>;
}

/// Executes the non-visible-copy → durable-staging → checked-publish sequence
/// used by writes, metadata changes, xattr changes, link, and rename.  The
/// provider supplies namespace serialization/revalidation through `publish`;
/// this helper ensures every pre-publication failure destroys staging and can
/// never leave a visible half-copied upper inode.
pub fn copy_up<B: OverlayCopyUpBackend>(
    backend: &B,
    upper_parent: &Location,
    name: &FsName,
    lower: &Location,
) -> VfsResult<Location> {
    let mut staged = backend.prepare(upper_parent, name, lower, lower.object_key())?;
    if let Err(error) = backend.copy_contents(&mut staged, lower) {
        backend.abort(staged);
        return Err(error);
    }
    if let Err(error) = backend.sync_staging(&staged) {
        backend.abort(staged);
        return Err(error);
    }
    let published = backend.publish(staged, upper_parent, name, lower)?;
    backend.sync_publication(upper_parent)?;
    Ok(published)
}

fn overlaps(left: &Location, right: &Location) -> VfsResult<bool> {
    if left.same_mount(right) {
        return Ok(left.is_same_or_ancestor_of(right) || right.is_same_or_ancestor_of(left));
    }
    // Distinct bind views can expose the same mutable directory tree.  Ask
    // the backend using stable export identities rather than comparing paths;
    // if it cannot establish the relation the caller rejects the mount.
    let same_filesystem = left.mountpoint().filesystem_handle().identity().device()
        == right.mountpoint().filesystem_handle().identity().device();
    if !same_filesystem {
        return Ok(false);
    }
    Ok(left.is_same_or_ancestor_in_filesystem(right)?
        || right.is_same_or_ancestor_in_filesystem(left)?)
}

fn set_unique_path(slot: &mut Option<FsPathBuf>, bytes: &[u8]) -> VfsResult<()> {
    if slot.is_some() {
        return Err(VfsError::InvalidInput);
    }
    *slot = Some(parse_absolute_path(bytes)?);
    Ok(())
}

fn parse_on_off(value: &[u8]) -> VfsResult<bool> {
    match value {
        b"on" => Ok(true),
        b"off" => Ok(false),
        _ => Err(VfsError::InvalidInput),
    }
}

fn parse_absolute_path(bytes: &[u8]) -> VfsResult<FsPathBuf> {
    if bytes.is_empty() || bytes.contains(&0) || !FsPath::new(bytes).is_absolute() {
        return Err(VfsError::InvalidInput);
    }
    Ok(FsPathBuf::from_vec(bytes.to_vec()))
}

fn parse_lowerdirs(value: &[u8]) -> VfsResult<Vec<FsPathBuf>> {
    if value.is_empty() || value.contains(&0) {
        return Err(VfsError::InvalidInput);
    }
    let mut result = Vec::new();
    result.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
    let mut part = Vec::new();
    let mut escaped = false;
    for byte in value {
        if escaped {
            if *byte != b':' && *byte != b'\\' {
                return Err(VfsError::InvalidInput);
            }
            part.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            part.push(*byte);
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b':' {
            result.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            result.push(parse_absolute_path(&part)?);
            part.clear();
        } else {
            part.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            part.push(*byte);
        }
    }
    if escaped {
        return Err(VfsError::InvalidInput);
    }
    result.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
    result.push(parse_absolute_path(&part)?);
    if result.len() > OVERLAY_MAX_LAYERS {
        return Err(VfsError::InvalidInput);
    }
    Ok(result)
}

// The adapter below deliberately has no global mount registry entry.  Its
// constructor is called only after the kernel fscontext has resolved every
// directory under its current namespace/idmap.  That is important: overlayfs
// owns a composed namespace, not a block device that `new_named` can safely
// manufacture from an arbitrary `MountedBlockDevice`.

const OVERLAY_WHITEOUT: &[u8] = b"trusted.overlay.whiteout";
const OVERLAY_OPAQUE: &[u8] = b"trusted.overlay.opaque";
const OVERLAY_ORIGIN: &[u8] = b"trusted.overlay.origin";
const OVERLAY_REDIRECT: &[u8] = b"trusted.overlay.redirect";
const OVERLAY_INDEX: &[u8] = b"trusted.overlay.index";
const OVERLAY_INDEX_PENDING: &[u8] = b"trusted.overlay.index.pending";
const OVERLAY_INDEX_TARGET: &[u8] = b"trusted.overlay.index.target";
/// Separates a regular-file hardlink index from the regular marker used for a
/// directory (directories themselves cannot be hardlinked).  This is made
/// durable before a marker's origin or payload can survive a crash.
const OVERLAY_INDEX_KIND: &[u8] = b"trusted.overlay.index.kind";
const OVERLAY_INDEX_FILE: &[u8] = b"file";
const OVERLAY_INDEX_DIRECTORY_MARKER: &[u8] = b"directory";
/// The index lives below workdir, never in the merged namespace.  Unlike the
/// compatibility xattr above, this directory owns a durable hard-link to the
/// copied-up inode and is therefore sufficient to recover/reuse a lower
/// object after every path used to reach it has been renamed away.
const OVERLAY_INDEX_DIRECTORY: &[u8] = b".ovl.index";
const OVERLAY_EXPORT_HANDLE_TYPE: i32 = 0x4f56_4c31; // "OVL1"
/// Durable pre-whiteout journal.  A lower-directory rename writes this marker
/// and syncs it before moving the copied-up source; if power fails before the
/// physical whiteout is installed, the next overlay mount still suppresses
/// the old lower name.
const OVERLAY_TOMBSTONES: &[u8] = b"trusted.overlay.tombstones";
static OVERLAY_STAGE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Eq, PartialEq)]
enum OverlayIndexKind {
    File,
    DirectoryMarker,
}

/// Object-safe upper/work transaction backend.  It is deliberately expressed
/// in terms of resolved objects rather than paths: a namespace rename cannot
/// retarget a transaction after fsconfig resolution.  The VFS implementation
/// below is used for native local filesystems; remote providers may supply a
/// stronger implementation with server-side transactions.
pub trait OverlayWriteBackend: Send + Sync {
    fn recover(&self, work: &Location, upper: &Location) -> VfsResult<()>;
    fn copy_up(
        &self,
        work: &Location,
        upper_parent: &Location,
        name: &FsName,
        lower: &Location,
        features: OverlayFeatures,
        id_mapper: &dyn OverlayIdMapper,
        before_publish: &mut dyn FnMut(ObjectKey),
    ) -> VfsResult<Location>;
    fn create(
        &self,
        work: &Location,
        upper_parent: &Location,
        name: &FsName,
        options: &axfs_ng_vfs::NamedCreateOptions,
    ) -> VfsResult<Location>;
    /// Creates a fully initialized symlink in the private work directory and
    /// publishes it only after the upper provider has accepted every prepared
    /// attribute.  A provider must reject unsupported requested attributes
    /// before publication rather than creating a temporary or partially
    /// initialized visible link.
    fn create_symlink(
        &self,
        work: &Location,
        upper_parent: &Location,
        name: &FsName,
        target: &FsPath,
        options: &axfs_ng_vfs::NamedCreateOptions,
        prepare_visible: &mut dyn FnMut(Location) -> VfsResult<()>,
        published: &mut dyn FnMut(),
    ) -> VfsResult<()>;
    fn whiteout(&self, work: &Location, upper_parent: &Location, name: &FsName) -> VfsResult<()>;
    /// Atomically replaces a visible upper name by a prepared whiteout.  This
    /// is used after removing an upper alias which still has a lower
    /// counterpart: publishing the whiteout by a separate unlink/create
    /// sequence would resurrect the lower entry to concurrent pathwalks.
    fn replace_with_whiteout(
        &self,
        work: &Location,
        upper_parent: &Location,
        name: &FsName,
        expected: &DirEntry,
    ) -> VfsResult<()>;
    fn add_tombstone(&self, upper_parent: &Location, name: &FsName) -> VfsResult<()>;
    fn set_opaque(&self, upper_dir: &Location, opaque: bool) -> VfsResult<()>;
    fn set_redirect(&self, upper: &Location, target: &FsPath) -> VfsResult<()>;
    fn set_origin(&self, upper: &Location, lower: ObjectKey) -> VfsResult<()>;
    fn set_index(&self, upper: &Location, lower: ObjectKey) -> VfsResult<()>;
    /// Resolves the durable index identity used by overlay NFS export.  The
    /// returned entry is revalidated against its origin before it is exposed.
    fn lookup_index(&self, _work: &Location, _lower: ObjectKey) -> VfsResult<Location> {
        Err(VfsError::OperationNotSupported)
    }
    fn index_directory_identity(
        &self,
        _work: &Location,
        _upper: &Location,
        _lower: ObjectKey,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }
    fn index_target(&self, _entry: &Location) -> VfsResult<Option<ObjectKey>> {
        Ok(None)
    }
}

/// Local VFS implementation of the upper/work protocol.  A staging name is
/// visible only below `workdir`, is synced before it is renamed into upper,
/// and is removed during mount recovery.  No operation publishes an empty
/// upper inode and fills it later.
#[derive(Default)]
pub struct VfsOverlayWriteBackend;

impl VfsOverlayWriteBackend {
    fn stage_name(&self) -> VfsResult<FsNameBuf> {
        let id = OVERLAY_STAGE_ID.fetch_add(1, Ordering::Relaxed);
        let text = alloc::format!(".ovl.stage.{id:016x}");
        FsNameBuf::from_vec(text.into_bytes())
    }

    fn index_name(lower: ObjectKey) -> VfsResult<FsNameBuf> {
        // ObjectKey is generation-bound.  The fixed-width spelling avoids
        // path encoding, collisions, and any dependence on a lower pathname.
        let text = alloc::format!(
            "{:016x}{:016x}{:016x}",
            lower.filesystem,
            lower.object,
            lower.generation
        );
        FsNameBuf::from_vec(text.into_bytes())
    }

    fn encode_key(lower: ObjectKey) -> [u8; 24] {
        let mut bytes = [0u8; 24];
        bytes[..8].copy_from_slice(&lower.filesystem.to_le_bytes());
        bytes[8..16].copy_from_slice(&lower.object.to_le_bytes());
        bytes[16..].copy_from_slice(&lower.generation.to_le_bytes());
        bytes
    }

    fn decode_key(bytes: &[u8]) -> Option<ObjectKey> {
        let bytes: &[u8; 24] = bytes.try_into().ok()?;
        Some(ObjectKey::new(
            u64::from_le_bytes(bytes[..8].try_into().ok()?),
            u64::from_le_bytes(bytes[8..16].try_into().ok()?),
            u64::from_le_bytes(bytes[16..].try_into().ok()?),
        ))
    }

    /// Reads the on-disk representation discriminator.  A missing or
    /// malformed discriminator is corruption, not a legacy regular index:
    /// accepting it would let an interrupted directory-marker construction
    /// be hardlinked into upper as a file.
    fn index_kind(&self, entry: &DirEntry) -> VfsResult<OverlayIndexKind> {
        let provider = entry
            .xattr_provider()
            .ok_or(VfsError::OperationNotSupported)?;
        match provider.get_xattr(OVERLAY_INDEX_KIND) {
            Ok(value) if value.as_slice() == OVERLAY_INDEX_FILE => Ok(OverlayIndexKind::File),
            Ok(value) if value.as_slice() == OVERLAY_INDEX_DIRECTORY_MARKER => {
                Ok(OverlayIndexKind::DirectoryMarker)
            }
            Ok(_) | Err(VfsError::NotFound) => Err(VfsError::InvalidInput),
            Err(error) => Err(error),
        }
    }

    fn index_directory(&self, work: &Location) -> VfsResult<Location> {
        let work_dir = work.entry().as_dir()?;
        let name = FsName::new(OVERLAY_INDEX_DIRECTORY);
        match work_dir.lookup(name) {
            Ok(entry) if entry.is_dir() => Ok(Location::new(work.mountpoint().clone(), entry)),
            Ok(_) => Err(VfsError::InvalidInput),
            Err(VfsError::NotFound) => {
                let entry = work_dir
                    .create_named(
                        name,
                        &axfs_ng_vfs::NamedCreateOptions {
                            node_type: NodeType::Directory,
                            permission: NodePermission::default(),
                            owner: None,
                            rdev: None,
                            initial_data: None,
                            initial_attributes: Default::default(),
                        },
                        CreateDisposition::Exclusive,
                    )?
                    .entry;
                let location = Location::new(work.mountpoint().clone(), entry);
                self.set_opaque(&location, true)?;
                location.sync(false)?;
                work.sync(false)?;
                Ok(location)
            }
            Err(error) => Err(error),
        }
    }

    fn indexed_location(&self, work: &Location, lower: ObjectKey) -> VfsResult<Option<Location>> {
        let index = self.index_directory(work)?;
        let name = Self::index_name(lower)?;
        match index.entry().as_dir()?.lookup(&name) {
            Ok(entry) => {
                let location = Location::new(index.mountpoint().clone(), entry);
                let entry = location.entry();
                let xattrs = entry
                    .xattr_provider()
                    .ok_or(VfsError::OperationNotSupported)?;
                let origin = match xattrs.get_xattr(OVERLAY_ORIGIN) {
                    Ok(value) => Self::decode_key(&value),
                    Err(VfsError::NotFound) => None,
                    Err(error) => return Err(error),
                };
                let pending = match xattrs.get_xattr(OVERLAY_INDEX_PENDING) {
                    Ok(_) => true,
                    Err(VfsError::NotFound) => false,
                    Err(error) => return Err(error),
                };
                let kind = self.index_kind(entry)?;
                let marker_target = self.marker_target(entry)?;
                let well_formed = match kind {
                    OverlayIndexKind::File => !entry.is_dir() && marker_target.is_none(),
                    OverlayIndexKind::DirectoryMarker => !entry.is_dir() && marker_target.is_some(),
                };
                if origin == Some(lower) && !pending && well_formed {
                    Ok(Some(location))
                } else {
                    Err(VfsError::InvalidInput)
                }
            }
            Err(VfsError::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn reuse_index(
        &self,
        work: &Location,
        upper_parent: &Location,
        name: &FsName,
        lower: ObjectKey,
        before_publish: &mut dyn FnMut(ObjectKey),
    ) -> VfsResult<Option<Location>> {
        let Some(indexed) = self.indexed_location(work, lower)? else {
            return Ok(None);
        };
        // Directory hardlinks are forbidden.  A directory's origin remains
        // persistent metadata, but a directory copy-up is never aliased.
        if indexed.entry().is_dir() || self.marker_target(indexed.entry())?.is_some() {
            return Ok(None);
        }
        let upper = upper_parent.entry().as_dir()?;
        match upper.lookup(name) {
            Ok(entry) => {
                return Ok(Some(Location::new(
                    upper_parent.mountpoint().clone(),
                    entry,
                )));
            }
            Err(VfsError::NotFound) => {}
            Err(error) => return Err(error),
        }
        // A hardlink publication retains the indexed inode identity.  Install
        // the alias before `link` makes this upper name lookup-visible.
        let final_key = indexed.object_key();
        before_publish(final_key);
        let entry = upper.link(name, indexed.entry())?;
        if entry.object_key() != final_key {
            // Do not leave an upper name whose identity was not admitted.
            let _ = upper.unlink_checked(name, false, &entry);
            return Err(VfsError::Io);
        }
        upper_parent.sync(false)?;
        Ok(Some(Location::new(
            upper_parent.mountpoint().clone(),
            entry,
        )))
    }

    fn link_index(&self, work: &Location, staged: &Location, lower: ObjectKey) -> VfsResult<()> {
        let index = self.index_directory(work)?;
        let name = Self::index_name(lower)?;
        let directory = index.entry().as_dir()?;
        if staged.entry().is_dir() {
            // Directory hardlinks are forbidden. The index therefore owns a
            // durable regular record containing the target upper ObjectKey;
            // its name and origin xattr independently bind it to the lower.
            match directory.lookup(&name) {
                Ok(entry) => {
                    let xattrs = entry
                        .xattr_provider()
                        .ok_or(VfsError::OperationNotSupported)?;
                    let origin = match xattrs.get_xattr(OVERLAY_ORIGIN) {
                        Ok(value) => Self::decode_key(&value),
                        Err(VfsError::NotFound) => None,
                        Err(error) => return Err(error),
                    };
                    if origin == Some(lower)
                        && self.index_kind(&entry)? == OverlayIndexKind::DirectoryMarker
                        && self.marker_target(&entry)? == Some(staged.object_key())
                    {
                        return Ok(());
                    }
                    return Err(VfsError::AlreadyExists);
                }
                Err(VfsError::NotFound) => {}
                Err(error) => return Err(error),
            }
            let entry = directory
                .create_named(
                    &name,
                    &axfs_ng_vfs::NamedCreateOptions {
                        node_type: NodeType::RegularFile,
                        permission: NodePermission::default(),
                        owner: None,
                        rdev: None,
                        initial_data: None,
                        initial_attributes: Default::default(),
                    },
                    CreateDisposition::Exclusive,
                )?
                .entry;
            let marker = Location::new(index.mountpoint().clone(), entry);
            let xattrs = marker
                .entry()
                .xattr_provider()
                .ok_or(VfsError::OperationNotSupported)?;
            // `pending` is a durability barrier, not merely a logical flag:
            // sync it before the recognisable kind.  Xattr updates are not a
            // compound atomic write, so doing this in the other order could
            // leave a durable kind that recovery mistakes for publication.
            xattrs.set_xattr(OVERLAY_INDEX_PENDING, b"y", XattrSetMode::Upsert)?;
            marker.sync(false)?;
            index.sync(false)?;
            work.sync(false)?;
            // A crash after this point has an unambiguous pending directory
            // marker.  Its payload/origin cannot be reused as a file index.
            xattrs.set_xattr(
                OVERLAY_INDEX_KIND,
                OVERLAY_INDEX_DIRECTORY_MARKER,
                XattrSetMode::Upsert,
            )?;
            marker.sync(false)?;
            index.sync(false)?;
            work.sync(false)?;
            marker
                .entry()
                .as_file()?
                .write_at(&Self::encode_key(staged.object_key()), 0)?;
            self.set_origin(&marker, lower)?;
            self.set_index(&marker, lower)?;
            xattrs.set_xattr(
                OVERLAY_INDEX_TARGET,
                &Self::encode_key(staged.object_key()),
                XattrSetMode::Upsert,
            )?;
            marker.sync(false)?;
            index.sync(false)?;
            return work.sync(false);
        }
        match directory.lookup(&name) {
            Ok(entry) if entry.object_key() == staged.object_key() => {
                if self.index_kind(&entry)? == OverlayIndexKind::File
                    && self.marker_target(&entry)?.is_none()
                {
                    return Ok(());
                }
                return Err(VfsError::AlreadyExists);
            }
            Ok(_) => return Err(VfsError::AlreadyExists),
            Err(VfsError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let entry = directory.link(&name, staged.entry())?;
        let xattrs = entry
            .xattr_provider()
            .ok_or(VfsError::OperationNotSupported)?;
        // A regular index hardlink may expose the staged inode's already
        // durable origin.  Persist pending before its file discriminator so
        // no crash can make an unpublished link look committed.
        xattrs.set_xattr(OVERLAY_INDEX_PENDING, b"y", XattrSetMode::Upsert)?;
        entry.sync(false)?;
        index.sync(false)?;
        work.sync(false)?;
        xattrs.set_xattr(OVERLAY_INDEX_KIND, OVERLAY_INDEX_FILE, XattrSetMode::Upsert)?;
        entry.sync(false)?;
        index.sync(false)?;
        work.sync(false)
    }

    fn commit_index(&self, work: &Location, lower: ObjectKey) -> VfsResult<()> {
        let index = self.index_directory(work)?;
        let name = Self::index_name(lower)?;
        let entry = index.entry().as_dir()?.lookup(&name)?;
        match entry
            .xattr_provider()
            .ok_or(VfsError::OperationNotSupported)?
            .remove_xattr(OVERLAY_INDEX_PENDING)
        {
            Ok(()) | Err(VfsError::NotFound) => {}
            Err(error) => return Err(error),
        }
        entry.sync(false)?;
        index.sync(false)?;
        work.sync(false)
    }

    fn marker_target(&self, entry: &DirEntry) -> VfsResult<Option<ObjectKey>> {
        let Some(provider) = entry.xattr_provider() else {
            return Ok(None);
        };
        match provider.get_xattr(OVERLAY_INDEX_TARGET) {
            Ok(value) => {
                let target = Self::decode_key(&value).ok_or(VfsError::InvalidInput)?;
                if entry.len()? != 24 {
                    return Err(VfsError::InvalidInput);
                }
                let mut bytes = [0u8; 24];
                if entry.as_file()?.read_at(&mut bytes, 0)? != bytes.len()
                    || Self::decode_key(&bytes) != Some(target)
                {
                    return Err(VfsError::InvalidInput);
                }
                Ok(Some(target))
            }
            Err(VfsError::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn upper_contains_key(&self, upper: &Location, key: ObjectKey) -> VfsResult<bool> {
        let mut pending = Vec::new();
        pending.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        pending.push(upper.clone());
        let mut seen = HashSet::new();
        while let Some(location) = pending.pop() {
            if !seen.insert(location.object_key()) {
                continue;
            }
            if location.object_key() == key {
                return Ok(true);
            }
            if !location.entry().is_dir() {
                continue;
            }
            let directory = location.entry().as_dir()?;
            let mut failure = None;
            directory.read_dir(0, &mut |name: &FsName, _, _, _| {
                if name.as_bytes() == b"." || name.as_bytes() == b".." {
                    return true;
                }
                match directory.lookup(name) {
                    Ok(entry) => {
                        if pending.try_reserve(1).is_err() {
                            failure = Some(VfsError::NoMemory);
                            return false;
                        }
                        pending.push(Location::new(location.mountpoint().clone(), entry));
                    }
                    Err(error) => {
                        failure = Some(error);
                        return false;
                    }
                }
                true
            })?;
            if let Some(error) = failure {
                return Err(error);
            }
        }
        Ok(false)
    }

    fn unlink_index(&self, work: &Location, lower: ObjectKey) {
        let Ok(index) = self.index_directory(work) else {
            return;
        };
        let Ok(name) = Self::index_name(lower) else {
            return;
        };
        let Ok(directory) = index.entry().as_dir() else {
            return;
        };
        if let Ok(entry) = directory.lookup(&name) {
            let _ = directory.unlink_checked(&name, false, &entry);
        }
    }

    fn copy_metadata(
        &self,
        from: &Location,
        to: &Location,
        id_mapper: &dyn OverlayIdMapper,
    ) -> VfsResult<()> {
        let meta = from.metadata()?;
        to.update_metadata(MetadataUpdate {
            mode: Some(meta.mode),
            owner: Some((
                id_mapper.upper_visible_uid_to_kernel(id_mapper.lower_uid_to_upper(
                    from,
                    id_mapper.lower_kernel_uid_to_visible(from, meta.uid)?,
                )?)?,
                id_mapper.upper_visible_gid_to_kernel(id_mapper.lower_gid_to_upper(
                    from,
                    id_mapper.lower_kernel_gid_to_visible(from, meta.gid)?,
                )?)?,
            )),
            project_id: Some(meta.project_id),
            rdev: Some(meta.rdev),
            atime: Some(meta.atime),
            mtime: Some(meta.mtime),
            ctime: Some(meta.ctime),
        })?;
        if let (Some(source), Some(destination)) =
            (from.file_attr_provider(), to.file_attr_provider())
        {
            destination.set_file_attr(source.get_file_attr()?)?;
        }
        Ok(())
    }

    fn copy_xattrs(&self, from: &Location, to: &Location) -> VfsResult<()> {
        let Some(source) = from.entry().xattr_provider() else {
            return Ok(());
        };
        let Some(destination) = to.entry().xattr_provider() else {
            return Ok(());
        };
        let names = source.list_xattrs()?;
        for raw_name in names
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
        {
            // Overlay control state belongs to this transaction, never to an
            // arbitrary lower inode which may be supplied by another mount.
            if raw_name.starts_with(b"trusted.overlay.") {
                continue;
            }
            let value = source.get_xattr(raw_name)?;
            destination.set_xattr(raw_name, &value, XattrSetMode::Upsert)?;
        }
        Ok(())
    }

    fn copy_regular_data(&self, from: &Location, to: &Location) -> VfsResult<()> {
        let source = from.entry().as_file()?;
        let destination = to.entry().as_file()?;
        let mut offset = 0u64;
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(64 * 1024)
            .map_err(|_| VfsError::NoMemory)?;
        buffer.resize(64 * 1024, 0);
        loop {
            let read = source.read_at(&mut buffer, offset)?;
            if read == 0 {
                break;
            }
            let mut written = 0usize;
            while written != read {
                let count =
                    destination.write_at(&buffer[written..read], offset + written as u64)?;
                if count == 0 {
                    return Err(VfsError::Io);
                }
                written += count;
            }
            offset = offset
                .checked_add(read as u64)
                .ok_or(VfsError::InvalidInput)?;
        }
        destination.set_len(from.len()?)?;
        Ok(())
    }

    fn publish(
        &self,
        work: &Location,
        staged: &FsName,
        upper_parent: &Location,
        name: &FsName,
    ) -> VfsResult<Location> {
        let work_dir = work.entry().as_dir()?;
        let staged_entry = work_dir.lookup(staged)?;
        let target_dir = upper_parent.entry().as_dir()?;
        // Publication is exclusive.  Replacing an entry here would allow two
        // concurrent copy-ups to discard one writer's already-visible inode.
        match target_dir.lookup(name) {
            Ok(_) => return Err(VfsError::AlreadyExists),
            Err(VfsError::NotFound) => {}
            Err(error) => return Err(error),
        }
        work_dir.rename(staged, &staged_entry, target_dir, name, None)?;
        // `staged_entry` is the same inode after rename. Returning it avoids
        // a post-publication fallible lookup that would otherwise make a
        // completed rename indistinguishable from an unpublished staging
        // transaction to the cleanup path.
        Ok(Location::new(
            upper_parent.mountpoint().clone(),
            staged_entry,
        ))
    }
}

impl OverlayWriteBackend for VfsOverlayWriteBackend {
    fn recover(&self, work: &Location, upper: &Location) -> VfsResult<()> {
        let directory = work.entry().as_dir()?;
        // First construct a complete cleanup plan.  Recovery runs before this
        // overlay is exposed, but it must still be fail-closed: an I/O,
        // permission, or corruption error while inspecting one durable index
        // entry must not cause us to delete another entry that may be needed
        // after the next mount.
        let mut stale = Vec::new();
        let mut scan_error = None;
        directory.read_dir(0, &mut |name: &FsName, _, _, _| {
            if name.as_bytes().starts_with(b".ovl.stage.") {
                let owned = match FsNameBuf::from_vec(name.as_bytes().to_vec()) {
                    Ok(owned) => owned,
                    Err(error) => {
                        scan_error = Some(error);
                        return false;
                    }
                };
                let entry = match directory.lookup(name) {
                    Ok(entry) => entry,
                    Err(error) => {
                        scan_error = Some(error);
                        return false;
                    }
                };
                if stale.try_reserve(1).is_err() {
                    scan_error = Some(VfsError::NoMemory);
                    return false;
                }
                stale.push((owned, entry));
            }
            true
        })?;
        if let Some(error) = scan_error {
            return Err(error);
        }
        // Validate every persistent index entry while the work directory is
        // private. An entry without a matching generation-bound origin is
        // never trusted. A single-link index remains valid: it may preserve
        // identity after another upper hardlink alias was whiteouted.
        let index = self.index_directory(work)?;
        let index_dir = index.entry().as_dir()?;
        let mut stale_index = Vec::new();
        let mut committed_pending = Vec::new();
        let mut recovery_error = None;
        index_dir.read_dir(0, &mut |name: &FsName, _, _, _| {
            let entry = match index_dir.lookup(name) {
                Ok(entry) => entry,
                Err(error) => {
                    recovery_error = Some(error);
                    return false;
                }
            };
            let owned = match FsNameBuf::from_vec(name.as_bytes().to_vec()) {
                Ok(owned) => owned,
                Err(error) => {
                    recovery_error = Some(error);
                    return false;
                }
            };
            let valid_name = core::str::from_utf8(name.as_bytes())
                .ok()
                .is_some_and(|text| {
                    text.len() == 48 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
                });
            let Some(xattrs) = entry.xattr_provider() else {
                // We cannot distinguish a damaged record from an upper that
                // cannot currently expose its durable metadata.  Preserve
                // the link until a later recovery can inspect it.
                recovery_error = Some(VfsError::OperationNotSupported);
                return false;
            };
            let origin = match xattrs.get_xattr(OVERLAY_ORIGIN) {
                Ok(value) => Self::decode_key(&value),
                // A missing origin is an explicitly invalid index record.
                Err(VfsError::NotFound) => None,
                Err(error) => {
                    recovery_error = Some(error);
                    return false;
                }
            };
            let valid_origin = origin
                .and_then(|key| Self::index_name(key).ok())
                .is_some_and(|expected| expected.as_bytes() == name.as_bytes());
            if !valid_name || !valid_origin {
                if stale_index.try_reserve(1).is_err() {
                    recovery_error = Some(VfsError::NoMemory);
                    return false;
                }
                stale_index.push((owned, entry));
                return true;
            }
            let kind = match self.index_kind(&entry) {
                Ok(kind) => kind,
                // A record written by the former ambiguous protocol, or one
                // interrupted before its discriminator, is never reusable.
                Err(VfsError::InvalidInput) => {
                    if stale_index.try_reserve(1).is_err() {
                        recovery_error = Some(VfsError::NoMemory);
                        return false;
                    }
                    stale_index.push((owned, entry));
                    return true;
                }
                Err(error) => {
                    recovery_error = Some(error);
                    return false;
                }
            };
            let marker_target = match self.marker_target(&entry) {
                Ok(target) => target,
                // The target xattr/payload pair was successfully inspected
                // but is malformed, so this is a safe stale record.
                Err(VfsError::InvalidInput) => {
                    if stale_index.try_reserve(1).is_err() {
                        recovery_error = Some(VfsError::NoMemory);
                        return false;
                    }
                    stale_index.push((owned, entry));
                    return true;
                }
                Err(error) => {
                    recovery_error = Some(error);
                    return false;
                }
            };
            let pending = match xattrs.get_xattr(OVERLAY_INDEX_PENDING) {
                Ok(_) => true,
                // The marker is optional once publication has committed.
                Err(VfsError::NotFound) => false,
                Err(error) => {
                    recovery_error = Some(error);
                    return false;
                }
            };
            let well_formed = match kind {
                OverlayIndexKind::File => !entry.is_dir() && marker_target.is_none(),
                OverlayIndexKind::DirectoryMarker => !entry.is_dir() && marker_target.is_some(),
            };
            if !well_formed {
                if stale_index.try_reserve(1).is_err() {
                    recovery_error = Some(VfsError::NoMemory);
                    return false;
                }
                stale_index.push((owned, entry));
                return true;
            }
            if pending {
                let target = marker_target.unwrap_or_else(|| entry.object_key());
                match self.upper_contains_key(upper, target) {
                    Ok(true) => {
                        if committed_pending.try_reserve(1).is_err() {
                            recovery_error = Some(VfsError::NoMemory);
                            return false;
                        }
                        committed_pending.push((owned, entry));
                    }
                    // This is the only liveness-based deletion: the target
                    // was successfully validated and is absent from upper.
                    Ok(false) => {
                        if stale_index.try_reserve(1).is_err() {
                            recovery_error = Some(VfsError::NoMemory);
                            return false;
                        }
                        stale_index.push((owned, entry));
                    }
                    Err(error) => {
                        recovery_error = Some(error);
                        return false;
                    }
                }
            }
            true
        })?;
        if let Some(error) = recovery_error {
            return Err(error);
        }
        // The plan is complete.  Do not issue another provider lookup here:
        // the collected dentries bind each unlink to the object that was
        // validated above, and avoid turning a later read failure into a
        // partial cleanup.
        for (name, entry) in stale {
            directory.unlink_checked(&name, entry.is_dir(), &entry)?;
        }
        let mut changed_index = false;
        for (name, entry) in stale_index {
            index_dir.unlink_checked(&name, false, &entry)?;
            changed_index = true;
        }
        for (_, entry) in committed_pending {
            match entry
                .xattr_provider()
                .ok_or(VfsError::OperationNotSupported)?
                .remove_xattr(OVERLAY_INDEX_PENDING)
            {
                Ok(()) | Err(VfsError::NotFound) => {}
                Err(error) => return Err(error),
            }
            entry.sync(false)?;
            changed_index = true;
        }
        if changed_index {
            index.sync(false)?;
        }
        work.sync(false)
    }

    fn copy_up(
        &self,
        work: &Location,
        upper_parent: &Location,
        name: &FsName,
        lower: &Location,
        features: OverlayFeatures,
        id_mapper: &dyn OverlayIdMapper,
        before_publish: &mut dyn FnMut(ObjectKey),
    ) -> VfsResult<Location> {
        if let Ok(entry) = upper_parent.entry().as_dir()?.lookup(name) {
            return Ok(Location::new(upper_parent.mountpoint().clone(), entry));
        }
        let lower_key = lower.object_key();
        if features.index {
            if let Some(reused) =
                self.reuse_index(work, upper_parent, name, lower_key, before_publish)?
            {
                return Ok(reused);
            }
        }
        let staged = self.stage_name()?;
        let metadata = lower.metadata()?;
        let options = axfs_ng_vfs::NamedCreateOptions {
            node_type: metadata.node_type,
            permission: metadata.mode,
            owner: Some((metadata.uid, metadata.gid)),
            rdev: Some(metadata.rdev),
            initial_data: None,
            initial_attributes: Default::default(),
        };
        let work_dir = work.entry().as_dir()?;
        let entry = match metadata.node_type {
            NodeType::Symlink => work_dir.create_symlink(
                &staged,
                &lower.read_link()?,
                metadata.mode,
                Some((metadata.uid, metadata.gid)),
            )?,
            _ => {
                work_dir
                    .create_named(&staged, &options, CreateDisposition::Exclusive)?
                    .entry
            }
        };
        let staged_location = Location::new(work.mountpoint().clone(), entry);
        let mut was_published = false;
        let result = (|| {
            if metadata.node_type == NodeType::RegularFile {
                self.copy_regular_data(lower, &staged_location)?;
            }
            self.copy_metadata(lower, &staged_location, id_mapper)?;
            self.copy_xattrs(lower, &staged_location)?;
            self.set_origin(&staged_location, lower_key)?;
            if features.index {
                self.set_index(&staged_location, lower_key)?;
            }
            // The copied inode and its origin must reach stable storage
            // before an index entry can ever retain it through a crash.
            staged_location.sync(false)?;
            if features.index {
                // Link from the private index before changing the merged
                // namespace.  The final work→upper rename retains this same
                // inode, so a crash leaves either a recoverable private
                // entry or a fully indexed upper object, never an xattr-only
                // promise of identity.
                self.link_index(work, &staged_location, lower_key)?;
            }
            work.sync(false)?;
            // Rename preserves the staged inode key. Install the overlay
            // runtime alias before the upper name becomes lookup-visible.
            before_publish(staged_location.object_key());
            let location = self.publish(work, &staged, upper_parent, name)?;
            was_published = true;
            // Once the name is visible, retain its index entry even if this
            // flush fails.  That gives recovery and errseq consumers a real
            // inode to revisit instead of turning a post-publication failure
            // into an unindexed half-success.
            upper_parent.sync(false)?;
            if features.index {
                self.commit_index(work, lower_key)?;
            }
            Ok(location)
        })();
        if result.is_err() && !was_published {
            if features.index {
                self.unlink_index(work, lower_key);
            }
            if let Ok(entry) = work_dir.lookup(&staged) {
                let _ = work_dir.unlink_checked(&staged, entry.is_dir(), &entry);
            }
        }
        result
    }

    fn create(
        &self,
        work: &Location,
        upper_parent: &Location,
        name: &FsName,
        options: &axfs_ng_vfs::NamedCreateOptions,
    ) -> VfsResult<Location> {
        let staged = self.stage_name()?;
        let work_dir = work.entry().as_dir()?;
        let entry = work_dir
            .create_named(&staged, options, CreateDisposition::Exclusive)?
            .entry;
        let location = Location::new(work.mountpoint().clone(), entry);
        let mut was_published = false;
        let result = (|| {
            location.sync(false)?;
            work.sync(false)?;
            let published_location = self.publish(work, &staged, upper_parent, name)?;
            was_published = true;
            // `publish` has completed its work→upper rename and dropped its
            // directory operations.  Durably commit that visible name only
            // afterwards; a sync failure reports writeback failure but must
            // never retract the now-recoverable upper object.
            upper_parent.sync(false)?;
            Ok(published_location)
        })();
        if result.is_err() && !was_published {
            if let Ok(entry) = work_dir.lookup(&staged) {
                let _ = work_dir.unlink_checked(&staged, entry.is_dir(), &entry);
            }
        }
        result
    }

    fn create_symlink(
        &self,
        work: &Location,
        upper_parent: &Location,
        name: &FsName,
        target: &FsPath,
        options: &axfs_ng_vfs::NamedCreateOptions,
        prepare_visible: &mut dyn FnMut(Location) -> VfsResult<()>,
        published: &mut dyn FnMut(),
    ) -> VfsResult<()> {
        if options.node_type != NodeType::Symlink {
            return Err(VfsError::InvalidInput);
        }
        let staged = self.stage_name()?;
        let work_dir = work.entry().as_dir()?;
        // The native prepared primitive atomically installs target, owner,
        // mode, project state and ACLs.  Never create an empty/under-attributed
        // link and patch it up after it has a directory name.
        let entry = work_dir.create_symlink_prepared(&staged, target, options)?;
        let location = Location::new(work.mountpoint().clone(), entry);
        let mut was_published = false;
        let result = (|| {
            location.sync(false)?;
            work.sync(false)?;
            // Build every overlay-visible object against its final upper
            // mount context while the inode is still hidden in workdir.
            prepare_visible(Location::new(
                upper_parent.mountpoint().clone(),
                location.entry().clone(),
            ))?;
            self.publish(work, &staged, upper_parent, name)?;
            was_published = true;
            published();
            upper_parent.sync(false)?;
            Ok(())
        })();
        if result.is_err() && !was_published {
            if let Ok(entry) = work_dir.lookup(&staged) {
                let _ = work_dir.unlink_checked(&staged, false, &entry);
            }
        }
        result
    }

    fn whiteout(&self, work: &Location, upper_parent: &Location, name: &FsName) -> VfsResult<()> {
        let staged = self.stage_name()?;
        let options = axfs_ng_vfs::NamedCreateOptions {
            node_type: NodeType::CharacterDevice,
            permission: NodePermission::default(),
            owner: None,
            rdev: Some(axfs_ng_vfs::DeviceId(0)),
            initial_data: None,
            initial_attributes: Default::default(),
        };
        let work_dir = work.entry().as_dir()?;
        let entry = work_dir
            .create_named(&staged, &options, CreateDisposition::Exclusive)?
            .entry;
        let whiteout = Location::new(work.mountpoint().clone(), entry);
        let mut published = false;
        let result = (|| {
            whiteout
                .entry()
                .xattr_provider()
                .ok_or(VfsError::OperationNotSupported)?
                .set_xattr(OVERLAY_WHITEOUT, b"y", XattrSetMode::Upsert)?;
            whiteout.sync(false)?;
            work.sync(false)?;
            self.publish(work, &staged, upper_parent, name)?;
            published = true;
            // The whiteout is already visible after publish.  Persist the
            // parent directory after the rename path has released its locks;
            // on failure leave the recoverable whiteout in place.
            upper_parent.sync(false)?;
            Ok(())
        })();
        if result.is_err() && !published {
            if let Ok(entry) = work_dir.lookup(&staged) {
                let _ = work_dir.unlink_checked(&staged, false, &entry);
            }
        }
        result
    }

    fn replace_with_whiteout(
        &self,
        work: &Location,
        upper_parent: &Location,
        name: &FsName,
        expected: &DirEntry,
    ) -> VfsResult<()> {
        let staged = self.stage_name()?;
        let options = axfs_ng_vfs::NamedCreateOptions {
            node_type: NodeType::CharacterDevice,
            permission: axfs_ng_vfs::NodePermission::default(),
            owner: None,
            rdev: Some(axfs_ng_vfs::DeviceId(0)),
            initial_data: None,
            initial_attributes: Default::default(),
        };
        let work_dir = work.entry().as_dir()?;
        let entry = work_dir
            .create_named(&staged, &options, CreateDisposition::Exclusive)?
            .entry;
        let whiteout = Location::new(work.mountpoint().clone(), entry);
        let result = (|| {
            whiteout
                .entry()
                .xattr_provider()
                .ok_or(VfsError::OperationNotSupported)?
                .set_xattr(OVERLAY_WHITEOUT, b"y", XattrSetMode::Upsert)?;
            whiteout.sync(false)?;
            work.sync(false)?;
            work_dir.rename(
                &staged,
                &whiteout.entry(),
                upper_parent.entry().as_dir()?,
                name,
                Some(expected),
            )?;
            upper_parent.sync(false)
        })();
        if result.is_err() {
            if let Ok(entry) = work_dir.lookup(&staged) {
                let _ = work_dir.unlink_checked(&staged, false, &entry);
            }
        }
        result
    }

    fn add_tombstone(&self, upper_parent: &Location, name: &FsName) -> VfsResult<()> {
        let xattr = upper_parent
            .entry()
            .xattr_provider()
            .ok_or(VfsError::OperationNotSupported)?;
        let mut entries = match xattr.get_xattr(OVERLAY_TOMBSTONES) {
            Ok(entries) => entries,
            Err(VfsError::NotFound) => Vec::new(),
            Err(error) => return Err(error),
        };
        if entries
            .split(|byte| *byte == 0)
            .any(|entry| entry == name.as_bytes())
        {
            return Ok(());
        }
        entries
            .try_reserve(name.as_bytes().len() + 1)
            .map_err(|_| VfsError::NoMemory)?;
        entries.extend_from_slice(name.as_bytes());
        entries.push(0);
        xattr.set_xattr(OVERLAY_TOMBSTONES, &entries, XattrSetMode::Upsert)?;
        upper_parent.sync(false)
    }

    fn set_opaque(&self, upper_dir: &Location, opaque: bool) -> VfsResult<()> {
        let xattr = upper_dir
            .entry()
            .xattr_provider()
            .ok_or(VfsError::OperationNotSupported)?;
        if opaque {
            xattr.set_xattr(OVERLAY_OPAQUE, b"y", XattrSetMode::Upsert)?;
        } else {
            xattr.remove_xattr(OVERLAY_OPAQUE)?;
        }
        upper_dir.sync(false)
    }

    fn set_redirect(&self, upper: &Location, target: &FsPath) -> VfsResult<()> {
        upper
            .entry()
            .xattr_provider()
            .ok_or(VfsError::OperationNotSupported)?
            .set_xattr(OVERLAY_REDIRECT, target.as_bytes(), XattrSetMode::Upsert)?;
        upper.sync(false)
    }

    fn set_origin(&self, upper: &Location, lower: ObjectKey) -> VfsResult<()> {
        let provider = upper
            .entry()
            .xattr_provider()
            .ok_or(VfsError::OperationNotSupported)?;
        provider.set_xattr(
            OVERLAY_ORIGIN,
            &Self::encode_key(lower),
            XattrSetMode::Upsert,
        )
    }

    fn set_index(&self, upper: &Location, lower: ObjectKey) -> VfsResult<()> {
        upper
            .entry()
            .xattr_provider()
            .ok_or(VfsError::OperationNotSupported)?
            .set_xattr(
                OVERLAY_INDEX,
                &Self::encode_key(lower),
                XattrSetMode::Upsert,
            )
    }

    fn lookup_index(&self, work: &Location, lower: ObjectKey) -> VfsResult<Location> {
        self.indexed_location(work, lower)?
            .ok_or(VfsError::NotFound)
    }

    fn index_directory_identity(
        &self,
        work: &Location,
        upper: &Location,
        lower: ObjectKey,
    ) -> VfsResult<()> {
        self.link_index(work, upper, lower)?;
        self.set_origin(upper, lower)?;
        self.set_index(upper, lower)?;
        // The index's pending bit is the commit record.  Do not clear it
        // until the directory target and its origin/index metadata have
        // reached stable storage; otherwise recovery could treat a marker as
        // committed while its target proof is still only dirty cache state.
        upper.sync(false)?;
        self.commit_index(work, lower)
    }

    fn index_target(&self, entry: &Location) -> VfsResult<Option<ObjectKey>> {
        self.marker_target(entry.entry())
    }
}

/// A live overlay VFS.  The read path is fully composed: upper entries hide
/// lower entries, whiteouts hide all lowers, opaque directories terminate the
/// lower walk, and readdir has one merged namespace.  Mutation entry points
/// are intentionally fail-closed until supplied with a backend implementing
/// [`OverlayCopyUpBackend`]; the generic VFS cannot synthesize a safe staging
/// inode or atomic workdir publication from ordinary create/rename calls.
pub struct OverlayFilesystem {
    topology: OverlayTopology,
    backend: Option<Arc<dyn OverlayWriteBackend>>,
    root: Mutex<Option<DirEntry>>,
    /// Serializes copy-up publication across independently materialized
    /// dentries for the same lower object.  Correctness comes first here;
    /// provider-specific backends may later replace this with keyed locks.
    copy_up_serial: Mutex<()>,
    file_runtime: Mutex<hashbrown::HashMap<ObjectKey, alloc::sync::Weak<NodeUserData>>>,
    namespace_epoch: AtomicU64,
}

impl OverlayFilesystem {
    fn file_runtime(&self, identity: ObjectKey) -> VfsResult<Arc<NodeUserData>> {
        let mut runtime = self.file_runtime.lock();
        // Aliases are weak-only; reclaim dead lower/upper identities before
        // admitting another lookup so copy-up history cannot grow forever.
        runtime.retain(|_, state| state.strong_count() != 0);
        if let Some(existing) = runtime.get(&identity).and_then(alloc::sync::Weak::upgrade) {
            return Ok(existing);
        }
        runtime.remove(&identity);
        let created = Arc::try_new(NodeUserData::new()).map_err(|_| VfsError::NoMemory)?;
        runtime.insert(identity, Arc::downgrade(&created));
        Ok(created)
    }

    fn alias_file_runtime(&self, origin: ObjectKey, upper: ObjectKey, runtime: &Arc<NodeUserData>) {
        let mut entries = self.file_runtime.lock();
        let weak = Arc::downgrade(runtime);
        entries.insert(origin, weak.clone());
        entries.insert(upper, weak);
    }

    pub fn new(topology: OverlayTopology) -> VfsResult<Filesystem> {
        let backend: Option<Arc<dyn OverlayWriteBackend>> = if topology.upper.is_some() {
            // Keep the public storage type erased: callers of the generic
            // constructor must observe the same backend contract as callers
            // that provide a provider-specific implementation.
            Some(Arc::new(VfsOverlayWriteBackend))
        } else {
            None
        };
        Self::new_with_backend(topology, backend)
    }

    pub fn new_with_backend(
        topology: OverlayTopology,
        backend: Option<Arc<dyn OverlayWriteBackend>>,
    ) -> VfsResult<Filesystem> {
        if topology.upper.is_some() != backend.is_some() {
            return Err(VfsError::InvalidInput);
        }
        let fs = Arc::try_new(Self {
            topology,
            backend,
            root: Mutex::new(None),
            copy_up_serial: Mutex::new(()),
            file_runtime: Mutex::new(hashbrown::HashMap::new()),
            namespace_epoch: AtomicU64::new(1),
        })
        .map_err(|_| VfsError::NoMemory)?;
        let filesystem = Filesystem::try_new(fs.clone())?;
        let root_layers = OverlayLayers {
            upper: fs.topology.upper.clone(),
            lower: fs.topology.lower.clone(),
        };
        if root_layers.lower.is_empty() {
            return Err(VfsError::InvalidInput);
        }
        if let (Some(backend), Some(work)) = (&fs.backend, &fs.topology.work) {
            backend.recover(
                work,
                fs.topology.upper.as_ref().ok_or(VfsError::InvalidInput)?,
            )?;
        }
        // Prefer the lower-origin identity whenever it exists.  A copy-up
        // backend may publish the upper dentry before returning from
        // `copy_up`; a concurrent relookup must nevertheless resolve this
        // same runtime before the explicit upper-key alias is installed.
        let root_identity = root_layers
            .lower
            .first()
            .map(Location::object_key)
            .unwrap_or_else(|| root_layers.visible().object_key());
        let root_runtime = fs.file_runtime(root_identity)?;
        let root = DirEntry::new_dir(
            |self_entry| {
                DirNode::new(Arc::new(OverlayDir {
                    fs: fs.clone(),
                    layers: root_layers,
                    self_entry,
                    parent: None,
                    name: None,
                    materialized_upper: Mutex::new(None),
                    runtime: root_runtime,
                    origin_key: root_identity,
                }))
            },
            Reference::root(),
        );
        *fs.root.lock() = Some(root);
        Ok(filesystem)
    }

    fn epoch(&self) -> u64 {
        self.namespace_epoch.load(Ordering::Acquire)
    }

    fn read_only_error() -> VfsError {
        LinuxError::EROFS.into()
    }

    fn root_entry(&self) -> DirEntry {
        self.root
            .lock()
            .clone()
            .expect("overlay root installed before publication")
    }

    fn resolve_redirect(&self, key: ObjectKey) -> VfsResult<Location> {
        // Redirects name a lower object identity, not a potentially stale
        // lower pathname.  Resolve it afresh under this mount's captured
        // lower roots; this keeps a redirect valid across upper renames and
        // rejects a recycled lower inode generation.
        let mut pending = Vec::new();
        pending
            .try_reserve(self.topology.lower.len())
            .map_err(|_| VfsError::NoMemory)?;
        for root in &self.topology.lower {
            pending.push(root.clone());
        }
        let mut seen = HashSet::new();
        while let Some(location) = pending.pop() {
            if !seen.insert(location.object_key()) {
                continue;
            }
            if location.object_key() == key {
                return Ok(location);
            }
            if !location.entry().is_dir() {
                continue;
            }
            let directory = location.entry().as_dir()?;
            let mut failure = None;
            directory.read_dir(0, &mut |name: &FsName, _, _, _| {
                if name.as_bytes() == b"." || name.as_bytes() == b".." {
                    return true;
                }
                match directory.lookup(name) {
                    Ok(entry) => {
                        if pending.try_reserve(1).is_err() {
                            failure = Some(VfsError::NoMemory);
                            return false;
                        }
                        pending.push(Location::new(location.mountpoint().clone(), entry));
                    }
                    Err(error) => {
                        failure = Some(error);
                        return false;
                    }
                }
                true
            })?;
            if let Some(error) = failure {
                return Err(error);
            }
        }
        Err(VfsError::NotFound)
    }

    fn find_upper_origin(&self, origin: ObjectKey) -> VfsResult<Location> {
        let root = self.topology.upper.as_ref().ok_or(VfsError::NotFound)?;
        let mut pending = Vec::new();
        pending.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        pending.push(root.clone());
        let mut seen = HashSet::new();
        while let Some(location) = pending.pop() {
            if !seen.insert(location.object_key()) {
                continue;
            }
            let matches = location
                .entry()
                .xattr_provider()
                .and_then(|xattrs| xattrs.get_xattr(OVERLAY_ORIGIN).ok())
                .and_then(|value| VfsOverlayWriteBackend::decode_key(&value))
                == Some(origin);
            if matches {
                return Ok(location);
            }
            if !location.entry().is_dir() {
                continue;
            }
            let directory = location.entry().as_dir()?;
            let mut failure = None;
            directory.read_dir(0, &mut |name: &FsName, _, _, _| {
                if name.as_bytes() == b"." || name.as_bytes() == b".." {
                    return true;
                }
                // The workdir-only index is not below upperdir, but reserve
                // the prefix anyway so a malformed upper cannot make an
                // internal-looking object exportable.
                if name.as_bytes() == OVERLAY_INDEX_DIRECTORY {
                    return true;
                }
                match directory.lookup(name) {
                    Ok(entry) => {
                        if pending.try_reserve(1).is_err() {
                            failure = Some(VfsError::NoMemory);
                            return false;
                        }
                        pending.push(Location::new(location.mountpoint().clone(), entry));
                    }
                    Err(error) => {
                        failure = Some(error);
                        return false;
                    }
                }
                true
            })?;
            if let Some(error) = failure {
                return Err(error);
            }
        }
        Err(VfsError::NotFound)
    }

    fn find_overlay_entry_by_upper_key(&self, key: ObjectKey) -> VfsResult<DirEntry> {
        let mut pending = Vec::new();
        pending.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        pending.push(self.root_entry());
        let mut seen = HashSet::new();
        while let Some(entry) = pending.pop() {
            if let Ok(file) = entry.downcast::<OverlayFile>() {
                if file.location().object_key() == key {
                    return Ok(entry);
                }
                continue;
            }
            let Ok(directory) = entry.downcast::<OverlayDir>() else {
                continue;
            };
            if !seen.insert(directory.object_key()) {
                continue;
            }
            if directory
                .present_upper()
                .is_some_and(|upper| upper.object_key() == key)
            {
                return Ok(entry);
            }
            let mut failure = None;
            directory.read_dir(0, &mut |name: &FsName, _, _, _| {
                match directory.lookup(name) {
                    Ok(child) => {
                        if pending.try_reserve(1).is_err() {
                            failure = Some(VfsError::NoMemory);
                            return false;
                        }
                        pending.push(child);
                    }
                    Err(error) => {
                        failure = Some(error);
                        return false;
                    }
                }
                true
            })?;
            if let Some(error) = failure {
                return Err(error);
            }
        }
        Err(VfsError::NotFound)
    }
}

impl FilesystemOps for OverlayFilesystem {
    fn name(&self) -> &str {
        "overlay"
    }

    fn root_dir(&self) -> DirEntry {
        self.root_entry()
    }

    fn stat(&self) -> VfsResult<StatFs> {
        // Linux reports the upper statfs for writable overlays and the first
        // visible lower for read-only overlays.  This is a real provider
        // result, not a fabricated aggregate with misleading free space.
        let location = self
            .topology
            .upper
            .as_ref()
            .unwrap_or(&self.topology.lower[0]);
        location.filesystem().stat()
    }

    fn flush(&self) -> VfsResult<()> {
        if let Some(upper) = &self.topology.upper {
            upper.filesystem().flush()?;
        }
        for lower in &self.topology.lower {
            lower.filesystem().flush()?;
        }
        Ok(())
    }

    fn encode_export_handle(
        &self,
        entry: &DirEntry,
        _mode: ExportHandleMode,
    ) -> VfsResult<ExportHandle> {
        if !self.topology.features.nfs_export {
            return Err(VfsError::OperationNotSupported);
        }
        let location = if let Ok(file) = entry.downcast::<OverlayFile>() {
            file.upper()?
        } else if let Ok(dir) = entry.downcast::<OverlayDir>() {
            dir.upper_location()?
        } else {
            return Err(VfsError::InvalidInput);
        };
        let directory_lower = if location.entry().is_dir() {
            Some(
                entry
                    .downcast::<OverlayDir>()?
                    .layers
                    .lower
                    .first()
                    .ok_or(VfsError::NotFound)?
                    .object_key(),
            )
        } else {
            None
        };
        let origin = match location
            .entry()
            .xattr_provider()
            .ok_or(VfsError::NotFound)?
            .get_xattr(OVERLAY_ORIGIN)
        {
            Ok(value) => VfsOverlayWriteBackend::decode_key(&value).ok_or(VfsError::NotFound)?,
            Err(VfsError::NotFound) if location.entry().is_dir() => {
                let lower = directory_lower.ok_or(VfsError::NotFound)?;
                self.backend
                    .as_ref()
                    .ok_or(VfsError::OperationNotSupported)?
                    .index_directory_identity(
                        self.topology
                            .work
                            .as_ref()
                            .ok_or(VfsError::OperationNotSupported)?,
                        &location,
                        lower,
                    )?;
                lower
            }
            Err(error) => return Err(error),
        };
        // The xattr is merely the inode-side proof; the private index is the
        // durable reachability proof required by an export handle.
        let backend = self
            .backend
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?;
        let work = self
            .topology
            .work
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?;
        let indexed = match backend.lookup_index(work, origin) {
            Ok(indexed) => indexed,
            Err(VfsError::NotFound)
                if location.entry().is_dir() && directory_lower == Some(origin) =>
            {
                backend.index_directory_identity(work, &location, origin)?;
                backend.lookup_index(work, origin)?
            }
            Err(error) => return Err(error),
        };
        if !location.entry().is_dir() && indexed.object_key() != location.object_key() {
            return Err(VfsError::NotFound);
        }
        Ok(ExportHandle {
            handle_type: OVERLAY_EXPORT_HANDLE_TYPE,
            bytes: VfsOverlayWriteBackend::encode_key(origin).to_vec(),
        })
    }

    fn decode_export_handle_with_mode(
        &self,
        handle_type: i32,
        bytes: &[u8],
        mode: ExportHandleDecodeMode,
    ) -> VfsResult<DirEntry> {
        if !self.topology.features.nfs_export || handle_type != OVERLAY_EXPORT_HANDLE_TYPE {
            return Err(VfsError::NotFound);
        }
        let lower = VfsOverlayWriteBackend::decode_key(bytes).ok_or(VfsError::NotFound)?;
        // Missing index is stale, even when a forged/stale upper origin
        // xattr happens to match.  Directory records are physical index
        // marker files, so resolve their target through the upper origin walk
        // only after the marker has been validated above.
        let indexed = self
            .backend
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?
            .lookup_index(
                self.topology
                    .work
                    .as_ref()
                    .ok_or(VfsError::OperationNotSupported)?,
                lower,
            )?;
        let marker_target = self
            .backend
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?
            .index_target(&indexed)?;
        let location = if let Some(expected) = marker_target {
            let target = self.find_upper_origin(lower)?;
            if target.object_key() != expected || !target.entry().is_dir() {
                return Err(VfsError::NotFound);
            }
            target
        } else {
            indexed
        };
        if mode == ExportHandleDecodeMode::DirectoryOnly && !location.entry().is_dir() {
            return Err(VfsError::NotFound);
        }
        self.find_overlay_entry_by_upper_key(location.object_key())
    }

    fn unmount(&self) {
        self.root.lock().take();
    }
}

#[derive(Clone)]
struct OverlayLayers {
    upper: Option<Location>,
    lower: Vec<Location>,
}

impl OverlayLayers {
    fn visible(&self) -> &Location {
        self.upper.as_ref().unwrap_or(&self.lower[0])
    }
}

fn redirect_path(keys: &[Location]) -> VfsResult<FsPathBuf> {
    if keys.is_empty() {
        return Err(VfsError::InvalidInput);
    }
    let mut text = alloc::format!(
        "/.ovl.redirect/{:016x}{:016x}{:016x}",
        keys[0].object_key().filesystem,
        keys[0].object_key().object,
        keys[0].object_key().generation
    );
    for location in &keys[1..] {
        let key = location.object_key();
        text.push_str(&alloc::format!(
            ",{:016x}{:016x}{:016x}",
            key.filesystem,
            key.object,
            key.generation
        ));
    }
    Ok(FsPathBuf::from_vec(text.into_bytes()))
}

fn redirect_keys(location: &Location) -> VfsResult<Option<Vec<ObjectKey>>> {
    let Some(provider) = location.entry().xattr_provider() else {
        return Ok(None);
    };
    let value = match provider.get_xattr(OVERLAY_REDIRECT) {
        Ok(value) => value,
        Err(VfsError::NotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    let text = core::str::from_utf8(&value).map_err(|_| VfsError::InvalidInput)?;
    let value = text
        .strip_prefix("/.ovl.redirect/")
        .ok_or(VfsError::InvalidInput)?;
    let mut keys = Vec::new();
    for component in value.split(',') {
        if component.len() != 48 || !component.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(VfsError::InvalidInput);
        }
        keys.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        keys.push(ObjectKey::new(
            u64::from_str_radix(&component[..16], 16).map_err(|_| VfsError::InvalidInput)?,
            u64::from_str_radix(&component[16..32], 16).map_err(|_| VfsError::InvalidInput)?,
            u64::from_str_radix(&component[32..], 16).map_err(|_| VfsError::InvalidInput)?,
        ));
    }
    if keys.is_empty() {
        return Err(VfsError::InvalidInput);
    }
    Ok(Some(keys))
}

struct OverlayDir {
    fs: Arc<OverlayFilesystem>,
    layers: OverlayLayers,
    self_entry: axfs_ng_vfs::WeakDirEntry,
    parent: Option<axfs_ng_vfs::WeakDirEntry>,
    name: Option<FsNameBuf>,
    materialized_upper: Mutex<Option<Location>>,
    runtime: Arc<NodeUserData>,
    /// The first visible lower/upper object identity.  Copy-up aliases this
    /// to the new upper key so a relookup cannot split the inode gate.
    origin_key: ObjectKey,
}

impl OverlayDir {
    fn present_upper(&self) -> Option<Location> {
        self.materialized_upper
            .lock()
            .clone()
            .or_else(|| self.layers.upper.clone())
    }

    /// Returns the backing object currently exposed by this directory.  A
    /// directory that was copied up after its dentry was materialized must
    /// not keep reporting or syncing the old lower inode: held directory
    /// FDs, index aliases, and redirect lookups all retain this same node.
    ///
    /// `Location` is cloned while the short-lived publication mutex is held,
    /// then the guard is dropped by `present_upper` before a backend method
    /// can be called by the caller.
    fn active_location(&self) -> Location {
        self.present_upper()
            .unwrap_or_else(|| self.layers.visible().clone())
    }

    fn upper_location(&self) -> VfsResult<Location> {
        if let Some(upper) = self.present_upper() {
            return Ok(upper);
        }
        let parent = self
            .parent
            .as_ref()
            .and_then(axfs_ng_vfs::WeakDirEntry::upgrade)
            .ok_or_else(OverlayFilesystem::read_only_error)?;
        let upper_parent = parent.downcast::<OverlayDir>()?.upper_location()?;
        let name = self.name.as_ref().ok_or(VfsError::InvalidInput)?;
        let lower = self.layers.lower.first().ok_or(VfsError::NotFound)?;
        let backend = self
            .fs
            .backend
            .as_ref()
            .ok_or_else(OverlayFilesystem::read_only_error)?;
        let work = self
            .fs
            .topology
            .work
            .as_ref()
            .ok_or_else(OverlayFilesystem::read_only_error)?;
        let _copy_up = self.fs.copy_up_serial.lock();
        if let Some(upper) = self.present_upper() {
            return Ok(upper);
        }
        let origin = self.origin_key;
        let runtime = self.runtime.clone();
        let fs = self.fs.clone();
        let upper = backend.copy_up(
            work,
            &upper_parent,
            name,
            lower,
            self.fs.topology.features,
            self.fs.topology.id_mapper.as_ref(),
            &mut |upper| fs.alias_file_runtime(origin, upper, &runtime),
        )?;
        self.fs
            .alias_file_runtime(self.origin_key, upper.object_key(), &self.runtime);
        *self.materialized_upper.lock() = Some(upper.clone());
        self.fs.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(upper)
    }

    fn make_child(
        &self,
        name: &FsName,
        layers: OverlayLayers,
        node_type: NodeType,
    ) -> VfsResult<DirEntry> {
        let parent = self.self_entry.upgrade().ok_or(VfsError::NotFound)?;
        let reference = Reference::try_new(Some(parent.clone()), name)?;
        if node_type == NodeType::Directory {
            // `DirEntry::new_dir` needs a cyclic weak reference and therefore
            // accepts an infallible constructor closure.  Finish every
            // fallible ownership conversion before entering that closure.
            let parent = parent.downgrade();
            let name = FsNameBuf::from_vec(name.as_bytes().to_vec())?;
            let identity = layers
                .lower
                .first()
                .map(Location::object_key)
                .unwrap_or_else(|| layers.visible().object_key());
            let runtime = self.fs.file_runtime(identity)?;
            Ok(DirEntry::new_dir(
                |self_entry| {
                    DirNode::new(Arc::new(OverlayDir {
                        fs: self.fs.clone(),
                        layers,
                        self_entry,
                        parent: Some(parent),
                        name: Some(name),
                        materialized_upper: Mutex::new(None),
                        runtime,
                        origin_key: identity,
                    }))
                },
                reference,
            ))
        } else {
            let copied_upper = Arc::try_new(Mutex::new(None)).map_err(|_| VfsError::NoMemory)?;
            let parent = self
                .self_entry
                .upgrade()
                .ok_or(VfsError::NotFound)?
                .downgrade();
            let owned_name = FsNameBuf::from_vec(name.as_bytes().to_vec())?;
            let xattr_layers = layers.clone();
            let identity = layers
                .lower
                .first()
                .map(Location::object_key)
                .unwrap_or_else(|| layers.visible().object_key());
            let runtime = self.fs.file_runtime(identity)?;
            let node = Arc::try_new(OverlayFile {
                fs: self.fs.clone(),
                layers,
                parent: parent.clone(),
                name: owned_name.clone(),
                copied_upper: copied_upper.clone(),
                runtime: runtime.clone(),
                origin_key: identity,
                xattr: OverlayXattr {
                    fs: self.fs.clone(),
                    layers: xattr_layers,
                    parent,
                    name: owned_name,
                    copied_upper: copied_upper.clone(),
                    runtime,
                    origin_key: identity,
                },
            })
            .map_err(|_| VfsError::NoMemory)?;
            DirEntry::try_new_file(FileNode::new(node), node_type, reference)
        }
    }

    fn lookup_layers(&self, name: &FsName) -> VfsResult<(OverlayLayers, NodeType)> {
        let mut upper = None;
        let tombstoned = self
            .present_upper()
            .as_ref()
            .map(|parent| location_has_tombstone(parent, name))
            .transpose()?
            .unwrap_or(false);
        if let Some(parent) = self.present_upper().as_ref() {
            match parent.entry().as_dir()?.lookup(name) {
                Ok(entry) => {
                    if is_whiteout(&entry)? {
                        return Err(VfsError::NotFound);
                    }
                    let node_type = entry.node_type();
                    let location = Location::new(parent.mountpoint().clone(), entry);
                    // A non-directory upper entry always wins.  Directories
                    // continue the lower merge unless explicitly opaque.
                    if node_type != NodeType::Directory {
                        return Ok((
                            OverlayLayers {
                                upper: Some(location),
                                lower: Vec::new(),
                            },
                            node_type,
                        ));
                    }
                    upper = Some(location);
                }
                Err(VfsError::NotFound) => {}
                Err(error) => return Err(error),
            }
        }

        let mut lower = Vec::new();
        let upper_is_opaque = upper
            .as_ref()
            .map(location_is_opaque)
            .transpose()?
            .unwrap_or(false);
        if !tombstoned && !upper_is_opaque {
            let redirected =
                if let Some(redirects) = upper.as_ref().map(redirect_keys).transpose()?.flatten() {
                    lower
                        .try_reserve(redirects.len())
                        .map_err(|_| VfsError::NoMemory)?;
                    for redirect in redirects {
                        let location = self.fs.resolve_redirect(redirect)?;
                        if !location.entry().is_dir() {
                            return Err(VfsError::NotFound);
                        }
                        lower.push(location);
                    }
                    true
                } else {
                    false
                };
            for parent in self.layers.lower.iter().filter(|_| !redirected) {
                match parent.entry().as_dir()?.lookup(name) {
                    Ok(entry) => {
                        let node_type = entry.node_type();
                        let location = Location::new(parent.mountpoint().clone(), entry);
                        if upper.is_some() {
                            if node_type != NodeType::Directory {
                                continue;
                            }
                            lower.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                            lower.push(location);
                        } else {
                            let mut only = Vec::new();
                            only.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                            only.push(location);
                            return Ok((
                                OverlayLayers {
                                    upper: None,
                                    lower: only,
                                },
                                node_type,
                            ));
                        }
                    }
                    Err(VfsError::NotFound) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        if let Some(upper) = upper {
            return Ok((
                OverlayLayers {
                    upper: Some(upper),
                    lower,
                },
                NodeType::Directory,
            ));
        }
        Err(VfsError::NotFound)
    }

    fn lower_has_child(&self, name: &FsName) -> VfsResult<bool> {
        for lower in &self.layers.lower {
            match lower.entry().as_dir()?.lookup(name) {
                Ok(entry) => {
                    if !is_whiteout(&entry)? {
                        return Ok(true);
                    }
                }
                Err(VfsError::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }

    fn whiteout_replaced_upper(
        &self,
        upper: &Location,
        name: &FsName,
        expected: &DirEntry,
    ) -> VfsResult<()> {
        self.fs
            .backend
            .as_ref()
            .ok_or_else(OverlayFilesystem::read_only_error)?
            .replace_with_whiteout(
                self.fs
                    .topology
                    .work
                    .as_ref()
                    .ok_or_else(OverlayFilesystem::read_only_error)?,
                upper,
                name,
                expected,
            )
    }

    /// `redirect_dir=on` is permitted to use a redirect optimisation, but it
    /// must never make a renamed lower directory lose children when the
    /// redirect cannot be resolved in the receiving namespace.  Materialize
    /// the merged lower tree before a cross-name directory rename; this is
    /// the conservative, crash-safe implementation of the same visible
    /// semantics and also makes export/index identity self contained.
    fn materialize_lower_tree(&self) -> VfsResult<()> {
        if self.layers.lower.is_empty() {
            return Ok(());
        }
        let upper = self.upper_location()?;
        let backend = self
            .fs
            .backend
            .as_ref()
            .ok_or_else(OverlayFilesystem::read_only_error)?;
        let work = self
            .fs
            .topology
            .work
            .as_ref()
            .ok_or_else(OverlayFilesystem::read_only_error)?;
        let _copy_up = self.fs.copy_up_serial.lock();
        for lower in &self.layers.lower {
            materialize_overlay_tree(
                self.fs.as_ref(),
                backend.as_ref(),
                work,
                &upper,
                lower,
                self.fs.topology.features,
                self.fs.topology.id_mapper.as_ref(),
            )?;
        }
        upper.sync(false)
    }
}

fn materialize_overlay_tree(
    filesystem: &OverlayFilesystem,
    backend: &dyn OverlayWriteBackend,
    work: &Location,
    upper: &Location,
    lower: &Location,
    features: OverlayFeatures,
    id_mapper: &dyn OverlayIdMapper,
) -> VfsResult<()> {
    let lower_dir = lower.entry().as_dir()?;
    let mut failure = None;
    lower_dir.read_dir(0, &mut |name: &FsName, _, kind, _| {
        if name.as_bytes() == b"." || name.as_bytes() == b".." {
            return true;
        }
        let lower_child = match lower_dir.lookup(name) {
            Ok(entry) => entry,
            Err(error) => {
                failure = Some(error);
                return false;
            }
        };
        let lower_location = Location::new(lower.mountpoint().clone(), lower_child);
        let upper_dir = match upper.entry().as_dir() {
            Ok(dir) => dir,
            Err(error) => {
                failure = Some(error);
                return false;
            }
        };
        let upper_child = match upper_dir.lookup(name) {
            Ok(entry) => match is_whiteout(&entry) {
                Ok(true) => return true,
                Ok(false) => Location::new(upper.mountpoint().clone(), entry),
                Err(error) => {
                    failure = Some(error);
                    return false;
                }
            },
            Err(VfsError::NotFound) => {
                let origin = lower_location.object_key();
                let runtime = match filesystem.file_runtime(origin) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        failure = Some(error);
                        return false;
                    }
                };
                match backend.copy_up(
                    work,
                    upper,
                    name,
                    &lower_location,
                    features,
                    id_mapper,
                    &mut |upper| filesystem.alias_file_runtime(origin, upper, &runtime),
                ) {
                    Ok(location) => location,
                    Err(error) => {
                        failure = Some(error);
                        return false;
                    }
                }
            }
            Err(error) => {
                failure = Some(error);
                return false;
            }
        };
        if kind == NodeType::Directory {
            // A higher-priority non-directory masks this lower directory.
            // It is already the visible result and must not be treated as a
            // recursive destination merely because a later lower says `dir`.
            if !upper_child.entry().is_dir() {
                return true;
            }
            if let Err(error) = materialize_overlay_tree(
                filesystem,
                backend,
                work,
                &upper_child,
                &lower_location,
                features,
                id_mapper,
            ) {
                failure = Some(error);
                return false;
            }
        }
        true
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(())
}

impl NodeOps for OverlayDir {
    fn inode(&self) -> u64 {
        self.active_location().inode()
    }
    fn object_key(&self) -> ObjectKey {
        self.active_location().object_key()
    }
    fn metadata(&self) -> VfsResult<Metadata> {
        self.active_location().metadata()
    }
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        self.upper_location()?.update_metadata(update)
    }
    fn filesystem(&self) -> &dyn FilesystemOps {
        self.fs.as_ref()
    }
    fn sync(&self, data_only: bool) -> VfsResult<()> {
        self.active_location().sync(data_only)
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
    fn flags(&self) -> NodeFlags {
        self.active_location().flags()
    }
    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        Some(self.runtime.as_ref())
    }
    fn xattr_provider(&self) -> Option<&dyn axfs_ng_vfs::XattrProvider> {
        Some(self)
    }
    fn file_attr_provider(&self) -> Option<&dyn axfs_ng_vfs::FileAttrProvider> {
        Some(self)
    }
    fn get_file_attr(&self) -> VfsResult<axfs_ng_vfs::FileAttr> {
        self.active_location().get_file_attr()
    }
    fn set_file_attr(&self, attr: axfs_ng_vfs::FileAttr) -> VfsResult<()> {
        self.upper_location()?.set_file_attr(attr)
    }
    fn get_legacy_file_flags(&self) -> VfsResult<u32> {
        self.active_location().get_legacy_file_flags()
    }
    fn set_legacy_file_flags(&self, flags: u32) -> VfsResult<()> {
        self.upper_location()?.set_legacy_file_flags(flags)
    }
}

/// Directory xattrs and inode flags are mutation facades, not borrowed lower
/// providers.  Returning `self` makes every write pass through `upper_location`
/// and therefore through the copy-up transaction.
impl axfs_ng_vfs::XattrProvider for OverlayDir {
    fn get_xattr(&self, name: &[u8]) -> VfsResult<Vec<u8>> {
        reject_overlay_control_xattr(name)?;
        self.active_location()
            .entry()
            .xattr_provider()
            .ok_or(VfsError::OperationNotSupported)?
            .get_xattr(name)
    }
    fn list_xattrs(&self) -> VfsResult<Vec<u8>> {
        let raw = self
            .active_location()
            .entry()
            .xattr_provider()
            .ok_or(VfsError::OperationNotSupported)?
            .list_xattrs()?;
        visible_xattrs(raw)
    }
    fn set_xattr(&self, name: &[u8], value: &[u8], mode: XattrSetMode) -> VfsResult<()> {
        reject_overlay_control_xattr(name)?;
        self.upper_location()?
            .entry()
            .xattr_provider()
            .ok_or(VfsError::OperationNotSupported)?
            .set_xattr(name, value, mode)
    }
    fn remove_xattr(&self, name: &[u8]) -> VfsResult<()> {
        reject_overlay_control_xattr(name)?;
        self.upper_location()?
            .entry()
            .xattr_provider()
            .ok_or(VfsError::OperationNotSupported)?
            .remove_xattr(name)
    }
}

impl axfs_ng_vfs::FileAttrProvider for OverlayDir {
    fn get_file_attr(&self) -> VfsResult<axfs_ng_vfs::FileAttr> {
        self.active_location().get_file_attr()
    }
    fn set_file_attr(&self, attr: axfs_ng_vfs::FileAttr) -> VfsResult<()> {
        self.upper_location()?.set_file_attr(attr)
    }
    fn get_legacy_flags(&self) -> VfsResult<u32> {
        self.active_location().get_legacy_file_flags()
    }
    fn set_legacy_flags(&self, flags: u32) -> VfsResult<()> {
        self.upper_location()?.set_legacy_file_flags(flags)
    }
}

impl DirNodeOps for OverlayDir {
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let mut names = HashSet::<FsNameBuf>::new();
        let mut merged = Vec::<(FsNameBuf, u64, NodeType)>::new();
        if let Some(upper) = self.present_upper().as_ref() {
            collect_overlay_dir_entries(upper, true, &mut names, &mut merged)?;
        }
        // Tombstones are persisted before lower-directory rename publication.
        // They only suppress lower names; an upper entry with the same name
        // remains visible and naturally takes precedence.
        if let Some(upper) = self.present_upper().as_ref() {
            if let Some(raw) = get_overlay_control_xattr(upper.entry(), OVERLAY_TOMBSTONES)? {
                for name in raw.split(|byte| *byte == 0).filter(|name| !name.is_empty()) {
                    let mut bytes = Vec::new();
                    bytes
                        .try_reserve_exact(name.len())
                        .map_err(|_| VfsError::NoMemory)?;
                    bytes.extend_from_slice(name);
                    let name = FsNameBuf::from_vec(bytes)?;
                    names.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                    names.insert(name);
                }
            }
        }
        let upper_is_opaque = self
            .present_upper()
            .as_ref()
            .map(location_is_opaque)
            .transpose()?
            .unwrap_or(false);
        if !upper_is_opaque {
            for lower in &self.layers.lower {
                collect_overlay_dir_entries(lower, false, &mut names, &mut merged)?;
            }
        }
        let mut count = 0usize;
        for (index, (name, ino, kind)) in merged.into_iter().enumerate().skip(offset as usize) {
            if !sink.accept(&name, ino, kind, (index + 1) as u64) {
                break;
            }
            count += 1;
        }
        Ok(count)
    }

    fn lookup(&self, name: &FsName) -> VfsResult<DirEntry> {
        let (layers, kind) = self.lookup_layers(name)?;
        self.make_child(name, layers, kind)
    }

    fn is_cacheable(&self) -> bool {
        false
    }
    fn namespace_epoch(&self) -> u64 {
        self.fs.epoch()
    }
    fn supports_named_create(&self, _node_type: NodeType) -> bool {
        self.fs.backend.is_some()
    }
    fn supports_symlink(&self) -> bool {
        self.fs.backend.is_some()
    }
    fn supports_hard_links(&self) -> bool {
        self.fs.backend.is_some()
    }
    fn supports_unlink(&self) -> bool {
        self.fs.backend.is_some()
    }
    fn supports_rmdir(&self) -> bool {
        self.fs.backend.is_some()
    }
    fn supports_rename(&self) -> bool {
        self.fs.backend.is_some()
    }
    fn create_named(
        &self,
        name: &FsName,
        options: &axfs_ng_vfs::NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<axfs_ng_vfs::CreateOutcome<DirEntry>> {
        let upper = self.upper_location()?;
        if disposition == CreateDisposition::OpenOrCreate {
            if let Ok(existing) = self.lookup(name) {
                return Ok(axfs_ng_vfs::CreateOutcome {
                    entry: existing,
                    created: false,
                });
            }
        }
        let backend = self
            .fs
            .backend
            .as_ref()
            .ok_or_else(OverlayFilesystem::read_only_error)?;
        let work = self
            .fs
            .topology
            .work
            .as_ref()
            .ok_or_else(OverlayFilesystem::read_only_error)?;
        let location = backend.create(work, &upper, name, options)?;
        self.fs.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        let layers = OverlayLayers {
            upper: Some(location),
            lower: Vec::new(),
        };
        let entry = self.make_child(name, layers, options.node_type)?;
        Ok(axfs_ng_vfs::CreateOutcome {
            entry,
            created: true,
        })
    }
    fn create_symlink(
        &self,
        name: &FsName,
        target: &FsPath,
        permission: NodePermission,
        user: Option<(u32, u32)>,
    ) -> VfsResult<DirEntry> {
        self.create_symlink_prepared(
            name,
            target,
            &axfs_ng_vfs::NamedCreateOptions {
                node_type: NodeType::Symlink,
                permission,
                owner: user,
                rdev: None,
                initial_data: None,
                initial_attributes: Default::default(),
            },
        )
    }
    fn create_symlink_prepared(
        &self,
        name: &FsName,
        target: &FsPath,
        options: &axfs_ng_vfs::NamedCreateOptions,
    ) -> VfsResult<DirEntry> {
        if options.node_type != NodeType::Symlink {
            return Err(VfsError::InvalidInput);
        }
        let upper = self.upper_location()?;
        let backend = self
            .fs
            .backend
            .as_ref()
            .ok_or_else(OverlayFilesystem::read_only_error)?;
        let work = self
            .fs
            .topology
            .work
            .as_ref()
            .ok_or_else(OverlayFilesystem::read_only_error)?;
        let mut child = None;
        let mut prepare_visible = |location: Location| -> VfsResult<()> {
            child = Some(self.make_child(
                name,
                OverlayLayers {
                    upper: Some(location),
                    lower: Vec::new(),
                },
                NodeType::Symlink,
            )?);
            Ok(())
        };
        let mut committed = || {
            self.fs.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        };
        backend.create_symlink(
            work,
            &upper,
            name,
            target,
            options,
            &mut prepare_visible,
            &mut committed,
        )?;
        child.ok_or(VfsError::BadState)
    }
    fn link(&self, name: &FsName, node: &DirEntry) -> VfsResult<DirEntry> {
        let upper = self.upper_location()?;
        // `link(2)` on a lower file first copies it up; linking the borrowed
        // lower inode would make the lower filesystem writable through the
        // overlay and would lose overlay origin/index state.
        let source = node.downcast::<OverlayFile>()?.upper()?;
        let entry = upper.entry().as_dir()?.link(name, source.entry())?;
        self.fs.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        self.make_child(
            name,
            OverlayLayers {
                upper: Some(Location::new(upper.mountpoint().clone(), entry)),
                lower: Vec::new(),
            },
            node.node_type(),
        )
    }
    fn unlink(&self, request: UnlinkRequest<'_>) -> VfsResult<()> {
        let upper = self.upper_location()?;
        let directory = upper.entry().as_dir()?;
        match directory.lookup(request.name) {
            Ok(entry) => {
                if self.lower_has_child(request.name)? {
                    // Atomic replacement makes the lower alias disappear in
                    // the same namespace transition that removes upper.
                    self.whiteout_replaced_upper(&upper, request.name, &entry)?;
                } else {
                    directory.unlink_checked(request.name, request.is_dir, &entry)?;
                    upper.sync(false)?;
                }
            }
            Err(VfsError::NotFound) if self.lower_has_child(request.name)? => self
                .fs
                .backend
                .as_ref()
                .ok_or_else(OverlayFilesystem::read_only_error)?
                .whiteout(
                    self.fs
                        .topology
                        .work
                        .as_ref()
                        .ok_or_else(OverlayFilesystem::read_only_error)?,
                    &upper,
                    request.name,
                )?,
            Err(error) => return Err(error),
        }
        self.fs.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    fn rename(&self, request: RenameRequest<'_>) -> VfsResult<()> {
        let source_upper = self.upper_location()?;
        let destination = request.dst_dir.downcast::<OverlayDir>()?;
        let destination_upper = destination.upper_location()?;
        let source_dir = source_upper.entry().as_dir()?;
        // A lower-only source is first fully materialized.  Moving a lower
        // dentry directly would either mutate the lower filesystem or let it
        // reappear at the old name.
        let source_from_lower = if let Ok(file) = request.src.downcast::<OverlayFile>() {
            let lower = !file.layers.lower.is_empty();
            let _ = file.upper()?;
            lower
        } else if let Ok(dir) = request.src.downcast::<OverlayDir>() {
            let lower = !dir.layers.lower.is_empty();
            let copied = dir.upper_location()?;
            if lower {
                if self.fs.topology.features.redirect_dir {
                    let target = redirect_path(&dir.layers.lower)?;
                    self.fs
                        .backend
                        .as_ref()
                        .ok_or_else(OverlayFilesystem::read_only_error)?
                        .set_redirect(&copied, &target)?;
                } else {
                    dir.materialize_lower_tree()?;
                }
            }
            lower
        } else {
            return Err(VfsError::InvalidInput);
        };
        let source = source_dir.lookup(request.src_name)?;
        let destination_dir = destination_upper.entry().as_dir()?;
        let existing = match destination_dir.lookup(request.dst_name) {
            Ok(entry) => Some(entry),
            Err(VfsError::NotFound) => None,
            Err(error) => return Err(error),
        };
        let native_whiteout = source_from_lower
            && source_dir.supports_rename_whiteout()
            && destination_dir.supports_rename_whiteout();
        if source_from_lower && !native_whiteout {
            // Commit a provider-owned suppression record before the move.  It
            // closes the crash interval that a generic underlying filesystem
            // cannot express as Linux's RENAME_WHITEOUT in one primitive.
            self.fs
                .backend
                .as_ref()
                .ok_or_else(OverlayFilesystem::read_only_error)?
                .add_tombstone(&source_upper, request.src_name)?;
        }
        if native_whiteout {
            source_dir.rename_whiteout(
                request.src_name,
                &source,
                destination_dir,
                request.dst_name,
                existing.as_ref(),
            )?;
        } else {
            source_dir.rename(
                request.src_name,
                &source,
                destination_dir,
                request.dst_name,
                existing.as_ref(),
            )?;
        }
        if source_from_lower && !native_whiteout {
            // The generic VFS has no RENAME_WHITEOUT primitive yet.  The
            // source is already detached from its old upper name; publish the
            // durable whiteout immediately before allowing the operation to
            // return so later pathwalks cannot resurrect the lower source.
            self.fs
                .backend
                .as_ref()
                .ok_or_else(OverlayFilesystem::read_only_error)?
                .whiteout(
                    self.fs
                        .topology
                        .work
                        .as_ref()
                        .ok_or_else(OverlayFilesystem::read_only_error)?,
                    &source_upper,
                    request.src_name,
                )?;
        }
        source_upper.sync(false)?;
        destination_upper.sync(false)?;
        self.fs.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn supports_rename_whiteout(&self) -> bool {
        self.upper_location().is_ok_and(|upper| {
            upper
                .entry()
                .as_dir()
                .is_ok_and(|dir| dir.supports_rename_whiteout())
        })
    }

    fn supports_rename_exchange(&self) -> bool {
        self.upper_location().is_ok_and(|upper| {
            upper
                .entry()
                .as_dir()
                .is_ok_and(|dir| dir.supports_rename_exchange())
        })
    }

    fn rename_exchange(&self, request: axfs_ng_vfs::RenameExchangeRequest<'_>) -> VfsResult<()> {
        let source_upper = self.upper_location()?;
        let destination = request.dst_dir.downcast::<OverlayDir>()?;
        let destination_upper = destination.upper_location()?;
        // Both lower-only participants are fully copied up before the upper
        // transaction is entered.  Their upper origin/index state is thus
        // durable first; the only visible namespace transition is the native
        // upper exchange below.
        if let Ok(file) = request.src.downcast::<OverlayFile>() {
            let _ = file.upper()?;
        } else if let Ok(dir) = request.src.downcast::<OverlayDir>() {
            let lower = !dir.layers.lower.is_empty();
            let _ = dir.upper_location()?;
            if lower {
                dir.materialize_lower_tree()?;
            }
        } else {
            return Err(VfsError::InvalidInput);
        }
        if let Ok(file) = request.dst.downcast::<OverlayFile>() {
            let _ = file.upper()?;
        } else if let Ok(dir) = request.dst.downcast::<OverlayDir>() {
            let lower = !dir.layers.lower.is_empty();
            let _ = dir.upper_location()?;
            if lower {
                dir.materialize_lower_tree()?;
            }
        } else {
            return Err(VfsError::InvalidInput);
        }
        let source_dir = source_upper.entry().as_dir()?;
        let destination_dir = destination_upper.entry().as_dir()?;
        let source = source_dir.lookup(request.src_name)?;
        let target = destination_dir.lookup(request.dst_name)?;
        source_dir.rename_exchange(
            request.src_name,
            &source,
            destination_dir,
            request.dst_name,
            &target,
        )?;
        source_upper.sync(false)?;
        destination_upper.sync(false)?;
        self.fs.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn rename_whiteout(&self, request: axfs_ng_vfs::RenameWhiteoutRequest<'_>) -> VfsResult<()> {
        let source_upper = self.upper_location()?;
        let destination = request.dst_dir.downcast::<OverlayDir>()?;
        let destination_upper = destination.upper_location()?;
        // Copy-up completes its origin/index bookkeeping before the upper
        // rename transaction starts. The final move plus 0:0 whiteout is then
        // one operation owned by the upper provider, so lower visibility can
        // never reappear between the two names.
        if let Ok(file) = request.src.downcast::<OverlayFile>() {
            let _ = file.upper()?;
        } else if let Ok(dir) = request.src.downcast::<OverlayDir>() {
            let lower = !dir.layers.lower.is_empty();
            let _ = dir.upper_location()?;
            if lower {
                dir.materialize_lower_tree()?;
            }
        } else {
            return Err(VfsError::InvalidInput);
        }
        let source_dir = source_upper.entry().as_dir()?;
        let destination_dir = destination_upper.entry().as_dir()?;
        let source = source_dir.lookup(request.src_name)?;
        let existing = match destination_dir.lookup(request.dst_name) {
            Ok(entry) => Some(entry),
            Err(VfsError::NotFound) => None,
            Err(error) => return Err(error),
        };
        source_dir.rename_whiteout(
            request.src_name,
            &source,
            destination_dir,
            request.dst_name,
            existing.as_ref(),
        )?;
        source_upper.sync(false)?;
        destination_upper.sync(false)?;
        self.fs.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

struct OverlayFile {
    fs: Arc<OverlayFilesystem>,
    layers: OverlayLayers,
    parent: axfs_ng_vfs::WeakDirEntry,
    name: FsNameBuf,
    copied_upper: Arc<Mutex<Option<Location>>>,
    runtime: Arc<NodeUserData>,
    origin_key: ObjectKey,
    xattr: OverlayXattr,
}

struct OverlayXattr {
    fs: Arc<OverlayFilesystem>,
    layers: OverlayLayers,
    parent: axfs_ng_vfs::WeakDirEntry,
    name: FsNameBuf,
    copied_upper: Arc<Mutex<Option<Location>>>,
    runtime: Arc<NodeUserData>,
    origin_key: ObjectKey,
}

impl OverlayXattr {
    fn active(&self) -> Location {
        self.copied_upper
            .lock()
            .clone()
            .or_else(|| self.layers.upper.clone())
            .unwrap_or_else(|| self.layers.lower[0].clone())
    }
    fn upper(&self) -> VfsResult<Location> {
        if let Some(upper) = self.copied_upper.lock().clone() {
            return Ok(upper);
        }
        let parent = self
            .parent
            .upgrade()
            .ok_or(VfsError::NotFound)?
            .downcast::<OverlayDir>()?;
        let upper_parent = parent.upper_location()?;
        let lower = self.active();
        let _copy_up = self.fs.copy_up_serial.lock();
        if let Some(upper) = self
            .copied_upper
            .lock()
            .clone()
            .or_else(|| self.layers.upper.clone())
        {
            return Ok(upper);
        }
        let origin = self.origin_key;
        let runtime = self.runtime.clone();
        let fs = self.fs.clone();
        let upper = self
            .fs
            .backend
            .as_ref()
            .ok_or_else(OverlayFilesystem::read_only_error)?
            .copy_up(
                self.fs
                    .topology
                    .work
                    .as_ref()
                    .ok_or_else(OverlayFilesystem::read_only_error)?,
                &upper_parent,
                &self.name,
                &lower,
                self.fs.topology.features,
                self.fs.topology.id_mapper.as_ref(),
                &mut |upper| fs.alias_file_runtime(origin, upper, &runtime),
            )?;
        self.fs
            .alias_file_runtime(self.origin_key, upper.object_key(), &self.runtime);
        *self.copied_upper.lock() = Some(upper.clone());
        Ok(upper)
    }
}

impl axfs_ng_vfs::XattrProvider for OverlayXattr {
    fn get_xattr(&self, name: &[u8]) -> VfsResult<Vec<u8>> {
        reject_overlay_control_xattr(name)?;
        self.active()
            .entry()
            .xattr_provider()
            .ok_or(VfsError::OperationNotSupported)?
            .get_xattr(name)
    }
    fn list_xattrs(&self) -> VfsResult<Vec<u8>> {
        visible_xattrs(
            self.active()
                .entry()
                .xattr_provider()
                .ok_or(VfsError::OperationNotSupported)?
                .list_xattrs()?,
        )
    }
    fn set_xattr(&self, name: &[u8], value: &[u8], mode: XattrSetMode) -> VfsResult<()> {
        reject_overlay_control_xattr(name)?;
        self.upper()?
            .entry()
            .xattr_provider()
            .ok_or(VfsError::OperationNotSupported)?
            .set_xattr(name, value, mode)
    }
    fn remove_xattr(&self, name: &[u8]) -> VfsResult<()> {
        reject_overlay_control_xattr(name)?;
        self.upper()?
            .entry()
            .xattr_provider()
            .ok_or(VfsError::OperationNotSupported)?
            .remove_xattr(name)
    }
}

impl OverlayFile {
    fn location(&self) -> Location {
        self.copied_upper
            .lock()
            .clone()
            .or_else(|| self.layers.upper.clone())
            .unwrap_or_else(|| self.layers.lower[0].clone())
    }

    fn upper(&self) -> VfsResult<Location> {
        if let Some(upper) = self
            .copied_upper
            .lock()
            .clone()
            .or_else(|| self.layers.upper.clone())
        {
            return Ok(upper);
        }
        let parent = self.parent.upgrade().ok_or(VfsError::NotFound)?;
        let parent = parent.downcast::<OverlayDir>()?;
        let upper_parent = parent.upper_location()?;
        let backend = self
            .fs
            .backend
            .as_ref()
            .ok_or_else(OverlayFilesystem::read_only_error)?;
        let work = self
            .fs
            .topology
            .work
            .as_ref()
            .ok_or_else(OverlayFilesystem::read_only_error)?;
        let _copy_up = self.fs.copy_up_serial.lock();
        if let Some(upper) = self
            .copied_upper
            .lock()
            .clone()
            .or_else(|| self.layers.upper.clone())
        {
            return Ok(upper);
        }
        let origin = self.origin_key;
        let runtime = self.runtime.clone();
        let fs = self.fs.clone();
        let copied = backend.copy_up(
            work,
            &upper_parent,
            &self.name,
            &self.layers.lower[0],
            self.fs.topology.features,
            self.fs.topology.id_mapper.as_ref(),
            &mut |upper| fs.alias_file_runtime(origin, upper, &runtime),
        )?;
        self.fs
            .alias_file_runtime(self.origin_key, copied.object_key(), &self.runtime);
        *self.copied_upper.lock() = Some(copied.clone());
        self.fs.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(copied)
    }
}

impl NodeOps for OverlayFile {
    fn inode(&self) -> u64 {
        self.location().inode()
    }
    fn object_key(&self) -> ObjectKey {
        self.location().object_key()
    }
    fn metadata(&self) -> VfsResult<Metadata> {
        self.location().metadata()
    }
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        self.upper()?.update_metadata(update)
    }
    fn filesystem(&self) -> &dyn FilesystemOps {
        self.fs.as_ref()
    }
    fn sync(&self, data_only: bool) -> VfsResult<()> {
        self.location().sync(data_only)
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
    fn flags(&self) -> NodeFlags {
        self.location().flags()
    }
    fn open(&self, read: bool, write: bool) -> VfsResult<()> {
        let location = if write {
            self.upper()?
        } else {
            self.location()
        };
        location.open(read, write)
    }
    fn xattr_provider(&self) -> Option<&dyn axfs_ng_vfs::XattrProvider> {
        Some(&self.xattr)
    }
    fn lock_ops(&self) -> Option<&dyn LockOps> {
        Some(self)
    }
    fn quota_ops(&self) -> Option<&dyn QuotaOps> {
        Some(self)
    }
    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        Some(self.runtime.as_ref())
    }
    fn file_attr_provider(&self) -> Option<&dyn axfs_ng_vfs::FileAttrProvider> {
        Some(self)
    }
    fn get_file_attr(&self) -> VfsResult<axfs_ng_vfs::FileAttr> {
        self.location().get_file_attr()
    }
    fn set_file_attr(&self, attr: axfs_ng_vfs::FileAttr) -> VfsResult<()> {
        self.upper()?.set_file_attr(attr)
    }
    fn get_legacy_file_flags(&self) -> VfsResult<u32> {
        self.location().get_legacy_file_flags()
    }
    fn set_legacy_file_flags(&self, flags: u32) -> VfsResult<()> {
        self.upper()?.set_legacy_file_flags(flags)
    }
}

impl axfs_ng_vfs::FileAttrProvider for OverlayFile {
    fn get_file_attr(&self) -> VfsResult<axfs_ng_vfs::FileAttr> {
        self.location().get_file_attr()
    }
    fn try_get_file_attr(&self) -> VfsResult<axfs_ng_vfs::FileAttr> {
        let Some(active_upper) = self
            .copied_upper
            .lock()
            .clone()
            .or_else(|| self.layers.upper.clone())
        else {
            return Err(VfsError::WouldBlock);
        };
        active_upper.try_get_file_attr()
    }
    fn set_file_attr(&self, attr: axfs_ng_vfs::FileAttr) -> VfsResult<()> {
        self.upper()?.set_file_attr(attr)
    }
    fn get_legacy_flags(&self) -> VfsResult<u32> {
        self.location().get_legacy_file_flags()
    }
    fn set_legacy_flags(&self, flags: u32) -> VfsResult<()> {
        self.upper()?.set_legacy_file_flags(flags)
    }
}

impl LockOps for OverlayFile {
    fn get_lock(&self, owner: u64, lock: FileLock) -> VfsResult<FileLock> {
        self.location()
            .entry()
            .as_file()?
            .lock_ops()
            .ok_or(VfsError::OperationNotSupported)?
            .get_lock(owner, lock)
    }

    fn set_lock(&self, owner: u64, lock: FileLock, wait: bool) -> VfsResult<()> {
        // A lower-only lock must not be left attached to an inode that a
        // later writer silently replaces.  Copy-up first and keep all future
        // locking on the durable upper identity.
        self.upper()?
            .entry()
            .as_file()?
            .lock_ops()
            .ok_or(VfsError::OperationNotSupported)?
            .set_lock(owner, lock, wait)
    }
}

impl QuotaOps for OverlayFile {
    fn quota_usage(&self) -> VfsResult<QuotaUsage> {
        self.location()
            .entry()
            .as_file()?
            .quota_ops()
            .ok_or(VfsError::OperationNotSupported)?
            .quota_usage()
    }
}

impl FileNodeOps for OverlayFile {
    fn mutate_range(&self, request: FileRangeRequest) -> VfsResult<()> {
        // Range mutations change allocation as well as data.  They must never
        // be forwarded to a visible lower inode: force the same serialized
        // workdir-backed copy-up used by ordinary writes, then delegate the
        // typed operation to the selected upper provider.
        self.upper()?.entry().as_file()?.mutate_range(request)
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.location().entry().as_file()?.read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.upper()?.entry().as_file()?.write_at(buf, offset)
    }
    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        self.upper()?.entry().as_file()?.append(buf)
    }
    fn set_len(&self, len: u64) -> VfsResult<()> {
        self.upper()?.entry().as_file()?.set_len(len)
    }
    fn set_symlink(&self, target: &FsPath) -> VfsResult<()> {
        self.upper()?.entry().as_file()?.set_symlink(target)
    }
}

impl Pollable for OverlayFile {
    fn poll(&self) -> IoEvents {
        self.location().poll()
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        // The visible backing location can change during copy-up, while a
        // `PollRegistration` may outlive this call.  Forwarding a borrowed
        // registration would therefore retain a reference to a temporary
        // location (or, worse, the wrong inode after copy-up).  Native
        // regular-file overlay objects have no independently wakeable state;
        // retain the readiness snapshot and publish an empty subscription.
        PollRegistration::empty()
    }
}

fn collect_overlay_dir_entries(
    parent: &Location,
    upper: bool,
    names: &mut HashSet<FsNameBuf>,
    merged: &mut Vec<(FsNameBuf, u64, NodeType)>,
) -> VfsResult<()> {
    let directory = parent.entry().as_dir()?;
    let mut failure = None;
    directory.read_dir(0, &mut |name: &FsName, ino, kind, _| {
        if name.as_bytes() == b"." || name.as_bytes() == b".." || names.contains(name) {
            return true;
        }
        let child = match directory.lookup(name) {
            Ok(child) => child,
            Err(error) => {
                failure = Some(error);
                return false;
            }
        };
        if upper {
            let whiteout = match is_whiteout(&child) {
                Ok(whiteout) => whiteout,
                Err(error) => {
                    failure = Some(error);
                    return false;
                }
            };
            if whiteout {
                let owned = match try_owned_name(name) {
                    Ok(name) => name,
                    Err(error) => {
                        failure = Some(error);
                        return false;
                    }
                };
                if names.try_reserve(1).is_err() {
                    failure = Some(VfsError::NoMemory);
                    return false;
                }
                names.insert(owned);
                return true;
            }
        }
        let owned = match try_owned_name(name) {
            Ok(name) => name,
            Err(error) => {
                failure = Some(error);
                return false;
            }
        };
        if names.try_reserve(1).is_err() || merged.try_reserve(1).is_err() {
            failure = Some(VfsError::NoMemory);
            return false;
        }
        names.insert(owned.clone());
        merged.push((owned, ino, kind));
        true
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(())
}

fn try_owned_name(name: &FsName) -> VfsResult<FsNameBuf> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(name.as_bytes().len())
        .map_err(|_| VfsError::NoMemory)?;
    bytes.extend_from_slice(name.as_bytes());
    FsNameBuf::from_vec(bytes)
}

fn get_overlay_control_xattr(entry: &DirEntry, name: &[u8]) -> VfsResult<Option<Vec<u8>>> {
    let Some(provider) = entry.xattr_provider() else {
        return Ok(None);
    };
    match provider.get_xattr(name) {
        Ok(value) => Ok(Some(value)),
        Err(VfsError::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

fn xattr_is(entry: &DirEntry, name: &[u8], expected: &[u8]) -> VfsResult<bool> {
    Ok(get_overlay_control_xattr(entry, name)?.is_some_and(|value| value == expected))
}

fn is_whiteout(entry: &DirEntry) -> VfsResult<bool> {
    if xattr_is(entry, OVERLAY_WHITEOUT, b"y")? {
        return Ok(true);
    }
    if entry.node_type() != NodeType::CharacterDevice {
        return Ok(false);
    }
    Ok(entry.metadata()?.rdev.0 == 0)
}

fn location_is_opaque(location: &Location) -> VfsResult<bool> {
    xattr_is(location.entry(), OVERLAY_OPAQUE, b"y")
}

fn location_has_tombstone(location: &Location, name: &FsName) -> VfsResult<bool> {
    Ok(
        get_overlay_control_xattr(location.entry(), OVERLAY_TOMBSTONES)?.is_some_and(|raw| {
            raw.split(|byte| *byte == 0)
                .any(|entry| entry == name.as_bytes())
        }),
    )
}

fn reject_overlay_control_xattr(name: &[u8]) -> VfsResult<()> {
    if name.starts_with(b"trusted.overlay.") {
        return Err(VfsError::OperationNotSupported);
    }
    Ok(())
}

fn visible_xattrs(raw: Vec<u8>) -> VfsResult<Vec<u8>> {
    let mut visible = Vec::new();
    for name in raw.split(|byte| *byte == 0).filter(|name| !name.is_empty()) {
        if name.starts_with(b"trusted.overlay.") {
            continue;
        }
        visible
            .try_reserve(name.len() + 1)
            .map_err(|_| VfsError::NoMemory)?;
        visible.extend_from_slice(name);
        visible.push(0);
    }
    Ok(visible)
}
