#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#ifndef FUTEX_32
#define FUTEX_32 2
#endif

#ifndef FUTEX_PRIVATE_FLAG
#define FUTEX_PRIVATE_FLAG 128
#endif

#ifndef FUTEX_WAKE
#define FUTEX_WAKE 1
#endif

/* Keep the x86_64 Linux syscall numbers available with older headers. */
#ifndef SYS_futex_waitv
#ifdef __NR_futex_waitv
#define SYS_futex_waitv __NR_futex_waitv
#else
#define SYS_futex_waitv 449
#endif
#endif

#ifndef SYS_tgkill
#ifdef __NR_tgkill
#define SYS_tgkill __NR_tgkill
#else
#define SYS_tgkill 234
#endif
#endif

#define WAITV_FLAGS (FUTEX_32 | FUTEX_PRIVATE_FLAG)
#define WAIT_TIMEOUT_NS 5000000000LL
#define BLOCK_TIMEOUT_NS 3000000000LL

struct local_futex_waitv {
    uint64_t val;
    uint64_t uaddr;
    uint32_t flags;
    uint32_t reserved;
};

_Static_assert(sizeof(struct local_futex_waitv) == 24,
               "futex_waitv ABI layout must remain 24 bytes");

struct wait_case {
    uint32_t *word;
    _Atomic int ready;
    _Atomic int done;
    _Atomic pid_t tid;
    struct timespec timeout;
    long result;
    int saved_errno;
    pthread_t thread;
};

static volatile sig_atomic_t sigusr1_hits;

static int fail(const char *stage) {
    fprintf(stderr,
            "THEKERNEL_FUTEX2_WAITV_SIGNAL_FAIL %s errno=%d (%s)\n", stage,
            errno, strerror(errno));
    return 1;
}

static int fail_value(const char *stage, long actual, long expected) {
    fprintf(stderr,
            "THEKERNEL_FUTEX2_WAITV_SIGNAL_FAIL %s actual=%ld expected=%ld "
            "errno=%d (%s)\n",
            stage, actual, expected, errno, strerror(errno));
    return 1;
}

static void marker(const char *value) {
    puts(value);
    fflush(stdout);
}

static void sigusr1_handler(int signo) {
    if (signo == SIGUSR1) {
        ++sigusr1_hits;
    }
}

static const char *unsupported_errno_name(int error_number) {
    switch (error_number) {
    case ENOSYS:
        return "ENOSYS";
    case EOPNOTSUPP:
        return "EOPNOTSUPP";
    default:
        return NULL;
    }
}

/* A zero-entry probe must be rejected with EINVAL by an implemented syscall.
 * ENOSYS/EOPNOTSUPP is a capability boundary and is never mislabeled as a
 * successful EINTR result. */
static int probe_waitv(int *unsupported_errno) {
    errno = 0;
    long result = syscall(SYS_futex_waitv, NULL, 0U, 0U, NULL,
                          CLOCK_MONOTONIC);
    if (result == -1 && errno == EINVAL) {
        return 0;
    }
    if (result == -1 && unsupported_errno != NULL &&
        unsupported_errno_name(errno) != NULL) {
        *unsupported_errno = errno;
        return 1;
    }
    if (result == -1) {
        return -1;
    }
    errno = EPROTO;
    return -1;
}

static long raw_futex_waitv(const struct local_futex_waitv *waiters,
                            uint32_t count, const struct timespec *timeout) {
    return syscall(SYS_futex_waitv, waiters, count, 0U, timeout,
                   CLOCK_MONOTONIC);
}

static long raw_futex_wake(uint32_t *word, int count) {
    return syscall(SYS_futex, word, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, count,
                   NULL, NULL, 0U);
}

static int64_t monotonic_ns(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        return -1;
    }
    return (int64_t)now.tv_sec * 1000000000LL + now.tv_nsec;
}

static int future_timeout(struct timespec *timeout) {
    if (clock_gettime(CLOCK_MONOTONIC, timeout) != 0) {
        return -1;
    }
    timeout->tv_sec += WAIT_TIMEOUT_NS / 1000000000LL;
    timeout->tv_nsec += WAIT_TIMEOUT_NS % 1000000000LL;
    if (timeout->tv_nsec >= 1000000000L) {
        ++timeout->tv_sec;
        timeout->tv_nsec -= 1000000000L;
    }
    return 0;
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
    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
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

/* The state handshake proves the waiter reached the interruptible sleep after
 * futex_waitv published its registration.  It avoids racing tgkill against
 * the user-space setup and accidentally delivering SIGUSR1 before the wait. */
static int wait_until_blocked(const struct wait_case *test) {
    int64_t start = monotonic_ns();
    if (start < 0) {
        return -1;
    }
    for (;;) {
        pid_t tid = atomic_load_explicit(&test->tid, memory_order_acquire);
        if (tid > 0) {
            int state = task_state(getpid(), tid);
            if (state == 'S' || state == 'D') {
                return 0;
            }
            if (state < 0) {
                return -1;
            }
        }
        if (atomic_load_explicit(&test->done, memory_order_acquire) != 0) {
            errno = EPROTO;
            return -1;
        }
        int64_t now = monotonic_ns();
        if (now < 0 || now - start >= BLOCK_TIMEOUT_NS) {
            errno = ETIMEDOUT;
            return -1;
        }
        sched_yield();
    }
}

static void *waiter_main(void *opaque) {
    struct wait_case *test = opaque;
    struct local_futex_waitv waiter = {
        .val = 0,
        .uaddr = (uint64_t)(uintptr_t)test->word,
        .flags = WAITV_FLAGS,
        .reserved = 0,
    };

    atomic_store_explicit(&test->tid, (pid_t)syscall(SYS_gettid),
                          memory_order_release);
    atomic_store_explicit(&test->ready, 1, memory_order_release);
    errno = 0;
    test->result = raw_futex_waitv(&waiter, 1U, &test->timeout);
    test->saved_errno = test->result == -1 ? errno : 0;
    atomic_store_explicit(&test->done, 1, memory_order_release);
    return NULL;
}

static int start_waiter(struct wait_case *test, uint32_t *word) {
    memset(test, 0, sizeof(*test));
    test->word = word;
    if (future_timeout(&test->timeout) != 0) {
        return fail("waiter-timeout-clock");
    }
    int result = pthread_create(&test->thread, NULL, waiter_main, test);
    if (result != 0) {
        errno = result;
        return fail("waiter-create");
    }
    return 0;
}

static int join_waiter(struct wait_case *test, const char *stage) {
    int result = pthread_join(test->thread, NULL);
    if (result != 0) {
        errno = result;
        return fail(stage);
    }
    return 0;
}

static void stop_waiter(struct wait_case *test, int deliver_signal) {
    pid_t tid = atomic_load_explicit(&test->tid, memory_order_acquire);
    if (deliver_signal && tid > 0) {
        (void)syscall(SYS_tgkill, getpid(), tid, SIGUSR1);
    }
    (void)raw_futex_wake(test->word, INT_MAX);
    (void)pthread_join(test->thread, NULL);
}

static int test_waitv_signal(void) {
    sigset_t unblock;
    sigemptyset(&unblock);
    sigaddset(&unblock, SIGUSR1);
    if (sigprocmask(SIG_UNBLOCK, &unblock, NULL) != 0) {
        return fail("sigprocmask-unblock");
    }

    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = sigusr1_handler;
    sigemptyset(&action.sa_mask);
    /* Deliberately leave SA_RESTART clear: EINTR is the contract under test. */
    action.sa_flags = 0;
    if (sigaction(SIGUSR1, &action, NULL) != 0) {
        return fail("sigaction-install");
    }

    uint32_t futex_word = 0;
    struct wait_case interrupted;
    if (start_waiter(&interrupted, &futex_word) != 0) {
        return 1;
    }
    if (wait_until_blocked(&interrupted) != 0) {
        int saved_errno = errno;
        stop_waiter(&interrupted, 1);
        errno = saved_errno;
        return fail("waitv-block-handshake");
    }

    pid_t tid = atomic_load_explicit(&interrupted.tid, memory_order_acquire);
    if (tid <= 0 || syscall(SYS_tgkill, getpid(), tid, SIGUSR1) != 0) {
        int saved_errno = errno != 0 ? errno : EIO;
        stop_waiter(&interrupted, 1);
        errno = saved_errno;
        return fail("waitv-signal-delivery");
    }
    if (join_waiter(&interrupted, "waitv-signal-join") != 0) {
        return 1;
    }
    if (interrupted.result != -1 || interrupted.saved_errno != EINTR ||
        sigusr1_hits != 1) {
        errno = interrupted.saved_errno != 0 ? interrupted.saved_errno : EIO;
        return fail("waitv-signal-result");
    }
    marker("THEKERNEL_FUTEX2_WAITV_SIGNAL_SUPPORTED_OK");
    marker("THEKERNEL_FUTEX2_WAITV_SIGNAL_EINTR_OK absolute_timeout=1 "
           "signal=SIGUSR1 sa_restart=0");

    /* An interrupted wait must have removed every queue registration before
     * returning. Waking with INT_MAX makes a stale registration observable. */
    errno = 0;
    long stale_wake = raw_futex_wake(interrupted.word, INT_MAX);
    if (stale_wake != 0) {
        return fail_value("waitv-stale-wake", stale_wake, 0);
    }
    marker("THEKERNEL_FUTEX2_WAITV_SIGNAL_NO_STALE_WAITER_OK same_address=1 "
           "wake_count=0");

    struct wait_case reused;
    if (start_waiter(&reused, &futex_word) != 0) {
        return 1;
    }
    if (wait_until_blocked(&reused) != 0) {
        int saved_errno = errno;
        stop_waiter(&reused, 0);
        errno = saved_errno;
        return fail("waitv-reuse-block-handshake");
    }
    errno = 0;
    long wake_count = raw_futex_wake(reused.word, INT_MAX);
    if (wake_count != 1) {
        int saved_errno = errno != 0 ? errno : EIO;
        stop_waiter(&reused, 0);
        errno = saved_errno;
        return fail_value("waitv-reuse-wake", wake_count, 1);
    }
    if (join_waiter(&reused, "waitv-reuse-join") != 0) {
        return 1;
    }
    if (reused.result != 0 || reused.saved_errno != 0) {
        errno = reused.saved_errno != 0 ? reused.saved_errno : EIO;
        return fail("waitv-reuse-result");
    }
    errno = 0;
    long post_wake = raw_futex_wake(reused.word, INT_MAX);
    if (post_wake != 0) {
        return fail_value("waitv-reuse-post-wake", post_wake, 0);
    }
    marker("THEKERNEL_FUTEX2_WAITV_SIGNAL_REUSE_WAKE_OK same_address=1 "
           "wake_count=1");
    marker("THEKERNEL_FUTEX2_WAITV_SIGNAL_OK");
    return 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    int unsupported_errno = 0;
    int probe = probe_waitv(&unsupported_errno);
    if (probe > 0) {
        printf("THEKERNEL_FUTEX2_WAITV_SIGNAL_UNSUPPORTED errno=%s\n",
               unsupported_errno_name(unsupported_errno));
        fflush(stdout);
        return 0;
    }
    if (probe < 0) {
        return fail("waitv-probe");
    }
    return test_waitv_signal();
}
