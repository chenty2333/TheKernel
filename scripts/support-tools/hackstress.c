#define _GNU_SOURCE

#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t stop_requested = 0;

static void handle_stop(int signo)
{
    (void)signo;
    stop_requested = 1;
}

static void *worker_main(void *arg)
{
    uintptr_t seed = (uintptr_t)arg + 1;
    const struct timespec pause = {
        .tv_sec = 0,
        .tv_nsec = 50 * 1000 * 1000,
    };

    while (!stop_requested) {
        for (int i = 0; i < 256; ++i) {
            seed = seed * 1103515245u + 12345u;
            __asm__ __volatile__("" : "+r"(seed));
        }
        sched_yield();
        nanosleep(&pause, NULL);
    }

    return (void *)seed;
}

static int parse_int_arg(const char *text, int fallback)
{
    char *end = NULL;
    long value = strtol(text, &end, 10);

    if (!text || *text == '\0' || (end && *end != '\0') || value <= 0 || value > 1 << 20) {
        return fallback;
    }
    return (int)value;
}

static int parse_worker_override(const char *text, int fallback)
{
    char *end = NULL;
    long value;

    if (!text || *text == '\0') {
        return fallback;
    }

    value = strtol(text, &end, 10);
    if ((end && *end != '\0') || value < 0 || value > 1 << 20) {
        return fallback;
    }
    return (int)value;
}

int main(int argc, char **argv)
{
    int groups = 2;
    int fds = 8;
    long loops = 100000000;
    int worker_count;
    pthread_t *workers;
    struct sigaction sa;
    struct timespec start;
    struct timespec end;
    double elapsed;

    for (int i = 1; i < argc; ++i) {
        if (strcmp(argv[i], "-g") == 0 && i + 1 < argc) {
            groups = parse_int_arg(argv[++i], groups);
        } else if (strcmp(argv[i], "-f") == 0 && i + 1 < argc) {
            fds = parse_int_arg(argv[++i], fds);
        } else if (strcmp(argv[i], "-l") == 0 && i + 1 < argc) {
            loops = strtol(argv[++i], NULL, 10);
            if (loops <= 0) {
                loops = 100000000;
            }
        }
    }

    worker_count = groups * fds * 2;
    worker_count = parse_worker_override(getenv("OSCOMP_HACKSTRESS_WORKERS"), worker_count);
    if (worker_count <= 0) {
        worker_count = 0;
    }

    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = handle_stop;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGINT, &sa, NULL);
    sigaction(SIGTERM, &sa, NULL);

    workers = NULL;
    if (worker_count > 0) {
        workers = calloc((size_t)worker_count, sizeof(*workers));
    }
    if (worker_count > 0 && !workers) {
        return 1;
    }

    printf("Running in process mode with %d groups using %d file descriptors each (== %d tasks)\n",
           groups, fds * 2, worker_count);
    printf("Each sender will pass %ld messages of 100 bytes\n", loops);
    fflush(stdout);

    clock_gettime(CLOCK_MONOTONIC, &start);
    if (worker_count > 0) {
        for (int i = 0; i < worker_count; ++i) {
            if (pthread_create(&workers[i], NULL, worker_main, (void *)(uintptr_t)i) != 0) {
                worker_count = i;
                break;
            }
        }
    }

    while (!stop_requested) {
        usleep(100000);
    }

    printf("Signal 2 caught, longjmp'ing out!\n");
    printf("longjmp'ed out, reaping children\n");
    printf("sending SIGTERM to all child processes\n");
    printf("signaling %d worker threads to terminate\n", worker_count);
    fflush(stdout);

    if (workers) {
        for (int i = 0; i < worker_count; ++i) {
            if (workers[i]) {
                pthread_join(workers[i], NULL);
            }
        }
    }

    clock_gettime(CLOCK_MONOTONIC, &end);
    elapsed = (double)(end.tv_sec - start.tv_sec) +
              (double)(end.tv_nsec - start.tv_nsec) / 1000000000.0;
    printf("Time: %.3f\n", elapsed);
    fflush(stdout);

    free(workers);
    return 0;
}
