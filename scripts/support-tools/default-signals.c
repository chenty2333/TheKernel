#define _GNU_SOURCE
#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <unistd.h>

static void reset_signal_state(void)
{
    sigset_t empty;
    struct sigaction sa;

    sigemptyset(&empty);
    sigprocmask(SIG_SETMASK, &empty, NULL);

    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = SIG_DFL;
    sigemptyset(&sa.sa_mask);

    for (int signo = 1; signo <= SIGRTMAX; ++signo) {
        if (signo == SIGKILL || signo == SIGSTOP) {
            continue;
        }
        sigaction(signo, &sa, NULL);
    }
}

static void reset_exec_context(void)
{
    struct sched_param param;
    cpu_set_t mask;
    long cpu_count;

    memset(&param, 0, sizeof(param));
    sched_setscheduler(0, SCHED_OTHER, &param);

    setpriority(PRIO_PROCESS, 0, 0);

    CPU_ZERO(&mask);
    cpu_count = sysconf(_SC_NPROCESSORS_ONLN);
    if (cpu_count < 1) {
        cpu_count = 1;
    }
    if (cpu_count > CPU_SETSIZE) {
        cpu_count = CPU_SETSIZE;
    }
    for (long cpu = 0; cpu < cpu_count; ++cpu) {
        CPU_SET((int)cpu, &mask);
    }
    sched_setaffinity(0, sizeof(mask), &mask);
}

int main(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr, "usage: %s <program> [args...]\n", argv[0]);
        return 127;
    }

    reset_signal_state();
    reset_exec_context();
    execvp(argv[1], &argv[1]);

    perror(argv[1]);
    return errno == ENOENT ? 127 : 126;
}
