#define _GNU_SOURCE

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t usr1_seen;
static volatile sig_atomic_t alrm_seen;
static sigset_t usr1_mask;

static void usr1_handler(int signo)
{
    (void)signo;
    usr1_seen++;
}

static void alrm_handler(int signo)
{
    (void)signo;
    alrm_seen++;
}

static int fail(const char *stage)
{
    fprintf(stderr, "CI_PAUSE_FAIL %s errno=%d (%s)\n", stage, errno,
            strerror(errno));
    return 1;
}

static long raw_pause(void)
{
    return syscall(SYS_pause);
}

static long raw_kill(pid_t pid, int signo)
{
    return syscall(SYS_kill, pid, signo);
}

static long raw_fork(void)
{
    return syscall(SYS_fork);
}

static long raw_wait4(pid_t pid, int *status)
{
    return syscall(SYS_wait4, pid, status, 0, NULL);
}

static long raw_setitimer(const struct itimerval *value)
{
    return syscall(SYS_setitimer, ITIMER_REAL, value, NULL);
}

static long raw_nanosleep(long nanoseconds)
{
    struct timespec request;

    request.tv_sec = 0;
    request.tv_nsec = nanoseconds;
    return syscall(SYS_nanosleep, &request, NULL);
}

static void child_exit(int status)
{
    syscall(SYS_exit, status);
    for (;;) {
    }
}

static int install_handlers(void)
{
    struct sigaction action;

    memset(&action, 0, sizeof(action));
    action.sa_handler = usr1_handler;
    action.sa_flags = SA_RESTART;
    if (sigemptyset(&action.sa_mask) != 0 ||
        sigaction(SIGUSR1, &action, NULL) != 0)
        return fail("usr1-handler");

    memset(&action, 0, sizeof(action));
    action.sa_handler = alrm_handler;
    if (sigemptyset(&action.sa_mask) != 0 ||
        sigaction(SIGALRM, &action, NULL) != 0)
        return fail("alrm-handler");
    return 0;
}

static int wait_child(pid_t child, int *status, const char *stage)
{
    if (raw_wait4(child, status) != child)
        return fail(stage);
    return 0;
}

static int test_handler_eintr(void)
{
    pid_t child;
    int status;
    long result;
    int saved_errno;

    usr1_seen = 0;
    child = (pid_t)raw_fork();
    if (child < 0)
        return fail("restart-fork");
    if (child == 0) {
        if (raw_kill((pid_t)syscall(SYS_getppid), SIGUSR1) != 0)
            child_exit(2);
        child_exit(0);
    }

    errno = 0;
    result = raw_pause();
    saved_errno = errno;
    if (wait_child(child, &status, "restart-wait") != 0)
        return 1;
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0 || result != -1 ||
        saved_errno != EINTR || usr1_seen != 1) {
        errno = saved_errno;
        return fail("restart-eintr");
    }
    return 0;
}

static int test_blocked_then_unblock(void)
{
    struct itimerval timer;
    sigset_t pending;
    sigset_t old_mask;
    long result;
    int saved_errno;

    if (sigprocmask(SIG_BLOCK, &usr1_mask, &old_mask) != 0)
        return fail("blocked-install");
    usr1_seen = 0;
    alrm_seen = 0;
    if (raw_kill((pid_t)syscall(SYS_getpid), SIGUSR1) != 0) {
        sigprocmask(SIG_SETMASK, &old_mask, NULL);
        return fail("blocked-queue");
    }

    memset(&timer, 0, sizeof(timer));
    timer.it_value.tv_usec = 50000;
    if (raw_setitimer(&timer) != 0) {
        sigprocmask(SIG_SETMASK, &old_mask, NULL);
        return fail("blocked-timer-arm");
    }
    errno = 0;
    result = raw_pause();
    saved_errno = errno;

    memset(&timer, 0, sizeof(timer));
    if (raw_setitimer(&timer) != 0) {
        sigprocmask(SIG_SETMASK, &old_mask, NULL);
        return fail("blocked-timer-clear");
    }
    if (result != -1 || saved_errno != EINTR || alrm_seen != 1 ||
        usr1_seen != 0) {
        sigprocmask(SIG_SETMASK, &old_mask, NULL);
        errno = saved_errno;
        return fail("blocked-does-not-wake");
    }
    if (sigpending(&pending) != 0 || sigismember(&pending, SIGUSR1) != 1) {
        sigprocmask(SIG_SETMASK, &old_mask, NULL);
        return fail("blocked-remains-pending");
    }

    if (sigprocmask(SIG_UNBLOCK, &usr1_mask, NULL) != 0) {
        sigprocmask(SIG_SETMASK, &old_mask, NULL);
        return fail("blocked-unblock");
    }
    if (usr1_seen != 1) {
        sigprocmask(SIG_SETMASK, &old_mask, NULL);
        errno = EPROTO;
        return fail("blocked-delivery-after-unblock");
    }
    if (sigprocmask(SIG_SETMASK, &old_mask, NULL) != 0)
        return fail("blocked-restore");
    return 0;
}

static int test_stop_continue(void)
{
    pid_t child;
    int status;
    long result;
    int saved_errno;

    usr1_seen = 0;
    child = (pid_t)raw_fork();
    if (child < 0)
        return fail("stop-fork");
    if (child == 0) {
        pid_t parent = (pid_t)syscall(SYS_getppid);

        if (raw_nanosleep(20000000) != 0 || raw_kill(parent, SIGSTOP) != 0)
            child_exit(2);
        if (raw_nanosleep(20000000) != 0 || raw_kill(parent, SIGCONT) != 0)
            child_exit(3);
        if (raw_nanosleep(20000000) != 0 || raw_kill(parent, SIGUSR1) != 0)
            child_exit(4);
        child_exit(0);
    }

    errno = 0;
    result = raw_pause();
    saved_errno = errno;
    if (wait_child(child, &status, "stop-wait") != 0)
        return 1;
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0 || result != -1 ||
        saved_errno != EINTR || usr1_seen != 1) {
        errno = saved_errno;
        return fail("stop-continue-eintr");
    }
    return 0;
}

static int test_fatal_child(void)
{
    pid_t child;
    int status;

    child = (pid_t)raw_fork();
    if (child < 0)
        return fail("fatal-fork");
    if (child == 0) {
        if (raw_nanosleep(20000000) != 0)
            child_exit(2);
        errno = 0;
        if (raw_pause() != -1 || errno != EINTR)
            child_exit(3);
        child_exit(4);
    }
    if (raw_nanosleep(40000000) != 0 || raw_kill(child, SIGTERM) != 0)
        return fail("fatal-send");
    if (wait_child(child, &status, "fatal-wait") != 0)
        return 1;
    if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGTERM) {
        errno = EPROTO;
        return fail("fatal-observable");
    }
    return 0;
}

int main(void)
{
    sigset_t empty;

    if (sigemptyset(&empty) != 0 || sigprocmask(SIG_SETMASK, &empty, NULL) != 0)
        return fail("initial-mask");
    if (sigemptyset(&usr1_mask) != 0 || sigaddset(&usr1_mask, SIGUSR1) != 0)
        return fail("usr1-mask");
    if (install_handlers() != 0 || test_handler_eintr() != 0 ||
        test_blocked_then_unblock() != 0 || test_stop_continue() != 0 ||
        test_fatal_child() != 0)
        return 1;

    puts("CI_PAUSE_PASS");
    return 0;
}
