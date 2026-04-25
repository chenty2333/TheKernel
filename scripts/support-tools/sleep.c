#define _GNU_SOURCE

#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int parse_seconds(const char *arg, struct timespec *req)
{
    char *end = NULL;
    unsigned long long secs = 0;

    errno = 0;
    secs = strtoull(arg, &end, 10);
    if (errno != 0 || end == arg || *end != '\0') {
        return -1;
    }
    if (secs > (unsigned long long)LONG_MAX) {
        return -1;
    }

    req->tv_sec = (time_t)secs;
    req->tv_nsec = 0;
    return 0;
}

int main(int argc, char **argv)
{
    struct timespec req = {0};
    struct timespec remaining = {0};

    if (argc != 2) {
        fprintf(stderr, "usage: %s SECONDS\n", argv[0]);
        return 2;
    }

    if (parse_seconds(argv[1], &req) != 0) {
        fprintf(stderr, "invalid sleep seconds: %s\n", argv[1]);
        return 2;
    }

    remaining = req;
    while (syscall(SYS_clock_nanosleep,
                   CLOCK_MONOTONIC,
                   0,
                   &remaining,
                   &remaining)
           == -1) {
        if (errno != EINTR) {
            perror("clock_nanosleep");
            return 1;
        }
    }

    return 0;
}
