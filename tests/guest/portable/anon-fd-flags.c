#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/inotify.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <sys/signalfd.h>
#include <sys/timerfd.h>
#include <unistd.h>

static int check_flags(int fd, int nonblock, int cloexec) {
    int status = fcntl(fd, F_GETFL);
    int descriptor = fcntl(fd, F_GETFD);
    if (status < 0 || descriptor < 0 ||
        !!(status & O_NONBLOCK) != nonblock ||
        !!(descriptor & FD_CLOEXEC) != cloexec) {
        fprintf(stderr, "anon-fd flags fd=%d status=%x descriptor=%x\n",
                fd, status, descriptor);
        return 1;
    }
    return 0;
}

static int empty_read(int fd) {
    char buffer[4096];
    errno = 0;
    ssize_t result = read(fd, buffer, sizeof(buffer));
    if (result != -1 || errno != EAGAIN) {
        fprintf(stderr, "anon-fd empty read fd=%d result=%zd errno=%d\n",
                fd, result, errno);
        return 1;
    }
    return 0;
}

static int receive_signal(int fd, int mode, int signo) {
    if (mode == 0) {
        struct pollfd pfd = { .fd = fd, .events = POLLIN };
        if (poll(&pfd, 1, -1) != 1 || !(pfd.revents & POLLIN))
            return 1;
    } else if (mode == 1) {
        int ep = epoll_create1(EPOLL_CLOEXEC);
        struct epoll_event interest = { .events = EPOLLIN, .data.fd = fd };
        struct epoll_event result;
        if (ep < 0 || epoll_ctl(ep, EPOLL_CTL_ADD, fd, &interest) ||
            epoll_wait(ep, &result, 1, -1) != 1 ||
            result.data.fd != fd || !(result.events & EPOLLIN))
            return 1;
        close(ep);
    }
    struct signalfd_siginfo info;
    return read(fd, &info, sizeof(info)) != sizeof(info) ||
           info.ssi_signo != (unsigned)signo;
}

static int reap_ok(pid_t child) {
    int status = -1;
    return waitpid(child, &status, 0) != child || status != 0;
}

/* Signal generation follows wait setup, rather than pre-filling a queue and
 * only testing poll's immediate readiness check. Alarm is a failure bound;
 * the waits themselves have no periodic timeout or retry to hide a lost wake. */
static int asynchronous_signalfd(void) {
    const struct timespec delay = { .tv_nsec = 100000000 };
    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGCHLD);
    if (sigprocmask(SIG_BLOCK, &blocked, NULL))
        return 1;
    for (int mode = 0; mode < 3; ++mode) {
        int signo = mode == 0 ? SIGCHLD : SIGUSR1;
        sigset_t mask;
        sigemptyset(&mask);
        sigaddset(&mask, signo);
        int fd = signalfd(-1, &mask, SFD_CLOEXEC | (mode == 2 ? 0 : SFD_NONBLOCK));
        int start[2];
        if (fd < 0 || pipe(start))
            return 1;
        pid_t parent = getpid();
        pid_t parent_tid = (pid_t)syscall(SYS_gettid);
        pid_t child = fork();
        if (child < 0)
            return 1;
        if (!child) {
            char token;
            close(start[1]);
            if (read(start[0], &token, 1) != 1 || nanosleep(&delay, NULL))
                _exit(1);
            if (mode == 1 && kill(parent, SIGUSR1))
                _exit(2);
            if (mode == 2 && syscall(SYS_tgkill, parent, parent_tid, SIGUSR1))
                _exit(3);
            _exit(0);
        }
        close(start[0]);
        struct timespec before, after;
        if (clock_gettime(CLOCK_MONOTONIC, &before) || write(start[1], "x", 1) != 1)
            return 1;
        close(start[1]);
        if (receive_signal(fd, mode, signo) ||
            clock_gettime(CLOCK_MONOTONIC, &after) || reap_ok(child))
            return 1;
        long long elapsed = (after.tv_sec - before.tv_sec) * 1000000000LL +
                            after.tv_nsec - before.tv_nsec;
        if (elapsed < 50000000)
            return 1;
        close(fd);
    }

    /* A shared OFD follows the child reader's queues. The parent's already
     * pending signal is not inherited and must survive the child's read. */
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGUSR1);
    int fd = signalfd(-1, &mask, SFD_NONBLOCK | SFD_CLOEXEC);
    int ready[2];
    if (fd < 0 || pipe(ready) || kill(getpid(), SIGUSR1))
        return 1;
    pid_t child = fork();
    if (child < 0)
        return 1;
    if (!child) {
        close(ready[0]);
        if (empty_read(fd) || write(ready[1], "x", 1) != 1)
            _exit(1);
        close(ready[1]);
        _exit(receive_signal(fd, 1, SIGUSR1));
    }
    char token;
    close(ready[1]);
    if (read(ready[0], &token, 1) != 1 || nanosleep(&delay, NULL) ||
        kill(child, SIGUSR1) || reap_ok(child) || receive_signal(fd, 0, SIGUSR1))
        return 1;
    close(ready[0]);
    close(fd);
    return 0;
}

int main(void) {
    /* A missing nonblocking OFD flag must fail instead of hanging the suite. */
    alarm(10);
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGUSR1);
    if (sigprocmask(SIG_BLOCK, &mask, NULL) != 0)
        return 1;
    int signal_fd = signalfd(-1, &mask, SFD_NONBLOCK | SFD_CLOEXEC);
    int timer_fd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK | TFD_CLOEXEC);
    int notify_fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    int fds[] = {signal_fd, timer_fd, notify_fd};
    for (unsigned int i = 0; i < sizeof(fds) / sizeof(fds[0]); ++i) {
        if (fds[i] < 0 || check_flags(fds[i], 1, 1) || empty_read(fds[i]))
            return 1;
    }
    /* Updating a signalfd mask never changes existing creation flags. */
    if (signalfd(signal_fd, &mask, 0) != signal_fd ||
        check_flags(signal_fd, 1, 1) || empty_read(signal_fd))
        return 1;
    int blocking_fd = signalfd(-1, &mask, 0);
    if (blocking_fd < 0 ||
        signalfd(blocking_fd, &mask, SFD_NONBLOCK | SFD_CLOEXEC) != blocking_fd ||
        check_flags(blocking_fd, 0, 0))
        return 1;
    if (close(blocking_fd) != 0)
        return 1;
    for (unsigned int i = 0; i < sizeof(fds) / sizeof(fds[0]); ++i)
        if (close(fds[i]) != 0)
            return 1;
    if (asynchronous_signalfd()) {
        fprintf(stderr, "anon-fd asynchronous signalfd failed errno=%d\n", errno);
        return 1;
    }
    alarm(0);
    puts("THEKERNEL_ANON_FD_FLAGS_OK");
    return 0;
}
