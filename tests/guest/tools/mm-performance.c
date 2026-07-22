#define _GNU_SOURCE

/*
 * Repository-owned, end-to-end MM evidence helper.  The latency metrics use
 * nearest-rank quantiles.  The direct-I/O cases are user-visible proxies: a
 * successful sample does not by itself prove that the kernel selected its
 * short-pin fast path, nor does it isolate one internal lock.
 */

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
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
#include <sys/wait.h>
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

#ifndef MAP_FIXED_NOREPLACE
#define MAP_FIXED_NOREPLACE 0x100000
#endif

#if defined(__riscv)
#define MM_PERF_COMPILED_ARCH "rv"
#elif defined(__loongarch__)
#define MM_PERF_COMPILED_ARCH "la"
#else
#define MM_PERF_COMPILED_ARCH "host"
#endif

#define MM_PERF_RUN_SCHEMA "thekernel-mm-performance-run-v2"

enum {
    DEFAULT_ITERATIONS = 256,
    DEFAULT_LIVE_VMAS = 512,
    DEFAULT_PIN_ITERATIONS = 64,
    PIN_BUFFER_BYTES = 64 * 1024,
    PIN_WARMUP_ITERATIONS = 64,
    PROTECT_TOUCH_PAGES = 64,
    MREMAP_FIXED_PAGES = 2,
    MREMAP_FILE_PAGES = 2,
    MREMAP_SMALL_PAGES = 16,
    MREMAP_LARGE_PAGES = 32,
    MREMAP_CONTENTION_WORKERS = 2,
    MREMAP_CONTENTION_SLOT_PAGES = 2,
    MREMAP_CONTENTION_STRIDE_PAGES = 3,
    MAX_ITERATIONS = 100000,
    MAX_LIVE_VMAS = 16384,
    MAX_PIN_ITERATIONS = 10000,
    MAX_PIN_WORKERS = 64,
    CROSS_AS_TIMEOUT_MS = 30000,
    CROSS_AS_CLEANUP_TIMEOUT_MS = 5000,
};

static const uint64_t CROSS_AS_COW_PARENT_SENTINEL =
    UINT64_C(0x54484b504152454e);

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

struct vma_fixture_report {
    size_t requested_vmas;
    bool has_fixture_vmas;
    size_t fixture_vmas;
    uintptr_t base;
    size_t span;
    size_t stride;
    size_t page_size;
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

static void emit_metric_record(const char *name,
                               const struct metric_result *result,
                               const struct vma_fixture_report *fixture)
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
        if (fixture != NULL) {
            printf(" requested_vmas=%zu fixture_vmas=",
                   fixture->requested_vmas);
            if (fixture->has_fixture_vmas) {
                printf("%zu", fixture->fixture_vmas);
            } else {
                printf("missing");
            }
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
    if (fixture != NULL) {
        printf(" requested_vmas=%zu fixture_vmas=",
               fixture->requested_vmas);
        if (fixture->has_fixture_vmas) {
            printf("%zu", fixture->fixture_vmas);
        } else {
            printf("missing");
        }
    }
    printf(" reason=%s errno=%d\n", result->reason, result->error_number);
}

static void emit_metric(const char *name, const struct metric_result *result)
{
    emit_metric_record(name, result, NULL);
}

static void emit_metric_with_fixture(const char *name,
                                     const struct metric_result *result,
                                     const struct vma_fixture_report *fixture)
{
    emit_metric_record(name, result, fixture);
}

static void init_vma_fixture_report(struct vma_fixture_report *report,
                                    size_t requested_vmas)
{
    *report = (struct vma_fixture_report){
        .requested_vmas = requested_vmas,
        .has_fixture_vmas = false,
        .fixture_vmas = 0,
        .base = 0,
        .span = 0,
        .stride = 0,
        .page_size = 0,
    };
}

static int cleanup_mappings(void **mappings, size_t count, size_t page_size)
{
    int first_error = 0;
    size_t index;

    for (index = 0; index < count; ++index) {
        if (mappings[index] != MAP_FAILED && mappings[index] != NULL) {
            if (munmap(mappings[index], page_size) != 0 && first_error == 0) {
                first_error = errno;
            }
        }
    }
    return first_error;
}

static void record_cleanup_error(int error_number, int *first_error)
{
    if (error_number != 0 && *first_error == 0) {
        *first_error = error_number;
    }
}

static void fail_stop_join_error(const char *workload, int error_number)
{
    fprintf(stderr, "MM_PERF_FATAL workload=%s reason=pthread_join_failed errno=%d\n",
            workload, error_number);
    fflush(stderr);
    _Exit(3);
}

static int count_sparse_vma_fixture(uintptr_t base, size_t span,
                                    size_t stride, size_t page_size,
                                    size_t *live_vmas,
                                    const char **failure_reason,
                                    int *failure_errno)
{
    char line[512];
    FILE *maps;
    size_t count = 0;

    maps = fopen("/proc/self/maps", "r");
    if (maps == NULL) {
        *failure_reason = "fixture_proc_maps_unavailable";
        *failure_errno = errno;
        return -1;
    }
    while (fgets(line, sizeof(line), maps) != NULL) {
        uintptr_t start;
        uintptr_t end;
        uintptr_t offset;

        if (sscanf(line, "%" SCNxPTR "-%" SCNxPTR, &start, &end) != 2) {
            continue;
        }
        if (end <= base || start >= base + span) {
            continue;
        }
        if (start < base || end > base + span) {
            *failure_reason = "fixture_proc_maps_layout_mismatch";
            *failure_errno = EIO;
            (void)fclose(maps);
            return -1;
        }
        offset = start - base;
        if (offset % stride != 0 || end - start != page_size) {
            *failure_reason = "fixture_proc_maps_layout_mismatch";
            *failure_errno = EIO;
            (void)fclose(maps);
            return -1;
        }
        count += 1U;
    }
    if (ferror(maps)) {
        *failure_reason = "fixture_proc_maps_read_failed";
        *failure_errno = errno != 0 ? errno : EIO;
        (void)fclose(maps);
        return -1;
    }
    if (fclose(maps) != 0) {
        *failure_reason = "fixture_proc_maps_read_failed";
        *failure_errno = errno;
        return -1;
    }
    *live_vmas = count;
    return 0;
}

static int verify_sparse_vma_fixture(struct vma_fixture_report *report,
                                     size_t *verified_vmas,
                                     const char **failure_reason,
                                     int *failure_errno)
{
    size_t count = 0;

    if (!report->has_fixture_vmas || report->base == 0 ||
        report->span == 0 || report->stride == 0 || report->page_size == 0) {
        *failure_reason = "fixture_geometry_unavailable";
        *failure_errno = EINVAL;
        return -1;
    }
    if (count_sparse_vma_fixture(report->base, report->span, report->stride,
                                 report->page_size, &count, failure_reason,
                                 failure_errno) != 0) {
        return -1;
    }
    if (count != report->requested_vmas) {
        *failure_reason = "fixture_vma_count_mismatch";
        *failure_errno = EIO;
        return -1;
    }
    report->fixture_vmas = count;
    if (verified_vmas != NULL) {
        *verified_vmas = count;
    }
    return 0;
}

static int setup_sparse_vma_fixture(void **mappings, size_t count,
                                    size_t page_size, size_t *completed,
                                    struct vma_fixture_report *report,
                                    const char **failure_reason,
                                    int *failure_errno)
{
    const size_t stride = page_size * 2U;
    size_t span;
    void *reservation;
    uintptr_t base;
    size_t index;

    init_vma_fixture_report(report, count);
    *completed = 0;
    *failure_reason = "fixture_setup_failed";
    *failure_errno = 0;
    if (page_size > SIZE_MAX / 2U || count > SIZE_MAX / stride) {
        *failure_reason = "fixture_span_overflow";
        *failure_errno = EOVERFLOW;
        return -1;
    }
    span = count * stride;
    reservation = mmap(NULL, span, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS,
                       -1, 0);
    if (reservation == MAP_FAILED) {
        *failure_reason = "fixture_reserve_failed";
        *failure_errno = errno;
        return -1;
    }
    base = (uintptr_t)reservation;
    if (base > UINTPTR_MAX - span) {
        *failure_reason = "fixture_span_overflow";
        *failure_errno = EOVERFLOW;
        (void)munmap(reservation, span);
        return -1;
    }
    if (munmap(reservation, span) != 0) {
        *failure_reason = "fixture_reserve_unmap_failed";
        *failure_errno = errno;
        return -1;
    }
    report->base = base;
    report->span = span;
    report->stride = stride;
    report->page_size = page_size;
    for (index = 0; index < count; ++index) {
        void *target = (void *)(base + index * stride);
        void *mapping = mmap(target, page_size, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS |
                                 MAP_FIXED_NOREPLACE,
                             -1, 0);

        if (mapping == MAP_FAILED) {
            *failure_reason =
                errno == EINVAL || errno == ENOSYS || errno == EOPNOTSUPP
                    ? "fixture_fixed_noreplace_unavailable"
                    : "fixture_fixed_noreplace_failed";
            *failure_errno = errno;
            return -1;
        }
        if (mapping != target) {
            *failure_reason = "fixture_fixed_noreplace_ignored";
            *failure_errno = EOPNOTSUPP;
            (void)munmap(mapping, page_size);
            return -1;
        }
        mappings[index] = mapping;
        *completed += 1U;
    }
    report->has_fixture_vmas = true;
    if (verify_sparse_vma_fixture(report, &report->fixture_vmas,
                                  failure_reason, failure_errno) != 0) {
        return -1;
    }
    return 0;
}

static struct metric_result run_vma_scale(const struct config *config,
                                          size_t page_size,
                                          struct vma_fixture_report *fixture)
{
    void **live = calloc(config->live_vmas, sizeof(*live));
    uint64_t *samples = calloc(config->iterations, sizeof(*samples));
    struct metric_result result;
    const char *fixture_reason = "fixture_setup_failed";
    int fixture_errno = 0;
    int cleanup_error = 0;
    size_t live_count = 0;
    size_t index;

    init_vma_fixture_report(fixture, config->live_vmas);
    if (live == NULL || samples == NULL) {
        result = missing_result("allocation_failed", ENOMEM, false);
        goto out;
    }
    if (setup_sparse_vma_fixture(live, config->live_vmas, page_size,
                                 &live_count, fixture, &fixture_reason,
                                 &fixture_errno) != 0) {
        result = missing_result(fixture_reason, fixture_errno, false);
        goto out;
    }
    if (verify_sparse_vma_fixture(fixture, NULL, &fixture_reason,
                                  &fixture_errno) != 0) {
        result = missing_result(fixture_reason, fixture_errno, false);
        goto out;
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
    if (verify_sparse_vma_fixture(fixture, NULL, &fixture_reason,
                                  &fixture_errno) != 0) {
        result = missing_result(fixture_reason, fixture_errno, false);
        goto out;
    }
    result = successful_result(samples, config->iterations, false, 0);

out:
    if (live != NULL) {
        record_cleanup_error(
            cleanup_mappings(live, live_count, page_size), &cleanup_error);
    }
    if (result.ok && cleanup_error != 0) {
        result = missing_result("vma_fixture_cleanup_failed", cleanup_error,
                                false);
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

static struct metric_result
run_mremap_fixed_replace_latency(const struct config *config, size_t page_size,
                                 struct vma_fixture_report *fixture)
{
#ifdef SYS_mremap
    const size_t mapping_size = page_size * MREMAP_FIXED_PAGES;
    uint64_t *samples = calloc(config->iterations, sizeof(*samples));
    void **vma_fixture = calloc(config->live_vmas, sizeof(*vma_fixture));
    void *source = MAP_FAILED;
    void *destination = MAP_FAILED;
    struct metric_result result;
    const char *fixture_reason = "fixture_setup_failed";
    int fixture_errno = 0;
    int cleanup_error = 0;
    size_t fixture_count = 0;
    size_t index;

    init_vma_fixture_report(fixture, config->live_vmas);
    if (samples == NULL || vma_fixture == NULL) {
        result = missing_result("allocation_failed", ENOMEM, false);
        goto out;
    }
    if (setup_sparse_vma_fixture(vma_fixture, config->live_vmas, page_size,
                                 &fixture_count, fixture, &fixture_reason,
                                 &fixture_errno) != 0) {
        result = missing_result(fixture_reason, fixture_errno, false);
        goto out;
    }
    if (verify_sparse_vma_fixture(fixture, NULL, &fixture_reason,
                                  &fixture_errno) != 0) {
        result = missing_result(fixture_reason, fixture_errno, false);
        goto out;
    }

    for (index = 0; index < config->iterations; ++index) {
        void *fixed_target;
        void *remapped;
        uint64_t before;
        uint64_t after;
        int remap_errno = 0;

        source = mmap(NULL, mapping_size, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (source == MAP_FAILED) {
            result = missing_result("fixed_source_mmap_failed", errno, false);
            goto out;
        }
        destination = mmap(NULL, mapping_size, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (destination == MAP_FAILED) {
            result = missing_result("fixed_target_mmap_failed", errno, false);
            goto out;
        }
        write_page_sentinels(source, mapping_size, page_size);
        memset(destination, 0xa5, mapping_size);
        fixed_target = destination;

        if (monotonic_ns(&before) != 0) {
            result = missing_result("clock_failed", errno, false);
            goto out;
        }
        remapped = (void *)syscall(SYS_mremap, source, mapping_size,
                                   mapping_size,
                                   MREMAP_MAYMOVE | MREMAP_FIXED, fixed_target);
        if (remapped == MAP_FAILED) {
            remap_errno = errno;
        }
        if (monotonic_ns(&after) != 0) {
            int saved_errno = errno;

            if (remapped != MAP_FAILED) {
                source = MAP_FAILED;
                destination = remapped;
            }
            result = missing_result("clock_failed", saved_errno, false);
            goto out;
        }
        if (remapped == MAP_FAILED) {
            result = missing_result("mremap_fixed_replace_unavailable",
                                    remap_errno, false);
            goto out;
        }
        source = MAP_FAILED;
        destination = remapped;
        if (remapped != fixed_target) {
            result = missing_result("mremap_fixed_target_mismatch", EIO,
                                    false);
            goto out;
        }
        if (!page_sentinels_match(remapped, mapping_size, page_size)) {
            result = missing_result("mremap_fixed_content_mismatch", EIO,
                                    false);
            goto out;
        }
        samples[index] = after - before;
        if (munmap(destination, mapping_size) != 0) {
            result = missing_result("mremap_fixed_cleanup_failed", errno,
                                    false);
            goto out;
        }
        destination = MAP_FAILED;
    }
    if (verify_sparse_vma_fixture(fixture, NULL, &fixture_reason,
                                  &fixture_errno) != 0) {
        result = missing_result(fixture_reason, fixture_errno, false);
        goto out;
    }
    result = successful_result(samples, config->iterations, false, 0);

out:
    if (source != MAP_FAILED) {
        if (munmap(source, mapping_size) != 0) {
            record_cleanup_error(errno, &cleanup_error);
        }
    }
    if (destination != MAP_FAILED) {
        if (munmap(destination, mapping_size) != 0) {
            record_cleanup_error(errno, &cleanup_error);
        }
    }
    if (vma_fixture != NULL) {
        record_cleanup_error(
            cleanup_mappings(vma_fixture, fixture_count, page_size),
            &cleanup_error);
    }
    if (result.ok && cleanup_error != 0) {
        result = missing_result("mremap_fixed_cleanup_failed", cleanup_error,
                                false);
    }
    free(vma_fixture);
    free(samples);
    return result;
#else
    (void)page_size;
    init_vma_fixture_report(fixture, config->live_vmas);
    return missing_result("mremap_syscall_unavailable", ENOSYS, false);
#endif
}

static struct metric_result
run_mremap_file_duplicate_latency(const struct config *config,
                                  size_t page_size)
{
#ifdef SYS_mremap
    const size_t mapping_size = page_size * MREMAP_FILE_PAGES;
    char path[] = "/tmp/thekernel-mm-remap-file-XXXXXX";
    uint64_t *samples = calloc(config->iterations, sizeof(*samples));
    void *mapping = MAP_FAILED;
    void *alias = MAP_FAILED;
    struct metric_result result;
    bool path_created = false;
    int fd = -1;
    size_t index;

    if (samples == NULL) {
        return missing_result("allocation_failed", ENOMEM, false);
    }
    fd = mkstemp(path);
    if (fd < 0) {
        result = missing_result("file_duplicate_open_failed", errno, false);
        goto out;
    }
    path_created = true;
    if (ftruncate(fd, (off_t)mapping_size) != 0) {
        result = missing_result("file_duplicate_truncate_failed", errno, false);
        goto out;
    }
    mapping = mmap(NULL, mapping_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd,
                   0);
    if (mapping == MAP_FAILED) {
        result = missing_result("file_duplicate_mmap_failed", errno, false);
        goto out;
    }
    write_page_sentinels(mapping, mapping_size, page_size);

    for (index = 0; index < config->iterations; ++index) {
        const unsigned char alias_value =
            (unsigned char)(0x80U ^ (index & 0x3fU));
        const unsigned char original_value =
            (unsigned char)(0x40U ^ (index & 0x3fU));
        uint64_t before;
        uint64_t after;
        int remap_errno = 0;

        if (monotonic_ns(&before) != 0) {
            result = missing_result("clock_failed", errno, false);
            goto out;
        }
        alias = (void *)syscall(SYS_mremap, mapping, 0, mapping_size,
                                MREMAP_MAYMOVE);
        if (alias == MAP_FAILED) {
            remap_errno = errno;
        }
        if (monotonic_ns(&after) != 0) {
            result = missing_result("clock_failed", errno, false);
            goto out;
        }
        if (alias == MAP_FAILED) {
            result = missing_result("mremap_file_duplicate_unavailable",
                                    remap_errno, false);
            goto out;
        }
        if (alias == mapping) {
            alias = MAP_FAILED;
            result = missing_result("mremap_file_duplicate_alias_mismatch",
                                    EIO, false);
            goto out;
        }
        if (!page_sentinels_match(mapping, mapping_size, page_size) ||
            !page_sentinels_match(alias, mapping_size, page_size)) {
            result = missing_result("mremap_file_duplicate_content_mismatch",
                                    EIO, false);
            goto out;
        }
        ((volatile unsigned char *)alias)[0] = alias_value;
        if (((volatile unsigned char *)mapping)[0] != alias_value) {
            result = missing_result("mremap_file_alias_to_original_mismatch",
                                    EIO, false);
            goto out;
        }
        ((volatile unsigned char *)mapping)[page_size] = original_value;
        if (((volatile unsigned char *)alias)[page_size] != original_value) {
            result = missing_result("mremap_file_original_to_alias_mismatch",
                                    EIO, false);
            goto out;
        }
        samples[index] = after - before;
        write_page_sentinels(mapping, mapping_size, page_size);
        if (munmap(alias, mapping_size) != 0) {
            result = missing_result("mremap_file_alias_cleanup_failed", errno,
                                    false);
            goto out;
        }
        alias = MAP_FAILED;
    }
    result = successful_result(samples, config->iterations, false, 0);

out:
    if (alias != MAP_FAILED) {
        (void)munmap(alias, mapping_size);
    }
    if (mapping != MAP_FAILED) {
        (void)munmap(mapping, mapping_size);
    }
    if (fd >= 0) {
        const int close_result = close(fd);
        const int close_errno = errno;

        if (close_result != 0 && result.ok) {
            result = missing_result("file_duplicate_close_failed", close_errno,
                                    false);
        }
    }
    if (path_created) {
        const int unlink_result = unlink(path);
        const int unlink_errno = errno;

        if (unlink_result != 0 && result.ok) {
            result = missing_result("file_duplicate_unlink_failed",
                                    unlink_errno, false);
        }
    }
    free(samples);
    return result;
#else
    (void)config;
    (void)page_size;
    return missing_result("mremap_syscall_unavailable", ENOSYS, false);
#endif
}

static struct metric_result
run_mremap_shared_anon_resize_latency(const struct config *config,
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
                   MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (mapping == MAP_FAILED) {
        result = missing_result("shared_anon_setup_mmap_failed", errno, false);
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
            result = missing_result("mremap_shared_anon_resize_unavailable",
                                    remap_errno, false);
            goto out;
        }
        mapping = remapped;
        current_size = target_size;
        if (!page_sentinels_match(mapping, small_size, page_size)) {
            result = missing_result("mremap_shared_anon_content_mismatch", EIO,
                                    false);
            goto out;
        }
        samples[index] = after - before;
        /*
         * Linux may leave the grown tail outside the shared anonymous
         * backing object's populated extent.  The contract under test is
         * resize plus preservation of the original prefix, so keep all
         * accesses within that prefix on both sides of the grow/shrink pair.
         */
        write_page_sentinels(mapping, small_size, page_size);
    }
    if (current_size != small_size) {
        result = missing_result("mremap_shared_anon_restore_failed", EIO,
                                false);
        goto out;
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
    struct worker_gate *gate;
    atomic_int *failure_errno;
};

struct worker_gate {
    pthread_mutex_t mutex;
    pthread_cond_t condition;
    atomic_size_t ready_workers;
    atomic_size_t started_workers;
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

static int cleanup_pin_workers(struct pin_worker *workers, size_t count)
{
    int first_error = 0;
    size_t index;

    if (workers == NULL) {
        return 0;
    }
    for (index = 0; index < count; ++index) {
        if (workers[index].fd >= 0) {
            if (close(workers[index].fd) != 0 && first_error == 0) {
                first_error = errno;
            }
        }
        if (workers[index].path[0] != '\0') {
            if (unlink(workers[index].path) != 0 && errno != ENOENT &&
                first_error == 0) {
                first_error = errno;
            }
        }
        free(workers[index].buffer);
    }
    return first_error;
}

static struct metric_result run_pin_metric(size_t worker_count,
                                           size_t iterations,
                                           bool contention,
                                           const int *worker_cpus,
                                           size_t live_vmas,
                                           size_t page_size,
                                           struct vma_fixture_report *fixture)
{
    const size_t sample_count = worker_count * iterations;
    const char *mode = contention ? "contention" : "single";
    pthread_t *threads = NULL;
    struct pin_worker *workers = NULL;
    uint64_t *samples = NULL;
    void **vma_fixture = NULL;
    struct worker_gate gate;
    bool gate_mutex_initialized = false;
    bool gate_condition_initialized = false;
    atomic_int failure_errno;
    const char *fixture_reason = "fixture_setup_failed";
    int fixture_errno = 0;
    size_t live_count = 0;
    struct metric_result result;
    size_t initialized = 0;
    size_t created = 0;
    size_t index;
    size_t fixture_before_vmas = 0;
    size_t fixture_after_vmas = 0;
    bool fixture_before_failed = false;
    int cleanup_error = 0;
    uint64_t start_ns = 0;
    uint64_t end_ns;

    init_vma_fixture_report(fixture, live_vmas);
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

    if (setup_sparse_vma_fixture(vma_fixture, live_vmas, page_size,
                                 &live_count, fixture, &fixture_reason,
                                 &fixture_errno) != 0) {
        result = missing_result(fixture_reason, fixture_errno, true);
        goto out;
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
    atomic_init(&gate.started_workers, 0U);
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
        if (created == worker_count &&
            verify_sparse_vma_fixture(fixture, &fixture_before_vmas,
                                      &fixture_reason,
                                      &fixture_errno) != 0) {
            fixture_before_failed = true;
            record_first_failure(&failure_errno, fixture_errno);
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
        /*
         * These workers borrow the gate, sample array, fd, and buffer.  A
         * timed return would make those borrows dangling, while portable
         * pthreads provide no cancellation-safe timed join.  The nightly
         * guest process timeout remains the final owner for a pwrite that
         * never returns; do not detach and manufacture a completed metric.
         */
        int join_error = pthread_join(threads[index], NULL);

        if (join_error != 0) {
            fail_stop_join_error("direct_io_pin_proxy", join_error);
        }
    }
    if (created != worker_count) {
        result = missing_result("pthread_create_failed",
                                atomic_load_explicit(&failure_errno,
                                                     memory_order_acquire),
                                true);
        goto out;
    }
    if (fixture_before_failed) {
        result = missing_result(fixture_reason, fixture_errno, true);
        goto out;
    }
    if (atomic_load_explicit(&failure_errno, memory_order_acquire) != 0) {
        result = missing_result("direct_io_operation_failed",
                                atomic_load_explicit(&failure_errno,
                                                     memory_order_acquire),
                                true);
        goto out;
    }
    if (verify_sparse_vma_fixture(fixture, &fixture_after_vmas,
                                  &fixture_reason, &fixture_errno) != 0) {
        result = missing_result(fixture_reason, fixture_errno, true);
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
               " over_10ms=%zu over_50ms=%zu"
               " fixture_before_vmas=%zu fixture_after_vmas=%zu\n",
               mode, index, workers[index].cpu, workers[index].completed,
               worker_p99, over_10_ms, over_50_ms, fixture_before_vmas,
               fixture_after_vmas);
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
        record_cleanup_error(
            cleanup_mappings(vma_fixture, live_count, page_size),
            &cleanup_error);
    }
    if (gate_condition_initialized) {
        record_cleanup_error(pthread_cond_destroy(&gate.condition),
                             &cleanup_error);
    }
    if (gate_mutex_initialized) {
        record_cleanup_error(pthread_mutex_destroy(&gate.mutex),
                             &cleanup_error);
    }
    record_cleanup_error(cleanup_pin_workers(workers, initialized),
                         &cleanup_error);
    if (result.ok && cleanup_error != 0) {
        result = missing_result("direct_io_cleanup_failed", cleanup_error,
                                true);
    }
    free(vma_fixture);
    free(samples);
    free(workers);
    free(threads);
    return result;
}

#ifdef SYS_mremap
struct mremap_contention_worker {
    size_t index;
    size_t iterations;
    size_t page_size;
    size_t mapping_size;
    int cpu;
    int start_cpu;
    int end_cpu;
    void *slot_a;
    void *slot_b;
    void *current;
    uint64_t *samples;
    size_t completed;
    size_t remaps_succeeded;
    uint64_t start_ns;
    uint64_t end_ns;
    struct worker_gate *gate;
    atomic_int *failure_errno;
};

static void write_contention_sentinels(
    const struct mremap_contention_worker *worker)
{
    volatile unsigned char *bytes = worker->current;
    size_t page;

    for (page = 0; page < worker->mapping_size / worker->page_size; ++page) {
        bytes[page * worker->page_size] =
            (unsigned char)(0x60U ^ (worker->index << 3U) ^ page);
    }
}

static bool contention_sentinels_match(
    const struct mremap_contention_worker *worker)
{
    const volatile unsigned char *bytes = worker->current;
    size_t page;

    for (page = 0; page < worker->mapping_size / worker->page_size; ++page) {
        const unsigned char expected =
            (unsigned char)(0x60U ^ (worker->index << 3U) ^ page);

        if (bytes[page * worker->page_size] != expected) {
            return false;
        }
    }
    return true;
}

static void *mremap_contention_worker_main(void *opaque)
{
    struct mremap_contention_worker *worker = opaque;
    cpu_set_t affinity;
    bool abort;
    int worker_error;
    size_t index;

    CPU_ZERO(&affinity);
    CPU_SET(worker->cpu, &affinity);
    worker_error =
        pthread_setaffinity_np(pthread_self(), sizeof(affinity), &affinity);
    if (worker_error != 0) {
        record_first_failure(worker->failure_errno, worker_error);
    } else if (sched_getcpu() != worker->cpu) {
        record_first_failure(worker->failure_errno, EXDEV);
    }
    if (!contention_sentinels_match(worker)) {
        record_first_failure(worker->failure_errno, EIO);
    }

    atomic_fetch_add_explicit(&worker->gate->ready_workers, 1U,
                              memory_order_release);
    worker_error = pthread_mutex_lock(&worker->gate->mutex);
    if (worker_error != 0) {
        record_first_failure(worker->failure_errno, worker_error);
        return NULL;
    }
    while (!worker->gate->start && !worker->gate->abort) {
        worker_error =
            pthread_cond_wait(&worker->gate->condition, &worker->gate->mutex);
        if (worker_error != 0) {
            record_first_failure(worker->failure_errno, worker_error);
            worker->gate->abort = true;
            (void)pthread_cond_broadcast(&worker->gate->condition);
            break;
        }
    }
    abort = worker->gate->abort;
    worker_error = pthread_mutex_unlock(&worker->gate->mutex);
    if (worker_error != 0) {
        record_first_failure(worker->failure_errno, worker_error);
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
    if (monotonic_ns(&worker->start_ns) != 0) {
        record_first_failure(worker->failure_errno, errno);
        return NULL;
    }
    /*
     * The start-condition broadcast does not guarantee that both pinned
     * workers run before one completes a short sample set.  Qualify the
     * contention window only after every worker has published its timestamp.
     */
    atomic_fetch_add_explicit(&worker->gate->started_workers, 1U,
                              memory_order_release);
    while (atomic_load_explicit(&worker->gate->started_workers,
                                memory_order_acquire) <
           MREMAP_CONTENTION_WORKERS) {
        if (atomic_load_explicit(worker->failure_errno,
                                 memory_order_acquire) != 0) {
            return NULL;
        }
        (void)sched_yield();
    }

    for (index = 0; index < worker->iterations; ++index) {
        void *target = worker->current == worker->slot_a ? worker->slot_b
                                                        : worker->slot_a;
        void *remapped;
        uint64_t before;
        uint64_t after;
        int remap_errno = 0;

        if (atomic_load_explicit(worker->failure_errno,
                                 memory_order_acquire) != 0) {
            break;
        }
        if (monotonic_ns(&before) != 0) {
            record_first_failure(worker->failure_errno, errno);
            break;
        }
        remapped = (void *)syscall(SYS_mremap, worker->current,
                                   worker->mapping_size, worker->mapping_size,
                                   MREMAP_MAYMOVE | MREMAP_FIXED, target);
        if (remapped == MAP_FAILED) {
            remap_errno = errno;
        } else {
            worker->current = remapped;
            worker->remaps_succeeded += 1U;
        }
        if (monotonic_ns(&after) != 0) {
            record_first_failure(worker->failure_errno, errno);
            break;
        }
        if (remapped == MAP_FAILED) {
            record_first_failure(worker->failure_errno, remap_errno);
            break;
        }
        if (remapped != target || !contention_sentinels_match(worker)) {
            record_first_failure(worker->failure_errno, EIO);
            break;
        }
        worker->samples[index] = after - before;
        worker->completed = index + 1U;
    }
    if (monotonic_ns(&worker->end_ns) != 0) {
        record_first_failure(worker->failure_errno, errno);
    }
    worker->end_cpu = sched_getcpu();
    if (worker->end_cpu != worker->cpu) {
        record_first_failure(worker->failure_errno, EXDEV);
    }
    return NULL;
}

static bool ranges_overlap(uintptr_t left, uintptr_t right, size_t bytes)
{
    return left < right + bytes && right < left + bytes;
}

static struct metric_result run_mremap_disjoint_same_as_contention(
    const struct config *config, const int *worker_cpus,
    size_t available_cpus, size_t page_size,
    struct vma_fixture_report *fixture)
{
    const size_t worker_count = MREMAP_CONTENTION_WORKERS;
    const size_t slot_count = worker_count * 2U;
    const size_t sample_count = worker_count * config->iterations;
    const size_t mapping_size = page_size * MREMAP_CONTENTION_SLOT_PAGES;
    const size_t slot_stride = page_size * MREMAP_CONTENTION_STRIDE_PAGES;
    pthread_t threads[MREMAP_CONTENTION_WORKERS];
    struct mremap_contention_worker workers[MREMAP_CONTENTION_WORKERS];
    struct worker_gate gate;
    atomic_int failure_errno;
    uint64_t *samples = NULL;
    void **vma_fixture = NULL;
    void *reservation = MAP_FAILED;
    uintptr_t reservation_base = 0;
    size_t reservation_span = 0;
    size_t live_count = 0;
    size_t created = 0;
    bool slot_mapped[MREMAP_CONTENTION_WORKERS * 2U] = {false};
    bool gate_mutex_initialized = false;
    bool gate_condition_initialized = false;
    bool reservation_mapped = false;
    bool fixture_failed = false;
    const char *fixture_reason = "fixture_setup_failed";
    int fixture_errno = 0;
    int cleanup_error = 0;
    struct metric_result result;
    size_t fixture_before_vmas = 0;
    size_t fixture_after_vmas = 0;
    size_t index;

    init_vma_fixture_report(fixture, config->live_vmas);
    memset(threads, 0, sizeof(threads));
    memset(workers, 0, sizeof(workers));
    if (available_cpus < worker_count) {
        return missing_result("insufficient_online_cpus", 0, false);
    }
    if (page_size > SIZE_MAX / MREMAP_CONTENTION_STRIDE_PAGES ||
        slot_count > SIZE_MAX / slot_stride) {
        return missing_result("mremap_contention_span_overflow", EOVERFLOW,
                              false);
    }
    reservation_span = slot_count * slot_stride;
    samples = calloc(sample_count, sizeof(*samples));
    vma_fixture = calloc(config->live_vmas, sizeof(*vma_fixture));
    if (samples == NULL || vma_fixture == NULL) {
        result = missing_result("allocation_failed", ENOMEM, false);
        goto out;
    }

    reservation = mmap(NULL, reservation_span, PROT_NONE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (reservation == MAP_FAILED) {
        result = missing_result("mremap_contention_reserve_failed", errno,
                                false);
        goto out;
    }
    reservation_mapped = true;
    reservation_base = (uintptr_t)reservation;
    if (reservation_base > UINTPTR_MAX - reservation_span) {
        result = missing_result("mremap_contention_span_overflow", EOVERFLOW,
                                false);
        goto out;
    }
    if (munmap(reservation, reservation_span) != 0) {
        result = missing_result("mremap_contention_reserve_unmap_failed",
                                errno, false);
        goto out;
    }
    reservation_mapped = false;
    reservation = MAP_FAILED;

    for (index = 0; index < slot_count; ++index) {
        void *target = (void *)(reservation_base + index * slot_stride);
        void *mapping = mmap(target, mapping_size,
                             PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS |
                                 MAP_FIXED_NOREPLACE,
                             -1, 0);

        if (mapping == MAP_FAILED) {
            result = missing_result(
                errno == EINVAL || errno == ENOSYS || errno == EOPNOTSUPP
                    ? "mremap_contention_fixed_noreplace_unavailable"
                    : "mremap_contention_slot_mmap_failed",
                errno, false);
            goto out;
        }
        if (mapping != target) {
            if (munmap(mapping, mapping_size) != 0) {
                result = missing_result(
                    "mremap_contention_unexpected_mapping_cleanup_failed",
                    errno, false);
            } else {
                result = missing_result(
                    "mremap_contention_fixed_noreplace_ignored", EOPNOTSUPP,
                    false);
            }
            goto out;
        }
        slot_mapped[index] = true;
    }

    memset(&gate, 0, sizeof(gate));
    atomic_init(&gate.ready_workers, 0U);
    atomic_init(&gate.started_workers, 0U);
    {
        int gate_error = pthread_mutex_init(&gate.mutex, NULL);

        if (gate_error != 0) {
            result = missing_result("pthread_mutex_init_failed", gate_error,
                                    false);
            goto out;
        }
        gate_mutex_initialized = true;
        gate_error = pthread_cond_init(&gate.condition, NULL);
        if (gate_error != 0) {
            result = missing_result("pthread_cond_init_failed", gate_error,
                                    false);
            goto out;
        }
        gate_condition_initialized = true;
    }
    atomic_init(&failure_errno, 0);
    for (index = 0; index < worker_count; ++index) {
        int thread_error;

        workers[index].index = index;
        workers[index].iterations = config->iterations;
        workers[index].page_size = page_size;
        workers[index].mapping_size = mapping_size;
        workers[index].cpu = worker_cpus[index];
        workers[index].start_cpu = -1;
        workers[index].end_cpu = -1;
        workers[index].slot_a =
            (void *)(reservation_base + (index * 2U) * slot_stride);
        workers[index].slot_b =
            (void *)(reservation_base + (index * 2U + 1U) * slot_stride);
        workers[index].current = workers[index].slot_a;
        workers[index].samples = samples + index * config->iterations;
        workers[index].gate = &gate;
        workers[index].failure_errno = &failure_errno;
        write_contention_sentinels(&workers[index]);
        memset(workers[index].slot_b, 0xa5, mapping_size);
        thread_error = pthread_create(&threads[index], NULL,
                                      mremap_contention_worker_main,
                                      &workers[index]);
        if (thread_error != 0) {
            record_first_failure(&failure_errno, thread_error);
            break;
        }
        created += 1U;
    }

    while (atomic_load_explicit(&gate.ready_workers,
                                memory_order_acquire) < created) {
        (void)sched_yield();
    }
    if (created == worker_count &&
        setup_sparse_vma_fixture(vma_fixture, config->live_vmas, page_size,
                                 &live_count, fixture, &fixture_reason,
                                 &fixture_errno) != 0) {
        fixture_failed = true;
        record_first_failure(&failure_errno, fixture_errno);
    }
    if (created == worker_count &&
        atomic_load_explicit(&failure_errno, memory_order_acquire) == 0 &&
        verify_sparse_vma_fixture(fixture, &fixture_before_vmas,
                                  &fixture_reason, &fixture_errno) != 0) {
        fixture_failed = true;
        record_first_failure(&failure_errno, fixture_errno);
    }
    {
        int gate_error = pthread_mutex_lock(&gate.mutex);

        if (gate_error != 0) {
            record_first_failure(&failure_errno, gate_error);
        } else {
            if (created != worker_count ||
                atomic_load_explicit(&failure_errno,
                                     memory_order_acquire) != 0) {
                gate.abort = true;
            } else {
                gate.start = true;
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
            fail_stop_join_error("mremap_disjoint_same_as_contention",
                                 join_error);
        }
    }
    for (index = 0; index < created; ++index) {
        if (workers[index].remaps_succeeded != 0) {
            if (workers[index].current == workers[index].slot_a) {
                slot_mapped[index * 2U] = true;
                slot_mapped[index * 2U + 1U] = false;
            } else if (workers[index].current == workers[index].slot_b) {
                slot_mapped[index * 2U] = false;
                slot_mapped[index * 2U + 1U] = true;
            } else {
                slot_mapped[index * 2U] = false;
                slot_mapped[index * 2U + 1U] = false;
                if (munmap(workers[index].current, mapping_size) != 0) {
                    record_cleanup_error(errno, &cleanup_error);
                }
            }
        }
    }
    if (created != worker_count) {
        result = missing_result("pthread_create_failed",
                                atomic_load_explicit(&failure_errno,
                                                     memory_order_acquire),
                                false);
        goto out;
    }
    if (atomic_load_explicit(&failure_errno, memory_order_acquire) != 0) {
        result = missing_result(
            fixture_failed ? fixture_reason
                           : "mremap_contention_operation_failed",
            fixture_failed
                ? fixture_errno
                : atomic_load_explicit(&failure_errno, memory_order_acquire),
            false);
        goto out;
    }
    if (verify_sparse_vma_fixture(fixture, &fixture_after_vmas,
                                  &fixture_reason, &fixture_errno) != 0) {
        result = missing_result(fixture_reason, fixture_errno, false);
        goto out;
    }

    for (index = 0; index < worker_count; ++index) {
        size_t other;

        if (workers[index].completed != config->iterations ||
            workers[index].start_cpu != workers[index].cpu ||
            workers[index].end_cpu != workers[index].cpu ||
            workers[index].start_ns >= workers[index].end_ns) {
            result = missing_result("mremap_contention_worker_invalid",
                                    EXDEV, false);
            goto out;
        }
        for (other = index + 1U; other < worker_count; ++other) {
            const uintptr_t ranges[] = {
                (uintptr_t)workers[index].slot_a,
                (uintptr_t)workers[index].slot_b,
            };
            const uintptr_t other_ranges[] = {
                (uintptr_t)workers[other].slot_a,
                (uintptr_t)workers[other].slot_b,
            };
            size_t left;
            size_t right;

            for (left = 0; left < 2U; ++left) {
                for (right = 0; right < 2U; ++right) {
                    if (ranges_overlap(ranges[left], other_ranges[right],
                                       mapping_size)) {
                        result = missing_result(
                            "mremap_contention_slot_overlap", EIO, false);
                        goto out;
                    }
                }
            }
        }
    }
    if (workers[0].cpu == workers[1].cpu) {
        result = missing_result("mremap_contention_duplicate_cpu", EXDEV,
                                false);
        goto out;
    }
    {
        const uint64_t latest_start =
            workers[0].start_ns > workers[1].start_ns
                ? workers[0].start_ns
                : workers[1].start_ns;
        const uint64_t earliest_end =
            workers[0].end_ns < workers[1].end_ns ? workers[0].end_ns
                                                  : workers[1].end_ns;

        if (latest_start >= earliest_end) {
            result = missing_result("mremap_contention_no_window_overlap",
                                    EAGAIN, false);
            goto out;
        }
    }
    for (index = 0; index < worker_count; ++index) {
        uint64_t worker_p99;

        qsort(workers[index].samples, config->iterations,
              sizeof(*workers[index].samples), compare_u64);
        worker_p99 =
            nearest_rank(workers[index].samples, config->iterations, 990);
        if (worker_p99 == 0) {
            result = missing_result("mremap_contention_zero_latency", EIO,
                                    false);
            goto out;
        }
        printf("MM_PERF_MREMAP_WORKER status=ok worker=%zu cpu=%d"
               " completed=%zu slot_a=%" PRIuPTR " slot_b=%" PRIuPTR
               " bytes=%zu start_ns=%" PRIu64 " end_ns=%" PRIu64
               " p99_ns=%" PRIu64
               " fixture_before_vmas=%zu fixture_after_vmas=%zu\n",
               index, workers[index].cpu, workers[index].completed,
               (uintptr_t)workers[index].slot_a,
               (uintptr_t)workers[index].slot_b, mapping_size,
               workers[index].start_ns, workers[index].end_ns, worker_p99,
               fixture_before_vmas, fixture_after_vmas);
    }
    result = successful_result(samples, sample_count, false, 0);

out:
    if (vma_fixture != NULL) {
        record_cleanup_error(
            cleanup_mappings(vma_fixture, live_count, page_size),
            &cleanup_error);
    }
    if (gate_condition_initialized) {
        record_cleanup_error(pthread_cond_destroy(&gate.condition),
                             &cleanup_error);
    }
    if (gate_mutex_initialized) {
        record_cleanup_error(pthread_mutex_destroy(&gate.mutex),
                             &cleanup_error);
    }
    if (reservation_mapped) {
        if (munmap(reservation, reservation_span) != 0) {
            record_cleanup_error(errno, &cleanup_error);
        }
    } else if (reservation_base != 0) {
        for (index = 0; index < slot_count; ++index) {
            if (slot_mapped[index] &&
                munmap((void *)(reservation_base + index * slot_stride),
                       mapping_size) != 0) {
                record_cleanup_error(errno, &cleanup_error);
            }
        }
    }
    if (result.ok && cleanup_error != 0) {
        result = missing_result("mremap_contention_cleanup_failed",
                                cleanup_error, false);
    }
    free(vma_fixture);
    free(samples);
    return result;
}
#else
static struct metric_result run_mremap_disjoint_same_as_contention(
    const struct config *config, const int *worker_cpus,
    size_t available_cpus, size_t page_size,
    struct vma_fixture_report *fixture)
{
    (void)worker_cpus;
    (void)available_cpus;
    (void)page_size;
    init_vma_fixture_report(fixture, config->live_vmas);
    return missing_result("mremap_syscall_unavailable", ENOSYS, false);
}
#endif

/*
 * The pthread contention case above deliberately keeps one AddrSpace.  This
 * process case uses forked children and qualifies their private COW state
 * before collecting samples.  The parent owns only setup and result
 * collection; the timed samples are written after a second start barrier.
 */
struct cross_as_child_result {
    pid_t pid;
    size_t completed;
    int start_cpu;
    int end_cpu;
    int failure_errno;
    size_t fixture_before_vmas;
    size_t fixture_after_vmas;
    int cow_isolated;
    uint64_t completion_ns;
};

struct cross_as_shared {
    atomic_int failure_errno;
    size_t worker_count;
    size_t iterations;
    uint64_t start_ns;
    struct cross_as_child_result children[];
};

static struct cross_as_child_result *cross_as_child(
    struct cross_as_shared *shared, size_t index)
{
    return &shared->children[index];
}

static uint64_t *cross_as_samples(struct cross_as_shared *shared, size_t index)
{
    return (uint64_t *)(shared->children + shared->worker_count) +
           index * shared->iterations;
}

static void cross_as_record_failure(struct cross_as_shared *shared,
                                    int error_number)
{
    int expected = 0;

    if (error_number == 0) {
        error_number = EIO;
    }
    (void)atomic_compare_exchange_strong_explicit(
        &shared->failure_errno, &expected, error_number, memory_order_release,
        memory_order_relaxed);
}

static uint64_t cross_as_cow_child_sentinel(size_t index)
{
    return UINT64_C(0x54484b4348494c44) ^ (uint64_t)(index + 1U);
}

static int cross_as_write_byte(int fd, unsigned char value)
{
    for (;;) {
        ssize_t written = write(fd, &value, sizeof(value));

        if (written == (ssize_t)sizeof(value)) {
            return 0;
        }
        if (written < 0 && errno == EINTR) {
            continue;
        }
        if (written >= 0) {
            errno = EIO;
        }
        return -1;
    }
}

static int cross_as_read_byte(int fd, unsigned char *value)
{
    for (;;) {
        ssize_t received = read(fd, value, sizeof(*value));

        if (received == (ssize_t)sizeof(*value)) {
            return 0;
        }
        if (received < 0 && errno == EINTR) {
            continue;
        }
        if (received == 0) {
            errno = EPIPE;
        } else if (received > 0) {
            errno = EIO;
        }
        return -1;
    }
}

static int cross_as_child_main(
    struct cross_as_shared *shared, size_t index, int ready_fd,
    int qualification_fd, int start_fd, int cpu, size_t iterations,
    const struct vma_fixture_report *fixture_template,
    volatile uint64_t *cow_probe)
{
    struct cross_as_child_result *child = cross_as_child(shared, index);
    struct vma_fixture_report fixture = *fixture_template;
    const uint64_t cow_sentinel = cross_as_cow_child_sentinel(index);
    const char *fixture_reason = "fixture_setup_failed";
    char path[128];
    unsigned char command = 0;
    unsigned char *buffer = NULL;
    cpu_set_t affinity;
    int fd = -1;
    int allocation_error;
    int child_errno = 0;
    int cleanup_errno = 0;
    int fixture_errno = 0;
    bool path_linked = false;
    size_t iteration;

    child->start_cpu = -1;
    child->end_cpu = -1;
    child->pid = getpid();
    child->failure_errno = 0;
    child->completed = 0;
    child->fixture_before_vmas = 0;
    child->fixture_after_vmas = 0;
    child->cow_isolated = 0;
    child->completion_ns = 0;

    CPU_ZERO(&affinity);
    CPU_SET(cpu, &affinity);
    if (sched_setaffinity(0, sizeof(affinity), &affinity) != 0) {
        child_errno = errno;
        goto ready;
    }
    if (sched_getcpu() != cpu) {
        child_errno = EXDEV;
        goto ready;
    }
    if (snprintf(path, sizeof(path), "/tmp/thekernel-mm-cross-as-%ld-%zu",
                 (long)getpid(), index) >= (int)sizeof(path)) {
        child_errno = ENAMETOOLONG;
        goto ready;
    }
    fd = open(path, O_CREAT | O_EXCL | O_RDWR | O_DIRECT,
              S_IRUSR | S_IWUSR);
    if (fd < 0) {
        child_errno = errno;
        goto ready;
    }
    path_linked = true;
    if (unlink(path) != 0) {
        child_errno = errno;
        goto ready;
    }
    path_linked = false;
    allocation_error = posix_memalign((void **)&buffer, 4096U,
                                      PIN_BUFFER_BYTES);
    if (allocation_error != 0) {
        child_errno = allocation_error;
        goto ready;
    }
    memset(buffer, (int)(index & 0xffU), PIN_BUFFER_BYTES);
    if (ftruncate(fd, PIN_BUFFER_BYTES) != 0) {
        child_errno = errno;
        goto ready;
    }
    for (iteration = 0; iteration < PIN_WARMUP_ITERATIONS; ++iteration) {
        ssize_t written = pwrite(fd, buffer, PIN_BUFFER_BYTES, 0);

        if (written != (ssize_t)PIN_BUFFER_BYTES) {
            child_errno = written < 0 ? errno : EIO;
            goto ready;
        }
    }
    if (verify_sparse_vma_fixture(&fixture, &child->fixture_before_vmas,
                                  &fixture_reason, &fixture_errno) != 0) {
        child_errno = fixture_errno;
        goto ready;
    }
    if (*cow_probe != CROSS_AS_COW_PARENT_SENTINEL) {
        child_errno = EIO;
        goto ready;
    }
    *cow_probe = cow_sentinel;
    if (*cow_probe != cow_sentinel) {
        child_errno = EIO;
        goto ready;
    }

ready:
    if (child_errno != 0) {
        child->failure_errno = child_errno;
        cross_as_record_failure(shared, child_errno);
    }
    if (cross_as_write_byte(ready_fd, child_errno == 0 ? 1U : 0U) != 0) {
        child->failure_errno = errno;
        cross_as_record_failure(shared, errno);
        goto out;
    }
    if (cross_as_read_byte(qualification_fd, &command) != 0 || command != 1U ||
        atomic_load_explicit(&shared->failure_errno, memory_order_acquire) !=
            0) {
        if (child->failure_errno == 0) {
            child->failure_errno =
                command == 1U ? atomic_load_explicit(
                                    &shared->failure_errno,
                                    memory_order_acquire)
                               : ECANCELED;
        }
        goto out;
    }
    if (*cow_probe != cow_sentinel ||
        verify_sparse_vma_fixture(&fixture, NULL, &fixture_reason,
                                  &fixture_errno) != 0) {
        child->failure_errno = fixture_errno != 0 ? fixture_errno : EIO;
        cross_as_record_failure(shared, child->failure_errno);
        goto out;
    }
    child->cow_isolated = 1;
    if (cross_as_write_byte(ready_fd, 1U) != 0) {
        child->failure_errno = errno;
        cross_as_record_failure(shared, errno);
        goto out;
    }
    command = 0;
    if (cross_as_read_byte(start_fd, &command) != 0 || command != 1U ||
        atomic_load_explicit(&shared->failure_errno, memory_order_acquire) !=
            0) {
        if (child->failure_errno == 0) {
            child->failure_errno =
                command == 1U ? atomic_load_explicit(
                                    &shared->failure_errno,
                                    memory_order_acquire)
                               : ECANCELED;
        }
        goto out;
    }
    child->start_cpu = sched_getcpu();
    if (child->start_cpu != cpu) {
        child->failure_errno = EXDEV;
        cross_as_record_failure(shared, EXDEV);
        goto out;
    }
    for (iteration = 0; iteration < iterations; ++iteration) {
        uint64_t before;
        uint64_t after;
        ssize_t written;

        if (atomic_load_explicit(&shared->failure_errno,
                                 memory_order_acquire) != 0) {
            break;
        }
        if (monotonic_ns(&before) != 0) {
            child->failure_errno = errno;
            cross_as_record_failure(shared, errno);
            break;
        }
        written = pwrite(fd, buffer, PIN_BUFFER_BYTES, 0);
        child_errno = written < 0 ? errno : 0;
        if (monotonic_ns(&after) != 0) {
            child->failure_errno = errno;
            cross_as_record_failure(shared, errno);
            break;
        }
        if (written != (ssize_t)PIN_BUFFER_BYTES) {
            child->failure_errno = child_errno != 0 ? child_errno : EIO;
            cross_as_record_failure(shared, child->failure_errno);
            break;
        }
        cross_as_samples(shared, index)[iteration] = after - before;
        child->completed = iteration + 1U;
        child->completion_ns = after;
    }
    child->end_cpu = sched_getcpu();
    if (child->end_cpu != cpu) {
        child->failure_errno = EXDEV;
        cross_as_record_failure(shared, EXDEV);
    }
    if (child->failure_errno == 0 &&
        (verify_sparse_vma_fixture(&fixture, &child->fixture_after_vmas,
                                   &fixture_reason, &fixture_errno) != 0 ||
         *cow_probe != cow_sentinel)) {
        child->failure_errno = fixture_errno != 0 ? fixture_errno : EIO;
        cross_as_record_failure(shared, child->failure_errno);
    }

out:
    if (fd >= 0) {
        if (path_linked && unlink(path) != 0) {
            cleanup_errno = errno;
        }
        if (close(fd) != 0 && cleanup_errno == 0) {
            cleanup_errno = errno;
        }
    }
    if (child->failure_errno == 0 && cleanup_errno != 0) {
        child->failure_errno = cleanup_errno;
        cross_as_record_failure(shared, cleanup_errno);
    }
    free(buffer);
    return child->failure_errno == 0 && child->completed == iterations ? 0
                                                                         : 1;
}

static int cross_as_wait_ready(int fd, size_t worker_count, bool *all_ready)
{
    size_t received = 0;
    uint64_t deadline;

    *all_ready = true;
    if (monotonic_ns(&deadline) != 0) {
        return -1;
    }
    if (deadline > UINT64_MAX - (uint64_t)CROSS_AS_TIMEOUT_MS *
                                      UINT64_C(1000000)) {
        errno = EOVERFLOW;
        return -1;
    }
    deadline += (uint64_t)CROSS_AS_TIMEOUT_MS * UINT64_C(1000000);
    while (received < worker_count) {
        struct pollfd descriptor = {.fd = fd, .events = POLLIN};
        uint64_t now;
        uint64_t remaining_ns;
        int timeout_ms;
        int poll_result;

        if (monotonic_ns(&now) != 0) {
            return -1;
        }
        if (now >= deadline) {
            errno = ETIMEDOUT;
            return -1;
        }
        remaining_ns = deadline - now;
        timeout_ms = (int)((remaining_ns + UINT64_C(999999)) /
                           UINT64_C(1000000));
        if (timeout_ms > CROSS_AS_TIMEOUT_MS) {
            timeout_ms = CROSS_AS_TIMEOUT_MS;
        }
        poll_result = poll(&descriptor, 1, timeout_ms);
        if (poll_result < 0 && errno == EINTR) {
            continue;
        }
        if (poll_result <= 0) {
            if (poll_result == 0) {
                errno = ETIMEDOUT;
            }
            return -1;
        }
        while (received < worker_count) {
            unsigned char ready;

            if (read(fd, &ready, sizeof(ready)) != (ssize_t)sizeof(ready)) {
                if (errno == EINTR) {
                    continue;
                }
                return -1;
            }
            if (ready != 1U) {
                *all_ready = false;
            }
            received += 1U;
            if (received == worker_count) {
                break;
            }
            descriptor.revents = 0;
            poll_result = poll(&descriptor, 1, 0);
            if (poll_result <= 0 || (descriptor.revents & POLLIN) == 0) {
                break;
            }
        }
    }
    return 0;
}

static int cross_as_wait_child_bounded(pid_t child, int *status,
                                       bool *reaped, uint64_t deadline)
{
    *reaped = false;
    for (;;) {
        pid_t result = waitpid(child, status, WNOHANG);
        uint64_t now;

        if (result == child) {
            *reaped = true;
            return 0;
        }
        if (result < 0 && errno == ECHILD) {
            *reaped = true;
            return -1;
        }
        if (result < 0 && errno != EINTR) {
            return -1;
        }
        if (monotonic_ns(&now) != 0) {
            return -1;
        }
        if (now >= deadline) {
            errno = ETIMEDOUT;
            return -1;
        }
        {
            struct timespec pause = {.tv_sec = 0, .tv_nsec = 1000000};

            while (nanosleep(&pause, &pause) != 0 && errno == EINTR) {
            }
        }
    }
}

static int cross_as_abort_and_reap(pid_t *children, size_t child_count)
{
    uint64_t deadline;
    size_t index;

    for (index = 0; index < child_count; ++index) {
        if (children[index] > 0) {
            (void)kill(children[index], SIGKILL);
        }
    }
    if (monotonic_ns(&deadline) != 0) {
        return -1;
    }
    if (deadline > UINT64_MAX - (uint64_t)CROSS_AS_CLEANUP_TIMEOUT_MS *
                                    UINT64_C(1000000)) {
        errno = EOVERFLOW;
        return -1;
    }
    deadline +=
        (uint64_t)CROSS_AS_CLEANUP_TIMEOUT_MS * UINT64_C(1000000);
    for (;;) {
        size_t live = 0;
        uint64_t now;

        for (index = 0; index < child_count; ++index) {
            int status;
            pid_t waited;

            if (children[index] <= 0) {
                continue;
            }
            waited = waitpid(children[index], &status, WNOHANG);
            if (waited == children[index] ||
                (waited < 0 && errno == ECHILD)) {
                children[index] = 0;
                continue;
            }
            if (waited < 0 && errno != EINTR) {
                return -1;
            }
            live += 1U;
        }
        if (live == 0) {
            return 0;
        }
        if (monotonic_ns(&now) != 0) {
            return -1;
        }
        if (now >= deadline) {
            errno = ETIMEDOUT;
            return -1;
        }
        {
            struct timespec pause = {.tv_sec = 0, .tv_nsec = 1000000};

            while (nanosleep(&pause, &pause) != 0 && errno == EINTR) {
            }
        }
    }
}

static struct metric_result run_direct_io_pin_proxy_cross_as_contention(
    size_t worker_count, size_t iterations, const int *worker_cpus,
    size_t live_vmas, size_t page_size, struct vma_fixture_report *fixture)
{
    const size_t child_bytes = sizeof(struct cross_as_child_result);
    size_t sample_count;
    size_t sample_bytes;
    size_t storage_size;
    void **vma_fixture = NULL;
    struct cross_as_shared *shared = MAP_FAILED;
    volatile uint64_t *cow_probe = MAP_FAILED;
    uint64_t *samples = NULL;
    pid_t *children = NULL;
    int ready_pipe[2] = {-1, -1};
    int qualification_pipe[2] = {-1, -1};
    int start_pipe[2] = {-1, -1};
    struct sigaction old_pipe_action;
    struct metric_result result;
    const char *fixture_reason = "fixture_setup_failed";
    int fixture_errno = 0;
    size_t live_count = 0;
    size_t child_count = 0;
    size_t index;
    bool all_ready = false;
    bool pipe_action_installed = false;
    int child_failure = 0;
    int cleanup_error = 0;
    uint64_t end_ns = 0;
    uint64_t start_ns = 0;
    uint64_t child_deadline = 0;

    init_vma_fixture_report(fixture, live_vmas);
    if (worker_count < 2U) {
        return missing_result("insufficient_online_cpus", 0, true);
    }
    if (iterations == 0 || worker_count > SIZE_MAX / iterations ||
        worker_count > (SIZE_MAX - sizeof(*shared)) / child_bytes) {
        return missing_result("shared_storage_size_overflow", EOVERFLOW, true);
    }
    sample_count = worker_count * iterations;
    if (sample_count > SIZE_MAX / sizeof(uint64_t)) {
        return missing_result("shared_storage_size_overflow", EOVERFLOW, true);
    }
    sample_bytes = sample_count * sizeof(uint64_t);
    if (sample_bytes > SIZE_MAX - sizeof(*shared) - worker_count * child_bytes) {
        return missing_result("shared_storage_size_overflow", EOVERFLOW, true);
    }
    storage_size = sizeof(*shared) + worker_count * child_bytes + sample_bytes;
    vma_fixture = calloc(live_vmas, sizeof(*vma_fixture));
    children = calloc(worker_count, sizeof(*children));
    samples = calloc(sample_count, sizeof(*samples));
    if (vma_fixture == NULL || children == NULL || samples == NULL) {
        result = missing_result("allocation_failed", ENOMEM, true);
        goto out;
    }
    {
        struct sigaction ignore_pipe;

        memset(&ignore_pipe, 0, sizeof(ignore_pipe));
        ignore_pipe.sa_handler = SIG_IGN;
        if (sigemptyset(&ignore_pipe.sa_mask) != 0 ||
            sigaction(SIGPIPE, &ignore_pipe, &old_pipe_action) != 0) {
            result = missing_result("cross_as_sigpipe_guard_failed", errno,
                                    true);
            goto out;
        }
        pipe_action_installed = true;
    }
    shared = mmap(NULL, storage_size, PROT_READ | PROT_WRITE,
                  MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        result = missing_result("cross_as_shared_mmap_failed", errno, true);
        shared = MAP_FAILED;
        goto out;
    }
    memset(shared, 0, storage_size);
    atomic_init(&shared->failure_errno, 0);
    if (!atomic_is_lock_free(&shared->failure_errno)) {
        result = missing_result("cross_as_shared_atomic_unavailable", ENOTSUP,
                                true);
        goto out;
    }
    shared->worker_count = worker_count;
    shared->iterations = iterations;
    for (index = 0; index < worker_count; ++index) {
        cross_as_child(shared, index)->start_cpu = -1;
        cross_as_child(shared, index)->end_cpu = -1;
        cross_as_child(shared, index)->pid = -1;
    }
    cow_probe = mmap(NULL, page_size, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (cow_probe == MAP_FAILED) {
        result = missing_result("cross_as_cow_probe_mmap_failed", errno, true);
        cow_probe = MAP_FAILED;
        goto out;
    }
    *cow_probe = CROSS_AS_COW_PARENT_SENTINEL;
    if (pipe(ready_pipe) != 0 || pipe(qualification_pipe) != 0 ||
        pipe(start_pipe) != 0) {
        result = missing_result("cross_as_pipe_failed", errno, true);
        goto out;
    }
    if (setup_sparse_vma_fixture(vma_fixture, live_vmas, page_size,
                                 &live_count, fixture, &fixture_reason,
                                 &fixture_errno) != 0) {
        result = missing_result(fixture_reason, fixture_errno, true);
        goto out;
    }
    if (verify_sparse_vma_fixture(fixture, NULL, &fixture_reason,
                                  &fixture_errno) != 0) {
        result = missing_result(fixture_reason, fixture_errno, true);
        goto out;
    }
    for (index = 0; index < worker_count; ++index) {
        pid_t child = fork();

        if (child < 0) {
            result = missing_result("cross_as_fork_failed", errno, true);
            goto abort_children;
        }
        if (child == 0) {
            int child_result;

            (void)close(ready_pipe[0]);
            (void)close(qualification_pipe[1]);
            (void)close(start_pipe[1]);
            child_result = cross_as_child_main(
                shared, index, ready_pipe[1], qualification_pipe[0],
                start_pipe[0], worker_cpus[index], iterations, fixture,
                cow_probe);
            (void)close(ready_pipe[1]);
            (void)close(qualification_pipe[0]);
            (void)close(start_pipe[0]);
            _exit(child_result == 0 ? 0 : 1);
        }
        children[child_count++] = child;
    }
    (void)close(ready_pipe[1]);
    ready_pipe[1] = -1;
    (void)close(qualification_pipe[0]);
    qualification_pipe[0] = -1;
    (void)close(start_pipe[0]);
    start_pipe[0] = -1;
    if (cross_as_wait_ready(ready_pipe[0], worker_count, &all_ready) != 0) {
        result = missing_result("cross_as_ready_timeout", errno, true);
        goto abort_children;
    }
    if (!all_ready ||
        atomic_load_explicit(&shared->failure_errno, memory_order_acquire) !=
            0) {
        child_failure = atomic_load_explicit(&shared->failure_errno,
                                             memory_order_acquire);
        result = missing_result("cross_as_child_setup_failed",
                                child_failure != 0 ? child_failure : EIO, true);
        goto abort_children;
    }
    if (*cow_probe != CROSS_AS_COW_PARENT_SENTINEL) {
        result = missing_result("cross_as_cow_parent_changed", EIO, true);
        goto abort_children;
    }
    for (index = 0; index < worker_count; ++index) {
        if (cross_as_write_byte(qualification_pipe[1], 1U) != 0) {
            result = missing_result("cross_as_qualification_pipe_failed", errno,
                                    true);
            goto abort_children;
        }
    }
    all_ready = false;
    if (cross_as_wait_ready(ready_pipe[0], worker_count, &all_ready) != 0) {
        result = missing_result("cross_as_qualification_timeout", errno, true);
        goto abort_children;
    }
    if (!all_ready ||
        atomic_load_explicit(&shared->failure_errno, memory_order_acquire) !=
            0 ||
        *cow_probe != CROSS_AS_COW_PARENT_SENTINEL) {
        child_failure = atomic_load_explicit(&shared->failure_errno,
                                             memory_order_acquire);
        result = missing_result("cross_as_qualification_failed",
                                child_failure != 0 ? child_failure : EIO, true);
        goto abort_children;
    }
    if (monotonic_ns(&start_ns) != 0) {
        result = missing_result("clock_failed", errno, true);
        goto abort_children;
    }
    shared->start_ns = start_ns;
    if (start_ns > UINT64_MAX - (uint64_t)CROSS_AS_TIMEOUT_MS *
                                    UINT64_C(1000000)) {
        result = missing_result("clock_overflow", EOVERFLOW, true);
        goto abort_children;
    }
    child_deadline = start_ns +
                     (uint64_t)CROSS_AS_TIMEOUT_MS * UINT64_C(1000000);
    for (index = 0; index < worker_count; ++index) {
        if (cross_as_write_byte(start_pipe[1], 1U) != 0) {
            result = missing_result("cross_as_start_pipe_failed", errno, true);
            goto abort_children;
        }
    }
    for (index = 0; index < child_count; ++index) {
        int status;
        bool reaped;
        const pid_t expected_pid = children[index];

        if (cross_as_wait_child_bounded(expected_pid, &status, &reaped,
                                        child_deadline) != 0) {
            if (reaped) {
                children[index] = 0;
            }
            result = missing_result("cross_as_waitpid_failed", errno, true);
            goto abort_children;
        }
        children[index] = 0;
        if (cross_as_child(shared, index)->pid != expected_pid) {
            result = missing_result("cross_as_worker_pid_mismatch", EIO,
                                    true);
            goto abort_children;
        }
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
            child_failure = cross_as_child(shared, index)->failure_errno;
            result = missing_result("cross_as_child_failed",
                                    child_failure != 0 ? child_failure : EIO,
                                    true);
            goto abort_children;
        }
    }
    if (atomic_load_explicit(&shared->failure_errno, memory_order_acquire) !=
            0 ||
        start_ns == 0) {
        child_failure = atomic_load_explicit(&shared->failure_errno,
                                             memory_order_acquire);
        result = missing_result("cross_as_operation_failed",
                                child_failure != 0 ? child_failure : EIO, true);
        goto out;
    }
    if (*cow_probe != CROSS_AS_COW_PARENT_SENTINEL) {
        result = missing_result("cross_as_cow_parent_changed", EIO, true);
        goto out;
    }
    if (verify_sparse_vma_fixture(fixture, NULL, &fixture_reason,
                                  &fixture_errno) != 0) {
        result = missing_result(fixture_reason, fixture_errno, true);
        goto out;
    }
    for (index = 0; index < worker_count; ++index) {
        struct cross_as_child_result *child = cross_as_child(shared, index);
        size_t other;
        size_t sample_index;

        if (child->pid <= 0 || child->pid == getpid()) {
            result = missing_result("cross_as_worker_pid_invalid", EIO, true);
            goto out;
        }
        for (other = 0; other < index; ++other) {
            if (cross_as_child(shared, other)->pid == child->pid) {
                result = missing_result("cross_as_worker_pid_duplicate", EIO,
                                        true);
                goto out;
            }
        }
        if (child->completed != iterations ||
            child->start_cpu != worker_cpus[index] ||
            child->end_cpu != worker_cpus[index]) {
            result = missing_result("cross_as_worker_placement_failed", EXDEV,
                                    true);
            goto out;
        }
        if (child->fixture_before_vmas != live_vmas ||
            child->fixture_after_vmas != live_vmas ||
            child->cow_isolated != 1) {
            result = missing_result("cross_as_worker_witness_failed", EIO,
                                    true);
            goto out;
        }
        if (child->completion_ns > end_ns) {
            end_ns = child->completion_ns;
        }
        memcpy(samples + index * iterations, cross_as_samples(shared, index),
               iterations * sizeof(uint64_t));
        qsort(samples + index * iterations, iterations, sizeof(uint64_t),
              compare_u64);
        printf("MM_PERF_PIN_CROSS_AS_WORKER status=ok worker=%zu pid=%ld cpu=%d"
               " completed=%zu p99_ns=%" PRIu64
               " fixture_before_vmas=%zu fixture_after_vmas=%zu"
               " cow_isolated=1\n",
               index, (long)child->pid, worker_cpus[index], child->completed,
               nearest_rank(samples + index * iterations, iterations, 990),
               child->fixture_before_vmas, child->fixture_after_vmas);
        for (sample_index = 0; sample_index < iterations; ++sample_index) {
            if (samples[index * iterations + sample_index] == 0) {
                result = missing_result("cross_as_zero_sample", EIO, true);
                goto out;
            }
        }
    }
    if (end_ns <= start_ns) {
        result = missing_result("zero_elapsed_time", 0, true);
        goto out;
    }
    {
        const uint64_t elapsed_ns = end_ns - start_ns;
        const uint64_t bytes = (uint64_t)sample_count * PIN_BUFFER_BYTES;
        const uint64_t throughput = (uint64_t)(
            ((long double)bytes * 1000000000.0L) / (long double)elapsed_ns);

        result = successful_result(samples, sample_count, true, throughput);
    }
    goto out;

abort_children:
    if (cross_as_abort_and_reap(children, child_count) != 0) {
        result = missing_result("cross_as_cleanup_failed", errno, true);
    }

out:
    if (ready_pipe[0] >= 0) {
        if (close(ready_pipe[0]) != 0) {
            record_cleanup_error(errno, &cleanup_error);
        }
    }
    if (ready_pipe[1] >= 0) {
        if (close(ready_pipe[1]) != 0) {
            record_cleanup_error(errno, &cleanup_error);
        }
    }
    if (qualification_pipe[0] >= 0) {
        if (close(qualification_pipe[0]) != 0) {
            record_cleanup_error(errno, &cleanup_error);
        }
    }
    if (qualification_pipe[1] >= 0) {
        if (close(qualification_pipe[1]) != 0) {
            record_cleanup_error(errno, &cleanup_error);
        }
    }
    if (start_pipe[0] >= 0) {
        if (close(start_pipe[0]) != 0) {
            record_cleanup_error(errno, &cleanup_error);
        }
    }
    if (start_pipe[1] >= 0) {
        if (close(start_pipe[1]) != 0) {
            record_cleanup_error(errno, &cleanup_error);
        }
    }
    if (pipe_action_installed) {
        if (sigaction(SIGPIPE, &old_pipe_action, NULL) != 0) {
            record_cleanup_error(errno, &cleanup_error);
        }
    }
    if (shared != MAP_FAILED) {
        if (munmap(shared, storage_size) != 0) {
            record_cleanup_error(errno, &cleanup_error);
        }
    }
    if (cow_probe != MAP_FAILED) {
        if (munmap((void *)cow_probe, page_size) != 0) {
            record_cleanup_error(errno, &cleanup_error);
        }
    }
    if (vma_fixture != NULL) {
        record_cleanup_error(
            cleanup_mappings(vma_fixture, live_count, page_size),
            &cleanup_error);
    }
    if (result.ok && cleanup_error != 0) {
        result = missing_result("cross_as_cleanup_failed", cleanup_error, true);
    }
    free(vma_fixture);
    free(children);
    free(samples);
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
    size_t affinity_index;
    struct metric_result result;
    struct vma_fixture_report vma_scale_fixture;
    struct vma_fixture_report fixed_remap_fixture;
    struct vma_fixture_report mremap_contention_fixture;
    struct vma_fixture_report direct_io_pin_proxy_throughput_fixture;
    struct vma_fixture_report direct_io_pin_proxy_same_as_contention_fixture;
    struct vma_fixture_report direct_io_pin_proxy_cross_as_contention_fixture;
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
        printf("MM_PERF_AFFINITY status=ok bytes=%zu allowed_cpus=%zu"
               " cpu_ids=",
               affinity_bytes, affinity_cpus);
        for (affinity_index = 0;
             affinity_index < affinity_cpus &&
             affinity_index < MAX_PIN_WORKERS;
             ++affinity_index) {
            printf("%s%d", affinity_index == 0 ? "" : ",",
                   affinity_cpu_ids[affinity_index]);
        }
        printf(" cpu_ids_complete=%d\n",
               affinity_cpus <= MAX_PIN_WORKERS ? 1 : 0);
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

        init_vma_fixture_report(&vma_scale_fixture, config.live_vmas);
        init_vma_fixture_report(&fixed_remap_fixture, config.live_vmas);
        init_vma_fixture_report(&mremap_contention_fixture,
                                config.live_vmas);
        init_vma_fixture_report(&direct_io_pin_proxy_throughput_fixture, config.live_vmas);
        init_vma_fixture_report(&direct_io_pin_proxy_same_as_contention_fixture, config.live_vmas);
        init_vma_fixture_report(&direct_io_pin_proxy_cross_as_contention_fixture,
                                config.live_vmas);
        result = missing_result("page_size_unavailable", saved_errno, false);
        emit_metric_with_fixture("vma_scale", &result, &vma_scale_fixture);
        emit_metric("mremap_latency", &result);
        emit_metric_with_fixture("mremap_fixed_replace_latency", &result,
                                 &fixed_remap_fixture);
        emit_metric_with_fixture("mremap_disjoint_same_as_contention", &result,
                                 &mremap_contention_fixture);
        emit_metric("mremap_file_duplicate_latency", &result);
        emit_metric("mremap_shared_anon_resize_latency", &result);
        emit_metric("protect_touch_latency", &result);
        result = missing_result("page_size_unavailable", saved_errno, true);
        emit_metric_with_fixture("direct_io_pin_proxy_throughput", &result,
                                 &direct_io_pin_proxy_throughput_fixture);
        emit_metric_with_fixture("direct_io_pin_proxy_same_as_contention", &result,
                                 &direct_io_pin_proxy_same_as_contention_fixture);
        emit_metric_with_fixture("direct_io_pin_proxy_cross_as_contention", &result,
                                 &direct_io_pin_proxy_cross_as_contention_fixture);
        puts("MM_PERF_DONE status=ok");
        return 0;
    }
    page_size = (size_t)system_page_size;
    printf("MM_PERF_RUN schema=%s arch=%s iterations=%zu vmas=%zu"
           " pin_iterations=%zu pin_workers=%zu page_size=%zu\n",
           MM_PERF_RUN_SCHEMA, MM_PERF_COMPILED_ARCH, config.iterations,
           config.live_vmas, config.pin_iterations, config.pin_workers,
           page_size);

    if (verify_mremap_semantics(page_size, &failed_semantic_test,
                                &semantic_errno) != 0) {
        printf("MM_PERF_SEMANTICS status=fail test=%s errno=%d\n",
               failed_semantic_test, semantic_errno);
        return 1;
    }
    puts("MM_PERF_SEMANTICS status=ok");

    result = run_vma_scale(&config, page_size, &vma_scale_fixture);
    emit_metric_with_fixture("vma_scale", &result, &vma_scale_fixture);
    result = run_mremap_latency(&config, page_size);
    emit_metric("mremap_latency", &result);
    if (!result.ok && result.reason != NULL &&
        strcmp(result.reason, "mremap_content_mismatch") == 0) {
        return 1;
    }
    result = run_mremap_fixed_replace_latency(&config, page_size,
                                              &fixed_remap_fixture);
    emit_metric_with_fixture("mremap_fixed_replace_latency", &result,
                             &fixed_remap_fixture);
    if (!result.ok && result.reason != NULL &&
        (strcmp(result.reason, "mremap_fixed_target_mismatch") == 0 ||
         strcmp(result.reason, "mremap_fixed_content_mismatch") == 0)) {
        return 1;
    }
    result = run_mremap_disjoint_same_as_contention(
        &config, affinity_cpu_ids, affinity_cpus, page_size,
        &mremap_contention_fixture);
    emit_metric_with_fixture("mremap_disjoint_same_as_contention", &result,
                             &mremap_contention_fixture);
    result = run_mremap_file_duplicate_latency(&config, page_size);
    emit_metric("mremap_file_duplicate_latency", &result);
    if (!result.ok && result.reason != NULL &&
        (strcmp(result.reason, "mremap_file_duplicate_content_mismatch") == 0 ||
         strcmp(result.reason, "mremap_file_duplicate_alias_mismatch") == 0 ||
         strcmp(result.reason, "mremap_file_alias_to_original_mismatch") == 0 ||
         strcmp(result.reason, "mremap_file_original_to_alias_mismatch") == 0)) {
        return 1;
    }
    result = run_mremap_shared_anon_resize_latency(&config, page_size);
    emit_metric("mremap_shared_anon_resize_latency", &result);
    if (!result.ok && result.reason != NULL &&
        (strcmp(result.reason, "mremap_shared_anon_content_mismatch") == 0 ||
         strcmp(result.reason, "mremap_shared_anon_restore_failed") == 0)) {
        return 1;
    }
    result = run_protect_touch(&config, page_size);
    emit_metric("protect_touch_latency", &result);
    result = run_pin_metric(1U, config.pin_iterations, false,
                            affinity_cpu_ids, config.live_vmas, page_size,
                            &direct_io_pin_proxy_throughput_fixture);
    emit_metric_with_fixture("direct_io_pin_proxy_throughput", &result,
                             &direct_io_pin_proxy_throughput_fixture);
    result = run_pin_metric(config.pin_workers, config.pin_iterations, true,
                            affinity_cpu_ids, config.live_vmas, page_size,
                            &direct_io_pin_proxy_same_as_contention_fixture);
    emit_metric_with_fixture("direct_io_pin_proxy_same_as_contention", &result,
                             &direct_io_pin_proxy_same_as_contention_fixture);
    result = run_direct_io_pin_proxy_cross_as_contention(
        config.pin_workers, config.pin_iterations, affinity_cpu_ids,
        config.live_vmas, page_size, &direct_io_pin_proxy_cross_as_contention_fixture);
    emit_metric_with_fixture("direct_io_pin_proxy_cross_as_contention", &result,
                             &direct_io_pin_proxy_cross_as_contention_fixture);
    puts("MM_PERF_DONE status=ok");
    return 0;
}
