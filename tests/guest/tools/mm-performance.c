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
    PIN_WARMUP_ITERATIONS = 64,
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
    int cpu;
    int fd;
    unsigned char *buffer;
    char path[128];
    uint64_t *samples;
    size_t completed;
    int start_cpu;
    int end_cpu;
    uint64_t completion_ns;
    struct pin_gate *gate;
    atomic_int *failure_errno;
};

struct pin_gate {
    pthread_mutex_t mutex;
    pthread_cond_t condition;
    atomic_size_t ready_workers;
    bool start;
    bool abort;
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
    cpu_set_t affinity;
    int affinity_error;
    bool abort;
    size_t index;

    CPU_ZERO(&affinity);
    CPU_SET(worker->cpu, &affinity);
    affinity_error =
        pthread_setaffinity_np(pthread_self(), sizeof(affinity), &affinity);
    if (affinity_error != 0) {
        record_first_failure(worker->failure_errno, affinity_error);
    } else if (sched_getcpu() != worker->cpu) {
        record_first_failure(worker->failure_errno, EXDEV);
    }

    for (index = 0;
         index < PIN_WARMUP_ITERATIONS &&
         atomic_load_explicit(worker->failure_errno, memory_order_acquire) == 0;
         ++index) {
        ssize_t written =
            pwrite(worker->fd, worker->buffer, worker->buffer_size, 0);

        if (written != (ssize_t)worker->buffer_size) {
            record_first_failure(worker->failure_errno,
                                 written < 0 ? errno : EIO);
        }
    }

    atomic_fetch_add_explicit(&worker->gate->ready_workers, 1U,
                              memory_order_release);
    affinity_error = pthread_mutex_lock(&worker->gate->mutex);
    if (affinity_error != 0) {
        record_first_failure(worker->failure_errno, affinity_error);
        return NULL;
    }
    while (!worker->gate->start && !worker->gate->abort) {
        affinity_error =
            pthread_cond_wait(&worker->gate->condition, &worker->gate->mutex);
        if (affinity_error != 0) {
            record_first_failure(worker->failure_errno, affinity_error);
            worker->gate->abort = true;
            (void)pthread_cond_broadcast(&worker->gate->condition);
            break;
        }
    }
    abort = worker->gate->abort;
    affinity_error = pthread_mutex_unlock(&worker->gate->mutex);
    if (affinity_error != 0) {
        record_first_failure(worker->failure_errno, affinity_error);
        return NULL;
    }
    if (abort ||
        atomic_load_explicit(worker->failure_errno, memory_order_acquire) != 0) {
        return NULL;
    }

    worker->start_cpu = sched_getcpu();
    if (worker->start_cpu != worker->cpu) {
        record_first_failure(worker->failure_errno, EXDEV);
        return NULL;
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
        worker->completed = index + 1U;
        worker->completion_ns = after;
    }
    worker->end_cpu = sched_getcpu();
    if (worker->end_cpu != worker->cpu) {
        record_first_failure(worker->failure_errno, EXDEV);
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
                                           bool contention,
                                           const int *worker_cpus,
                                           size_t live_vmas,
                                           size_t page_size)
{
    const size_t sample_count = worker_count * iterations;
    const char *mode = contention ? "contention" : "single";
    pthread_t *threads = NULL;
    struct pin_worker *workers = NULL;
    uint64_t *samples = NULL;
    void **vma_fixture = NULL;
    struct pin_gate gate;
    bool gate_mutex_initialized = false;
    bool gate_condition_initialized = false;
    atomic_int failure_errno;
    size_t live_count = 0;
    struct metric_result result;
    size_t initialized = 0;
    size_t created = 0;
    size_t index;
    uint64_t start_ns = 0;
    uint64_t end_ns;

    if (contention && worker_count < 2U) {
        return missing_result("insufficient_online_cpus", 0, true);
    }
    threads = calloc(worker_count, sizeof(*threads));
    workers = calloc(worker_count, sizeof(*workers));
    samples = calloc(sample_count, sizeof(*samples));
    vma_fixture = calloc(live_vmas, sizeof(*vma_fixture));
    if (threads == NULL || workers == NULL || samples == NULL ||
        vma_fixture == NULL) {
        result = missing_result("allocation_failed", ENOMEM, true);
        goto out;
    }

    for (index = 0; index < live_vmas; ++index) {
        void *mapping = mmap(NULL, page_size * 2U, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

        if (mapping == MAP_FAILED) {
            result = missing_result("pin_vma_fixture_mmap_failed", errno, true);
            goto out;
        }
        if (munmap((unsigned char *)mapping + page_size, page_size) != 0) {
            int saved_errno = errno;

            (void)munmap(mapping, page_size * 2U);
            result = missing_result("pin_vma_fixture_munmap_failed",
                                    saved_errno, true);
            goto out;
        }
        vma_fixture[index] = mapping;
        live_count += 1U;
    }

    for (index = 0; index < worker_count; ++index) {
        int allocation_error;

        workers[index].fd = -1;
        workers[index].cpu = worker_cpus[index];
        workers[index].start_cpu = -1;
        workers[index].end_cpu = -1;
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
        workers[index].iterations = iterations;
        workers[index].buffer_size = PIN_BUFFER_BYTES;
        workers[index].samples = samples + index * iterations;
    }

    memset(&gate, 0, sizeof(gate));
    atomic_init(&gate.ready_workers, 0U);
    {
        int gate_error = pthread_mutex_init(&gate.mutex, NULL);

        if (gate_error != 0) {
            result = missing_result("pthread_mutex_init_failed", gate_error,
                                    true);
            goto out;
        }
        gate_mutex_initialized = true;
        gate_error = pthread_cond_init(&gate.condition, NULL);
        if (gate_error != 0) {
            result = missing_result("pthread_cond_init_failed", gate_error,
                                    true);
            goto out;
        }
        gate_condition_initialized = true;
    }
    atomic_init(&failure_errno, 0);
    for (index = 0; index < worker_count; ++index) {
        int thread_error;

        workers[index].gate = &gate;
        workers[index].failure_errno = &failure_errno;
        thread_error = pthread_create(&threads[index], NULL, pin_worker_main,
                                      &workers[index]);
        if (thread_error != 0) {
            record_first_failure(&failure_errno, thread_error);
            break;
        }
        created += 1U;
    }
    {
        int gate_error;

        while (atomic_load_explicit(&gate.ready_workers,
                                    memory_order_acquire) < created) {
            (void)sched_yield();
        }
        gate_error = pthread_mutex_lock(&gate.mutex);
        if (gate_error != 0) {
            record_first_failure(&failure_errno, gate_error);
        } else {
            if (created != worker_count) {
                gate.abort = true;
            }
            if (atomic_load_explicit(&failure_errno, memory_order_acquire) != 0) {
                gate.abort = true;
            }
            if (!gate.abort) {
                if (monotonic_ns(&start_ns) != 0) {
                    record_first_failure(&failure_errno, errno);
                    gate.abort = true;
                    start_ns = 0;
                } else {
                    gate.start = true;
                }
            }
            (void)pthread_cond_broadcast(&gate.condition);
            gate_error = pthread_mutex_unlock(&gate.mutex);
            if (gate_error != 0) {
                record_first_failure(&failure_errno, gate_error);
            }
        }
    }
    for (index = 0; index < created; ++index) {
        int join_error = pthread_join(threads[index], NULL);

        if (join_error != 0) {
            record_first_failure(&failure_errno, join_error);
        }
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
    end_ns = 0;
    for (index = 0; index < worker_count; ++index) {
        size_t sample_index;
        size_t over_10_ms = 0;
        size_t over_50_ms = 0;
        uint64_t worker_p99;

        if (workers[index].completed != iterations ||
            workers[index].start_cpu != workers[index].cpu ||
            workers[index].end_cpu != workers[index].cpu) {
            result = missing_result("pin_worker_placement_failed", EXDEV, true);
            goto out;
        }
        if (workers[index].completion_ns > end_ns) {
            end_ns = workers[index].completion_ns;
        }
        for (sample_index = 0; sample_index < iterations; ++sample_index) {
            const uint64_t sample = workers[index].samples[sample_index];

            over_10_ms += sample > UINT64_C(10000000);
            over_50_ms += sample > UINT64_C(50000000);
        }
        qsort(workers[index].samples, iterations,
              sizeof(*workers[index].samples), compare_u64);
        worker_p99 = nearest_rank(workers[index].samples, iterations, 990);
        printf("MM_PERF_PIN_WORKER mode=%s status=ok worker=%zu cpu=%d"
               " completed=%zu p99_ns=%" PRIu64
               " over_10ms=%zu over_50ms=%zu\n",
               mode, index, workers[index].cpu, workers[index].completed,
               worker_p99, over_10_ms, over_50_ms);
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
    if (vma_fixture != NULL) {
        cleanup_mappings(vma_fixture, live_count, page_size);
    }
    if (gate_condition_initialized) {
        (void)pthread_cond_destroy(&gate.condition);
    }
    if (gate_mutex_initialized) {
        (void)pthread_mutex_destroy(&gate.mutex);
    }
    cleanup_pin_workers(workers, initialized);
    free(vma_fixture);
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

static int raw_affinity_snapshot(size_t *returned_bytes,
                                 size_t *allowed_cpus,
                                 int *cpu_ids,
                                 size_t cpu_id_capacity,
                                 int *failure_errno)
{
#ifdef SYS_sched_getaffinity
    unsigned char mask[sizeof(cpu_set_t)];
    long result;
    size_t count = 0;
    size_t index;

    memset(mask, 0, sizeof(mask));
    result = syscall(SYS_sched_getaffinity, 0, sizeof(mask), mask);
    if (result <= 0 || (unsigned long)result > sizeof(mask) ||
        (size_t)result % sizeof(unsigned long) != 0) {
        *failure_errno = result < 0 ? errno : EPROTO;
        return -1;
    }
    for (index = 0; index < (size_t)result; ++index) {
        unsigned char byte = mask[index];
        size_t bit;

        for (bit = 0; bit < 8U; ++bit) {
            if ((byte & (1U << bit)) == 0) {
                continue;
            }
            if (count < cpu_id_capacity) {
                cpu_ids[count] = (int)(index * 8U + bit);
            }
            count += 1U;
        }
    }
    if (count == 0) {
        *failure_errno = EIO;
        return -1;
    }
    *returned_bytes = (size_t)result;
    *allowed_cpus = count;
    return 0;
#else
    (void)returned_bytes;
    (void)allowed_cpus;
    (void)cpu_ids;
    (void)cpu_id_capacity;
    *failure_errno = ENOSYS;
    return -1;
#endif
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
    size_t affinity_bytes = 0;
    size_t affinity_cpus = 0;
    int affinity_cpu_ids[MAX_PIN_WORKERS];
    int affinity_errno = 0;
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
    } else if (raw_affinity_snapshot(&affinity_bytes, &affinity_cpus,
                                     affinity_cpu_ids, MAX_PIN_WORKERS,
                                     &affinity_errno) != 0) {
        printf("MM_PERF_TOPOLOGY status=missing online_cpus=missing"
               " reason=affinity_abi_invalid errno=%d\n",
               affinity_errno);
    } else if ((size_t)online_cpus != affinity_cpus) {
        printf("MM_PERF_TOPOLOGY status=missing online_cpus=missing"
               " reason=topology_affinity_mismatch errno=%d\n",
               EIO);
    } else {
        printf("MM_PERF_TOPOLOGY status=ok online_cpus=%ld\n", online_cpus);
        printf("MM_PERF_AFFINITY status=ok bytes=%zu allowed_cpus=%zu\n",
               affinity_bytes, affinity_cpus);
    }
    if (config.pin_workers == 0) {
        config.pin_workers = online_cpus > 0 ? (size_t)online_cpus : 1U;
        if (config.pin_workers > MAX_PIN_WORKERS) {
            config.pin_workers = MAX_PIN_WORKERS;
        }
    }
    if (config.pin_workers > affinity_cpus ||
        config.pin_workers > MAX_PIN_WORKERS) {
        fprintf(stderr, "pin workers exceed available guest CPUs\n");
        return 2;
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
    result = run_pin_metric(1U, config.pin_iterations, false,
                            affinity_cpu_ids, config.live_vmas, page_size);
    emit_metric("pin_throughput", &result);
    result = run_pin_metric(config.pin_workers, config.pin_iterations, true,
                            affinity_cpu_ids, config.live_vmas, page_size);
    emit_metric("pin_contention", &result);
    puts("MM_PERF_DONE status=ok");
    return 0;
}
