#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

/* PID1 is the sole initial member. Children remain alive until the failed
 * fork is observed; success cannot be confused with children exiting early. */
static int pids_probe(void) {
    int hold[2]; pid_t children[8]; size_t count = 0;
    if (pipe(hold) != 0) return 1;
    while (count < 8) {
        errno = 0; pid_t child = fork();
        if (child == -1) break;
        if (!child) {
            close(hold[1]); char byte;
            while (read(hold[0], &byte, 1) == -1 && errno == EINTR) {}
            close(hold[0]); _exit(0);
        }
        children[count++] = child;
    }
    int limited = count == 7 && errno == EAGAIN;
    close(hold[0]); close(hold[1]);
    for (size_t i = 0; i < count; ++i) {
        int status;
        if (waitpid(children[i], &status, 0) != children[i] ||
            !WIFEXITED(status) || WEXITSTATUS(status)) limited = 0;
    }
    if (!limited) { fprintf(stderr, "pids limit mismatch: children=%zu\n", count); return 1; }
    puts("CRUN_PIDS_FORK_EAGAIN_OK children=7 limit=8");
    return 0;
}
int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "pids")) return pids_probe();
    return 2;
}
