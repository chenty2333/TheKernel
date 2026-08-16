#define _GNU_SOURCE

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>

static volatile sig_atomic_t sigalrm_seen;

static void alarm_handler(int signo)
{
    if (signo == SIGALRM)
        sigalrm_seen++;
}

static int fail(const char *stage)
{
    fprintf(stderr, "THEKERNEL_ALARM_FAIL %s errno=%d (%s)\n", stage, errno,
            strerror(errno));
    return 1;
}

static long raw_alarm(unsigned int seconds)
{
    return syscall(SYS_alarm, seconds);
}

static long raw_setitimer(const struct itimerval *new_value,
                          struct itimerval *old_value)
{
    return syscall(SYS_setitimer, ITIMER_REAL, new_value, old_value);
}

static int in_range(long value, long minimum, long maximum)
{
    return value >= minimum && value <= maximum;
}

static int test_alarm_reset_and_cancel(void)
{
    long previous = raw_alarm(2);
    if (previous != 0)
        return fail("reset-first-arm");

    previous = raw_alarm(2);
    if (!in_range(previous, 1, 2))
        return fail("reset-return");
    puts("THEKERNEL_ALARM_RESET_OK");

    previous = raw_alarm(0);
    if (!in_range(previous, 1, 2))
        return fail("cancel-return");
    if (raw_alarm(0) != 0)
        return fail("cancel-empty");
    puts("THEKERNEL_ALARM_CANCEL_OK");
    return 0;
}

static int test_setitimer_replacement(void)
{
    struct itimerval replacement;
    struct itimerval previous;

    memset(&replacement, 0, sizeof(replacement));
    replacement.it_value.tv_sec = 5;
    if (raw_alarm(5) != 0)
        return fail("setitimer-swap-alarm-arm");
    memset(&previous, 0, sizeof(previous));
    if (raw_setitimer(&replacement, &previous) != 0)
        return fail("setitimer-swap-alarm");
    if (previous.it_interval.tv_sec != 0 || previous.it_interval.tv_usec != 0 ||
        !in_range(previous.it_value.tv_sec, 4, 5) ||
        previous.it_value.tv_usec < 0 || previous.it_value.tv_usec >= 1000000)
        return fail("setitimer-old-alarm");

    memset(&replacement, 0, sizeof(replacement));
    replacement.it_value.tv_sec = 5;
    if (raw_setitimer(&replacement, NULL) != 0)
        return fail("setitimer-swap-arm");
    if (!in_range(raw_alarm(1), 4, 5))
        return fail("alarm-old-setitimer");
    if (raw_alarm(0) != 1)
        return fail("alarm-setitimer-cancel");

    memset(&replacement, 0, sizeof(replacement));
    if (raw_setitimer(&replacement, NULL) != 0)
        return fail("setitimer-swap-clear");
    puts("THEKERNEL_ALARM_SETITIMER_SWAP_OK");
    return 0;
}

static int test_alarm_handler(void)
{
    struct sigaction action;
    sigset_t blocked_mask;
    sigset_t old_mask;
    sigset_t wait_mask;

    memset(&action, 0, sizeof(action));
    action.sa_handler = alarm_handler;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGALRM, &action, NULL) != 0)
        return fail("handler-install");

    sigemptyset(&blocked_mask);
    sigaddset(&blocked_mask, SIGALRM);
    if (sigprocmask(SIG_BLOCK, &blocked_mask, &old_mask) != 0)
        return fail("handler-block");

    sigalrm_seen = 0;
    if (raw_alarm(1) != 0) {
        sigprocmask(SIG_SETMASK, &old_mask, NULL);
        return fail("handler-arm");
    }
    wait_mask = old_mask;
    sigdelset(&wait_mask, SIGALRM);
    errno = 0;
    if (sigsuspend(&wait_mask) != -1 || errno != EINTR || sigalrm_seen != 1) {
        sigprocmask(SIG_SETMASK, &old_mask, NULL);
        return fail("handler-delivery");
    }
    if (sigprocmask(SIG_SETMASK, &old_mask, NULL) != 0)
        return fail("handler-unblock");
    if (raw_alarm(0) != 0)
        return fail("handler-clear");
    puts("THEKERNEL_ALARM_HANDLER_OK");
    return 0;
}

int main(void)
{
    if (raw_alarm(0) != 0)
        return fail("first-zero");
    puts("THEKERNEL_ALARM_FIRST_ZERO_OK");
    if (test_alarm_reset_and_cancel() || test_setitimer_replacement() ||
        test_alarm_handler())
        return 1;
    puts("THEKERNEL_ALARM_OK");
    return 0;
}
