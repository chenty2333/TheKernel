#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/futex.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef FUTEX_WAIT
#define FUTEX_WAIT 0
#endif
#ifndef FUTEX_WAKE
#define FUTEX_WAKE 1
#endif
#ifndef FUTEX_CMP_REQUEUE
#define FUTEX_CMP_REQUEUE 4
#endif
#ifndef FUTEX_REQUEUE
#define FUTEX_REQUEUE 3
#endif
#ifndef FUTEX_WAIT_BITSET
#define FUTEX_WAIT_BITSET 9
#endif
#ifndef FUTEX_WAKE_BITSET
#define FUTEX_WAKE_BITSET 10
#endif
#ifndef FUTEX_CLOCK_REALTIME
#define FUTEX_CLOCK_REALTIME 256
#endif
#ifndef FUTEX_PRIVATE_FLAG
#define FUTEX_PRIVATE_FLAG 128
#endif
#ifndef FUTEX_BITSET_MATCH_ANY
#define FUTEX_BITSET_MATCH_ANY 0xffffffffU
#endif

#define WAKE_COUNT_WAITERS 4U
#define WAKE_COUNT_FIRST_BATCH 2U
#define REQUEUE_WAITERS 3U
#define WAIT_TIMEOUT_NS 100000000L
#define WAIT_TIMEOUT_MIN_MS 90L
#define WAIT_TIMEOUT_MAX_MS 5000L
#define EAGAIN_LATENCY_MAX_MS 500L
#define BLOCK_POLL_ATTEMPTS 5000U

#ifndef MFD_CLOEXEC
#define MFD_CLOEXEC 0x0001U
#endif

struct waiter {
    uint32_t *uaddr;
    int op;
    uint32_t expected;
    uint32_t bitset;
    const struct timespec *timeout;
    _Atomic pid_t tid;
    _Atomic int done;
    long result;
    int saved_errno;
    pthread_t thread;
};

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_FUTEX_FAIL %s errno=%d (%s)\n", stage, errno,
            strerror(errno));
    return 1;
}

static int fail_value(const char *stage, long actual, long expected) {
    fprintf(stderr,
            "THEKERNEL_FUTEX_FAIL %s actual=%ld expected=%ld errno=%d (%s)\n",
            stage, actual, expected, errno, strerror(errno));
    return 1;
}

static void marker(const char *value) {
    puts(value);
    fflush(stdout);
}

static long sys_futex(uint32_t *uaddr, int op, uint32_t val,
                      const struct timespec *timeout, uint32_t *uaddr2,
                      uint32_t val3) {
    return syscall(SYS_futex, uaddr, op, val, timeout, uaddr2, val3);
}

static long elapsed_ms(const struct timespec *start,
                       const struct timespec *end) {
    return (end->tv_sec - start->tv_sec) * 1000L +
           (end->tv_nsec - start->tv_nsec) / 1000000L;
}

/* Read the scheduler state character from /proc/<pid>/task/<tid>/stat. The
 * comm field may contain spaces, so parse from the last ')'. */
static int task_state(pid_t pid, pid_t tid) {
    char path[96];
    char buffer[512];
    int length = snprintf(path, sizeof(path), "/proc/%ld/task/%ld/stat",
                          (long)pid, (long)tid);
    if (length <= 0 || (size_t)length >= sizeof(path)) {
        return -1;
    }
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    ssize_t count = read(fd, buffer, sizeof(buffer) - 1);
    close(fd);
    if (count <= 0) {
        return -1;
    }
    buffer[count] = '\0';
    const char *close_paren = strrchr(buffer, ')');
    if (close_paren == NULL || close_paren[1] != ' ' ||
        close_paren[2] == '\0') {
        return -1;
    }
    return (unsigned char)close_paren[2];
}

/* Handshake: the waiter publishes its TID immediately before entering the
 * kernel, and a task only shows state 'S' once it sleeps in schedule(), which
 * in futex_wait happens strictly after the waiter is on the futex hash
 * bucket. Polling until 'S' therefore proves the waiter is queued; the poll
 * delay is bounded retry, not a synchronization assumption. */
static int wait_until_task_blocked(pid_t pid, pid_t tid) {
    const struct timespec delay = {.tv_sec = 0, .tv_nsec = 1000000};
    for (unsigned int attempt = 0; attempt < BLOCK_POLL_ATTEMPTS; ++attempt) {
        int state = task_state(pid, tid);
        if (state == 'S') {
            return 0;
        }
        if (state < 0) {
            return -1;
        }
        (void)nanosleep(&delay, NULL);
    }
    return -1;
}

static int wait_until_waiter_blocked(const struct waiter *entry) {
    const struct timespec delay = {.tv_sec = 0, .tv_nsec = 1000000};
    pid_t tid = 0;
    for (unsigned int attempt = 0; attempt < BLOCK_POLL_ATTEMPTS; ++attempt) {
        tid = atomic_load_explicit(&entry->tid, memory_order_acquire);
        if (tid > 0) {
            break;
        }
        (void)nanosleep(&delay, NULL);
    }
    if (tid <= 0) {
        return -1;
    }
    return wait_until_task_blocked(getpid(), tid);
}

static void *waiter_main(void *argument) {
    struct waiter *entry = argument;
    atomic_store_explicit(&entry->tid, (pid_t)syscall(SYS_gettid),
                          memory_order_release);
    errno = 0;
    entry->result = sys_futex(entry->uaddr, entry->op, entry->expected,
                               entry->timeout, NULL, entry->bitset);
    entry->saved_errno = errno;
    atomic_store_explicit(&entry->done, 1, memory_order_release);
    return NULL;
}

static int start_waiter_with_timeout(struct waiter *entry, uint32_t *uaddr,
                                     int op, uint32_t expected,
                                     uint32_t bitset,
                                     const struct timespec *timeout) {
    memset(entry, 0, sizeof(*entry));
    entry->uaddr = uaddr;
    entry->op = op;
    entry->expected = expected;
    entry->bitset = bitset;
    entry->timeout = timeout;
    if (pthread_create(&entry->thread, NULL, waiter_main, entry) != 0) {
        return fail("waiter-create");
    }
    return 0;
}

static int start_waiter(struct waiter *entry, uint32_t *uaddr, int op,
                        uint32_t expected, uint32_t bitset) {
    return start_waiter_with_timeout(entry, uaddr, op, expected, bitset, NULL);
}

static unsigned int done_count(const struct waiter *waiters, unsigned int n) {
    unsigned int count = 0;
    for (unsigned int index = 0; index < n; ++index) {
        if (atomic_load_explicit(&waiters[index].done,
                                 memory_order_acquire) != 0) {
            ++count;
        }
    }
    return count;
}

static int wait_for_done_count(const struct waiter *waiters, unsigned int n,
                               unsigned int expected) {
    const struct timespec delay = {.tv_sec = 0, .tv_nsec = 1000000};
    for (unsigned int attempt = 0; attempt < BLOCK_POLL_ATTEMPTS; ++attempt) {
        if (done_count(waiters, n) == expected) {
            return 0;
        }
        (void)nanosleep(&delay, NULL);
    }
    return -1;
}

static int join_waiters(struct waiter *waiters, unsigned int n,
                        const char *stage) {
    int failed = 0;
    for (unsigned int index = 0; index < n; ++index) {
        if (pthread_join(waiters[index].thread, NULL) != 0) {
            failed = 1;
        }
    }
    if (failed) {
        errno = EPROTO;
        return fail(stage);
    }
    return 0;
}

/* Best-effort fail-path cleanup: wake every waiter on both possible words in
 * both scopes, then join, so no blocked thread outlives the test. */
static void abandon_waiters(struct waiter *waiters, unsigned int n,
                            uint32_t *uaddr, uint32_t *uaddr2) {
    (void)sys_futex(uaddr, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, INT_MAX, NULL,
                    NULL, 0);
    (void)sys_futex(uaddr, FUTEX_WAKE, INT_MAX, NULL, NULL, 0);
    if (uaddr2 != NULL) {
        (void)sys_futex(uaddr2, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, INT_MAX,
                        NULL, NULL, 0);
        (void)sys_futex(uaddr2, FUTEX_WAKE, INT_MAX, NULL, NULL, 0);
    }
    for (unsigned int index = 0; index < n; ++index) {
        (void)pthread_join(waiters[index].thread, NULL);
    }
}

static int create_memfd_file(size_t size) {
#ifndef SYS_memfd_create
#define SYS_memfd_create 319
#endif
    int fd = (int)syscall(SYS_memfd_create, "thekernel-futex", MFD_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    if (ftruncate(fd, (off_t)size) != 0) {
        int saved_errno = errno;
        close(fd);
        errno = saved_errno;
        return -1;
    }
    return fd;
}

static int test_wait_eagain(void) {
    static uint32_t word;
    const struct timespec timeout = {.tv_sec = 1, .tv_nsec = 0};
    struct timespec start;
    struct timespec end;

    word = 0;
    if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
        return fail("wait-eagain-clock");
    }
    errno = 0;
    long result = sys_futex(&word, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 1,
                            &timeout, NULL, 0);
    if (result != -1 || errno != EAGAIN) {
        return fail_value("wait-eagain-result", result, -1);
    }
    if (clock_gettime(CLOCK_MONOTONIC, &end) != 0) {
        return fail("wait-eagain-clock-end");
    }
    long spent = elapsed_ms(&start, &end);
    if (spent >= EAGAIN_LATENCY_MAX_MS) {
        errno = ETIME;
        return fail_value("wait-eagain-latency", spent,
                          EAGAIN_LATENCY_MAX_MS);
    }
    marker("THEKERNEL_FUTEX_WAIT_EAGAIN_OK");
    return 0;
}

static int test_wait_timeout(void) {
    static uint32_t word;
    const struct timespec timeout = {.tv_sec = 0,
                                     .tv_nsec = WAIT_TIMEOUT_NS};
    struct timespec start;
    struct timespec end;

    word = 7;
    if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
        return fail("wait-timeout-clock");
    }
    errno = 0;
    long result = sys_futex(&word, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 7,
                            &timeout, NULL, 0);
    if (result != -1 || errno != ETIMEDOUT) {
        return fail_value("wait-timeout-result", result, -1);
    }
    if (clock_gettime(CLOCK_MONOTONIC, &end) != 0) {
        return fail("wait-timeout-clock-end");
    }
    long spent = elapsed_ms(&start, &end);
    printf("THEKERNEL_FUTEX_WAIT_TIMEOUT_BOUNDARY elapsed_ms=%ld\n", spent);
    fflush(stdout);
    if (spent < WAIT_TIMEOUT_MIN_MS || spent > WAIT_TIMEOUT_MAX_MS) {
        errno = ETIME;
        return fail_value("wait-timeout-elapsed", spent,
                          WAIT_TIMEOUT_MIN_MS);
    }
    marker("THEKERNEL_FUTEX_WAIT_TIMEOUT_OK");
    return 0;
}

static int test_wake_count(void) {
    static uint32_t word;
    struct waiter waiters[WAKE_COUNT_WAITERS];

    word = 0;
    errno = 0;
    long result = sys_futex(&word, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, INT_MAX,
                            NULL, NULL, 0);
    if (result != 0) {
        return fail_value("wake-count-empty", result, 0);
    }

    for (unsigned int index = 0; index < WAKE_COUNT_WAITERS; ++index) {
        if (start_waiter(&waiters[index], &word,
                         FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 0, 0)) {
            abandon_waiters(waiters, index, &word, NULL);
            return 1;
        }
    }
    for (unsigned int index = 0; index < WAKE_COUNT_WAITERS; ++index) {
        if (wait_until_waiter_blocked(&waiters[index]) != 0) {
            errno = ETIME;
            abandon_waiters(waiters, WAKE_COUNT_WAITERS, &word, NULL);
            return fail("wake-count-block-handshake");
        }
    }

    errno = 0;
    result = sys_futex(&word, FUTEX_WAKE | FUTEX_PRIVATE_FLAG,
                       WAKE_COUNT_FIRST_BATCH, NULL, NULL, 0);
    if (result != (long)WAKE_COUNT_FIRST_BATCH) {
        abandon_waiters(waiters, WAKE_COUNT_WAITERS, &word, NULL);
        return fail_value("wake-count-first-batch", result,
                          (long)WAKE_COUNT_FIRST_BATCH);
    }
    if (wait_for_done_count(waiters, WAKE_COUNT_WAITERS,
                            WAKE_COUNT_FIRST_BATCH) != 0) {
        errno = ETIME;
        abandon_waiters(waiters, WAKE_COUNT_WAITERS, &word, NULL);
        return fail("wake-count-first-batch-done");
    }

    errno = 0;
    result = sys_futex(&word, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, INT_MAX, NULL,
                       NULL, 0);
    if (result != (long)(WAKE_COUNT_WAITERS - WAKE_COUNT_FIRST_BATCH)) {
        abandon_waiters(waiters, WAKE_COUNT_WAITERS, &word, NULL);
        return fail_value("wake-count-remainder", result,
                          (long)(WAKE_COUNT_WAITERS -
                                 WAKE_COUNT_FIRST_BATCH));
    }
    if (join_waiters(waiters, WAKE_COUNT_WAITERS, "wake-count-join")) {
        return 1;
    }
    for (unsigned int index = 0; index < WAKE_COUNT_WAITERS; ++index) {
        if (waiters[index].result != 0) {
            errno = waiters[index].saved_errno;
            return fail_value("wake-count-waiter-result",
                              waiters[index].result, 0);
        }
    }
    marker("THEKERNEL_FUTEX_WAKE_COUNT_OK");
    return 0;
}

static int test_shared_alias(void) {
    long page_size_value = sysconf(_SC_PAGESIZE);
    if (page_size_value <= 0) {
        return fail("shared-alias-page-size");
    }
    size_t page_size = (size_t)page_size_value;
    int fd = -1;
    void *wait_mapping = MAP_FAILED;
    void *wake_mapping = MAP_FAILED;
    struct waiter entry;
    int waiter_started = 0;
    int waiter_joined = 0;
    const char *failure = NULL;
    int failure_errno = 0;

    fd = create_memfd_file(page_size);
    if (fd < 0) {
        failure = "shared-alias-file";
        failure_errno = errno;
        goto cleanup;
    }
    wait_mapping = mmap(NULL, page_size, PROT_READ | PROT_WRITE, MAP_SHARED,
                        fd, 0);
    if (wait_mapping == MAP_FAILED) {
        failure = "shared-alias-wait-mmap";
        failure_errno = errno;
        goto cleanup;
    }
    wake_mapping = mmap(NULL, page_size, PROT_READ | PROT_WRITE, MAP_SHARED,
                        fd, 0);
    if (wake_mapping == MAP_FAILED) {
        failure = "shared-alias-wake-mmap";
        failure_errno = errno;
        goto cleanup;
    }
    *(uint32_t *)wait_mapping = 0;

    if (start_waiter(&entry, wait_mapping, FUTEX_WAIT, 0, 0)) {
        failure = "shared-alias-create";
        failure_errno = errno;
        goto cleanup;
    }
    waiter_started = 1;
    if (wait_until_waiter_blocked(&entry) != 0) {
        failure = "shared-alias-block-handshake";
        failure_errno = ETIME;
        goto cleanup;
    }

    *(uint32_t *)wake_mapping = 1;
    errno = 0;
    long wake_count = sys_futex(wake_mapping, FUTEX_WAKE, 1, NULL, NULL, 0);
    if (wake_count != 1) {
        failure = "shared-alias-wake";
        failure_errno = errno != 0 ? errno : EIO;
        goto cleanup;
    }
    if (join_waiters(&entry, 1, "shared-alias-join") != 0) {
        failure = "shared-alias-join";
        failure_errno = errno;
        goto cleanup;
    }
    waiter_joined = 1;
    if (entry.result != 0) {
        failure = "shared-alias-waiter-result";
        failure_errno = entry.saved_errno != 0 ? entry.saved_errno : EIO;
        goto cleanup;
    }

cleanup:
    if (waiter_started && !waiter_joined) {
        void *cleanup_addr = wait_mapping != MAP_FAILED
                                 ? wait_mapping
                                 : wake_mapping;
        if (cleanup_addr != MAP_FAILED) {
            abandon_waiters(&entry, 1, cleanup_addr, wake_mapping);
        } else {
            (void)pthread_join(entry.thread, NULL);
        }
        waiter_joined = 1;
    }
    if (wait_mapping != MAP_FAILED && munmap(wait_mapping, page_size) != 0 &&
        failure == NULL) {
        failure = "shared-alias-wait-munmap";
        failure_errno = errno;
    }
    if (wake_mapping != MAP_FAILED && munmap(wake_mapping, page_size) != 0 &&
        failure == NULL) {
        failure = "shared-alias-wake-munmap";
        failure_errno = errno;
    }
    if (fd >= 0 && close(fd) != 0 && failure == NULL) {
        failure = "shared-alias-close";
        failure_errno = errno;
    }
    if (failure != NULL) {
        errno = failure_errno != 0 ? failure_errno : EIO;
        return fail(failure);
    }
    marker("THEKERNEL_FUTEX_SHARED_ALIAS_OK");
    return 0;
}

static int run_shared_remap_case(size_t page_size, int original_fd,
                                 int replacement_fd, int expected_wake) {
    void *wait_mapping = MAP_FAILED;
    void *cleanup_mapping = MAP_FAILED;
    void *wait_address = NULL;
    struct waiter entry;
    int waiter_started = 0;
    int waiter_joined = 0;
    struct timespec start;
    struct timespec end;
    const struct timespec timeout = {.tv_sec = 0, .tv_nsec = WAIT_TIMEOUT_NS};
    const char *failure = NULL;
    int failure_errno = 0;

    wait_mapping = mmap(NULL, page_size, PROT_READ | PROT_WRITE, MAP_SHARED,
                        original_fd, 0);
    if (wait_mapping == MAP_FAILED) {
        failure = "shared-remap-wait-mmap";
        failure_errno = errno;
        goto cleanup;
    }
    wait_address = wait_mapping;
    cleanup_mapping = mmap(NULL, page_size, PROT_READ | PROT_WRITE,
                           MAP_SHARED, original_fd, 0);
    if (cleanup_mapping == MAP_FAILED) {
        failure = "shared-remap-cleanup-mmap";
        failure_errno = errno;
        goto cleanup;
    }
    *(uint32_t *)wait_mapping = 0;
    if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
        failure = "shared-remap-timeout-clock-start";
        failure_errno = errno;
        goto cleanup;
    }
    if (start_waiter_with_timeout(&entry, wait_mapping, FUTEX_WAIT, 0, 0,
                                  &timeout)) {
        failure = "shared-remap-create";
        failure_errno = errno;
        goto cleanup;
    }
    waiter_started = 1;
    if (wait_until_waiter_blocked(&entry) != 0) {
        failure = "shared-remap-block-handshake";
        failure_errno = ETIME;
        goto cleanup;
    }

    if (munmap(wait_mapping, page_size) != 0) {
        failure = "shared-remap-unmap";
        failure_errno = errno;
        goto cleanup;
    }
    wait_mapping = MAP_FAILED;
    wait_mapping = mmap(wait_address, page_size, PROT_READ | PROT_WRITE,
                        MAP_SHARED | MAP_FIXED, replacement_fd, 0);
    if (wait_mapping == MAP_FAILED || wait_mapping != wait_address) {
        failure = "shared-remap-fixed-mmap";
        failure_errno = errno != 0 ? errno : EIO;
        goto cleanup;
    }
    *(uint32_t *)wait_mapping = 0;
    if (expected_wake == 0 &&
        atomic_load_explicit(&entry.done, memory_order_acquire) != 0) {
        failure = "shared-remap-timeout-before-remap";
        failure_errno = ETIME;
        goto cleanup;
    }
    errno = 0;
    long wake_count = sys_futex(wait_mapping, FUTEX_WAKE, 1, NULL, NULL, 0);
    if (wake_count != expected_wake) {
        failure = "shared-remap-wake-count";
        failure_errno = errno != 0 ? errno : EIO;
        goto cleanup;
    }
    if (join_waiters(&entry, 1, "shared-remap-join") != 0) {
        failure = "shared-remap-join";
        failure_errno = errno;
        goto cleanup;
    }
    waiter_joined = 1;
    if (expected_wake == 0) {
        if (clock_gettime(CLOCK_MONOTONIC, &end) != 0) {
            failure = "shared-remap-timeout-clock-end";
            failure_errno = errno;
            goto cleanup;
        }
        long spent = elapsed_ms(&start, &end);
        if (spent < WAIT_TIMEOUT_MIN_MS || spent > WAIT_TIMEOUT_MAX_MS) {
            failure = "shared-remap-timeout-elapsed";
            failure_errno = ETIME;
            goto cleanup;
        }
    }
    if (expected_wake != 0) {
        if (entry.result != 0) {
            failure = "shared-remap-same-file-result";
            failure_errno = entry.saved_errno != 0 ? entry.saved_errno : EIO;
            goto cleanup;
        }
    } else if (entry.result != -1 || entry.saved_errno != ETIMEDOUT) {
        failure = "shared-remap-different-file-result";
        failure_errno = entry.saved_errno != 0 ? entry.saved_errno : EIO;
        goto cleanup;
    }

cleanup:
    if (waiter_started && !waiter_joined) {
        void *cleanup_addr = cleanup_mapping != MAP_FAILED
                                 ? cleanup_mapping
                                 : wait_mapping;
        if (cleanup_addr != MAP_FAILED) {
            abandon_waiters(&entry, 1, cleanup_addr, wait_mapping);
        } else {
            (void)pthread_join(entry.thread, NULL);
        }
        waiter_joined = 1;
    }
    if (wait_mapping != MAP_FAILED && munmap(wait_mapping, page_size) != 0 &&
        failure == NULL) {
        failure = "shared-remap-wait-munmap";
        failure_errno = errno;
    }
    if (cleanup_mapping != MAP_FAILED &&
        munmap(cleanup_mapping, page_size) != 0 && failure == NULL) {
        failure = "shared-remap-cleanup-munmap";
        failure_errno = errno;
    }
    if (failure != NULL) {
        errno = failure_errno != 0 ? failure_errno : EIO;
        return fail(failure);
    }
    return 0;
}

static int test_shared_remap(void) {
    long page_size_value = sysconf(_SC_PAGESIZE);
    if (page_size_value <= 0) {
        return fail("shared-remap-page-size");
    }
    size_t page_size = (size_t)page_size_value;
    int original_fd = create_memfd_file(page_size);
    if (original_fd < 0) {
        return fail("shared-remap-original-file");
    }
    int replacement_fd = create_memfd_file(page_size);
    if (replacement_fd < 0) {
        int saved_errno = errno;
        close(original_fd);
        errno = saved_errno;
        return fail("shared-remap-replacement-file");
    }

    int result = run_shared_remap_case(page_size, original_fd, replacement_fd,
                                       0);
    if (result == 0) {
        result = run_shared_remap_case(page_size, original_fd, original_fd, 1);
    }
    int saved_errno = errno;
    if (close(original_fd) != 0 && result == 0) {
        result = fail("shared-remap-original-close");
        saved_errno = errno;
    }
    if (close(replacement_fd) != 0 && result == 0) {
        result = fail("shared-remap-replacement-close");
        saved_errno = errno;
    }
    if (result != 0) {
        errno = saved_errno != 0 ? saved_errno : EIO;
        return 1;
    }
    marker("THEKERNEL_FUTEX_SHARED_REMAP_OK");
    return 0;
}

static int test_cmp_requeue(void) {
    static uint32_t source;
    static uint32_t target;
    struct waiter waiters[REQUEUE_WAITERS];

    source = 5;
    target = 0;
    for (unsigned int index = 0; index < REQUEUE_WAITERS; ++index) {
        if (start_waiter(&waiters[index], &source,
                         FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 5, 0)) {
            abandon_waiters(waiters, index, &source, &target);
            return 1;
        }
    }
    for (unsigned int index = 0; index < REQUEUE_WAITERS; ++index) {
        if (wait_until_waiter_blocked(&waiters[index]) != 0) {
            errno = ETIME;
            abandon_waiters(waiters, REQUEUE_WAITERS, &source, &target);
            return fail("cmp-requeue-block-handshake");
        }
    }

    /* val3 mismatch: fail with EAGAIN before waking or requeuing anyone. */
    errno = 0;
    long result = sys_futex(&source, FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG,
                            1, (const struct timespec *)(uintptr_t)INT_MAX,
                            &target, 6);
    if (result != -1 || errno != EAGAIN) {
        abandon_waiters(waiters, REQUEUE_WAITERS, &source, &target);
        return fail_value("cmp-requeue-val3-mismatch", result, -1);
    }
    if (done_count(waiters, REQUEUE_WAITERS) != 0) {
        errno = EPROTO;
        abandon_waiters(waiters, REQUEUE_WAITERS, &source, &target);
        return fail("cmp-requeue-mismatch-side-effect");
    }
    marker("THEKERNEL_FUTEX_CMP_REQUEUE_EAGAIN_OK");

    /* val3 match: wake 1, requeue the other 2, return the combined count. */
    errno = 0;
    result = sys_futex(&source, FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG, 1,
                       (const struct timespec *)(uintptr_t)INT_MAX, &target,
                       5);
    if (result != (long)REQUEUE_WAITERS) {
        abandon_waiters(waiters, REQUEUE_WAITERS, &source, &target);
        return fail_value("cmp-requeue-accounting", result,
                          (long)REQUEUE_WAITERS);
    }
    if (wait_for_done_count(waiters, REQUEUE_WAITERS, 1) != 0) {
        errno = ETIME;
        abandon_waiters(waiters, REQUEUE_WAITERS, &source, &target);
        return fail("cmp-requeue-woken-done");
    }

    /* Requeued waiters now sleep on the target word, not the source. */
    errno = 0;
    result = sys_futex(&source, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, INT_MAX,
                       NULL, NULL, 0);
    if (result != 0) {
        abandon_waiters(waiters, REQUEUE_WAITERS, &source, &target);
        return fail_value("cmp-requeue-source-empty", result, 0);
    }
    errno = 0;
    result = sys_futex(&target, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, INT_MAX,
                       NULL, NULL, 0);
    if (result != (long)(REQUEUE_WAITERS - 1)) {
        abandon_waiters(waiters, REQUEUE_WAITERS, &source, &target);
        return fail_value("cmp-requeue-target-wake", result,
                          (long)(REQUEUE_WAITERS - 1));
    }
    if (join_waiters(waiters, REQUEUE_WAITERS, "cmp-requeue-join")) {
        return 1;
    }
    for (unsigned int index = 0; index < REQUEUE_WAITERS; ++index) {
        if (waiters[index].result != 0) {
            errno = waiters[index].saved_errno;
            return fail_value("cmp-requeue-waiter-result",
                              waiters[index].result, 0);
        }
    }
    marker("THEKERNEL_FUTEX_CMP_REQUEUE_OK");
    return 0;
}

static int test_requeue(void) {
    static uint32_t source;
    static uint32_t target;
    struct waiter waiters[REQUEUE_WAITERS];

    source = 5;
    target = 0;
    for (unsigned int index = 0; index < REQUEUE_WAITERS; ++index) {
        if (start_waiter(&waiters[index], &source,
                         FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 5, 0)) {
            abandon_waiters(waiters, index, &source, &target);
            return 1;
        }
    }
    for (unsigned int index = 0; index < REQUEUE_WAITERS; ++index) {
        if (wait_until_waiter_blocked(&waiters[index]) != 0) {
            errno = ETIME;
            abandon_waiters(waiters, REQUEUE_WAITERS, &source, &target);
            return fail("requeue-block-handshake");
        }
    }

    errno = 0;
    long result = sys_futex(
        &source, FUTEX_REQUEUE | FUTEX_PRIVATE_FLAG, 1,
        (const struct timespec *)(uintptr_t)INT_MAX, &target, 0);
    if (result != (long)REQUEUE_WAITERS) {
        abandon_waiters(waiters, REQUEUE_WAITERS, &source, &target);
        return fail_value("requeue-accounting", result, (long)REQUEUE_WAITERS);
    }
    if (wait_for_done_count(waiters, REQUEUE_WAITERS, 1) != 0) {
        errno = ETIME;
        abandon_waiters(waiters, REQUEUE_WAITERS, &source, &target);
        return fail("requeue-woken-done");
    }

    errno = 0;
    result = sys_futex(&source, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, INT_MAX,
                       NULL, NULL, 0);
    if (result != 0) {
        abandon_waiters(waiters, REQUEUE_WAITERS, &source, &target);
        return fail_value("requeue-source-empty", result, 0);
    }
    errno = 0;
    result = sys_futex(&target, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, INT_MAX,
                       NULL, NULL, 0);
    if (result != (long)(REQUEUE_WAITERS - 1)) {
        abandon_waiters(waiters, REQUEUE_WAITERS, &source, &target);
        return fail_value("requeue-target-wake", result,
                          (long)(REQUEUE_WAITERS - 1));
    }
    if (join_waiters(waiters, REQUEUE_WAITERS, "requeue-join")) {
        return 1;
    }
    for (unsigned int index = 0; index < REQUEUE_WAITERS; ++index) {
        if (waiters[index].result != 0) {
            errno = waiters[index].saved_errno;
            return fail_value("requeue-waiter-result", waiters[index].result, 0);
        }
    }
    marker("THEKERNEL_FUTEX_REQUEUE_OK");
    return 0;
}

static int run_private_mapping_scope_direction(uint32_t *word, int wait_op,
                                               int wrong_wake_op,
                                               int right_wake_op,
                                               const char *stage) {
    struct waiter entry;
    if (start_waiter(&entry, word, wait_op, 0, 0)) {
        return 1;
    }
    if (wait_until_waiter_blocked(&entry) != 0) {
        errno = ETIME;
        abandon_waiters(&entry, 1, word, NULL);
        return fail(stage);
    }

    errno = 0;
    long result = sys_futex(word, wrong_wake_op, INT_MAX, NULL, NULL, 0);
    if (result != 0 || atomic_load_explicit(&entry.done, memory_order_acquire) != 0 ||
        wait_until_waiter_blocked(&entry) != 0) {
        abandon_waiters(&entry, 1, word, NULL);
        return fail_value(stage, result, 0);
    }
    errno = 0;
    result = sys_futex(word, right_wake_op, INT_MAX, NULL, NULL, 0);
    if (result != 1 || join_waiters(&entry, 1, stage) != 0) {
        if (atomic_load_explicit(&entry.done, memory_order_acquire) == 0) {
            abandon_waiters(&entry, 1, word, NULL);
        }
        return fail_value(stage, result, 1);
    }
    if (entry.result != 0) {
        errno = entry.saved_errno;
        return fail_value(stage, entry.result, 0);
    }
    return 0;
}

static int test_private_mapping_scope(void) {
    long page_size_value = sysconf(_SC_PAGESIZE);
    if (page_size_value <= 0) {
        return fail("private-mapping-scope-page-size");
    }
    size_t page_size = (size_t)page_size_value;
    uint32_t *word = mmap(NULL, page_size, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (word == MAP_FAILED) {
        return fail("private-mapping-scope-mmap");
    }
    *word = 0;

    int result = run_private_mapping_scope_direction(
        word, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, FUTEX_WAKE,
        FUTEX_WAKE | FUTEX_PRIVATE_FLAG, "private-mapping-private-wait");
    if (result == 0) {
        result = run_private_mapping_scope_direction(
            word, FUTEX_WAIT, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, FUTEX_WAKE,
            "private-mapping-shared-wait");
    }
    int saved_errno = errno;
    if (munmap(word, page_size) != 0 && result == 0) {
        result = fail("private-mapping-scope-munmap");
        saved_errno = errno;
    }
    if (result != 0) {
        errno = saved_errno != 0 ? saved_errno : EIO;
        return 1;
    }
    marker("THEKERNEL_FUTEX_PRIVATE_MAPPING_SCOPE_OK");
    return 0;
}

static int test_wait_bitset(void) {
    static uint32_t word;
    struct waiter entry;

    word = 0;
    errno = 0;
    long result = sys_futex(&word, FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG, 0,
                            NULL, NULL, 0);
    if (result != -1 || errno != EINVAL) {
        return fail_value("bitset-zero-mask", result, -1);
    }

    if (start_waiter(&entry, &word, FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG,
                     0, 0x1U)) {
        return 1;
    }
    if (wait_until_waiter_blocked(&entry) != 0) {
        errno = ETIME;
        abandon_waiters(&entry, 1, &word, NULL);
        return fail("bitset-block-handshake");
    }

    /* Wake mask 0x2 does not intersect wait mask 0x1: nobody wakes. */
    errno = 0;
    result = sys_futex(&word, FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG,
                       INT_MAX, NULL, NULL, 0x2U);
    if (result != 0) {
        abandon_waiters(&entry, 1, &word, NULL);
        return fail_value("bitset-disjoint-wake", result, 0);
    }
    if (atomic_load_explicit(&entry.done, memory_order_acquire) != 0 ||
        wait_until_waiter_blocked(&entry) != 0) {
        errno = EPROTO;
        abandon_waiters(&entry, 1, &word, NULL);
        return fail("bitset-disjoint-still-blocked");
    }
    marker("THEKERNEL_FUTEX_BITSET_NO_INTERSECT_OK");

    /* Mask 0x3 intersects 0x1: exactly one waiter wakes. */
    errno = 0;
    result = sys_futex(&word, FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG,
                       INT_MAX, NULL, NULL, 0x3U);
    if (result != 1) {
        abandon_waiters(&entry, 1, &word, NULL);
        return fail_value("bitset-intersect-wake", result, 1);
    }
    if (join_waiters(&entry, 1, "bitset-join")) {
        return 1;
    }
    if (entry.result != 0) {
        errno = entry.saved_errno;
        return fail_value("bitset-waiter-result", entry.result, 0);
    }
    marker("THEKERNEL_FUTEX_BITSET_OK");
    return 0;
}

static int test_wait_bitset_realtime(void) {
    static uint32_t word;
    struct timespec now;
    struct timespec past;
    struct timespec future;
    struct timespec start;
    struct timespec end;

    word = 0;
    if (clock_gettime(CLOCK_REALTIME, &now) != 0) {
        return fail("bitset-realtime-clock");
    }
    past = now;
    past.tv_sec--;
    errno = 0;
    long result = sys_futex(
        &word, FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME,
        0, &past, NULL, FUTEX_BITSET_MATCH_ANY);
    if (result != -1 || errno != ETIMEDOUT) {
        return fail_value("bitset-realtime-past", result, -1);
    }

    future = now;
    future.tv_nsec += 50000000L;
    if (future.tv_nsec >= 1000000000L) {
        future.tv_sec++;
        future.tv_nsec -= 1000000000L;
    }
    if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
        return fail("bitset-realtime-future-start");
    }
    errno = 0;
    result = sys_futex(
        &word, FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME,
        0, &future, NULL, FUTEX_BITSET_MATCH_ANY);
    int observed_errno = errno;
    if (clock_gettime(CLOCK_MONOTONIC, &end) != 0) {
        return fail("bitset-realtime-future-end");
    }
    long spent = elapsed_ms(&start, &end);
    if (result != -1 || observed_errno != ETIMEDOUT || spent < 40L ||
        spent > WAIT_TIMEOUT_MAX_MS) {
        errno = observed_errno != 0 ? observed_errno : ETIME;
        return fail_value("bitset-realtime-future", spent, 50);
    }
    marker("THEKERNEL_FUTEX_WAIT_BITSET_REALTIME_OK");
    return 0;
}

static int test_alignment_and_fault(void) {
    static uint32_t aligned_words[2];
    uint32_t *unaligned =
        (uint32_t *)((char *)&aligned_words[0] + 1);
    const struct timespec timeout = {.tv_sec = 0, .tv_nsec = 1000000};

    errno = 0;
    long result = sys_futex(unaligned, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 0,
                            &timeout, NULL, 0);
    if (result != -1 || errno != EINVAL) {
        return fail_value("unaligned-wait", result, -1);
    }
    errno = 0;
    result = sys_futex(unaligned, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, INT_MAX,
                       NULL, NULL, 0);
    if (result != -1 || errno != EINVAL) {
        return fail_value("unaligned-wake", result, -1);
    }
    marker("THEKERNEL_FUTEX_UNALIGNED_EINVAL_OK");

    long page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0) {
        return fail("fault-page-size");
    }
    void *page = mmap(NULL, (size_t)page_size, PROT_NONE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED) {
        return fail("fault-mmap");
    }
    uint32_t *fault_word = page;

    errno = 0;
    result = sys_futex(fault_word, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 0,
                       &timeout, NULL, 0);
    if (result != -1 || errno != EFAULT) {
        (void)munmap(page, (size_t)page_size);
        return fail_value("fault-wait", result, -1);
    }
    /* A private wake never dereferences uaddr (the key is mm+address), so it
     * succeeds on an unreadable page and reports zero waiters. */
    errno = 0;
    result = sys_futex(fault_word, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, INT_MAX,
                       NULL, NULL, 0);
    if (result != 0 || errno != 0) {
        (void)munmap(page, (size_t)page_size);
        return fail_value("fault-private-wake", result, 0);
    }
    /* A shared wake must resolve the backing page for its key: EFAULT. */
    errno = 0;
    result = sys_futex(fault_word, FUTEX_WAKE, INT_MAX, NULL, NULL, 0);
    if (result != -1 || errno != EFAULT) {
        (void)munmap(page, (size_t)page_size);
        return fail_value("fault-shared-wake", result, -1);
    }
    if (munmap(page, (size_t)page_size) != 0) {
        return fail("fault-munmap");
    }
    marker("THEKERNEL_FUTEX_EFAULT_OK");
    return 0;
}

static int test_private_scope(void) {
    static uint32_t word;
    struct waiter entry;

    /* In-process: a shared-namespace wake must not match a private waiter,
     * even though both name the same anonymous word. */
    word = 0;
    if (start_waiter(&entry, &word, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 0, 0)) {
        return 1;
    }
    if (wait_until_waiter_blocked(&entry) != 0) {
        errno = ETIME;
        abandon_waiters(&entry, 1, &word, NULL);
        return fail("private-scope-block-handshake");
    }
    errno = 0;
    long result = sys_futex(&word, FUTEX_WAKE, INT_MAX, NULL, NULL, 0);
    if (result != 0) {
        abandon_waiters(&entry, 1, &word, NULL);
        return fail_value("private-scope-shared-wake", result, 0);
    }
    if (atomic_load_explicit(&entry.done, memory_order_acquire) != 0 ||
        wait_until_waiter_blocked(&entry) != 0) {
        errno = EPROTO;
        abandon_waiters(&entry, 1, &word, NULL);
        return fail("private-scope-still-blocked");
    }
    errno = 0;
    result = sys_futex(&word, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, INT_MAX, NULL,
                       NULL, 0);
    if (result != 1) {
        abandon_waiters(&entry, 1, &word, NULL);
        return fail_value("private-scope-private-wake", result, 1);
    }
    if (join_waiters(&entry, 1, "private-scope-join")) {
        return 1;
    }
    if (entry.result != 0) {
        errno = entry.saved_errno;
        return fail_value("private-scope-waiter-result", entry.result, 0);
    }

    /* Cross-process: the child sleeps with a SHARED wait on a MAP_SHARED
     * word; the parent's private wake hashes with the parent's mm and can
     * never reach it, while the shared wake releases it. */
    long page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0) {
        return fail("private-scope-page-size");
    }
    uint32_t *shared_word =
        mmap(NULL, (size_t)page_size, PROT_READ | PROT_WRITE,
             MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared_word == MAP_FAILED) {
        return fail("private-scope-mmap");
    }
    *shared_word = 0;

    pid_t child = fork();
    if (child < 0) {
        (void)munmap(shared_word, (size_t)page_size);
        return fail("private-scope-fork");
    }
    if (child == 0) {
        errno = 0;
        long wait_result =
            sys_futex(shared_word, FUTEX_WAIT, 0, NULL, NULL, 0);
        _exit(wait_result == 0 ? 0 : 1);
    }
    if (wait_until_task_blocked(child, child) != 0) {
        errno = ETIME;
        (void)sys_futex(shared_word, FUTEX_WAKE, INT_MAX, NULL, NULL, 0);
        (void)waitpid(child, NULL, 0);
        (void)munmap(shared_word, (size_t)page_size);
        return fail("private-scope-child-handshake");
    }
    errno = 0;
    result = sys_futex(shared_word, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, INT_MAX,
                       NULL, NULL, 0);
    if (result != 0 || wait_until_task_blocked(child, child) != 0) {
        (void)sys_futex(shared_word, FUTEX_WAKE, INT_MAX, NULL, NULL, 0);
        (void)waitpid(child, NULL, 0);
        (void)munmap(shared_word, (size_t)page_size);
        return fail_value("private-scope-cross-private-wake", result, 0);
    }
    errno = 0;
    result = sys_futex(shared_word, FUTEX_WAKE, INT_MAX, NULL, NULL, 0);
    int status = 0;
    if (result != 1 || waitpid(child, &status, 0) != child ||
        !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        (void)munmap(shared_word, (size_t)page_size);
        return fail_value("private-scope-cross-shared-wake", result, 1);
    }
    if (munmap(shared_word, (size_t)page_size) != 0) {
        return fail("private-scope-munmap");
    }
    marker("THEKERNEL_FUTEX_PRIVATE_SCOPE_OK");
    return 0;
}

int main(int argc, char **argv) {
    (void)argv;
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    if (argc != 1) {
        errno = EINVAL;
        return fail("unknown-option");
    }

    if (test_wait_eagain() || test_wait_timeout() || test_wake_count() ||
        test_shared_alias() || test_shared_remap() || test_cmp_requeue() ||
        test_requeue() || test_wait_bitset() || test_wait_bitset_realtime() ||
        test_alignment_and_fault() || test_private_scope() ||
        test_private_mapping_scope()) {
        return 1;
    }

    marker("THEKERNEL_FUTEX_OK");
    return 0;
}
