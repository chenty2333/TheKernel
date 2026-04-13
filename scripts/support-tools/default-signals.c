#define _GNU_SOURCE
#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
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

int main(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr, "usage: %s <program> [args...]\n", argv[0]);
        return 127;
    }

    reset_signal_state();
    execvp(argv[1], &argv[1]);

    perror(argv[1]);
    return errno == ENOENT ? 127 : 126;
}
