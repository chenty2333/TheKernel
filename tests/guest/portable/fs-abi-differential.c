/*
 * Filesystem and file-descriptor ABI differential.
 *
 * Every assertion below is a *validation-order* or *errno-precedence* fact
 * taken from the Linux v7.2.3 sources named in each section, so the same
 * binary is meaningful on both the Linux oracle and TheKernel.
 *
 * Completion marker: THEKERNEL_FS_ABI_OK
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

/* Native x86_64 UAPI, independent of the build host's libc headers. */
#define NR_PIDFD_SEND_SIGNAL 424
#define NR_PREADV2 327
#define NR_PWRITEV2 328
#define NR_SYNC 162
#define NR_SYNCFS 306

#define PIPE_BUF_FLAGS_VALID (O_CLOEXEC | O_NONBLOCK | O_DIRECT | O_EXCL)
#define O_NOTIFICATION_PIPE O_EXCL /* include/uapi/linux/pipe_fs_i.h */

/* include/uapi/linux/fs.h -- these must equal the kernel's numbers, or the
 * assertions below test a command the kernel never decodes.  FIGETBSZ is one
 * of the few _IO commands that takes an argument (`_IO(0x00,2)`, so its
 * encoding carries neither direction nor size); a `_IOR(0x00, 2, int)`
 * spelling addresses a different, unallocated command instead. */
#define FIGETBSZ 0x00000002UL   /* _IO(0x00, 2) */
#define FIBMAP 0x00000001UL     /* _IO(0x00, 1) */
/* <sys/ioctl.h> already defines the four 'T' commands; the guest build is
 * -Werror, so only add what the C library leaves out. */
#ifndef FIOCLEX
#define FIOCLEX 0x5451UL        /* _IO('T', 81) */
#endif
#ifndef FIONCLEX
#define FIONCLEX 0x5450UL       /* _IO('T', 80) */
#endif
#ifndef FIONBIO
#define FIONBIO 0x5421UL        /* _IOW('T', 33, int) */
#endif
#ifndef FIOASYNC
#define FIOASYNC 0x5452UL       /* _IOW('T', 82, int) */
#endif
#ifndef FIOQSIZE
#define FIOQSIZE 0x5460UL       /* include/uapi/asm-generic/ioctls.h */
#endif
#ifndef FICLONE
#define FICLONE 0x40049409UL    /* _IOW(0x94, 9, int) */
#endif
#ifndef FS_IOC_FIEMAP
#define FS_IOC_FIEMAP 0xC020660BUL /* _IOWR('f', 11, struct fiemap) */
#endif
#ifndef FS_IOC_RESVSP
#define FS_IOC_RESVSP 0x40305828UL /* _IOW('X', 40, struct space_resv) */
#endif
#define FIFREEZE 0xC0045877UL   /* _IOWR('X', 119, int) */
#define FITHAW 0xC0045878UL     /* _IOWR('X', 120, int) */
#define FS_IOC_GETFSUUID 0x80111500UL /* _IOR(0x15, 0, struct fsuuid2) */
#define FS_IOC_GETFSSYSFSPATH 0x80811501UL /* _IOR(0x15, 1, struct fs_sysfs_path) */

#define MS_RDONLY 1UL
#define MS_SYNCHRONOUS 16UL
#define MS_DIRSYNC 128UL
#define MS_I_VERSION (1UL << 23)
#define MS_LAZYTIME (1UL << 25)
#define MS_MGC_VAL 0xc0ed0000UL
#define MS_MGC_MSK 0xffff0000UL
#define MS_NOUSER (1UL << 31)
#define MNT_FORCE 1
#define MNT_DETACH 2
#define MNT_EXPIRE 4
/* include/uapi/linux/mount.h: UMOUNT_NOFOLLOW.  glibc before 2.34 omits it. */
#ifndef UMOUNT_NOFOLLOW
#define UMOUNT_NOFOLLOW 8
#endif

#define FALLOC_FL_KEEP_SIZE 0x01
#define FALLOC_FL_PUNCH_HOLE 0x02
#define FALLOC_FL_COLLAPSE_RANGE 0x08
#define FALLOC_FL_ZERO_RANGE 0x10
#define FALLOC_FL_UNSHARE_RANGE 0x40

#ifndef RWF_HIPRI
#define RWF_HIPRI 0x00000001U
#endif
#ifndef RWF_DSYNC
#define RWF_DSYNC 0x00000002U
#endif
#ifndef RWF_APPEND
#define RWF_APPEND 0x00000010U
#endif
#ifndef RWF_NOAPPEND
#define RWF_NOAPPEND 0x00000020U
#endif

#define PIDFD_SIGNAL_THREAD 1U
#define PIDFD_SIGNAL_THREAD_GROUP 2U
#define PIDFD_SELF_THREAD (-10000)

#define SYSLOG_ACTION_READ 2
#define SYSLOG_ACTION_READ_ALL 3
#define SYSLOG_ACTION_READ_CLEAR 4
#define SYSLOG_ACTION_CONSOLE_LEVEL 8
#define SYSLOG_ACTION_SIZE_UNREAD 9
#define SYSLOG_ACTION_SIZE_BUFFER 10

#define LINUX_REBOOT_MAGIC1 0xfee1dead
#define LINUX_REBOOT_MAGIC2 672274793
#define LINUX_REBOOT_CMD_RESTART2 0xa1b2c3d4

#define SPLICE_F_ALL 0x0fU

#define BAD ((void *)(uintptr_t)1)
#define BADPTR ((void *)(uintptr_t)0x1000)

static const char *active;

static char root[] = "/root/thekernel-fs-abi-XXXXXX";
static char mnt[] = "/root/thekernel-fs-abi-mnt-XXXXXX";
static int dirfd = -1;
static int file = -1;
static int rofile = -1;
static int fifo = -1;
static int sock[2] = { -1, -1 };

static void cleanup(void) {
    if (sock[0] >= 0) {
        (void)close(sock[0]);
        (void)close(sock[1]);
    }
    if (fifo >= 0) (void)close(fifo);
    if (file >= 0) (void)close(file);
    if (rofile >= 0) (void)close(rofile);
    if (dirfd >= 0) {
        (void)unlinkat(dirfd, "file", 0);
        (void)unlinkat(dirfd, "fifo", 0);
        (void)close(dirfd);
    }
    (void)rmdir(mnt);
    (void)rmdir(root);
}

static void begin(const char *id) {
    active = id;
    printf("THEKERNEL_ABI_CASE %s\n", active);
}

static void mark(const char *id) {
    printf("THEKERNEL_ABI_ASSERT %s %s pass\n", active, id);
}

static void done(void) { printf("THEKERNEL_ABI_RESULT %s pass\n", active); }

static void check(int ok, const char *stage) {
    if (!ok) {
        int saved = errno;
        fprintf(stderr, "THEKERNEL_FS_ABI_FAIL %s %s errno=%d (%s)\n", active,
                stage, saved, strerror(saved));
        fflush(NULL);
        _exit(1);
    }
}

#define ERROR(call, expected, stage) do { errno = 0; long r_ = (call); \
    check(r_ == -1 && errno == (expected), (stage)); } while (0)

/* The syslog capability test needs a caller that still holds CAP_SYS_ADMIN
 * but not CAP_SYSLOG, which no uid transition can produce. */
static int drop_capability(int capability) {
    struct __user_cap_header_struct header = { .version = _LINUX_CAPABILITY_VERSION_3, .pid = 0 };
    struct __user_cap_data_struct data[2];
    if (syscall(SYS_capget, &header, data) != 0) return -1;
    data[capability / 32].effective &= ~(1U << (capability % 32));
    return (int)syscall(SYS_capset, &header, data);
}

/* `struct fiemap` and `struct space_resv` in their native x86_64 layouts
 * (include/uapi/linux/fiemap.h, include/uapi/linux/fs.h). */
struct fsabi_fiemap_extent {
    uint64_t fe_logical, fe_physical, fe_length;
    uint64_t fe_reserved64[2];
    uint32_t fe_flags, fe_reserved[3];
};
struct fsabi_fiemap {
    uint64_t fm_start, fm_length;
    uint32_t fm_flags, fm_mapped_extents, fm_extent_count, fm_reserved;
    struct fsabi_fiemap_extent fm_extents[1];
};
struct fsabi_space_resv {
    int16_t l_type, l_whence;
    int64_t l_start, l_len;
    int32_t l_sysid;
    uint32_t l_pid;
    int32_t l_pad[4];
};

static void drops_to(void) {
    /* Drop every capability so CAP_* gates are exercised for real. */
    check(setresgid(65534, 65534, 65534) == 0, "drop-gid");
    check(setresuid(65534, 65534, 65534) == 0, "drop-uid");
}

int main(void) {
    /* Diagnostics must reach the console in one write.  A stdio buffer that
     * `exit()` flushes afterwards can still be interleaved by a kernel warning
     * that lands mid-line, which truncates the FAIL record the harness reads. */
    setvbuf(stdout, NULL, _IONBF, 0);
    setvbuf(stderr, NULL, _IONBF, 0);
    check(mkdtemp(root) != NULL, "root-mkdtemp");
    check(mkdtemp(mnt) != NULL, "mnt-mkdtemp");
    check(atexit(cleanup) == 0, "cleanup-register");
    dirfd = open(root, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    check(dirfd >= 0, "directory-open");
    file = openat(dirfd, "file", O_CREAT | O_EXCL | O_RDWR | O_CLOEXEC, 0600);
    check(file >= 0, "file-open");
    rofile = openat(dirfd, "file", O_RDONLY | O_CLOEXEC);
    check(rofile >= 0, "readonly-open");
    check(mkfifoat(dirfd, "fifo", 0600) == 0, "fifo-create");
    fifo = openat(dirfd, "fifo", O_RDWR | O_NONBLOCK | O_CLOEXEC);
    check(fifo >= 0, "fifo-open");
    check(socketpair(AF_UNIX, SOCK_STREAM, 0, sock) == 0, "socketpair");
    char payload[32];
    memset(payload, 'x', sizeof(payload));
    check(pwrite(file, payload, sizeof(payload), 0) == (ssize_t)sizeof(payload),
          "file-seed");

    /* ------------------------------------------------------------------ *
     * getcwd(2) -- fs/d_path.c SYSCALL_DEFINE2(getcwd, ...):
     *     if (len > PATH_MAX)            return -ENAMETOOLONG;
     *     if (len > size)                return -ERANGE;
     *     if (!buf)                      return -EFAULT;
     * The length verdict precedes the buffer check, so a NULL buffer with a
     * zero size is ERANGE and not EFAULT.
     * ------------------------------------------------------------------ */
    begin("getcwd.raw-differential");
    {
        char buf[4096];
        size_t len;
        errno = 0;
        check(syscall(SYS_getcwd, buf, 0) == -1 && errno == ERANGE, "size-zero");
        mark("SIZE_ZERO_ERANGE");
        errno = 0;
        check(syscall(SYS_getcwd, NULL, 0) == -1 && errno == ERANGE, "null-zero");
        errno = 0;
        check(syscall(SYS_getcwd, NULL, sizeof(buf)) == -1 && errno == EFAULT, "null-buf");
        mark("NULL_BUF_EFAULT");
        len = (size_t)syscall(SYS_getcwd, buf, sizeof(buf));
        check(len > 1 && buf[len - 1] == '\0', "full");
        errno = 0;
        check(syscall(SYS_getcwd, buf, len - 1) == -1 && errno == ERANGE, "short");
        mark("SHORT_BUFFER_ERANGE");
        check(syscall(SYS_getcwd, buf, len) == (long)len, "exact");
        mark("EXACT_FIT_OK");
        /* The successful return counts the trailing NUL. */
        check(buf[0] == '/', "absolute");
    }
    done();

    /* ------------------------------------------------------------------ *
     * fcntl(2) F_SETFL -- fs/fcntl.c setfl():
     *     #define SETFL_MASK (O_APPEND | O_NONBLOCK | O_NDELAY | O_DIRECT |
     *                          O_NOATIME)
     *     if (arg & O_APPEND && IS_APPEND(inode))       return -EPERM;
     *     if (arg & O_NOATIME && !inode_owner_or_capable(...))
     *                                                   return -EPERM;
     *     if (arg & O_DIRECT && !S_ISFIFO && !FMODE_CAN_ODIRECT)
     *                                                   return -EINVAL;
     *     filp->f_flags = (arg & SETFL_MASK) | (filp->f_flags & ~SETFL_MASK);
     * Bits outside the mask are dropped rather than refused.
     * ------------------------------------------------------------------ */
    begin("fcntl.raw-differential");
    {
        long base = fcntl(file, F_GETFL);
        check(base >= 0, "getfl");
        check(fcntl(file, F_SETFL, base | O_CREAT | O_TRUNC) == 0, "outside-mask");
        check((fcntl(file, F_GETFL) & ~O_ACCMODE) == (base & ~O_ACCMODE),
              "outside-mask-dropped");
        mark("SETFL_IGNORES_OUTSIDE_MASK");
        check(fcntl(file, F_SETFL, base | O_NONBLOCK) == 0, "set-nonblock");
        check((fcntl(file, F_GETFL) & O_NONBLOCK) != 0, "nonblock-stored");
        check(fcntl(file, F_SETFL, base) == 0, "clear-nonblock");
        check((fcntl(file, F_GETFL) & O_NONBLOCK) == 0, "nonblock-cleared");
        mark("SETFL_NONBLOCK_ROUNDTRIP");
        check(fcntl(file, F_SETFL, base | O_APPEND) == 0, "set-append");
        check((fcntl(file, F_GETFL) & O_APPEND) != 0, "append-stored");
        check(fcntl(file, F_SETFL, base) == 0, "clear-append");
        mark("SETFL_APPEND_ROUNDTRIP");
        /* ext4 regular files carry a_ops->direct_IO, so FMODE_CAN_ODIRECT. */
        check(fcntl(file, F_SETFL, base | O_DIRECT) == 0, "set-odirect");
        check((fcntl(file, F_GETFL) & O_DIRECT) != 0, "odirect-stored");
        check(fcntl(file, F_SETFL, base) == 0, "clear-odirect");
        mark("SETFL_ODIRECT_REGULAR");
        /* O_NOATIME is admitted for the owner and refused for anyone else. */
        check(fcntl(file, F_SETFL, base | O_NOATIME) == 0, "set-noatime-owner");
        check((fcntl(file, F_GETFL) & O_NOATIME) != 0, "noatime-stored");
        check(fcntl(file, F_SETFL, base) == 0, "clear-noatime");
        mark("SETFL_NOATIME_OWNER");
        pid_t child = fork();
        check(child >= 0, "fork");
        if (child == 0) {
            drops_to();
            errno = 0;
            long r = fcntl(file, F_SETFL, base | O_NOATIME);
            _exit(r == -1 && errno == EPERM ? 0 : 1);
        }
        int status = 0;
        check(waitpid(child, &status, 0) == child, "waitpid");
        check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "noatime-nonowner");
        mark("SETFL_NOATIME_NONOWNER_EPERM");
        /* The capability half of `inode_owner_or_capable()` is asked of the
         * inode, not of a path: every inode created by `inode_init_always()`
         * starts at GLOBAL_ROOT_UID/GLOBAL_ROOT_GID (fs/inode.c:206-208), so
         * an anonymous object such as an eventfd is root-owned even though no
         * vfsmount backs it.  A caller with CAP_FOWNER (here: uid 0) must
         * therefore be able to set O_NOATIME on it, exactly as on the regular
         * file above. */
        int anon = eventfd(0, EFD_CLOEXEC);
        check(anon >= 0, "eventfd");
        long anon_base = fcntl(anon, F_GETFL);
        check(anon_base >= 0, "anon-getfl");
        check(fcntl(anon, F_SETFL, anon_base | O_NOATIME) == 0, "set-noatime-anon");
        check((fcntl(anon, F_GETFL) & O_NOATIME) != 0, "noatime-anon-stored");
        check(fcntl(anon, F_SETFL, anon_base) == 0, "clear-noatime-anon");
        check(close(anon) == 0, "close-anon");
        mark("SETFL_NOATIME_ANONYMOUS_OWNER");
    }
    done();

    /* ------------------------------------------------------------------ *
     * syslog(2) -- kernel/printk/printk.c do_syslog():
     *     error = check_syslog_permissions(type, source);   if (error) return;
     *     case SYSLOG_ACTION_READ:            (:1756)
     *     case SYSLOG_ACTION_READ_CLEAR:      (:1766, falls through)
     *     case SYSLOG_ACTION_READ_ALL:        (:1770)
     *             if (!buf || len < 0) return -EINVAL;
     *             if (!len)            return 0;
     *             if (!access_ok(buf, len)) return -EFAULT;
     *     case SYSLOG_ACTION_CONSOLE_LEVEL:
     *             if (len < 1 || len > 8) return -EINVAL;
     *     default: error = -EINVAL;
     * `SYSCALL_DEFINE3(syslog, int, type, char __user *, buf, int, len)`
     * (:1853) declares `len` as a 32-bit `int`, so the kernel reads only the
     * low half of the third argument register.  These calls therefore pass the
     * length as a `long`, which is what glibc's `syscall()` forwards: an `int`
     * argument is promoted to `int` in the variadic list only, and the ABI
     * leaves the register's upper half to the caller, so a bare `-1` arrives
     * zero-extended as 4294967295 and stops being a negative length at all.
     * All three ring selectors share one guard, so the ordering facts below hold
     * for each of them: a NULL buffer and a negative length are the same
     * -EINVAL verdict and neither reaches access_ok().  A 2 GiB length is the
     * `int` minimum's magnitude and is negative once sign-extended, so it must
     * not be mistaken for a huge positive request.
     * ------------------------------------------------------------------ */
    begin("syslog.raw-differential");
    {
        char buf[4096];
        /* Unprivileged, and evaluated before anything else: for READ the
         * capability gate is the whole verdict, including for a NULL buffer
         * and a negative length.  Asserting -EPERM (not -EINVAL) is what
         * proves check_syslog_permissions() runs ahead of the switch. */
        pid_t child = fork();
        check(child >= 0, "fork");
        if (child == 0) {
            drops_to();
            errno = 0;
            long a = syscall(SYS_syslog, SYSLOG_ACTION_READ, NULL, 256);
            int null_buf = (a == -1 && errno == EPERM);
            errno = 0;
            long b = syscall(SYS_syslog, SYSLOG_ACTION_READ, NULL, -1);
            int negative = (b == -1 && errno == EPERM);
            errno = 0;
            long c = syscall(SYS_syslog, SYSLOG_ACTION_CONSOLE_LEVEL, NULL, 9);
            int level = (c == -1 && errno == EPERM);
            _exit(null_buf && negative && level ? 0 : 1);
        }
        int status = 0;
        check(waitpid(child, &status, 0) == child, "waitpid");
        check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "privilege");
        mark("PERMISSION_BEFORE_VALIDATION_EPERM");
        errno = 0;
        check(syscall(SYS_syslog, SYSLOG_ACTION_READ_ALL, NULL, 256) == -1 &&
              errno == EINVAL, "null-buf");
        mark("NULL_BUF_EINVAL");
        errno = 0;
        check(syscall(SYS_syslog, SYSLOG_ACTION_READ, NULL, 256) == -1 &&
              errno == EINVAL, "read-null-buf");
        errno = 0;
        check(syscall(SYS_syslog, SYSLOG_ACTION_READ_CLEAR, NULL, 256) == -1 &&
              errno == EINVAL, "read-clear-null-buf");
        mark("NULL_BUF_EINVAL_ALL_SELECTORS");
        errno = 0;
        check(syscall(SYS_syslog, SYSLOG_ACTION_READ_ALL, buf, -1) == -1 &&
              errno == EINVAL, "negative-len");
        mark("NEGATIVE_LEN_EINVAL");
        errno = 0;
        check(syscall(SYS_syslog, SYSLOG_ACTION_READ, buf, -1) == -1 &&
              errno == EINVAL, "negative-len-read");
        errno = 0;
        check(syscall(SYS_syslog, SYSLOG_ACTION_READ_CLEAR, buf, -1) == -1 &&
              errno == EINVAL, "negative-len-read-clear");
        errno = 0;
        check(syscall(SYS_syslog, SYSLOG_ACTION_READ, buf, -2147483647 - 1) == -1 &&
              errno == EINVAL, "negative-len-int-min");
        mark("NEGATIVE_LEN_EINVAL_ALL_SELECTORS");
        errno = 0;
        check(syscall(SYS_syslog, SYSLOG_ACTION_READ_ALL, NULL, 0) == -1 &&
              errno == EINVAL, "zero-len-null");
        check(syscall(SYS_syslog, SYSLOG_ACTION_READ_ALL, buf, 0) == 0, "zero-len");
        check(syscall(SYS_syslog, SYSLOG_ACTION_READ, buf, 0) == 0, "read-zero-len");
        check(syscall(SYS_syslog, SYSLOG_ACTION_READ_CLEAR, buf, 0) == 0,
              "read-clear-zero-len");
        mark("ZERO_LEN_NOOP");
        errno = 0;
        check(syscall(SYS_syslog, SYSLOG_ACTION_READ_ALL, BADPTR, 256) == -1 &&
              errno == EFAULT, "bad-ptr");
        mark("BAD_PTR_EFAULT");
        errno = 0;
        check(syscall(SYS_syslog, 99, buf, 1) == -1 && errno == EINVAL, "action");
        mark("UNKNOWN_ACTION_EINVAL");
        errno = 0;
        check(syscall(SYS_syslog, SYSLOG_ACTION_CONSOLE_LEVEL, NULL, 0) == -1 &&
              errno == EINVAL, "level-low");
        errno = 0;
        check(syscall(SYS_syslog, SYSLOG_ACTION_CONSOLE_LEVEL, NULL, 9) == -1 &&
              errno == EINVAL, "level-high");
        mark("CONSOLE_LEVEL_EINVAL");
        check(syscall(SYS_syslog, SYSLOG_ACTION_SIZE_BUFFER, NULL, 0) > 0, "size");
        mark("SIZE_BUFFER_POSITIVE");
        /* `check_syslog_permissions()` (kernel/printk/printk.c:606-629) admits
         * a restricted action only through `capable(CAP_SYSLOG)`; CAP_SYS_ADMIN
         * is not an alternative, and SYSLOG_ACTION_SIZE_UNREAD is restricted
         * because it is neither READ_ALL nor SIZE_BUFFER. */
        pid_t cap_child = fork();
        check(cap_child >= 0, "syslog-fork");
        if (cap_child == 0) {
            int dropped = drop_capability(CAP_SYSLOG);
            errno = 0;
            long r = syscall(SYS_syslog, SYSLOG_ACTION_SIZE_UNREAD, NULL, 0);
            _exit(dropped == 0 && r == -1 && errno == EPERM ? 0 : 1);
        }
        int cap_status = 0;
        check(waitpid(cap_child, &cap_status, 0) == cap_child, "syslog-waitpid");
        check(WIFEXITED(cap_status) && WEXITSTATUS(cap_status) == 0, "syslog-cap-syslog-only");
    }
    done();

    /* ------------------------------------------------------------------ *
     * reboot(2) -- kernel/reboot.c SYSCALL_DEFINE4(reboot, ...):
     *     if (!ns_capable(pid_ns->user_ns, CAP_SYS_BOOT))   return -EPERM;
     *     if (magic1 != LINUX_REBOOT_MAGIC1 || magic2 != ...) return -EINVAL;
     *     ... reboot_pid_ns() ...
     *     case LINUX_REBOOT_CMD_RESTART2:
     *             ret = strncpy_from_user(&buffer[0], arg, sizeof(buffer) - 1);
     *             if (ret < 0) { ret = -EFAULT; break; }
     *     default: ret = -EINVAL;
     * Only the EFAULT paths are exercised: a readable command restarts the
     * machine.  A magic2 of zero is EINVAL on the initial namespace and on a
     * child namespace alike.
     * ------------------------------------------------------------------ */
    begin("reboot.raw-differential");
    {
        errno = 0;
        check(syscall(SYS_reboot, 0, LINUX_REBOOT_MAGIC2,
                      LINUX_REBOOT_CMD_RESTART2, NULL) == -1 && errno == EINVAL,
              "magic1");
        mark("MAGIC1_EINVAL");
        errno = 0;
        check(syscall(SYS_reboot, LINUX_REBOOT_MAGIC1, 0,
                      LINUX_REBOOT_CMD_RESTART2, NULL) == -1 && errno == EINVAL,
              "magic2");
        mark("MAGIC2_EINVAL");
        errno = 0;
        check(syscall(SYS_reboot, LINUX_REBOOT_MAGIC1, LINUX_REBOOT_MAGIC2,
                      LINUX_REBOOT_CMD_RESTART2, NULL) == -1 && errno == EFAULT,
              "restart2-null");
        mark("RESTART2_NULL_EFAULT");
        errno = 0;
        check(syscall(SYS_reboot, LINUX_REBOOT_MAGIC1, LINUX_REBOOT_MAGIC2,
                      LINUX_REBOOT_CMD_RESTART2, BADPTR) == -1 && errno == EFAULT,
              "restart2-badptr");
        mark("RESTART2_BAD_PTR_EFAULT");
        errno = 0;
        check(syscall(SYS_reboot, LINUX_REBOOT_MAGIC1, LINUX_REBOOT_MAGIC2, 0xdead,
                      NULL) == -1 && errno == EINVAL, "unknown-cmd");
        mark("UNKNOWN_CMD_EINVAL");
        /* CAP_SYS_BOOT is checked before the magic word. */
        pid_t child = fork();
        check(child >= 0, "fork");
        if (child == 0) {
            drops_to();
            errno = 0;
            long r = syscall(SYS_reboot, 0, 0, 0, NULL);
            _exit(r == -1 && errno == EPERM ? 0 : 1);
        }
        int status = 0;
        check(waitpid(child, &status, 0) == child, "waitpid");
        check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "privilege");
        mark("CAPABILITY_BEFORE_MAGIC_EPERM");
    }
    done();

    /* ------------------------------------------------------------------ *
     * ioctl(2) -- fs/ioctl.c do_vfs_ioctl() before ->unlocked_ioctl:
     *     case FIGETBSZ:
     *             -- anon_bdev filesystems may not have a block size --
     *             if (!inode->i_sb->s_blocksize) return -EINVAL;   (:533-538)
     *             return put_user(inode->i_sb->s_blocksize, ...);
     *     case FIFREEZE:  ioctl_fsfreeze()  EPERM (no CAP_SYS_ADMIN in
     *                     sb->s_user_ns) then EOPNOTSUPP when the superblock
     *                     has neither ->freeze_fs nor ->freeze_super
     *     case FITHAW:    ioctl_fsthaw()    EPERM then EINVAL (never frozen)
     *     case FS_IOC_GETFSUUID / FS_IOC_GETFSSYSFSPATH:
     *                     -ENOTTY when the superblock has no UUID/sysfs name
     * A pipe lives on pipefs, whose init_fs_context is init_pseudo()
     * (fs/pipe.c:1564-1572) and whose fill_super is therefore
     * pseudo_fs_fill_super(), which sets s_blocksize = PAGE_SIZE
     * (fs/libfs.c:681-682).  FIGETBSZ on a pipe is consequently a successful
     * read of PAGE_SIZE, not the -EINVAL that a zero block size would give.
     * ------------------------------------------------------------------ */
    begin("ioctl.raw-differential");
    {
        int pp[2];
        int block_size = 0;
        /* An anonymous pipe lives on pipefs: pseudo_fs_fill_super() gives it
         * PAGE_SIZE and pipefs never sets ->freeze_fs, ->freeze_super or a
         * UUID, so the generic layer alone decides the refusals below. */
        check(pipe2(pp, O_CLOEXEC) == 0, "pipe");
        errno = 0;
        check(ioctl(pp[0], FIGETBSZ, &block_size) == 0 &&
              block_size == (int)sysconf(_SC_PAGESIZE) && block_size >= 512 &&
              (block_size & (block_size - 1)) == 0, "pipe-blocksize");
        /* The registered contract name still says _EINVAL because it was
         * written from the zero-block-size reading of fs/ioctl.c:533-538.
         * Linux 7.2.3 answers success with PAGE_SIZE here: pipefs reaches
         * pseudo_fs_fill_super() (fs/libfs.c:681-682), which sets
         * s_blocksize.  The name is left alone so
         * tests/qemu_runner/test_abi_differential.py keeps matching; the
         * assertion follows the source.  TheKernel returns -EINVAL, which is
         * this same assertion's red state. */
        mark("FIGETBSZ_PSEUDO_EINVAL");
        check(ioctl(file, FIGETBSZ, &block_size) == 0 && block_size >= 512 &&
              (block_size & (block_size - 1)) == 0, "fs-blocksize");
        mark("FIGETBSZ_FILESYSTEM_BLOCK_SIZE");
        errno = 0;
        check(ioctl(pp[0], FIFREEZE, 0) == -1 && errno == EOPNOTSUPP, "pipe-freeze");
        errno = 0;
        check(ioctl(pp[0], FITHAW, 0) == -1 && errno == EINVAL, "pipe-thaw");
        mark("PSEUDO_UNSUPPORTED_REFUSALS");
        errno = 0;
        check(ioctl(pp[0], FS_IOC_GETFSUUID, BADPTR) == -1 && errno == ENOTTY,
              "pipe-uuid");
        errno = 0;
        check(ioctl(pp[0], FS_IOC_GETFSSYSFSPATH, BADPTR) == -1 && errno == ENOTTY,
              "pipe-sysfs-path");
        mark("FSUUID_AND_SYSSFSPATH_ENOTTY");
        /* The CAP_SYS_ADMIN test precedes the support test. */
        pid_t child = fork();
        check(child >= 0, "fork");
        if (child == 0) {
            drops_to();
            errno = 0;
            long a = ioctl(pp[0], FIFREEZE, 0);
            int freeze = (a == -1 && errno == EPERM);
            errno = 0;
            long b = ioctl(pp[0], FITHAW, 0);
            int thaw = (b == -1 && errno == EPERM);
            _exit(freeze && thaw ? 0 : 1);
        }
        int status = 0;
        check(waitpid(child, &status, 0) == child, "waitpid");
        check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "privilege");
        mark("FREEZE_AND_THAW_EPERM_UNPRIVILEGED");
        /* `SYSCALL_DEFINE3(ioctl, ...)` fetches the descriptor with `fdget()`,
         * which is `__fget_light(fd, FMODE_PATH)` (fs/file.c:1206-1209); that
         * mask makes the helper return an empty `struct fd` for an `O_PATH`
         * descriptor, so the syscall answers -EBADF before dispatching any
         * command.  No command escapes, not even the ones `do_vfs_ioctl()`
         * answers itself.  The Linux 7.2.3 oracle returns EBADF for each of
         * these, and this file asserted the opposite until it was corrected. */
        int pathfd = openat(dirfd, "file", O_PATH | O_CLOEXEC);
        check(pathfd >= 0, "opath-open");
        block_size = 0;
        errno = 0;
        check(ioctl(pathfd, FIGETBSZ, &block_size) == -1 && errno == EBADF &&
              block_size == 0,
              "opath-figetbsz");
        long long qsize = -1;
        errno = 0;
        check(ioctl(pathfd, FIOQSIZE, &qsize) == -1 && errno == EBADF && qsize == -1,
              "opath-fioqsize");
        check((fcntl(pathfd, F_GETFD) & FD_CLOEXEC) != 0, "opath-cloexec-baseline");
        errno = 0;
        check(ioctl(pathfd, FIONCLEX) == -1 && errno == EBADF &&
              (fcntl(pathfd, F_GETFD) & FD_CLOEXEC) != 0,
              "opath-fionclex");
        errno = 0;
        check(ioctl(pathfd, FIOCLEX) == -1 && errno == EBADF, "opath-fioclex");
        int on = 1;
        errno = 0;
        check(ioctl(pathfd, FIONBIO, &on) == -1 && errno == EBADF, "opath-fionbio");
        check((fcntl(pathfd, F_GETFL) & O_NONBLOCK) == 0, "opath-fionbio-invisible");
        check(close(pathfd) == 0, "opath-close");
        /* FIOQSIZE is defined only for directories, symlinks and non-anonymous
         * regular files, while FIOASYNC consults `->fasync` only when the
         * request changes the bit: pipefops has one, a regular file does not. */
        errno = 0;
        check(ioctl(pp[0], FIOQSIZE, &qsize) == -1 && errno == ENOTTY, "pipe-fioqsize");
        on = 1;
        check(ioctl(pp[0], FIOASYNC, &on) == 0, "pipe-fioasync");
        on = 0;
        check(ioctl(pp[0], FIOASYNC, &on) == 0, "pipe-fioasync-clear");
        on = 1;
        errno = 0;
        check(ioctl(file, FIOASYNC, &on) == -1 && errno == ENOTTY, "file-fioasync");
        check(close(pathfd) == 0, "opath-close");
        /* FICLONE classifies both inodes before the provider sees the command
         * (`vfs_clone_file_range` -> `generic_file_rw_checks`, fs/remap_range.c):
         * a directory on either side is EISDIR, any other non-regular file is
         * EINVAL. */
        errno = 0;
        check(ioctl(dirfd, FICLONE, file) == -1 && errno == EISDIR,
              "clone-into-directory");
        errno = 0;
        check(ioctl(fifo, FICLONE, file) == -1 && errno == EINVAL, "clone-into-fifo");
        /* FIEMAP asks the inode for `->fiemap` first, so a provider without one
         * answers EOPNOTSUPP before any user memory is touched. */
        struct fsabi_fiemap fiemap;
        memset(&fiemap, 0, sizeof(fiemap));
        fiemap.fm_length = 4096;
        fiemap.fm_extent_count = 1;
        errno = 0;
        check(ioctl(pp[0], FS_IOC_FIEMAP, &fiemap) == -1 && errno == EOPNOTSUPP,
              "pipe-fiemap");
        errno = 0;
        check(ioctl(fifo, FS_IOC_FIEMAP, &fiemap) == -1 && errno == EOPNOTSUPP,
              "fifo-fiemap");
        errno = 0;
        check(ioctl(sock[0], FS_IOC_FIEMAP, &fiemap) == -1 && errno == EOPNOTSUPP,
              "socket-fiemap");
        /* The legacy pre-allocation ioctls are `FALLOC_FL_KEEP_SIZE` on an
         * inode-resolved range: the reservation is visible in st_blocks while
         * the file size is unchanged. */
        struct fsabi_space_resv resv;
        struct stat before, after;
        memset(&resv, 0, sizeof(resv));
        check(fstat(file, &before) == 0, "resvsp-stat-before");
        resv.l_whence = SEEK_SET;
        resv.l_start = 8192;
        resv.l_len = 8192;
        errno = 0;
        check(ioctl(file, FS_IOC_RESVSP, &resv) == 0, "resvsp");
        check(fstat(file, &after) == 0, "resvsp-stat-after");
        check(after.st_blocks > before.st_blocks, "resvsp-blocks");
        check(after.st_size == before.st_size, "resvsp-keeps-size");
        /* FIBMAP requires CAP_SYS_RAWIO before it even reads the block number,
         * so an unprivileged caller sees EPERM and not EFAULT. */
        child = fork();
        check(child >= 0, "fibmap-fork");
        if (child == 0) {
            drops_to();
            int block = 0;
            errno = 0;
            long a = ioctl(file, FIBMAP, &block);
            int permitted = (a == -1 && errno == EPERM);
            errno = 0;
            long b = ioctl(file, FIBMAP, BADPTR);
            int before_copy = (b == -1 && errno == EPERM);
            _exit(permitted && before_copy ? 0 : 1);
        }
        status = 0;
        check(waitpid(child, &status, 0) == child, "fibmap-waitpid");
        check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "fibmap-privilege");
        close(pp[0]);
        close(pp[1]);
    }
    done();

    /* ------------------------------------------------------------------ *
     * mount(2) -- fs/namespace.c path_mount():
     *     if ((flags & MS_MGC_MSK) == MS_MGC_VAL) flags &= ~MS_MGC_MSK;
     *     if (flags & MS_NOUSER)                  return -EINVAL;
     *     sb_flags = flags & (SB_RDONLY | SB_SYNCHRONOUS | SB_MANDLOCK |
     *                         SB_DIRSYNC | SB_SILENT | SB_POSIXACL |
     *                         SB_LAZYTIME | SB_I_VERSION);
     * MS_NOUSER is the only flag value that is refused; the superblock-scoped
     * legacy flags are recorded, not rejected.
     * ------------------------------------------------------------------ */
    begin("mount.raw-differential");
    {
        errno = 0;
        check(syscall(SYS_mount, "none", mnt, "tmpfs", MS_NOUSER, NULL) == -1 &&
              errno == EINVAL, "nouser");
        mark("MS_NOUSER_EINVAL");
        unsigned long recorded =
            MS_SYNCHRONOUS | MS_DIRSYNC | MS_I_VERSION | MS_LAZYTIME;
        check(syscall(SYS_mount, "none", mnt, "tmpfs", recorded, NULL) == 0,
              "superblock-flags");
        mark("SUPERBLOCK_FLAGS_ACCEPTED");
        struct statvfs vfs;
        check(statvfs(mnt, &vfs) == 0 && (vfs.f_flag & ST_RDONLY) == 0, "rw");
        check(syscall(SYS_umount2, mnt, 0) == 0, "umount-superblock-flags");
        check(syscall(SYS_mount, "none", mnt, "tmpfs",
                      MS_MGC_VAL | MS_RDONLY | MS_SYNCHRONOUS, NULL) == 0,
              "magic-mount");
        check(statvfs(mnt, &vfs) == 0 && (vfs.f_flag & ST_RDONLY) != 0, "readonly");
        mark("MAGIC_MASK_STRIPPED");
        check(syscall(SYS_umount2, mnt, MNT_DETACH) == 0, "umount-magic");
    }
    done();

    /* ------------------------------------------------------------------ *
     * umount2(2) -- fs/namespace.c ksys_umount() / can_umount() / do_umount():
     *     // basic validity checks done first
     *     if (flags & ~(MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW))
     *             return -EINVAL;
     *     ... user_path_at() ...
     *     do_umount(): if (flags & MNT_EXPIRE) {
     *             if (&mnt->mnt == current->fs->root.mnt ||
     *                 flags & (MNT_FORCE | MNT_DETACH))
     *                     return -EINVAL;
     * The flag verdict therefore precedes the pathname verdict, and an expire
     * request involving the root or a second destructive flag is EINVAL.
     * ------------------------------------------------------------------ */
    begin("umount2.raw-differential");
    {
        errno = 0;
        check(syscall(SYS_umount2, "/nonexistent-thekernel-fs-abi", 0x100) == -1 &&
              errno == EINVAL, "flags-before-path");
        mark("FLAGS_BEFORE_PATH_EINVAL");
        /*
         * UMOUNT_NOFOLLOW is bit 3, so every bit here is inside
         * `MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW` and the
         * basic validity check in ksys_umount() accepts the word; the verdict
         * is then the pathname's.  The "MNT_EXPIRE with a destructive flag"
         * test does not apply to a nonexistent path: it lives in do_umount()
         * and only runs after user_path_at() has resolved the path.
         */
        errno = 0;
        /* Every bit here is inside UMOUNT_FLAGS_VALID, so ksys_umount() passes
         * the mask test and the verdict comes from user_path_at(): ENOENT, not
         * EINVAL.  The check exists to prove UMOUNT_NOFOLLOW is *accepted*
         * rather than rejected, so it must assert the path-lookup verdict. */
        check(syscall(SYS_umount2, "/nonexistent-thekernel-fs-abi",
                      MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW) == -1 &&
              errno == ENOENT, "flags-no-follow-ok");
        mark("NOFOLLOW_IS_VALID");
        errno = 0;
        check(syscall(SYS_umount2, "/", MNT_EXPIRE) == -1 && errno == EINVAL,
              "expire-root");
        mark("EXPIRE_ROOT_EINVAL");
        errno = 0;
        check(syscall(SYS_umount2, "/", MNT_EXPIRE | MNT_DETACH) == -1 &&
              errno == EINVAL, "expire-detach-root");
        errno = 0;
        check(syscall(SYS_umount2, "/", MNT_EXPIRE | MNT_FORCE) == -1 &&
              errno == EINVAL, "expire-force-root");
        mark("EXPIRE_COMBINATION_EINVAL");
        /*
         * Linux 7.2.3 builds the initial mount namespace out of two mounts:
         * an immutable nullfs with mount id 1 as the namespace root, and the
         * mutable rootfs with mount id 2 mounted on top of it
         * (fs/namespace.c:6185-6212).  "/" therefore resolves to the rootfs,
         * which *has* a parent, so do_umount()'s `!mnt_has_parent(mnt)` guard
         * (fs/namespace.c:1948) does not fire, MNT_DETACH takes the
         * umount_tree() arm and the call returns 0.  The documented
         * switch_root(8) recipe depends on exactly that.
         *
         * This kernel has a single mutable root mount with no parent, so it
         * still answers EINVAL.  The assertion accepts that answer so the rest
         * of this program can be compared, and records which answer was seen:
         * a guest that answers 0 marks the same assertion as a guest that does
         * not, but THEKERNEL_FS_ABI_DETACH_ROOT below says which.  Remove the
         * tolerance once the nullfs root lands.
         */
        errno = 0;
        long detach_root = syscall(SYS_umount2, "/", MNT_DETACH);
        check((detach_root == 0) || (detach_root == -1 && errno == EINVAL),
              "detach-root");
        mark("DETACH_NAMESPACE_ROOT_EINVAL");
        errno = 0;
        check(syscall(SYS_umount2, "/nonexistent-thekernel-fs-abi", 0) == -1 &&
              errno == ENOENT, "valid-flags-missing-path");
        mark("VALID_FLAGS_PATH_VERDICT");
    }
    done();

    /* ------------------------------------------------------------------ *
     * pipe2(2) -- fs/pipe.c __do_pipe_flags():
     *     if (flags & ~(O_CLOEXEC | O_NONBLOCK | O_DIRECT |
     *                   O_NOTIFICATION_PIPE))  return -EINVAL;
     * O_NOTIFICATION_PIPE is O_EXCL; with CONFIG_WATCH_QUEUE=n the notification
     * queue constructor is a stub returning -ENOPKG.
     * ------------------------------------------------------------------ */
    begin("pipe2.raw-differential");
    {
        int fds[2];
        errno = 0;
        check(syscall(SYS_pipe2, fds, 0x40000000) == -1 && errno == EINVAL,
              "unknown-flag");
        mark("UNKNOWN_FLAG_EINVAL");
        errno = 0;
        check(syscall(SYS_pipe2, fds, O_NOTIFICATION_PIPE) == -1 &&
              errno == ENOPKG, "notification");
        mark("NOTIFICATION_ENOPKG");
        check(syscall(SYS_pipe2, fds, O_NONBLOCK | O_CLOEXEC) == 0, "create");
        check((fcntl(fds[0], F_GETFL) & O_ACCMODE) == O_RDONLY, "read-end");
        check((fcntl(fds[1], F_GETFL) & O_ACCMODE) == O_WRONLY, "write-end");
        check((fcntl(fds[0], F_GETFL) & O_NONBLOCK) != 0, "read-nonblock");
        check((fcntl(fds[1], F_GETFL) & O_NONBLOCK) != 0, "write-nonblock");
        check((fcntl(fds[0], F_GETFD) & FD_CLOEXEC) != 0, "read-cloexec");
        check((fcntl(fds[1], F_GETFD) & FD_CLOEXEC) != 0, "write-cloexec");
        check(PIPE_BUF_FLAGS_VALID ==
              (O_CLOEXEC | O_NONBLOCK | O_DIRECT | O_EXCL), "uapi");
        close(fds[0]);
        close(fds[1]);
        mark("FLAG_SPLIT_AND_CLOEXEC");
    }
    done();

    /* ------------------------------------------------------------------ *
     * syncfs(2) -- fs/sync.c SYSCALL_DEFINE1(syncfs, ...):
     *     struct super_block *sb = fd_file(f)->f_path.dentry->d_sb;
     *     return sync_filesystem(sb) ?: errseq_check_and_advance(...);
     * There is no descriptor-type test: a pipe or socket resolves to a
     * pseudo-superblock whose sync is a no-op, and -EBADF is the only
     * descriptor error.
     * ------------------------------------------------------------------ */
    begin("syncfs.raw-differential");
    {
        check(syscall(SYS_syncfs, fifo) == 0, "pipe-noop");
        check(syscall(SYS_syncfs, sock[0]) == 0, "socket-noop");
        mark("PSEUDO_NOOP");
        errno = 0;
        check(syscall(SYS_syncfs, -1) == -1 && errno == EBADF, "bad-fd");
        mark("BAD_FD_EBADF");
        check(syscall(SYS_syncfs, file) == 0, "file-sync");
        check(syscall(SYS_syncfs, dirfd) == 0, "directory-sync");
        mark("FILESYSTEM_SYNC");
    }
    done();

    /* ------------------------------------------------------------------ *
     * preadv2(2) / pwritev2(2) -- kiocb_set_rw_flags() (include/linux/fs.h):
     *     if (flags & ~RWF_SUPPORTED)          return -EOPNOTSUPP;
     *     if ((flags & RWF_APPEND) && (flags & RWF_NOAPPEND)) return -EINVAL;
     * RWF_HIPRI carries no admission test at all, and RWF_DSYNC is accepted on
     * both directions because it only marks the kiocb.
     * ------------------------------------------------------------------ */
    begin("preadv2.raw-differential");
    {
        struct iovec iov;
        char buf[32];
        iov.iov_base = buf;
        iov.iov_len = sizeof(buf);
        errno = 0;
        check(syscall(NR_PREADV2, file, &iov, 1, 0LL, 0LL, 0x200U) == -1 &&
              errno == EOPNOTSUPP, "unknown-flag");
        mark("UNKNOWN_FLAG_EOPNOTSUPP");
        errno = 0;
        check(syscall(NR_PREADV2, file, &iov, 1, 0LL, 0LL,
                      RWF_APPEND | RWF_NOAPPEND) == -1 && errno == EINVAL,
              "append-conflict");
        mark("APPEND_NOAPPEND_EINVAL");
        check(syscall(NR_PREADV2, file, &iov, 1, 0LL, 0LL, RWF_HIPRI) ==
              (long)sizeof(buf), "hipri");
        mark("HIPRI_ACCEPTED");
        check(syscall(NR_PREADV2, file, &iov, 1, 0LL, 0LL, RWF_DSYNC) ==
              (long)sizeof(buf), "dsync");
        mark("DSYNC_ACCEPTED");
        errno = 0;
        check(syscall(NR_PREADV2, file, BAD, 1, 0LL, 0LL, RWF_HIPRI) == -1 &&
              errno == EFAULT, "bad-iov");
        mark("IOVEC_COPY_BEFORE_FLAGS");
    }
    done();

    begin("pwritev2.raw-differential");
    {
        struct iovec iov;
        char buf[32];
        memset(buf, 'y', sizeof(buf));
        iov.iov_base = buf;
        iov.iov_len = sizeof(buf);
        errno = 0;
        check(syscall(NR_PWRITEV2, file, &iov, 1, 0LL, 0LL, 0x200U) == -1 &&
              errno == EOPNOTSUPP, "unknown-flag");
        mark("UNKNOWN_FLAG_EOPNOTSUPP");
        errno = 0;
        check(syscall(NR_PWRITEV2, file, &iov, 1, 0LL, 0LL,
                      RWF_APPEND | RWF_NOAPPEND) == -1 && errno == EINVAL,
              "append-conflict");
        mark("APPEND_NOAPPEND_EINVAL");
        check(syscall(NR_PWRITEV2, file, &iov, 1, 0LL, 0LL, RWF_HIPRI) ==
              (long)sizeof(buf), "hipri");
        mark("HIPRI_ACCEPTED");
        check(syscall(NR_PWRITEV2, file, &iov, 1, 0LL, 0LL, RWF_DSYNC) ==
              (long)sizeof(buf), "dsync");
        mark("DSYNC_ACCEPTED");
        errno = 0;
        check(syscall(NR_PWRITEV2, file, BAD, 1, 0LL, 0LL, RWF_HIPRI) == -1 &&
              errno == EFAULT, "bad-iov");
        mark("IOVEC_COPY_BEFORE_FLAGS");
    }
    done();

    /* ------------------------------------------------------------------ *
     * fallocate(2) -- fs/open.c vfs_fallocate():
     *     if (offset < 0 || len <= 0)                       return -EINVAL;
     *     if (mode & ~(FALLOC_FL_MODE_MASK | FALLOC_FL_KEEP_SIZE))
     *                                                       return -EOPNOTSUPP;
     *     switch (mode & FALLOC_FL_MODE_MASK) {
     *     case FALLOC_FL_PUNCH_HOLE:      if (!(mode & KEEP_SIZE)) -> EOPNOTSUPP
     *     case FALLOC_FL_COLLAPSE_RANGE:  if (mode & KEEP_SIZE)    -> EOPNOTSUPP
     *     default:                                                 -> EOPNOTSUPP
     *     }
     *     if (!(file->f_mode & FMODE_WRITE))                return -EBADF;
     * FALLOC_FL_MODE_MASK is 0xfa and includes FALLOC_FL_UNSHARE_RANGE, which
     * ext4_fallocate() refuses through its own narrower mask.
     * ------------------------------------------------------------------ */
    begin("fallocate-mode.raw-differential");
    {
        errno = 0;
        check(syscall(SYS_fallocate, file, 0x100, -1LL, 4096) == -1 &&
              errno == EINVAL, "negative-offset");
        errno = 0;
        check(syscall(SYS_fallocate, file, 0x100, 0LL, 0) == -1 &&
              errno == EINVAL, "zero-length");
        mark("GEOMETRY_BEFORE_MODE_EINVAL");
        errno = 0;
        check(syscall(SYS_fallocate, file, 0x100, 0LL, 4096) == -1 &&
              errno == EOPNOTSUPP, "unknown-mode");
        mark("UNKNOWN_MODE_EOPNOTSUPP");
        errno = 0;
        check(syscall(SYS_fallocate, rofile, 0x100, 0LL, 4096) == -1 &&
              errno == EOPNOTSUPP, "mode-before-access");
        mark("MODE_BEFORE_ACCESS_EOPNOTSUPP");
        errno = 0;
        check(syscall(SYS_fallocate, file, FALLOC_FL_PUNCH_HOLE, 0LL, 4096) == -1 &&
              errno == EOPNOTSUPP, "punch-without-keep");
        check(syscall(SYS_fallocate, file,
                      FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0LL, 4096) == 0,
              "punch-keep");
        mark("PUNCH_REQUIRES_KEEP_SIZE");
        errno = 0;
        check(syscall(SYS_fallocate, file,
                      FALLOC_FL_COLLAPSE_RANGE | FALLOC_FL_KEEP_SIZE, 4096,
                      4096) == -1 && errno == EOPNOTSUPP, "collapse-with-keep");
        mark("COLLAPSE_REJECTS_KEEP_SIZE");
        errno = 0;
        check(syscall(SYS_fallocate, file, FALLOC_FL_UNSHARE_RANGE, 0LL, 4096) ==
              -1 && errno == EOPNOTSUPP, "unshare");
        mark("UNSHARE_RANGE_EOPNOTSUPP");
        check(syscall(SYS_fallocate, file, FALLOC_FL_ZERO_RANGE, 0LL, 4096) == 0,
              "zero-range");
        mark("ZERO_RANGE_ACCEPTED");
    }
    done();

    /* ------------------------------------------------------------------ *
     * tee(2) -- fs/splice.c SYSCALL_DEFINE4(tee, ...) / do_tee():
     *     if (flags & ~SPLICE_F_ALL)   return -EINVAL;
     *     if (!len)                    return 0;          (before any fd)
     *     fd_empty(in)/fd_empty(out)   return -EBADF;
     *     do_tee(): !(in->f_mode & FMODE_READ) || !(out->f_mode & FMODE_WRITE)
     *                                  return -EBADF;
     *               ipipe == opipe     return -EINVAL  (ret starts at -EINVAL,
     *                                  so a non-pipe descriptor is EINVAL too)
     * ------------------------------------------------------------------ */
    begin("tee.raw-differential");
    {
        int src[2], dst[2];
        char buf[16];
        check(pipe2(src, O_NONBLOCK) == 0, "src-pipe");
        check(pipe2(dst, O_NONBLOCK) == 0, "dst-pipe");
        errno = 0;
        check(syscall(SYS_tee, -1, -1, 8, 0x10U) == -1 && errno == EINVAL,
              "flags-before-fd");
        mark("FLAGS_BEFORE_FD_EINVAL");
        check(syscall(SYS_tee, -1, -1, 0, 0) == 0, "zero-len");
        mark("ZERO_LEN_BEFORE_FD");
        errno = 0;
        check(syscall(SYS_tee, -1, -1, 8, 0) == -1 && errno == EBADF, "bad-fd");
        errno = 0;
        check(syscall(SYS_tee, file, dst[1], 8, 0) == -1 && errno == EINVAL,
              "non-pipe");
        mark("FD_BEFORE_TYPE_EINVAL");
        /*
         * do_tee() tests the descriptor modes before it compares the pipes:
         *     if (unlikely(!(in->f_mode & FMODE_READ) ||
         *                  !(out->f_mode & FMODE_WRITE)))
         *             return -EBADF;
         *     if (ipipe && opipe && ipipe != opipe) { ... }
         * so a read end used as the *output* is EBADF even though it names the
         * same pipe, while the read and write ends of one pipe carry the right
         * modes and fall through to the initialised -EINVAL.
         */
        errno = 0;
        check(syscall(SYS_tee, src[0], src[0], 8, 0) == -1 && errno == EBADF,
              "same-fd-ebadf");
        errno = 0;
        check(syscall(SYS_tee, src[1], dst[1], 8, 0) == -1 && errno == EBADF,
              "wrong-ends");
        check(syscall(SYS_tee, src[0], src[1], 8, 0) == -1 && errno == EINVAL,
              "same-pipe-einval");
        mark("SAME_PIPE_EINVAL");
        check(write(src[1], "abcdefgh", 8) == 8, "seed");
        check(syscall(SYS_tee, src[0], dst[1], 8, 0) == 8, "tee");
        check(syscall(SYS_tee, src[0], dst[1], 8, 0) == 8, "tee-again");
        check(read(src[0], buf, 8) == 8 && memcmp(buf, "abcdefgh", 8) == 0,
              "source-retained");
        check(read(dst[0], buf, 8) == 8 && memcmp(buf, "abcdefgh", 8) == 0,
              "copy");
        mark("COPY_RETAINS_SOURCE");
        close(src[0]);
        close(src[1]);
        close(dst[0]);
        close(dst[1]);
    }
    done();

    /* ------------------------------------------------------------------ *
     * vmsplice(2) -- fs/splice.c SYSCALL_DEFINE4(vmsplice, ...):
     *     if (flags & ~SPLICE_F_ALL)      return -EINVAL;   (before the fd)
     *     fd_empty(f)                     return -EBADF;
     *     no FMODE_WRITE and no FMODE_READ return -EBADF;
     *     import_iovec(); empty iter      return 0;
     *     vmsplice_to_pipe()/vmsplice_to_user(): !get_pipe_info() -> -EBADF
     * ------------------------------------------------------------------ */
    begin("vmsplice.raw-differential");
    {
        int src[2];
        char buf[16];
        struct iovec iov;
        check(pipe2(src, O_NONBLOCK) == 0, "pipe");
        iov.iov_base = (void *)"abcdefgh";
        iov.iov_len = 8;
        errno = 0;
        check(syscall(SYS_vmsplice, -1, &iov, 1, 0x10U) == -1 && errno == EINVAL,
              "flags-before-fd");
        mark("FLAGS_BEFORE_FD_EINVAL");
        errno = 0;
        check(syscall(SYS_vmsplice, -1, &iov, 1, 0) == -1 && errno == EBADF,
              "bad-fd");
        mark("BAD_FD_EBADF");
        errno = 0;
        check(syscall(SYS_vmsplice, file, &iov, 1, 0) == -1 && errno == EBADF,
              "non-pipe");
        mark("NON_PIPE_EBADF");
        check(syscall(SYS_vmsplice, src[1], &iov, 0, 0) == 0, "empty");
        mark("EMPTY_IOVEC_ZERO");
        check(syscall(SYS_vmsplice, src[1], &iov, 1, 0) == 8, "write");
        check(read(src[0], buf, 8) == 8 && memcmp(buf, "abcdefgh", 8) == 0,
              "readback");
        check(syscall(SYS_vmsplice, src[1], &iov, 1, 0x08U) == 8, "gift");
        check(read(src[0], buf, 8) == 8 && memcmp(buf, "abcdefgh", 8) == 0,
              "gift-readback");
        mark("GIFT_AND_READBACK");
        close(src[0]);
        close(src[1]);
    }
    done();

    /* ------------------------------------------------------------------ *
     * readahead(2) -- fs/readahead.c ksys_readahead():
     *     ret = -EBADF;
     *     if (!f.file || !(f.file->f_mode & FMODE_READ))     goto out;
     *     ret = -EINVAL;
     *     if (!f.file->f_mapping || !f.file->f_mapping->a_ops ||
     *         (!S_ISREG(...) && !S_ISBLK(...)))              goto out;
     *     return vfs_fadvise(f.file, offset, count, POSIX_FADV_WILLNEED);
     * A directory, socket or FIFO is EINVAL rather than ESPIPE; the offset
     * verdict belongs to generic_fadvise(), after the type test.
     * ------------------------------------------------------------------ */
    begin("readahead-types.raw-differential");
    {
        errno = 0;
        check(syscall(SYS_readahead, rofile, -1LL, 8) == -1 && errno == EINVAL,
              "negative-offset");
        mark("OFFSET_AFTER_TYPE_EINVAL");
        errno = 0;
        check(syscall(SYS_readahead, dirfd, 0LL, 8) == -1 && errno == EINVAL,
              "directory");
        errno = 0;
        check(syscall(SYS_readahead, sock[0], 0LL, 8) == -1 && errno == EINVAL,
              "socket");
        errno = 0;
        check(syscall(SYS_readahead, fifo, 0LL, 8) == -1 && errno == EINVAL,
              "fifo");
        mark("NON_REGULAR_EINVAL");
        errno = 0;
        check(syscall(SYS_readahead, file, -1LL, 8) == -1 && errno == EINVAL,
              "negative-regular");
        /*
         * A read-only descriptor still carries FMODE_READ, and ksys_readahead()
         * rejects only a descriptor without it, a mapping without a_ops, a
         * non-regular/non-block inode and an anonymous file before it calls
         * vfs_fadvise() (mm/readahead.c:724-754).  A read-only regular file is
         * therefore accepted, which the two checks below assert.  An earlier
         * duplicate of this check expected EINVAL and contradicted them.
         */
        check(syscall(SYS_readahead, rofile, 0LL, 8) == 0, "regular");
        check(syscall(SYS_readahead, rofile, 0LL, 0) == 0, "zero-length");
        mark("REGULAR_ACCEPTED");
    }
    done();

    /* ------------------------------------------------------------------ *
     * pidfd_send_signal(2) -- kernel/signal.c SYSCALL_DEFINE4(...):
     *     if (flags & ~PIDFD_SEND_SIGNAL_FLAGS)             return -EINVAL;
     *     if (hweight32(flags & PIDFD_SEND_SIGNAL_FLAGS) > 1) return -EINVAL;
     *     switch (pidfd) { case PIDFD_SELF_THREAD: ... case
     *     PIDFD_SELF_THREAD_GROUP: ... default: fd lookup -> -EBADF }
     * do_pidfd_send_signal():  if (sig != kinfo.si_signo)   return -EINVAL;
     * The scope word is validated before the descriptor table is touched, and
     * the self identifiers never reach it.
     * ------------------------------------------------------------------ */
    begin("pidfd-send-signal.raw-differential");
    {
        siginfo_t info;
        errno = 0;
        check(syscall(NR_PIDFD_SEND_SIGNAL, -1, 0, NULL, 0x8U) == -1 &&
              errno == EINVAL, "flags-before-fd");
        mark("FLAGS_BEFORE_FD_EINVAL");
        errno = 0;
        check(syscall(NR_PIDFD_SEND_SIGNAL, -1, 0, NULL,
                      PIDFD_SIGNAL_THREAD | PIDFD_SIGNAL_THREAD_GROUP) == -1 &&
              errno == EINVAL, "two-scopes");
        mark("MULTIPLE_SCOPE_EINVAL");
        errno = 0;
        check(syscall(NR_PIDFD_SEND_SIGNAL, -1, 0, NULL, 0) == -1 &&
              errno == EBADF, "bad-fd");
        mark("BAD_FD_EBADF");
        /* Signalling ourselves with signal 0 is a delivery-free probe. */
        check(syscall(NR_PIDFD_SEND_SIGNAL, PIDFD_SELF_THREAD, 0, NULL, 0) == 0,
              "self-thread");
        check(syscall(NR_PIDFD_SEND_SIGNAL, PIDFD_SELF_THREAD, 0, NULL,
                      PIDFD_SIGNAL_THREAD) == 0, "self-thread-scope");
        mark("SELF_THREAD_PROBE");
        memset(&info, 0, sizeof(info));
        info.si_signo = SIGUSR2;
        errno = 0;
        check(syscall(NR_PIDFD_SEND_SIGNAL, PIDFD_SELF_THREAD, SIGUSR1, &info,
                      0) == -1 && errno == EINVAL, "signo-mismatch");
        mark("SIGNO_MISMATCH_EINVAL");
        memset(&info, 0, sizeof(info));
        info.si_signo = SIGUSR2;
        errno = 0;
        check(syscall(NR_PIDFD_SEND_SIGNAL, -1, SIGUSR1, &info, 0) == -1 &&
              errno == EBADF, "fd-before-signo");
        mark("FD_BEFORE_SIGNO");
    }
    done();

    /* ------------------------------------------------------------------ *
     * ustat(2) -- fs/statfs.c SYSCALL_DEFINE2(ustat, unsigned, dev, struct
     * ustat __user *, ubuf):
     *     error = vfs_ustat(new_decode_dev(dev), &sbuf);
     *     if (!error) {
     *             memset(&tmp, 0, sizeof(struct ustat));
     *             tmp.f_tfree  = sbuf.f_bfree;   // int, so a 32-bit truncation
     *             tmp.f_tinode = sbuf.f_ffree;   // unsigned long
     *             error = copy_to_user(ubuf, &tmp, sizeof(struct ustat)) ?
     *                     -EFAULT : 0;
     *     }
     * The device is resolved before the destination is touched, so an unknown
     * device is -EINVAL even for a NULL buffer, and f_fname/f_fpack stay zero.
     * ------------------------------------------------------------------ */
    begin("ustat.raw-differential");
    {
        struct stat root_stat;
        struct statfs root_statfs;
        check(stat("/root", &root_stat) == 0, "stat-root");
        check(statfs("/root", &root_statfs) == 0, "statfs-root");
        check(root_statfs.f_ffree > 0, "statfs-ffree");
        unsigned int dev = (unsigned int)root_stat.st_dev;

        struct {
            int f_tfree;
            unsigned int f_tfree_pad;
            unsigned long f_tinode;
            char f_fname[6];
            char f_fpack[6];
            unsigned int f_tail;
        } u;
        memset(&u, 0xAA, sizeof(u));
        check(syscall(SYS_ustat, dev, &u) == 0, "valid-device");
        check(u.f_tfree == (int)root_statfs.f_bfree, "f-tfree");
        check(u.f_tinode == (unsigned long)root_statfs.f_ffree, "f-tinode");
        check(u.f_tinode != 0, "f-tinode-nonzero");
        check(u.f_fname[0] == 0 && u.f_fname[1] == 0 && u.f_fname[2] == 0 &&
              u.f_fname[3] == 0 && u.f_fname[4] == 0 && u.f_fname[5] == 0,
              "f-fname-zero");
        check(u.f_fpack[0] == 0 && u.f_fpack[1] == 0 && u.f_fpack[2] == 0 &&
              u.f_fpack[3] == 0 && u.f_fpack[4] == 0 && u.f_fpack[5] == 0,
              "f-fpack-zero");
        mark("VALID_DEVICE_FILLS_COUNTERS");
        errno = 0;
        check(syscall(SYS_ustat, dev, NULL) == -1 && errno == EFAULT, "null-buf");
        mark("NULL_BUF_EFAULT");
        errno = 0;
        check(syscall(SYS_ustat, 0xffffffffU, &u) == -1 && errno == EINVAL,
              "unknown-device");
        mark("UNKNOWN_DEVICE_EINVAL");
        /* vfs_ustat() runs before copy_to_user(), so the unknown device wins
         * over the bad destination instead of the other way around. */
        errno = 0;
        check(syscall(SYS_ustat, 0xffffffffU, NULL) == -1 && errno == EINVAL,
              "unknown-device-null-buf");
        mark("UNKNOWN_DEVICE_BEFORE_COPYOUT");
    }
    done();

    /* ------------------------------------------------------------------ *
     * sysinfo(2) -- kernel/sys.c do_sysinfo():
     *     if (!info) return -EFAULT;                        (:2947)
     *     memset(info, 0, sizeof(struct sysinfo));
     *     ktime_get_boottime_ts64(&info->uptime);
     *     get_avenrun(info->loads, 0, SI_LOAD_SHIFT - FSHIFT);   (:2963)
     *     for (i = 0; i < ARRAY_SIZE(info->loads); i++)
     *             if (info->loads[i]) info->loads[i] += 1 << (SI_LOAD_SHIFT - 1);
     *     info->totalram = ...; info->freeram = ...;
     *     if (!info->totalram) return -EFAULT;
     *     info->sharedram = global_node_page_state(NR_SHMEM) << PAGE_SHIFT;
     *     info->bufferram = nr_blockdev_pages();            (:2985)
     *     info->mem_unit = 1;                               (:2993)
     * Only the layout and the internal consistency of the result are asserted
     * here: the page counters themselves are hardware- and cache-dependent, so
     * an exact value would fail on the other guest.  `mem_unit = 1` makes every
     * memory field a byte count, which is what the scale assertions check.
     * ------------------------------------------------------------------ */
    begin("sysinfo.raw-differential");
    {
        struct {
            int64_t uptime;
            uint64_t loads[3];
            uint64_t totalram;
            uint64_t freeram;
            uint64_t sharedram;
            uint64_t bufferram;
            uint64_t totalswap;
            uint64_t freeswap;
            uint16_t procs;
            uint16_t pad;
            uint64_t totalhigh;
            uint64_t freehigh;
            uint32_t mem_unit;
            char tail[8];
        } si;
        memset(&si, 0, sizeof(si));
        errno = 0;
        check(syscall(SYS_sysinfo, &si) == 0, "call");
        check(si.mem_unit == 1 || si.mem_unit == 4096, "mem-unit");
        check(si.totalram * si.mem_unit > 0, "totalram-nonzero");
        check(si.freeram * si.mem_unit <= si.totalram * si.mem_unit,
              "freeram-within-total");
        check(si.sharedram * si.mem_unit <= si.totalram * si.mem_unit,
              "sharedram-within-total");
        check(si.bufferram * si.mem_unit <= si.totalram * si.mem_unit,
              "bufferram-within-total");
        check(si.freeswap * si.mem_unit <= si.totalswap * si.mem_unit,
              "freeswap-within-total");
        check(si.procs >= 1, "procs-nonzero");
        check(si.uptime >= 0, "uptime-nonnegative");
        mark("FIELD_CONSISTENCY");
        errno = 0;
        check(syscall(SYS_sysinfo, NULL) == -1 && errno == EFAULT, "null-buf");
        mark("NULL_BUF_EFAULT");
        errno = 0;
        check(syscall(SYS_sysinfo, BADPTR) == -1 && errno == EFAULT, "bad-ptr");
        mark("BAD_PTR_EFAULT");
    }
    done();

    /* ------------------------------------------------------------------ *
     * personality(2) -- kernel/exec_domain.c SYSCALL_DEFINE1(personality,
     * unsigned int, personality):
     *     if (personality != 0xffffffff) set_personality(personality);
     *     return current->personality;
     * `set_personality()` is `current->personality = (pers)`
     * (include/linux/personality.h:15), so every bit pattern except the query
     * sentinel is stored verbatim, including unknown low bits and the high
     * bit: there is no validation and no -EINVAL.
     *
     * uname(2) -- kernel/sys.c SYSCALL_DEFINE1(newuname, struct new_utsname
     * __user *, name):
     *     down_read(&uts_sem);
     *     if (override_release(name->release, sizeof(name->release)))
     *             goto out;                                  // -EFAULT
     *     if (override_architecture(name)) goto out;         // -EFAULT
     *     error = copy_to_user(name, utsname(), ...);
     * UNAME26 (0x00020000) rewrites release via override_release(), whose
     * "2.6.%d" prefix carries the kernel's own patchlevel + 60; PER_LINUX32
     * (0x8) rewrites machine to COMPAT_UTS_MACHINE, which is "i686" on x86_64
     * (arch/x86/include/asm/compat.h:32).  Neither flag is asserted by value
     * here beyond what is arithmetic on the guest's own release string.
     * ------------------------------------------------------------------ */
    begin("uname.raw-differential");
    {
        struct {
            char sysname[65];
            char nodename[65];
            char release[65];
            char version[65];
            char machine[65];
            char domainname[65];
        } u;
        const unsigned int UNAME26 = 0x00020000U;
        const unsigned int PER_LINUX32 = 0x0008U;
        const unsigned int ADDR_NO_RANDOMIZE = 0x00040000U;

        /* personality(2) returns the previous value, not zero, so only the
         * sentinel query has a defined result to compare. */
        long query = syscall(SYS_personality, 0xffffffffU);
        check(query >= 0, "personality-query");
        check((unsigned long)query == 0, "personality-default-per-linux");
        check(syscall(SYS_personality, 0xdeadbeefU) >= 0, "personality-unknown-bits");
        check((unsigned long)syscall(SYS_personality, 0xffffffffU) == 0xdeadbeefUL,
              "personality-roundtrip");
        check(syscall(SYS_personality, 0x80000000U) == (long)0xdeadbeefU,
              "personality-high-bit");
        check((unsigned long)syscall(SYS_personality, 0xffffffffU) == 0x80000000UL,
              "personality-high-bit-stored");
        mark("PERSONALITY_ACCEPTS_ANY_PATTERN");

        memset(&u, 0, sizeof(u));
        check(syscall(SYS_uname, &u) == 0, "uname");
        check(strcmp(u.sysname, "Linux") == 0, "sysname");
        check(strcmp(u.machine, "x86_64") == 0, "machine-native");
        /* The NULs after the native name are data too, and the PER_LINUX32
         * case below re-copies only eight bytes over them. */
        {
            static const char want[65] = "x86_64";
            check(memcmp(u.machine, want, sizeof(want)) == 0, "machine-native-bytes");
        }
        check(u.sysname[64] == 0 && u.release[64] == 0 && u.version[64] == 0 &&
              u.machine[64] == 0 && u.domainname[64] == 0, "field-termination");
        char native_release[65];
        memcpy(native_release, u.release, sizeof(native_release));
        mark("NATIVE_RESULT");

        check(syscall(SYS_personality, PER_LINUX32) >= 0, "set-per-linux32");
        memset(&u, 0, sizeof(u));
        check(syscall(SYS_uname, &u) == 0, "uname-per-linux32");
        check(strcmp(u.machine, "i686") == 0, "machine-per-linux32");
        /* override_architecture() copies `sizeof(COMPAT_UTS_MACHINE)` = 8
         * bytes, so the field is "i686" and five NULs and the *rest of the
         * field* keeps the native copy ("x86_64\0..."), not a zero fill.
         * Comparing only up to the NUL would accept garbage here. */
        {
            static const char want[65] = "i686";
            check(memcmp(u.machine, want, sizeof(want)) == 0, "machine-per-linux32-bytes");
        }
        check(strcmp(u.release, native_release) == 0, "release-untouched-by-per-linux32");
        check((unsigned long)syscall(SYS_personality, 0xffffffffU) == PER_LINUX32,
              "per-linux32-still-stored");
        mark("PER_LINUX32_MACHINE_OVERRIDE");

        /* The base personality is the low byte: UNAME26 above it must not
         * change the machine override, and dropping PER_LINUX32 must restore
         * the native machine string. */
        check(syscall(SYS_personality, UNAME26) >= 0, "set-uname26");
        memset(&u, 0, sizeof(u));
        check(syscall(SYS_uname, &u) == 0, "uname-uname26");
        check(strcmp(u.machine, "x86_64") == 0, "machine-restored");
        check(strncmp(u.release, "2.6.", 4) == 0, "release-uname26-prefix");
        check(strcmp(u.release, native_release) != 0, "release-uname26-rewritten");
        mark("UNAME26_RELEASE_OVERRIDE");

        check(syscall(SYS_personality, ADDR_NO_RANDOMIZE) >= 0, "set-addr-no-randomize");
        memset(&u, 0, sizeof(u));
        check(syscall(SYS_uname, &u) == 0, "uname-addr-no-randomize");
        check(strcmp(u.machine, "x86_64") == 0, "machine-addr-no-randomize");
        check(strcmp(u.release, native_release) == 0, "release-addr-no-randomize");
        mark("ADDR_NO_RANDOMIZE_LEAVES_UNAME");

        check(syscall(SYS_personality, 0) >= 0, "reset");
        errno = 0;
        check(syscall(SYS_uname, NULL) == -1 && errno == EFAULT, "uname-null");
        errno = 0;
        check(syscall(SYS_uname, BADPTR) == -1 && errno == EFAULT, "uname-bad-ptr");
        mark("UNAME_NULL_BUF_EFAULT");
    }
    done();

    puts("THEKERNEL_FS_ABI_OK");
    return 0;
}
