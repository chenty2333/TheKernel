/*
 * Linux v7.2.3 mount/quota/fanotify UAPI surface, asserted through raw
 * syscalls only.  Covers open_tree(428), fsconfig(431), fsmount(432),
 * fspick(433), statmount(457), quotactl(179), quotactl_fd(443),
 * pivot_root(155), fanotify_init(300) and fanotify_mark(301).
 *
 * open_tree_attr(467) is owned by fsattrs-differential.c (case
 * open-tree-attr.raw-differential); mount(165) and umount2(166) are out of
 * scope for this program.  Every assertion below is a property of the
 * reference fs/namespace.c, fs/fsopen.c, fs/quota/quota.c and
 * fs/notify/fanotify/fanotify_user.c in 7.2.3, in the order those files apply
 * it, so the same binary must pass on both guests.
 *
 * Two facilities are configuration-dependent in the reference build:
 * fanotify needs CONFIG_FANOTIFY and quota needs CONFIG_QUOTA.  A guest
 * without them answers ENOSYS, so both cases probe the facility first and keep
 * the assertion set identical while passing vacuously - there is nothing to
 * compare when the reference kernel does not implement the entry point.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* Native x86_64 UAPI, independent of the build host's libc headers. */
enum {
    NR_PIVOT_ROOT = 155,
    NR_QUOTACTL = 179,
    NR_FANOTIFY_INIT = 300,
    NR_FANOTIFY_MARK = 301,
    NR_OPEN_TREE = 428,
    NR_FSOPEN = 430,
    NR_FSCONFIG = 431,
    NR_FSMOUNT = 432,
    NR_FSPICK = 433,
    NR_QUOTACTL_FD = 443,
    NR_STATMOUNT = 457,
};
#define OPEN_TREE_CLONE 0x1U
#define OPEN_TREE_NAMESPACE 0x2U
#define OPEN_TREE_CLOEXEC O_CLOEXEC
#define AT_RECURSIVE 0x8000
#define FSOPEN_CLOEXEC 0x1U
#define FSMOUNT_CLOEXEC 0x1U
#define FSMOUNT_NAMESPACE 0x2U
#define FSPICK_CLOEXEC 0x1U
#define FSCONFIG_SET_FLAG 0U
#define FSCONFIG_SET_STRING 1U
#define FSCONFIG_SET_BINARY 2U
#define FSCONFIG_SET_PATH 3U
#define FSCONFIG_SET_PATH_EMPTY 4U
#define FSCONFIG_SET_FD 5U
#define FSCONFIG_CMD_CREATE 6U
#define FSCONFIG_CMD_RECONFIGURE 7U
#define FSCONFIG_CMD_CREATE_EXCL 8U
#define STATMOUNT_BY_FD 0x1U
struct mnt_id_req {
    uint32_t size;
    uint32_t mnt_fd; /* union with mnt_ns_fd */
    uint64_t mnt_id;
    uint64_t param;
    uint64_t mnt_ns_id;
};
_Static_assert(sizeof(struct mnt_id_req) == 32, "mnt_id_req is MNT_ID_REQ_SIZE_VER1");
struct statmount {
    uint32_t size, mnt_opts;
    uint64_t mask;
    uint32_t sb_dev_major, sb_dev_minor;
    uint64_t sb_magic;
    uint32_t sb_flags, fs_type;
    uint64_t mnt_id, mnt_parent_id;
    uint32_t mnt_id_old, mnt_parent_id_old;
    uint64_t mnt_attr, mnt_propagation, mnt_peer_group, mnt_master, propagate_from;
    uint32_t mnt_root, mnt_point;
    uint64_t mnt_ns_id;
    uint64_t spare2[49];
};
_Static_assert(sizeof(struct statmount) == 512, "statmount prefix is 512 bytes");
#define STATMOUNT_SB_BASIC 0x001ULL
#define STATMOUNT_MNT_BASIC 0x002ULL
#define STATMOUNT_MNT_ROOT 0x008ULL
#define STATMOUNT_MNT_POINT 0x010ULL
#define STATMOUNT_FS_TYPE 0x020ULL
#define STATMOUNT_STRING_REQ (STATMOUNT_MNT_ROOT | STATMOUNT_MNT_POINT | STATMOUNT_FS_TYPE)
#define QCMD(command, type) (((uint32_t)(command) << 8) | (uint32_t)(type))
#define Q_SYNC 0x800001U
#define Q_QUOTAON 0x800002U
#define Q_QUOTAOFF 0x800003U
#define Q_GETFMT 0x800004U
#define Q_GETINFO 0x800005U
#define Q_SETINFO 0x800006U
#define Q_GETQUOTA 0x800007U
#define Q_SETQUOTA 0x800008U
#define Q_GETNEXTQUOTA 0x800009U
/* XQM_CMD(x) is ('X' << 8) + x, so the XFS selectors are 0x5801..0x5809. */
#define Q_XQUOTAON 0x5801U
#define Q_XQUOTAOFF 0x5802U
#define Q_XGETQUOTA 0x5803U
#define Q_XSETQLIM 0x5804U
#define Q_XGETQSTAT 0x5805U
#define Q_XQUOTARM 0x5806U
#define Q_XQUOTASYNC 0x5807U
#define Q_XGETQSTATV 0x5808U
#define Q_XGETNEXTQUOTA 0x5809U
#define USRQUOTA 0
#define QFMT_VFS_V1 4
#define FAN_CLOEXEC 0x1U
#define FAN_CLASS_NOTIF 0U
#define FAN_CLASS_CONTENT 0x4U
#define FAN_CLASS_PRE_CONTENT 0x8U
/* include/uapi/linux/fanotify.h: marks 83-100, events 8-38, init 45-69. */
#define FAN_MARK_ADD 0x1U
#define FAN_MARK_REMOVE 0x2U
#define FAN_MARK_ONLYDIR 0x8U
#define FAN_MARK_MOUNT 0x10U
#define FAN_MARK_FLUSH 0x80U
#define FAN_MARK_MNTNS 0x110U
#define FAN_ACCESS 0x1ULL
#define FAN_ATTRIB 0x4ULL
#define FAN_Q_OVERFLOW 0x4000ULL
#define FAN_OPEN_PERM 0x10000ULL
#define FAN_ONDIR 0x40000000ULL
#define FAN_MNT_ATTACH 0x1000000ULL
_Static_assert(FAN_MARK_ONLYDIR == 0x8 && FAN_MARK_FLUSH == 0x80 && FAN_OPEN_PERM == 0x10000ULL,
               "fanotify mark bits must match include/uapi/linux/fanotify.h");
#define BAD ((void *)(uintptr_t)1)
#define BAD_FLAGS 0x80000000U

static const char *active;
static char dir[] = "/root/thekernel-mount-api-XXXXXX";
static char file_path[sizeof(dir) + 8];
static int dfd = -1, file_fd = -1, pipe_fd[2] = { -1, -1 };
/* Big enough for the largest XFS quota reply (fs_quota_statv, 160 bytes) and
 * addressable from the unprivileged child bodies. */
static uint8_t format_buffer[256];
static int fanotify_absent, quota_absent, quota_active;

static void cleanup(void) {
    if (file_fd >= 0) (void)close(file_fd);
    if (dfd >= 0) {
        (void)unlinkat(dfd, "file", 0);
        (void)unlinkat(dfd, "target", AT_REMOVEDIR);
        (void)close(dfd);
    }
    if (pipe_fd[0] >= 0) (void)close(pipe_fd[0]);
    if (pipe_fd[1] >= 0) (void)close(pipe_fd[1]);
    (void)rmdir(dir);
}
static void begin(const char *id) {
    active = id;
    printf("THEKERNEL_ABI_CASE %s\n", active);
}
static void mark(const char *id) { printf("THEKERNEL_ABI_ASSERT %s %s pass\n", active, id); }
static void done(void) { printf("THEKERNEL_ABI_RESULT %s pass\n", active); }
static void check(int ok, const char *stage) {
    if (!ok) {
        fprintf(stderr, "THEKERNEL_MOUNT_API_FAIL %s %s errno=%d (%s)\n", active, stage, errno,
                strerror(errno));
        exit(1);
    }
}
#define ERROR(call, expected, stage) do { errno = 0; long r_ = (long)(call); \
    check(r_ == -1 && errno == (expected), (stage)); } while (0)
/* CONFIG_FANOTIFY=n answers ENOSYS; the assertion set stays identical. */
#define FAN_ERROR(call, expected, stage) do { errno = 0; long r_ = (long)(call); \
    check(r_ == -1 && errno == (fanotify_absent ? ENOSYS : (expected)), (stage)); } while (0)
/* CONFIG_QUOTA=n answers ENOSYS; an active quota type changes the errno. */
static int quota_errno(int inactive, int active_errno) {
    if (quota_absent) return ENOSYS;
    return quota_active ? active_errno : inactive;
}
#define QUOTA_ERROR(call, inactive, active_errno, stage) do { errno = 0; long r_ = (long)(call); \
    check(r_ == -1 && errno == quota_errno((inactive), (active_errno)), (stage)); } while (0)

/* Q_XGETQSTAT reports ENOSYS while no quota type is active and renders the
 * struct once one is; a group without CONFIG_QUOTA is always ENOSYS. */
#define QUOTA_ACTIVE_ERROR(call, stage) do { errno = 0; long r_ = (long)(call); \
    check(quota_absent ? (r_ == -1 && errno == ENOSYS) : \
          (quota_active ? r_ == 0 : (r_ == -1 && errno == ENOSYS)), (stage)); } while (0)

/* The capability decisions of open_tree(2) and fsmount(2), and the privilege
 * table quotactl_fd shares with quotactl(2), are only observable without
 * CAP_SYS_ADMIN, so those cells run in a child that dropped to nobody. */
static int child_probe_rc;

/* Runs `body` in a child with uid/gid 65534 and returns the child call's
 * non-negative result or its negated errno. */
static int unprivileged_child(void (*body)(void)) {
    int fds[2];
    check(pipe(fds) == 0, "unprivileged-pipe");
    pid_t child = fork();
    check(child >= 0, "unprivileged-fork");
    if (child == 0) {
        (void)close(fds[0]);
        int reported = 0;
        if (setgroups(0, NULL) == 0 && setgid(65534) == 0 && setuid(65534) == 0) {
            errno = 0;
            body();
            reported = child_probe_rc == -1 ? -errno : child_probe_rc;
        }
        (void)!write(fds[1], &reported, sizeof(reported));
        _exit(0);
    }
    (void)close(fds[1]);
    int reported = 0;
    check(read(fds[0], &reported, sizeof(reported)) == (ssize_t)sizeof(reported), "unprivileged-read");
    (void)close(fds[0]);
    int status = 0;
    check(waitpid(child, &status, 0) == child, "unprivileged-wait");
    return reported;
}

static void body_open_tree_namespace(void) {
    child_probe_rc = (int)syscall(NR_OPEN_TREE, dfd, "", OPEN_TREE_NAMESPACE | AT_EMPTY_PATH);
}
static void body_fsmount_attr(void) {
    child_probe_rc = (int)syscall(NR_FSMOUNT, -1, 0, BAD_FLAGS);
}
static void body_fsmount_namespace_attr(void) {
    child_probe_rc = (int)syscall(NR_FSMOUNT, -1, FSMOUNT_NAMESPACE, BAD_FLAGS);
}
static void body_fsmount_flags_attr(void) {
    child_probe_rc = (int)syscall(NR_FSMOUNT, -1, BAD_FLAGS, BAD_FLAGS);
}
static void body_quota_setinfo(void) {
    child_probe_rc = (int)syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_SETINFO, USRQUOTA), 0, &format_buffer);
}
static void body_quota_getquota_other(void) {
    child_probe_rc = (int)syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_GETQUOTA, USRQUOTA), 0, &format_buffer);
}
static void body_quota_getnext(void) {
    child_probe_rc =
        (int)syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_GETNEXTQUOTA, USRQUOTA), 0, &format_buffer);
}
static void body_quota_xgetquota_other(void) {
    child_probe_rc =
        (int)syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_XGETQUOTA, USRQUOTA), 0, &format_buffer);
}
static void body_quota_quotaon(void) {
    child_probe_rc =
        (int)syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_QUOTAON, USRQUOTA), QFMT_VFS_V1, &format_buffer);
}
static void body_quota_xgetqstat(void) {
    child_probe_rc =
        (int)syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_XGETQSTAT, USRQUOTA), 0, &format_buffer);
}
/* The errno a call that must fail in the unprivileged child left, or 0. */
static int unprivileged_errno(void (*body)(void)) {
    int reported = unprivileged_child(body);
    return reported < 0 ? -reported : 0;
}

int main(void) {
    active = "mount-api.setup";
    check(mkdtemp(dir) != NULL, "mkdir");
    check(atexit(cleanup) == 0, "cleanup-register");
    dfd = open(dir, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    check(dfd >= 0, "directory-open");
    check(snprintf(file_path, sizeof(file_path), "%s/file", dir) > 0, "path-format");
    file_fd = openat(dfd, "file", O_CREAT | O_EXCL | O_RDWR | O_CLOEXEC, 0600);
    check(file_fd >= 0, "file-open");
    check(mkdirat(dfd, "target", 0700) == 0, "target-mkdir");
    check(pipe2(pipe_fd, O_CLOEXEC) == 0, "pipe");

    /* 428: vfs_open_tree() admits the flag word before it copies the pathname,
     * and AT_RECURSIVE needs one of the clone flags. */
    begin("open-tree.raw-differential");
    int tree = (int)syscall(NR_OPEN_TREE, dfd, "", OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC | AT_EMPTY_PATH);
    check(tree >= 0, "clone");
    struct stat cloned, source;
    check(fstat(tree, &cloned) == 0 && fstat(dfd, &source) == 0, "clone-stat");
    check(fcntl(tree, F_GETFD) == FD_CLOEXEC && S_ISDIR(cloned.st_mode) &&
          cloned.st_ino == source.st_ino && cloned.st_dev == source.st_dev, "clone-identity");
    check(close(tree) == 0, "clone-close");
    mark("CLONE_CLOEXEC_IDENTITY");
    ERROR(syscall(NR_OPEN_TREE, -1, BAD, BAD_FLAGS, NULL, 0), EINVAL, "flags-before-path");
    ERROR(syscall(NR_OPEN_TREE, -1, BAD, OPEN_TREE_CLONE | 0x40000000, NULL, 0), EINVAL,
          "unknown-flag-before-path");
    mark("FLAG_BITS_BEFORE_PATH");
    /* vfs_open_tree() decides the namespace capability before it copies the
     * pathname, and OPEN_TREE_NAMESPACE is anchored on CAP_SYS_ADMIN in the
     * caller's current user namespace rather than on may_mount(). */
    ERROR(syscall(NR_OPEN_TREE, -1, BAD, OPEN_TREE_NAMESPACE, NULL, 0), EFAULT,
          "namespace-before-path");
    check(unprivileged_errno(body_open_tree_namespace) == EPERM, "namespace-unprivileged");
    ERROR(syscall(NR_OPEN_TREE, dfd, "", AT_EMPTY_PATH | AT_RECURSIVE, NULL, 0), EINVAL,
          "recursive-without-clone");
    mark("RECURSIVE_REQUIRES_CLONE");
    done();

    /* 431: SYSCALL_DEFINE5(fsconfig) checks fd < 0, then the per-command shape,
     * then the descriptor, then the key/value copies. */
    begin("fsconfig.raw-differential");
    int ctx = (int)syscall(NR_FSOPEN, "tmpfs", 0);
    check(ctx >= 0, "fsopen-tmpfs");
    char blob[4] = { 1, 2, 3, 4 };
    ERROR(syscall(NR_FSCONFIG, -1, FSCONFIG_SET_STRING, "key", "value", 0), EINVAL, "negative-fd");
    ERROR(syscall(NR_FSCONFIG, ctx, FSCONFIG_SET_STRING, NULL, "value", 0), EINVAL, "null-key");
    ERROR(syscall(NR_FSCONFIG, ctx, FSCONFIG_SET_STRING, "key", NULL, 0), EINVAL, "null-value");
    ERROR(syscall(NR_FSCONFIG, ctx, FSCONFIG_SET_STRING, "key", "value", 1), EINVAL, "string-aux");
    ERROR(syscall(NR_FSCONFIG, ctx, FSCONFIG_SET_FLAG, "key", blob, 0), EINVAL, "flag-value");
    ERROR(syscall(NR_FSCONFIG, ctx, FSCONFIG_SET_BINARY, "key", blob, 0), EINVAL, "binary-aux");
    ERROR(syscall(NR_FSCONFIG, ctx, FSCONFIG_SET_BINARY, "key", blob, 1024 * 1024 + 1), EINVAL,
          "binary-too-large");
    ERROR(syscall(NR_FSCONFIG, ctx, FSCONFIG_CMD_CREATE, "key", NULL, 0), EINVAL, "create-key");
    ERROR(syscall(NR_FSCONFIG, ctx, FSCONFIG_CMD_RECONFIGURE, NULL, blob, 0), EINVAL,
          "reconfigure-value");
    ERROR(syscall(NR_FSCONFIG, ctx, FSCONFIG_SET_PATH, "key", "/", -2), EINVAL, "path-aux");
    ERROR(syscall(NR_FSCONFIG, ctx, FSCONFIG_SET_FD, "key", blob, -1), EINVAL, "fd-value");
    ERROR(syscall(NR_FSCONFIG, ctx, FSCONFIG_SET_STRING, "key", BAD, 0), EFAULT, "value-copy");
    mark("SHAPE_AND_COPY_ORDER");
    ERROR(syscall(NR_FSCONFIG, ctx, 0x7fffffff, NULL, NULL, 0), EOPNOTSUPP, "unknown-command");
    mark("UNKNOWN_COMMAND_EOPNOTSUPP");
    ERROR(syscall(NR_FSCONFIG, pipe_fd[0], FSCONFIG_SET_STRING, "key", "value", 0), EINVAL,
          "not-a-context");
    ERROR(syscall(NR_FSCONFIG, 1 << 30, FSCONFIG_SET_STRING, "key", "value", 0), EBADF, "bad-fd");
    mark("CONTEXT_FD_EINVAL");
    /* `vfs_cmd_reconfigure()` requires FS_CONTEXT_RECONF_PARAMS
     * (fs/fsopen.c:262-263), so a context that is still collecting creation
     * parameters answers EBUSY rather than EINVAL. */
    ERROR(syscall(NR_FSCONFIG, ctx, FSCONFIG_CMD_RECONFIGURE, NULL, NULL, 0), EBUSY,
          "reconfigure-before-create");
    check(close(ctx) == 0, "ctx-close");
    done();

    /* 432: SYSCALL_DEFINE3(fsmount) validates both flag words before the
     * descriptor, and an uncreated context has no root. */
    begin("fsmount.raw-differential");
    ctx = (int)syscall(NR_FSOPEN, "tmpfs", 0);
    check(ctx >= 0, "fsopen-tmpfs");
    ERROR(syscall(NR_FSMOUNT, -1, BAD_FLAGS, 0), EINVAL, "unknown-flag");
    ERROR(syscall(NR_FSMOUNT, -1, 0, BAD_FLAGS), EINVAL, "unknown-attr");
    ERROR(syscall(NR_FSMOUNT, -1, 0, 0), EBADF, "bad-fd");
    ERROR(syscall(NR_FSMOUNT, pipe_fd[0], 0, 0), EINVAL, "not-a-context");
    mark("SCALARS_BEFORE_DESCRIPTOR");
    /* fsmount() checks CAP_SYS_ADMIN in the caller's current user namespace
     * after the flag word and before the attribute word, for both the
     * FSMOUNT_NAMESPACE form and the mount-attribute form. */
    check(unprivileged_errno(body_fsmount_flags_attr) == EINVAL, "flags-before-capability");
    check(unprivileged_errno(body_fsmount_attr) == EPERM, "attributes-after-capability");
    check(unprivileged_errno(body_fsmount_namespace_attr) == EPERM,
          "namespace-attributes-after-capability");
    ERROR(syscall(NR_FSMOUNT, -1, FSMOUNT_NAMESPACE, BAD_FLAGS), EINVAL,
          "namespace-attributes-capable");
    /* fs/namespace.c:4479-4484 fetches the descriptor after the two flag
     * words, so the FSMOUNT_NAMESPACE form reports the descriptor error too. */
    ERROR(syscall(NR_FSMOUNT, -1, FSMOUNT_NAMESPACE, 0), EBADF, "namespace-bad-fd");
    ERROR(syscall(NR_FSMOUNT, ctx, 0, 0), EINVAL, "uncreated-context");
    mark("UNCREATED_CONTEXT_EINVAL");
    check(syscall(NR_FSCONFIG, ctx, FSCONFIG_CMD_CREATE, NULL, NULL, 0) == 0, "create");
    /* The context is now FS_CONTEXT_AWAITING_MOUNT, so the reconfigure gate
     * refuses it with EBUSY as well. */
    ERROR(syscall(NR_FSCONFIG, ctx, FSCONFIG_CMD_RECONFIGURE, NULL, NULL, 0), EBUSY,
          "reconfigure-after-create");
    int mounted = (int)syscall(NR_FSMOUNT, ctx, FSMOUNT_CLOEXEC, 0);
    check(mounted >= 0, "mount");
    check(fcntl(mounted, F_GETFD) == FD_CLOEXEC, "mount-cloexec");
    check(close(mounted) == 0, "mount-close");
    mark("TMPFS_CLONE_CLOEXEC");
    check(close(ctx) == 0, "ctx-close");
    done();

    /* 433: vfs_fspick() takes may_mount() and the flag word before the walk,
     * and only a mount root can be reconfigured. */
    begin("fspick.raw-differential");
    ERROR(syscall(NR_FSPICK, -1, BAD, BAD_FLAGS), EINVAL, "flags-before-path");
    ERROR(syscall(NR_FSPICK, AT_FDCWD, NULL, 0), EFAULT, "null-path");
    ERROR(syscall(NR_FSPICK, AT_FDCWD, "/nonexistent-mount-api", 0), ENOENT, "missing-path");
    mark("FLAGS_BEFORE_PATH");
    ERROR(syscall(NR_FSPICK, AT_FDCWD, file_path, 0), EINVAL, "regular-file");
    ERROR(syscall(NR_FSPICK, AT_FDCWD, dir, 0), EINVAL, "directory-below-root");
    mark("MOUNT_ROOT_ONLY");
    ctx = (int)syscall(NR_FSPICK, AT_FDCWD, "/", FSPICK_CLOEXEC);
    check(ctx >= 0, "pick-root");
    check(fcntl(ctx, F_GETFD) == FD_CLOEXEC, "pick-cloexec");
    /* fspick(2) leaves the context in `FS_CONTEXT_RECONF_PARAMS`
     * (fs/fsopen.c:200) while `fc->root` is already the picked superblock root
     * (fs/fs_context.c:288-291), so fsmount(2) reaches its phase test
     * (fs/namespace.c:4499-4501) and answers EBUSY instead of mounting the
     * same superblock a second time. */
    ERROR(syscall(NR_FSMOUNT, ctx, 0, 0), EBUSY, "reconfigure-context-busy");
    check(close(ctx) == 0, "pick-close");
    mark("CLOEXEC_CONTEXT_FD");
    done();

    /* 457: copy_mnt_id_req() splits the two request forms, STATMOUNT_BY_FD
     * takes the descriptor from the offset-4 union, and prepare_kstatmount()
     * refuses the fixed-size buffer for a string request. */
    begin("statmount.raw-differential");
    struct mnt_id_req req;
    struct statmount prefix;
    uint8_t buffer[1024];
    memset(&req, 0, sizeof(req));
    req.size = sizeof(req);
    req.param = STATMOUNT_MNT_BASIC;
    ERROR(syscall(NR_STATMOUNT, BAD, buffer, sizeof(buffer), 0x2), EINVAL, "flags-before-request");
    ERROR(syscall(NR_STATMOUNT, BAD, buffer, sizeof(buffer), 0), EFAULT, "request-copy");
    ERROR(syscall(NR_STATMOUNT, &req, buffer, sizeof(buffer), 0), EINVAL, "zero-mount-id");
    mark("FLAGS_BEFORE_REQUEST");
    req.mnt_fd = 0x7fffffffU;
    req.mnt_id = 1;
    ERROR(syscall(NR_STATMOUNT, &req, buffer, sizeof(buffer), STATMOUNT_BY_FD), EINVAL,
          "by-fd-with-mnt-id");
    req.mnt_id = 0;
    req.mnt_ns_id = 1;
    ERROR(syscall(NR_STATMOUNT, &req, buffer, sizeof(buffer), STATMOUNT_BY_FD), EINVAL,
          "by-fd-with-ns-id");
    req.mnt_ns_id = 0;
    mark("BY_FD_REQUEST_EXCLUSIVE");
    ERROR(syscall(NR_STATMOUNT, &req, buffer, sizeof(buffer), STATMOUNT_BY_FD), EBADF, "by-fd-bad-fd");
    mark("BY_FD_EBADF");
    /* do_statmount() has no namespace for a descriptor that is not a mount
     * (EINVAL) and adopts an anonymous one for a detached mount, whose root
     * has no child to find (ENOENT). */
    req.mnt_fd = (uint32_t)pipe_fd[0];
    ERROR(syscall(NR_STATMOUNT, &req, buffer, sizeof(buffer), STATMOUNT_BY_FD), EINVAL, "by-fd-pipe");
    int detached = (int)syscall(NR_OPEN_TREE, dfd, "", OPEN_TREE_CLONE | AT_EMPTY_PATH);
    check(detached >= 0, "detached-open");
    req.mnt_fd = (uint32_t)detached;
    ERROR(syscall(NR_STATMOUNT, &req, buffer, sizeof(buffer), STATMOUNT_BY_FD), ENOENT,
          "by-fd-detached");
    check(close(detached) == 0, "detached-close");
    req.mnt_fd = (uint32_t)dfd;
    req.param = STATMOUNT_STRING_REQ;
    ERROR(syscall(NR_STATMOUNT, &req, buffer, 512, STATMOUNT_BY_FD), EOVERFLOW, "string-needs-room");
    mark("STRING_REQ_EOVERFLOW");
    req.param = STATMOUNT_MNT_BASIC | STATMOUNT_SB_BASIC;
    check(syscall(NR_STATMOUNT, &req, buffer, sizeof(buffer), STATMOUNT_BY_FD) == 0, "by-fd");
    memcpy(&prefix, buffer, sizeof(prefix));
    check(prefix.size == sizeof(prefix) && prefix.mask & STATMOUNT_MNT_BASIC && prefix.mnt_id != 0 &&
          prefix.mask & STATMOUNT_SB_BASIC && prefix.sb_magic != 0, "by-fd-prefix");
    mark("BY_FD_PREFIX_MASK");
    done();

    /* 179: SYSCALL_DEFINE4(quotactl) splits the command, validates the type,
     * treats a NULL device specially and runs lookup_bdev() - never an ordinary
     * file - before the provider sees the command. */
    begin("quotactl.raw-differential");
    uint32_t format = 0;
    ERROR(syscall(NR_QUOTACTL, QCMD(Q_GETQUOTA, 3), BAD, 0, NULL), EINVAL, "type-before-path");
    ERROR(syscall(NR_QUOTACTL, 0x123456, BAD, 0, NULL), EINVAL, "type-before-device");
    mark("TYPE_BEFORE_PATH");
    ERROR(syscall(NR_QUOTACTL, QCMD(Q_GETFMT, USRQUOTA), dir, 0, &format), ENOTBLK, "directory");
    ERROR(syscall(NR_QUOTACTL, QCMD(Q_GETFMT, USRQUOTA), file_path, 0, &format), ENOTBLK, "file");
    ERROR(syscall(NR_QUOTACTL, QCMD(Q_GETQUOTA, USRQUOTA), file_path, 0, &format), ENOTBLK,
          "query-file");
    ERROR(syscall(NR_QUOTACTL, QCMD(Q_GETFMT, 1), file_path, 0, &format), ENOTBLK, "group-type");
    mark("BLOCK_DEVICE_REQUIRED");
    ERROR(syscall(NR_QUOTACTL, QCMD(Q_GETFMT, USRQUOTA), "/nonexistent-mount-api", 0, &format),
          ENOENT, "missing-device");
    ERROR(syscall(NR_QUOTACTL, QCMD(Q_QUOTAOFF, USRQUOTA), NULL, 0, NULL), ENODEV, "off-no-device");
    ERROR(syscall(NR_QUOTACTL, QCMD(Q_GETFMT, USRQUOTA), NULL, 0, &format), ENODEV, "fmt-no-device");
    mark("NULL_SPECIAL_ERRNO");
    check(syscall(NR_QUOTACTL, QCMD(Q_SYNC, USRQUOTA), NULL, 0, NULL) == 0, "sync-all");
    check(syscall(NR_QUOTACTL, QCMD(Q_SYNC, 1), NULL, 0, NULL) == 0, "sync-all-group");
    mark("SYNC_ALL_WITHOUT_SPECIAL");
    done();

    /* 443: SYSCALL_DEFINE4(quotactl_fd) reads the descriptor first, then the
     * type, then takes the mount write reference; the quota file is always
     * EINVAL because do_quotactl() receives ERR_PTR(-EINVAL). */
    begin("quotactl-fd.raw-differential");
    errno = 0;
    long probe = syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_GETFMT, USRQUOTA), 0, &format);
    if (probe == 0) quota_active = 1;
    else if (errno == ENOSYS) quota_absent = 1;
    ERROR(syscall(NR_QUOTACTL_FD, -1, QCMD(Q_GETFMT, USRQUOTA), 0, &format), EBADF, "bad-fd");
    ERROR(syscall(NR_QUOTACTL_FD, -1, 0x123456, 0, BAD), EBADF, "fd-before-type");
    ERROR(syscall(NR_QUOTACTL_FD, dfd, 0x123456, 0, BAD), EINVAL, "type-before-provider");
    mark("FD_BEFORE_TYPE");
    ERROR(syscall(NR_QUOTACTL_FD, pipe_fd[0], QCMD(Q_GETFMT, USRQUOTA), 0, &format), ENOSYS,
          "pipe-provider");
    ERROR(syscall(NR_QUOTACTL_FD, pipe_fd[1], QCMD(Q_SYNC, USRQUOTA), 0, NULL), ENOSYS, "pipe-sync");
    /* The shifted XFS selectors reach the same provider lookup. */
    ERROR(syscall(NR_QUOTACTL_FD, pipe_fd[0], QCMD(Q_XGETQUOTA, USRQUOTA), 0, format_buffer), ENOSYS,
          "pipe-xgetquota");
    ERROR(syscall(NR_QUOTACTL_FD, pipe_fd[0], QCMD(Q_XQUOTAON, USRQUOTA), 0, format_buffer), ENOSYS,
          "pipe-xquotaon");
    mark("NON_PATH_FD_ENOSYS");
    QUOTA_ERROR(syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_QUOTAON, USRQUOTA), QFMT_VFS_V1, BAD), EINVAL,
                EINVAL, "quotaon-path");
    /* Q_XQUOTAON/Q_XQUOTAOFF/Q_XQUOTARM copy their flag word before the
     * provider is consulted (EFAULT), and dquot_quota_enable()/disable()/
     * dquot_rm_xquota() answer ENOSYS because ext4 keeps no quota system
     * file. */
    ERROR(syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_XQUOTAON, USRQUOTA), 0, BAD), EFAULT, "xquotaon-copy");
    QUOTA_ERROR(syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_XQUOTAON, USRQUOTA), 0, format_buffer), ENOSYS,
                ENOSYS, "xquotaon-provider");
    QUOTA_ERROR(syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_XQUOTAOFF, USRQUOTA), 0, format_buffer), ENOSYS,
                ENOSYS, "xquotaoff-provider");
    QUOTA_ERROR(syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_XQUOTARM, USRQUOTA), 0, format_buffer), ENOSYS,
                ENOSYS, "xquotarm-provider");
    /* Q_XGETQSTATV copies the caller's version byte first, so a bad pointer is
     * EFAULT and an unknown version is EINVAL regardless of provider state. */
    ERROR(syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_XGETQSTATV, USRQUOTA), 0, BAD), EFAULT,
          "xgetqstatv-version");
    format_buffer[0] = 0;
    ERROR(syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_XGETQSTATV, USRQUOTA), 0, format_buffer), EINVAL,
          "xgetqstatv-unknown-version");
    /* Q_XQUOTASYNC only reports the read-only state of the superblock. */
    check(syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_XQUOTASYNC, USRQUOTA), 0, NULL) == 0, "xquotasync");
    QUOTA_ACTIVE_ERROR(syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_XGETQSTAT, USRQUOTA), 0, format_buffer),
                       "xgetqstat-state");
    /* check_quotactl_permission() lets an unprivileged caller read only its own
     * identity's quota, exempts Q_XGETQSTAT, and denies everything else before
     * the provider runs. */
    check(unprivileged_errno(body_quota_setinfo) == EPERM, "unprivileged-setinfo");
    check(unprivileged_errno(body_quota_getquota_other) == EPERM, "unprivileged-other-uid");
    check(unprivileged_errno(body_quota_getnext) == EPERM, "unprivileged-getnext");
    check(unprivileged_errno(body_quota_xgetquota_other) == EPERM, "unprivileged-xgetquota");
    check(unprivileged_errno(body_quota_quotaon) == EPERM, "unprivileged-quotaon");
    check(unprivileged_errno(body_quota_xgetqstat) == quota_errno(ENOSYS, 0),
          "unprivileged-xgetqstat");
    mark("QUOTAON_DEFERRED_EINVAL");
    QUOTA_ERROR(syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_GETFMT, USRQUOTA), 0, NULL), ESRCH, EFAULT,
                "state-errno");
    QUOTA_ERROR(syscall(NR_QUOTACTL_FD, dfd, QCMD(Q_GETINFO, USRQUOTA), 0, NULL), ESRCH, EFAULT,
                "info-errno");
    mark("PROVIDER_STATE_ERRNO");
    done();

    /* 155: SYSCALL_DEFINE2(pivot_root) resolves both pathnames with
     * LOOKUP_FOLLOW|LOOKUP_DIRECTORY before path_pivot_root() checks for
     * CAP_SYS_ADMIN, so a bad pointer or a non-directory is never EPERM. */
    begin("pivot-root.raw-differential");
    ERROR(syscall(NR_PIVOT_ROOT, BAD, BAD), EFAULT, "bad-pointers");
    ERROR(syscall(NR_PIVOT_ROOT, "/nonexistent-mount-api", "/"), ENOENT, "missing-new-root");
    ERROR(syscall(NR_PIVOT_ROOT, "/", "/nonexistent-mount-api"), ENOENT, "missing-put-old");
    mark("PATH_BEFORE_CAPABILITY");
    ERROR(syscall(NR_PIVOT_ROOT, file_path, "/"), ENOTDIR, "file-new-root");
    mark("LOOKUP_DIRECTORY_NEW_ROOT");
    ERROR(syscall(NR_PIVOT_ROOT, "/", file_path), ENOTDIR, "file-put-old");
    mark("LOOKUP_DIRECTORY_PUT_OLD");
    done();

    /* 300: SYSCALL_DEFINE2(fanotify_init) capability-checks the privileged
     * flag bits, validates the grammar, and installs a CLOEXEC group. */
    begin("fanotify-init.raw-differential");
    int group = (int)syscall(NR_FANOTIFY_INIT, FAN_CLASS_NOTIF | FAN_CLOEXEC, O_RDONLY);
    if (group < 0 && errno == ENOSYS) {
        fanotify_absent = 1;
    } else {
        check(group >= 0, "group");
    }
    FAN_ERROR(syscall(NR_FANOTIFY_INIT, 0x40000000, O_RDONLY), EINVAL, "unknown-flag");
    FAN_ERROR(syscall(NR_FANOTIFY_INIT, FAN_CLASS_CONTENT | FAN_CLASS_PRE_CONTENT, O_RDONLY), EINVAL,
              "class-conflict");
    mark("UNKNOWN_FLAG_EINVAL");
    mark("CLASS_CONFLICT_EINVAL");
    FAN_ERROR(syscall(NR_FANOTIFY_INIT, FAN_CLASS_NOTIF, O_ACCMODE), EINVAL, "access-mode");
    mark("ACCESS_MODE_EINVAL");
    check(fanotify_absent || fcntl(group, F_GETFD) == FD_CLOEXEC, "group-cloexec");
    mark("GROUP_FD_CLOEXEC");
    done();

    /* 301: do_fanotify_mark() settles every scalar and group rule before it
     * looks at the descriptor or the pathname. */
    begin("fanotify-mark.raw-differential");
    FAN_ERROR(syscall(NR_FANOTIFY_MARK, -1, FAN_MARK_ADD | BAD_FLAGS, FAN_ACCESS, AT_FDCWD, BAD),
              EINVAL, "unknown-flag-before-fd");
    FAN_ERROR(syscall(NR_FANOTIFY_MARK, -1, FAN_MARK_ADD, (uint64_t)1 << 32, AT_FDCWD, BAD), EINVAL,
              "upper-mask-before-fd");
    FAN_ERROR(syscall(NR_FANOTIFY_MARK, -1, FAN_MARK_ADD | FAN_MARK_REMOVE, FAN_ACCESS, AT_FDCWD, BAD),
              EINVAL, "two-commands-before-fd");
    mark("SCALARS_BEFORE_DESCRIPTOR");
    FAN_ERROR(syscall(NR_FANOTIFY_MARK, -1, FAN_MARK_ADD, FAN_ACCESS, AT_FDCWD, BAD), EBADF, "bad-fd");
    FAN_ERROR(syscall(NR_FANOTIFY_MARK, pipe_fd[0], FAN_MARK_ADD, FAN_ACCESS, AT_FDCWD, BAD), EINVAL,
              "not-a-group");
    FAN_ERROR(syscall(NR_FANOTIFY_MARK, group, FAN_MARK_ADD, FAN_ACCESS, AT_FDCWD, BAD), EFAULT,
              "path-copy");
    mark("DESCRIPTOR_BEFORE_PATH");
    FAN_ERROR(syscall(NR_FANOTIFY_MARK, group, FAN_MARK_ADD, 0, AT_FDCWD, BAD), EINVAL, "empty-mask");
    FAN_ERROR(syscall(NR_FANOTIFY_MARK, group, FAN_MARK_ADD, FAN_Q_OVERFLOW, AT_FDCWD, BAD), EINVAL,
              "overflow-mask");
    mark("EMPTY_AND_OVERFLOW_MASK");
    FAN_ERROR(syscall(NR_FANOTIFY_MARK, group, FAN_MARK_ADD | FAN_MARK_MNTNS, FAN_MNT_ATTACH,
                      AT_FDCWD, BAD),
              EINVAL, "mntns-scope");
    FAN_ERROR(syscall(NR_FANOTIFY_MARK, group, FAN_MARK_ADD, FAN_OPEN_PERM, AT_FDCWD, BAD), EINVAL,
              "perm-on-notif");
    FAN_ERROR(syscall(NR_FANOTIFY_MARK, group, FAN_MARK_ADD | FAN_MARK_MOUNT, FAN_ATTRIB, AT_FDCWD, BAD),
              EINVAL, "fid-event-on-mount");
    mark("SCOPE_RULES_BEFORE_PATH");
    FAN_ERROR(syscall(NR_FANOTIFY_MARK, group, FAN_MARK_ADD | FAN_MARK_ONLYDIR, FAN_ACCESS | FAN_ONDIR,
                      AT_FDCWD, file_path),
              ENOTDIR, "onlydir-file");
    FAN_ERROR(syscall(NR_FANOTIFY_MARK, group, FAN_MARK_ADD, FAN_ACCESS, AT_FDCWD,
                      "/nonexistent-mount-api"),
              ENOENT, "missing-path");
    check(fanotify_absent ||
              syscall(NR_FANOTIFY_MARK, group, FAN_MARK_ADD | FAN_MARK_ONLYDIR, FAN_ACCESS | FAN_ONDIR,
                      AT_FDCWD, dir) == 0,
          "add-dir-mark");
    check(fanotify_absent ||
              syscall(NR_FANOTIFY_MARK, group, FAN_MARK_REMOVE | FAN_MARK_ONLYDIR,
                      FAN_ACCESS | FAN_ONDIR, AT_FDCWD, dir) == 0,
          "remove-dir-mark");
    mark("ONLYDIR_TARGET_TYPE");
    check(fanotify_absent ||
              syscall(NR_FANOTIFY_MARK, group, FAN_MARK_FLUSH, 0, -1, BAD) == 0,
          "flush-ignores-path");
    mark("FLUSH_IGNORES_PATH");
    done();

    check(close(file_fd) == 0, "close"); file_fd = -1;
    check(unlinkat(dfd, "file", 0) == 0, "unlink-file");
    check(unlinkat(dfd, "target", AT_REMOVEDIR) == 0, "unlink-target");
    check(close(dfd) == 0, "directory-close"); dfd = -1;
    check(close(pipe_fd[0]) == 0 && close(pipe_fd[1]) == 0, "pipe-close");
    pipe_fd[0] = pipe_fd[1] = -1;
    check(rmdir(dir) == 0, "rmdir");
    puts("THEKERNEL_MOUNT_API_OK");
    return 0;
}
