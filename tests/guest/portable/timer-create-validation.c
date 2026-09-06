#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int reject(int clock, const void *event, int *id, int expected) {
    errno = 0;
    long result = syscall(SYS_timer_create, clock, event, id);
    if (result != -1 || errno != expected) {
        fprintf(stderr, "timer_create clock=%d result=%ld errno=%d expected=%d\n",
                clock, result, errno, expected);
        return 1;
    }
    return 0;
}

int main(void) {
    struct sigevent event = {.sigev_notify = SIGEV_NONE};
    int id = -1;
    int clocks[] = {CLOCK_MONOTONIC_RAW, CLOCK_REALTIME_COARSE,
                    CLOCK_MONOTONIC_COARSE};
    for (unsigned i = 0; i < sizeof(clocks) / sizeof(clocks[0]); ++i) {
        if (reject(clocks[i], &event, &id, EOPNOTSUPP) ||
            reject(clocks[i], NULL, NULL, EOPNOTSUPP) ||
            reject(clocks[i], (void *)1, &id, EFAULT)) return 1;
    }
    if (reject(123456, NULL, NULL, EINVAL) ||
        reject(123456, (void *)1, &id, EFAULT) ||
        reject(CLOCK_MONOTONIC, &event, NULL, EFAULT)) return 1;
    if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &id) ||
        syscall(SYS_timer_delete, id)) return 1;
    puts("THEKERNEL_TIMER_CREATE_VALIDATION_OK");
    return 0;
}
