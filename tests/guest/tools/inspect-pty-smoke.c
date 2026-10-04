#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc < 2) return 1;
    int master = posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC);
    if (master < 0 || grantpt(master) || unlockpt(master)) return 1;
    char *name = ptsname(master);
    if (!name) return 1;
    pid_t child = fork();
    if (child < 0) return 1;
    if (!child) {
        if (setsid() < 0) _exit(1);
        int slave = open(name, O_RDWR);
        if (slave < 0) _exit(1);
        for (int fd = 0; fd < 3; fd++) if (dup2(slave, fd) < 0) _exit(1);
        if (slave > 2) close(slave);
        close(master);
        execv(argv[1], &argv[1]);
        _exit(127);
    }
    unsigned long bytes = 0;
    char diagnostic[2048];
    size_t kept = 0;
    int status = 0, exited = 0;
    /* Drain the screen (do not let an undrained PTY block the UI), then send
     * the application's own quit key. A timeout is always failure. */
    for (int i = 0; i < 100; i++) {
        struct pollfd pollfd = { .fd = master, .events = POLLIN };
        int n = poll(&pollfd, 1, 50);
        if (n > 0 && (pollfd.revents & POLLIN)) {
            char buffer[4096];
            ssize_t count = read(master, buffer, sizeof(buffer));
            if (count > 0) {
                bytes += (unsigned long)count;
                size_t save = (size_t)count;
                if (save > sizeof(diagnostic)-kept) save = sizeof(diagnostic)-kept;
                memcpy(diagnostic+kept, buffer, save); kept += save;
            }
        }
        if (i == 20 && write(master, "q", 1) != 1) break;
        pid_t result = waitpid(child, &status, WNOHANG);
        if (result == child) { exited = 1; break; }
        if (result < 0 && errno != EINTR) break;
    }
    if (!exited) { kill(child, SIGKILL); waitpid(child, &status, 0); }
    close(master);
    if (!exited || !WIFEXITED(status) || WEXITSTATUS(status) || bytes < 128) {
        fwrite(diagnostic, 1, kept, stderr);
        fprintf(stderr, "PTY_TOOL_FAIL exited=%d status=%d bytes=%lu\n", exited, status, bytes);
        return 1;
    }
    printf("PTY_TOOL_OK rendered_bytes=%lu\n", bytes);
    return 0;
}
