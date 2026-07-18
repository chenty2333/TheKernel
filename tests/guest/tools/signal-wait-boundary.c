#define _GNU_SOURCE

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t usr1_seen;
static volatile sig_atomic_t usr2_seen;
static volatile sig_atomic_t usr2_saw_usr1_blocked;

static void handler(int signo)
{
    if (signo == SIGUSR1)
        usr1_seen++;
    if (signo == SIGUSR2) {
        sigset_t current;
        int saved_errno = errno;

        usr2_seen++;
        if (sigprocmask(SIG_SETMASK, NULL, &current) != 0)
            usr2_saw_usr1_blocked = -1;
        else
            usr2_saw_usr1_blocked = sigismember(&current, SIGUSR1);
        errno = saved_errno;
    }
}

static int fail(const char *stage)
{
    printf("CI_SIGNAL_WAIT_BOUNDARY_FAIL %s errno=%d\n", stage, errno);
    return 1;
}

static int mask_has(int signo)
{
    sigset_t current;

    if (sigprocmask(SIG_SETMASK, NULL, &current) != 0)
        return -1;
    return sigismember(&current, signo);
}

static long long monotonic_ns(const struct timespec *value)
{
    return (long long)value->tv_sec * 1000000000LL + value->tv_nsec;
}

int main(void)
{
    struct sigaction action;
    struct sigevent async_event;
    struct itimerspec async_value;
    struct timespec async_timeout;
    struct timespec elapsed_end;
    struct timespec elapsed_start;
    struct timespec elapsed_timeout;
    struct timespec timeout;
    siginfo_t info;
    sigset_t original;
    sigset_t blocked;
    sigset_t forbidden;
    sigset_t waited;
    sigset_t suspend_mask;
    timer_t async_timer;
    int result;
    int saved_errno;
    long raw_result;

    memset(&action, 0, sizeof(action));
    action.sa_handler = handler;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR1, &action, NULL) != 0)
        return fail("sigaction");
    if (sigaction(SIGUSR2, &action, NULL) != 0)
        return fail("sigaction-async");

    if (sigprocmask(SIG_SETMASK, NULL, &original) != 0)
        return fail("save-mask");
    blocked = original;
    sigaddset(&blocked, SIGUSR1);
    if (sigprocmask(SIG_SETMASK, &blocked, NULL) != 0)
        return fail("install-mask");

    sigemptyset(&waited);
    sigaddset(&waited, SIGUSR1);
    timeout.tv_sec = 0;
    timeout.tv_nsec = 0;

    /* A synchronously accepted pending signal must beat a zero timeout. */
    if (kill(getpid(), SIGUSR1) != 0)
        return fail("queue-ready");
    memset(&info, 0, sizeof(info));
    result = sigtimedwait(&waited, &info, &timeout);
    if (result != SIGUSR1 || info.si_signo != SIGUSR1 || usr1_seen != 0) {
        errno = EPROTO;
        return fail("ready-before-timeout");
    }
    if (mask_has(SIGUSR1) != 1)
        return fail("ready-mask-restore");

    /* With no pending member, the same zero timeout must report EAGAIN. */
    errno = 0;
    result = sigtimedwait(&waited, NULL, &timeout);
    if (result != -1 || errno != EAGAIN)
        return fail("empty-timeout");

    /* Exercise a real timer reservation and block/wake cycle. The lower bound
     * is intentionally well below the requested 50 ms so scheduler and QEMU
     * jitter cannot turn this into a fragile performance assertion. */
    elapsed_timeout.tv_sec = 0;
    elapsed_timeout.tv_nsec = 50000000;
    if (clock_gettime(CLOCK_MONOTONIC, &elapsed_start) != 0)
        return fail("elapsed-clock-start");
    errno = 0;
    result = sigtimedwait(&waited, NULL, &elapsed_timeout);
    saved_errno = errno;
    if (clock_gettime(CLOCK_MONOTONIC, &elapsed_end) != 0)
        return fail("elapsed-clock-end");
    if (result != -1 || saved_errno != EAGAIN ||
        monotonic_ns(&elapsed_end) - monotonic_ns(&elapsed_start) < 10000000LL) {
        printf("CI_SIGNAL_WAIT_BOUNDARY_DIAG elapsed result=%d saved_errno=%d elapsed_ns=%lld\n",
               result, saved_errno,
               monotonic_ns(&elapsed_end) - monotonic_ns(&elapsed_start));
        errno = saved_errno;
        return fail("nonzero-timeout");
    }

    /* Exercise the raw ABI so libc cannot pre-sanitize the set. Linux never
     * permits SIGKILL or SIGSTOP to be synchronously accepted. With neither
     * pending, the sanitized empty selection must report EAGAIN. */
    sigemptyset(&forbidden);
    sigaddset(&forbidden, SIGKILL);
    sigaddset(&forbidden, SIGSTOP);
    errno = 0;
    raw_result = syscall(SYS_rt_sigtimedwait, &forbidden, NULL, &timeout,
                         sizeof(unsigned long));
    if (raw_result != -1 || errno != EAGAIN)
        return fail("kill-stop-excluded");

    /* A caught signal outside the selected set owns EINTR at the Linux syscall
     * ABI. Use the raw syscall here because libc policy is not uniform: musl's
     * sigtimedwait() deliberately retries EINTR while glibc exposes it. A
     * one-shot POSIX timer publishes the signal after this task has entered the
     * wait, exercising a real timer-to-signal wake without helper scheduling. */
    memset(&async_event, 0, sizeof(async_event));
    async_event.sigev_notify = SIGEV_SIGNAL;
    async_event.sigev_signo = SIGUSR2;
    if (timer_create(CLOCK_MONOTONIC, &async_event, &async_timer) != 0)
        return fail("async-timer-create");
    memset(&async_value, 0, sizeof(async_value));
    async_value.it_value.tv_nsec = 20000000;
    if (timer_settime(async_timer, 0, &async_value, NULL) != 0)
        return fail("async-timer-arm");

    async_timeout.tv_sec = 1;
    async_timeout.tv_nsec = 0;
    usr2_seen = 0;
    usr2_saw_usr1_blocked = 0;
    errno = 0;
    result = (int)syscall(SYS_rt_sigtimedwait, &waited, NULL, &async_timeout,
                          sizeof(unsigned long));
    saved_errno = errno;
    if (timer_delete(async_timer) != 0)
        return fail("async-timer-delete");
    if (result != -1 || saved_errno != EINTR || usr2_seen != 1 ||
        usr2_saw_usr1_blocked != 1) {
        printf("CI_SIGNAL_WAIT_BOUNDARY_DIAG nonselected result=%d saved_errno=%d seen=%d waited_blocked_in_handler=%d\n",
               result, saved_errno, (int)usr2_seen,
               (int)usr2_saw_usr1_blocked);
        errno = saved_errno;
        return fail("nonselected-eintr");
    }
    if (mask_has(SIGUSR1) != 1)
        return fail("nonselected-mask-restore");

    /* sigsuspend hands restoration to the handler frame and returns EINTR
     * only after the caught handler has run. Queueing while SIGUSR1 is
     * blocked also proves the mask replacement and wait admission are atomic
     * to userspace: the already-pending signal must not be lost. */
    sigemptyset(&suspend_mask);
    usr1_seen = 0;
    if (kill(getpid(), SIGUSR1) != 0)
        return fail("queue-sigsuspend");
    errno = 0;
    result = sigsuspend(&suspend_mask);
    saved_errno = errno;
    if (result != -1 || saved_errno != EINTR || usr1_seen != 1) {
        printf("CI_SIGNAL_WAIT_BOUNDARY_DIAG sigsuspend-handler result=%d saved_errno=%d seen=%d\n",
               result, saved_errno, (int)usr1_seen);
        errno = saved_errno;
        return fail("sigsuspend-handler");
    }
    if (mask_has(SIGUSR1) != 1)
        return fail("sigsuspend-mask-restore");

    if (sigprocmask(SIG_SETMASK, &original, NULL) != 0)
        return fail("final-mask-restore");

    puts("CI_SIGNAL_WAIT_BOUNDARY_PASS");
    return 0;
}
