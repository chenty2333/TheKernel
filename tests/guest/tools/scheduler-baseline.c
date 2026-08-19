#define _GNU_SOURCE

/*
 * Small scheduler baseline helper used by the TheKernel and Linux KVM lanes.
 *
 * The helper intentionally uses only ordinary Linux ABI operations: pthreads,
 * private futexes, pipes, clock_gettime, and sched_setaffinity.  It therefore
 * runs unchanged in a TheKernel rootfs and in a Linux guest image.  Every
 * measured sample is emitted, so the host parser can retain raw observations
 * and recompute quantiles independently of the guest summary.
 */

#include <errno.h>
#include <inttypes.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error "scheduler baseline helper requires x86_64"
#endif

#define RUN_SCHEMA "thekernel-scheduler-baseline-run-v1"
#define SAMPLE_SCHEMA "thekernel-scheduler-baseline-sample-v1"
#define RESULT_SCHEMA "thekernel-scheduler-baseline-result-v1"
#define SAMPLE_CHECKSUM_OFFSET UINT64_C(14695981039346656037)
#define SAMPLE_CHECKSUM_PRIME UINT64_C(1099511628211)
#define SAMPLE_CHECKSUM_SELFTEST UINT64_C(5931715932612696898)

enum {
    DEFAULT_ITERATIONS = 1000,
    DEFAULT_WARMUP = 100,
    DEFAULT_CPU_WORK = 256,
    MAX_ITERATIONS = 1000000,
    MAX_WARMUP = 1000000,
    MAX_CPU_WORK = 1000000,
    MAX_WORKERS = 2,
};

enum workload_kind {
    WORKLOAD_FUTEX,
    WORKLOAD_PIPE,
    WORKLOAD_CPU,
};

enum placement_kind {
    PLACEMENT_SAME,
    PLACEMENT_CROSS,
};

struct config {
    enum workload_kind workload;
    enum placement_kind placement;
    size_t iterations;
    size_t warmup;
    size_t cpu_work;
    int expected_cpus;
};

struct shared_run {
    const struct config *config;
    pthread_barrier_t barrier;
    atomic_int failed;
    atomic_int futex_word;
    int pipe_a[2];
    int pipe_b[2];
    uint64_t *samples;
    size_t sample_count;
};

struct worker_args {
    struct shared_run *run;
    int worker;
};

static uint64_t monotonic_ns(void)
{
    struct timespec now;

    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        return 0;
    }
    return (uint64_t)now.tv_sec * UINT64_C(1000000000) +
           (uint64_t)now.tv_nsec;
}

static int compare_u64(const void *left, const void *right)
{
    const uint64_t lhs = *(const uint64_t *)left;
    const uint64_t rhs = *(const uint64_t *)right;

    return (lhs > rhs) - (lhs < rhs);
}

/* Nearest-rank quantiles; percentile is expressed in thousandths. */
static uint64_t nearest_rank(const uint64_t *samples, size_t count,
                             size_t permille)
{
    size_t rank = (count * permille + 999U) / 1000U;

    if (rank == 0) {
        rank = 1;
    }
    if (rank > count) {
        rank = count;
    }
    return samples[rank - 1U];
}

/*
 * The protocol checksum is FNV-1a over three fixed-width uint64_t fields for
 * each emitted sample: worker, sample index, and latency_ns.  Every field is
 * encoded as eight little-endian bytes, so the result is independent of the
 * guest compiler's integer representation and matches the host parser's
 * canonical implementation.
 */
static uint64_t checksum_field_le(uint64_t checksum, uint64_t field)
{
    size_t byte;

    for (byte = 0; byte < sizeof(field); ++byte) {
        checksum ^= field & UINT64_C(0xff);
        checksum *= SAMPLE_CHECKSUM_PRIME;
        field >>= 8;
    }
    return checksum;
}

static uint64_t checksum_tuple(uint64_t checksum, uint64_t worker,
                               uint64_t sample, uint64_t latency_ns)
{
    checksum = checksum_field_le(checksum, worker);
    checksum = checksum_field_le(checksum, sample);
    return checksum_field_le(checksum, latency_ns);
}

static uint64_t sample_checksum(const struct config *config,
                                const struct shared_run *run, size_t count)
{
    uint64_t checksum = SAMPLE_CHECKSUM_OFFSET;
    size_t index;

    for (index = 0; index < count; ++index) {
        uint64_t worker = config->workload == WORKLOAD_CPU
                              ? index / config->iterations
                              : 0;
        uint64_t sample = config->workload == WORKLOAD_CPU
                              ? index % config->iterations
                              : index;

        checksum = checksum_tuple(checksum, worker, sample,
                                  run->samples[index]);
    }
    return checksum;
}

struct checksum_selftest_sample {
    uint64_t worker;
    uint64_t sample;
    uint64_t latency_ns;
};

static int checksum_selftest(void)
{
    static const struct checksum_selftest_sample samples[] = {
        {0, 0, UINT64_C(1)},
        {1, 7, UINT64_C(0x0123456789abcdef)},
    };
    uint64_t checksum = SAMPLE_CHECKSUM_OFFSET;
    size_t index;

    for (index = 0; index < sizeof(samples) / sizeof(samples[0]); ++index) {
        checksum = checksum_tuple(checksum, samples[index].worker,
                                  samples[index].sample,
                                  samples[index].latency_ns);
    }
    printf("SCHED_BASELINE_CHECKSUM_SELFTEST status=%s checksum=%" PRIu64
           "\n",
           checksum == SAMPLE_CHECKSUM_SELFTEST ? "ok" : "fail", checksum);
    return checksum == SAMPLE_CHECKSUM_SELFTEST ? 0 : 1;
}

static const char *workload_name(enum workload_kind workload)
{
    switch (workload) {
    case WORKLOAD_FUTEX:
        return "futex";
    case WORKLOAD_PIPE:
        return "pipe";
    case WORKLOAD_CPU:
        return "cpu-worker";
    }
    return "unknown";
}

static const char *placement_name(enum placement_kind placement)
{
    return placement == PLACEMENT_CROSS ? "cross" : "same";
}

static int parse_positive(const char *text, size_t *value, size_t maximum,
                          const char *name)
{
    char *end = NULL;
    unsigned long long parsed;

    if (text == NULL || *text == '\0' || text[0] == '-') {
        fprintf(stderr, "scheduler-baseline: invalid %s: %s\n", name,
                text != NULL ? text : "(null)");
        return -1;
    }
    errno = 0;
    parsed = strtoull(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0' || parsed == 0 ||
        parsed > maximum) {
        fprintf(stderr, "scheduler-baseline: %s must be 1..%zu: %s\n", name,
                maximum, text);
        return -1;
    }
    *value = (size_t)parsed;
    return 0;
}

static int parse_nonnegative(const char *text, size_t *value, size_t maximum,
                             const char *name)
{
    if (text != NULL && strcmp(text, "0") == 0) {
        *value = 0;
        return 0;
    }
    return parse_positive(text, value, maximum, name);
}

static int parse_workload(const char *text, enum workload_kind *workload)
{
    if (strcmp(text, "futex") == 0) {
        *workload = WORKLOAD_FUTEX;
    } else if (strcmp(text, "pipe") == 0) {
        *workload = WORKLOAD_PIPE;
    } else if (strcmp(text, "cpu-worker") == 0) {
        *workload = WORKLOAD_CPU;
    } else {
        fprintf(stderr, "scheduler-baseline: unsupported workload: %s\n",
                text);
        return -1;
    }
    return 0;
}

static int parse_placement(const char *text, enum placement_kind *placement)
{
    if (strcmp(text, "same") == 0) {
        *placement = PLACEMENT_SAME;
    } else if (strcmp(text, "cross") == 0) {
        *placement = PLACEMENT_CROSS;
    } else {
        fprintf(stderr, "scheduler-baseline: unsupported placement: %s\n",
                text);
        return -1;
    }
    return 0;
}

static int worker_cpu(enum placement_kind placement, int worker)
{
    if (placement == PLACEMENT_SAME) {
        return 0;
    }
    return worker;
}

static int pin_worker(const struct config *config, int worker)
{
    cpu_set_t mask;
    int cpu = worker_cpu(config->placement, worker);

    CPU_ZERO(&mask);
    if (cpu < 0 || cpu >= CPU_SETSIZE) {
        return EINVAL;
    }
    CPU_SET((unsigned)cpu, &mask);
    if (sched_setaffinity(0, sizeof(mask), &mask) != 0) {
        return errno;
    }
    return 0;
}

static int futex_wait(atomic_int *word, int expected)
{
    long result = syscall(SYS_futex, (int *)word, 128 /* WAIT_PRIVATE */,
                          expected, NULL, NULL, 0);

    if (result == 0 || errno == EAGAIN || errno == EINTR) {
        return 0;
    }
    return errno;
}

static int futex_wake(atomic_int *word)
{
    long result = syscall(SYS_futex, (int *)word, 129 /* WAKE_PRIVATE */, 1,
                          NULL, NULL, 0);

    if (result >= 0) {
        return 0;
    }
    return errno;
}

static int futex_round(struct shared_run *run, int worker)
{
    atomic_int *word = &run->futex_word;
    int expected = worker == 0 ? 0 : 1;
    int mine = worker == 0 ? 1 : 0;
    int error;

    for (;;) {
        if (atomic_load_explicit(word, memory_order_acquire) == expected) {
            break;
        }
        error = futex_wait(word, !expected);
        if (error != 0) {
            return error;
        }
    }
    atomic_store_explicit(word, mine, memory_order_release);
    error = futex_wake(word);
    return error;
}

static int pipe_round(struct shared_run *run, int worker)
{
    unsigned char token = 0x5a;
    int read_fd = worker == 0 ? run->pipe_b[0] : run->pipe_a[0];
    int write_fd = worker == 0 ? run->pipe_a[1] : run->pipe_b[1];
    ssize_t count;

    if (worker == 1) {
        count = read(read_fd, &token, sizeof(token));
        if (count != (ssize_t)sizeof(token)) {
            return errno != 0 ? errno : EIO;
        }
    }
    count = write(write_fd, &token, sizeof(token));
    if (count != (ssize_t)sizeof(token)) {
        return errno != 0 ? errno : EIO;
    }
    if (worker == 0) {
        count = read(read_fd, &token, sizeof(token));
        if (count != (ssize_t)sizeof(token)) {
            return errno != 0 ? errno : EIO;
        }
    }
    return 0;
}

static uint64_t cpu_round(uint64_t state, size_t work)
{
    size_t index;

    for (index = 0; index < work; ++index) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state += UINT64_C(0x9e3779b97f4a7c15);
    }
    return state;
}

static int synchronize_workers(struct shared_run *run)
{
    int result = pthread_barrier_wait(&run->barrier);

    if (result != 0 && result != PTHREAD_BARRIER_SERIAL_THREAD) {
        atomic_store_explicit(&run->failed, EIO, memory_order_release);
        return EIO;
    }
    return 0;
}

static void *ping_worker(void *opaque)
{
    struct worker_args *args = opaque;
    struct shared_run *run = args->run;
    const struct config *config = run->config;
    int worker = args->worker;
    size_t index;
    int error;

    error = pin_worker(config, worker);
    if (error != 0) {
        atomic_store_explicit(&run->failed, error, memory_order_release);
    }
    synchronize_workers(run);
    for (index = 0; index < config->warmup; ++index) {
        if (config->workload == WORKLOAD_FUTEX) {
            error = futex_round(run, worker);
        } else {
            error = pipe_round(run, worker);
        }
        if (error != 0) {
            atomic_store_explicit(&run->failed, error, memory_order_release);
            break;
        }
    }
    synchronize_workers(run);
    if (worker == 0 && config->workload != WORKLOAD_CPU &&
        atomic_load_explicit(&run->failed, memory_order_acquire) == 0) {
        for (index = 0; index < config->iterations; ++index) {
            uint64_t before = monotonic_ns();

            if (config->workload == WORKLOAD_FUTEX) {
                error = futex_round(run, worker);
            } else {
                error = pipe_round(run, worker);
            }
            if (error != 0) {
                atomic_store_explicit(&run->failed, error, memory_order_release);
                break;
            }
            run->samples[run->sample_count++] = monotonic_ns() - before;
        }
    } else if (worker == 1 && config->workload != WORKLOAD_CPU) {
        for (index = 0; index < config->iterations; ++index) {
            if (atomic_load_explicit(&run->failed, memory_order_acquire) != 0) {
                break;
            }
            if (config->workload == WORKLOAD_FUTEX) {
                error = futex_round(run, worker);
            } else {
                error = pipe_round(run, worker);
            }
            if (error != 0) {
                atomic_store_explicit(&run->failed, error, memory_order_release);
                break;
            }
        }
    }
    return NULL;
}

static void *cpu_worker(void *opaque)
{
    struct worker_args *args = opaque;
    struct shared_run *run = args->run;
    const struct config *config = run->config;
    int worker = args->worker;
    size_t index;
    uint64_t state = UINT64_C(0x123456789abcdef0) + (uint64_t)worker;
    int error = pin_worker(config, worker);

    if (error != 0) {
        atomic_store_explicit(&run->failed, error, memory_order_release);
    }
    synchronize_workers(run);
    for (index = 0; index < config->warmup; ++index) {
        state = cpu_round(state, config->cpu_work);
    }
    synchronize_workers(run);
    for (index = 0; index < config->iterations; ++index) {
        uint64_t before = monotonic_ns();

        state = cpu_round(state, config->cpu_work);
        run->samples[(size_t)worker * config->iterations + index] =
            monotonic_ns() - before;
    }
    return NULL;
}

static void close_pipe_pair(int pair[2])
{
    if (pair[0] >= 0) {
        (void)close(pair[0]);
        pair[0] = -1;
    }
    if (pair[1] >= 0) {
        (void)close(pair[1]);
        pair[1] = -1;
    }
}

static void emit_missing(const struct config *config, const char *reason,
                         int error_number)
{
    printf("SCHED_BASELINE_RESULT schema=%s workload=%s placement=%s "
           "status=missing count=0 p50_ns=missing p99_ns=missing "
           "p999_ns=missing reason=%s errno=%d\n",
           RESULT_SCHEMA, workload_name(config->workload),
           placement_name(config->placement), reason, error_number);
}

static int emit_result(const struct config *config, struct shared_run *run,
                       size_t count)
{
    uint64_t *sorted;
    uint64_t checksum;

    if (count == 0) {
        emit_missing(config, "no_samples", EIO);
        return -1;
    }
    sorted = malloc(count * sizeof(*sorted));
    if (sorted == NULL) {
        emit_missing(config, "allocation_failed", ENOMEM);
        return -1;
    }
    memcpy(sorted, run->samples, count * sizeof(*sorted));
    qsort(sorted, count, sizeof(*sorted), compare_u64);
    checksum = sample_checksum(config, run, count);
    printf("SCHED_BASELINE_RESULT schema=%s workload=%s placement=%s "
           "status=ok count=%zu p50_ns=%" PRIu64 " p99_ns=%" PRIu64
           " p999_ns=%" PRIu64 " checksum=%" PRIu64 "\n",
           RESULT_SCHEMA, workload_name(config->workload),
           placement_name(config->placement), count,
           nearest_rank(sorted, count, 500), nearest_rank(sorted, count, 990),
           nearest_rank(sorted, count, 999), checksum);
    free(sorted);
    return 0;
}

static int run_workload(const struct config *config)
{
    struct shared_run run = {
        .config = config,
        .pipe_a = {-1, -1},
        .pipe_b = {-1, -1},
    };
    struct worker_args args[MAX_WORKERS];
    pthread_t threads[MAX_WORKERS];
    size_t sample_capacity = config->workload == WORKLOAD_CPU
                                 ? config->iterations * MAX_WORKERS
                                 : config->iterations;
    size_t started = 0;
    size_t worker_count = MAX_WORKERS;
    int error = 0;
    int online_cpus;
    int index;

    online_cpus = (int)sysconf(_SC_NPROCESSORS_ONLN);
    if (online_cpus < 1 ||
        (config->placement == PLACEMENT_CROSS && online_cpus < 2)) {
        emit_missing(config, "insufficient_online_cpus", 0);
        return 0;
    }
    if (config->expected_cpus > 0 && online_cpus < config->expected_cpus) {
        emit_missing(config, "guest_cpu_topology_below_request", 0);
        return 0;
    }
    run.samples = calloc(sample_capacity, sizeof(*run.samples));
    if (run.samples == NULL) {
        emit_missing(config, "allocation_failed", ENOMEM);
        return -1;
    }
    atomic_init(&run.failed, 0);
    atomic_init(&run.futex_word, 0);
    if (config->workload == WORKLOAD_PIPE) {
        if (pipe(run.pipe_a) != 0 || pipe(run.pipe_b) != 0) {
            emit_missing(config, "pipe_create_failed", errno);
            close_pipe_pair(run.pipe_a);
            close_pipe_pair(run.pipe_b);
            free(run.samples);
            return -1;
        }
    }
    if (pthread_barrier_init(&run.barrier, NULL, MAX_WORKERS + 1) != 0) {
        emit_missing(config, "barrier_init_failed", errno);
        close_pipe_pair(run.pipe_a);
        close_pipe_pair(run.pipe_b);
        free(run.samples);
        return -1;
    }
    for (index = 0; index < (int)worker_count; ++index) {
        args[index] = (struct worker_args){.run = &run, .worker = index};
        error = pthread_create(
            &threads[index], NULL,
            config->workload == WORKLOAD_CPU ? cpu_worker : ping_worker,
            &args[index]);
        if (error != 0) {
            atomic_store_explicit(&run.failed, error, memory_order_release);
            break;
        }
        started += 1U;
    }
    if (started != worker_count) {
        /* The two workers are deliberately fixed; no partial result is valid. */
        for (index = 0; index < (int)started; ++index) {
            (void)pthread_cancel(threads[index]);
            (void)pthread_join(threads[index], NULL);
        }
        pthread_barrier_destroy(&run.barrier);
        close_pipe_pair(run.pipe_a);
        close_pipe_pair(run.pipe_b);
        free(run.samples);
        emit_missing(config, "pthread_create_failed", error);
        return -1;
    }
    synchronize_workers(&run);
    synchronize_workers(&run);
    for (index = 0; index < (int)worker_count; ++index) {
        (void)pthread_join(threads[index], NULL);
    }
    error = atomic_load_explicit(&run.failed, memory_order_acquire);
    if (error != 0) {
        emit_missing(config, "worker_failed", error);
    } else {
        run.sample_count = config->workload == WORKLOAD_CPU
                                ? sample_capacity
                                : run.sample_count;
        for (size_t sample = 0; sample < run.sample_count; ++sample) {
            size_t worker = config->workload == WORKLOAD_CPU
                                 ? sample / config->iterations
                                 : 0;
            size_t sample_index = config->workload == WORKLOAD_CPU
                                      ? sample % config->iterations
                                      : sample;
            printf("SCHED_BASELINE_SAMPLE schema=%s workload=%s placement=%s "
                   "worker=%zu sample=%zu latency_ns=%" PRIu64 "\n",
                   SAMPLE_SCHEMA, workload_name(config->workload),
                   placement_name(config->placement), worker, sample_index,
                   run.samples[sample]);
        }
        (void)emit_result(config, &run, run.sample_count);
    }
    pthread_barrier_destroy(&run.barrier);
    close_pipe_pair(run.pipe_a);
    close_pipe_pair(run.pipe_b);
    free(run.samples);
    return error == 0 ? 0 : -1;
}

static void usage(const char *program)
{
    fprintf(stderr,
            "Usage: %s --workload {futex|pipe|cpu-worker} "
            "--placement {same|cross} [OPTIONS]\n"
            "  --iterations N  measured rounds (default %d)\n"
            "  --warmup N      discarded rounds (default %d)\n"
            "  --cpu-work N    xorshift steps for cpu-worker (default %d)\n"
            "  --cpus N        expected online guest CPUs (optional)\n"
            "  --selftest-checksum  verify canonical sample checksum\n",
            program, DEFAULT_ITERATIONS, DEFAULT_WARMUP, DEFAULT_CPU_WORK);
}

int main(int argc, char **argv)
{
    struct config config = {
        .workload = WORKLOAD_FUTEX,
        .placement = PLACEMENT_SAME,
        .iterations = DEFAULT_ITERATIONS,
        .warmup = DEFAULT_WARMUP,
        .cpu_work = DEFAULT_CPU_WORK,
        .expected_cpus = 0,
    };
    int index;

    if (argc == 2 && strcmp(argv[1], "--selftest-checksum") == 0) {
        return checksum_selftest();
    }

    for (index = 1; index < argc; ++index) {
        if (strcmp(argv[index], "--workload") == 0 && index + 1 < argc) {
            if (parse_workload(argv[++index], &config.workload) != 0) {
                return 2;
            }
        } else if (strcmp(argv[index], "--placement") == 0 &&
                   index + 1 < argc) {
            if (parse_placement(argv[++index], &config.placement) != 0) {
                return 2;
            }
        } else if (strcmp(argv[index], "--iterations") == 0 &&
                   index + 1 < argc) {
            if (parse_positive(argv[++index], &config.iterations,
                               MAX_ITERATIONS, "iterations") != 0) {
                return 2;
            }
        } else if (strcmp(argv[index], "--warmup") == 0 && index + 1 < argc) {
            if (parse_nonnegative(argv[++index], &config.warmup, MAX_WARMUP,
                                  "warmup") != 0) {
                return 2;
            }
        } else if (strcmp(argv[index], "--cpu-work") == 0 &&
                   index + 1 < argc) {
            if (parse_positive(argv[++index], &config.cpu_work, MAX_CPU_WORK,
                               "cpu-work") != 0) {
                return 2;
            }
        } else if (strcmp(argv[index], "--cpus") == 0 && index + 1 < argc) {
            size_t requested;

            if (parse_positive(argv[++index], &requested, CPU_SETSIZE,
                               "cpus") != 0) {
                return 2;
            }
            config.expected_cpus = (int)requested;
        } else if (strcmp(argv[index], "--help") == 0 ||
                   strcmp(argv[index], "-h") == 0) {
            usage(argv[0]);
            return 0;
        } else {
            usage(argv[0]);
            return 2;
        }
    }
    printf("SCHED_BASELINE_RUN schema=%s arch=x86_64 workload=%s "
           "placement=%s iterations=%zu warmup=%zu cpus=%d cpu_work=%zu\n",
           RUN_SCHEMA, workload_name(config.workload),
           placement_name(config.placement), config.iterations, config.warmup,
           config.expected_cpus, config.cpu_work);
    if (run_workload(&config) != 0) {
        return 1;
    }
    printf("SCHED_BASELINE_DONE schema=%s workload=%s placement=%s\n",
           RUN_SCHEMA, workload_name(config.workload),
           placement_name(config.placement));
    return 0;
}
