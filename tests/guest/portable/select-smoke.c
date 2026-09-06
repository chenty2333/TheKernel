#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int fail(const char *stage)
{
    fprintf(stderr, "THEKERNEL_SELECT_FAIL stage=%s errno=%d\n", stage, errno);
    return 1;
}

int main(void)
{
    alarm(10);
    struct timeval tv = { .tv_usec = 20000 };
    if (syscall(SYS_select, 0, NULL, NULL, NULL, &tv) != 0 ||
        tv.tv_sec != 0 || tv.tv_usec != 0)
        return fail("select-timeout-writeback");
    struct timespec ts = { .tv_nsec = 20000000 };
    /* libc pselect intentionally hides the raw syscall's timeout update. */
    if (syscall(SYS_pselect6, 0, NULL, NULL, NULL, &ts, NULL) != 0 ||
        ts.tv_sec != 0 || ts.tv_nsec != 0)
        return fail("pselect-timeout-writeback");
    struct { const void *mask; size_t bytes; } invalid_mask = {(void *)1, 7};
    ts = (struct timespec){ .tv_sec = 1 };
    errno = 0;
    if (syscall(SYS_pselect6, 0, NULL, NULL, NULL, &ts, &invalid_mask) != -1 ||
        errno != EINVAL || ts.tv_sec != 1 || ts.tv_nsec != 0)
        return fail("pselect-mask-error-preserves-timeout");

    int pipefd[2];
    if (pipe(pipefd) || close(pipefd[0]))
        return fail("pipe");
    fd_set writes, exceptions;
    FD_ZERO(&writes);
    FD_ZERO(&exceptions);
    FD_SET(pipefd[1], &writes);
    FD_SET(pipefd[1], &exceptions);
    tv = (struct timeval){0};
    if (syscall(SYS_select, pipefd[1] + 1, NULL, &writes, &exceptions, &tv) != 1 ||
        !FD_ISSET(pipefd[1], &writes) || FD_ISSET(pipefd[1], &exceptions))
        return fail("pipe-error-is-writable-not-priority");

    FD_SET(pipefd[1], &writes);
    tv = (struct timeval){ .tv_usec = 1500000 };
    if (syscall(SYS_select, pipefd[1] + 1, NULL, &writes, NULL, &tv) != 1 ||
        tv.tv_sec < 0 || tv.tv_usec < 0 || tv.tv_usec >= 1000000 ||
        tv.tv_sec * 1000000 + tv.tv_usec > 1500000)
        return fail("normalize-timeval");

    long page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0)
        return fail("page-size");
    struct timeval *readonly = mmap(NULL, (size_t)page_size,
        PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (readonly == MAP_FAILED)
        return fail("timeout-map");
    *readonly = (struct timeval){ .tv_sec = 1 };
    if (mprotect(readonly, (size_t)page_size, PROT_READ))
        return fail("timeout-protect");
    FD_SET(pipefd[1], &writes);
    if (syscall(SYS_select, pipefd[1] + 1, NULL, &writes, NULL, readonly) != 1 ||
        readonly->tv_sec != 1 || readonly->tv_usec != 0)
        return fail("readonly-timeout-preserves-result");
    if (munmap(readonly, (size_t)page_size) || close(pipefd[1]))
        return fail("cleanup");
    void *boundary = mmap(NULL, (size_t)page_size * 2, PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (boundary == MAP_FAILED ||
        mprotect((char *)boundary + page_size, (size_t)page_size, PROT_NONE))
        return fail("fdset-boundary-map");
    uint64_t *word = (uint64_t *)((char *)boundary + page_size - sizeof(*word));
    *word = 0;
    tv = (struct timeval){0};
    if (syscall(SYS_select, 1, word, NULL, NULL, &tv) != 0 || *word != 0)
        return fail("fdset-native-word-copy");
    if (syscall(SYS_select, 0, (void *)1, (void *)1, (void *)1, &tv) != 0)
        return fail("zero-nfds-ignores-sets");
    if (syscall(SYS_select, 1000000, NULL, NULL, NULL, &tv) != 0)
        return fail("nfds-clamped-to-table");
    errno = 0;
    if (syscall(SYS_select, -1, NULL, NULL, NULL, &tv) != -1 || errno != EINVAL)
        return fail("negative-nfds");
    if (munmap(boundary, (size_t)page_size * 2))
        return fail("fdset-boundary-unmap");
    alarm(0);
    puts("THEKERNEL_SELECT_OK");
    return 0;
}
