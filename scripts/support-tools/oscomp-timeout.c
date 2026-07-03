#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static long parse_secs(const char *text) {
    char *end = NULL;
    errno = 0;
    long value = strtol(text, &end, 10);
    if (errno || end == text || *end != '\0' || value <= 0) {
        fprintf(stderr, "oscomp-timeout: invalid seconds: %s\n", text);
        exit(2);
    }
    return value;
}

static long monotonic_secs(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return time(NULL);
    }
    return ts.tv_sec;
}

static const char *debug_target(int argc, char **argv) {
    for (int i = 2; i < argc; i++) {
        if (strstr(argv[i], "madvise10") ||
            strstr(argv[i], "membarrier01") ||
            strstr(argv[i], "tar01") ||
            strstr(argv[i], "tar_tests.sh") ||
            strstr(argv[i], "waitpid01") ||
            strstr(argv[i], "access02") ||
            strstr(argv[i], "stat04") ||
            strstr(argv[i], "lstat01") ||
            strstr(argv[i], "chmod01")) {
            return argv[i];
        }
    }
    return NULL;
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: oscomp-timeout SECS command [args...]\n");
        return 2;
    }

    long timeout_secs = parse_secs(argv[1]);
    const char *debug = debug_target(argc, argv);
    if (debug) {
        fprintf(stderr, "oscomp-timeout-trace target=%s event=start timeout=%ld\n",
                debug, timeout_secs);
    }
    pid_t child = fork();
    if (child < 0) {
        perror("fork");
        return 125;
    }
    if (child == 0) {
        setpgid(0, 0);
        execvp(argv[2], &argv[2]);
        perror(argv[2]);
        _exit(errno == ENOENT ? 127 : 126);
    }

    setpgid(child, child);
    long deadline = monotonic_secs() + timeout_secs;
    long last_report = 0;
    if (debug) {
        fprintf(stderr,
                "oscomp-timeout-trace target=%s event=forked child=%ld deadline=%ld\n",
                debug, (long)child, deadline);
    }
    int status = 0;
    for (;;) {
        pid_t waited = waitpid(child, &status, WNOHANG);
        if (waited == child) {
            if (debug) {
                fprintf(stderr,
                        "oscomp-timeout-trace target=%s event=waited status=%d\n",
                        debug, status);
            }
            if (WIFEXITED(status)) {
                return WEXITSTATUS(status);
            }
            if (WIFSIGNALED(status)) {
                return 128 + WTERMSIG(status);
            }
            return 125;
        }
        if (waited < 0 && errno != EINTR) {
            perror("waitpid");
            return 125;
        }
        long now = monotonic_secs();
        if (debug && now - last_report >= 5) {
            fprintf(stderr,
                    "oscomp-timeout-trace target=%s event=poll now=%ld deadline=%ld child=%ld\n",
                    debug, now, deadline, (long)child);
            last_report = now;
        }
        if (now >= deadline) {
            break;
        }
        struct timespec nap = {.tv_sec = 0, .tv_nsec = 100000000L};
        nanosleep(&nap, NULL);
    }

    if (debug) {
        fprintf(stderr,
                "oscomp-timeout-trace target=%s event=sigterm child=%ld\n",
                debug, (long)child);
    }
    kill(-child, SIGTERM);
    long grace_deadline = monotonic_secs() + 2;
    while (monotonic_secs() < grace_deadline) {
        pid_t waited = waitpid(child, &status, WNOHANG);
        if (waited == child) {
            if (debug) {
                fprintf(stderr,
                        "oscomp-timeout-trace target=%s event=waited-after-term status=%d\n",
                        debug, status);
            }
            return 124;
        }
        if (waited < 0 && errno != EINTR) {
            return 124;
        }
        struct timespec nap = {.tv_sec = 0, .tv_nsec = 100000000L};
        nanosleep(&nap, NULL);
    }

    if (debug) {
        fprintf(stderr,
                "oscomp-timeout-trace target=%s event=sigkill child=%ld\n",
                debug, (long)child);
    }
    kill(-child, SIGKILL);
    long kill_deadline = monotonic_secs() + 2;
    while (monotonic_secs() < kill_deadline) {
        pid_t waited = waitpid(child, &status, WNOHANG);
        if (waited == child) {
            if (debug) {
                fprintf(stderr,
                        "oscomp-timeout-trace target=%s event=waited-after-kill status=%d\n",
                        debug, status);
            }
            return 124;
        }
        if (waited < 0 && errno != EINTR) {
            return 124;
        }
        struct timespec nap = {.tv_sec = 0, .tv_nsec = 100000000L};
        nanosleep(&nap, NULL);
    }
    return 124;
}
