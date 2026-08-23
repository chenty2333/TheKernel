#define _GNU_SOURCE

/*
 * vfork's observable release boundaries.  The old shared-memory probe could
 * pass even if the parent was resumed for an unrelated reason.  The exit case
 * pins the parent and child to one CPU, publishes phase 1 and phase 2 around
 * two raw sleep windows, checking each window's monotonic elapsed time before
 * publishing the next phase, and publishes phase 3 immediately before raw
 * exit.  The parent observes phase 3 before it performs a blocking reap.  The
 * exec case likewise checks the child's raw sleep duration before exec,
 * publishes a phase immediately before the raw execve, and then requires
 * /proc/<pid>/exe to converge to the target identity.  The signal case checks
 * a raw monotonic sleep before publishing
 * its final phase and immediately issuing raw tgkill(SIGKILL); the parent
 * observes that phase before checking the fatal wait status.  A parent
 * resumed during any userspace window still sees the old phase, so it cannot
 * pass the boundary checks.
 */

#include <errno.h>
#include <stdio.h>
#include <signal.h>
#include <stdlib.h>
#include <stdatomic.h>
#include <stdint.h>
#include <string.h>
#include <sched.h>
#include <sys/syscall.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define EXEC_TARGET_ENV "THEKERNEL_VFORK_EXEC_TARGET"
#define EXEC_TARGET_DEFAULT "/bin/sleep"
#define EXEC_DELAY_NS 100000000ULL
#define EXIT_PHASE_DELAY_NS 100000000ULL
#define SIGNAL_PHASE_DELAY_NS 100000000ULL
#define EXEC_IDENTITY_RETRY_NS 1000000ULL
#define EXEC_IDENTITY_RETRIES 100
#define SLEEP_LOWER_BOUND_TOLERANCE_NS (20ULL * 1000ULL * 1000ULL)
#define EXEC_MIN_ELAPSED_NS \
    (EXEC_DELAY_NS - SLEEP_LOWER_BOUND_TOLERANCE_NS)
#define EXIT_PHASE_MIN_ELAPSED_NS \
    (EXIT_PHASE_DELAY_NS - SLEEP_LOWER_BOUND_TOLERANCE_NS)
#define SIGNAL_PHASE_MIN_ELAPSED_NS \
    (SIGNAL_PHASE_DELAY_NS - SLEEP_LOWER_BOUND_TOLERANCE_NS)

static const struct timespec exit_phase_delay = {
    .tv_sec = 0,
    .tv_nsec = (long)EXIT_PHASE_DELAY_NS,
};
static const struct timespec exec_delay = {
    .tv_sec = 0,
    .tv_nsec = (long)EXEC_DELAY_NS,
};
static const struct timespec signal_phase_delay = {
    .tv_sec = 0,
    .tv_nsec = (long)SIGNAL_PHASE_DELAY_NS,
};
static const struct timespec exec_identity_retry = {
    .tv_sec = 0,
    .tv_nsec = (long)EXEC_IDENTITY_RETRY_NS,
};
static const char *exec_target_path;
static int exec_target_is_busybox;
static char *exec_argv[4];
static _Atomic int exit_phase;
static _Atomic int signal_phase;
static _Atomic int exec_phase;
extern char **environ;

static int fail(const char *stage)
{
    fprintf(stderr, "THEKERNEL_VFORK_FAIL %s errno=%d (%s)\n", stage, errno,
            strerror(errno));
    return 1;
}

_Noreturn static void child_exit(int status)
{
    syscall(SYS_exit, status);
    for (;;) {
    }
}

static int raw_monotonic_now(struct timespec *now)
{
    return syscall(SYS_clock_gettime, CLOCK_MONOTONIC, now) == 0 ? 0 : -1;
}

static int elapsed_at_least(const struct timespec *start,
                            const struct timespec *end,
                            uint64_t minimum_ns)
{
    if (end->tv_sec < start->tv_sec)
        return 0;

    uint64_t seconds = (uint64_t)(end->tv_sec - start->tv_sec);
    if (seconds > UINT64_MAX / 1000000000ULL)
        return 1;
    uint64_t elapsed = seconds * 1000000000ULL;
    uint64_t remainder;
    if (end->tv_nsec >= start->tv_nsec) {
        remainder = (uint64_t)(end->tv_nsec - start->tv_nsec);
    } else {
        if (elapsed < 1000000000ULL)
            return 0;
        elapsed -= 1000000000ULL;
        remainder = 1000000000ULL + (uint64_t)end->tv_nsec -
                    (uint64_t)start->tv_nsec;
    }
    if (elapsed > UINT64_MAX - remainder)
        return 1;
    return elapsed + remainder >= minimum_ns;
}

/* The lower bound is checked inside the child, so host log timing cannot
 * manufacture a pass.  A 20 ms tolerance covers timer granularity and
 * measurement overhead while still rejecting a collapsed 100 ms window. */
static int raw_nanosleep_with_lower_bound(const struct timespec *delay,
                                          uint64_t minimum_ns)
{
    struct timespec start;
    struct timespec end;
    struct timespec remaining = *delay;
    long result;

    if (raw_monotonic_now(&start) != 0)
        return -1;

    do {
        result = syscall(SYS_nanosleep, &remaining, &remaining);
    } while (result < 0 && errno == EINTR);
    if (result != 0 || raw_monotonic_now(&end) != 0)
        return -1;
    return elapsed_at_least(&start, &end, minimum_ns) ? 0 : -1;
}

static int pin_to_first_allowed_cpu(void)
{
    cpu_set_t allowed;
    cpu_set_t selected;

    CPU_ZERO(&allowed);
    if (sched_getaffinity(0, sizeof(allowed), &allowed) != 0)
        return -1;
    for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
        if (!CPU_ISSET(cpu, &allowed))
            continue;
        CPU_ZERO(&selected);
        CPU_SET(cpu, &selected);
        return sched_setaffinity(0, sizeof(selected), &selected);
    }
    errno = EINVAL;
    return -1;
}

static int wait_success(pid_t child, const char *stage)
{
    int status = 0;
    pid_t waited;

    do {
        waited = syscall(SYS_wait4, child, &status, 0, NULL);
    } while (waited < 0 && errno == EINTR);
    if (waited != child)
        return fail(stage);
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        errno = EPROTO;
        return fail(stage);
    }
    return 0;
}

static int wait_killed(pid_t child, const char *stage)
{
    int status = 0;
    pid_t waited;

    do {
        waited = syscall(SYS_wait4, child, &status, 0, NULL);
    } while (waited < 0 && errno == EINTR);
    if (waited != child)
        return fail(stage);
    if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGKILL) {
        errno = EPROTO;
        return fail(stage);
    }
    return 0;
}

static int proc_exe_identity(pid_t child, char *path, size_t path_size,
                             struct stat *identity)
{
    char link_path[64];
    int length = snprintf(link_path, sizeof(link_path), "/proc/%ld/exe",
                          (long)child);
    if (length < 0 || (size_t)length >= sizeof(link_path)) {
        errno = EOVERFLOW;
        return -1;
    }
    ssize_t count = readlink(link_path, path, path_size - 1U);
    if (count < 0)
        return -1;
    if ((size_t)count >= path_size - 1U) {
        errno = EOVERFLOW;
        return -1;
    }
    path[count] = '\0';
    return stat(link_path, identity);
}

static int test_exit_release(void)
{
    if (pin_to_first_allowed_cpu() != 0) {
        fprintf(stderr,
                "THEKERNEL_VFORK_EXIT_UNSUPPORTED affinity_errno=%d\n",
                errno);
        return 1;
    }

    atomic_store_explicit(&exit_phase, 0, memory_order_relaxed);

    pid_t child = vfork();
    if (child < 0)
        return fail("exit-vfork");
    if (child == 0) {
        atomic_store_explicit(&exit_phase, 1, memory_order_release);
        if (raw_nanosleep_with_lower_bound(&exit_phase_delay,
                                           EXIT_PHASE_MIN_ELAPSED_NS) != 0)
            child_exit(125);
        atomic_store_explicit(&exit_phase, 2, memory_order_release);
        if (raw_nanosleep_with_lower_bound(&exit_phase_delay,
                                           EXIT_PHASE_MIN_ELAPSED_NS) != 0)
            child_exit(125);
        atomic_store_explicit(&exit_phase, 3, memory_order_release);
        child_exit(0);
    }

    /* This must be the first parent-side operation after vfork returns.  A
     * blocking wait is cleanup only; it is deliberately after the phase
     * proof and makes no claim about immediate waitability. */
    int phase = atomic_load_explicit(&exit_phase, memory_order_acquire);
    if (phase != 3) {
        if (wait_success(child, "exit-early-cleanup") != 0)
            return 1;
        errno = EPROTO;
        return fail("exit-parent-resumed-before-exit-phase3");
    }
    if (wait_success(child, "exit-wait") != 0)
        return 1;
    puts("THEKERNEL_VFORK_EXIT_PHASE3_OK");
    return 0;
}

static int test_signal_release(void)
{
    if (pin_to_first_allowed_cpu() != 0) {
        fprintf(stderr,
                "THEKERNEL_VFORK_SIGNAL_UNSUPPORTED affinity_errno=%d\n",
                errno);
        return 1;
    }

    atomic_store_explicit(&signal_phase, 0, memory_order_relaxed);

    pid_t child = vfork();
    if (child < 0)
        return fail("signal-vfork");
    if (child == 0) {
        atomic_store_explicit(&signal_phase, 1, memory_order_release);
        if (raw_nanosleep_with_lower_bound(&signal_phase_delay,
                                           SIGNAL_PHASE_MIN_ELAPSED_NS) != 0)
            child_exit(125);
        atomic_store_explicit(&signal_phase, 2, memory_order_release);

        /* After the final publication, the vfork child must use only raw
         * syscalls: no libc wrapper may run before this fatal signal. */
        long tgid = syscall(SYS_getpid);
        long tid = syscall(SYS_gettid);
        if (tgid <= 0 || tid <= 0 ||
            syscall(SYS_tgkill, tgid, tid, SIGKILL) != 0)
            child_exit(125);
        child_exit(125);
    }

    /* This must be the first parent-side operation after vfork returns.  A
     * blocking wait is cleanup only; phase 2 is published immediately before
     * the raw fatal signal and therefore must not be observed early. */
    int phase = atomic_load_explicit(&signal_phase, memory_order_acquire);
    if (phase != 2) {
        if (wait_killed(child, "signal-early-cleanup") != 0)
            return 1;
        errno = EPROTO;
        return fail("signal-parent-resumed-before-sigkill");
    }
    if (wait_killed(child, "signal-wait") != 0)
        return 1;
    puts("THEKERNEL_VFORK_SIGNAL_KILL_OK");
    return 0;
}

static int test_exec_release(const char *self)
{
    const char *target = getenv(EXEC_TARGET_ENV);
    if (target == NULL || target[0] == '\0')
        target = EXEC_TARGET_DEFAULT;
    exec_target_path = target;
    const char *target_name = strrchr(target, '/');
    target_name = target_name == NULL ? target : target_name + 1;
    exec_target_is_busybox = strcmp(target_name, "busybox") == 0;
    exec_argv[0] = (char *)exec_target_path;
    if (exec_target_is_busybox) {
        exec_argv[1] = (char *)"sleep";
        exec_argv[2] = (char *)"1";
        exec_argv[3] = NULL;
    } else {
        exec_argv[1] = (char *)"1";
        exec_argv[2] = NULL;
        exec_argv[3] = NULL;
    }
    if (pin_to_first_allowed_cpu() != 0) {
        fprintf(stderr,
                "THEKERNEL_VFORK_EXEC_UNSUPPORTED affinity_errno=%d\n",
                errno);
        return 1;
    }

    struct stat parent_stat;
    struct stat target_stat;
    if (stat(self, &parent_stat) != 0 || stat(target, &target_stat) != 0) {
        return fail("exec-target-stat");
    }
    if (parent_stat.st_dev == target_stat.st_dev &&
        parent_stat.st_ino == target_stat.st_ino) {
        errno = EINVAL;
        return fail("exec-target-same-inode");
    }
    atomic_store_explicit(&exec_phase, 0, memory_order_relaxed);
    char child_exe[256] = "unobserved";
    struct stat child_stat = {0};
    pid_t child = vfork();
    if (child < 0)
        return fail("exec-vfork");
    if (child == 0) {
        if (raw_nanosleep_with_lower_bound(&exec_delay,
                                           EXEC_MIN_ELAPSED_NS) != 0)
            child_exit(125);
        atomic_store_explicit(&exec_phase, 1, memory_order_release);
        (void)syscall(SYS_execve, exec_target_path, exec_argv, environ);
        child_exit(127);
    }

    /* Linux releases the parent when the child calls execve, before the new
     * mm is necessarily visible through /proc/<pid>/exe.  The phase load is
     * therefore the immediate release-boundary proof; the image identity is
     * a separate bounded convergence check for successful exec completion. */
    int phase = atomic_load_explicit(&exec_phase, memory_order_acquire);
    if (phase != 1) {
        (void)kill(child, SIGKILL);
        (void)wait_killed(child, "exec-phase-cleanup");
        errno = EPROTO;
        return fail("exec-parent-resumed-before-exec");
    }

    int identity_matches = 0;
    for (int attempt = 0; attempt < EXEC_IDENTITY_RETRIES; ++attempt) {
        if (proc_exe_identity(child, child_exe, sizeof(child_exe),
                              &child_stat) != 0) {
            int saved_errno = errno;
            (void)kill(child, SIGKILL);
            (void)wait_killed(child, "exec-identity-cleanup");
            errno = saved_errno;
            return fail("exec-identity-read");
        }
        if (child_stat.st_dev == target_stat.st_dev &&
            child_stat.st_ino == target_stat.st_ino) {
            identity_matches = 1;
            break;
        }
        if (child_stat.st_dev != parent_stat.st_dev ||
            child_stat.st_ino != parent_stat.st_ino) {
            break;
        }
        (void)syscall(SYS_nanosleep, &exec_identity_retry, NULL);
    }
    fprintf(stderr,
            "thekernel_vfork: exec_identity target=%s observed=%s dev=%llu:%llu\n",
            exec_target_path, child_exe,
            (unsigned long long)child_stat.st_dev,
            (unsigned long long)child_stat.st_ino);
    if (!identity_matches) {
        (void)kill(child, SIGKILL);
        (void)wait_killed(child, "exec-identity-early-cleanup");
        errno = EPROTO;
        return fail("exec-identity-did-not-converge");
    }
    if (kill(child, SIGKILL) != 0) {
        int saved_errno = errno;
        (void)wait_killed(child, "exec-kill-cleanup");
        errno = saved_errno;
        return fail("exec-kill");
    }
    if (wait_killed(child, "exec-wait") != 0)
        return 1;
    puts("THEKERNEL_VFORK_EXEC_IDENTITY_OK");
    puts("THEKERNEL_VFORK_EXEC_BOUNDARY_OK");
    puts("THEKERNEL_VFORK_EXEC_OK");
    return 0;
}

int main(int argc, char **argv)
{
    if (argc != 1) {
        errno = EINVAL;
        return fail("arguments");
    }
    if (test_exit_release() != 0 || test_signal_release() != 0 ||
        test_exec_release(argv[0]) != 0)
        return 1;
    puts("THEKERNEL_VFORK_OK");
    return 0;
}
