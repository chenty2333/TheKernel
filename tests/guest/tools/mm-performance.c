#define _GNU_SOURCE

/*
 * Repository-owned, end-to-end MM evidence helper.  The latency metrics use
 * nearest-rank quantiles; the direct-I/O cases reach the ordinary short-pin
 * path without test-only controls.  They intentionally remain user-visible
 * proxies rather than claims about hardware TLB events or one internal lock.
 */

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#ifndef O_DIRECT
#define O_DIRECT 040000
#endif

#ifndef MREMAP_MAYMOVE
#define MREMAP_MAYMOVE 1
#endif

#ifndef MREMAP_FIXED
#define MREMAP_FIXED 2
#endif

enum {
    DEFAULT_ITERATIONS = 256,
    DEFAULT_LIVE_VMAS = 512,
    DEFAULT_PIN_ITERATIONS = 64,
    PIN_BUFFER_BYTES = 64 * 1024,
    PROTECT_TOUCH_PAGES = 64,
    MREMAP_SMALL_PAGES = 16,
    MREMAP_LARGE_PAGES = 32,
    MAX_ITERATIONS = 100000,
    MAX_LIVE_VMAS = 16384,
    MAX_PIN_ITERATIONS = 10000,
    MAX_PIN_WORKERS = 64,
};

struct config {
    size_t iterations;
    size_t live_vmas;
    size_t pin_iterations;
    size_t pin_workers;
};

struct metric_result {
    bool ok;
    size_t count;
    uint64_t p50_ns;
    uint64_t p99_ns;
    uint64_t p999_ns;
    bool has_throughput;
    uint64_t throughput_bytes_per_sec;
    const char *reason;
    int error_number;
};

static int monotonic_ns(uint64_t *value)
{
    struct timespec now;

    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        return -1;
    }
    *value = (uint64_t)now.tv_sec * UINT64_C(1000000000) +
             (uint64_t)now.tv_nsec;
    return 0;
}

static int compare_u64(const void *left, const void *right)
{
    const uint64_t lhs = *(const uint64_t *)left;
    const uint64_t rhs = *(const uint64_t *)right;

    return (lhs > rhs) - (lhs < rhs);
}

/* Nearest-rank quantiles, with the percentile expressed in thousandths. */
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

static struct metric_result missing_result(const char *reason,
                                           int error_number,
                                           bool has_throughput)
{
    return (struct metric_result){
        .ok = false,
        .count = 0,
        .has_throughput = has_throughput,
        .reason = reason,
        .error_number = error_number,
    };
}

static struct metric_result successful_result(uint64_t *samples, size_t count,
                                              bool has_throughput,
                                              uint64_t throughput)
{
    qsort(samples, count, sizeof(*samples), compare_u64);
    return (struct metric_result){
        .ok = true,
        .count = count,
        .p50_ns = nearest_rank(samples, count, 500),
        .p99_ns = nearest_rank(samples, count, 990),
        .p999_ns = nearest_rank(samples, count, 999),
        .has_throughput = has_throughput,
        .throughput_bytes_per_sec = throughput,
        .reason = "",
        .error_number = 0,
    };
}

static void emit_metric(const char *name, const struct metric_result *result)
{
    if (result->ok) {
        printf("MM_PERF metric=%s status=ok count=%zu p50_ns=%" PRIu64
               " p99_ns=%" PRIu64 " p999_ns=%" PRIu64,
               name, result->count, result->p50_ns, result->p99_ns,
               result->p999_ns);
        if (result->has_throughput) {
            printf(" throughput_bytes_per_sec=%" PRIu64,
                   result->throughput_bytes_per_sec);
        }
        putchar('\n');
        return;
    }

    printf("MM_PERF metric=%s status=missing count=0 p50_ns=missing"
           " p99_ns=missing p999_ns=missing",
           name);
    if (result->has_throughput) {
        printf(" throughput_bytes_per_sec=missing");
    }
    printf(" reason=%s errno=%d\n", result->reason, result->error_number);
}

static void cleanup_mappings(void **mappings, size_t count, size_t page_size)
{
    size_t index;

    for (index = 0; index < count; ++index) {
        if (mappings[index] != MAP_FAILED && mappings[index] != NULL) {
            (void)munmap(mappings[index], page_size);
        }
    }
}

static struct metric_result run_vma_scale(const struct config *config,
                                          size_t page_size)
{
    void **live = calloc(config->live_vmas, sizeof(*live));
    uint64_t *samples = calloc(config->iterations, sizeof(*samples));
    struct metric_result result;
    size_t live_count = 0;
    size_t index;

    if (live == NULL || samples == NULL) {
        result = missing_result("allocation_failed", ENOMEM, false);
        goto out;
    }
    for (index = 0; index < config->live_vmas; ++index) {
        void *mapping = mmap(NULL, page_size * 2U, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

        if (mapping == MAP_FAILED) {
            result = missing_result("vma_setup_mmap_failed", errno, false);
            goto out;
        }
        if (munmap((unsigned char *)mapping + page_size, page_size) != 0) {
            int saved_errno = errno;

            (void)munmap(mapping, page_size * 2U);
            result = missing_result("vma_setup_munmap_failed", saved_errno,
                                    false);
            goto out;
        }
        live[index] = mapping;
        live_count += 1U;
    }

    for (index = 0; index < config->iterations; ++index) {
        uint64_t before;
        uint64_t after;
        void *mapping;
        int mapping_errno = 0;

        if (monotonic_ns(&before) != 0) {
            result = missing_result("clock_failed", errno, false);
            goto out;
        }
        mapping = mmap(NULL, page_size * 2U, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (mapping == MAP_FAILED) {
            mapping_errno = errno;
        }
        if (monotonic_ns(&after) != 0) {
            int saved_errno = errno;

            if (mapping != MAP_FAILED) {
                (void)munmap(mapping, page_size * 2U);
            }
            result = missing_result("clock_failed", saved_errno, false);
            goto out;
        }
        if (mapping == MAP_FAILED) {
            result = missing_result("vma_sample_mmap_failed", mapping_errno,
                                    false);
            goto out;
        }
        samples[index] = after - before;
        if (munmap(mapping, page_size * 2U) != 0) {
            result = missing_result("vma_sample_munmap_failed", errno, false);
            goto out;
        }
    }
    result = successful_result(samples, config->iterations, false, 0);

out:
    if (live != NULL) {
        cleanup_mappings(live, live_count, page_size);
    }
    free(samples);
    free(live);
    return result;
}

static void touch_mapping(void *mapping, size_t size, size_t page_size)
{
    volatile unsigned char *bytes = mapping;
    size_t offset;

    for (offset = 0; offset < size; offset += page_size) {
        bytes[offset] ^= 1U;
    }
}

static void write_page_sentinels(void *mapping, size_t size, size_t page_size)
{
    volatile unsigned char *bytes = mapping;
    size_t page;

    for (page = 0; page < size / page_size; ++page) {
        bytes[page * page_size] = (unsigned char)(0x5aU ^ (page & 0xffU));
    }
}

static bool page_sentinels_match(const void *mapping, size_t size,
                                 size_t page_size)
{
    const volatile unsigned char *bytes = mapping;
    size_t page;

    for (page = 0; page < size / page_size; ++page) {
        const unsigned char expected =
            (unsigned char)(0x5aU ^ (page & 0xffU));

        if (bytes[page * page_size] != expected) {
            return false;
        }
    }
    return true;
}

static int verify_mremap_semantics(size_t page_size, const char **failed_test,
                                   int *failure_errno)
{
#ifdef SYS_mremap
    const size_t mapping_size = 2U * page_size;
    void *source = MAP_FAILED;
    void *destination = MAP_FAILED;
    void *shared = MAP_FAILED;
    void *alias = MAP_FAILED;
    void *fixed_target;
    void *remapped;
    int result = -1;

    source = mmap(NULL, mapping_size, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    destination = mmap(NULL, mapping_size, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (source == MAP_FAILED || destination == MAP_FAILED) {
        *failed_test = "fixed_setup";
        *failure_errno = errno;
        goto out;
    }
    write_page_sentinels(source, mapping_size, page_size);
    memset(destination, 0xa5, mapping_size);
    fixed_target = destination;
    remapped = (void *)syscall(SYS_mremap, source, mapping_size,
                               mapping_size, MREMAP_MAYMOVE | MREMAP_FIXED,
                               destination);
    if (remapped == MAP_FAILED) {
        *failed_test = "fixed_replace";
        *failure_errno = errno;
        goto out;
    }
    source = MAP_FAILED;
    destination = remapped;
    if (remapped != fixed_target ||
        !page_sentinels_match(remapped, mapping_size, page_size)) {
        *failed_test = "fixed_content";
        *failure_errno = EIO;
        goto out;
    }
    if (munmap(destination, mapping_size) != 0) {
        *failed_test = "fixed_cleanup";
        *failure_errno = errno;
        destination = MAP_FAILED;
        goto out;
    }
    destination = MAP_FAILED;

    shared = mmap(NULL, mapping_size, PROT_READ | PROT_WRITE,
                  MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        *failed_test = "alias_setup";
        *failure_errno = errno;
        goto out;
    }
    write_page_sentinels(shared, mapping_size, page_size);
    alias = (void *)syscall(SYS_mremap, shared, 0, mapping_size,
                            MREMAP_MAYMOVE);
    if (alias == MAP_FAILED) {
        *failed_test = "alias_duplicate";
        *failure_errno = errno;
        goto out;
    }
    if (!page_sentinels_match(alias, mapping_size, page_size)) {
        *failed_test = "alias_content";
        *failure_errno = EIO;
        goto out;
    }
    ((volatile unsigned char *)alias)[page_size] = 0x33U;
    if (((volatile unsigned char *)shared)[page_size] != 0x33U) {
        *failed_test = "alias_coherence";
        *failure_errno = EIO;
        goto out;
    }
    result = 0;

out:
    if (source != MAP_FAILED) {
        (void)munmap(source, mapping_size);
    }
    if (destination != MAP_FAILED) {
        (void)munmap(destination, mapping_size);
    }
    if (alias != MAP_FAILED) {
        (void)munmap(alias, mapping_size);
    }
    if (shared != MAP_FAILED) {
        (void)munmap(shared, mapping_size);
    }
    return result;
#else
    (void)page_size;
    *failed_test = "syscall_unavailable";
    *failure_errno = ENOSYS;
    return -1;
#endif
}

static struct metric_result run_mremap_latency(const struct config *config,
                                               size_t page_size)
{
#ifdef SYS_mremap
    const size_t small_size = page_size * MREMAP_SMALL_PAGES;
    const size_t large_size = page_size * MREMAP_LARGE_PAGES;
    const size_t sample_count = config->iterations * 2U;
    uint64_t *samples = calloc(sample_count, sizeof(*samples));
    void *mapping = MAP_FAILED;
    size_t current_size = small_size;
    struct metric_result result;
    size_t index;

    if (samples == NULL) {
        return missing_result("allocation_failed", ENOMEM, false);
    }
    mapping = mmap(NULL, small_size, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mapping == MAP_FAILED) {
        result = missing_result("mremap_setup_mmap_failed", errno, false);
        goto out;
    }
    write_page_sentinels(mapping, small_size, page_size);

    for (index = 0; index < sample_count; ++index) {
        const size_t target_size =
            current_size == small_size ? large_size : small_size;
        uint64_t before;
        uint64_t after;
        void *remapped;
        int remap_errno = 0;

        if (monotonic_ns(&before) != 0) {
            result = missing_result("clock_failed", errno, false);
            goto out;
        }
        remapped = (void *)syscall(SYS_mremap, mapping, current_size,
                                   target_size, MREMAP_MAYMOVE);
        if (remapped == MAP_FAILED) {
            remap_errno = errno;
        }
        if (monotonic_ns(&after) != 0) {
            int saved_errno = errno;

            if (remapped != MAP_FAILED) {
                mapping = remapped;
                current_size = target_size;
            }
            result = missing_result("clock_failed", saved_errno, false);
            goto out;
        }
        if (remapped == MAP_FAILED) {
            result = missing_result("mremap_unavailable", remap_errno, false);
            goto out;
        }
        mapping = remapped;
        current_size = target_size;
        samples[index] = after - before;
        if (!page_sentinels_match(mapping, small_size, page_size)) {
            result = missing_result("mremap_content_mismatch", EIO, false);
            goto out;
        }
        write_page_sentinels(mapping, current_size, page_size);
    }
    result = successful_result(samples, sample_count, false, 0);

out:
    if (mapping != MAP_FAILED) {
        (void)munmap(mapping, current_size);
    }
    free(samples);
    return result;
#else
    (void)config;
    (void)page_size;
    return missing_result("mremap_syscall_unavailable", ENOSYS, false);
#endif
}

static struct metric_result run_protect_touch(const struct config *config,
                                              size_t page_size)
{
    const size_t mapping_size = page_size * PROTECT_TOUCH_PAGES;
    uint64_t *samples = calloc(config->iterations, sizeof(*samples));
    void *mapping = MAP_FAILED;
    struct metric_result result;
    size_t index;

    if (samples == NULL) {
        return missing_result("allocation_failed", ENOMEM, false);
    }
    mapping = mmap(NULL, mapping_size, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mapping == MAP_FAILED) {
        result = missing_result("protect_setup_mmap_failed", errno, false);
        goto out;
    }
    touch_mapping(mapping, mapping_size, page_size);

    for (index = 0; index < config->iterations; ++index) {
        uint64_t before;
        uint64_t after;

        if (monotonic_ns(&before) != 0) {
            result = missing_result("clock_failed", errno, false);
            goto out;
        }
        if (mprotect(mapping, mapping_size, PROT_NONE) != 0) {
            result = missing_result("mprotect_none_unavailable", errno, false);
            goto out;
        }
        if (mprotect(mapping, mapping_size, PROT_READ | PROT_WRITE) != 0) {
            result = missing_result("mprotect_restore_unavailable", errno,
                                    false);
            goto out;
        }
        touch_mapping(mapping, mapping_size, page_size);
        if (monotonic_ns(&after) != 0) {
            result = missing_result("clock_failed", errno, false);
            goto out;
        }
        samples[index] = after - before;
    }
    result = successful_result(samples, config->iterations, false, 0);

out:
    if (mapping != MAP_FAILED) {
        (void)munmap(mapping, mapping_size);
    }
    free(samples);
    return result;
}

struct pin_worker {
    size_t iterations;
    size_t buffer_size;
    int fd;
    unsigned char *buffer;
    char path[128];
    uint64_t *samples;
    atomic_size_t *ready_workers;
    atomic_bool *start;
    atomic_int *failure_errno;
};

static void record_first_failure(atomic_int *failure_errno, int error_number)
{
    int expected = 0;

    if (error_number == 0) {
        error_number = EIO;
    }
    (void)atomic_compare_exchange_strong_explicit(
        failure_errno, &expected, error_number, memory_order_release,
        memory_order_relaxed);
}

static void *pin_worker_main(void *opaque)
{
    struct pin_worker *worker = opaque;
    size_t index;

    atomic_fetch_add_explicit(worker->ready_workers, 1U, memory_order_release);
    while (!atomic_load_explicit(worker->start, memory_order_acquire)) {
        (void)sched_yield();
    }

    for (index = 0; index < worker->iterations; ++index) {
        uint64_t before;
        uint64_t after;
        ssize_t written;
        int write_errno = 0;

        if (atomic_load_explicit(worker->failure_errno,
                                 memory_order_acquire) != 0) {
            break;
        }
        if (monotonic_ns(&before) != 0) {
            record_first_failure(worker->failure_errno, errno);
            break;
        }
        written = pwrite(worker->fd, worker->buffer, worker->buffer_size, 0);
        if (written < 0) {
            write_errno = errno;
        }
        if (monotonic_ns(&after) != 0) {
            record_first_failure(worker->failure_errno, errno);
            break;
        }
        if (written != (ssize_t)worker->buffer_size) {
            record_first_failure(worker->failure_errno,
                                 written < 0 ? write_errno : EIO);
            break;
        }
        worker->samples[index] = after - before;
    }
    return NULL;
}

static void cleanup_pin_workers(struct pin_worker *workers, size_t count)
{
    size_t index;

    if (workers == NULL) {
        return;
    }
    for (index = 0; index < count; ++index) {
        if (workers[index].fd >= 0) {
            (void)close(workers[index].fd);
        }
        if (workers[index].path[0] != '\0') {
            (void)unlink(workers[index].path);
        }
        free(workers[index].buffer);
    }
}

static struct metric_result run_pin_metric(size_t worker_count,
                                           size_t iterations,
                                           bool contention)
{
    const size_t sample_count = worker_count * iterations;
    pthread_t *threads = NULL;
    struct pin_worker *workers = NULL;
    uint64_t *samples = NULL;
    atomic_size_t ready_workers;
    atomic_bool start;
    atomic_int failure_errno;
    struct metric_result result;
    size_t initialized = 0;
    size_t created = 0;
    size_t index;
    uint64_t start_ns;
    uint64_t end_ns;

    if (contention && worker_count < 2U) {
        return missing_result("insufficient_online_cpus", 0, true);
    }
    threads = calloc(worker_count, sizeof(*threads));
    workers = calloc(worker_count, sizeof(*workers));
    samples = calloc(sample_count, sizeof(*samples));
    if (threads == NULL || workers == NULL || samples == NULL) {
        result = missing_result("allocation_failed", ENOMEM, true);
        goto out;
    }
    for (index = 0; index < worker_count; ++index) {
        int allocation_error;
        ssize_t warmup;

        workers[index].fd = -1;
        if (snprintf(workers[index].path, sizeof(workers[index].path),
                     "/tmp/thekernel-mm-performance-%ld-%zu", (long)getpid(),
                     index) >= (int)sizeof(workers[index].path)) {
            result = missing_result("direct_io_path_too_long", ENAMETOOLONG,
                                    true);
            goto out;
        }
        workers[index].fd =
            open(workers[index].path, O_CREAT | O_EXCL | O_RDWR | O_DIRECT,
                 S_IRUSR | S_IWUSR);
        if (workers[index].fd < 0) {
            result = missing_result("direct_io_open_failed", errno, true);
            goto out;
        }
        initialized += 1U;
        if (unlink(workers[index].path) == 0) {
            workers[index].path[0] = '\0';
        }
        allocation_error = posix_memalign((void **)&workers[index].buffer,
                                          4096U, PIN_BUFFER_BYTES);
        if (allocation_error != 0) {
            result = missing_result("aligned_allocation_failed",
                                    allocation_error, true);
            goto out;
        }
        memset(workers[index].buffer, (int)(index & 0xffU), PIN_BUFFER_BYTES);
        if (ftruncate(workers[index].fd, PIN_BUFFER_BYTES) != 0) {
            result = missing_result("direct_io_resize_failed", errno, true);
            goto out;
        }
        warmup = pwrite(workers[index].fd, workers[index].buffer,
                        PIN_BUFFER_BYTES, 0);
        if (warmup != PIN_BUFFER_BYTES) {
            result = missing_result("direct_io_unavailable",
                                    warmup < 0 ? errno : EIO, true);
            goto out;
        }
        workers[index].iterations = iterations;
        workers[index].buffer_size = PIN_BUFFER_BYTES;
        workers[index].samples = samples + index * iterations;
    }

    atomic_init(&ready_workers, 0U);
    atomic_init(&start, false);
    atomic_init(&failure_errno, 0);
    for (index = 0; index < worker_count; ++index) {
        int thread_error;

        workers[index].ready_workers = &ready_workers;
        workers[index].start = &start;
        workers[index].failure_errno = &failure_errno;
        thread_error = pthread_create(&threads[index], NULL, pin_worker_main,
                                      &workers[index]);
        if (thread_error != 0) {
            record_first_failure(&failure_errno, thread_error);
            break;
        }
        created += 1U;
    }
    while (atomic_load_explicit(&ready_workers, memory_order_acquire) < created) {
        (void)sched_yield();
    }
    if (monotonic_ns(&start_ns) != 0) {
        record_first_failure(&failure_errno, errno);
        start_ns = 0;
    }
    atomic_store_explicit(&start, true, memory_order_release);
    for (index = 0; index < created; ++index) {
        int join_error = pthread_join(threads[index], NULL);

        if (join_error != 0) {
            record_first_failure(&failure_errno, join_error);
        }
    }
    if (monotonic_ns(&end_ns) != 0) {
        record_first_failure(&failure_errno, errno);
        end_ns = start_ns;
    }
    if (created != worker_count) {
        result = missing_result("pthread_create_failed",
                                atomic_load_explicit(&failure_errno,
                                                     memory_order_acquire),
                                true);
        goto out;
    }
    if (atomic_load_explicit(&failure_errno, memory_order_acquire) != 0) {
        result = missing_result("direct_io_operation_failed",
                                atomic_load_explicit(&failure_errno,
                                                     memory_order_acquire),
                                true);
        goto out;
    }
    if (end_ns <= start_ns) {
        result = missing_result("zero_elapsed_time", 0, true);
        goto out;
    }
    {
        const uint64_t elapsed_ns = end_ns - start_ns;
        const uint64_t bytes =
            (uint64_t)sample_count * (uint64_t)PIN_BUFFER_BYTES;
        const uint64_t throughput = (uint64_t)(
            ((long double)bytes * 1000000000.0L) / (long double)elapsed_ns);

        result = successful_result(samples, sample_count, true, throughput);
    }

out:
    cleanup_pin_workers(workers, initialized);
    free(samples);
    free(workers);
    free(threads);
    return result;
}

static int parse_positive_size(const char *text, size_t maximum, size_t *value)
{
    char *end = NULL;
    unsigned long long parsed;

    errno = 0;
    parsed = strtoull(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0' || parsed == 0 ||
        parsed > maximum) {
        return -1;
    }
    *value = (size_t)parsed;
    return 0;
}

static int parse_arguments(int argc, char **argv, struct config *config)
{
    int index;

    for (index = 1; index < argc; index += 2) {
        size_t *destination;
        size_t maximum;

        if (index + 1 >= argc) {
            return -1;
        }
        if (strcmp(argv[index], "--iterations") == 0) {
            destination = &config->iterations;
            maximum = MAX_ITERATIONS;
        } else if (strcmp(argv[index], "--vmas") == 0) {
            destination = &config->live_vmas;
            maximum = MAX_LIVE_VMAS;
        } else if (strcmp(argv[index], "--pin-iterations") == 0) {
            destination = &config->pin_iterations;
            maximum = MAX_PIN_ITERATIONS;
        } else if (strcmp(argv[index], "--pin-workers") == 0) {
            destination = &config->pin_workers;
            maximum = MAX_PIN_WORKERS;
        } else {
            return -1;
        }
        if (parse_positive_size(argv[index + 1], maximum, destination) != 0) {
            return -1;
        }
    }
    return 0;
}

int main(int argc, char **argv)
{
    struct config config = {
        .iterations = DEFAULT_ITERATIONS,
        .live_vmas = DEFAULT_LIVE_VMAS,
        .pin_iterations = DEFAULT_PIN_ITERATIONS,
        .pin_workers = 0,
    };
    const long online_cpus = sysconf(_SC_NPROCESSORS_ONLN);
    const long system_page_size = sysconf(_SC_PAGESIZE);
    size_t page_size;
    struct metric_result result;
    const char *failed_semantic_test = "unknown";
    int semantic_errno = 0;

    if (parse_arguments(argc, argv, &config) != 0) {
        fprintf(stderr,
                "usage: %s [--iterations N] [--vmas N]"
                " [--pin-iterations N] [--pin-workers N]\n",
                argv[0]);
        return 2;
    }
    if (online_cpus <= 0) {
        printf("MM_PERF_TOPOLOGY status=missing online_cpus=missing"
               " reason=sysconf_failed errno=%d\n",
               errno);
    } else {
        printf("MM_PERF_TOPOLOGY status=ok online_cpus=%ld\n", online_cpus);
    }
    if (config.pin_workers == 0) {
        config.pin_workers = online_cpus > 0 ? (size_t)online_cpus : 1U;
        if (config.pin_workers > MAX_PIN_WORKERS) {
            config.pin_workers = MAX_PIN_WORKERS;
        }
    }
    if (system_page_size <= 0 || (unsigned long)system_page_size > SIZE_MAX) {
        const int saved_errno = errno;

        result = missing_result("page_size_unavailable", saved_errno, false);
        emit_metric("vma_scale", &result);
        emit_metric("mremap_latency", &result);
        emit_metric("protect_touch_latency", &result);
        result = missing_result("page_size_unavailable", saved_errno, true);
        emit_metric("pin_throughput", &result);
        emit_metric("pin_contention", &result);
        puts("MM_PERF_DONE status=ok");
        return 0;
    }
    page_size = (size_t)system_page_size;

    if (verify_mremap_semantics(page_size, &failed_semantic_test,
                                &semantic_errno) != 0) {
        printf("MM_PERF_SEMANTICS status=fail test=%s errno=%d\n",
               failed_semantic_test, semantic_errno);
        return 1;
    }
    puts("MM_PERF_SEMANTICS status=ok");

    result = run_vma_scale(&config, page_size);
    emit_metric("vma_scale", &result);
    result = run_mremap_latency(&config, page_size);
    emit_metric("mremap_latency", &result);
    if (!result.ok && result.reason != NULL &&
        strcmp(result.reason, "mremap_content_mismatch") == 0) {
        return 1;
    }
    result = run_protect_touch(&config, page_size);
    emit_metric("protect_touch_latency", &result);
    result = run_pin_metric(1U, config.pin_iterations, false);
    emit_metric("pin_throughput", &result);
    result = run_pin_metric(config.pin_workers, config.pin_iterations, true);
    emit_metric("pin_contention", &result);
    puts("MM_PERF_DONE status=ok");
    return 0;
}
