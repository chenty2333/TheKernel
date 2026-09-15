#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* Separate process lifecycles let one worker publish a clone while another
 * retires an exiting task. Serial fork/wait loops in a single process cannot
 * exercise the global fs-publication/task-parent ABBA deadlock. */
static int concurrent_clone_exit(void)
{
    enum { WORKERS = 4, ROUNDS = 32 };
    int start[2];
    pid_t workers[WORKERS];
    unsigned started = 0;
    int failed = 0;
    if (pipe(start))
        return 1;
    for (; started < WORKERS; ++started) {
        pid_t worker = fork();
        if (worker < 0) {
            failed = 1;
            break;
        }
        if (!worker) {
            close(start[1]);
            char token;
            alarm(12);
            if (read(start[0], &token, 1) != 1)
                _exit(1);
            close(start[0]);
            for (unsigned round = 0; round < ROUNDS; ++round) {
                unsigned long flags = SIGCHLD;
                if (round & 1)
                    flags |= CLONE_FS;
                pid_t child = (pid_t)syscall(SYS_clone, flags, 0, 0, 0, 0);
                if (child < 0)
                    _exit(2);
                if (!child) {
                    syscall((round & 1) ? SYS_exit : SYS_exit_group, 37);
                    _exit(127);
                }
                int status = -1;
                pid_t reaped;
                do {
                    reaped = waitpid(child, &status, 0);
                } while (reaped < 0 && errno == EINTR);
                if (reaped != child || status != (37 << 8))
                    _exit(3);
            }
            _exit(0);
        }
        workers[started] = worker;
    }
    close(start[0]);
    if (!failed && write(start[1], "go!!", WORKERS) != WORKERS)
        failed = 1;
    close(start[1]);
    for (unsigned i = 0; i < started; ++i) {
        int status = -1;
        pid_t reaped;
        do {
            reaped = waitpid(workers[i], &status, 0);
        } while (reaped < 0 && errno == EINTR);
        if (reaped != workers[i] || status != 0)
            failed = 1;
    }
    if (failed)
        fprintf(stderr, "THEKERNEL_EXIT_STATUS_FAIL concurrent-clone-exit\n");
    return failed;
}

static int late_worker(void *unused)
{
    (void)unused;
    struct timespec delay = {.tv_nsec = 50000000};
    syscall(SYS_nanosleep, &delay, NULL);
    syscall(SYS_exit, 0);
    return 0;
}

static int leader_exits_first(void)
{
    pid_t child = fork();
    if (child < 0) return 1;
    if (!child) {
        void *stack = malloc(65536);
        if (!stack || clone(late_worker, (char *)stack + 65536,
                CLONE_VM | CLONE_SIGHAND | CLONE_THREAD, NULL) < 0)
            _exit(127);
        syscall(SYS_exit, 42);
        _exit(127);
    }
    int status = -1;
    if (waitpid(child, &status, 0) != child || status != 0) {
        fprintf(stderr, "THEKERNEL_EXIT_STATUS_FAIL leader-first status=%x\n", status);
        return 1;
    }
    return 0;
}

int main(void)
{
    const int values[] = {0, 0x123456ab, -1, -2147483647 - 1};
    const long calls[] = {SYS_exit, SYS_exit_group};
    alarm(15);
    for (unsigned c = 0; c < sizeof(calls) / sizeof(calls[0]); ++c) {
        for (unsigned v = 0; v < sizeof(values) / sizeof(values[0]); ++v) {
            pid_t child = fork();
            if (child < 0)
                return 1;
            if (!child) {
                syscall(calls[c], values[v]);
                _exit(127);
            }
            int status = -1;
            if (waitpid(child, &status, 0) != child ||
                status != ((values[v] & 0xff) << 8)) {
                fprintf(stderr, "THEKERNEL_EXIT_STATUS_FAIL syscall=%ld value=%d status=%x\n",
                        calls[c], values[v], status);
                return 1;
            }
        }
    }
    if (leader_exits_first() || concurrent_clone_exit())
        return 1;
    alarm(0);
    puts("THEKERNEL_EXIT_STATUS_OK");
    return 0;
}
