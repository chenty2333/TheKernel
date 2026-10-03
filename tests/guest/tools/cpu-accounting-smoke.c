#define _GNU_SOURCE

#include <errno.h>
#include <inttypes.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

/* Two syscall-heavy threads share one CPU. A reader on another available
 * CPU can observe publication during timer IRQs and scheduler switches,
 * rather than always forcing its own fresh accounting poll before reading.
 * This still checks monotonicity on a one-CPU guest, without claiming the
 * same coverage of concurrent observation. */
static atomic_int target_tid, stop, error;
static int worker_cpu;

static int pin(int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    return pthread_setaffinity_np(pthread_self(), sizeof(set), &set);
}

static uint64_t ns(struct timespec t) {
    return (uint64_t)t.tv_sec * 1000000000 + (uint64_t)t.tv_nsec;
}

static uint64_t us(struct timeval t) {
    return (uint64_t)t.tv_sec * 1000000 + (uint64_t)t.tv_usec;
}

static void *worker(void *arg) {
    if (pin(worker_cpu)) {
        atomic_store(&error, 1);
        return NULL;
    }
    if (arg) atomic_store(&target_tid, (int)syscall(SYS_gettid));
    uint64_t user = 0, system = 0;
    for (unsigned i = 0; !atomic_load(&stop); ++i) {
        struct rusage usage;
        struct timespec now;
        if (getrusage(RUSAGE_THREAD, &usage) ||
            clock_gettime(CLOCK_THREAD_CPUTIME_ID, &now) ||
            us(usage.ru_utime) < user || us(usage.ru_stime) < system) {
            atomic_store(&error, 1);
            break;
        }
        user = us(usage.ru_utime);
        system = us(usage.ru_stime);
        (void)getppid();
        if (!(i & 15)) sched_yield();
    }
    return NULL;
}

static int proc_ticks(int tid, uint64_t *user, uint64_t *system) {
    char path[80], line[2048];
    snprintf(path, sizeof(path), "/proc/self/task/%d/stat", tid);
    FILE *file = fopen(path, "r");
    if (!file) return 1;
    int failed = !fgets(line, sizeof(line), file);
    if (fclose(file)) failed = 1;
    if (failed) return 1;
    /* comm may contain spaces or parentheses. Fields start after its final
     * closing parenthesis; Linux utime/stime are fields 14/15. */
    char *p = strrchr(line, ')');
    if (!p) return 1;
    ++p;
    for (int field = 3; field <= 15; ++field) {
        while (*p == ' ') ++p;
        if (!*p || *p == '\n') return 1;
        char *end;
        if (field >= 14) {
            errno = 0;
            unsigned long long ticks = strtoull(p, &end, 10);
            if (errno || end == p || (*end != ' ' && *end != '\n')) return 1;
            if (field == 14) *user = ticks;
            else *system = ticks;
        } else {
            end = p;
            while (*end && *end != ' ' && *end != '\n') ++end;
        }
        p = end;
    }
    return 0;
}

int main(void) {
    cpu_set_t allowed;
    if (sched_getaffinity(0, sizeof(allowed), &allowed)) return 1;
    int observer_cpu = -1;
    worker_cpu = -1;
    for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
        if (!CPU_ISSET(cpu, &allowed)) continue;
        if (worker_cpu < 0) worker_cpu = cpu;
        else { observer_cpu = cpu; break; }
    }
    if (worker_cpu < 0) return 1;
    if (observer_cpu < 0) observer_cpu = worker_cpu;
    if (pin(observer_cpu)) return 1;
    alarm(30);
    pthread_t target, peer;
    if (pthread_create(&target, NULL, worker, (void *)1)) return 1;
    if (pthread_create(&peer, NULL, worker, NULL)) {
        atomic_store(&stop, 1);
        pthread_join(target, NULL);
        return 1;
    }
    while (!atomic_load(&target_tid) && !atomic_load(&error)) sched_yield();
    int tid = atomic_load(&target_tid);
    /* Linux's negative per-thread CPU clock encoding: SCHED=2, VIRT=1. */
    clockid_t total_clock = (clockid_t)((~(unsigned)tid << 3) | 6);
    clockid_t user_clock = (clockid_t)((~(unsigned)tid << 3) | 5);
    uint64_t total = 0, user = 0, samples = 0;
    uint64_t proc_user = 0, proc_system = 0, ru_user = 0, ru_system = 0;
    struct timespec begin, wall;
    int failed = atomic_load(&error) || clock_gettime(CLOCK_MONOTONIC, &begin);
    if (!failed) do {
        struct timespec u, t;
        if (clock_gettime(user_clock, &u) || clock_gettime(total_clock, &t)) {
            failed = 1;
            break;
        }
        if (ns(u) < user || ns(t) < total || ns(t) < ns(u)) {
            fprintf(stderr, "THEKERNEL_CPU_ACCOUNTING_FAIL clock-regression"
                    " samples=%" PRIu64 " previous=%" PRIu64 " current=%" PRIu64
                    " previous_user=%" PRIu64 " current_user=%" PRIu64 "\n",
                    samples, total, ns(t), user, ns(u));
            failed = 1;
            break;
        }
        total = ns(t);
        user = ns(u);
        if (!(samples & 255)) {
            uint64_t pu, ps;
            struct rusage usage;
            if (proc_ticks(tid, &pu, &ps) || getrusage(RUSAGE_SELF, &usage) ||
                pu < proc_user || ps < proc_system ||
                us(usage.ru_utime) < ru_user || us(usage.ru_stime) < ru_system ||
                clock_gettime(CLOCK_MONOTONIC, &wall)) {
                failed = 1;
                break;
            }
            proc_user = pu;
            proc_system = ps;
            ru_user = us(usage.ru_utime);
            ru_system = us(usage.ru_stime);
            if (ns(wall) - ns(begin) >= 10000000000ULL) break;
        }
        ++samples;
    } while (!atomic_load(&error));
    atomic_store(&stop, 1);
    int target_join = pthread_join(target, NULL);
    int peer_join = pthread_join(peer, NULL);
    if (target_join || peer_join) failed = 1;
    failed |= atomic_load(&error) || samples < 256 || !total;
    printf("THEKERNEL_CPU_ACCOUNTING_%s samples=%" PRIu64 " cpu_ns=%" PRIu64
           " worker_cpu=%d observer_cpu=%d\n", failed ? "FAIL" : "OK",
           samples, total, worker_cpu, observer_cpu);
    return failed;
}
