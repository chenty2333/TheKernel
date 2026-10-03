#define _GNU_SOURCE

#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

/* Match Vanilla Conquer's missing-data fault without SDL or game assets.
 * Keep the call indirect so the compiler does not replace the bad access
 * with a different trap or optimize away an unused destination. */
static void fault(void)
{
    char destination[768];
    void *(*volatile copy)(void *, const void *, size_t) = memmove;

    copy(destination, NULL, sizeof(destination));
    _exit(98);
}

static int ready[2];
static int park[2];

static void *peer(void *arg)
{
    char byte = 0;

    if (write(ready[1], &byte, 1) != 1)
        _exit(97);
    if (arg != NULL)
        fault();
    /* The leader's fatal fault must also terminate a sleeping peer. */
    if (read(park[0], &byte, 1) != 1)
        _exit(96);
    _exit(95);
}

static int run_case(int mode)
{
    pid_t child = fork();
    int status = 0;

    if (child < 0)
        return 1;
    if (child == 0) {
        struct rlimit limit = {0, 0};
        pthread_t thread;
        char byte;

        /* Test signal/group-exit, not dump I/O or host core policy. */
        if (setrlimit(RLIMIT_CORE, &limit) != 0)
            _exit(94);
        if (mode == 0)
            fault();
        if (pipe(ready) != 0 || pipe(park) != 0 ||
            pthread_create(&thread, NULL, peer, mode == 2 ? &byte : NULL) != 0)
            _exit(93);
        if (read(ready[0], &byte, 1) != 1)
            _exit(92);
        if (mode == 1)
            fault();
        /* A fault in a non-leader must kill the blocked leader as well. */
        if (read(park[0], &byte, 1) != 1)
            _exit(91);
        _exit(90);
    }
    if (waitpid(child, &status, 0) != child ||
        !WIFSIGNALED(status) || WTERMSIG(status) != SIGSEGV) {
        fprintf(stderr, "THEKERNEL_FATAL_FAULT_FAIL mode=%d status=%d\n", mode, status);
        return 1;
    }
    printf("THEKERNEL_FATAL_FAULT_CASE mode=%d signal=%d\n", mode, WTERMSIG(status));
    return 0;
}

int main(void)
{
    setbuf(stdout, NULL);
    alarm(20);
    for (int mode = 0; mode < 2; mode++) {
        if (run_case(mode) != 0)
            return 1;
    }
    /* A non-leader's SIGKILL wake can race the leader's ready-byte return
     * and its next blocking read. Repeat that crossing, not merely the
     * already-blocked-peer case. The watchdog still covers the whole run. */
    for (int round = 0; round < 32; round++) {
        if (run_case(2) != 0)
            return 1;
    }
    alarm(0);
    puts("THEKERNEL_FATAL_FAULT_OK");
    return 0;
}
