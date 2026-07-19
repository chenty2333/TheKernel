#define _GNU_SOURCE

#include <errno.h>
#include <limits.h>
#include <linux/futex.h>
#include <pthread.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef FUTEX_32
#define FUTEX_32 2
#endif

#ifndef FUTEX_PRIVATE_FLAG
#define FUTEX_PRIVATE_FLAG 128
#endif

#ifndef FUTEX_WAIT_PRIVATE
#define FUTEX_WAIT_PRIVATE (FUTEX_WAIT | FUTEX_PRIVATE_FLAG)
#endif

#ifndef FUTEX_WAKE_PRIVATE
#define FUTEX_WAKE_PRIVATE (FUTEX_WAKE | FUTEX_PRIVATE_FLAG)
#endif

/* futex_waitv uses the generic syscall number on RV64 and LoongArch64. */
#ifndef __NR_futex_waitv
#define __NR_futex_waitv 449
#endif

#define MAX_CPUS 64
#define CASE_TIMEOUT_NS 3000000000LL
#define RLIMIT_CASE_TIMEOUT_NS 10000000000LL
#define MIN_TIMER_PROGRESS_NS 10000000LL
#define RELATIVE_TIMER_NS 500000000LL

struct local_futex_waitv {
    uint64_t val;
    uint64_t uaddr;
    uint32_t flags;
    uint32_t reserved;
};

struct clock_worker {
    int cpu;
    int error;
};

struct futex_timeout_case {
    _Atomic uint32_t word;
    int error;
    long result;
    int64_t elapsed_ns;
};

struct futex_wake_case {
    _Atomic uint32_t word;
    _Atomic int ready;
    int error;
    unsigned int completed_wakes;
};

struct futex_waitv_case {
    _Atomic uint32_t words[2];
    _Atomic int ready;
    int error;
    long result;
};

static volatile sig_atomic_t itimer_hits;
static volatile sig_atomic_t rlimit_cpu_hits;
static volatile sig_atomic_t rlimit_cpu_signal_fd = -1;
static volatile sig_atomic_t cpu_itimer_virtual_hits;
static volatile sig_atomic_t cpu_itimer_prof_hits;
static volatile sig_atomic_t cpu_itimer_watchdog_hits;
static volatile uint64_t cpu_burn_sink;

static int fail(const char *stage)
{
    fprintf(stderr, "CI_WAIT_BOUNDARY_FAIL %s errno=%d (%s)\n",
            stage, errno, strerror(errno));
    return 1;
}

static _Noreturn void fail_and_exit(const char *stage)
{
    (void)fail(stage);
    fflush(NULL);
    _Exit(1);
}

static int64_t monotonic_ns(void)
{
    struct timespec now;

    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        return -1;
    }
    return (int64_t)now.tv_sec * 1000000000LL + now.tv_nsec;
}

static int wait_until_ready(_Atomic int *ready)
{
    int64_t start = monotonic_ns();
    if (start < 0) {
        return -1;
    }

    while (atomic_load_explicit(ready, memory_order_acquire) == 0) {
        int64_t now = monotonic_ns();
        if (now < 0) {
            return -1;
        }
        if (now - start >= CASE_TIMEOUT_NS) {
            errno = ETIMEDOUT;
            return -1;
        }
        sched_yield();
    }
    return 0;
}

static int join_bounded(pthread_t thread)
{
    int64_t start = monotonic_ns();
    if (start < 0) {
        fail_and_exit("thread-join-clock");
    }

    for (;;) {
        int result = pthread_tryjoin_np(thread, NULL);
        if (result == 0) {
            return 0;
        }
        if (result != EBUSY) {
            errno = result;
            fail_and_exit("thread-join-error");
        }
        int64_t now = monotonic_ns();
        if (now < 0) {
            fail_and_exit("thread-join-clock");
        }
        if (now - start >= CASE_TIMEOUT_NS) {
            errno = ETIMEDOUT;
            fail_and_exit("thread-join-timeout");
        }
        sched_yield();
    }
}

static void *clock_worker_main(void *opaque)
{
    struct clock_worker *worker = opaque;
    cpu_set_t affinity;
    struct timespec request = { .tv_sec = 0, .tv_nsec = 20000000 };

    CPU_ZERO(&affinity);
    CPU_SET(worker->cpu, &affinity);
    int result = pthread_setaffinity_np(pthread_self(), sizeof(affinity), &affinity);
    if (result != 0) {
        worker->error = result;
        return NULL;
    }
    if (sched_getcpu() != worker->cpu) {
        worker->error = EXDEV;
        return NULL;
    }

    int64_t start = monotonic_ns();
    if (start < 0) {
        worker->error = errno;
        return NULL;
    }
    do {
        result = clock_nanosleep(CLOCK_MONOTONIC, 0, &request, &request);
    } while (result == EINTR);
    int64_t end = monotonic_ns();
    if (result != 0) {
        worker->error = result;
    } else if (end < 0) {
        worker->error = errno;
    } else if (end - start < MIN_TIMER_PROGRESS_NS) {
        worker->error = EIO;
    } else if (sched_getcpu() != worker->cpu) {
        worker->error = EXDEV;
    }
    return NULL;
}

static int test_clock_per_cpu(long expected_cpus)
{
    long online_cpus = sysconf(_SC_NPROCESSORS_ONLN);
    if (online_cpus <= 0 || online_cpus > MAX_CPUS ||
        (expected_cpus != 0 && online_cpus != expected_cpus)) {
        errno = EINVAL;
        return fail("clock-online-cpus");
    }

    struct clock_worker *workers = calloc((size_t)online_cpus, sizeof(*workers));
    pthread_t *threads = calloc((size_t)online_cpus, sizeof(*threads));
    if (workers == NULL || threads == NULL) {
        free(workers);
        free(threads);
        return fail("clock-allocate");
    }

    long created = 0;
    for (; created < online_cpus; created++) {
        workers[created].cpu = (int)created;
        int result = pthread_create(&threads[created], NULL, clock_worker_main,
                                    &workers[created]);
        if (result != 0) {
            errno = result;
            break;
        }
    }

    int failed = created != online_cpus;
    for (long cpu = 0; cpu < created; cpu++) {
        if (join_bounded(threads[cpu]) != 0 || workers[cpu].error != 0) {
            if (workers[cpu].error != 0) {
                errno = workers[cpu].error;
            }
            failed = 1;
        }
    }
    free(workers);
    free(threads);
    if (failed) {
        return fail("clock-per-cpu");
    }

    printf("CI_WAIT_BOUNDARY_CLOCK_PERCPU_OK online_cpus=%ld\n", online_cpus);
    return 0;
}

static struct timespec timespec_add_ns(struct timespec value, int64_t delta_ns)
{
    value.tv_sec += (time_t)(delta_ns / 1000000000LL);
    value.tv_nsec += (long)(delta_ns % 1000000000LL);
    if (value.tv_nsec >= 1000000000L) {
        value.tv_sec++;
        value.tv_nsec -= 1000000000L;
    }
    return value;
}

static int test_timerfd_clock_step(void)
{
    struct timespec wall_before;
    struct timespec monotonic_before;
    int relative_fd = -1;
    int cancel_read_fd = -1;
    int cancel_rearm_fd = -1;
    int cancel_disarm_fd = -1;
    int wall_changed = 0;
    const char *failure = NULL;
    int failure_errno = 0;

    if (clock_gettime(CLOCK_REALTIME, &wall_before) != 0 ||
        clock_gettime(CLOCK_MONOTONIC, &monotonic_before) != 0) {
        return fail("timerfd-clock-snapshot");
    }

    relative_fd = timerfd_create(CLOCK_REALTIME, TFD_CLOEXEC | TFD_NONBLOCK);
    cancel_read_fd = timerfd_create(CLOCK_REALTIME, TFD_CLOEXEC | TFD_NONBLOCK);
    cancel_rearm_fd = timerfd_create(CLOCK_REALTIME, TFD_CLOEXEC | TFD_NONBLOCK);
    cancel_disarm_fd = timerfd_create(CLOCK_REALTIME, TFD_CLOEXEC | TFD_NONBLOCK);
    if (relative_fd < 0 || cancel_read_fd < 0 || cancel_rearm_fd < 0 ||
        cancel_disarm_fd < 0) {
        failure = "timerfd-create";
        failure_errno = errno;
        goto cleanup;
    }

    struct itimerspec relative = {
        .it_value = {
            .tv_sec = 0,
            .tv_nsec = RELATIVE_TIMER_NS,
        },
    };
    struct itimerspec absolute = {
        .it_value = timespec_add_ns(wall_before, 60000000000LL),
    };
    const int cancel_flags = TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET;
    if (timerfd_settime(relative_fd, 0, &relative, NULL) != 0 ||
        timerfd_settime(cancel_read_fd, cancel_flags, &absolute, NULL) != 0 ||
        timerfd_settime(cancel_rearm_fd, cancel_flags, &absolute, NULL) != 0 ||
        timerfd_settime(cancel_disarm_fd, cancel_flags, &absolute, NULL) != 0) {
        failure = "timerfd-arm";
        failure_errno = errno;
        goto cleanup;
    }

    struct timespec stepped = timespec_add_ns(wall_before, 1000000000LL);
    if (clock_settime(CLOCK_REALTIME, &stepped) != 0) {
        if (errno == EPERM) {
            puts("CI_WAIT_BOUNDARY_TIMERFD_CANCEL_SKIPPED no_cap_sys_time=1");
            goto cleanup;
        }
        failure = "timerfd-clock-step";
        failure_errno = errno;
        goto cleanup;
    }
    wall_changed = 1;

    struct pollfd relative_poll = {
        .fd = relative_fd,
        .events = POLLIN,
    };
    if (poll(&relative_poll, 1, 0) != 0) {
        failure = "timerfd-relative-stepped-early";
        failure_errno = EIO;
        goto cleanup;
    }

    struct pollfd cancel_poll = {
        .fd = cancel_read_fd,
        .events = POLLIN,
    };
    if (poll(&cancel_poll, 1, 1000) != 1 ||
        (cancel_poll.revents & POLLIN) == 0) {
        failure = "timerfd-cancel-poll";
        failure_errno = ETIMEDOUT;
        goto cleanup;
    }

    uint64_t expirations = 0;
    errno = 0;
    if (read(cancel_read_fd, &expirations, sizeof(expirations)) != -1 ||
        errno != ECANCELED) {
        failure = "timerfd-cancel-read";
        failure_errno = errno != 0 ? errno : EIO;
        goto cleanup;
    }

    errno = 0;
    if (timerfd_settime(cancel_rearm_fd, cancel_flags, &absolute, NULL) != -1 ||
        errno != ECANCELED) {
        failure = "timerfd-cancel-rearm-result";
        failure_errno = errno != 0 ? errno : EIO;
        goto cleanup;
    }
    struct itimerspec rearmed;
    if (timerfd_gettime(cancel_rearm_fd, &rearmed) != 0 ||
        (rearmed.it_value.tv_sec == 0 && rearmed.it_value.tv_nsec == 0)) {
        failure = "timerfd-cancel-rearm-state";
        failure_errno = errno != 0 ? errno : EIO;
        goto cleanup;
    }

    struct itimerspec disarmed = {0};
    errno = 0;
    if (timerfd_settime(cancel_disarm_fd, cancel_flags, &disarmed, NULL) != 0) {
        failure = "timerfd-cancel-disarm-result";
        failure_errno = errno;
        goto cleanup;
    }
    struct pollfd disarmed_poll = {
        .fd = cancel_disarm_fd,
        .events = POLLIN,
    };
    if (poll(&disarmed_poll, 1, 0) != 0) {
        failure = "timerfd-cancel-disarm-poll";
        failure_errno = EIO;
        goto cleanup;
    }
    errno = 0;
    if (timerfd_settime(cancel_disarm_fd, cancel_flags, &absolute, NULL) != -1 ||
        errno != ECANCELED) {
        failure = "timerfd-cancel-disarm-preserve";
        failure_errno = errno != 0 ? errno : EIO;
        goto cleanup;
    }

    relative_poll.revents = 0;
    if (poll(&relative_poll, 1, 2000) != 1 ||
        (relative_poll.revents & POLLIN) == 0) {
        failure = "timerfd-relative-timeout";
        failure_errno = ETIMEDOUT;
        goto cleanup;
    }
    int64_t relative_end = monotonic_ns();
    int64_t relative_start = (int64_t)monotonic_before.tv_sec * 1000000000LL +
                             monotonic_before.tv_nsec;
    if (relative_end < 0 || relative_end - relative_start < MIN_TIMER_PROGRESS_NS) {
        failure = "timerfd-relative-progress";
        failure_errno = relative_end < 0 ? errno : EIO;
        goto cleanup;
    }

cleanup:
    if (wall_changed) {
        struct timespec monotonic_after;
        if (clock_gettime(CLOCK_MONOTONIC, &monotonic_after) != 0) {
            if (failure == NULL) {
                failure = "timerfd-restore-snapshot";
                failure_errno = errno;
            }
        } else {
            int64_t elapsed =
                ((int64_t)monotonic_after.tv_sec - monotonic_before.tv_sec) *
                    1000000000LL +
                monotonic_after.tv_nsec - monotonic_before.tv_nsec;
            struct timespec restored = timespec_add_ns(wall_before, elapsed);
            if (clock_settime(CLOCK_REALTIME, &restored) != 0) {
                if (failure == NULL) {
                    failure = "timerfd-clock-restore";
                    failure_errno = errno;
                } else {
                    fprintf(stderr,
                            "CI_WAIT_BOUNDARY_FAIL timerfd-clock-restore-secondary "
                            "errno=%d (%s)\n",
                            errno, strerror(errno));
                }
            }
        }
    }
    if (relative_fd >= 0) {
        close(relative_fd);
    }
    if (cancel_read_fd >= 0) {
        close(cancel_read_fd);
    }
    if (cancel_rearm_fd >= 0) {
        close(cancel_rearm_fd);
    }
    if (cancel_disarm_fd >= 0) {
        close(cancel_disarm_fd);
    }
    if (failure != NULL) {
        errno = failure_errno;
        return fail(failure);
    }
    if (wall_changed) {
        puts("CI_WAIT_BOUNDARY_TIMERFD_CANCEL_OK");
    }
    return 0;
}

static void itimer_alarm_handler(int signal_number)
{
    if (signal_number == SIGALRM) {
        itimer_hits++;
    }
}

struct itimer_arm_case {
    struct itimerval periodic;
    int error;
};

static void *itimer_arm_worker(void *opaque)
{
    struct itimer_arm_case *test = opaque;

    if (setitimer(ITIMER_REAL, &test->periodic, NULL) != 0) {
        test->error = errno;
    }
    return NULL;
}

static int test_itimer_periodic(void)
{
    struct sigaction action = {
        .sa_handler = itimer_alarm_handler,
    };
    struct sigaction previous;
    struct itimer_arm_case test = {
        .periodic = {
            .it_interval = {
                .tv_sec = 0,
                .tv_usec = 20000,
            },
            .it_value = {
                .tv_sec = 0,
                .tv_usec = 20000,
            },
        },
    };
    const struct itimerval disarmed = {0};
    const char *failure = NULL;
    int failure_errno = 0;
    pthread_t armer;

    sigemptyset(&action.sa_mask);
    itimer_hits = 0;
    if (sigaction(SIGALRM, &action, &previous) != 0) {
        return fail("itimer-periodic-sigaction");
    }
    int result = pthread_create(&armer, NULL, itimer_arm_worker, &test);
    if (result != 0) {
        failure = "itimer-periodic-create";
        failure_errno = result;
        goto cleanup;
    }
    if (join_bounded(armer) != 0) {
        failure = "itimer-periodic-join";
        failure_errno = errno;
        goto cleanup;
    }
    if (test.error != 0) {
        failure = "itimer-periodic-arm";
        failure_errno = test.error;
        goto cleanup;
    }

    sig_atomic_t hits_at_join = itimer_hits;
    int64_t start = monotonic_ns();
    if (start < 0) {
        failure = "itimer-periodic-clock";
        failure_errno = errno;
        goto cleanup;
    }
    while (itimer_hits - hits_at_join < 3) {
        int64_t now = monotonic_ns();
        if (now < 0) {
            failure = "itimer-periodic-clock";
            failure_errno = errno;
            goto cleanup;
        }
        if (now - start >= CASE_TIMEOUT_NS) {
            failure = "itimer-periodic-timeout";
            failure_errno = ETIMEDOUT;
            goto cleanup;
        }

        struct timespec pause = {
            .tv_sec = 0,
            .tv_nsec = 5000000,
        };
        while (nanosleep(&pause, &pause) != 0 && errno == EINTR &&
               itimer_hits - hits_at_join < 3) {
        }
    }

cleanup:
    if (setitimer(ITIMER_REAL, &disarmed, NULL) != 0 && failure == NULL) {
        failure = "itimer-periodic-disarm";
        failure_errno = errno;
    }
    if (sigaction(SIGALRM, &previous, NULL) != 0 && failure == NULL) {
        failure = "itimer-periodic-restore";
        failure_errno = errno;
    }
    if (failure != NULL) {
        errno = failure_errno;
        return fail(failure);
    }
    puts("CI_WAIT_BOUNDARY_ITIMER_PERIODIC_OK min_hits=3");
    return 0;
}

static void cpu_itimer_handler(int signal_number)
{
    switch (signal_number) {
    case SIGVTALRM:
        cpu_itimer_virtual_hits++;
        break;
    case SIGPROF:
        cpu_itimer_prof_hits++;
        break;
    case SIGALRM:
        cpu_itimer_watchdog_hits++;
        break;
    default:
        break;
    }
}

static int run_cpu_itimer_burn(int which, volatile sig_atomic_t *hits)
{
    const struct itimerval target = {
        .it_value = {
            .tv_sec = 0,
            .tv_usec = 50000,
        },
    };
    const struct itimerval watchdog = {
        .it_value = {
            .tv_sec = 2,
            .tv_usec = 0,
        },
    };
    const struct itimerval disarmed = {0};

    *hits = 0;
    cpu_itimer_watchdog_hits = 0;
    if (setitimer(ITIMER_REAL, &watchdog, NULL) != 0 ||
        setitimer(which, &target, NULL) != 0) {
        (void)setitimer(ITIMER_REAL, &disarmed, NULL);
        (void)setitimer(which, &disarmed, NULL);
        return -1;
    }

    /* Deliberately no calls in this loop: a syscall or yield would create the
       accounting edge this regression is meant to prove unnecessary. */
    uint64_t value = cpu_burn_sink | 1U;
    while (*hits == 0 && cpu_itimer_watchdog_hits == 0) {
        value = value * UINT64_C(6364136223846793005) + UINT64_C(1);
        cpu_burn_sink = value;
    }

    int saved_errno = 0;
    if (setitimer(which, &disarmed, NULL) != 0) {
        saved_errno = errno;
    }
    if (setitimer(ITIMER_REAL, &disarmed, NULL) != 0 && saved_errno == 0) {
        saved_errno = errno;
    }
    if (saved_errno != 0) {
        errno = saved_errno;
        return -1;
    }
    if (*hits == 0 || cpu_itimer_watchdog_hits != 0) {
        errno = ETIMEDOUT;
        return -1;
    }
    return 0;
}

static int test_cpu_itimers_without_syscall_edges(void)
{
    struct sigaction action = {
        .sa_handler = cpu_itimer_handler,
    };
    struct sigaction previous_virtual;
    struct sigaction previous_prof;
    struct sigaction previous_alarm;
    const char *failure = NULL;
    int failure_errno = 0;

    sigemptyset(&action.sa_mask);
    if (sigaction(SIGVTALRM, &action, &previous_virtual) != 0 ||
        sigaction(SIGPROF, &action, &previous_prof) != 0 ||
        sigaction(SIGALRM, &action, &previous_alarm) != 0) {
        return fail("itimer-cpu-sigaction");
    }

    if (run_cpu_itimer_burn(ITIMER_VIRTUAL, &cpu_itimer_virtual_hits) != 0) {
        failure = "itimer-virtual-cpu-burn";
        failure_errno = errno;
    } else if (run_cpu_itimer_burn(ITIMER_PROF, &cpu_itimer_prof_hits) != 0) {
        failure = "itimer-prof-cpu-burn";
        failure_errno = errno;
    }

    if (sigaction(SIGVTALRM, &previous_virtual, NULL) != 0 && failure == NULL) {
        failure = "itimer-virtual-restore";
        failure_errno = errno;
    }
    if (sigaction(SIGPROF, &previous_prof, NULL) != 0 && failure == NULL) {
        failure = "itimer-prof-restore";
        failure_errno = errno;
    }
    if (sigaction(SIGALRM, &previous_alarm, NULL) != 0 && failure == NULL) {
        failure = "itimer-watchdog-restore";
        failure_errno = errno;
    }
    if (failure != NULL) {
        errno = failure_errno;
        return fail(failure);
    }
    puts("CI_WAIT_BOUNDARY_ITIMER_CPU_OK no_syscall_loop=1");
    return 0;
}

struct rlimit_cpu_report {
    rlim_t soft;
    rlim_t hard;
    sig_atomic_t signal_hits;
};

struct rlimit_cpu_burner_state {
    _Atomic int ready;
    _Atomic int start;
    int error;
};

static void rlimit_cpu_handler(int signal_number)
{
    if (signal_number != SIGXCPU) {
        return;
    }

    int saved_errno = errno;
    rlimit_cpu_hits++;
    if (rlimit_cpu_signal_fd >= 0) {
        const unsigned char marker = 1;
        (void)write((int)rlimit_cpu_signal_fd, &marker, sizeof(marker));
    }
    errno = saved_errno;
}

static _Noreturn void burn_cpu_forever(void)
{
    volatile uint64_t value = UINT64_C(0x9e3779b97f4a7c15);

    for (;;) {
        value ^= value << 7;
        value ^= value >> 9;
        value += UINT64_C(0x9e3779b97f4a7c15);
    }
}

static void *rlimit_cpu_burner(void *opaque)
{
    struct rlimit_cpu_burner_state *state = opaque;
    sigset_t blocked;

    sigemptyset(&blocked);
    sigaddset(&blocked, SIGXCPU);
    int result = pthread_sigmask(SIG_BLOCK, &blocked, NULL);
    if (result != 0) {
        state->error = result;
        atomic_store_explicit(&state->ready, 1, memory_order_release);
        return NULL;
    }

    atomic_store_explicit(&state->ready, 1, memory_order_release);
    while (atomic_load_explicit(&state->start, memory_order_acquire) == 0) {
        sched_yield();
    }
    burn_cpu_forever();
}

static int waitpid_bounded(pid_t child, int *status)
{
    int64_t start = monotonic_ns();
    if (start < 0) {
        return -1;
    }

    for (;;) {
        pid_t result = waitpid(child, status, WNOHANG);
        if (result == child) {
            return 0;
        }
        if (result < 0 && errno != EINTR) {
            return -1;
        }

        int64_t now = monotonic_ns();
        if (now < 0) {
            return -1;
        }
        if (now - start >= RLIMIT_CASE_TIMEOUT_NS) {
            (void)kill(child, SIGKILL);
            do {
                result = waitpid(child, status, 0);
            } while (result < 0 && errno == EINTR);
            errno = ETIMEDOUT;
            return -1;
        }

        struct timespec pause = {
            .tv_sec = 0,
            .tv_nsec = 5000000,
        };
        while (nanosleep(&pause, &pause) != 0 && errno == EINTR) {
        }
    }
}

static int read_rlimit_cpu_report(int fd, struct rlimit_cpu_report *report)
{
    size_t received = 0;

    while (received < sizeof(*report)) {
        ssize_t result = read(fd, (unsigned char *)report + received,
                              sizeof(*report) - received);
        if (result > 0) {
            received += (size_t)result;
            continue;
        }
        if (result < 0 && errno == EINTR) {
            continue;
        }
        if (result < 0) {
            return -1;
        }
        errno = EIO;
        return -1;
    }
    return 0;
}

static _Noreturn void run_rlimit_cpu_escalation_child(int report_fd)
{
    struct sigaction action = {
        .sa_handler = rlimit_cpu_handler,
    };
    struct rlimit_cpu_burner_state state = {0};
    const struct rlimit limit = {
        .rlim_cur = 1,
        .rlim_max = 3,
    };
    sigset_t cpu_signal;
    pthread_t burner;

    rlimit_cpu_hits = 0;
    rlimit_cpu_signal_fd = -1;
    sigemptyset(&action.sa_mask);
    sigemptyset(&cpu_signal);
    sigaddset(&cpu_signal, SIGXCPU);
    if (sigaction(SIGXCPU, &action, NULL) != 0 ||
        pthread_sigmask(SIG_BLOCK, &cpu_signal, NULL) != 0) {
        _Exit(90);
    }

    int result = pthread_create(&burner, NULL, rlimit_cpu_burner, &state);
    if (result != 0 || wait_until_ready(&state.ready) != 0 || state.error != 0) {
        _Exit(91);
    }
    if (setrlimit(RLIMIT_CPU, &limit) != 0 ||
        pthread_sigmask(SIG_UNBLOCK, &cpu_signal, NULL) != 0) {
        _Exit(92);
    }
    atomic_store_explicit(&state.start, 1, memory_order_release);

    while (rlimit_cpu_hits == 0) {
        struct timespec pause = {
            .tv_sec = 0,
            .tv_nsec = 5000000,
        };
        while (nanosleep(&pause, &pause) != 0 && errno == EINTR &&
               rlimit_cpu_hits == 0) {
        }
    }

    struct rlimit observed;
    if (getrlimit(RLIMIT_CPU, &observed) != 0) {
        _Exit(93);
    }
    const struct rlimit_cpu_report report = {
        .soft = observed.rlim_cur,
        .hard = observed.rlim_max,
        .signal_hits = rlimit_cpu_hits,
    };
    ssize_t written;
    do {
        written = write(report_fd, &report, sizeof(report));
    } while (written < 0 && errno == EINTR);
    if (written != (ssize_t)sizeof(report)) {
        _Exit(94);
    }

    for (;;) {
        pause();
    }
}

static int test_rlimit_cpu_escalation(void)
{
    int report_pipe[2];
    if (pipe(report_pipe) != 0) {
        return fail("rlimit-cpu-escalation-pipe");
    }

    pid_t child = fork();
    if (child < 0) {
        close(report_pipe[0]);
        close(report_pipe[1]);
        return fail("rlimit-cpu-escalation-fork");
    }
    if (child == 0) {
        close(report_pipe[0]);
        run_rlimit_cpu_escalation_child(report_pipe[1]);
    }

    close(report_pipe[1]);
    int status = 0;
    if (waitpid_bounded(child, &status) != 0) {
        close(report_pipe[0]);
        return fail("rlimit-cpu-escalation-timeout");
    }

    struct rlimit_cpu_report report;
    if (read_rlimit_cpu_report(report_pipe[0], &report) != 0) {
        close(report_pipe[0]);
        return fail("rlimit-cpu-escalation-report");
    }
    close(report_pipe[0]);
    if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGKILL ||
        report.soft != 2 || report.hard != 3 || report.signal_hits < 1) {
        errno = EIO;
        return fail("rlimit-cpu-escalation-result");
    }

    puts("CI_WAIT_BOUNDARY_RLIMIT_CPU_ESCALATION_OK soft_after_signal=2 "
         "hard_signal=SIGKILL");
    return 0;
}

static _Noreturn void run_rlimit_cpu_hard_only_child(int signal_fd)
{
    struct sigaction action = {
        .sa_handler = rlimit_cpu_handler,
    };
    const struct rlimit limit = {
        .rlim_cur = 1,
        .rlim_max = 1,
    };
    sigset_t cpu_signal;

    rlimit_cpu_hits = 0;
    rlimit_cpu_signal_fd = signal_fd;
    sigemptyset(&action.sa_mask);
    sigemptyset(&cpu_signal);
    sigaddset(&cpu_signal, SIGXCPU);
    if (sigaction(SIGXCPU, &action, NULL) != 0 ||
        pthread_sigmask(SIG_UNBLOCK, &cpu_signal, NULL) != 0 ||
        setrlimit(RLIMIT_CPU, &limit) != 0) {
        _Exit(95);
    }
    burn_cpu_forever();
}

static int test_rlimit_cpu_hard_only(void)
{
    int signal_pipe[2];
    if (pipe(signal_pipe) != 0) {
        return fail("rlimit-cpu-hard-only-pipe");
    }

    pid_t child = fork();
    if (child < 0) {
        close(signal_pipe[0]);
        close(signal_pipe[1]);
        return fail("rlimit-cpu-hard-only-fork");
    }
    if (child == 0) {
        close(signal_pipe[0]);
        run_rlimit_cpu_hard_only_child(signal_pipe[1]);
    }

    close(signal_pipe[1]);
    int status = 0;
    if (waitpid_bounded(child, &status) != 0) {
        close(signal_pipe[0]);
        return fail("rlimit-cpu-hard-only-timeout");
    }

    unsigned char marker;
    ssize_t read_result;
    do {
        read_result = read(signal_pipe[0], &marker, sizeof(marker));
    } while (read_result < 0 && errno == EINTR);
    close(signal_pipe[0]);
    if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGKILL ||
        read_result != 0) {
        errno = EIO;
        return fail("rlimit-cpu-hard-only-result");
    }

    puts("CI_WAIT_BOUNDARY_RLIMIT_CPU_HARD_ONLY_OK signal=SIGKILL sigxcpu=0");
    return 0;
}

static int expect_prlimit64_error(pid_t pid, unsigned int resource,
                                  const struct rlimit *new_limit,
                                  int expected_errno)
{
    errno = 0;
    long result = syscall(SYS_prlimit64, pid, resource, new_limit, NULL);
    if (result != -1 || errno != expected_errno) {
        errno = EIO;
        return -1;
    }
    return 0;
}

static int test_prlimit64_error_precedence(void)
{
    const struct rlimit *bad_limit = (const struct rlimit *)(uintptr_t)1;

    if (expect_prlimit64_error(INT_MAX, UINT_MAX, bad_limit, EFAULT) != 0 ||
        expect_prlimit64_error(0, UINT_MAX, bad_limit, EFAULT) != 0 ||
        expect_prlimit64_error(INT_MAX, UINT_MAX, NULL, ESRCH) != 0) {
        return fail("prlimit64-error-precedence");
    }

    puts("CI_WAIT_BOUNDARY_PRLIMIT_PRECEDENCE_OK bad_new=EFAULT "
         "bad_pid_before_resource=ESRCH");
    return 0;
}

static int run_prlimit64_transaction_child(void)
{
    struct rlimit initial;
    if (getrlimit(RLIMIT_NOFILE, &initial) != 0 ||
        initial.rlim_cur < 4 || initial.rlim_max < 4) {
        return fail("prlimit64-transaction-initial");
    }

    struct rlimit replacement = {
        .rlim_cur = initial.rlim_cur - 1,
        .rlim_max = initial.rlim_max,
    };
    struct rlimit observed_old = {0};
    if (syscall(SYS_prlimit64, 0, RLIMIT_NOFILE, &replacement,
                &observed_old) != 0 ||
        observed_old.rlim_cur != initial.rlim_cur ||
        observed_old.rlim_max != initial.rlim_max) {
        return fail("prlimit64-transaction-old-new");
    }

    struct rlimit observed;
    if (getrlimit(RLIMIT_NOFILE, &observed) != 0 ||
        observed.rlim_cur != replacement.rlim_cur ||
        observed.rlim_max != replacement.rlim_max) {
        return fail("prlimit64-transaction-installed");
    }

    struct rlimit invalid = {
        .rlim_cur = replacement.rlim_max,
        .rlim_max = replacement.rlim_max - 1,
    };
    errno = 0;
    if (syscall(SYS_prlimit64, 0, RLIMIT_NOFILE, &invalid, NULL) != -1 ||
        errno != EINVAL || getrlimit(RLIMIT_NOFILE, &observed) != 0 ||
        observed.rlim_cur != replacement.rlim_cur ||
        observed.rlim_max != replacement.rlim_max) {
        errno = EIO;
        return fail("prlimit64-transaction-rollback");
    }

    struct rlimit committed_before_fault = {
        .rlim_cur = replacement.rlim_cur - 1,
        .rlim_max = replacement.rlim_max,
    };
    errno = 0;
    if (syscall(SYS_prlimit64, 0, RLIMIT_NOFILE, &committed_before_fault,
                (struct rlimit *)(uintptr_t)1) != -1 ||
        errno != EFAULT || getrlimit(RLIMIT_NOFILE, &observed) != 0 ||
        observed.rlim_cur != committed_before_fault.rlim_cur ||
        observed.rlim_max != committed_before_fault.rlim_max) {
        errno = EIO;
        return fail("prlimit64-transaction-copyout");
    }

    return 0;
}

static int test_prlimit64_owner_transaction(void)
{
    pid_t child = fork();
    if (child < 0) {
        return fail("prlimit64-transaction-fork");
    }
    if (child == 0) {
        _exit(run_prlimit64_transaction_child() == 0 ? 0 : 1);
    }

    int status = 0;
    if (waitpid_bounded(child, &status) != 0 || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0) {
        errno = EIO;
        return fail("prlimit64-transaction-child");
    }

    puts("CI_WAIT_BOUNDARY_PRLIMIT_TRANSACTION_OK old_new=atomic "
         "invalid=rollback copyout_fault=committed");
    return 0;
}

static int test_legacy_limit_timer_error_precedence(void)
{
    const struct rlimit *bad_limit = (const struct rlimit *)(uintptr_t)1;
    const struct itimerval *bad_timer =
        (const struct itimerval *)(uintptr_t)1;

    errno = 0;
    if (syscall(SYS_setrlimit, UINT_MAX, bad_limit) != -1 ||
        errno != EFAULT) {
        errno = EIO;
        return fail("setrlimit-error-precedence");
    }

    errno = 0;
    if (syscall(SYS_setitimer, INT_MAX, bad_timer, NULL) != -1 ||
        errno != EFAULT) {
        errno = EIO;
        return fail("setitimer-error-precedence");
    }

    puts("CI_WAIT_BOUNDARY_LEGACY_PRECEDENCE_OK "
         "setrlimit_bad_new=EFAULT setitimer_bad_new=EFAULT");
    return 0;
}

static long raw_futex(_Atomic uint32_t *word, int operation, uint32_t value,
                      const struct timespec *timeout)
{
    return syscall(SYS_futex, (uint32_t *)word, operation, value,
                   timeout, NULL, 0);
}

static int drive_one_direct_wake(_Atomic uint32_t *word)
{
    int64_t start = monotonic_ns();
    if (start < 0) {
        return -1;
    }

    for (;;) {
        long count = raw_futex(word, FUTEX_WAKE_PRIVATE, 1, NULL);
        if (count == 1) {
            return 0;
        }
        if (count < 0) {
            return -1;
        }
        int64_t now = monotonic_ns();
        if (now < 0) {
            return -1;
        }
        if (now - start >= CASE_TIMEOUT_NS) {
            errno = ETIMEDOUT;
            return -1;
        }
        sched_yield();
    }
}

static void *futex_wake_waiter(void *opaque)
{
    struct futex_wake_case *test = opaque;

    atomic_store_explicit(&test->ready, 1, memory_order_release);
    while (atomic_load_explicit(&test->word, memory_order_acquire) == 0) {
        long result = raw_futex(&test->word, FUTEX_WAIT_PRIVATE, 0, NULL);
        if (result == 0) {
            test->completed_wakes++;
            continue;
        }
        if (errno == EAGAIN || errno == EINTR) {
            continue;
        }
        test->error = errno;
        return NULL;
    }
    return NULL;
}

static int test_futex_wake(void)
{
    struct futex_wake_case test = {0};
    pthread_t waiter;
    int result = pthread_create(&waiter, NULL, futex_wake_waiter, &test);
    if (result != 0) {
        errno = result;
        return fail("futex-wake-create");
    }
    if (wait_until_ready(&test.ready) != 0 || drive_one_direct_wake(&test.word) != 0) {
        atomic_store_explicit(&test.word, 1, memory_order_release);
        (void)raw_futex(&test.word, FUTEX_WAKE_PRIVATE, 1, NULL);
        (void)join_bounded(waiter);
        return fail("futex-wake-admission");
    }

    atomic_store_explicit(&test.word, 1, memory_order_release);
    (void)raw_futex(&test.word, FUTEX_WAKE_PRIVATE, 1, NULL);
    if (join_bounded(waiter) != 0) {
        return fail("futex-wake-join");
    }
    if (test.error != 0 || test.completed_wakes == 0) {
        errno = test.error != 0 ? test.error : EIO;
        return fail("futex-wake-result");
    }
    puts("CI_WAIT_BOUNDARY_FUTEX_WAKE_OK");
    return 0;
}

static void *futex_timeout_waiter(void *opaque)
{
    struct futex_timeout_case *test = opaque;
    const struct timespec timeout = { .tv_sec = 0, .tv_nsec = 50000000 };

    int64_t start = monotonic_ns();
    if (start < 0) {
        test->error = errno;
        return NULL;
    }
    errno = 0;
    test->result = raw_futex(&test->word, FUTEX_WAIT_PRIVATE, 0, &timeout);
    int saved_errno = errno;
    int64_t end = monotonic_ns();
    if (end < 0) {
        test->error = errno;
    } else {
        test->elapsed_ns = end - start;
        if (test->result != -1 || saved_errno != ETIMEDOUT ||
            test->elapsed_ns < MIN_TIMER_PROGRESS_NS) {
            test->error = test->result != -1 ? EIO : saved_errno;
            if (test->error == 0 || test->elapsed_ns < MIN_TIMER_PROGRESS_NS) {
                test->error = EIO;
            }
        }
    }
    return NULL;
}

static int test_futex_timeout(void)
{
    struct futex_timeout_case test = {0};
    pthread_t waiter;
    int result = pthread_create(&waiter, NULL, futex_timeout_waiter, &test);
    if (result != 0) {
        errno = result;
        return fail("futex-timeout-create");
    }
    if (join_bounded(waiter) != 0) {
        return fail("futex-timeout-join");
    }
    if (test.error != 0 || test.result != -1 ||
        test.elapsed_ns < MIN_TIMER_PROGRESS_NS) {
        errno = test.error != 0 ? test.error : EIO;
        return fail("futex-timeout-result");
    }
    puts("CI_WAIT_BOUNDARY_FUTEX_TIMEOUT_OK");
    return 0;
}

static void *futex_waitv_waiter(void *opaque)
{
    struct futex_waitv_case *test = opaque;
    struct local_futex_waitv waiters[2] = {
        {
            .val = 0,
            .uaddr = (uint64_t)(uintptr_t)&test->words[0],
            .flags = FUTEX_32 | FUTEX_PRIVATE_FLAG,
        },
        {
            .val = 0,
            .uaddr = (uint64_t)(uintptr_t)&test->words[1],
            .flags = FUTEX_32 | FUTEX_PRIVATE_FLAG,
        },
    };

    atomic_store_explicit(&test->ready, 1, memory_order_release);
    test->result = syscall(__NR_futex_waitv, waiters, 2U, 0U, NULL,
                           CLOCK_MONOTONIC);
    if (test->result < 0) {
        test->error = errno;
    }
    return NULL;
}

static int test_futex_waitv(void)
{
    struct futex_waitv_case test = {0};
    pthread_t waiter;
    int result = pthread_create(&waiter, NULL, futex_waitv_waiter, &test);
    if (result != 0) {
        errno = result;
        return fail("futex-waitv-create");
    }
    if (wait_until_ready(&test.ready) != 0 ||
        drive_one_direct_wake(&test.words[1]) != 0) {
        (void)raw_futex(&test.words[0], FUTEX_WAKE_PRIVATE, 1, NULL);
        (void)raw_futex(&test.words[1], FUTEX_WAKE_PRIVATE, 1, NULL);
        (void)join_bounded(waiter);
        return fail("futex-waitv-admission");
    }
    if (join_bounded(waiter) != 0) {
        return fail("futex-waitv-join");
    }
    if (test.error != 0 || test.result != 1) {
        errno = test.error != 0 ? test.error : EIO;
        return fail("futex-waitv-result");
    }
    puts("CI_WAIT_BOUNDARY_FUTEX_WAITV_OK");
    return 0;
}

static int parse_expected_cpus(int argc, char **argv, long *expected_cpus)
{
    if (argc == 1) {
        *expected_cpus = 0;
        return 0;
    }
    if (argc != 3 || strcmp(argv[1], "--expect-cpus") != 0) {
        errno = EINVAL;
        return -1;
    }

    char *end = NULL;
    errno = 0;
    long value = strtol(argv[2], &end, 10);
    if (errno != 0 || end == argv[2] || *end != '\0' ||
        value <= 0 || value > MAX_CPUS) {
        errno = EINVAL;
        return -1;
    }
    *expected_cpus = value;
    return 0;
}

int main(int argc, char **argv)
{
    long expected_cpus;

    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);
    if (parse_expected_cpus(argc, argv, &expected_cpus) != 0) {
        return fail("arguments");
    }
    if (test_clock_per_cpu(expected_cpus) != 0 ||
        test_timerfd_clock_step() != 0 ||
        test_itimer_periodic() != 0 ||
        test_cpu_itimers_without_syscall_edges() != 0 ||
        test_rlimit_cpu_escalation() != 0 ||
        test_rlimit_cpu_hard_only() != 0 ||
        test_prlimit64_error_precedence() != 0 ||
        test_prlimit64_owner_transaction() != 0 ||
        test_legacy_limit_timer_error_precedence() != 0 ||
        test_futex_wake() != 0 ||
        test_futex_timeout() != 0 ||
        test_futex_waitv() != 0) {
        return 1;
    }
    puts("CI_WAIT_BOUNDARY_PASS");
    return 0;
}
