# Native namespace filesystem and proc magic links

Status: implementation and validation in progress (B1 follow-up34).

The earlier namespace objects were regular procfs files. Their real namespace
inode numbers and ioctl references worked, but their filesystem/device identity
was procfs's. Actual util-linux2.42.3 lsns consequently mistook ordinary proc
file descriptors for namespace descriptors and reported unsupported ioctls.

## Model

One private, kernel-owned `nsfs` filesystem and hidden root mount carries opaque
namespace files. Its device identity comes from the existing VFS filesystem
identity allocator; no guessed Linux device number or fake ioctl response is
used. Namespace inode IDs come from the actual retained namespace objects.
A weak dentry index reuses live inode wrappers without keeping namespaces alive
when their actual holders disappear. The hidden empty root is not inserted in
user mount graphs and cannot enumerate other namespaces.

Proc namespace nodes are real symlinks with namespace display labels. They
retain a weak target-task reference, not a frozen namespace: follow/readlink
select the task's current namespace and reuse the existing READ_FSCREDS image
access/revalidation machinery. An opened namespace target, in contrast, pins
that particular namespace. An O_PATH/no-follow source link remains dynamic.
Readlink captures its complete label in one observation rather than combining
a size from one namespace generation with bytes from another.

The generic VFS object-backed link callback returns an actual retained Location;
the label is not interpreted as a pathname. Source magic/symlink policy runs
before the callback, cross-mount policy observes a jump to the private mount,
and subsequent directory search remains admitted; final operation rights are
checked by the caller. A final regular target must not acquire an extra execute
permission check from the directory-search callback. Both ordinary resolution and
open/create-facing final-link resolution use these rules. No-MAGICLINKS,
no-SYMLINKS and no-XDEV behavior is required, not bypassed.

Only this exact private mount receives no-idmap/default internal-mount policy.
Unknown ordinary mounts still fail their existing policy lookup. Existing
namespace-kind/object ioctls and FD scope remain the same provider; related FD
creation no longer needs to borrow procfs's filesystem identity. Descriptor
readlink displays the actual namespace label. Opaque namespace files are not
byte streams and reject payload reads with EINVAL.

## Validation boundaries

The paired C base probe currently passes on host Linux: source/target type and
mode, link size, namespace-vs-proc device identity, nsfs statfs magic, labels
and truncation, non-byte-stream reads and openat2 policies. Host namespaces are
not changed. Guest-only tests additionally exercise held dynamic source links,
opened namespace lifetime, child/ancestor scope and close-on-exec behavior.
Actual lsns must emit no diagnostics, not merely exit0.

Initial kernel2618 tests passed before the dynamic-source revision. The latest
source, generic VFS policy test and guest/tool/ABI tests remain unverified.
This does not claim all nsfs ioctls, namespace inode attribute/export semantics,
or a mount-tree-to-mount-namespace constructor. Broader filesystem UID namespace
projection is not corrected by this feature. B2 container levels are not yet
accepted. No physical hardware is involved.

## Follow-up validation

After fixing two implementation issues (directory-search X_OK incorrectly
applied to a final regular target, and the source symlink missing native inode
user-data needed by OFD errseq ownership), actual signed Alpine lsns lists eight
initial namespace types plus the live child without diagnostics:
shell-lq89w57m, NS_REL_TOOLS_RC=0. The C fixture passes namespace filesystem
identity/magic, source/target metadata, labels/truncation, openat2 policy, a held
source O_PATH link changing across unshare, pinned opened target surviving its
creator's exit, ancestry/capability scope and CLOEXEC assertions.

Related VFS and kernel2618 host tests and q35 lint pass (784 existing warnings).
The generic directory-continuation test initially reused a cached old target;
it now uses a fresh fixture, without altering production cache rules. Full
guest/ABI regression after these changes is pending. This is KVM/host evidence,
not hardware or container-level acceptance.

Final runtime regression passes: KVM guest68/68 without skips/normal shutdown
(system-1oroyxtd), full ABI257/257 on both guests (abi-zshngyku). Related host
checks are axfs177, VFS27 and kernel2618 tests; q35 lint retains784 existing
warnings. The signed-tool fixture above passes on the same repaired runtime
implementation. Contract metadata was synchronized without promoting progress
counters. Native physical hardware and B2 container levels remain untested.
