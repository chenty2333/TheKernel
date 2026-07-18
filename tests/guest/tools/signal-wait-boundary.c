#define _GNU_SOURCE

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t usr1_seen;

static void handler(int signo)
{
    if (signo == SIGUSR1)
        usr1_seen++;
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

int main(void)
{
    struct sigaction action;
    struct timespec timeout;
    siginfo_t info;
    sigset_t original;
    sigset_t blocked;
    sigset_t waited;
    sigset_t suspend_mask;
    int result;
    int saved_errno;

    memset(&action, 0, sizeof(action));
    action.sa_handler = handler;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR1, &action, NULL) != 0)
        return fail("sigaction");

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
