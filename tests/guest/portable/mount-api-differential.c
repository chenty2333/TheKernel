/*
 * Linux v7.2.3 mount/quota/fanotify UAPI surface, asserted through raw
 * syscalls only.  Covers open_tree(428), move_mount(429), fsopen(430),
 * fsconfig(431), fsmount(432), fspick(433), mount_setattr(442), statmount(457),
 * listmount(458), quotactl(179), quotactl_fd(443), pivot_root(155),
 * fanotify_init(300) and fanotify_mark(301).
 *
 * open_tree_attr(467) is owned by fsattrs-differential.c (case
 * open-tree-attr.raw-differential).  mount(165) is out of scope for this
 * program, and umount2(166) appears only as teardown of the placement the
 * mount-descriptors case creates.  Every assertion below is a property of the
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
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/vfs.h>
#include <sys/wait.h>
#include <unistd.h>

/* Native x86_64 UAPI, independent of the build host's libc headers. */
enum {
    NR_PIVOT_ROOT = 155,
    NR_UMOUNT2 = 166,
    NR_QUOTACTL = 179,
    NR_FANOTIFY_INIT = 300,
    NR_FANOTIFY_MARK = 301,
    NR_OPEN_TREE = 428,
    NR_MOVE_MOUNT = 429,
    NR_FSOPEN = 430,
    NR_FSCONFIG = 431,
    NR_FSMOUNT = 432,
    NR_FSPICK = 433,
    NR_MOUNT_SETATTR = 442,
    NR_QUOTACTL_FD = 443,
    NR_STATMOUNT = 457,
    NR_LISTMOUNT = 458,
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
/* include/uapi/linux/mount.h:71-79, 118-141, 169, 212 and 238. */
#define MOVE_MOUNT_F_EMPTY_PATH 0x04U
#define MOVE_MOUNT_T_EMPTY_PATH 0x40U
#define MOVE_MOUNT_SET_GROUP 0x100U
#define MOVE_MOUNT_BENEATH 0x200U
#define MOUNT_ATTR_RDONLY 0x1ULL
#define MOUNT_ATTR_NOATIME 0x10ULL
#define MS_PRIVATE (1U << 18)
#define MS_SHARED (1U << 20)
#define MNT_DETACH 2
#define LISTMOUNT_REVERSE 1U
#define LSMT_ROOT (~0ULL)
/* include/uapi/linux/magic.h:6,25: the fixture's own filesystem and tmpfs. */
#define TMPFS_MAGIC 0x01021994U
#define EXT4_SUPER_MAGIC 0xEF53U
struct mnt_id_req {
    uint32_t size;
    uint32_t mnt_fd; /* union with mnt_ns_fd */
    uint64_t mnt_id;
    uint64_t param;
    uint64_t mnt_ns_id;
};
_Static_assert(sizeof(struct mnt_id_req) == 32, "mnt_id_req is MNT_ID_REQ_SIZE_VER1");
/* struct mount_attr (include/uapi/linux/mount.h:143-153) is
 * MOUNT_ATTR_SIZE_VER0 bytes with no trailing version-1 extension. */
struct mount_attr {
    uint64_t attr_set, attr_clr, propagation, userns_fd;
};
_Static_assert(sizeof(struct mount_attr) == 32, "mount_attr is MOUNT_ATTR_SIZE_VER0");
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
#define NR_MOUNT 165
#define MS_REMOUNT 32U
/* include/uapi/linux/quota.h:88-93 and include/uapi/linux/dqblk_xfs.h:9-23
 * selectors; the VFS translates them one for one in copy_from_xfs_dqblk()
 * (fs/quota/quota.c:566-591). */
#define QIF_BLIMITS 0x1U
#define QIF_ILIMITS 0x4U
#define FS_DQ_ISOFT (1U << 0)
#define FS_DQ_IHARD (1U << 1)
#define FS_DQ_BSOFT (1U << 2)
#define FS_DQ_BHARD (1U << 3)
#define FS_DQ_RTBSOFT (1U << 4)
#define FS_DQ_RTBHARD (1U << 5)
#define FS_DQ_BTIMER (1U << 6)
#define FS_DQ_ITIMER (1U << 7)
#define FS_DQ_RTBTIMER (1U << 8)
#define FS_DQ_BWARNS (1U << 9)
#define FS_DQ_IWARNS (1U << 10)
#define FS_DQ_RTBWARNS (1U << 11)
#define FS_DQ_BCOUNT (1U << 12)
#define FS_DQ_ICOUNT (1U << 13)
#define FS_DQ_RTBCOUNT (1U << 14)
#define FS_DQ_BIGTIME (1U << 15)
/* The identifier space is searchable only once a record exists in the quota
 * file's tree, so the fixtures below use this one identifier. */
#define QUOTA_ID 1000U
/* struct if_dqblk, struct if_dqinfo and struct if_nextdqblk
 * (include/uapi/linux/quota.h:25-70) and struct fs_disk_quota
 * (include/uapi/linux/dqblk_xfs.h:28-70); the syscall copies whichever view the
 * command selects, with no translation of the caller's layout. */
struct if_dqblk {
    uint64_t dqb_bhardlimit;
    uint64_t dqb_bsoftlimit;
    uint64_t dqb_curspace;
    uint64_t dqb_ihardlimit;
    uint64_t dqb_isoftlimit;
    uint64_t dqb_curinodes;
    uint64_t dqb_btime;
    uint64_t dqb_itime;
    uint32_t dqb_valid;
    uint32_t dqb_pad;
};
struct if_dqinfo {
    uint64_t dqi_bgrace;
    uint64_t dqi_igrace;
    uint32_t dqi_flags;
    uint32_t dqi_valid;
};
struct if_nextdqblk {
    uint64_t dqb_bhardlimit;
    uint64_t dqb_bsoftlimit;
    uint64_t dqb_curspace;
    uint64_t dqb_ihardlimit;
    uint64_t dqb_isoftlimit;
    uint64_t dqb_curinodes;
    uint64_t dqb_btime;
    uint64_t dqb_itime;
    uint32_t dqb_valid;
    uint32_t dqb_id;
};
struct fs_disk_quota {
    int8_t d_version;
    int8_t d_flags;
    uint16_t d_fieldmask;
    uint32_t d_id;
    uint64_t d_blk_hardlimit;
    uint64_t d_blk_softlimit;
    uint64_t d_ino_hardlimit;
    uint64_t d_ino_softlimit;
    uint64_t d_bcount;
    uint64_t d_icount;
    int32_t d_itimer;
    int32_t d_btimer;
    uint16_t d_iwarns;
    uint16_t d_bwarns;
    int8_t d_itimer_hi;
    int8_t d_btimer_hi;
    int8_t d_rtbtimer_hi;
    int8_t d_padding2;
    uint64_t d_rtb_hardlimit;
    uint64_t d_rtb_softlimit;
    uint64_t d_rtbcount;
    int32_t d_rtbtimer;
    uint16_t d_rtbwarns;
    int16_t d_padding3;
    int8_t d_padding4[8];
};
_Static_assert(sizeof(struct if_dqblk) == 72 && sizeof(struct if_dqinfo) == 24 &&
                   sizeof(struct if_nextdqblk) == 72 && sizeof(struct fs_disk_quota) == 112,
               "quota UAPI structures must match the native layouts");
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
static char target_path[sizeof(dir) + 8];
static int dfd = -1, file_fd = -1, pipe_fd[2] = { -1, -1 };
/* Big enough for the largest XFS quota reply (fs_quota_statv, 160 bytes) and
 * addressable from the unprivileged child bodies. */
static uint8_t format_buffer[256];
static int fanotify_absent, quota_absent, quota_active;
/* The mount-descriptor case keeps every object it hands to the kernel at file
 * scope, because the unprivileged probes run in a forked child. */
static uint64_t mount_ids[32], reverse_mount_ids[32];
static uint8_t statmount_buffer[1024];
static char oversized_fs_name[4097];
static struct mount_attr noop_mount_attr;
static struct mount_attr rdonly_mount_attr = { .attr_set = MOUNT_ATTR_RDONLY };
static struct mount_attr clear_rdonly_mount_attr = { .attr_clr = MOUNT_ATTR_RDONLY };
static struct mount_attr unknown_mount_attr = { .attr_set = 1ULL << 63 };
static struct mount_attr double_propagation_mount_attr = { .propagation = MS_SHARED | MS_PRIVATE };
static struct mount_attr atime_only_mount_attr = { .attr_set = MOUNT_ATTR_NOATIME };
static struct {
    struct mount_attr attr;
    uint64_t tail;
} extended_mount_attr = { .tail = 1 }, extended_mount_attr_zeroed;

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
/* The mount-descriptor admissions that Linux decides before it copies
 * anything: fsopen(2) checks may_mount() and the flag word first, move_mount(2)
 * its flag word, and mount_setattr(2) its no-op short circuit. */
static void body_fsopen(void) {
    child_probe_rc = (int)syscall(NR_FSOPEN, "tmpfs", 0);
}
static void body_fsopen_bad_flags(void) {
    child_probe_rc = (int)syscall(NR_FSOPEN, BAD, BAD_FLAGS);
}
static void body_move_mount(void) {
    child_probe_rc = (int)syscall(NR_MOVE_MOUNT, -1, BAD, -1, BAD, BAD_FLAGS);
}
static void body_mount_setattr_noop(void) {
    child_probe_rc = (int)syscall(NR_MOUNT_SETATTR, AT_FDCWD, "/", 0, &noop_mount_attr, 32);
}
/* The errno a call that must fail in the unprivileged child left, or 0. */
static int unprivileged_errno(void (*body)(void)) {
    int reported = unprivileged_child(body);
    return reported < 0 ? -reported : 0;
}

/* The smallest QFMT_VFS_V1 quota file both providers accept: the v2 header
 * (magic and version at 0), the info block with dqi_blocks = 2, and the empty
 * root pointer block behind it (fs/quota/quotaio_v2.h:12-40,
 * fs/quota/quota_v2.c:96-160).  The reference provider validates exactly this
 * shape in v2_check_quota_file()/v2_read_file_info() before it enables the
 * type, and the same header carries the grace periods Q_GETINFO reports.
 *
 * The second block must be zero: it is the trie root whose entries are block
 * references, and every reference is range-checked against
 * `[QT_TREEOFF, dqi_blocks - 1]` before it is followed
 * (fs/quota/quota_tree.c:80-88), so a stray value there makes
 * dquot_acquire()-time tree insertion fail with EUCLEAN. */
static int write_quota_file(const char *path) {
    uint8_t block[1024];
    /* magic, version, dqi_bgrace, dqi_igrace, dqi_flags, dqi_blocks. */
    const uint32_t header[6] = { 0xd9c01f11U, 1U, 604800U, 604800U, 0U, 2U };
    memset(block, 0, sizeof(block));
    memcpy(block, header, sizeof(header));
    int fd = open(path, O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0600);
    if (fd < 0) {
        return -1;
    }
    ssize_t first = write(fd, block, sizeof(block));
    memset(block, 0, sizeof(block));
    ssize_t second = write(fd, block, sizeof(block));
    if (close(fd) != 0) {
        return -1;
    }
    return first == (ssize_t)sizeof(block) && second == (ssize_t)sizeof(block) ? 0 : -1;
}

int main(void) {
    active = "mount-api.setup";
    check(mkdtemp(dir) != NULL, "mkdir");
    check(atexit(cleanup) == 0, "cleanup-register");
    dfd = open(dir, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    check(dfd >= 0, "directory-open");
    check(snprintf(file_path, sizeof(file_path), "%s/file", dir) > 0, "path-format");
    check(snprintf(target_path, sizeof(target_path), "%s/target", dir) > 0, "target-format");
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

    /* Every remaining property of the quota provider is only observable on an
     * active type, so enable one and put it back afterwards: the reference
     * provider refuses Q_QUOTAON on a filesystem which was not mounted with
     * `usrquota` (ext4_quota_on() -> test_opt(sb, QUOTA) -> -EINVAL), which is
     * why the already-mounted root is reconfigured first.  That call's own
     * result is deliberately not compared: TheKernel's provider has no such
     * mount-option gate, so only the active state it produces is shared. */
    char quota_file[sizeof(dir) + 16];
    check(snprintf(quota_file, sizeof(quota_file), "%s/aquota.user", dir) > 0, "quota-path");
    check(write_quota_file(quota_file) == 0, "quota-file");
    (void)syscall(NR_MOUNT, "none", "/", NULL, MS_REMOUNT, "usrquota");
    check(syscall(NR_QUOTACTL, QCMD(Q_QUOTAON, USRQUOTA), "/dev/vda", QFMT_VFS_V1, quota_file) == 0,
          "quotaon");
    format = 0;
    check(syscall(NR_QUOTACTL, QCMD(Q_GETFMT, USRQUOTA), "/dev/vda", 0, &format) == 0 &&
              format == QFMT_VFS_V1,
          "getfmt-active");
    mark("QUOTA_TYPE_ACTIVATION");

    struct fs_disk_quota xdq;
    struct if_dqblk idq;
    struct if_nextdqblk next;
    /* struct fs_disk_quota counts 512-byte basic blocks, struct if_dqblk
     * counts 1024-byte quota blocks, and the provider holds bytes:
     * quota_bbtob()/quota_btobb() and qbtos()/stoqb() are the two views of one
     * limit (fs/quota/quota.c:177-185, :529-537).  A one-block limit is the
     * value a lossy conversion turns into "no limit". */
    memset(&xdq, 0, sizeof(xdq));
    xdq.d_version = 1;
    xdq.d_fieldmask = FS_DQ_BHARD | FS_DQ_IHARD;
    xdq.d_blk_hardlimit = 1;
    xdq.d_ino_hardlimit = 10;
    check(syscall(NR_QUOTACTL, QCMD(Q_XSETQLIM, USRQUOTA), "/dev/vda", QUOTA_ID, &xdq) == 0,
          "xsetqlim");
    memset(&xdq, 0, sizeof(xdq));
    check(syscall(NR_QUOTACTL, QCMD(Q_XGETQUOTA, USRQUOTA), "/dev/vda", QUOTA_ID, &xdq) == 0 &&
              xdq.d_id == QUOTA_ID && xdq.d_blk_hardlimit == 1 && xdq.d_ino_hardlimit == 10,
          "xgetquota-one-block");
    memset(&idq, 0, sizeof(idq));
    check(syscall(NR_QUOTACTL, QCMD(Q_GETQUOTA, USRQUOTA), "/dev/vda", QUOTA_ID, &idq) == 0 &&
              idq.dqb_bhardlimit == 1 && idq.dqb_ihardlimit == 10,
          "getquota-one-block");
    /* Three 1024-byte quota blocks are six 512-byte basic blocks. */
    memset(&idq, 0, sizeof(idq));
    idq.dqb_valid = QIF_BLIMITS;
    idq.dqb_bhardlimit = 3;
    check(syscall(NR_QUOTACTL, QCMD(Q_SETQUOTA, USRQUOTA), "/dev/vda", QUOTA_ID, &idq) == 0,
          "setquota-blocks");
    memset(&xdq, 0, sizeof(xdq));
    check(syscall(NR_QUOTACTL, QCMD(Q_XGETQUOTA, USRQUOTA), "/dev/vda", QUOTA_ID, &xdq) == 0 &&
              xdq.d_blk_hardlimit == 6,
          "xgetquota-setquota-blocks");
    mark("QUOTA_LIMIT_UNITS");

    /* find_next_id() starts at `__get_index(info, *id, depth)`, which is `*id`
     * itself at the root of the tree, so the search is `>= id`
     * (fs/quota/quota_tree.c:792-844) as both UAPIs document it
     * (include/uapi/linux/quota.h:70-74, include/uapi/linux/dqblk_xfs.h:41-44). */
    memset(&xdq, 0, sizeof(xdq));
    check(syscall(NR_QUOTACTL, QCMD(Q_XGETNEXTQUOTA, USRQUOTA), "/dev/vda", QUOTA_ID, &xdq) == 0 &&
              xdq.d_id == QUOTA_ID,
          "xgetnextquota-at-id");
    memset(&next, 0, sizeof(next));
    check(syscall(NR_QUOTACTL, QCMD(Q_GETNEXTQUOTA, USRQUOTA), "/dev/vda", QUOTA_ID, &next) == 0 &&
              next.dqb_id == QUOTA_ID,
          "getnextquota-at-id");
    memset(&xdq, 0, sizeof(xdq));
    check(syscall(NR_QUOTACTL, QCMD(Q_XGETNEXTQUOTA, USRQUOTA), "/dev/vda", QUOTA_ID - 1, &xdq) == 0 &&
              xdq.d_id == QUOTA_ID,
          "xgetnextquota-below-id");
    mark("QUOTA_NEXT_ID_INCLUSIVE");

    /* An identifier of 0 which selects a grace period or a warning count is
     * the superblock-wide default, so it goes to ->set_info() and is stripped
     * from the field mask before the dquot is touched
     * (fs/quota/quota.c:632-668); the ID-0 dquot's own timer stays untouched. */
    struct if_dqinfo info;
    memset(&xdq, 0, sizeof(xdq));
    xdq.d_version = 1;
    xdq.d_fieldmask = FS_DQ_BTIMER;
    xdq.d_btimer = 4242;
    check(syscall(NR_QUOTACTL, QCMD(Q_XSETQLIM, USRQUOTA), "/dev/vda", 0, &xdq) == 0,
          "xsetqlim-id0-timer");
    memset(&info, 0, sizeof(info));
    check(syscall(NR_QUOTACTL, QCMD(Q_GETINFO, USRQUOTA), "/dev/vda", 0, &info) == 0 &&
              info.dqi_bgrace == 4242,
          "getinfo-grace");
    memset(&idq, 0, sizeof(idq));
    check(syscall(NR_QUOTACTL, QCMD(Q_GETQUOTA, USRQUOTA), "/dev/vda", 0, &idq) == 0 &&
              idq.dqb_btime == 0,
          "getquota-id0-btime");
    mark("QUOTA_ID_ZERO_INFO_ROUTING");

    /* The warning counts and the realtime timer have no superblock-wide
     * default, so dquot_set_dqinfo() rejects the same shape with EINVAL
     * (fs/quota/dquot.c:2893-2897); for any other identifier the selectors
     * outside VFS_QC_MASK are EINVAL in do_set_dqblk()
     * (fs/quota/dquot.c:2740-2753). */
    memset(&xdq, 0, sizeof(xdq));
    xdq.d_version = 1;
    xdq.d_fieldmask = FS_DQ_BWARNS;
    xdq.d_bwarns = 7;
    ERROR(syscall(NR_QUOTACTL, QCMD(Q_XSETQLIM, USRQUOTA), "/dev/vda", 0, &xdq), EINVAL,
          "id0-warns");
    memset(&xdq, 0, sizeof(xdq));
    xdq.d_version = 1;
    xdq.d_fieldmask = FS_DQ_RTBTIMER;
    xdq.d_rtbtimer = 11;
    ERROR(syscall(NR_QUOTACTL, QCMD(Q_XSETQLIM, USRQUOTA), "/dev/vda", 0, &xdq), EINVAL,
          "id0-realtime-timer");
    memset(&xdq, 0, sizeof(xdq));
    xdq.d_version = 1;
    xdq.d_fieldmask = FS_DQ_BWARNS;
    xdq.d_bwarns = 7;
    ERROR(syscall(NR_QUOTACTL, QCMD(Q_XSETQLIM, USRQUOTA), "/dev/vda", QUOTA_ID, &xdq), EINVAL,
          "warns");
    memset(&xdq, 0, sizeof(xdq));
    xdq.d_version = 1;
    xdq.d_fieldmask = FS_DQ_RTBHARD;
    xdq.d_rtb_hardlimit = 5;
    ERROR(syscall(NR_QUOTACTL, QCMD(Q_XSETQLIM, USRQUOTA), "/dev/vda", QUOTA_ID, &xdq), EINVAL,
          "realtime-blocks");
    mark("QUOTA_UNHANDLED_SELECTORS_EINVAL");

    /* A selected limit above the format's maximum is ERANGE before anything is
     * stored (fs/quota/dquot.c:2757-2763); QFMT_VFS_V1 installs 2^63-1 as both
     * ceilings (fs/quota/quota_v2.c:140-141), so its neighbour is accepted. */
    memset(&idq, 0, sizeof(idq));
    idq.dqb_valid = QIF_ILIMITS;
    idq.dqb_ihardlimit = 0x8000000000000000ULL;
    ERROR(syscall(NR_QUOTACTL, QCMD(Q_SETQUOTA, USRQUOTA), "/dev/vda", 2000, &idq), ERANGE,
          "setquota-over-max");
    memset(&xdq, 0, sizeof(xdq));
    xdq.d_version = 1;
    xdq.d_fieldmask = FS_DQ_IHARD;
    xdq.d_ino_hardlimit = 0x8000000000000000ULL;
    ERROR(syscall(NR_QUOTACTL, QCMD(Q_XSETQLIM, USRQUOTA), "/dev/vda", 2000, &xdq), ERANGE,
          "xsetqlim-over-max");
    memset(&idq, 0, sizeof(idq));
    idq.dqb_valid = QIF_ILIMITS;
    idq.dqb_ihardlimit = 0x7fffffffffffffffULL;
    check(syscall(NR_QUOTACTL, QCMD(Q_SETQUOTA, USRQUOTA), "/dev/vda", 2000, &idq) == 0,
          "setquota-at-max");
    memset(&idq, 0, sizeof(idq));
    check(syscall(NR_QUOTACTL, QCMD(Q_GETQUOTA, USRQUOTA), "/dev/vda", 2000, &idq) == 0 &&
              idq.dqb_ihardlimit == 0x7fffffffffffffffULL,
          "getquota-at-max");
    mark("QUOTA_OVER_MAXIMUM_ERANGE");

    /* dquot_disable() drops the active state, so Q_GETFMT reports ESRCH again
     * and the cases below see the same inactive provider the registered
     * assertions were written against. */
    check(syscall(NR_QUOTACTL, QCMD(Q_QUOTAOFF, USRQUOTA), "/dev/vda", 0, NULL) == 0, "quotaoff");
    format = 0;
    ERROR(syscall(NR_QUOTACTL, QCMD(Q_GETFMT, USRQUOTA), "/dev/vda", 0, &format), ESRCH,
          "getfmt-inactive");
    /* The quota file lives in the shared scratch directory every case uses, so
     * it has to be gone by the time the last case removes that directory. */
    check(unlink(quota_file) == 0, "quota-file-unlink");
    mark("QUOTA_TYPE_DEACTIVATION");
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

    /* 430/429/442/458: the descriptor half of the new mount API, which the
     * per-syscall cases above only use as setup.  fsopen(2) admits the caller
     * and the flag word before it copies the filesystem name, and its
     * strndup_user() answers EINVAL once the name does not terminate inside the
     * first page (fs/fsopen.c:121-146).  move_mount(2) validates the flag word
     * before either pathname, looks the target up before the source, and
     * rejects a placement whose directory-ness differs (fs/namespace.c:4577-4647
     * and 3642-3643).  mount_setattr(2) settles the attribute shape before the
     * copy-in, skips the target entirely for a no-op, and reaches the pathname
     * only through getname_flags(), so a NULL pointer is EFAULT even with
     * AT_EMPTY_PATH (fs/namespace.c:5145-5184).  listmount(2) validates the
     * flag word, the count and the request before the copy-in, and orders the
     * reply by unique mount id (fs/namespace.c:6123-6170, 5891-5922, 6026-6087).
     * umount2(166) appears only as fixture teardown for the placement this case
     * creates; its own contract belongs to another cell. */
    begin("mount-descriptors.raw-differential");
    int fs_ctx_cloexec = (int)syscall(NR_FSOPEN, "tmpfs", FSOPEN_CLOEXEC);
    int fs_ctx_plain = (int)syscall(NR_FSOPEN, "tmpfs", 0);
    check(fs_ctx_cloexec >= 0 && fs_ctx_plain >= 0 && fs_ctx_cloexec != fs_ctx_plain, "fsopen-fds");
    check(fcntl(fs_ctx_cloexec, F_GETFD) == FD_CLOEXEC && fcntl(fs_ctx_plain, F_GETFD) == 0,
          "fsopen-cloexec");
    mark("FSOPEN_CLOEXEC_DESCRIPTOR");
    ERROR(syscall(NR_FSOPEN, "tmpfs", 0x2), EINVAL, "unknown-flag");
    ERROR(syscall(NR_FSOPEN, BAD, BAD_FLAGS), EINVAL, "flags-before-name");
    mark("FSOPEN_FLAGS_BEFORE_NAME");
    ERROR(syscall(NR_FSOPEN, BAD, 0), EFAULT, "name-copy");
    ERROR(syscall(NR_FSOPEN, "", 0), ENODEV, "empty-name");
    ERROR(syscall(NR_FSOPEN, "thekernel-no-such-fs", 0), ENODEV, "unknown-name");
    memset(oversized_fs_name, 'a', sizeof(oversized_fs_name));
    oversized_fs_name[sizeof(oversized_fs_name) - 1] = 0;
    ERROR(syscall(NR_FSOPEN, oversized_fs_name, 0), EINVAL, "oversized-name");
    mark("FSOPEN_NAME_ADMISSION");
    check(unprivileged_errno(body_fsopen) == EPERM, "unprivileged-fsopen");
    check(unprivileged_errno(body_fsopen_bad_flags) == EPERM, "unprivileged-fsopen-flags");
    mark("FSOPEN_UNPRIVILEGED_EPERM");
    check(close(fs_ctx_cloexec) == 0 && close(fs_ctx_plain) == 0, "fsopen-close");

    ERROR(syscall(NR_MOVE_MOUNT, -1, BAD, -1, BAD, BAD_FLAGS), EINVAL, "unknown-flag");
    ERROR(syscall(NR_MOVE_MOUNT, -1, BAD, -1, BAD, MOVE_MOUNT_BENEATH | MOVE_MOUNT_SET_GROUP),
          EINVAL, "beneath-with-set-group");
    mark("MOVE_MOUNT_FLAGS_BEFORE_PATHS");
    ERROR(syscall(NR_MOVE_MOUNT, -1, BAD, AT_FDCWD, "/nonexistent-mount-api", 0), ENOENT,
          "target-before-source");
    ERROR(syscall(NR_MOVE_MOUNT, -1, BAD, AT_FDCWD, "/", 0), EFAULT, "source-copy");
    mark("MOVE_MOUNT_TARGET_BEFORE_SOURCE");
    ERROR(syscall(NR_MOVE_MOUNT, 1 << 30, NULL, AT_FDCWD, "/", MOVE_MOUNT_F_EMPTY_PATH), EBADF,
          "source-empty-path-fd");
    ERROR(syscall(NR_MOVE_MOUNT, AT_FDCWD, "/", 1 << 30, "", MOVE_MOUNT_T_EMPTY_PATH), EBADF,
          "target-empty-path-fd");
    ERROR(syscall(NR_MOVE_MOUNT, -1, NULL, AT_FDCWD, "/", MOVE_MOUNT_F_EMPTY_PATH), EBADF,
          "at-fdcwd-is-not-a-mount");
    mark("MOVE_MOUNT_EMPTY_PATH_DESCRIPTOR");
    ERROR(syscall(NR_MOVE_MOUNT, AT_FDCWD, dir, AT_FDCWD, "/", 0), EINVAL, "source-not-mount-root");
    ERROR(syscall(NR_MOVE_MOUNT, AT_FDCWD, "/", AT_FDCWD, file_path, 0), EINVAL, "file-target");
    mark("MOVE_MOUNT_PLACEMENT_TYPE");
    check(unprivileged_errno(body_move_mount) == EPERM, "unprivileged-move-mount");
    mark("MOVE_MOUNT_UNPRIVILEGED_EPERM");
    int attach_ctx = (int)syscall(NR_FSOPEN, "tmpfs", 0);
    check(attach_ctx >= 0, "attach-fsopen");
    check(syscall(NR_FSCONFIG, attach_ctx, FSCONFIG_CMD_CREATE, NULL, NULL, 0) == 0, "attach-create");
    int attach_mnt = (int)syscall(NR_FSMOUNT, attach_ctx, 0, 0);
    check(attach_mnt >= 0, "attach-fsmount");
    check(close(attach_ctx) == 0, "attach-context-close");
    ERROR(syscall(NR_MOVE_MOUNT, attach_mnt, "", AT_FDCWD, file_path, MOVE_MOUNT_F_EMPTY_PATH),
          EINVAL, "detached-file-target");
    struct statfs source_fs, target_fs;
    check(statfs(dir, &source_fs) == 0 && statfs(target_path, &target_fs) == 0, "statfs-before");
    check((unsigned)source_fs.f_type == EXT4_SUPER_MAGIC &&
              (unsigned)target_fs.f_type == EXT4_SUPER_MAGIC,
          "fixture-is-ext4");
    check(syscall(NR_MOVE_MOUNT, attach_mnt, "", AT_FDCWD, target_path, MOVE_MOUNT_F_EMPTY_PATH) == 0,
          "attach");
    check(statfs(target_path, &target_fs) == 0 && (unsigned)target_fs.f_type == TMPFS_MAGIC,
          "target-is-tmpfs");
    check(statfs(dir, &source_fs) == 0 && (unsigned)source_fs.f_type == EXT4_SUPER_MAGIC,
          "source-unchanged");
    mark("MOVE_MOUNT_ATTACH_DETACHED");
    check(close(attach_mnt) == 0, "attach-mount-close");
    check(syscall(NR_UMOUNT2, target_path, MNT_DETACH) == 0, "detach");
    check(statfs(target_path, &target_fs) == 0 && (unsigned)target_fs.f_type == EXT4_SUPER_MAGIC,
          "target-restored");
    mark("MOVE_MOUNT_DETACH_RESTORES");

    ERROR(syscall(NR_MOUNT_SETATTR, dfd, BAD, BAD_FLAGS, BAD, 32), EINVAL, "unknown-flag");
    ERROR(syscall(NR_MOUNT_SETATTR, dfd, BAD, 0, BAD, 31), EINVAL, "short-attribute");
    ERROR(syscall(NR_MOUNT_SETATTR, dfd, BAD, 0, BAD, 0), EINVAL, "empty-attribute");
    ERROR(syscall(NR_MOUNT_SETATTR, dfd, BAD, 0, BAD, 4097), E2BIG, "oversized-attribute");
    mark("MOUNT_SETATTR_SHAPE_BEFORE_COPY");
    ERROR(syscall(NR_MOUNT_SETATTR, dfd, BAD, 0, BAD, 32), EFAULT, "attribute-copy");
    mark("MOUNT_SETATTR_ATTR_COPY");
    check(syscall(NR_MOUNT_SETATTR, dfd, BAD, 0, &noop_mount_attr, 32) == 0, "noop");
    mark("MOUNT_SETATTR_NOOP_SKIPS_PATH");
    ERROR(syscall(NR_MOUNT_SETATTR, dfd, BAD, 0, &unknown_mount_attr, 32), EINVAL, "unknown-attr");
    ERROR(syscall(NR_MOUNT_SETATTR, dfd, BAD, 0, &double_propagation_mount_attr, 32), EINVAL,
          "two-propagation-bits");
    ERROR(syscall(NR_MOUNT_SETATTR, dfd, BAD, 0, &atime_only_mount_attr, 32), EINVAL,
          "atime-set-without-clear");
    mark("MOUNT_SETATTR_ATTRIBUTE_RULES");
    ERROR(syscall(NR_MOUNT_SETATTR, dfd, BAD, 0, &extended_mount_attr, 40), E2BIG, "tail-bytes");
    check(syscall(NR_MOUNT_SETATTR, dfd, BAD, 0, &extended_mount_attr_zeroed, 40) == 0, "zero-tail");
    mark("MOUNT_SETATTR_TRAILING_BYTES");
    ERROR(syscall(NR_MOUNT_SETATTR, dfd, target_path, 0, &rdonly_mount_attr, 32), EINVAL,
          "not-a-mount-root");
    mark("MOUNT_SETATTR_MOUNT_ROOT_ONLY");
    check(unprivileged_errno(body_mount_setattr_noop) == EPERM, "unprivileged-noop");
    mark("MOUNT_SETATTR_UNPRIVILEGED_EPERM");
    int attr_ctx = (int)syscall(NR_FSOPEN, "tmpfs", 0);
    check(attr_ctx >= 0, "attr-fsopen");
    check(syscall(NR_FSCONFIG, attr_ctx, FSCONFIG_CMD_CREATE, NULL, NULL, 0) == 0, "attr-create");
    int attr_mnt = (int)syscall(NR_FSMOUNT, attr_ctx, 0, 0);
    check(attr_mnt >= 0, "attr-fsmount");
    check(close(attr_ctx) == 0, "attr-context-close");
    ERROR(syscall(NR_MOUNT_SETATTR, attr_mnt, NULL, AT_EMPTY_PATH, &rdonly_mount_attr, 32), EFAULT,
          "null-pathname");
    mark("MOUNT_SETATTR_PATH_ADMISSION");
    check(syscall(NR_MOUNT_SETATTR, attr_mnt, "", AT_EMPTY_PATH, &rdonly_mount_attr, 32) == 0,
          "set-readonly");
    struct statvfs detached_fs;
    check(fstatvfs(attr_mnt, &detached_fs) == 0 && (detached_fs.f_flag & ST_RDONLY) != 0,
          "detached-readonly");
    mark("MOUNT_SETATTR_DETACHED_RDONLY");
    check(syscall(NR_MOUNT_SETATTR, attr_mnt, "", AT_EMPTY_PATH, &clear_rdonly_mount_attr, 32) == 0,
          "clear-readonly");
    check(fstatvfs(attr_mnt, &detached_fs) == 0 && (detached_fs.f_flag & ST_RDONLY) == 0,
          "detached-writable");
    mark("MOUNT_SETATTR_DETACHED_CLEAR");
    check(close(attr_mnt) == 0, "attr-mount-close");

    struct mnt_id_req list_req, single, cursor;
    memset(&list_req, 0, sizeof(list_req));
    list_req.size = sizeof(list_req);
    ERROR(syscall(NR_LISTMOUNT, BAD, BAD, 1, 0x2), EINVAL, "unknown-flags");
    ERROR(syscall(NR_LISTMOUNT, BAD, BAD, 1000001, 0), EOVERFLOW, "count-limit");
    mark("LISTMOUNT_FLAGS_AND_COUNT");
    ERROR(syscall(NR_LISTMOUNT, BAD, mount_ids, 1, 0), EFAULT, "request-copy");
    mark("LISTMOUNT_REQUEST_COPY");
    ERROR(syscall(NR_LISTMOUNT, &list_req, mount_ids, 4, 0), EINVAL, "zero-mount-id");
    list_req.mnt_id = 1ULL << 31;
    ERROR(syscall(NR_LISTMOUNT, &list_req, mount_ids, 4, 0), EINVAL, "unique-id-floor");
    list_req.mnt_id = LSMT_ROOT;
    list_req.mnt_fd = 3;
    list_req.mnt_ns_id = 1;
    ERROR(syscall(NR_LISTMOUNT, &list_req, mount_ids, 4, 0), EINVAL, "namespace-fd-and-id");
    list_req.mnt_fd = 0;
    list_req.mnt_ns_id = 0;
    mark("LISTMOUNT_REQUEST_IDENTITY");
    long listed = syscall(NR_LISTMOUNT, &list_req, mount_ids, 32, 0);
    check(listed >= 2 && listed <= 32, "list-count");
    check(mount_ids[0] > (1ULL << 31), "unique-id-range");
    int ascending = 1;
    for (long index = 1; index < listed; index++) {
        if (mount_ids[index] <= mount_ids[index - 1]) {
            ascending = 0;
        }
    }
    check(ascending, "ascending-ids");
    long reversed = syscall(NR_LISTMOUNT, &list_req, reverse_mount_ids, 32, LISTMOUNT_REVERSE);
    check(reversed == listed, "reverse-count");
    int mirrored = 1;
    for (long index = 0; index < listed; index++) {
        if (reverse_mount_ids[index] != mount_ids[listed - 1 - index]) {
            mirrored = 0;
        }
    }
    check(mirrored, "reverse-order");
    mark("LISTMOUNT_ROOT_FORWARD_REVERSE");
    single = list_req;
    single.mnt_id = mount_ids[0];
    single.param = STATMOUNT_MNT_BASIC;
    check(syscall(NR_STATMOUNT, &single, statmount_buffer, sizeof(statmount_buffer), 0) == 0,
          "identity-statmount");
    struct statmount identity_prefix;
    memcpy(&identity_prefix, statmount_buffer, sizeof(identity_prefix));
    check(identity_prefix.mnt_id == mount_ids[0], "identity-match");
    mark("LISTMOUNT_STATMOUNT_IDENTITY");
    cursor = list_req;
    cursor.param = mount_ids[0];
    check(syscall(NR_LISTMOUNT, &cursor, reverse_mount_ids, 32, 0) == listed - 1, "cursor");
    mark("LISTMOUNT_CURSOR");
    int seen_ctx = (int)syscall(NR_FSOPEN, "tmpfs", 0);
    check(seen_ctx >= 0, "seen-fsopen");
    check(syscall(NR_FSCONFIG, seen_ctx, FSCONFIG_CMD_CREATE, NULL, NULL, 0) == 0, "seen-create");
    int seen_mnt = (int)syscall(NR_FSMOUNT, seen_ctx, 0, 0);
    check(seen_mnt >= 0, "seen-fsmount");
    check(close(seen_ctx) == 0, "seen-context-close");
    check(syscall(NR_MOVE_MOUNT, seen_mnt, "", AT_FDCWD, target_path, MOVE_MOUNT_F_EMPTY_PATH) == 0,
          "seen-attach");
    check(syscall(NR_LISTMOUNT, &list_req, reverse_mount_ids, 32, 0) == listed + 1, "seen-count");
    check(close(seen_mnt) == 0, "seen-mount-close");
    check(syscall(NR_UMOUNT2, target_path, MNT_DETACH) == 0, "seen-detach");
    mark("LISTMOUNT_ATTACH_OBSERVED");
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
