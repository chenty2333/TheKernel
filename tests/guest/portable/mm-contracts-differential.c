#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/prctl.h>
#include <sys/vfs.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

/* Native x86_64 syscall numbers, verified against Linux's syscall_64.tbl. */
enum { NR_PROTECT = 10, NR_UNMAP = 11, NR_MINCORE = 27,
       NR_READV = 310, NR_WRITEV = 311, NR_SEAL = 462 };
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
    mark("FILE_PAGE_RESIDENCY"); done();
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
int main(void) {
    active = "mm.setup";
    check(sysconf(_SC_PAGESIZE) == PAGE, "native-page-size");
    protect_case(); unmap_case(); mincore_case();
    vm_case(NR_READV, "process-vm-readv.raw-differential");
    vm_case(NR_WRITEV, "process-vm-writev.raw-differential");
    seal_case();
    lock_prefix_case(); process_advice_case(); lockall_case();
    puts("THEKERNEL_MM_CONTRACTS_OK");
    return 0;
}
