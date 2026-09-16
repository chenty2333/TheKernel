/*
 * File-descriptor lifetime, pidfd and file-handle ABI differential.
 *
 * Every assertion below is an errno-precedence, validation-order or
 * descriptor-state fact taken from the Linux v7.2.3 sources named in each
 * section, so the same binary is meaningful on both the Linux oracle and
 * TheKernel.
 *
 * Sections and their authorities:
 *   close_range        fs/file.c:761-868
 *   openat2            fs/open.c:1133-1306,1393-1418, include/linux/uaccess.h:392-415
 *   name_to_handle_at  fs/fhandle.c:18-168
 *   open_by_handle_at  fs/fhandle.c:170-462
 *   pidfd_open         kernel/pid.c:697-716, kernel/fork.c:1890-1932
 *   pidfd_getfd        kernel/pid.c:881-976, fs/file.c:1385-1406, fs/pidfs.c:706-711
 *
 * Completion marker: THEKERNEL_FD_LIFECYCLE_OK
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* Native x86_64 syscall numbers (arch/x86/entry/syscalls/syscall_64.tbl). */
#ifndef SYS_name_to_handle_at
#define SYS_name_to_handle_at 303
#endif
#ifndef SYS_open_by_handle_at
#define SYS_open_by_handle_at 304
#endif
#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif
#ifndef SYS_close_range
#define SYS_close_range 436
#endif
#ifndef SYS_openat2
#define SYS_openat2 437
#endif
#ifndef SYS_pidfd_getfd
#define SYS_pidfd_getfd 438
#endif

/* include/uapi/linux/close_range.h:6,9 */
#define CLOSE_RANGE_UNSHARE (1U << 1)
#define CLOSE_RANGE_CLOEXEC (1U << 2)

/* include/uapi/linux/pidfd.h:11-12 */
#define PIDFD_NONBLOCK 0x800U /* O_NONBLOCK, include/uapi/asm-generic/fcntl.h:42 */
#define PIDFD_THREAD 0x80U    /* O_EXCL,     include/uapi/asm-generic/fcntl.h:30 */

/* include/uapi/linux/fcntl.h:135,138,183,186,187 and include/linux/exportfs.h:15 */
#ifndef AT_SYMLINK_FOLLOW
#define AT_SYMLINK_FOLLOW 0x400
#endif
#ifndef AT_EMPTY_PATH
#define AT_EMPTY_PATH 0x1000
#endif
#ifndef AT_HANDLE_FID
#define AT_HANDLE_FID 0x200 /* AT_REMOVEDIR, include/uapi/linux/fcntl.h:135 */
#endif
#ifndef AT_HANDLE_MNT_ID_UNIQUE
#define AT_HANDLE_MNT_ID_UNIQUE 0x001
#endif
#ifndef AT_HANDLE_CONNECTABLE
#define AT_HANDLE_CONNECTABLE 0x002
#endif
#ifndef AT_SYMLINK_NOFOLLOW
#define AT_SYMLINK_NOFOLLOW 0x100
#endif
#define MAX_HANDLE_SZ 128
#define FILEID_INVALID 0xff
#define FILEID_IS_CONNECTABLE 0x10000
#define FILEID_IS_DIR 0x20000

/* include/uapi/linux/openat2.h:33-48 (and this tree's :30 OPENAT2_REGULAR). */
#define RESOLVE_NO_XDEV 0x01
#define RESOLVE_NO_MAGICLINKS 0x02
#define RESOLVE_NO_SYMLINKS 0x04
#define RESOLVE_BENEATH 0x08
#define RESOLVE_IN_ROOT 0x10
#define RESOLVE_CACHED 0x20
#ifndef OPENAT2_REGULAR
#define OPENAT2_REGULAR ((uint64_t)1 << 32)
#endif

/* include/uapi/asm-generic/fcntl.h: glibc exports O_LARGEFILE as 0 on x86_64. */
#define LINUX_O_LARGEFILE 00100000
#define LINUX_O_TMPFILE_RAW 0020000000 /* __O_TMPFILE */
#ifndef O_EMPTYPATH
#define O_EMPTYPATH (1 << 26) /* include/uapi/asm-generic/fcntl.h:95-96 */
#endif

struct fd_lifecycle_open_how {
    uint64_t flags;
    uint64_t mode;
    uint64_t resolve;
};

struct fd_lifecycle_handle {
    uint32_t handle_bytes;
    int32_t handle_type;
    unsigned char f_handle[MAX_HANDLE_SZ];
};

#define BADPTR ((void *)(uintptr_t)0x1000)

static const char *active;

static char root[] = "/root/thekernel-fd-lifecycle-XXXXXX";
static int dirfd = -1;
static int filefd = -1; /* "file", O_RDWR */
static int rofd = -1;   /* "file", O_RDONLY */

static void cleanup(void) {
    if (rofd >= 0) (void)close(rofd);
    if (filefd >= 0) (void)close(filefd);
    if (dirfd >= 0) {
        (void)unlinkat(dirfd, "file", 0);
        (void)unlinkat(dirfd, "created", 0);
        (void)unlinkat(dirfd, "slink", 0);
        (void)unlinkat(dirfd, "dangling", 0);
        (void)unlinkat(dirfd, "stale", 0);
        (void)unlinkat(dirfd, "sub/inner", 0);
        (void)unlinkat(dirfd, "sub/slink", 0);
        (void)unlinkat(dirfd, "sub", AT_REMOVEDIR);
        (void)close(dirfd);
    }
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
        fprintf(stderr, "THEKERNEL_FD_LIFECYCLE_FAIL %s %s errno=%d (%s)\n",
                active == NULL ? "exec-stage" : active, stage, saved,
                strerror(saved));
        fflush(NULL);
        _exit(1);
    }
}

#define ERROR(call, expected, stage) do { errno = 0; long r_ = (call); \
    check(r_ == -1 && errno == (expected), (stage)); } while (0)

static int fd_open(int fd) {
    errno = 0;
    return fcntl(fd, F_GETFD) >= 0;
}

static int fd_cloexec(int fd) {
    errno = 0;
    int flags = fcntl(fd, F_GETFD);
    return flags >= 0 && (flags & FD_CLOEXEC) != 0;
}

static int wait_status(pid_t pid) {
    int status = 0;
    if (waitpid(pid, &status, 0) != pid) return -1;
    if (!WIFEXITED(status)) return -1;
    return WEXITSTATUS(status);
}

static long nr_close_range(unsigned int first, unsigned int last,
                           unsigned int flags) {
    return syscall(SYS_close_range, first, last, flags);
}

static long nr_openat2(int dfd, const char *path, const void *how, size_t size) {
    return syscall(SYS_openat2, dfd, path, how, size);
}

static long nr_pidfd_open(int pid, unsigned int flags) {
    return syscall(SYS_pidfd_open, pid, flags);
}

static long nr_pidfd_getfd(int pidfd, int target, unsigned int flags) {
    return syscall(SYS_pidfd_getfd, pidfd, target, flags);
}

static long nr_name_to_handle_at(int dfd, const char *name, void *handle,
                                 void *mnt_id, int flags) {
    return syscall(SYS_name_to_handle_at, dfd, name, handle, mnt_id, flags);
}

static long nr_open_by_handle_at(int mountfd, const void *handle, int flags) {
    return syscall(SYS_open_by_handle_at, mountfd, handle, flags);
}

static long openat2_flags(int dfd, const char *path, uint64_t flags,
                          uint64_t mode, uint64_t resolve) {
    struct fd_lifecycle_open_how how = { flags, mode, resolve };
    return nr_openat2(dfd, path, &how, sizeof(how));
}

/* Runs only after execve(); its exit status is the observation. */
static int exec_stage(int marked, int kept) {
    errno = 0;
    if (fcntl(marked, F_GETFD) != -1 || errno != EBADF) return 1;
    errno = 0;
    int flags = fcntl(kept, F_GETFD);
    if (flags < 0) return 2;
    if (flags & FD_CLOEXEC) return 3;
    char byte = 0;
    if (read(kept, &byte, 1) != 0) return 4; /* /dev/null reads as EOF */
    return 0;
}

/* ==================================================================== */
/* close_range(2) -- fs/file.c:761-868                                  */
/* ==================================================================== */
static void case_close_range(void) {
    begin("close-range.raw-differential");

    /* fs/file.c:824-825 validates the flag mask before anything else. */
    ERROR(nr_close_range(0, 4, 0x8), EINVAL, "close-range-unknown-flag");
    ERROR(nr_close_range(0, 4, 0xfffffff0U), EINVAL, "close-range-high-flags");
    ERROR(nr_close_range(3, 1, 0x8), EINVAL, "close-range-flag-before-range");
    mark("FLAGS_MASK_EINVAL");

    /* fs/file.c:827-828: strict `fd > max_fd`; first == last is valid. */
    ERROR(nr_close_range(4, 3, 0), EINVAL, "close-range-inverted");
    ERROR(nr_close_range(~0U, 0, 0), EINVAL, "close-range-inverted-max");
    mark("RANGE_ORDER_EINVAL");

    int source = open("/dev/null", O_RDONLY);
    check(source >= 0, "close-range-source");
    for (int fd = 20; fd <= 23; fd++)
        check(dup2(source, fd) == fd, "close-range-fixture");
    /* fs/file.c:814-816: [first, last] inclusive; gaps are skipped, never
     * EBADF (fs/file.c:787-789 iterates the open_fds bitmap). */
    check(nr_close_range(20, 21, 0) == 0, "close-range-gap");
    check(!fd_open(31), "close-range-precondition");
    check(nr_close_range(30, 31, 0) == 0, "close-range-closed-slots");
    mark("GAP_TOLERATED");
    check(nr_close_range(20, 21, 0) == 0, "close-range-inclusive");
    check(!fd_open(20) && !fd_open(21), "close-range-closed-first");
    check(fd_open(22) && fd_open(23), "close-range-kept-outside");
    check(nr_close_range(22, 22, 0) == 0, "close-range-single");
    check(!fd_open(22), "close-range-single-closed");
    mark("INCLUSIVE_ENDPOINTS");

    /* fs/file.c:785 min(max_fd, last_fd(fdt)): a range past the table is a
     * silent no-op returning 0. */
    check(nr_close_range(5000, 6000, 0) == 0, "close-range-beyond-table");
    mark("BEYOND_TABLE_OK");

    /* fs/file.c:851-852,761-773: CLOSE_RANGE_CLOEXEC only sets bits. */
    check(nr_close_range(23, 23, CLOSE_RANGE_CLOEXEC) == 0, "close-range-cloexec");
    check(fd_open(23), "close-range-cloexec-still-open");
    check(fd_cloexec(23), "close-range-cloexec-bit");
    int outside = open("/dev/null", O_RDONLY);
    check(outside >= 0 && !fd_cloexec(outside), "close-range-cloexec-outside");
    mark("CLOEXEC_MARKS_WITHOUT_CLOSING");
    mark("CLOEXEC_OUTSIDE_UNTOUCHED");

    /* fs/file.c:605 __set_open_fd(fd, fdt, flags & O_CLOEXEC) clears a stale
     * close_on_exec bit when the slot is handed to a new descriptor. */
    int filler = open("/dev/null", O_RDONLY);
    check(filler >= 0, "close-range-filler");
    for (int fd = 3; fd < 32; fd++)
        if (!fd_open(fd)) check(dup2(filler, fd) == fd, "close-range-fill");
    int reclaimed = open("/dev/null", O_RDONLY);
    check(reclaimed == 32, "close-range-lowest-free");
    check(close(reclaimed) == 0, "close-range-free-slot");
    check(nr_close_range(32, 32, CLOSE_RANGE_CLOEXEC) == 0, "close-range-mark-closed");
    reclaimed = open("/dev/null", O_RDONLY);
    check(reclaimed == 32, "close-range-realloc");
    check(!fd_cloexec(reclaimed), "close-range-stale-bit");
    mark("CLOEXEC_STALE_BIT_CLEARED_ON_ALLOC");
    (void)close(reclaimed);

    /* fs/file.c:851-852 + 864-865: CLOSE_RANGE_UNSHARE|CLOSE_RANGE_CLOEXEC
     * keeps every descriptor (punch_hole = NULL) and only marks them. */
    check(nr_close_range(23, 23, CLOSE_RANGE_UNSHARE | CLOSE_RANGE_CLOEXEC) == 0,
          "close-range-unshare-cloexec");
    check(fd_open(23) && fd_cloexec(23), "close-range-unshare-cloexec-kept");
    mark("UNSHARE_CLOEXEC_KEEPS_FDS");

    /* Without UNSHARE a CLONE_FILES table is shared: the close is visible in
     * the parent.  With UNSHARE (fs/file.c:830-849) the table is copied and
     * the parent keeps its descriptor. */
    check(dup2(source, 40) == 40 && dup2(source, 41) == 41, "close-range-shared");
    pid_t child = (pid_t)syscall(SYS_clone, (unsigned long)(CLONE_FILES | SIGCHLD),
                                 0, 0, 0, 0);
    if (child < 0) check(0, "close-range-clone");
    if (child == 0) _exit(nr_close_range(40, 40, 0) == 0 ? 0 : 1);
    check(wait_status(child) == 0, "close-range-shared-wait");
    check(!fd_open(40), "close-range-shared-visible");
    mark("SHARED_TABLE_CLOSE_VISIBLE");

    child = (pid_t)syscall(SYS_clone, (unsigned long)(CLONE_FILES | SIGCHLD),
                           0, 0, 0, 0);
    if (child < 0) check(0, "close-range-clone-unshare");
    if (child == 0) _exit(nr_close_range(41, 41, CLOSE_RANGE_UNSHARE) == 0 ? 0 : 1);
    check(wait_status(child) == 0, "close-range-unshare-wait");
    check(fd_open(41), "close-range-unshare-isolated");
    mark("UNSHARE_ISOLATES");
    (void)close(41);

    /* CLOSE_RANGE_CLOEXEC is applied by do_close_on_exec(): the marked
     * descriptor is gone after execve(), the unmarked one survives. */
    check(dup2(source, 44) == 44 && dup2(source, 45) == 45, "close-range-exec-fixture");
    check(nr_close_range(44, 44, CLOSE_RANGE_CLOEXEC) == 0, "close-range-exec-mark");
    check(!fd_cloexec(45), "close-range-exec-unmarked");
    child = fork();
    if (child < 0) check(0, "close-range-exec-fork");
    if (child == 0) {
        execl("/proc/self/exe", "fd-lifecycle-differential", "--exec-stage",
              "44", "45", (char *)NULL);
        _exit(97);
    }
    check(wait_status(child) == 0, "close-range-exec-stage");
    mark("CLOEXEC_APPLIED_ACROSS_EXEC");
    (void)close(44);
    (void)close(45);

    /* close_range(0, ~0U, 0) closes every open descriptor including 0/1/2;
     * never EBADF and never a table-size error (fs/file.c:775-805,867). */
    child = fork();
    if (child < 0) check(0, "close-range-full-fork");
    if (child == 0) {
        long r = nr_close_range(0, ~0U, 0);
        int gone = !fd_open(source) && !fd_open(0) && !fd_open(1) && !fd_open(2);
        _exit(r == 0 && gone ? 0 : 1);
    }
    check(wait_status(child) == 0, "close-range-full-child");
    mark("FULL_RANGE_CLOSES_OPEN_FDS");

    (void)close(outside);
    (void)close(23);
    /* Drop descriptors this case created, but never the fixtures later cases
     * still share within this one boot. */
    for (int fd = 3; fd < 32; fd++)
        if (fd != dirfd && fd != filefd && fd != rofd && fd_open(fd))
            (void)close(fd);
    done();
}

/* ==================================================================== */
/* openat2(2) -- fs/open.c:1133-1306,1393-1418                          */
/* ==================================================================== */
static void case_openat2(void) {
    begin("openat2.raw-differential");

    struct fd_lifecycle_open_how how = { O_RDONLY, 0, 0 };

    /* fs/open.c:1402-1405: the struct size gate runs before any user copy and
     * before the filename is touched. */
    ERROR(nr_openat2(AT_FDCWD, root, NULL, 0), EINVAL, "openat2-size-zero");
    ERROR(nr_openat2(AT_FDCWD, root, NULL, 23), EINVAL, "openat2-size-short");
    ERROR(nr_openat2(AT_FDCWD, NULL, NULL, 0), EINVAL, "openat2-size-before-filename");
    ERROR(nr_openat2(AT_FDCWD, NULL, NULL, 4097), E2BIG, "openat2-size-large");
    mark("SIZE_GATE_BEFORE_COPY");

    unsigned char padded[32];
    memset(padded, 0, sizeof(padded));
    memcpy(padded, &how, sizeof(how));
    check(nr_openat2(AT_FDCWD, root, padded, 32) >= 0, "openat2-zero-padding");
    (void)close((int)nr_openat2(AT_FDCWD, root, padded, 32));
    mark("PADDING_ZERO_ACCEPTED");
    padded[24] = 0x01;
    ERROR(nr_openat2(AT_FDCWD, root, padded, 32), E2BIG, "openat2-head-padding");
    padded[24] = 0;
    padded[31] = 0x80;
    ERROR(nr_openat2(AT_FDCWD, root, padded, 32), E2BIG, "openat2-tail-padding");
    mark("PADDING_NONZERO_E2BIG");

    /* include/linux/uaccess.h:404-412 checks the zeroed tail BEFORE copying the
     * head, so a non-zero readable tail wins over an unreadable head. */
    long page = sysconf(_SC_PAGESIZE);
    check(page == 4096, "openat2-page-size");
    unsigned char *two = mmap(NULL, (size_t)page * 2, PROT_READ | PROT_WRITE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check(two != MAP_FAILED, "openat2-mmap");
    check(munmap(two, (size_t)page) == 0, "openat2-munmap-head");
    unsigned char *straddle = two + page - sizeof(how); /* head unmapped */
    memset(two + page, 0xff, 8);
    ERROR(nr_openat2(AT_FDCWD, root, straddle, 32), E2BIG, "openat2-tail-first");
    mark("TAIL_ZERO_CHECK_BEFORE_HEAD_COPY");
    memset(two + page, 0x00, 8);
    ERROR(nr_openat2(AT_FDCWD, root, straddle, 32), EFAULT, "openat2-straddle");
    check(munmap(two + page, (size_t)page) == 0, "openat2-munmap-tail");

    ERROR(nr_openat2(AT_FDCWD, root, NULL, sizeof(how)), EFAULT, "openat2-null-how");
    ERROR(nr_openat2(AT_FDCWD, root, BADPTR, sizeof(how)), EFAULT, "openat2-bad-how");
    mark("HOW_FAULT_EFAULT");

    /* fs/open.c:1180-1181: every bit outside VALID_OPENAT2_FLAGS (with this
     * tree's OPENAT2_REGULAR at 1<<32) is EINVAL. */
    ERROR(openat2_flags(AT_FDCWD, root, O_RDONLY | (1ULL << 63), 0, 0), EINVAL,
          "openat2-flag-63");
    ERROR(openat2_flags(AT_FDCWD, root, O_RDONLY | (1ULL << 33), 0, 0), EINVAL,
          "openat2-flag-33");
    ERROR(openat2_flags(AT_FDCWD, root, 0x80000000ULL, 0, 0), EINVAL,
          "openat2-flag-31");
    mark("FLAGS_MASK_EINVAL");

    ERROR(openat2_flags(AT_FDCWD, root, O_RDONLY, 0, 0x1337), EINVAL,
          "openat2-resolve-mask");
    ERROR(openat2_flags(AT_FDCWD, root, O_RDONLY, 0, 1ULL << 6), EINVAL,
          "openat2-resolve-bit6");
    mark("RESOLVE_MASK_EINVAL");

    /* fs/open.c:1186-1187 */
    ERROR(openat2_flags(AT_FDCWD, root, O_RDONLY, 0, RESOLVE_BENEATH | RESOLVE_IN_ROOT),
          EINVAL, "openat2-scope-conflict");
    mark("BENEATH_IN_ROOT_EINVAL");

    /* fs/open.c:1190-1198 */
    ERROR(openat2_flags(AT_FDCWD, root, O_RDONLY, 0600, 0), EINVAL,
          "openat2-mode-without-create");
    ERROR(openat2_flags(AT_FDCWD, root, O_CREAT | O_WRONLY, 0xffff, 0), EINVAL,
          "openat2-mode-bits");
    ERROR(openat2_flags(AT_FDCWD, root, O_CREAT | O_WRONLY, 0xc000000000000000ULL, 0),
          EINVAL, "openat2-mode-high");
    mark("MODE_VALIDATION_EINVAL");

    /* fs/open.c:1205-1206 */
    ERROR(openat2_flags(AT_FDCWD, root, O_DIRECTORY | O_CREAT | O_RDONLY, 0600, 0),
          EINVAL, "openat2-directory-create");
    mark("DIRECTORY_CREATE_EINVAL");

    /* fs/open.c:1209-1219 */
    ERROR(openat2_flags(AT_FDCWD, root, LINUX_O_TMPFILE_RAW | O_RDWR, 0, 0),
          EINVAL, "openat2-tmpfile-no-directory");
    ERROR(openat2_flags(AT_FDCWD, root, LINUX_O_TMPFILE_RAW | O_DIRECTORY | O_RDONLY,
                        0, 0),
          EINVAL, "openat2-tmpfile-readonly");
    mark("TMPFILE_ADMISSION_EINVAL");

    /* fs/open.c:1228-1231: O_PATH admits only O_PATH_FLAGS. */
    ERROR(openat2_flags(AT_FDCWD, root, O_PATH | O_RDWR, 0, 0), EINVAL,
          "openat2-path-readwrite");
    ERROR(openat2_flags(AT_FDCWD, root, O_PATH | LINUX_O_LARGEFILE, 0, 0), EINVAL,
          "openat2-path-largefile");
    mark("PATH_FLAG_MASK_EINVAL");

    /* fs/open.c:1297-1300: RESOLVE_CACHED with a mutating flag is EAGAIN, and
     * that verdict precedes the filename copy (fs/open.c:1367-1368). */
    ERROR(nr_openat2(AT_FDCWD, BADPTR, &(struct fd_lifecycle_open_how)
                     { O_TRUNC | O_RDWR, 0, RESOLVE_CACHED }, sizeof(how)),
          EAGAIN, "openat2-cached-truncate");
    ERROR(nr_openat2(AT_FDCWD, BADPTR, &(struct fd_lifecycle_open_how)
                     { O_CREAT | O_RDWR, 0600, RESOLVE_CACHED }, sizeof(how)),
          EAGAIN, "openat2-cached-create");
    mark("CACHED_MUTATION_EAGAIN_BEFORE_PATH");

    /* fs/open.c:1413-1415 adds O_LARGEFILE for every non-O_PATH openat2 on
     * x86_64 (include/linux/fcntl.h:42-43); O_CLOEXEC reaches FD_CLOEXEC via
     * FD_ADD(how->flags) (fs/open.c:1368, fs/file.c:605). */
    int fd = (int)openat2_flags(AT_FDCWD, root, O_RDONLY | O_CLOEXEC | O_DIRECTORY, 0, 0);
    check(fd >= 0, "openat2-success");
    int status = fcntl(fd, F_GETFL);
    check(status >= 0, "openat2-getfl");
    check((status & LINUX_O_LARGEFILE) != 0, "openat2-largefile");
    check((status & O_DIRECTORY) != 0, "openat2-directory-bit");
    check((status & O_CLOEXEC) == 0, "openat2-cloexec-hidden");
    check((status & O_CREAT) == 0, "openat2-create-hidden");
    check(fd_cloexec(fd), "openat2-cloexec-fd");
    (void)close(fd);
    int pfd = (int)openat2_flags(AT_FDCWD, root, O_PATH, 0, 0);
    check(pfd >= 0, "openat2-path-open");
    check((fcntl(pfd, F_GETFL) & LINUX_O_LARGEFILE) == 0, "openat2-path-no-largefile");
    (void)close(pfd);
    mark("SUCCESS_FLAG_SHAPE");

    mode_t old = umask(0);
    fd = (int)openat2_flags(dirfd, "created", O_CREAT | O_WRONLY | O_EXCL, 0640, 0);
    umask(old);
    check(fd >= 0, "openat2-create");
    struct stat st;
    check(fstat(fd, &st) == 0 && (st.st_mode & 07777) == 0640, "openat2-create-mode");
    check((fcntl(fd, F_GETFL) & O_CREAT) == 0, "openat2-create-bit-hidden");
    (void)close(fd);
    mark("CREATE_MODE_EFFECT");

    /* fs/namei.c:202-206 with LOOKUP_EMPTY from O_EMPTYPATH
     * (fs/open.c:1273-1274): the flag is admitted and an empty name then
     * denotes the dirfd itself. */
    fd = (int)openat2_flags(AT_FDCWD, root, O_RDONLY | O_EMPTYPATH, 0, 0);
    check(fd >= 0, "openat2-emptypath-flag");
    (void)close(fd);
    mark("EMPTYPATH_FLAG_ACCEPTED");
    fd = (int)openat2_flags(dirfd, "", O_RDONLY | O_EMPTYPATH, 0, 0);
    check(fd >= 0, "openat2-emptypath-open");
    struct stat empty_st, dir_st;
    check(fstat(fd, &empty_st) == 0 && fstat(dirfd, &dir_st) == 0, "openat2-emptypath-stat");
    check(empty_st.st_dev == dir_st.st_dev && empty_st.st_ino == dir_st.st_ino,
          "openat2-emptypath-identity");
    (void)close(fd);
    mark("EMPTYPATH_EMPTY_PATH_OPENS_DIRFD");

    /* fs/namei.c:2040-2042 */
    check(symlinkat("file", dirfd, "slink") == 0 || errno == EEXIST, "openat2-symlink");
    ERROR(openat2_flags(dirfd, "slink", O_RDONLY, 0, RESOLVE_NO_SYMLINKS), ELOOP,
          "openat2-no-symlinks");
    ERROR(openat2_flags(dirfd, "sub/slink", O_RDONLY, 0, RESOLVE_NO_SYMLINKS), ELOOP,
          "openat2-no-symlinks-mid");
    fd = (int)openat2_flags(dirfd, "slink", O_RDONLY, 0, 0);
    check(fd >= 0, "openat2-symlink-default");
    (void)close(fd);
    mark("NO_SYMLINKS_ELOOP");

    /* fs/namei.c:1168-1174 rejects procfs magic links; fs/proc/fd.c:177-189
     * returns ENOENT for a closed descriptor before the magic link is
     * followed. */
    char procpath[64];
    check(snprintf(procpath, sizeof(procpath), "/proc/self/fd/%d", rofd) > 0,
          "openat2-proc-path");
    ERROR(openat2_flags(AT_FDCWD, procpath, O_RDONLY, 0, RESOLVE_NO_MAGICLINKS),
          ELOOP, "openat2-magiclink");
    ERROR(openat2_flags(AT_FDCWD, "/proc/self/fd/9999", O_RDONLY, 0,
                        RESOLVE_NO_MAGICLINKS),
          ENOENT, "openat2-magiclink-absent");
    mark("NO_MAGICLINKS_ELOOP");

    /* fs/namei.c:1132-1135 */
    ERROR(openat2_flags(dirfd, "/etc/passwd", O_RDONLY, 0, RESOLVE_BENEATH), EXDEV,
          "openat2-beneath-absolute");
    fd = (int)openat2_flags(dirfd, "file", O_RDONLY, 0, RESOLVE_BENEATH);
    check(fd >= 0, "openat2-beneath-relative");
    (void)close(fd);
    mark("BENEATH_ABSOLUTE_EXDEV");

    /* fs/namei.c:2718-2720,2771-2780: IN_ROOT rebases an absolute pathname on
     * the dirfd. */
    fd = (int)openat2_flags(dirfd, "/file", O_RDONLY, 0, RESOLVE_IN_ROOT);
    check(fd >= 0, "openat2-in-root-open");
    check(fstat(fd, &st) == 0, "openat2-in-root-stat");
    struct stat root_st;
    check(fstat(rofd, &root_st) == 0, "openat2-in-root-source");
    check(st.st_dev == root_st.st_dev && st.st_ino == root_st.st_ino,
          "openat2-in-root-identity");
    (void)close(fd);
    ERROR(openat2_flags(dirfd, "/etc/passwd", O_RDONLY, 0, RESOLVE_IN_ROOT), ENOENT,
          "openat2-in-root-escape");
    fd = (int)openat2_flags(AT_FDCWD, "/etc/passwd", O_RDONLY, 0, 0);
    check(fd >= 0, "openat2-absolute-default");
    (void)close(fd);
    mark("IN_ROOT_ABSOLUTE_SCOPED");

    done();
}

/* ==================================================================== */
/* name_to_handle_at(2) -- fs/fhandle.c:18-168                          */
/* ==================================================================== */
static void case_name_to_handle(void) {
    begin("name-to-handle-at.raw-differential");

    struct fd_lifecycle_handle handle;
    int mnt_id = 0;

    /* fs/fhandle.c:138-140. */
    ERROR(nr_name_to_handle_at(AT_FDCWD, root, &handle, &mnt_id, 0x4), EINVAL,
          "ntha-flag-4");
    ERROR(nr_name_to_handle_at(AT_FDCWD, root, &handle, &mnt_id, AT_SYMLINK_NOFOLLOW),
          EINVAL, "ntha-symlink-nofollow");
    ERROR(nr_name_to_handle_at(AT_FDCWD, root, &handle, &mnt_id, (int)0x80000000),
          EINVAL, "ntha-flag-sign");
    mark("FLAG_MASK_EINVAL");

    /* fs/fhandle.c:159-166: the name is copied and resolved before the handle
     * header is read at :43. */
    memset(&handle, 0, sizeof(handle));
    handle.handle_bytes = MAX_HANDLE_SZ;
    ERROR(nr_name_to_handle_at(AT_FDCWD, "/nonexistent-fd-lifecycle", NULL, NULL, 0),
          ENOENT, "ntha-path-before-handle");
    mark("PATH_BEFORE_HANDLE_EFAULT");

    ERROR(nr_name_to_handle_at(AT_FDCWD, BADPTR, &handle, &mnt_id, 0), EFAULT,
          "ntha-name-fault");
    mark("NAME_FAULT_EFAULT");

    ERROR(nr_name_to_handle_at(AT_FDCWD, root, NULL, &mnt_id, 0), EFAULT,
          "ntha-null-handle");
    handle.handle_bytes = MAX_HANDLE_SZ;
    ERROR(nr_name_to_handle_at(AT_FDCWD, root, &handle, NULL, 0), EFAULT,
          "ntha-null-mnt-id");
    mark("NULL_HANDLE_EFAULT");

    /* fs/fhandle.c:46-47 */
    memset(&handle, 0, sizeof(handle));
    handle.handle_bytes = MAX_HANDLE_SZ + 1;
    ERROR(nr_name_to_handle_at(AT_FDCWD, root, &handle, &mnt_id, 0), EINVAL,
          "ntha-over-max");
    memset(&handle, 0, sizeof(handle));
    handle.handle_bytes = MAX_HANDLE_SZ;
    check(nr_name_to_handle_at(AT_FDCWD, root, &handle, &mnt_id, 0) == 0,
          "ntha-max-accepted");
    mark("OVER_MAX_EINVAL");
    mark("MAX_SIZE_ACCEPTED");

    /* fs/fhandle.c:57-109: a too-small buffer reports the required size in
     * the fixed header with FILEID_INVALID, writes mount_id, and returns
     * EOVERFLOW. */
    struct fd_lifecycle_handle probe;
    memset(&probe, 0, sizeof(probe));
    probe.handle_bytes = 0;
    int probe_mnt = 0;
    ERROR(nr_name_to_handle_at(AT_FDCWD, root, &probe, &probe_mnt, 0), EOVERFLOW,
          "ntha-probe");
    check(probe.handle_bytes > 0 && probe.handle_bytes <= MAX_HANDLE_SZ,
          "ntha-probe-size");
    check(probe.handle_type == FILEID_INVALID, "ntha-probe-type");
    check(probe_mnt == mnt_id, "ntha-probe-mntid");
    struct fd_lifecycle_handle sized;
    memset(&sized, 0, sizeof(sized));
    sized.handle_bytes = probe.handle_bytes;
    int sized_mnt = 0;
    check(nr_name_to_handle_at(AT_FDCWD, root, &sized, &sized_mnt, 0) == 0,
          "ntha-sized");
    check(sized.handle_bytes > 0 && sized.handle_bytes <= probe.handle_bytes,
          "ntha-sized-bytes");
    check(sized.handle_type >= 0, "ntha-sized-type");
    check(sized_mnt == probe_mnt, "ntha-sized-mntid");
    /* fs/fhandle.c:95-104 writes mount_id before the header copy, so a faulting
     * mount_id beats the EOVERFLOW verdict. */
    memset(&probe, 0, sizeof(probe));
    probe.handle_bytes = 0;
    ERROR(nr_name_to_handle_at(AT_FDCWD, root, &probe, NULL, 0), EFAULT,
          "ntha-probe-mntid-first");
    mark("PROBE_EOVERFLOW_HEADER");

    /* fs/fhandle.c:95-104: (int __user *) without the flag, (u64 __user *) with
     * AT_HANDLE_MNT_ID_UNIQUE. */
    uint64_t wide = 0xdeadbeefdeadbeefULL;
    check(nr_name_to_handle_at(AT_FDCWD, root, &handle, &wide, 0) == 0,
          "ntha-wide-legacy");
    check((uint32_t)wide == (uint32_t)mnt_id, "ntha-wide-low");
    check((uint32_t)(wide >> 32) == 0xdeadbeefU, "ntha-wide-high-untouched");
    wide = 0xdeadbeefdeadbeefULL;
    check(nr_name_to_handle_at(AT_FDCWD, root, &handle, &wide, AT_HANDLE_MNT_ID_UNIQUE) == 0,
          "ntha-wide-unique");
    check((uint32_t)(wide >> 32) != 0xdeadbeefU, "ntha-wide-high-written");
    mark("MNT_ID_WRITE_WIDTH");

    /* fs/namei.c:233 + :202-206. */
    memset(&handle, 0, sizeof(handle));
    handle.handle_bytes = MAX_HANDLE_SZ;
    check(nr_name_to_handle_at(dirfd, "", &handle, &mnt_id, AT_EMPTY_PATH) == 0,
          "ntha-empty-path-flag");
    int emptyfd = (int)nr_open_by_handle_at(dirfd, &handle, O_RDONLY);
    check(emptyfd >= 0, "ntha-empty-path-open");
    struct stat empty_st, dir_st;
    check(fstat(emptyfd, &empty_st) == 0 && fstat(dirfd, &dir_st) == 0, "ntha-empty-stat");
    check(empty_st.st_dev == dir_st.st_dev && empty_st.st_ino == dir_st.st_ino,
          "ntha-empty-identity");
    (void)close(emptyfd);
    memset(&handle, 0, sizeof(handle));
    handle.handle_bytes = MAX_HANDLE_SZ;
    ERROR(nr_name_to_handle_at(dirfd, "", &handle, &mnt_id, 0), ENOENT,
          "ntha-empty-no-flag");
    mark("EMPTY_PATH_FLAG");

    /* fs/fhandle.c:158: without AT_SYMLINK_FOLLOW only the link itself is
     * encoded, so a dangling symlink still yields a handle. */
    check(symlinkat("absent", dirfd, "dangling") == 0 || errno == EEXIST, "ntha-dangling");
    memset(&handle, 0, sizeof(handle));
    handle.handle_bytes = MAX_HANDLE_SZ;
    check(nr_name_to_handle_at(dirfd, "dangling", &handle, &mnt_id, 0) == 0,
          "ntha-dangling-nofollow");
    memset(&handle, 0, sizeof(handle));
    handle.handle_bytes = MAX_HANDLE_SZ;
    ERROR(nr_name_to_handle_at(dirfd, "dangling", &handle, &mnt_id, AT_SYMLINK_FOLLOW),
          ENOENT, "ntha-dangling-follow");
    mark("DANGLING_SYMLINK_RULES");

    /* include/linux/exportfs.h:341-353 + mm/shmem.c:4961: procfs has no
     * export operations, so a plain handle is EOPNOTSUPP, while
     * AT_HANDLE_FID still uses the generic ino64 encoder. */
    memset(&handle, 0, sizeof(handle));
    handle.handle_bytes = MAX_HANDLE_SZ;
    ERROR(nr_name_to_handle_at(AT_FDCWD, "/proc/self/status", &handle, &mnt_id, 0),
          EOPNOTSUPP, "ntha-proc-plain");
    mark("UNSUPPORTED_FS_EOPNOTSUPP");

    struct fd_lifecycle_handle fid;
    memset(&fid, 0, sizeof(fid));
    fid.handle_bytes = MAX_HANDLE_SZ;
    check(nr_name_to_handle_at(AT_FDCWD, "/proc/self/status", &fid, &mnt_id, AT_HANDLE_FID) == 0,
          "ntha-proc-fid");
    mark("FID_HANDLE_ON_UNEXPORTABLE_FS");

    done();
}

/* ==================================================================== */
/* open_by_handle_at(2) -- fs/fhandle.c:170-462                         */
/* ==================================================================== */
static void case_open_by_handle(void) {
    begin("open-by-handle-at.raw-differential");

    struct fd_lifecycle_handle handle;
    int mnt_id = 0;

    /* fs/fhandle.c:359-360 copies the header before the mount fd is resolved
     * at :370-372, so a bad handle pointer beats a bad mount fd. */
    ERROR(nr_open_by_handle_at(-1, NULL, O_RDONLY), EFAULT, "obha-null-handle-bad-fd");
    memset(&handle, 0, sizeof(handle));
    handle.handle_bytes = 0;
    ERROR(nr_open_by_handle_at(-1, &handle, O_RDONLY), EINVAL, "obha-zero-bytes-bad-fd");
    handle.handle_bytes = MAX_HANDLE_SZ + 1;
    ERROR(nr_open_by_handle_at(-1, &handle, O_RDONLY), EINVAL, "obha-over-max-bad-fd");
    handle.handle_bytes = 1;
    handle.handle_type = -1;
    ERROR(nr_open_by_handle_at(-1, &handle, O_RDONLY), EINVAL, "obha-negative-type");
    handle.handle_type = FILEID_IS_DIR | FILEID_IS_CONNECTABLE | 0x40000;
    ERROR(nr_open_by_handle_at(-1, &handle, O_RDONLY), EINVAL, "obha-unknown-type-flags");
    mark("HEADER_BEFORE_FD_EFAULT");
    mark("ZERO_BYTES_EINVAL");
    mark("OVER_MAX_EINVAL");
    mark("NEGATIVE_TYPE_EINVAL");
    mark("UNKNOWN_TYPE_FLAGS_EINVAL");

    memset(&handle, 0, sizeof(handle));
    handle.handle_bytes = MAX_HANDLE_SZ;
    check(nr_name_to_handle_at(dirfd, "file", &handle, &mnt_id, 0) == 0, "obha-encode");
    check(handle.handle_bytes > 0, "obha-encode-size");

    /* fs/fhandle.c:370-372 get_path_anchor -> fd_empty -> EBADF. */
    ERROR(nr_open_by_handle_at(-1, &handle, O_RDONLY), EBADF, "obha-bad-fd");
    ERROR(nr_open_by_handle_at(-2, &handle, O_RDONLY), EBADF, "obha-negative-fd");
    /* fs/file.c:1206-1208: fdget() masks FMODE_PATH, so an O_PATH mount fd is
     * EBADF while any plain descriptor is accepted. */
    int pathfd = open(root, O_PATH | O_DIRECTORY);
    check(pathfd >= 0, "obha-path-fd");
    ERROR(nr_open_by_handle_at(pathfd, &handle, O_RDONLY), EBADF, "obha-path-mount-fd");
    (void)close(pathfd);
    mark("BAD_FD_EBADF");
    mark("O_PATH_MOUNT_FD_EBADF");

    /* fs/fhandle.c:387-394 copies the handle body separately from the header. */
    unsigned char *body = mmap(NULL, (size_t)4096 * 2, PROT_READ | PROT_WRITE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check(body != MAP_FAILED, "obha-body-mmap");
    check(munmap(body + 4096, 4096) == 0, "obha-body-munmap");
    /* Only the header plus the first 32 body bytes stay inside the live page,
     * so the 128-byte body copy crosses into the unmapped neighbour. */
    struct fd_lifecycle_handle *torn =
        (struct fd_lifecycle_handle *)(body + 4096 - sizeof(torn->handle_bytes) -
                                       sizeof(torn->handle_type) - 32);
    memset(torn, 0, sizeof(torn->handle_bytes) + sizeof(torn->handle_type) + 32);
    torn->handle_bytes = MAX_HANDLE_SZ; /* body runs into the unmapped page */
    torn->handle_type = handle.handle_type;
    ERROR(nr_open_by_handle_at(dirfd, torn, O_RDONLY), EFAULT, "obha-body-fault");
    check(munmap(body, 4096) == 0, "obha-body-cleanup");
    mark("BODY_FAULT_EFAULT");

    /* fs/fhandle.c:301-348: without CAP_DAC_READ_SEARCH may_decode_fh returns
     * EPERM after the header and mount fd are admitted and before the body is
     * decoded. */
    pid_t child = fork();
    if (child < 0) check(0, "obha-perm-fork");
    if (child == 0) {
        (void)setresgid(65534, 65534, 65534);
        (void)setresuid(65534, 65534, 65534);
        errno = 0;
        long r = nr_open_by_handle_at(dirfd, &handle, O_RDONLY);
        _exit(r == -1 && errno == EPERM ? 0 : 1);
    }
    check(wait_status(child) == 0, "obha-perm-child");
    mark("EPERM_WITHOUT_DAC_SEARCH");

    /* fs/fhandle.c:427-438 + fs/open.c:1368: success returns a new descriptor
     * with O_CLOEXEC honoured and the same inode as the encoded name. */
    int fd = (int)nr_open_by_handle_at(AT_FDCWD, &handle, O_RDONLY | O_CLOEXEC);
    check(fd >= 0, "obha-round-trip");
    struct stat decoded, original;
    check(fstat(fd, &decoded) == 0 && fstat(rofd, &original) == 0, "obha-stat");
    check(decoded.st_dev == original.st_dev && decoded.st_ino == original.st_ino,
          "obha-identity");
    check((fcntl(fd, F_GETFL) & O_ACCMODE) == O_RDONLY, "obha-access");
    check(fd_cloexec(fd), "obha-cloexec");
    (void)close(fd);
    mark("ROUND_TRIP_IDENTITY");
    mark("CLOEXEC_RESULT_FLAG");

    /* A directory handle decodes; fs/namei.c:4901-4907 rejects a symlink
     * handle for a non-O_PATH open with ELOOP. */
    struct fd_lifecycle_handle dhandle;
    memset(&dhandle, 0, sizeof(dhandle));
    dhandle.handle_bytes = MAX_HANDLE_SZ;
    check(nr_name_to_handle_at(dirfd, "sub", &dhandle, &mnt_id, 0) == 0, "obha-dir-encode");
    fd = (int)nr_open_by_handle_at(dirfd, &dhandle, O_RDONLY | O_DIRECTORY);
    check(fd >= 0, "obha-dir-open");
    (void)close(fd);

    /* fs/fhandle.c:401-408 turns the FILEID_IS_DIR user bit into
     * EXPORT_FH_DIR_ONLY and masks every user bit away before the filesystem
     * decoder runs; fs/exportfs/expfs.c:460-462 reports a directory-only
     * request for a non-directory as ENOTDIR, which fs/fhandle.c:278-282
     * normalizes to ESTALE. */
    struct fd_lifecycle_handle typed = handle;
    typed.handle_type = handle.handle_type | FILEID_IS_DIR;
    ERROR(nr_open_by_handle_at(dirfd, &typed, O_RDONLY), ESTALE, "obha-dir-only-file");
    mark("TYPE_DIR_ONLY_REJECTS_FILE");

    /* The same bit is satisfied by a directory handle and an unknown user bit
     * remains EINVAL (fs/fhandle.c:366-368,401-408). */
    struct fd_lifecycle_handle dtyped = dhandle;
    dtyped.handle_type = dhandle.handle_type | FILEID_IS_DIR;
    fd = (int)nr_open_by_handle_at(dirfd, &dtyped, O_RDONLY | O_DIRECTORY);
    check(fd >= 0, "obha-dir-user-bits");
    (void)close(fd);
    dtyped.handle_type = dhandle.handle_type | 0x00040000;
    ERROR(nr_open_by_handle_at(dirfd, &dtyped, O_RDONLY), EINVAL, "obha-unknown-user-bit");
    mark("TYPE_USER_FLAGS_MASKED");
    struct fd_lifecycle_handle lhandle;
    memset(&lhandle, 0, sizeof(lhandle));
    lhandle.handle_bytes = MAX_HANDLE_SZ;
    check(nr_name_to_handle_at(dirfd, "slink", &lhandle, &mnt_id, 0) == 0, "obha-link-encode");
    ERROR(nr_open_by_handle_at(dirfd, &lhandle, O_RDONLY), ELOOP, "obha-link-open");
    fd = (int)nr_open_by_handle_at(dirfd, &lhandle, O_PATH);
    check(fd >= 0, "obha-link-path-open");
    (void)close(fd);
    mark("SYMLINK_HANDLE_ELOOP");

    /* fs/exportfs/expfs.c:454-455 + fs/fhandle.c:278-282: a handle whose
     * inode is gone decodes to ESTALE. */
    struct fd_lifecycle_handle stale;
    memset(&stale, 0, sizeof(stale));
    stale.handle_bytes = MAX_HANDLE_SZ;
    int stale_fd = openat(dirfd, "stale", O_CREAT | O_RDWR | O_EXCL, 0600);
    check(stale_fd >= 0, "obha-stale-create");
    (void)close(stale_fd);
    check(nr_name_to_handle_at(dirfd, "stale", &stale, &mnt_id, 0) == 0, "obha-stale-encode");
    check(unlinkat(dirfd, "stale", 0) == 0, "obha-stale-unlink");
    ERROR(nr_open_by_handle_at(dirfd, &stale, O_RDONLY), ESTALE, "obha-stale-open");
    mark("ESTALE_AFTER_UNLINK");

    /* fdget() of a pipe descriptor is fine, but pipefs has no export
     * operations, so decode reports ESTALE (fs/exportfs/expfs.c:454-455). */
    int pipefd[2];
    check(pipe(pipefd) == 0, "obha-pipe");
    ERROR(nr_open_by_handle_at(pipefd[0], &handle, O_RDONLY), ESTALE, "obha-pipe-mount-fd");
    (void)close(pipefd[0]);
    (void)close(pipefd[1]);
    mark("UNEXPORTABLE_MOUNT_FD_ESTALE");

    done();
}

/* ==================================================================== */
/* pidfd_open(2) -- kernel/pid.c:697-716, kernel/fork.c:1890-1932       */
/* ==================================================================== */
struct thread_handoff {
    int report[2];
    int release[2];
};

static void *thread_main(void *argument) {
    struct thread_handoff *handoff = argument;
    pid_t tid = (pid_t)syscall(SYS_gettid);
    if (write(handoff->report[1], &tid, sizeof(tid)) != (ssize_t)sizeof(tid))
        return NULL;
    char byte = 0;
    (void)read(handoff->release[0], &byte, 1);
    return NULL;
}

static void case_pidfd_open(void) {
    begin("pidfd-open.raw-differential");

    /* kernel/pid.c:702-710: the flag mask precedes the pid<=0 test and the pid
     * lookup, so an unknown bit is EINVAL even for a pid that does not exist. */
    ERROR(nr_pidfd_open(0x7ffffff0, 0x10), EINVAL, "pidfd-flags-before-lookup");
    ERROR(nr_pidfd_open(0x7ffffff0, PIDFD_THREAD << 1), EINVAL, "pidfd-flags-shift");
    ERROR(nr_pidfd_open(0x7ffffff0, 4096), EINVAL, "pidfd-stale-bit");
    mark("FLAGS_BEFORE_LOOKUP");

    ERROR(nr_pidfd_open(0, 0), EINVAL, "pidfd-zero");
    ERROR(nr_pidfd_open(-1, 0), EINVAL, "pidfd-negative");
    ERROR(nr_pidfd_open(-10000, 0), EINVAL, "pidfd-self-thread");
    mark("NONPOSITIVE_EINVAL");

    ERROR(nr_pidfd_open(0x7ffffff0, 0), ESRCH, "pidfd-unknown");
    mark("UNKNOWN_PID_ESRCH");

    /* kernel/fork.c:1922 always allocates the pidfd with O_CLOEXEC; fs/pidfs.c
     * :932-944 leaves f_flags = O_RDWR | (O_NONBLOCK) | (O_EXCL). */
    int fd = (int)nr_pidfd_open(getpid(), 0);
    check(fd >= 0, "pidfd-self");
    check(fd_cloexec(fd), "pidfd-cloexec");
    check(fcntl(fd, F_GETFL) == O_RDWR, "pidfd-getfl");
    errno = 0;
    check(read(fd, &fd, 1) == -1 && errno == EINVAL, "pidfd-read");
    errno = 0;
    check(write(fd, "x", 1) == -1 && errno == EINVAL, "pidfd-write");
    (void)close(fd);
    mark("CLOEXEC_AND_RDWR_FLAGS");
    mark("READ_WRITE_EINVAL");

    fd = (int)nr_pidfd_open(getpid(), PIDFD_NONBLOCK);
    check(fd >= 0 && fcntl(fd, F_GETFL) == (O_RDWR | O_NONBLOCK), "pidfd-nonblock-flags");
    (void)close(fd);
    mark("NONBLOCK_F_GETFL");

    fd = (int)nr_pidfd_open(getpid(), PIDFD_THREAD);
    check(fd >= 0 && fcntl(fd, F_GETFL) == (O_RDWR | O_EXCL), "pidfd-thread-flags");
    (void)close(fd);
    fd = (int)nr_pidfd_open(getpid(), PIDFD_THREAD | PIDFD_NONBLOCK);
    check(fd >= 0 && fcntl(fd, F_GETFL) == (O_RDWR | O_EXCL | O_NONBLOCK),
          "pidfd-both-flags");
    (void)close(fd);
    mark("THREAD_F_GETFL");

    /* kernel/fork.c:1918-1919: a non-leader tid is ENOENT without PIDFD_THREAD
     * and admitted with it. */
    int report[2], release[2];
    check(pipe(report) == 0 && pipe(release) == 0, "pidfd-thread-pipes");
    struct thread_handoff handoff = { { report[0], report[1] },
                                      { release[0], release[1] } };
    pthread_t thread;
    check(pthread_create(&thread, NULL, thread_main, &handoff) == 0, "pidfd-pthread");
    pid_t tid = 0;
    check(read(report[0], &tid, sizeof(tid)) == (ssize_t)sizeof(tid), "pidfd-thread-tid");
    check(tid > 0 && tid != getpid(), "pidfd-thread-distinct");
    ERROR(nr_pidfd_open(tid, 0), ENOENT, "pidfd-nonleader");
    mark("NONLEADER_PIDFD_ENOENT");
    fd = (int)nr_pidfd_open(tid, PIDFD_THREAD);
    check(fd >= 0, "pidfd-nonleader-thread");
    (void)close(fd);
    mark("NONLEADER_THREAD_PIDFD_OK");
    check(write(release[1], "x", 1) == 1, "pidfd-thread-release");
    check(pthread_join(thread, NULL) == 0, "pidfd-thread-join");
    (void)close(report[0]);
    (void)close(report[1]);
    (void)close(release[0]);
    (void)close(release[1]);

    /* kernel/fork.c:1911 accepts an unreaped zombie; fs/pidfs.c:320-324 makes
     * its pidfd poll-readable.  release_task frees the number, after which
     * kernel/pid.c:708-710 answers ESRCH. */
    pid_t zombie = fork();
    if (zombie < 0) check(0, "pidfd-zombie-fork");
    if (zombie == 0) _exit(0);
    int zfd = (int)nr_pidfd_open(zombie, 0);
    check(zfd >= 0, "pidfd-zombie");
    struct pollfd watch = { .fd = zfd, .events = POLLIN, .revents = 0 };
    int ready = 0;
    for (int attempt = 0; attempt < 20000 && !ready; attempt++) {
        int r = poll(&watch, 1, 1);
        if (r > 0 && (watch.revents & POLLIN) != 0) ready = 1;
    }
    check(ready == 1, "pidfd-zombie-poll");
    mark("ZOMBIE_PIDFD_POLL_READY");
    (void)close(zfd);
    int status = 0;
    check(waitpid(zombie, &status, 0) == zombie, "pidfd-zombie-reap");
    ERROR(nr_pidfd_open(zombie, 0), ESRCH, "pidfd-reaped");
    mark("REAPED_PIDFD_ESRCH");

    done();
}

/* ==================================================================== */
/* pidfd_getfd(2) -- kernel/pid.c:881-976, fs/file.c:1385-1406          */
/* ==================================================================== */
static void case_pidfd_getfd(void) {
    begin("pidfd-getfd.raw-differential");

    int self = (int)nr_pidfd_open(getpid(), 0);
    check(self >= 0, "getfd-self-pidfd");
    check(filefd >= 0, "getfd-file");

    /* kernel/pid.c:964-969: the flag test runs before any descriptor lookup. */
    ERROR(nr_pidfd_getfd(-1, 0, 1), EINVAL, "getfd-flags-before-fd");
    ERROR(nr_pidfd_getfd(self, -1, 1), EINVAL, "getfd-flags-before-target");
    mark("FLAGS_BEFORE_FD_LOOKUP");

    /* fs/pidfs.c:706-711 returns EBADF for a descriptor that is not a pidfs
     * file; kernel/pid.c:967-969 returns EBADF for a closed descriptor. */
    ERROR(nr_pidfd_getfd(-1, 0, 0), EBADF, "getfd-closed");
    ERROR(nr_pidfd_getfd(filefd, 0, 0), EBADF, "getfd-not-a-pidfd");
    ERROR(nr_pidfd_getfd(-10000, 0, 0), EBADF, "getfd-self-thread-alias");
    mark("NON_PIDFD_EBADF");

    /* kernel/pid.c:927-936 + fs/file.c:1385-1406: the transfer duplicates the
     * same open file description under a fresh CLOEXEC descriptor. */
    int transferred = (int)nr_pidfd_getfd(self, filefd, 0);
    check(transferred >= 0, "getfd-self");
    check(transferred != filefd, "getfd-distinct");
    check(fd_cloexec(transferred), "getfd-cloexec");
    check(fcntl(transferred, F_GETFL) == fcntl(filefd, F_GETFL), "getfd-same-description");
    (void)close(transferred);
    mark("SELF_PIDFD_TRANSFER");
    mark("RESULT_CLOEXEC");
    mark("SHARED_FILE_DESCRIPTION");

    /* kernel/pid.c:895,912-915 */
    ERROR(nr_pidfd_getfd(self, 999, 0), EBADF, "getfd-absent-target");
    ERROR(nr_pidfd_getfd(self, -1, 0), EBADF, "getfd-negative-target");
    mark("TARGET_FD_CLOSED_EBADF");

    /* A child's descriptor table is reachable through its pidfd. */
    int report[2], release[2];
    check(pipe(report) == 0 && pipe(release) == 0, "getfd-child-pipes");
    pid_t child = fork();
    if (child < 0) check(0, "getfd-child-fork");
    if (child == 0) {
        int own = openat(dirfd, "file", O_RDONLY);
        if (own < 0 || write(report[1], &own, sizeof(own)) != (ssize_t)sizeof(own))
            _exit(2);
        char byte = 0;
        (void)read(release[0], &byte, 1);
        _exit(0);
    }
    int childfd = -1;
    check(read(report[0], &childfd, sizeof(childfd)) == (ssize_t)sizeof(childfd),
          "getfd-child-fd");
    check(childfd >= 0, "getfd-child-value");
    int childpidfd = (int)nr_pidfd_open(child, 0);
    check(childpidfd >= 0, "getfd-child-pidfd");
    int stolen = (int)nr_pidfd_getfd(childpidfd, childfd, 0);
    check(stolen >= 0, "getfd-child-transfer");
    check(fd_cloexec(stolen), "getfd-child-cloexec");
    struct stat stolen_st, file_st;
    check(fstat(stolen, &stolen_st) == 0 && fstat(rofd, &file_st) == 0, "getfd-child-stat");
    check(stolen_st.st_dev == file_st.st_dev && stolen_st.st_ino == file_st.st_ino,
          "getfd-child-identity");
    (void)close(stolen);
    (void)close(childpidfd);
    check(write(release[1], "x", 1) == 1, "getfd-child-release");
    check(wait_status(child) == 0, "getfd-child-wait");
    (void)close(report[0]);
    (void)close(report[1]);
    (void)close(release[0]);
    (void)close(release[1]);
    mark("CHILD_TRANSFER");

    /* kernel/ptrace.c:326-355,368-369: a foreign uid without CAP_SYS_PTRACE is
     * EPERM.  pidfd_getfd's ptrace check precedes the target-fd lookup. */
    child = fork();
    if (child < 0) check(0, "getfd-perm-fork");
    if (child == 0) {
        (void)setresgid(65534, 65534, 65534);
        (void)setresuid(65534, 65534, 65534);
        errno = 0;
        long r = nr_pidfd_getfd(self, filefd, 0);
        _exit(r == -1 && errno == EPERM ? 0 : 1);
    }
    check(wait_status(child) == 0, "getfd-perm-child");
    mark("FOREIGN_UID_EPERM");

    /* kernel/pid.c:892-893: a target that has exited is ESRCH before the
     * target descriptor is examined. */
    pid_t zombie = fork();
    if (zombie < 0) check(0, "getfd-zombie-fork");
    if (zombie == 0) _exit(0);
    int zfd = (int)nr_pidfd_open(zombie, 0);
    check(zfd >= 0, "getfd-zombie-pidfd");
    struct pollfd watch = { .fd = zfd, .events = POLLIN, .revents = 0 };
    int ready = 0;
    for (int attempt = 0; attempt < 20000 && !ready; attempt++) {
        int r = poll(&watch, 1, 1);
        if (r > 0 && (watch.revents & POLLIN) != 0) ready = 1;
    }
    check(ready == 1, "getfd-zombie-ready");
    ERROR(nr_pidfd_getfd(zfd, 0, 0), ESRCH, "getfd-zombie-target");
    (void)close(zfd);
    int status = 0;
    check(waitpid(zombie, &status, 0) == zombie, "getfd-zombie-reap");
    mark("ZOMBIE_TARGET_ESRCH");

    (void)close(self);
    done();
}

int main(int argc, char **argv) {
    if (argc >= 4 && strcmp(argv[1], "--exec-stage") == 0)
        return exec_stage(atoi(argv[2]), atoi(argv[3]));

    setvbuf(stdout, NULL, _IONBF, 0);
    setvbuf(stderr, NULL, _IONBF, 0);
    check(mkdtemp(root) != NULL, "root-mkdtemp");
    check(atexit(cleanup) == 0, "cleanup-register");
    dirfd = open(root, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    check(dirfd >= 0, "directory-open");
    filefd = openat(dirfd, "file", O_CREAT | O_EXCL | O_RDWR | O_CLOEXEC, 0600);
    check(filefd >= 0, "file-create");
    check(write(filefd, "payload\n", 8) == 8, "file-write");
    rofd = openat(dirfd, "file", O_RDONLY);
    check(rofd >= 0, "file-reopen");
    check(mkdirat(dirfd, "sub", 0700) == 0, "subdir");
    int inner = openat(dirfd, "sub/inner", O_CREAT | O_RDWR | O_CLOEXEC, 0600);
    check(inner >= 0, "subdir-file");
    (void)close(inner);
    check(symlinkat("sub", dirfd, "sub/slink") == 0, "subdir-symlink");

    case_close_range();
    case_openat2();
    case_name_to_handle();
    case_open_by_handle();
    case_pidfd_open();
    case_pidfd_getfd();

    puts("THEKERNEL_FD_LIFECYCLE_OK");
    return 0;
}
