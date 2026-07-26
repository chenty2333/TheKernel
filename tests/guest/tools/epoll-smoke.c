#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#ifndef EPOLLEXCLUSIVE
#define EPOLLEXCLUSIVE (1U << 28)
#endif

#define EXCLUSIVE_WAITERS 4
#define EXCLUSIVE_SHARED_TAG 1U
#define EXCLUSIVE_RELEASE_TAG 2U
#define TIMEOUT_EXPIRY_MS 50
#define BOUNDED_WAIT_MS 2000
#define EXCLUSIVE_WAIT_MS 10000

static const char *self_path;

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_EPOLL_FAIL %s errno=%d (%s)\n", stage, errno,
            strerror(errno));
    return 1;
}

static int fail_value(const char *stage, long actual, long expected) {
    fprintf(stderr,
            "THEKERNEL_EPOLL_FAIL %s actual=%ld expected=%ld errno=%d (%s)\n",
            stage, actual, expected, errno, strerror(errno));
    return 1;
}

static void marker(const char *value) {
    puts(value);
    fflush(stdout);
}

static int ctl_add(int epoll_fd, int fd, uint32_t events, uint64_t data,
                   const char *stage) {
    struct epoll_event event = {.events = events};
    event.data.u64 = data;
    if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, fd, &event) != 0) {
        return fail(stage);
    }
    return 0;
}

static int ctl_expect_errno(int epoll_fd, int op, int fd,
                            struct epoll_event *event, int expected_errno,
                            const char *stage) {
    errno = 0;
    long result = epoll_ctl(epoll_fd, op, fd, event);
    if (result != -1 || errno != expected_errno) {
        return fail_value(stage, result, -1);
    }
    return 0;
}

static int wait_count(int epoll_fd, int timeout_ms, int expected,
                      const char *stage) {
    struct epoll_event events[2];
    errno = 0;
    int count = epoll_wait(epoll_fd, events, 2, timeout_ms);
    if (count != expected) {
        return fail_value(stage, count, expected);
    }
    return 0;
}

static int wait_single(int epoll_fd, int timeout_ms, uint32_t expected_events,
                       uint64_t expected_data, const char *stage) {
    struct epoll_event event;
    memset(&event, 0, sizeof(event));
    errno = 0;
    int count = epoll_wait(epoll_fd, &event, 1, timeout_ms);
    if (count != 1) {
        return fail_value(stage, count, 1);
    }
    if (event.events != expected_events) {
        return fail_value(stage, (long)event.events, (long)expected_events);
    }
    if (event.data.u64 != expected_data) {
        return fail_value(stage, (long)event.data.u64, (long)expected_data);
    }
    return 0;
}

static int test_level_vs_edge(void) {
    int pipe_fds[2];
    if (pipe2(pipe_fds, O_NONBLOCK | O_CLOEXEC) != 0) {
        return fail("lt-et-pipe");
    }
    int level_fd = epoll_create1(EPOLL_CLOEXEC);
    int edge_fd = epoll_create1(EPOLL_CLOEXEC);
    if (level_fd < 0 || edge_fd < 0) {
        return fail("lt-et-create");
    }
    if (ctl_add(level_fd, pipe_fds[0], EPOLLIN, 0xA1, "lt-add") ||
        ctl_add(edge_fd, pipe_fds[0], EPOLLIN | EPOLLET, 0xE1, "et-add")) {
        return 1;
    }
    if (wait_count(level_fd, 0, 0, "lt-empty") ||
        wait_count(edge_fd, 0, 0, "et-empty")) {
        return 1;
    }
    if (write(pipe_fds[1], "x", 1) != 1) {
        return fail("lt-et-write");
    }
    /* Level-triggered readiness re-reports on every wait while unread. */
    if (wait_single(level_fd, BOUNDED_WAIT_MS, EPOLLIN, 0xA1, "lt-first") ||
        wait_single(level_fd, 0, EPOLLIN, 0xA1, "lt-relevel")) {
        return 1;
    }
    marker("THEKERNEL_EPOLL_LT_RELEVEL_OK");
    /* Edge-triggered readiness reports exactly once for one edge. */
    if (wait_single(edge_fd, BOUNDED_WAIT_MS, EPOLLIN, 0xE1, "et-first") ||
        wait_count(edge_fd, 0, 0, "et-once")) {
        return 1;
    }
    marker("THEKERNEL_EPOLL_ET_ONCE_OK");
    /* A fresh write produces a fresh edge; level view is unaffected. */
    if (write(pipe_fds[1], "y", 1) != 1) {
        return fail("et-rearm-write");
    }
    if (wait_single(edge_fd, BOUNDED_WAIT_MS, EPOLLIN, 0xE1, "et-rearm") ||
        wait_single(level_fd, 0, EPOLLIN, 0xA1, "lt-still-level")) {
        return 1;
    }
    marker("THEKERNEL_EPOLL_ET_REARM_OK");
    close(level_fd);
    close(edge_fd);
    close(pipe_fds[0]);
    close(pipe_fds[1]);
    return 0;
}

static int test_et_partial_read(void) {
    int pipe_fds[2];
    if (pipe2(pipe_fds, O_NONBLOCK | O_CLOEXEC) != 0) {
        return fail("et-partial-pipe");
    }
    int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
    if (epoll_fd < 0 ||
        ctl_add(epoll_fd, pipe_fds[0], EPOLLIN | EPOLLET, 0xE2,
                "et-partial-add")) {
        return 1;
    }
    if (write(pipe_fds[1], "abcdefgh", 8) != 8) {
        return fail("et-partial-write");
    }
    if (wait_single(epoll_fd, BOUNDED_WAIT_MS, EPOLLIN, 0xE2,
                    "et-partial-first")) {
        return 1;
    }
    char buffer[4];
    if (read(pipe_fds[0], buffer, sizeof(buffer)) != (ssize_t)sizeof(buffer)) {
        return fail("et-partial-read");
    }
    /* Four bytes remain unread, but without a new edge ET stays silent. */
    if (wait_count(epoll_fd, 0, 0, "et-partial-silent")) {
        return 1;
    }
    if (write(pipe_fds[1], "wxyz", 4) != 4) {
        return fail("et-partial-rewrite");
    }
    if (wait_single(epoll_fd, BOUNDED_WAIT_MS, EPOLLIN, 0xE2,
                    "et-partial-fresh-data")) {
        return 1;
    }
    marker("THEKERNEL_EPOLL_ET_PARTIAL_READ_OK");
    close(epoll_fd);
    close(pipe_fds[0]);
    close(pipe_fds[1]);
    return 0;
}

static int test_oneshot(void) {
    int pipe_fds[2];
    if (pipe2(pipe_fds, O_NONBLOCK | O_CLOEXEC) != 0) {
        return fail("oneshot-pipe");
    }
    int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
    if (epoll_fd < 0 ||
        ctl_add(epoll_fd, pipe_fds[0], EPOLLIN | EPOLLONESHOT, 0x05,
                "oneshot-add")) {
        return 1;
    }
    if (write(pipe_fds[1], "a", 1) != 1) {
        return fail("oneshot-write");
    }
    if (wait_single(epoll_fd, BOUNDED_WAIT_MS, EPOLLIN, 0x05,
                    "oneshot-first")) {
        return 1;
    }
    /* Disarmed: neither the unread byte nor fresh data may report. */
    if (wait_count(epoll_fd, 0, 0, "oneshot-silent")) {
        return 1;
    }
    if (write(pipe_fds[1], "b", 1) != 1) {
        return fail("oneshot-second-write");
    }
    if (wait_count(epoll_fd, 0, 0, "oneshot-silent-after-write")) {
        return 1;
    }
    marker("THEKERNEL_EPOLL_ONESHOT_OK");
    struct epoll_event rearm = {.events = EPOLLIN | EPOLLONESHOT};
    rearm.data.u64 = 0x05;
    if (epoll_ctl(epoll_fd, EPOLL_CTL_MOD, pipe_fds[0], &rearm) != 0) {
        return fail("oneshot-mod");
    }
    if (wait_single(epoll_fd, BOUNDED_WAIT_MS, EPOLLIN, 0x05,
                    "oneshot-rearmed")) {
        return 1;
    }
    marker("THEKERNEL_EPOLL_ONESHOT_REARM_OK");
    close(epoll_fd);
    close(pipe_fds[0]);
    close(pipe_fds[1]);
    return 0;
}

static int test_ctl_errors(void) {
    int pipe_fds[2];
    if (pipe2(pipe_fds, O_NONBLOCK | O_CLOEXEC) != 0) {
        return fail("ctl-pipe");
    }
    int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
    if (epoll_fd < 0 ||
        ctl_add(epoll_fd, pipe_fds[0], EPOLLIN, 0x0C, "ctl-add")) {
        return 1;
    }
    struct epoll_event event = {.events = EPOLLIN};
    event.data.u64 = 0x0C;
    if (ctl_expect_errno(epoll_fd, EPOLL_CTL_ADD, pipe_fds[0], &event, EEXIST,
                         "ctl-add-duplicate")) {
        return 1;
    }
    marker("THEKERNEL_EPOLL_CTL_EEXIST_OK");
    if (ctl_expect_errno(epoll_fd, EPOLL_CTL_MOD, pipe_fds[1], &event, ENOENT,
                         "ctl-mod-absent") ||
        ctl_expect_errno(epoll_fd, EPOLL_CTL_DEL, pipe_fds[1], &event, ENOENT,
                         "ctl-del-absent")) {
        return 1;
    }
    marker("THEKERNEL_EPOLL_CTL_ENOENT_OK");
    if (ctl_expect_errno(epoll_fd, EPOLL_CTL_ADD, epoll_fd, &event, EINVAL,
                         "ctl-add-self")) {
        return 1;
    }
    marker("THEKERNEL_EPOLL_CTL_SELF_EINVAL_OK");
    int file_fd = open(self_path, O_RDONLY | O_CLOEXEC);
    if (file_fd < 0) {
        return fail("ctl-regular-open");
    }
    if (ctl_expect_errno(epoll_fd, EPOLL_CTL_ADD, file_fd, &event, EPERM,
                         "ctl-add-regular-file")) {
        return 1;
    }
    marker("THEKERNEL_EPOLL_CTL_REGULAR_FILE_EPERM_OK");
    close(file_fd);
    close(epoll_fd);
    close(pipe_fds[0]);
    close(pipe_fds[1]);
    return 0;
}

static int test_hup(void) {
    int pipe_fds[2];
    if (pipe2(pipe_fds, O_NONBLOCK | O_CLOEXEC) != 0) {
        return fail("hup-pipe");
    }
    int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
    /* Request NO events: EPOLLHUP must still be delivered. */
    if (epoll_fd < 0 || ctl_add(epoll_fd, pipe_fds[0], 0, 0xB0, "hup-add")) {
        return 1;
    }
    if (wait_count(epoll_fd, 0, 0, "hup-before-close")) {
        return 1;
    }
    if (close(pipe_fds[1]) != 0) {
        return fail("hup-close-writer");
    }
    /* Empty pipe + no writers: the mask is exactly EPOLLHUP. */
    if (wait_single(epoll_fd, BOUNDED_WAIT_MS, EPOLLHUP, 0xB0,
                    "hup-after-close")) {
        return 1;
    }
    marker("THEKERNEL_EPOLL_HUP_OK");
    marker("THEKERNEL_EPOLL_HUP_UNREQUESTED_OK");
    close(epoll_fd);
    close(pipe_fds[0]);
    return 0;
}

static long elapsed_ns(const struct timespec *start,
                       const struct timespec *end) {
    return (end->tv_sec - start->tv_sec) * 1000000000L +
           (end->tv_nsec - start->tv_nsec);
}

static int test_timeouts(void) {
    int pipe_fds[2];
    if (pipe2(pipe_fds, O_NONBLOCK | O_CLOEXEC) != 0) {
        return fail("timeout-pipe");
    }
    int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
    if (epoll_fd < 0 ||
        ctl_add(epoll_fd, pipe_fds[0], EPOLLIN, 0x70, "timeout-add")) {
        return 1;
    }
    /* timeout=0 never blocks: it polls and returns 0 with nothing ready. */
    if (wait_count(epoll_fd, 0, 0, "timeout-zero-idle")) {
        return 1;
    }
    marker("THEKERNEL_EPOLL_TIMEOUT_ZERO_OK");
    struct timespec start;
    struct timespec end;
    if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
        return fail("timeout-clock-start");
    }
    if (wait_count(epoll_fd, TIMEOUT_EXPIRY_MS, 0, "timeout-expiry-return")) {
        return 1;
    }
    if (clock_gettime(CLOCK_MONOTONIC, &end) != 0) {
        return fail("timeout-clock-end");
    }
    long waited_ns = elapsed_ns(&start, &end);
    if (waited_ns < (long)TIMEOUT_EXPIRY_MS * 1000000L) {
        return fail_value("timeout-expiry-early", waited_ns,
                          (long)TIMEOUT_EXPIRY_MS * 1000000L);
    }
    marker("THEKERNEL_EPOLL_TIMEOUT_EXPIRY_OK");
    close(epoll_fd);
    close(pipe_fds[0]);
    close(pipe_fds[1]);
    return 0;
}

static int test_nested(void) {
    int pipe_fds[2];
    if (pipe2(pipe_fds, O_NONBLOCK | O_CLOEXEC) != 0) {
        return fail("nested-pipe");
    }
    int inner_fd = epoll_create1(EPOLL_CLOEXEC);
    int outer_fd = epoll_create1(EPOLL_CLOEXEC);
    if (inner_fd < 0 || outer_fd < 0) {
        return fail("nested-create");
    }
    if (ctl_add(inner_fd, pipe_fds[0], EPOLLIN, 0x11, "nested-inner-add") ||
        ctl_add(outer_fd, inner_fd, EPOLLIN, 0x22, "nested-outer-add")) {
        return 1;
    }
    if (wait_count(outer_fd, 0, 0, "nested-idle")) {
        return 1;
    }
    if (write(pipe_fds[1], "n", 1) != 1) {
        return fail("nested-write");
    }
    /* Readiness must propagate: pipe -> inner instance -> outer instance. */
    if (wait_single(outer_fd, BOUNDED_WAIT_MS, EPOLLIN, 0x22,
                    "nested-outer-ready") ||
        wait_single(inner_fd, 0, EPOLLIN, 0x11, "nested-inner-ready")) {
        return 1;
    }
    marker("THEKERNEL_EPOLL_NESTED_OK");
    close(outer_fd);
    close(inner_fd);
    close(pipe_fds[0]);
    close(pipe_fds[1]);
    return 0;
}

struct exclusive_waiter {
    int epoll_fd;
    _Atomic pid_t tid;
    int wait_result;
    int saw_shared;
    int saw_release;
};

static struct exclusive_waiter exclusive_waiters[EXCLUSIVE_WAITERS];
static int exclusive_shared_efd = -1;
static int exclusive_winner_pipe[2] = {-1, -1};

static void *exclusive_waiter_main(void *argument) {
    struct exclusive_waiter *self = argument;
    atomic_store_explicit(&self->tid, (pid_t)syscall(SYS_gettid),
                          memory_order_release);
    struct epoll_event events[2];
    int count = epoll_wait(self->epoll_fd, events, 2, EXCLUSIVE_WAIT_MS);
    self->wait_result = count;
    for (int index = 0; index < count; ++index) {
        if (events[index].data.u32 == EXCLUSIVE_SHARED_TAG) {
            self->saw_shared = 1;
        }
        if (events[index].data.u32 == EXCLUSIVE_RELEASE_TAG) {
            self->saw_release = 1;
        }
    }
    if (self->saw_shared) {
        /* Drain the level source BEFORE signaling so late waiters woken by
         * the release write cannot also observe the shared fd as ready.
         * EAGAIN is fine: a concurrent winner already drained it. */
        uint64_t value = 0;
        (void)read(exclusive_shared_efd, &value, sizeof(value));
        if (write(exclusive_winner_pipe[1], "W", 1) != 1) {
            return (void *)(uintptr_t)1;
        }
    }
    return NULL;
}

static int thread_is_blocked(pid_t tid) {
    char path[64];
    char buffer[512];
    int length = snprintf(path, sizeof(path), "/proc/self/task/%ld/stat",
                          (long)tid);
    if (length <= 0 || (size_t)length >= sizeof(path)) {
        return 0;
    }
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return 0;
    }
    ssize_t count = read(fd, buffer, sizeof(buffer) - 1);
    close(fd);
    if (count <= 0) {
        return 0;
    }
    buffer[count] = '\0';
    const char *state = strrchr(buffer, ')');
    if (state == NULL || state[1] != ' ') {
        return 0;
    }
    return state[2] == 'S';
}

static int test_exclusive(void) {
    exclusive_shared_efd = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    int release_efd = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    if (exclusive_shared_efd < 0 || release_efd < 0) {
        return fail("exclusive-eventfd");
    }
    if (pipe2(exclusive_winner_pipe, O_CLOEXEC) != 0) {
        return fail("exclusive-winner-pipe");
    }
    for (unsigned int index = 0; index < EXCLUSIVE_WAITERS; ++index) {
        struct exclusive_waiter *waiter = &exclusive_waiters[index];
        waiter->epoll_fd = epoll_create1(EPOLL_CLOEXEC);
        if (waiter->epoll_fd < 0) {
            return fail("exclusive-create");
        }
        if (ctl_add(waiter->epoll_fd, exclusive_shared_efd,
                    EPOLLIN | EPOLLEXCLUSIVE, EXCLUSIVE_SHARED_TAG,
                    "exclusive-add-shared") ||
            ctl_add(waiter->epoll_fd, release_efd, EPOLLIN,
                    EXCLUSIVE_RELEASE_TAG, "exclusive-add-release")) {
            return 1;
        }
    }
    /* EPOLL_CTL_MOD may never carry EPOLLEXCLUSIVE. */
    struct epoll_event mod = {.events = EPOLLIN | EPOLLEXCLUSIVE};
    mod.data.u32 = EXCLUSIVE_RELEASE_TAG;
    if (ctl_expect_errno(exclusive_waiters[0].epoll_fd, EPOLL_CTL_MOD,
                         release_efd, &mod, EINVAL, "exclusive-mod")) {
        return 1;
    }
    marker("THEKERNEL_EPOLL_EXCLUSIVE_MOD_EINVAL_OK");

    pthread_t threads[EXCLUSIVE_WAITERS];
    for (unsigned int index = 0; index < EXCLUSIVE_WAITERS; ++index) {
        if (pthread_create(&threads[index], NULL, exclusive_waiter_main,
                           &exclusive_waiters[index]) != 0) {
            return fail("exclusive-pthread-create");
        }
    }
    /* Handshake: each waiter publishes its TID right before epoll_wait; the
     * only blocking point after that is epoll_wait itself, so task state 'S'
     * proves the waiter is parked there before we fire the single event. */
    const struct timespec poll_delay = {.tv_sec = 0, .tv_nsec = 1000000};
    int all_blocked = 0;
    for (unsigned int attempt = 0; attempt < 5000 && !all_blocked; ++attempt) {
        all_blocked = 1;
        for (unsigned int index = 0; index < EXCLUSIVE_WAITERS; ++index) {
            pid_t tid = atomic_load_explicit(&exclusive_waiters[index].tid,
                                             memory_order_acquire);
            if (tid <= 0 || !thread_is_blocked(tid)) {
                all_blocked = 0;
                break;
            }
        }
        if (!all_blocked) {
            (void)nanosleep(&poll_delay, NULL);
        }
    }
    if (!all_blocked) {
        errno = ETIMEDOUT;
        return fail("exclusive-blocked-handshake");
    }
    uint64_t one = 1;
    if (write(exclusive_shared_efd, &one, sizeof(one)) !=
        (ssize_t)sizeof(one)) {
        return fail("exclusive-fire");
    }
    char token = 0;
    if (read(exclusive_winner_pipe[0], &token, 1) != 1 || token != 'W') {
        return fail("exclusive-winner-token");
    }
    if (write(release_efd, &one, sizeof(one)) != (ssize_t)sizeof(one)) {
        return fail("exclusive-release");
    }
    int woken = 0;
    for (unsigned int index = 0; index < EXCLUSIVE_WAITERS; ++index) {
        void *thread_result = NULL;
        if (pthread_join(threads[index], &thread_result) != 0 ||
            thread_result != NULL) {
            errno = EPROTO;
            return fail("exclusive-join");
        }
        struct exclusive_waiter *waiter = &exclusive_waiters[index];
        if (waiter->wait_result < 1) {
            return fail_value("exclusive-wait-result", waiter->wait_result,
                              1);
        }
        if (waiter->saw_shared) {
            ++woken;
        } else if (!waiter->saw_release) {
            errno = EPROTO;
            return fail("exclusive-release-missed");
        }
    }
    if (woken < 1 || woken >= EXCLUSIVE_WAITERS) {
        return fail_value("exclusive-woken-range", woken, 1);
    }
    printf("THEKERNEL_EPOLL_EXCLUSIVE_BOUNDARY woken=%d waiters=%d\n", woken,
           EXCLUSIVE_WAITERS);
    fflush(stdout);
    marker("THEKERNEL_EPOLL_EXCLUSIVE_OK");
    for (unsigned int index = 0; index < EXCLUSIVE_WAITERS; ++index) {
        close(exclusive_waiters[index].epoll_fd);
    }
    close(release_efd);
    close(exclusive_shared_efd);
    close(exclusive_winner_pipe[0]);
    close(exclusive_winner_pipe[1]);
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    if (argc == 2 && strcmp(argv[1], "--thekernel") == 0) {
        /* Reserved for guest-strict variants; no semantic divergence today. */
    } else if (argc != 1) {
        errno = EINVAL;
        return fail("unknown-option");
    }
    if (argv[0] == NULL || argv[0][0] != '/') {
        errno = EINVAL;
        return fail("absolute-self-path-required");
    }
    self_path = argv[0];

    if (test_level_vs_edge() || test_et_partial_read() || test_oneshot() ||
        test_ctl_errors() || test_hup() || test_timeouts() || test_nested() ||
        test_exclusive()) {
        return 1;
    }

    marker("THEKERNEL_EPOLL_OK");
    return 0;
}
