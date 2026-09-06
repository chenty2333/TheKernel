#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/vfs.h>
#include <unistd.h>

enum { NR_ACCESS = 21, NR_FACCESSAT = 269, NR_FACCESSAT2 = 439,
       NR_FSTATAT = 262, NR_STATX = 332 };
#define BAD ((void *)(uintptr_t)1)
#define UNKNOWN 0x40000000U
#define BAD_FLAGS 0x80000000U
static const char *active;
static void check(int ok, const char *stage) {
    if (!ok) {
        fprintf(stderr, "THEKERNEL_STAT_ACCESS_FAIL %s %s errno=%d (%s)\n",
                active, stage, errno, strerror(errno));
        exit(1);
    }
}
#define ERROR(call, expected, stage) do { errno = 0; long r_ = (call); \
    check(r_ == -1 && errno == (expected), (stage)); } while (0)
static void begin(const char *name) {
    active = name; printf("THEKERNEL_ABI_CASE %s\n", name);
}
static void mark(const char *name) {
    printf("THEKERNEL_ABI_ASSERT %s %s pass\n", active, name);
}
static void done(void) { printf("THEKERNEL_ABI_RESULT %s pass\n", active); }
static long access_call(int nr, const void *path, unsigned mode, unsigned flags) {
    if (nr == NR_ACCESS) return syscall(nr, path, mode);
    return syscall(nr, AT_FDCWD, path, mode, flags);
}
static void access_case(int nr, const char *name) {
    begin(name);
    ERROR(access_call(nr, BAD, 8, 0), EINVAL, "mode-before-path");
    ERROR(access_call(nr, NULL, 8, 0), EINVAL, "mode-before-null");
    ERROR(access_call(nr, BAD, F_OK, 0), EFAULT, "bad-path");
    ERROR(access_call(nr, NULL, F_OK, 0), EFAULT, "null-path");
    ERROR(access_call(nr, "", F_OK, 0), ENOENT, "empty-path");
    mark("MODE_BEFORE_PATH");
    check(access_call(nr, "/", F_OK, 0) == 0, "exists");
    if (nr == NR_FACCESSAT2) {
        ERROR(access_call(nr, BAD, F_OK, BAD_FLAGS), EINVAL, "flags-before-path");
        check(access_call(nr, "", F_OK, AT_EMPTY_PATH) == 0, "empty-cwd");
    }
    mark("EXISTS_FLAGS"); done();
}
int main(void) {
    access_case(NR_ACCESS, "access.raw-differential");
    access_case(NR_FACCESSAT, "faccessat.raw-differential");
    access_case(NR_FACCESSAT2, "faccessat2.raw-differential");
    active = "stat.setup";
    int fd = open("/", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    check(fd >= 0, "open-root");
    struct stat reference, result;
    check(fstat(fd, &reference) == 0, "reference");
    begin("newfstatat.raw-differential");
    ERROR(syscall(NR_FSTATAT, fd, BAD, &result, BAD_FLAGS), EINVAL, "flags-before-bad-path");
    ERROR(syscall(NR_FSTATAT, fd, BAD, &result, 0), EFAULT, "bad-path");
    ERROR(syscall(NR_FSTATAT, fd, "", &result, 0), ENOENT, "empty-no-flag");
    ERROR(syscall(NR_FSTATAT, fd, NULL, &result, 0), EFAULT, "null-no-flag");
    mark("PATH_FLAG_ORDER");
    check(syscall(NR_FSTATAT, fd, NULL, &result, AT_EMPTY_PATH | BAD_FLAGS) == 0,
          "null-fd-shortcut");
    check(result.st_ino == reference.st_ino && result.st_dev == reference.st_dev &&
          result.st_mode == reference.st_mode, "null-identity");
    check(syscall(NR_FSTATAT, fd, "", &result, AT_EMPTY_PATH | BAD_FLAGS) == 0,
          "empty-fd-shortcut");
    check(result.st_ino == reference.st_ino && result.st_dev == reference.st_dev,
          "empty-identity");
    ERROR(syscall(NR_FSTATAT, -1, NULL, &result, AT_EMPTY_PATH), EBADF, "bad-empty-fd");
    mark("EMPTY_FD_IDENTITY");
    check(syscall(NR_FSTATAT, AT_FDCWD, "/", &result, AT_NO_AUTOMOUNT | 0x6000) == 0,
          "accepted-sync-flags");
    mark("NO_AUTOMOUNT_SYNC_FLAGS"); done();

    begin("statx.raw-differential");
    struct statx sx;
    _Static_assert(sizeof(sx) == 256, "native statx layout");
    check(syscall(NR_STATX, fd, "", AT_EMPTY_PATH, STATX_BASIC_STATS | UNKNOWN, &sx) == 0,
          "unknown-mask-ignored");
    check((sx.stx_mask & UNKNOWN) == 0 && sx.stx_ino == reference.st_ino &&
          sx.stx_mode == reference.st_mode, "unknown-mask-identity");
    ERROR(syscall(NR_STATX, fd, BAD, 0, 0x80000000U, &sx), EINVAL, "reserved-before-path");
    ERROR(syscall(NR_STATX, fd, BAD, 0x6000, STATX_BASIC_STATS, &sx), EINVAL,
          "sync-before-path");
    ERROR(syscall(NR_STATX, fd, BAD, BAD_FLAGS, STATX_BASIC_STATS, &sx), EINVAL,
          "flags-before-path");
    ERROR(syscall(NR_STATX, fd, BAD, 0, STATX_BASIC_STATS, &sx), EFAULT, "bad-path");
    mark("EXTENSIBLE_MASK_VALIDATION");
    check(syscall(NR_STATX, fd, NULL, AT_EMPTY_PATH | BAD_FLAGS,
                  STATX_BASIC_STATS, &sx) == 0 && sx.stx_ino == reference.st_ino,
          "null-fd-shortcut");
    check(syscall(NR_STATX, fd, "", AT_EMPTY_PATH | BAD_FLAGS,
                  STATX_BASIC_STATS, &sx) == 0 && sx.stx_ino == reference.st_ino,
          "empty-fd-shortcut");
    mark("EMPTY_FD_IDENTITY");
    memset(&sx, 0xa5, sizeof(sx));
    check(syscall(NR_STATX, AT_FDCWD, "/proc/self/status", 0,
                  STATX_BASIC_STATS | STATX_BTIME | STATX_DIOALIGN, &sx) == 0,
          "proc-provider-query");
    check((sx.stx_mask & (STATX_BTIME | STATX_DIOALIGN)) == 0 &&
          sx.stx_btime.tv_sec == 0 && sx.stx_btime.tv_nsec == 0 &&
          sx.stx_dio_mem_align == 0 && sx.stx_dio_offset_align == 0,
          "proc-does-not-advertise-unsupported-fields");
    mark("PROVIDER_OPTIONAL_FIELDS");
    char path[] = "/root/thekernel-statx-XXXXXX";
    int regular = mkstemp(path);
    check(regular >= 0, "ext4-create");
    check(unlink(path) == 0, "ext4-unlink");
    struct statfs fs;
    check(fstatfs(regular, &fs) == 0 && (unsigned long)fs.f_type == 0xef53,
          "ext4-fixture-provider");
    check(syscall(NR_STATX, regular, "", AT_EMPTY_PATH,
                  STATX_BASIC_STATS | STATX_BTIME | STATX_DIOALIGN, &sx) == 0,
          "ext4-provider-query");
    check((sx.stx_mask & (STATX_BTIME | STATX_DIOALIGN)) ==
          (STATX_BTIME | STATX_DIOALIGN) && sx.stx_btime.tv_sec > 0 &&
          sx.stx_btime.tv_nsec < 1000000000 &&
          sx.stx_dio_mem_align == 512 && sx.stx_dio_offset_align == 512,
          "ext4-backed-optional-fields");
    check(close(regular) == 0, "ext4-close");
    mark("EXT4_OPTIONAL_FIELDS"); done();
    check(close(fd) == 0, "close");
    puts("THEKERNEL_STAT_ACCESS_OK");
    return 0;
}
