#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/vfs.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

/* Native x86_64 syscall numbers, verified against Linux's syscall_64.tbl. */
enum { NR_PROTECT = 10, NR_UNMAP = 11, NR_BRK = 12, NR_MSYNC = 26,
       NR_MINCORE = 27, NR_MADVISE = 28, NR_CAPGET = 125, NR_CAPSET = 126,
       NR_READV = 310, NR_WRITEV = 311, NR_MEMFD_CREATE = 319,
       NR_PROCESS_MADVISE = 440, NR_PROCESS_MRELEASE = 448, NR_SEAL = 462 };
/* include/uapi/linux/fcntl.h:122-123. */
#define PIDFD_SELF_THREAD (-10000)
#define PIDFD_SELF_THREAD_GROUP (-10001)
/* linux/capability.h and include/uapi/linux/capability.h _LINUX_CAPABILITY_VERSION_3. */
#define CAP_VERSION_3 0x20080522U
#define CAP_IPC_LOCK 14
struct cap_header { uint32_t version; int pid; };
struct cap_data { uint32_t effective, permitted, inheritable; };
#ifndef MAP_DROPPABLE
#define MAP_DROPPABLE 0x08
#endif
/* include/uapi/linux/mman.h:16-18 and arch/x86/include/uapi/asm/mman.h. */
#ifndef MAP_SHARED_VALIDATE
#define MAP_SHARED_VALIDATE 0x03
#endif
#ifndef MAP_FIXED_NOREPLACE
#define MAP_FIXED_NOREPLACE 0x100000
#endif
#ifndef MAP_EXECUTABLE
#define MAP_EXECUTABLE 0x1000
#endif
#ifndef MAP_HUGETLB
#define MAP_HUGETLB 0x040000
#endif
#ifndef MADV_DONTNEED_LOCKED
#define MADV_DONTNEED_LOCKED 24
#endif
#ifndef MADV_COLLAPSE
#define MADV_COLLAPSE 25
#endif
#ifndef MADV_HWPOISON
#define MADV_HWPOISON 100
#endif
#ifndef MADV_SOFT_OFFLINE
#define MADV_SOFT_OFFLINE 101
#endif
#ifndef MADV_GUARD_INSTALL
#define MADV_GUARD_INSTALL 102
#endif
#ifndef MADV_DODUMP
#define MADV_DODUMP 17
#endif
#ifndef MADV_KEEPONFORK
#define MADV_KEEPONFORK 19
#endif
#ifndef MADV_PAGEOUT
#define MADV_PAGEOUT 21
#endif
/* Native x86_64 userfaultfd numbers and layouts from
   include/uapi/linux/userfaultfd.h: UFFD_API is 0xAA, and both ioctl numbers
   are _IOWR(0xAA, nr, size) for the structures below (24 and 32 bytes). */
enum { NR_USERFAULTFD = 323 };
#define UFFD_API_VALUE 0xAAULL
/* `#define UFFD_USER_MODE_ONLY 1` (include/uapi/linux/userfaultfd.h:384). */
#define UFFD_USER_MODE_ONLY 1
#define UFFDIO_REGISTER_MODE_MISSING 1ULL
#define UFFDIO_API_CMD 0xC018AA3FUL
#define UFFDIO_REGISTER_CMD 0xC020AA00UL
struct uffdio_range { uint64_t start, len; };
struct uffdio_api { uint64_t api, features, ioctls; };
struct uffdio_register { struct uffdio_range range; uint64_t mode, ioctls; };
/* include/uapi/linux/memfd.h and include/uapi/linux/fcntl.h. */
#ifndef MFD_ALLOW_SEALING
#define MFD_ALLOW_SEALING 0x0002U
#endif
#ifndef MFD_NOEXEC_SEAL
#define MFD_NOEXEC_SEAL 0x0008U
#endif
#ifndef MFD_EXEC
#define MFD_EXEC 0x0010U
#endif
#ifndef F_ADD_SEALS
#define F_ADD_SEALS 1033
#endif
#ifndef F_SEAL_SEAL
#define F_SEAL_SEAL 0x0001
#endif
#ifndef F_SEAL_EXEC
#define F_SEAL_EXEC 0x0020
#endif
#ifndef __NR_mbind
#define __NR_mbind 237
#endif
#define PAGE 4096UL
#define BAD ((void *)(uintptr_t)1)
static const char *active;
static void check(int ok, const char *stage) {
    if (!ok) {
        fprintf(stderr, "THEKERNEL_MM_CONTRACTS_FAIL %s %s errno=%d (%s)\n",
                active, stage, errno, strerror(errno));
        exit(1);
    }
}
#define ERROR(call, expected, stage) do { errno = 0; long r_ = (long)(call); \
    check(r_ == -1 && errno == (expected), (stage)); } while (0)
static void begin(const char *name) {
    active = name;
    printf("THEKERNEL_ABI_CASE %s\n", active);
}
static void mark(const char *name) {
    printf("THEKERNEL_ABI_ASSERT %s %s pass\n", active, name);
}
static void done(void) { printf("THEKERNEL_ABI_RESULT %s pass\n", active); }
static unsigned char *pages(size_t n) {
    void *p = mmap(NULL, n * PAGE, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check(p != MAP_FAILED, "mmap");
    return p;
}
static void reap(pid_t pid, int expected_signal) {
    int status;
    check(waitpid(pid, &status, 0) == pid, "wait");
    if (expected_signal) check(WIFSIGNALED(status) && WTERMSIG(status) == expected_signal,
                               "child-signal");
    else {
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 0)
            fprintf(stderr, "THEKERNEL_MM_CHILD_STATUS raw=%d exit=%d signal=%d\n",
                    status, WIFEXITED(status) ? WEXITSTATUS(status) : -1,
                    WIFSIGNALED(status) ? WTERMSIG(status) : 0);
        check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "child-exit");
    }
}
static void expect_write_fault(unsigned char *p) {
    fflush(NULL);
    pid_t pid = fork(); check(pid >= 0, "fork-fault");
    if (!pid) {
        struct rlimit limit = {0, 0};
        (void)setrlimit(RLIMIT_CORE, &limit);
        *(volatile unsigned char *)p = 99;
        _exit(2);
    }
    reap(pid, SIGSEGV);
}
static void protect_case(void) {
    begin("mprotect.raw-differential");
    unsigned char *p = pages(3);
    p[0] = 17; p[PAGE] = 18; p[2 * PAGE] = 19;
    check(syscall(NR_UNMAP, p + PAGE, PAGE) == 0, "make-hole");
    ERROR(syscall(NR_PROTECT, p, 3 * PAGE, PROT_READ), ENOMEM, "hole-prefix");
    expect_write_fault(p);
    p[2 * PAGE] = 20;
    check(p[0] == 17 && p[2 * PAGE] == 20, "prefix-only-state");
    mark("HOLE_COMMITS_PREFIX_ONLY");
    ERROR(syscall(NR_PROTECT, p + 1, PAGE, PROT_READ), EINVAL, "alignment");
    ERROR(syscall(NR_PROTECT, p, PAGE, 0x80000000U), EINVAL, "protection-bits");
    check(syscall(NR_PROTECT, p, PAGE, PROT_READ | PROT_WRITE) == 0, "restore-rw");
    p[0] = 21; check(p[0] == 21, "write-restored");
    check(syscall(NR_UNMAP, p, 3 * PAGE) == 0, "cleanup");
    mark("VALIDATION_RESTORE");
    p = pages(2); p[0] = 71; p[PAGE] = 72;
    check(mprotect(p, 2 * PAGE, PROT_NONE) == 0, "hide-before-fork");
    fflush(NULL);
    pid_t child = fork(); check(child >= 0, "fork-hidden");
    if (!child) {
        if (mprotect(p, 2 * PAGE, PROT_READ) != 0 || p[0] != 71 || p[PAGE] != 72) _exit(13);
        if (mprotect(p, PAGE, PROT_READ | PROT_WRITE) != 0) _exit(14);
        p[0] = 73;
        if (munmap(p, 2 * PAGE) != 0) _exit(15);
        _exit(0);
    }
    reap(child, 0);
    check(mprotect(p, 2 * PAGE, PROT_READ) == 0 && p[0] == 71 && p[PAGE] == 72,
          "hidden-fork-preserves-parent");
    check(mprotect(p, 2 * PAGE, PROT_NONE) == 0 && munmap(p, 2 * PAGE) == 0,
          "hidden-unmap");
    void *replacement = mmap(p, 2 * PAGE, PROT_READ | PROT_WRITE,
                              MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    check(replacement == p && p[0] == 0 && p[PAGE] == 0, "hidden-unmap-reuse");
    check(munmap(p, 2 * PAGE) == 0, "reuse-cleanup");
    mark("PROT_NONE_FORK_UNMAP_REUSE"); done();
}
static void unmap_case(void) {
    begin("munmap.raw-differential");
    unsigned char *p = pages(3), vec = 0;
    p[0] = 31; p[2 * PAGE] = 32;
    check(syscall(NR_UNMAP, p + PAGE, PAGE) == 0, "remove-middle");
    ERROR(syscall(NR_MINCORE, p + PAGE, PAGE, &vec), ENOMEM, "hole-observed");
    check(p[0] == 31 && p[2 * PAGE] == 32, "neighbors");
    check(syscall(NR_UNMAP, p + PAGE, PAGE) == 0, "repeat-hole");
    mark("HOLE_NEIGHBORS_IDEMPOTENT");
    ERROR(syscall(NR_UNMAP, p + 1, PAGE), EINVAL, "alignment");
    ERROR(syscall(NR_UNMAP, p, 0), EINVAL, "zero-length");
    check(p[0] == 31 && p[2 * PAGE] == 32, "errors-preserve");
    check(syscall(NR_UNMAP, p, 3 * PAGE) == 0, "cleanup");
    mark("VALIDATION_PRESERVES_NEIGHBORS"); done();
}
static void mincore_case(void) {
    begin("mincore.raw-differential");
    unsigned char *p = pages(2), vec[4] = {0xa5, 0, 0, 0xa5};
    p[0] = 41; p[PAGE] = 42;
    check(syscall(NR_MINCORE, p, 2 * PAGE, vec + 1) == 0, "resident-query");
    check((vec[1] & 1) && (vec[2] & 1) && vec[0] == 0xa5 && vec[3] == 0xa5,
          "resident-boundaries");
    check(mprotect(p, 2 * PAGE, PROT_NONE) == 0, "hide-resident");
    vec[1] = vec[2] = 0;
    check(syscall(NR_MINCORE, p, 2 * PAGE, vec + 1) == 0 && (vec[1] & 1) && (vec[2] & 1),
          "prot-none-still-resident");
    check(mprotect(p, 2 * PAGE, PROT_READ | PROT_WRITE) == 0 && p[0] == 41 && p[PAGE] == 42,
          "restore-resident-content");
    mark("TOUCHED_RESIDENCY_EXACT_OUTPUT");
    check(syscall(NR_MINCORE, p, 0, NULL) == 0, "zero-length");
    ERROR(syscall(NR_MINCORE, p + 1, PAGE, vec), EINVAL, "alignment");
    ERROR(syscall(NR_MINCORE, p, PAGE, BAD), EFAULT, "bad-output");
    check(syscall(NR_UNMAP, p, 2 * PAGE) == 0, "unmap");
    ERROR(syscall(NR_MINCORE, p, PAGE, vec), ENOMEM, "unmapped");
    mark("VALIDATION_ORDER");
    char path[] = "/root/thekernel-mincore-XXXXXX";
    int fd = mkstemp(path); check(fd >= 0, "file-create");
    check(unlink(path) == 0, "file-unlink");
    struct statfs fixture;
    check(fstatfs(fd, &fixture) == 0 && fixture.f_type == 0xef53, "ext4-fixture-required");
    unsigned char data[PAGE] = {51};
    check(write(fd, data, PAGE) == PAGE && fsync(fd) == 0, "file-write");
    p = mmap(NULL, PAGE, PROT_READ, MAP_SHARED, fd, 0);
    check(p != MAP_FAILED && *(volatile unsigned char *)p == 51, "file-map-touch");
    check(syscall(NR_MINCORE, p, PAGE, vec) == 0 && (vec[0] & 1), "file-resident");
    check(munmap(p, PAGE) == 0 && close(fd) == 0, "file-cleanup");
    mark("FILE_PAGE_RESIDENCY");
    char shared_path[] = "/root/thekernel-redirty-XXXXXX";
    fd = mkstemp(shared_path); check(fd >= 0, "redirty-create");
    check(ftruncate(fd, PAGE) == 0, "redirty-size");
    int direct = open(shared_path, O_RDONLY | O_DIRECT);
    check(direct >= 0 && unlink(shared_path) == 0, "redirty-direct-reader");
    void *readback = NULL;
    check(posix_memalign(&readback, PAGE, PAGE) == 0, "redirty-aligned-buffer");
    p = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    check(p != MAP_FAILED, "redirty-shared-map");
    /* Keep the same writable mapping alive across both writeback rounds.
       Direct I/O prevents a clean-but-stale cache page hiding lost writeback. */
    for (unsigned method = 0; method < 2; ++method) {
        for (unsigned round = 0; round < 2; ++round) {
            unsigned char value = 'A' + method * 2 + round;
            memset(p, value, PAGE);
            check((method ? fsync(fd) : msync(p, PAGE, MS_SYNC)) == 0,
                  "redirty-writeback");
        }
        /* Only read directly after B: an intervening direct read could revoke
           aliases itself and accidentally repair the dirty-tracking defect. */
        check(pread(direct, readback, PAGE, 0) == PAGE, "redirty-direct-read");
        for (size_t i = 0; i < PAGE; ++i)
            check(((unsigned char *)readback)[i] == 'B' + method * 2,
                  "redirty-persisted-content");
    }
    mark("SHARED_MSYNC_FSYNC_REDIRTY");
    check(mlock(p, PAGE) == 0, "redirty-mlock");
    memset(p, 'E', PAGE);
    check(fsync(fd) == 0, "locked-shared-fsync");
    check(mincore(p, PAGE, vec) == 0 && (vec[0] & 1), "locked-writeback-resident");
    check(munlock(p, PAGE) == 0, "redirty-munlock");
    check(pread(direct, readback, PAGE, 0) == PAGE, "locked-shared-direct-read");
    for (size_t i = 0; i < PAGE; ++i)
        check(((unsigned char *)readback)[i] == 'E', "locked-shared-persisted-content");
    check(munmap(p, PAGE) == 0 && close(direct) == 0 && close(fd) == 0,
          "redirty-cleanup");
    free(readback);
    mark("LOCKED_SHARED_FSYNC");
    done();
}
static void vm_case(int nr, const char *name) {
    begin(name);
    unsigned char *remote = pages(2), *local = pages(2);
    memset(remote, 'A', PAGE); memset(remote + PAGE, 'B', PAGE);
    int ready[2], finish[2];
    check(pipe(ready) == 0 && pipe(finish) == 0, "pipes");
    fflush(NULL);
    pid_t pid = fork(); check(pid >= 0, "fork-target");
    if (!pid) {
        close(ready[0]); close(finish[1]);
        if (mprotect(remote + PAGE, PAGE, PROT_NONE) != 0) _exit(3);
        if (write(ready[1], "r", 1) != 1) _exit(4);
        char byte;
        if (read(finish[0], &byte, 1) != 1 || byte != 'e') _exit(5);
        for (size_t i = 0; i < PAGE; ++i) {
            if (remote[i] != (nr == NR_WRITEV ? 'C' : 'A')) {
                fprintf(stderr, "THEKERNEL_MM_REMOTE_EXACT offset=%zu got=%u expected=%u\n",
                        i, remote[i], nr == NR_WRITEV ? 'C' : 'A');
                _exit(10);
            }
        }
        if (write(ready[1], "e", 1) != 1) _exit(11);
        if (read(finish[0], &byte, 1) != 1 || byte != 'f') _exit(5);
        if (mprotect(remote + PAGE, PAGE, PROT_READ) != 0) {
            fprintf(stderr, "THEKERNEL_MM_REMOTE_RESTORE errno=%d\n", errno);
            _exit(6);
        }
        for (size_t i = 0; i < PAGE; ++i)
            if (remote[i] != (nr == NR_WRITEV ? 'D' : 'A') || remote[PAGE + i] != 'B') {
                fprintf(stderr, "THEKERNEL_MM_REMOTE_CONTENT offset=%zu first=%u second=%u expected-first=%u\n",
                        i, remote[i], remote[PAGE + i], nr == NR_WRITEV ? 'D' : 'A');
                _exit(7);
            }
        if (mprotect(remote + PAGE, PAGE, PROT_READ | PROT_WRITE) != 0) _exit(12);
        remote[PAGE] = 'Z';
        _exit(0);
    }
    close(ready[1]); close(finish[0]);
    char byte;
    check(read(ready[0], &byte, 1) == 1, "target-ready");
    memset(local, nr == NR_WRITEV ? 'C' : 0x5a, 2 * PAGE);
    struct iovec liov = {local, PAGE}, riov = {remote, PAGE};
    check(syscall(nr, pid, &liov, 1, &riov, 1, 0) == PAGE, "exact-copy");
    if (nr == NR_READV)
        for (size_t i = 0; i < PAGE; ++i) check(local[i] == 'A', "exact-read-content");
    check(write(finish[1], "e", 1) == 1 && read(ready[0], &byte, 1) == 1 && byte == 'e',
          "exact-remote-content-confirmed");
    mark("EXACT_COPY");
    memset(local, nr == NR_WRITEV ? 'D' : 0x5a, 2 * PAGE);
    liov.iov_len = riov.iov_len = 2 * PAGE;
    check(syscall(nr, pid, &liov, 1, &riov, 1, 0) == PAGE, "fault-prefix-count");
    if (nr == NR_READV)
        for (size_t i = 0; i < PAGE; ++i)
            check(local[i] == 'A' && local[PAGE + i] == 0x5a, "prefix-content-tail");
    mark("REMOTE_FAULT_PREFIX");
    riov.iov_base = BAD; riov.iov_len = 1; liov.iov_len = 1;
    ERROR(syscall(nr, pid, &liov, 1, &riov, 1, 0), EFAULT, "first-byte-fault");
    ERROR(syscall(nr, -1, BAD, 1, BAD, 1, 1), EINVAL, "flags-first");
    ERROR(syscall(nr, pid, BAD, 1025, &riov, 1, 0), EINVAL, "iov-count");
    ERROR(syscall(nr, pid, BAD, 1, &riov, 1, 0), EFAULT, "descriptor-fault");
    check(syscall(nr, -1, BAD, 0, BAD, 1025, 0) == 0, "empty-local-short-circuit");
    mark("VALIDATION_EMPTY_LOCAL");
    check(write(finish[1], "f", 1) == 1, "release-target");
    reap(pid, 0);
    for (size_t i = 0; i < PAGE; ++i)
        check(remote[i] == 'A' && remote[PAGE + i] == 'B', "parent-cow-content-preserved");
    mark("REMOTE_CONTENT_CONFIRMED");
    check(close(ready[0]) == 0 && close(finish[1]) == 0, "pipe-close");
    /* The caller must lack CAP_SYS_PTRACE: guest root drops UID in the
     * child; an ordinary host user already lacks that capability. */
    int dumpable = prctl(PR_GET_DUMPABLE);
    check(dumpable == 0 || dumpable == 1, "dumpable-state");
    check(prctl(PR_SET_DUMPABLE, 0) == 0, "deny-ptrace");
    pid_t parent = getpid();
    fflush(NULL);
    pid = fork(); check(pid >= 0, "fork-denied-caller");
    if (!pid) {
        if (getuid() == 0 && setuid(65534) != 0) _exit(8);
        struct iovec denied_local = {local, 1}, denied_remote = {remote, 1};
        errno = 0;
        long result = syscall(nr, parent, &denied_local, 1, &denied_remote, 1, 0);
        _exit(result == -1 && errno == EPERM ? 0 : 9);
    }
    reap(pid, 0);
    check(prctl(PR_SET_DUMPABLE, dumpable) == 0, "restore-dumpable");
    mark("PERMISSION_EPERM");
    check(munmap(remote, 2 * PAGE) == 0 && munmap(local, 2 * PAGE) == 0, "cleanup");
    done();
}
static void seal_case(void) {
    begin("mseal.raw-differential");
    fflush(NULL);
    pid_t pid = fork(); check(pid >= 0, "fork-seal");
    if (!pid) {
        unsigned char *p = pages(1), *ro = pages(1), *target = pages(1);
        p[0] = 61; ro[0] = 62;
        ERROR(syscall(NR_SEAL, p, PAGE, 1), EINVAL, "flags");
        ERROR(syscall(NR_SEAL, p + 1, PAGE, 0), EINVAL, "alignment");
        check(syscall(NR_SEAL, p, PAGE, 0) == 0, "seal-rw");
        p[0] = 63;
        ERROR(syscall(NR_UNMAP, p, PAGE), EPERM, "sealed-unmap");
        ERROR(syscall(NR_PROTECT, p, PAGE, PROT_READ), EPERM, "sealed-protect");
        ERROR(mremap(p, PAGE, PAGE, MREMAP_MAYMOVE | MREMAP_FIXED, target), EPERM, "sealed-remap");
        check(p[0] == 63, "rw-preserved");
        check(mprotect(ro, PAGE, PROT_READ) == 0 && syscall(NR_SEAL, ro, PAGE, 0) == 0,
              "seal-ro");
        ERROR(madvise(ro, PAGE, MADV_DONTNEED), EPERM, "sealed-ro-discard");
        check(ro[0] == 62, "ro-preserved");
        /* Linux permits discard on sealed writable anonymous mappings. */
        check(madvise(p, PAGE, MADV_DONTNEED) == 0 && p[0] == 0, "sealed-rw-discard");
        _exit(0); /* Sealed mappings intentionally survive until mm teardown. */
    }
    reap(pid, 0);
    mark("VALIDATION_AND_MAPPING_SEAL");
    mark("DISCARD_RESPECTS_WRITE_PERMISSION"); done();
}
static void lock_prefix_case(void) {
    const int calls[] = {149, 325, 150};
    const char *names[] = {"mlock.raw-differential", "mlock2.raw-differential",
                           "munlock.raw-differential"};
    for (unsigned n = 0; n < 3; ++n) {
        begin(names[n]);
        unsigned char *p = pages(3);
        p[0] = 11; p[2 * PAGE] = 22;
        if (calls[n] == 150)
            check(syscall(149, p, 3 * PAGE) == 0, "prepare-locked");
        check(munmap(p + PAGE, PAGE) == 0, "make-hole");
        ERROR(syscall(calls[n], p, 3 * PAGE, calls[n] == 325 ? 1 : 0),
              ENOMEM, "hole-error");
        if (calls[n] == 150) {
            check(madvise(p, PAGE, MADV_DONTNEED) == 0 && p[0] == 0,
                  "prefix-unlocked");
            ERROR(madvise(p + 2 * PAGE, PAGE, MADV_DONTNEED), EINVAL,
                  "suffix-still-locked");
        } else {
            ERROR(madvise(p, PAGE, MADV_DONTNEED), EINVAL, "prefix-locked");
            check(p[0] == 11, "locked-data-preserved");
            check(madvise(p + 2 * PAGE, PAGE, MADV_DONTNEED) == 0 &&
                  p[2 * PAGE] == 0, "suffix-still-unlocked");
        }
        check(munmap(p, PAGE) == 0 && munmap(p + 2 * PAGE, PAGE) == 0, "cleanup");
        mark("HOLE_COMMITS_PREFIX"); done();
    }
}
static void process_advice_case(void) {
    begin("process-madvise.raw-differential");
    int fd = syscall(434, getpid(), 0);
    check(fd >= 0, "self-pidfd");
    unsigned char *p = pages(2);
    struct iovec iov = { p, PAGE };
    p[0] = 71; p[PAGE] = 72;
    check(syscall(440, fd, &iov, 1, MADV_DONTNEED, 0) == PAGE,
          "self-destructive-advice");
    check(p[0] == 0 && p[PAGE] == 72, "self-discard-exact-range");
    int sync[2]; check(pipe(sync) == 0, "remote-sync");
    fflush(NULL);
    pid_t child = fork(); check(child >= 0, "remote-fork");
    if (!child) {
        close(sync[1]); char byte;
        if (read(sync[0], &byte, 1) != 1 || p[PAGE] != 72) _exit(1);
        _exit(0);
    }
    close(sync[0]);
    int remote = syscall(434, child, 0); check(remote >= 0, "remote-pidfd");
    ERROR(syscall(440, remote, &iov, 1, MADV_DONTNEED, 0), EINVAL,
          "foreign-destructive-advice-rejected");
    check(write(sync[1], "x", 1) == 1, "release-remote");
    check(close(sync[1]) == 0 && close(remote) == 0, "remote-cleanup");
    reap(child, 0);
    ERROR(syscall(440, fd, &iov, 1, 0xffffffffU, 0), EINVAL, "invalid-advice");
    ERROR(syscall(440, fd, &iov, 1, MADV_DONTNEED, 1), EINVAL, "invalid-flags");
    /* kernel/pid.c:pidfd_get_task() resolves the two PIDFD_SELF_*
       identifiers (include/uapi/linux/fcntl.h:122-123) before any descriptor
       lookup, so both name the calling task instead of failing the lookup. */
    check(syscall(440, PIDFD_SELF_THREAD, &iov, 1, MADV_DONTNEED, 0) == PAGE,
          "self-thread-identifier");
    check(syscall(440, PIDFD_SELF_THREAD_GROUP, &iov, 1, MADV_DONTNEED, 0) ==
              PAGE,
          "self-thread-group-identifier");
    mark("PIDFD_SELF_IDENTIFIERS");
    /* A valid descriptor that is not a pidfd is -EBADF from pidfd_pid()
       (fs/pidfs.c:706-711), not -EINVAL from the identifier switch. */
    int plain[2];
    check(pipe(plain) == 0, "non-pidfd-pipe");
    ERROR(syscall(440, plain[0], &iov, 1, MADV_DONTNEED, 0), EBADF,
          "non-pidfd-descriptor-ebadf");
    check(close(plain[0]) == 0 && close(plain[1]) == 0, "non-pidfd-cleanup");
    mark("NON_PIDFD_EBADF");
    check(munmap(p, 2 * PAGE) == 0 && close(fd) == 0, "cleanup");
    mark("SELF_DESTRUCTIVE_ADVICE"); done();
}
static void lockall_case(void) {
    begin("mlockall.raw-differential");
    fflush(NULL);
    pid_t pid = fork(); check(pid >= 0, "fork-lockall");
    if (!pid) {
        int fd = syscall(319, "thekernel-mlockall", 0);
        check(fd >= 0, "empty-memfd");
        unsigned char *p = mmap(NULL, PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
        check(p != MAP_FAILED, "map-beyond-eof");
        /* The mapping cannot be populated (SIGBUS on access). Linux still
           commits MCL_CURRENT and ignores this per-VMA population failure. */
        check(syscall(151, MCL_CURRENT) == 0, "ignore-populate-failure");
        check(syscall(152) == 0, "unlock-all");
        check(munmap(p, PAGE) == 0 && close(fd) == 0, "cleanup");
        _exit(0);
    }
    reap(pid, 0);
    mark("POPULATE_FAILURE_IGNORED"); done();
}
/* Returns -1 when the sysctl is unreadable, so every scope-dependent
   expectation below is gated on an observed value instead of a guess. */
static int memfd_noexec_scope(void) {
    int fd = open("/proc/sys/vm/memfd_noexec", O_RDONLY);
    if (fd < 0) return -1;
    char buf[8] = {0};
    ssize_t count = read(fd, buf, sizeof(buf) - 1);
    check(close(fd) == 0, "memfd-sysctl-close");
    if (count <= 0 || buf[0] < '0' || buf[0] > '2') return -1;
    return buf[0] - '0';
}

static void memfd_case(void) {
    active = "mm-abi-extras";
    /* include/uapi/linux/fcntl.h: the seal set is 0x3f and F_SEAL_EXEC is
       0x20; a memfd is created with F_SEAL_SEAL already set unless
       MFD_ALLOW_SEALING (or MFD_NOEXEC_SEAL) clears it. */
    int sealable = (int)syscall(NR_MEMFD_CREATE, "thekernel-sealable",
                                MFD_ALLOW_SEALING);
    check(sealable >= 0, "memfd-sealable-create");
    check(fcntl(sealable, F_ADD_SEALS, F_SEAL_SEAL) == 0, "memfd-sealable-add");
    check(close(sealable) == 0, "memfd-sealable-close");
    int fixed = (int)syscall(NR_MEMFD_CREATE, "thekernel-unsealable", 0);
    check(fixed >= 0, "memfd-unsealable-create");
    ERROR(fcntl(fixed, F_ADD_SEALS, F_SEAL_SEAL), EPERM, "memfd-unsealable-add");
    check(close(fixed) == 0, "memfd-unsealable-close");
    /* Unknown flag bits are outside MFD_ALL_FLAGS for a non-hugetlb memfd. */
    ERROR(syscall(NR_MEMFD_CREATE, "thekernel-bad-flag", 0x20U), EINVAL,
          "memfd-unknown-flag");
    /* mm/memfd.c:sanitize_flags() rejects the two exec bits together. */
    ERROR(syscall(NR_MEMFD_CREATE, "thekernel-both-exec",
                  MFD_NOEXEC_SEAL | MFD_EXEC), EINVAL, "memfd-both-exec-bits");
    /* MFD_NOEXEC_SEAL clears the execute bits of the 0777 shmem inode and
       adds F_SEAL_EXEC, which makes shmem_setattr() reject any chmod that
       would change them.  MFD_EXEC keeps 0777. */
    int noexec = (int)syscall(NR_MEMFD_CREATE, "thekernel-noexec",
                              MFD_NOEXEC_SEAL);
    check(noexec >= 0, "memfd-noexec-create");
    struct stat status;
    check(fstat(noexec, &status) == 0 && (status.st_mode & 0777) == 0666,
          "memfd-noexec-mode");
    ERROR(fchmod(noexec, 0755), EPERM, "memfd-noexec-chmod-exec");
    check(fchmod(noexec, 0666) == 0, "memfd-noexec-chmod-same-mode");
    check(close(noexec) == 0, "memfd-noexec-close");
    if (memfd_noexec_scope() == 0) {
        /* MFD_ALLOW_SEALING is required to add a seal later: without it the
           memfd is created with F_SEAL_SEAL already set. */
        int executable = (int)syscall(NR_MEMFD_CREATE, "thekernel-exec",
                                      MFD_EXEC | MFD_ALLOW_SEALING);
        check(executable >= 0, "memfd-exec-create");
        check(fstat(executable, &status) == 0 && (status.st_mode & 0777) == 0777,
              "memfd-exec-mode");
        /* mm/memfd.c:memfd_add_seals() lets F_SEAL_EXEC be added to an
           executable memfd; the seal then freezes the execute bits. */
        check(fcntl(executable, F_ADD_SEALS, F_SEAL_EXEC) == 0,
              "memfd-exec-add-seal");
        ERROR(fchmod(executable, 0666), EPERM, "memfd-exec-sealed-chmod");
        check(fchmod(executable, 0777) == 0, "memfd-exec-sealed-same-mode");
        check(close(executable) == 0, "memfd-exec-close");
    }
}

/* A mapping larger than the machine: Linux admits it under the default
   heuristic overcommit policy only because MAP_NORESERVE sets VM_NORESERVE,
   which removes it from the accountable set (`mm/vma.c:accountable_mapping()`
   and `do_mmap()`'s `sysctl_overcommit_memory != OVERCOMMIT_NEVER` gate). */
static void noreserve_case(void) {
    active = "mm-abi-extras";
    const size_t huge = (size_t)8 << 30;
    void *sparse = mmap(NULL, huge, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    check(sparse != MAP_FAILED, "noreserve-admitted");
    check(munmap(sparse, huge) == 0, "noreserve-cleanup");
}

/* ------------------------------------------------- brk(12) and mmap(9) --- */

/* Linux reports a refused brk() by returning the unchanged break, never -1
   (mm/mmap.c:181-192 `goto out` restores mm->brk). */
static long brk_at(uintptr_t addr) {
    errno = 0;
    return (long)(uintptr_t)syscall(NR_BRK, (void *)addr);
}

/* The child exits with the errno it observed, or 0 when the call succeeded. */
static void expect_child_errno(pid_t pid, int expected, const char *stage) {
    int status;
    check(waitpid(pid, &status, 0) == pid, "wait");
    check(WIFEXITED(status) && WEXITSTATUS(status) == expected, stage);
}

static void brk_case(void) {
    begin("brk.raw-differential");
    uintptr_t start = (uintptr_t)sbrk(0);
    check(start != (uintptr_t)-1, "brk-sbrk");
    /* Round the break up so this program owns a private 1 MiB window; the brk
       VMA is the highest mapping, so nothing of ours is above it. */
    uintptr_t base = (start + 0x1fffffUL) & ~0xfffffUL;
    check(brk_at(base + 0x40000) == (long)(base + 0x40000), "brk-align");
    mark("BREAK_ALIGNMENT");
    /* Carve a hole at the top page of the heap. */
    check(munmap((void *)(base + 0x3f000), PAGE) == 0, "brk-hole-carve");
    /* A shrink whose range contains no VMA at all: vma_find() finds nothing
       and the `goto out` path keeps mm->brk (mm/mmap.c:186-192). */
    check(brk_at(base + 0x3f000) == (long)(base + 0x40000),
          "brk-shrink-into-hole-keeps-break");
    mark("SHRINK_KEEPS_BREAK");
    /* Growth whose previous VMA does not end at the break has to allocate a
       fresh anonymous VMA instead of refusing (mm/mmap.c:1845-1900). */
    check(brk_at(base + 0x50000) == (long)(base + 0x50000),
          "brk-grow-into-hole-accepted");
    volatile unsigned char *fresh = (volatile unsigned char *)(base + 0x4f000);
    *fresh = 0x5a;
    check(*fresh == 0x5a, "brk-grow-into-hole-usable");
    mark("GROW_INTO_HOLE");

    /* A MAP_GROWSDOWN VMA owns stack_guard_gap (256 pages, mm/mmap.c:940)
       bytes below its start, so brk() must stay one page short of it. */
    uintptr_t guard = 0;
    for (uintptr_t off = 0x100000; off <= 0x4000000; off += 0x100000) {
        void *want = (void *)(base + 0x50000 + off);
        void *got = mmap(want, PAGE, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS | MAP_GROWSDOWN |
                             MAP_FIXED_NOREPLACE, -1, 0);
        if (got == want) {
            guard = (uintptr_t)got;
            break;
        }
    }
    /* The scan walks upward until it finds a free page, so failing to place
       the neighbour means the guest cannot map anything above the heap at
       all and the boundary below has nothing to measure.  A registered case
       must not skip its records silently. */
    check(guard != 0, "brk-guard-vma-placed");
    uintptr_t before = (uintptr_t)brk_at(0);

    long boundary = -1;
    for (long k = 2; k <= 300; k++) {
        long got = brk_at(guard - (uintptr_t)k * PAGE);
        if (got != (long)before) {
            boundary = k;
            break;
        }
        before = (uintptr_t)got;
    }
    check(boundary == 257, "brk-guard-gap-boundary");
    uintptr_t settled = (uintptr_t)brk_at(0);
    check(brk_at(guard - PAGE) == (long)settled, "brk-guard-refuses-one-page");
    check((uintptr_t)brk_at(0) == settled, "brk-guard-break-unchanged");
    mark("GUARD_GAP_BOUNDARY");
    done();
}

/* Forked child: drop CAP_IPC_LOCK, set RLIMIT_MEMLOCK and try MAP_LOCKED.
   The address is pinned with MAP_FIXED so the kernel's own address search --
   which on x86_64 can answer first with -ENOMEM -- cannot mask the lock
   admission answer that do_mmap() computes at mm/mmap.c:407-422. */
static int child_locked_errno(unsigned long len, rlim_t soft, int fixed,
                              void *addr) {
    struct rlimit rl;
    if (getrlimit(RLIMIT_MEMLOCK, &rl) != 0) return 100;
    rl.rlim_cur = soft;
    if (setrlimit(RLIMIT_MEMLOCK, &rl) != 0) return 101;
    struct cap_header header = {CAP_VERSION_3, 0};
    struct cap_data data[2] = {{0, 0, 0}, {0, 0, 0}};
    if (syscall(NR_CAPGET, &header, data) != 0) return 102;
    data[0].effective &= ~(1u << CAP_IPC_LOCK);
    data[0].permitted &= ~(1u << CAP_IPC_LOCK);
    if (syscall(NR_CAPSET, &header, data) != 0) return 103;
    errno = 0;
    void *p = mmap(addr, len, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_LOCKED |
                       (fixed ? MAP_FIXED : 0),
                   -1, 0);
    if (p != MAP_FAILED) {
        munmap(p, len);
        return 0;
    }
    return errno;
}

/* Forked child: drop CAP_IPC_LOCK, set a small RLIMIT_MEMLOCK and call
   mlock()/mlock2() over it.  `do_mlock()` initializes `error = -ENOMEM` and
   returns exactly that for the rlimit failure (mm/mlock.c), unlike
   MAP_LOCKED's -EAGAIN from mlock_future_ok() in do_mmap(). */
static int child_mlock_errno(unsigned long len, rlim_t soft, int use_mlock2) {
    struct rlimit rl;
    if (getrlimit(RLIMIT_MEMLOCK, &rl) != 0) return 100;
    rl.rlim_cur = soft;
    if (setrlimit(RLIMIT_MEMLOCK, &rl) != 0) return 101;
    struct cap_header header = {CAP_VERSION_3, 0};
    struct cap_data data[2] = {{0, 0, 0}, {0, 0, 0}};
    if (syscall(NR_CAPGET, &header, data) != 0) return 102;
    data[0].effective &= ~(1u << CAP_IPC_LOCK);
    data[0].permitted &= ~(1u << CAP_IPC_LOCK);
    if (syscall(NR_CAPSET, &header, data) != 0) return 103;
    void *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) return 104;
    errno = 0;
    long result = use_mlock2 ? syscall(325, p, len, 0) : syscall(149, p, len);
    int saved = errno;
    if (result == 0) {
        munmap(p, len);
        return 0;
    }
    munmap(p, len);
    return saved;
}

static void mmap_extra_case(void) {
    begin("mmap.raw-differential");
    /* arch/x86/kernel/sys_x86_64.c:SYSCALL_DEFINE6(mmap) only rejects an
       offset whose low 12 bits are set; an anonymous mapping keeps the page
       offset in vm_pgoff instead of failing. */
    void *anon_offset = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, PAGE);
    check(anon_offset != MAP_FAILED, "mmap-anon-page-offset");
    check(munmap(anon_offset, PAGE) == 0, "mmap-anon-page-offset-cleanup");
    mark("ANON_OFFSET_IGNORED");
    /* arch/x86/include/asm/mman.h:arch_calc_vm_prot_bits only contributes
       pkey bits, so PROT_GROWSDOWN/PROT_GROWSUP are accepted and ignored. */
    void *down = mmap(NULL, PAGE, PROT_READ | PROT_WRITE | PROT_GROWSDOWN,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check(down != MAP_FAILED, "mmap-prot-growsdown");
    check(munmap(down, PAGE) == 0, "mmap-prot-growsdown-cleanup");
    void *up = mmap(NULL, PAGE, PROT_READ | PROT_WRITE | PROT_GROWSUP,
                    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check(up != MAP_FAILED, "mmap-prot-growsup");
    check(munmap(up, PAGE) == 0, "mmap-prot-growsup-cleanup");
    mark("PROT_GROWSDOWN_UP");
    /* MAP_DROPPABLE is a member of MAP_TYPE (include/uapi/linux/mman.h:20),
       so it is used without MAP_PRIVATE/MAP_SHARED and installs
       VM_DROPPABLE|VM_NORESERVE|VM_WIPEONFORK|VM_DONTDUMP; the child of a
       fork sees zeroes while the parent keeps its page (mm/mmap.c:505-543,
       and mm/mmap.c:1796-1841 skips copy_page_range for VM_WIPEONFORK). */
    unsigned char *drop = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                               MAP_ANONYMOUS | MAP_DROPPABLE, -1, 0);
    check(drop != MAP_FAILED, "mmap-droppable");
    volatile unsigned char *dropped = drop;
    dropped[0] = 0x5a;
    fflush(NULL);
    pid_t pid = fork();
    check(pid >= 0, "fork-droppable");
    if (!pid) _exit(dropped[0] == 0 ? 0 : 1);
    int wipe_status = 0;
    check(waitpid(pid, &wipe_status, 0) == pid, "wait-droppable");
    check(WIFEXITED(wipe_status) && WEXITSTATUS(wipe_status) == 0,
          "mmap-droppable-child-zero-page");
    check(dropped[0] == 0x5a, "mmap-droppable-parent-keeps");
    check(munmap(drop, PAGE) == 0, "mmap-droppable-cleanup");
    /* The combinations mm/mmap.c:495-543 rejects. */
    ERROR(mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
               MAP_SHARED | MAP_ANONYMOUS | MAP_DROPPABLE, -1, 0),
          EINVAL, "mmap-droppable-shared");
    ERROR(mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
               MAP_PRIVATE | MAP_ANONYMOUS | MAP_DROPPABLE, -1, 0),
          EINVAL, "mmap-droppable-private-combo");
    ERROR(mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
               MAP_ANONYMOUS | MAP_DROPPABLE | MAP_LOCKED, -1, 0),
          EINVAL, "mmap-droppable-locked");
    ERROR(mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
               MAP_ANONYMOUS | MAP_DROPPABLE | MAP_HUGETLB, -1, 0),
          EINVAL, "mmap-droppable-hugetlb");
    ERROR(mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
               MAP_PRIVATE | MAP_ANONYMOUS | MAP_GROWSDOWN | MAP_SHARED,
               -1, 0),
          EINVAL, "mmap-shared-growsdown");
    mark("DROPPABLE_MATRIX");

    /* MAP_LOCKED admission: a zero RLIMIT_MEMLOCK without CAP_IPC_LOCK is
       EPERM from can_do_mlock() (mm/mmap.c:417-419, mm/mlock.c:40-47), while
       a nonzero limit the request exceeds is EAGAIN from mlock_future_ok()
       (mm/mmap.c:421-422).  A privileged child ignores the limit. */
    unsigned char *window = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check(window != MAP_FAILED, "mmap-window");
    check(munmap(window, PAGE) == 0, "mmap-window-free");
    fflush(NULL);
    pid = fork();
    check(pid >= 0, "fork-locked-zero");
    if (!pid) _exit(child_locked_errno(4 * PAGE, 0, 1, window));
    expect_child_errno(pid, EPERM, "mmap-locked-zero-limit-eperm");
    fflush(NULL);
    pid = fork();
    check(pid >= 0, "fork-locked-small");
    if (!pid) _exit(child_locked_errno(4 * PAGE, PAGE, 1, window));
    expect_child_errno(pid, EAGAIN, "mmap-locked-small-limit-eagain");
    fflush(NULL);
    pid = fork();
    check(pid >= 0, "fork-locked-privileged");
    if (!pid) {
        /* The privileged control keeps CAP_IPC_LOCK, so can_do_mlock() is
           true regardless of the limit and the mapping must be admitted.  It
           asks for the address instead of pinning it: Linux rejects a
           MAP_FIXED range that rewrites the geometry of a neighbouring
           mapping with EINVAL once the lock admission has already passed. */
        struct rlimit rl;
        if (getrlimit(RLIMIT_MEMLOCK, &rl) != 0) _exit(100);
        rl.rlim_cur = 0;
        if (setrlimit(RLIMIT_MEMLOCK, &rl) != 0) _exit(101);
        errno = 0;
        void *p = mmap(NULL, 4 * PAGE, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_LOCKED, -1, 0);
        if (p == MAP_FAILED) _exit(errno);
        munmap(p, 4 * PAGE);
        _exit(0);
    }
    expect_child_errno(pid, 0, "mmap-locked-privileged-zero-limit");
    mark("LOCKED_LIMIT_ERRNOS");

    /* mlock()/mlock2() over RLIMIT_MEMLOCK is -ENOMEM, which must stay
       distinct from the -EAGAIN MAP_LOCKED reports for the same limit. */
    fflush(NULL);
    pid = fork();
    check(pid >= 0, "fork-mlock2-limit");
    if (!pid) _exit(child_mlock_errno(4 * PAGE, PAGE, 1));
    expect_child_errno(pid, ENOMEM, "mlock2-rlimit-enomem");
    fflush(NULL);
    pid = fork();
    check(pid >= 0, "fork-mlock-limit");
    if (!pid) _exit(child_mlock_errno(4 * PAGE, PAGE, 0));
    expect_child_errno(pid, ENOMEM, "mlock-rlimit-enomem");
    mark("MLOCK_RLIMIT_ENOMEM");

    /* `mm/mmap.c:do_mmap()` dispatches on `flags & MAP_TYPE` once per branch,
       and the branch depends on whether a file is behind the mapping.  With no
       file the switch has no MAP_SHARED_VALIDATE case, so it reaches
       `default: return -EINVAL` whatever the rest of the word says
       (`mm/mmap.c:505-543`); with a file the whole word is checked against
       `LEGACY_MAP_MASK` and anything outside it is -EOPNOTSUPP
       (`mm/mmap.c:425-476`).  MAP_FIXED_NOREPLACE, MAP_SYNC and MAP_DROPPABLE
       are the bits outside that mask, while MAP_EXECUTABLE is inside it even
       though the kernel ignores it. */
    ERROR(mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
               MAP_SHARED_VALIDATE | MAP_ANONYMOUS, -1, 0),
          EINVAL, "mmap-shared-validate-anonymous");
    int validate_fd = (int)syscall(NR_MEMFD_CREATE, "thekernel-mmap-validate", 0);
    check(validate_fd >= 0, "mmap-shared-validate-memfd");
    check(ftruncate(validate_fd, PAGE) == 0, "mmap-shared-validate-memfd-size");
    /* A free address keeps the MAP_FIXED_NOREPLACE EEXIST test out of the way:
       `do_mmap()` runs it before the MAP_TYPE dispatch (`mm/mmap.c:412-415`). */
    void *validate_hint = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check(validate_hint != MAP_FAILED, "mmap-shared-validate-hint");
    check(munmap(validate_hint, PAGE) == 0, "mmap-shared-validate-hint-free");
    ERROR(mmap(validate_hint, PAGE, PROT_READ,
               MAP_SHARED_VALIDATE | MAP_FIXED_NOREPLACE, validate_fd, 0),
          EOPNOTSUPP, "mmap-shared-validate-fixed-noreplace");
    unsigned char *legacy =
        mmap(NULL, PAGE, PROT_READ, MAP_SHARED_VALIDATE | MAP_EXECUTABLE, validate_fd, 0);
    check(legacy != MAP_FAILED, "mmap-shared-validate-legacy-bit");
    check(munmap(legacy, PAGE) == 0, "mmap-shared-validate-legacy-bit-cleanup");
    check(close(validate_fd) == 0, "mmap-shared-validate-close");
    mark("SHARED_VALIDATE_FLAG_MASK");

    /* `mm/mmap.c:1333-1357:may_expand_vm()` compares
       `mm->data_vm + npages` with RLIMIT_DATA only for a data mapping —
       `mm/vma.h:527-534` defines that as VM_WRITE without VM_SHARED or
       VM_STACK — and it exempts the case where the soft limit is exactly zero,
       because then `rlimit_max(RLIMIT_DATA)` decides (the Valgrind workaround).
       A limit of one byte is nonzero and below one page, so every private
       writable mapping must fail with ENOMEM while a read-only or shared one
       is not compared with the limit at all. */
    struct rlimit saved_data_limit;
    check(getrlimit(RLIMIT_DATA, &saved_data_limit) == 0, "mmap-data-limit-get");
    struct rlimit data_limit = saved_data_limit;
    data_limit.rlim_cur = 0;
    data_limit.rlim_max = RLIM_INFINITY;
    check(setrlimit(RLIMIT_DATA, &data_limit) == 0, "mmap-data-limit-zero-set");
    void *unbounded = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check(unbounded != MAP_FAILED, "mmap-data-limit-zero-maps");
    check(munmap(unbounded, PAGE) == 0, "mmap-data-limit-zero-cleanup");
    data_limit.rlim_cur = 1;
    check(setrlimit(RLIMIT_DATA, &data_limit) == 0, "mmap-data-limit-one-set");
    ERROR(mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0),
          ENOMEM, "mmap-data-limit-private-writable");
    void *read_only = mmap(NULL, PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check(read_only != MAP_FAILED, "mmap-data-limit-read-only");
    check(munmap(read_only, PAGE) == 0, "mmap-data-limit-read-only-cleanup");
    void *shared_pages = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                              MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    check(shared_pages != MAP_FAILED, "mmap-data-limit-shared");
    check(munmap(shared_pages, PAGE) == 0, "mmap-data-limit-shared-cleanup");
    check(setrlimit(RLIMIT_DATA, &saved_data_limit) == 0, "mmap-data-limit-restore");
    mark("RLIMIT_DATA_GROWTH");
    done();
}

/* ------------------------------------------------------------ madvise ---- */

static void madvise_extra_case(void) {
    begin("madvise.raw-differential");
    /* MADV_DONTNEED_LOCKED exists to discard a locked range: it is accepted
       for a locked anonymous VMA, while plain MADV_DONTNEED on the same VMA
       is EINVAL (mm/madvise.c:1438-1439, madvise_dontneed_free_valid_vma). */
    unsigned char *p = pages(1);
    p[0] = 1;
    check(mlock(p, PAGE) == 0, "madvise-lock-anon");
    check(syscall(NR_MADVISE, p, PAGE, MADV_DONTNEED_LOCKED) == 0,
          "madvise-dontneed-locked-anon");
    ERROR(syscall(NR_MADVISE, p, PAGE, MADV_DONTNEED), EINVAL,
          "madvise-dontneed-locked-anon-plain");
    check(munlock(p, PAGE) == 0, "madvise-unlock-anon");
    check(munmap(p, PAGE) == 0, "madvise-anon-cleanup");

    int fd = (int)syscall(NR_MEMFD_CREATE, "thekernel-madvise-locked", 0);
    check(fd >= 0, "madvise-memfd-create");
    check(ftruncate(fd, PAGE) == 0, "madvise-memfd-size");
    unsigned char *sh = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED,
                             fd, 0);
    check(sh != MAP_FAILED, "madvise-shared-mmap");
    sh[0] = 7;
    check(mlock(sh, PAGE) == 0, "madvise-lock-shared-file");
    check(syscall(NR_MADVISE, sh, PAGE, MADV_DONTNEED_LOCKED) == 0,
          "madvise-dontneed-locked-shared");
    ERROR(syscall(NR_MADVISE, sh, PAGE, MADV_DONTNEED), EINVAL,
          "madvise-dontneed-plain-shared-locked");
    ERROR(syscall(NR_MADVISE, sh, PAGE, MADV_REMOVE), EINVAL,
          "madvise-remove-locked-shared");
    check(munlock(sh, PAGE) == 0, "madvise-unlock-shared");
    check(syscall(NR_MADVISE, sh, PAGE, MADV_REMOVE) == 0,
          "madvise-remove-shared-file");
    check(munmap(sh, PAGE) == 0, "madvise-shared-cleanup");
    check(close(fd) == 0, "madvise-memfd-close");
    mark("DONTNEED_LOCKED");

    /* MADV_REMOVE needs a shared-writable file mapping: a private mapping has
       the file but not VM_SHARED|VM_MAYWRITE (EACCES), and anonymous memory
       has no file at all (EINVAL).  mm/madvise.c:1030-1040. */
    char path[] = "/tmp/thekernel-madvise-XXXXXX";
    int tmp = mkstemp(path);
    check(tmp >= 0, "madvise-tmpfile");
    check(ftruncate(tmp, PAGE) == 0, "madvise-tmpfile-size");
    unsigned char *pf = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE,
                             tmp, 0);
    check(pf != MAP_FAILED, "madvise-private-file-mmap");
    pf[0] = 3;
    ERROR(syscall(NR_MADVISE, pf, PAGE, MADV_REMOVE), EACCES,
          "madvise-remove-private-file");
    check(munmap(pf, PAGE) == 0, "madvise-private-file-cleanup");
    check(close(tmp) == 0, "madvise-tmpfile-close");
    check(unlink(path) == 0, "madvise-tmpfile-unlink");
    unsigned char *anon = pages(1);
    anon[0] = 1;
    ERROR(syscall(NR_MADVISE, anon, PAGE, MADV_REMOVE), EINVAL,
          "madvise-remove-anon");
    mark("REMOVE_BY_MAPPING_TYPE");
    /* MADV_FREE shares madvise_dontneed_free_valid_vma() with MADV_DONTNEED,
       so a locked VMA is EINVAL rather than EBUSY. */
    check(mlock(anon, PAGE) == 0, "madvise-free-lock");
    ERROR(syscall(NR_MADVISE, anon, PAGE, MADV_FREE), EINVAL,
          "madvise-free-locked");
    check(munlock(anon, PAGE) == 0, "madvise-free-unlock");
    mark("FREE_LOCKED_EINVAL");
    /* Advice whose feature the oracle kernel's .config does not enable is
       EINVAL from the availability table (mm/madvise.c:1517-1559), not
       success: KSM, transparent huge pages and memory failure are all off. */
    ERROR(syscall(NR_MADVISE, anon, PAGE, MADV_MERGEABLE), EINVAL,
          "madvise-mergeable-unavailable");
    ERROR(syscall(NR_MADVISE, anon, PAGE, MADV_UNMERGEABLE), EINVAL,
          "madvise-unmergeable-unavailable");
    ERROR(syscall(NR_MADVISE, anon, PAGE, MADV_HUGEPAGE), EINVAL,
          "madvise-hugepage-unavailable");
    ERROR(syscall(NR_MADVISE, anon, PAGE, MADV_NOHUGEPAGE), EINVAL,
          "madvise-nohugepage-unavailable");
    ERROR(syscall(NR_MADVISE, anon, PAGE, MADV_COLLAPSE), EINVAL,
          "madvise-collapse-unavailable");
    ERROR(syscall(NR_MADVISE, anon, PAGE, MADV_HWPOISON), EINVAL,
          "madvise-hwpoison-unavailable");
    ERROR(syscall(NR_MADVISE, anon, PAGE, MADV_SOFT_OFFLINE), EINVAL,
          "madvise-soft-offline-unavailable");
    mark("UNAVAILABLE_ADVICES");
    check(syscall(NR_MADVISE, anon, PAGE, MADV_DONTDUMP) == 0,
          "madvise-dontdump-ok");
    check(syscall(NR_MADVISE, anon, PAGE, MADV_GUARD_INSTALL) == 0,
          "madvise-guard-install-ok");
    check(syscall(NR_MADVISE, anon, PAGE, MADV_GUARD_REMOVE) == 0,
          "madvise-guard-remove-ok");
    check(munmap(anon, PAGE) == 0, "madvise-anon2-cleanup");
    mark("GUARD_AND_DONTDUMP");

    /* A hole inside the range does not stop the walk: Linux records the gap
       and carries on (`mm/madvise.c:1693-1704`, resumed by
       `vma = find_vma(mm, vma->vm_end)` at `:1730`), so both VMAs around the
       hole carry VM_WIPEONFORK and `dup_mmap()` skips `copy_page_range()`
       for each of them (`mm/mmap.c:1839-1840`), leaving the child a zero
       page on both sides. */
    unsigned char *w = pages(3);
    volatile unsigned char *wv = w;
    wv[0] = 0x11;
    wv[2 * PAGE] = 0x22;
    check(munmap(w + PAGE, PAGE) == 0, "madvise-hole-carve");
    ERROR(syscall(NR_MADVISE, w, 3 * PAGE, MADV_WIPEONFORK), ENOMEM,
          "madvise-wipeonfork-hole-enomem");
    fflush(NULL);
    pid_t pid = fork();
    check(pid >= 0, "fork-wipeonfork-hole");
    /* The child's exit status names which page still held the parent's byte:
       bit 0 is the VMA in front of the hole, bit 1 the VMA after it. */
    if (!pid) _exit((wv[0] == 0 ? 0 : 1) | (wv[2 * PAGE] == 0 ? 0 : 2));
    int hole_status = 0;
    check(waitpid(pid, &hole_status, 0) == pid, "wait-wipeonfork-hole");
    check(WIFEXITED(hole_status) && WEXITSTATUS(hole_status) == 0,
          "madvise-wipeonfork-child-zero-pages");
    check(munmap(w, 3 * PAGE) == 0, "madvise-hole-cleanup");
    mark("WIPEONFORK_HOLE");

    /* MAP_DROPPABLE is a persistent VMA property, not a pair of one-shot side
       effects applied at mmap time.  `mm/mmap.c:505-543` installs
       VM_DROPPABLE|VM_NORESERVE|VM_WIPEONFORK|VM_DONTDUMP, and the two
       advices that would clear the derived fork/dump policy refuse the VMA
       instead of silently undoing the mapping's contract:

       ```c
       	case MADV_KEEPONFORK:
       		if (new_flags & VM_DROPPABLE)
       			return -EINVAL;
       ```
       (`mm/madvise.c:1395-1397`), and

       ```c
       	case MADV_DODUMP:
       		if ((!is_vm_hugetlb_page(vma) && (new_flags & VM_SPECIAL)) ||
       		    (new_flags & VM_DROPPABLE))
       			return -EINVAL;
       ```
       (`mm/madvise.c:1402-1406`). */
    unsigned char *drop = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                               MAP_ANONYMOUS | MAP_DROPPABLE, -1, 0);
    check(drop != MAP_FAILED, "madvise-droppable-mmap");
    drop[0] = 0x5a;
    ERROR(syscall(NR_MADVISE, drop, PAGE, MADV_KEEPONFORK), EINVAL,
          "madvise-droppable-keeponfork");
    mark("DROPPABLE_KEEPONFORK_EINVAL");
    ERROR(syscall(NR_MADVISE, drop, PAGE, MADV_DODUMP), EINVAL,
          "madvise-droppable-dodump");
    mark("DROPPABLE_DODUMP_EINVAL");
    /* The same two advices still succeed on an ordinary anonymous VMA, so the
       refusals above are the droppable property and not a blanket refusal. */
    unsigned char *plain = pages(1);
    plain[0] = 0x5a;
    check(syscall(NR_MADVISE, plain, PAGE, MADV_KEEPONFORK) == 0,
          "madvise-plain-keeponfork");
    check(syscall(NR_MADVISE, plain, PAGE, MADV_DODUMP) == 0,
          "madvise-plain-dodump");
    /* A droppable folio is never marked swapbacked (`mm/rmap.c:1652-1656`),
       so reclaim discards it instead of writing it out, and the dirty-page
       restore in `try_to_unmap_one()` explicitly exempts it:

       ```c
       			if (folio_test_dirty(folio) && !(vma->vm_flags & VM_DROPPABLE)) {
       ```
       (`mm/rmap.c:2258`).  MADV_PAGEOUT is the only guest-visible route into
       that decision, and the dropped page reads back as a fresh zero page
       while the same advice leaves an ordinary anonymous page intact when no
       swap slot can be allocated. */
    check(syscall(NR_MADVISE, drop, PAGE, MADV_PAGEOUT) == 0,
          "madvise-droppable-pageout");
    check(drop[0] == 0, "madvise-droppable-pageout-drops-content");
    mark("DROPPABLE_PAGEOUT_DROPS");
    check(syscall(NR_MADVISE, plain, PAGE, MADV_PAGEOUT) == 0,
          "madvise-plain-pageout");
    check(plain[0] == 0x5a, "madvise-plain-pageout-keeps-content");
    mark("PLAIN_PAGEOUT_KEEPS");
    check(munmap(drop, PAGE) == 0, "madvise-droppable-cleanup");
    check(munmap(plain, PAGE) == 0, "madvise-plain-cleanup");
    /* `vma_can_userfault()` refuses a droppable VMA outright
       (`mm/userfaultfd.c:2114`), and `userfaultfd_register()` turns that into
       `-EINVAL` (`mm/userfaultfd.c:3658-3661`), so a userfaultfd context can
       never be attached to memory the kernel may drop at any time.
       `UFFD_USER_MODE_ONLY` is used so the creation itself is allowed without
       `CAP_SYS_PTRACE` or a permissive `vm.unprivileged_userfaultfd`:

       ```c
       	if (flags & UFFD_USER_MODE_ONLY)
       		return true;
       ```

       (`mm/userfaultfd.c:4481-4494`), which lets this assertion reach the
       registration decision instead of stopping at the creation gate.  The
       oracle kernel is built without CONFIG_USERFAULTFD
       (`# CONFIG_USERFAULTFD is not set`), where userfaultfd(2) is ENOSYS from
       the syscall stub whatever the flags are, so only there does the
       unavailable-syscall branch run, and the note line records it. */
    {
        int uffd = (int)syscall(NR_USERFAULTFD,
                                O_CLOEXEC | O_NONBLOCK | UFFD_USER_MODE_ONLY);
        if (uffd < 0) {
            check(errno == ENOSYS || errno == EPERM,
                  "madvise-droppable-uffd-unavailable");
            printf("THEKERNEL_MM_NOTE droppable-uffd-unavailable errno=%d\n",
                   errno);
        } else {
            struct uffdio_api api;
            memset(&api, 0, sizeof(api));
            api.api = UFFD_API_VALUE;
            check(ioctl(uffd, UFFDIO_API_CMD, &api) == 0,
                  "madvise-droppable-uffd-api");
            unsigned char *watch = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                                        MAP_ANONYMOUS | MAP_DROPPABLE, -1, 0);
            check(watch != MAP_FAILED, "madvise-droppable-uffd-mmap");
            /* An ordinary anonymous VMA registers, so the refusal below is
               the droppable property and not a broken registration call. */
            unsigned char *control = pages(1);
            struct uffdio_register reg;
            memset(&reg, 0, sizeof(reg));
            reg.range.start = (uint64_t)(uintptr_t)control;
            reg.range.len = PAGE;
            reg.mode = UFFDIO_REGISTER_MODE_MISSING;
            check(ioctl(uffd, UFFDIO_REGISTER_CMD, &reg) == 0,
                  "madvise-plain-uffd-register");
            memset(&reg, 0, sizeof(reg));
            reg.range.start = (uint64_t)(uintptr_t)watch;
            reg.range.len = PAGE;
            reg.mode = UFFDIO_REGISTER_MODE_MISSING;
            ERROR(ioctl(uffd, UFFDIO_REGISTER_CMD, &reg), EINVAL,
                  "madvise-droppable-uffd-register");
            check(munmap(control, PAGE) == 0, "madvise-plain-uffd-cleanup");
            check(munmap(watch, PAGE) == 0, "madvise-droppable-uffd-cleanup");
            check(close(uffd) == 0, "madvise-droppable-uffd-close");
        }
    }
    /* "refused" rather than "EINVAL" because the assertion covers both the
       live-syscall arm (EINVAL) and the unavailable-syscall arm. */
    mark("DROPPABLE_UFFDIO_REGISTER_REFUSED");
    done();
}

/* -------------------------------------------------------------- msync ---- */

static void msync_extra_case(void) {
    begin("msync.raw-differential");
    int fd = (int)syscall(NR_MEMFD_CREATE, "thekernel-msync", 0);
    check(fd >= 0, "msync-memfd-create");
    check(ftruncate(fd, 2 * PAGE) == 0, "msync-memfd-size");
    unsigned char *p = mmap(NULL, 2 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED,
                            fd, 0);
    check(p != MAP_FAILED, "msync-shared-mmap");
    p[0] = 1;
    p[PAGE] = 2;
    check(syscall(NR_MSYNC, p, 2 * PAGE, MS_SYNC) == 0, "msync-unlocked-shared");
    /* mlock() splits the VMA at the locked range.  MS_INVALIDATE still walks
       the whole span and reports EBUSY for the locked part (mm/msync.c:42-109
       keeps the first error), while a sync of the unlocked prefix succeeds. */
    check(mlock(p + PAGE, PAGE) == 0, "msync-lock-tail");
    ERROR(syscall(NR_MSYNC, p, 2 * PAGE, MS_SYNC | MS_INVALIDATE), EBUSY,
          "msync-partial-lock-invalidate");
    check(syscall(NR_MSYNC, p, PAGE, MS_SYNC) == 0,
          "msync-unlocked-prefix-syncs");
    check(munlock(p + PAGE, PAGE) == 0, "msync-unlock-tail");
    check(munmap(p, 2 * PAGE) == 0, "msync-cleanup");
    check(close(fd) == 0, "msync-memfd-close");
    mark("PARTIAL_LOCK_EBUSY");
    /* mm/msync.c applies MS_SYNC and MS_INVALIDATE per VMA in address order,
       so a shared file range in front of a later VM_LOCKED VMA is already
       written back when the syscall reports EBUSY.  A whole-range VM_LOCKED
       preflight would fail before flushing anything. */
    char order_path[] = "/root/thekernel-msync-order-XXXXXX";
    fd = mkstemp(order_path); check(fd >= 0, "msync-order-create");
    check(ftruncate(fd, 5 * PAGE) == 0, "msync-order-size");
    int direct = open(order_path, O_RDONLY | O_DIRECT);
    check(direct >= 0 && unlink(order_path) == 0, "msync-order-direct-reader");
    void *readback = NULL;
    check(posix_memalign(&readback, PAGE, PAGE) == 0, "msync-order-buffer");
    unsigned char *shared = mmap(NULL, 3 * PAGE, PROT_READ | PROT_WRITE,
                                 MAP_SHARED, fd, 0);
    check(shared != MAP_FAILED, "msync-order-shared-map");
    /* Replace the third page with a separate, non-contiguous file range so
       mlock() cannot leave the dirty page inside the locked VMA. */
    unsigned char *locked = mmap(shared + 2 * PAGE, PAGE, PROT_READ,
                                 MAP_SHARED | MAP_FIXED, fd, 4 * PAGE);
    check(locked == shared + 2 * PAGE, "msync-order-locked-map");
    memset(shared, 'G', PAGE);
    check(mlock(locked, PAGE) == 0, "msync-order-mlock");
    errno = 0;
    check(msync(shared, 3 * PAGE, MS_SYNC | MS_INVALIDATE) == -1 &&
          errno == EBUSY, "msync-order-ebusy");
    /* Unlock before the direct read: this kernel's O_DIRECT read refuses to
       revoke a file whose cached page is pinned by mlock (EBUSY), and that
       unrelated pin would mask whether the prefix reached the file.  The
       writeback under test already happened inside the msync above. */
    check(munlock(locked, PAGE) == 0, "msync-order-munlock");
    check(pread(direct, readback, PAGE, 0) == PAGE, "msync-order-direct-read");
    for (size_t i = 0; i < PAGE; ++i)
        check(((unsigned char *)readback)[i] == 'G',
              "msync-order-flushed-before-ebusy");
    check(munmap(shared, 3 * PAGE) == 0 && close(direct) == 0 && close(fd) == 0,
          "msync-order-cleanup");
    free(readback);
    /* The same order with the lock *inside* one mapping.  mlock() splits the
       VMA at the locked range (mm/mlock.c:mlock_fixup() reaches
       vma_modify_flags()), so the dirty first page sits in an unlocked VMA
       that the walk reaches before the locked one and has to be written back
       before the EBUSY.  A test that only asks whether *any* part of the
       requested range is locked answers EBUSY with nothing flushed. */
    char inner_path[] = "/root/thekernel-msync-inner-XXXXXX";
    fd = mkstemp(inner_path); check(fd >= 0, "msync-inner-create");
    check(ftruncate(fd, 3 * PAGE) == 0, "msync-inner-size");
    direct = open(inner_path, O_RDONLY | O_DIRECT);
    check(direct >= 0 && unlink(inner_path) == 0, "msync-inner-direct-reader");
    readback = NULL;
    check(posix_memalign(&readback, PAGE, PAGE) == 0, "msync-inner-buffer");
    unsigned char *inner = mmap(NULL, 3 * PAGE, PROT_READ | PROT_WRITE,
                                MAP_SHARED, fd, 0);
    check(inner != MAP_FAILED, "msync-inner-map");
    memset(inner, 'I', PAGE);
    check(mlock(inner + PAGE, PAGE) == 0, "msync-inner-mlock");
    errno = 0;
    check(msync(inner, 3 * PAGE, MS_SYNC | MS_INVALIDATE) == -1 &&
          errno == EBUSY, "msync-inner-ebusy");
    check(munlock(inner + PAGE, PAGE) == 0, "msync-inner-munlock");
    check(pread(direct, readback, PAGE, 0) == PAGE, "msync-inner-direct-read");
    for (size_t i = 0; i < PAGE; ++i)
        check(((unsigned char *)readback)[i] == 'I',
              "msync-inner-flushed-before-ebusy");
    check(munmap(inner, 3 * PAGE) == 0 && close(direct) == 0 && close(fd) == 0,
          "msync-inner-cleanup");
    free(readback);
    mark("PREFIX_FLUSHED_BEFORE_EBUSY");
    done();
}

/* ------------------------------------------ process_madvise(440) and
                                                process_mrelease(448) ---- */

static void *plain_exit_thread(void *arg) {
    int ready = *(int *)arg;
    /* exit(2) retires only this thread; the leader stays alive. */
    if (write(ready, "x", 1) != 1) _exit(1);
    syscall(SYS_exit, 0);
    return NULL;
}

static int thread_task_count(pid_t pid) {
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/task", pid);
    DIR *dir = opendir(path);
    if (!dir) return -1;
    int count = 0;
    struct dirent *entry;
    while ((entry = readdir(dir))) {
        if (entry->d_name[0] != '.') ++count;
    }
    closedir(dir);
    return count;
}

static void remote_extra_case(void) {
    begin("process-mrelease.raw-differential");
    unsigned char *p = pages(1);
    p[0] = 7;
    /* PIDFD_SELF_* names a live task, and a live task never has
       task_will_free_mem(): EINVAL, not ESRCH. */
    ERROR(syscall(NR_PROCESS_MRELEASE, PIDFD_SELF_THREAD, 0), EINVAL,
          "process-mrelease-self");
    ERROR(syscall(NR_PROCESS_MRELEASE, PIDFD_SELF_THREAD_GROUP, 0), EINVAL,
          "process-mrelease-self-group");
    mark("SELF_IDENTIFIERS_EINVAL");
    /* A valid descriptor that is not a pidfd is EBADF from pidfd_pid()
       (fs/pidfs.c:706-711), not EINVAL. */
    int fds[2];
    check(pipe(fds) == 0, "process-remote-pipe");
    ERROR(syscall(NR_PROCESS_MRELEASE, fds[0], 0), EBADF,
          "process-mrelease-pipe-fd");
    check(close(fds[0]) == 0, "process-remote-close-read");
    check(close(fds[1]) == 0, "process-remote-close-write");
    mark("NON_PIDFD_EBADF");
    check(munmap(p, PAGE) == 0, "process-remote-cleanup");

    /* A live child that is not dying holds an mm: find_lock_task_mm()
       succeeds (no ESRCH) and task_will_free_mem() is false -> EINVAL. */
    int sync[2];
    check(pipe(sync) == 0, "process-release-pipe");
    fflush(NULL);
    pid_t pid = fork();
    check(pid >= 0, "fork-release-child");
    if (!pid) {
        close(sync[1]);
        char byte;
        if (read(sync[0], &byte, 1) != 1) _exit(1);
        _exit(0);
    }
    close(sync[0]);
    int pidfd = (int)syscall(SYS_pidfd_open, pid, 0);
    if (pidfd >= 0) {
        ERROR(syscall(NR_PROCESS_MRELEASE, pidfd, 0), EINVAL,
              "process-mrelease-live-child");
        check(close(pidfd) == 0, "process-release-pidfd-close");
    }
    check(write(sync[1], "x", 1) == 1, "process-release-signal");
    check(close(sync[1]) == 0, "process-release-close");
    reap(pid, 0);

    /* One thread of a multi-threaded process calling exit(2) (not
       exit_group(2)) leaves the group alive: the leader is neither
       thread_group_empty() nor is the group exiting, so mrelease must
       answer EINVAL rather than reap the still-live mm. */
    int ready[2], release[2];
    check(pipe(ready) == 0 && pipe(release) == 0, "thread-exit-pipes");
    fflush(NULL);
    pid = fork();
    check(pid >= 0, "fork-thread-exit-child");
    if (!pid) {
        close(ready[0]); close(release[1]);
        pthread_t thread;
        if (pthread_create(&thread, NULL, plain_exit_thread, &ready[1]) != 0)
            _exit(1);
        char byte;
        if (read(release[0], &byte, 1) != 1) _exit(1);
        _exit(0);
    }
    close(ready[1]); close(release[0]);
    char byte;
    check(read(ready[0], &byte, 1) == 1, "thread-exit-ready");
    check(close(ready[0]) == 0, "thread-exit-ready-close");
    /* Wait until the exited thread has fully left the thread group so the
       eligibility snapshot is exact on any kernel. */
    int tasks = 0;
    for (int i = 0; i < 10000 && tasks != 1; ++i) {
        usleep(1000);
        tasks = thread_task_count(pid);
    }
    check(tasks == 1, "thread-exit-detached");
    pidfd = (int)syscall(SYS_pidfd_open, pid, 0);
    if (pidfd >= 0) {
        ERROR(syscall(NR_PROCESS_MRELEASE, pidfd, 0), EINVAL,
              "process-mrelease-one-thread-exited");
        check(close(pidfd) == 0, "thread-exit-pidfd-close");
    }
    check(write(release[1], "x", 1) == 1, "thread-exit-release");
    check(close(release[1]) == 0, "thread-exit-release-close");
    reap(pid, 0);
    mark("PARTIAL_THREAD_EXIT_EINVAL");
    done();
}

/* ------------------------------------------------------------------ *
 * migrate_pages / set_mempolicy_home_node
 * ------------------------------------------------------------------ */

#ifndef __NR_migrate_pages
#define __NR_migrate_pages 256
#endif
#ifndef __NR_set_mempolicy_home_node
#define __NR_set_mempolicy_home_node 450
#endif
#ifndef MPOL_DEFAULT
#define MPOL_DEFAULT 0
#endif
#ifndef MPOL_BIND
#define MPOL_BIND 2
#endif

/* kernel_migrate_pages() builds BOTH node masks through get_nodes() before it
 * looks the target up (mm/mempolicy.c), and get_nodes() returns 0 outright
 * once `--maxnode` reaches zero, so an empty mask is accepted for a pid that
 * does not exist.  This kernel resolved the target first and answered ESRCH. */
static void migrate_pages_case(void) {
    unsigned long mask = 1UL;
    pid_t self = getpid();

    begin("migrate-pages.raw-differential");

    /* An empty new-nodes mask is EINVAL: get_nodes() accepts it, but
     * kernel_migrate_pages() then rejects `nodes_empty(*new)` after the
     * target has been resolved (mm/mempolicy.c). */
    ERROR(syscall(__NR_migrate_pages, self, 1, NULL, NULL), EINVAL, "empty-new-mask-self");
    ERROR(syscall(__NR_migrate_pages, 0, 1, &mask, &mask), EINVAL, "empty-new-mask-zero-pid");
    mark("EMPTY_NEW_MASK_EINVAL");

    /* maxnode is pre-decremented, so 0 underflows and exceeds the 32 KiB
     * PAGE_SIZE*BITS_PER_BYTE ceiling of get_nodes(). */
    ERROR(syscall(__NR_migrate_pages, 0, 0, &mask, &mask), EINVAL, "maxnode-zero-einval");
    mark("MAXNODE_BOUND");

    /* An empty mask does NOT short-circuit the call: get_nodes() returns 0
     * and the target is still resolved, so a pid with no vpid is ESRCH. */
    ERROR(syscall(__NR_migrate_pages, 0x7ffffff0, 1, NULL, NULL), ESRCH,
          "unknown-pid-empty-mask-esrch");
    mark("EMPTY_MASK_DOES_NOT_SKIP_TARGET_LOOKUP");

    /* A bad maxnode, however, is decided by get_nodes() before the target is
     * ever looked up, so it is EINVAL and never ESRCH - including for a pid
     * that does not exist and for a negative one. */
    ERROR(syscall(__NR_migrate_pages, 0x7ffffff0, 0, &mask, &mask), EINVAL,
          "unknown-pid-bad-maxnode-einval");
    ERROR(syscall(__NR_migrate_pages, -1, 0, &mask, &mask), EINVAL, "negative-pid-bad-maxnode");
    mark("MASK_VALIDATION_PRECEDES_TARGET_LOOKUP");

    /* The migration itself is a no-op on this single-node configuration. */
    check(syscall(__NR_migrate_pages, self, 2, &mask, &mask) == 0, "single-node-migration");
    mark("SINGLE_NODE_MIGRATION_SUCCEEDS");

    done();
}

/* set_mempolicy_home_node() checks the start alignment, then the flag word,
 * then that home_node is online, and only then computes the range
 * (mm/mempolicy.c). */
static void home_node_case(void) {
    unsigned char *area = pages(1);
    pid_t child;
    begin("set-mempolicy-home-node.raw-differential");

    ERROR(syscall(__NR_set_mempolicy_home_node, 1UL, PAGE, 0UL, 0UL), EINVAL, "unaligned-start");
    ERROR(syscall(__NR_set_mempolicy_home_node, 0UL, PAGE, 0UL, 1UL), EINVAL, "flags-nonzero");
    mark("ALIGNMENT_AND_FLAGS_FIRST");

    /* home_node must be below MAX_NUMNODES and online; this configuration has
     * a single online node. */
    ERROR(syscall(__NR_set_mempolicy_home_node, 0UL, PAGE, 1UL, 0UL), EINVAL, "offline-node");
    mark("OFFLINE_NODE_EINVAL");

    /* PAGE_ALIGN(0) makes end == start, which returns success before any VMA
     * is examined. */
    check(syscall(__NR_set_mempolicy_home_node, (unsigned long)area, 0UL, 0UL, 0UL) == 0,
          "empty-range-succeeds");
    mark("EMPTY_RANGE_SUCCEEDS_EARLY");

    /* err starts at -ENOENT and is only cleared for a VMA carrying a
     * MPOL_BIND or MPOL_PREFERRED_MANY policy, so an anonymous mapping with
     * no policy reports ENOENT whether or not it is mapped. */
    ERROR(syscall(__NR_set_mempolicy_home_node, (unsigned long)area, PAGE, 0UL, 0UL), ENOENT,
          "mapped-without-policy");
    mark("UNPOLICED_RANGE_ENOENT");

    child = fork();
    if (child == 0) {
        _exit(0);
    }
    reap(child, 0);
    done();
}

int main(void) {
    active = "mm.setup";
    check(sysconf(_SC_PAGESIZE) == PAGE, "native-page-size");
    protect_case(); unmap_case(); mincore_case();
    vm_case(NR_READV, "process-vm-readv.raw-differential");
    vm_case(NR_WRITEV, "process-vm-writev.raw-differential");
    seal_case();
    lock_prefix_case(); process_advice_case(); lockall_case();
    noreserve_case(); memfd_case();
    brk_case(); mmap_extra_case(); madvise_extra_case(); msync_extra_case();
    remote_extra_case();
    migrate_pages_case(); home_node_case();
    puts("THEKERNEL_MM_CONTRACTS_OK");
    return 0;
}
