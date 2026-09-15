#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static int require_zero(const char *operation, long result) {
    if (result != 0) {
        fprintf(stderr, "%s: result=%ld errno=%d (%m)\n", operation, result, errno);
        return 1;
    }
    return 0;
}

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

static void on_alarm(int signo) { (void)signo; }

static int cpu_absolute_sleep_interrupt(void) {
    struct sigaction action = {.sa_handler = on_alarm};
    sigemptyset(&action.sa_mask);
    if (require_zero("sigaction(SIGALRM)", sigaction(SIGALRM, &action, NULL))) return 1;
    struct timespec deadline, remain = {.tv_sec = 123, .tv_nsec = 456};
    if (require_zero("clock_gettime(PROCESS_CPUTIME)", clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &deadline))) return 1;
    deadline.tv_sec += 60;
    struct itimerval alarm_timer = {.it_value.tv_usec = 20000};
    if (require_zero("setitimer(ITIMER_REAL)", setitimer(ITIMER_REAL, &alarm_timer, NULL))) return 1;
    errno = 0;
    long result = syscall(SYS_clock_nanosleep, CLOCK_PROCESS_CPUTIME_ID,
            TIMER_ABSTIME, &deadline, &remain);
    if (result != -1 || errno != EINTR || remain.tv_sec != 123 || remain.tv_nsec != 456) {
        fprintf(stderr, "absolute cpu sleep result=%ld errno=%d rem=%ld/%ld\n",
                result, errno, remain.tv_sec, remain.tv_nsec);
        return 1;
    }
    return 0;
}

int main(void) {
    const struct timespec zero = {0};
    if (require_zero("clock_nanosleep(TAI, relative)", syscall(SYS_clock_nanosleep, CLOCK_TAI, 0, &zero, NULL)) ||
        require_zero("clock_nanosleep(TAI, absolute)", syscall(SYS_clock_nanosleep, CLOCK_TAI, TIMER_ABSTIME, &zero, NULL))) return 1;
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
    if (require_zero("timer_create(SIGEV_NONE)", syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &id)) ||
        require_zero("timer_delete(SIGEV_NONE)", syscall(SYS_timer_delete, id))) return 1;
    event.sigev_notify = SIGEV_THREAD_ID;
    event.sigev_signo = SIGALRM;
    event._sigev_un._tid = 0x7fffffff;
    if (reject(CLOCK_MONOTONIC, &event, &id, EINVAL)) return 1;
    event._sigev_un._tid = (int)syscall(SYS_gettid);
    if (require_zero("timer_create(SIGEV_THREAD_ID)", syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &id)) ||
        require_zero("timer_delete(SIGEV_THREAD_ID)", syscall(SYS_timer_delete, id)) || cpu_absolute_sleep_interrupt()) return 1;
    puts("THEKERNEL_TIMER_CREATE_VALIDATION_OK");
    return 0;
}
