#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#define PAGE_BYTES 4096U
#define CACHE_PAGES 512U
#define ALLOCATION_CHUNK_PAGES 256U
#define MAX_ALLOCATION_CHUNKS 1024U
#define POLL_ATTEMPTS 80U
#define PRESSURE_PATH "/proc/memory_pressure"
#define CACHE_PATH "/var/tmp/thekernel-mm-pressure-cache"

struct pressure_snapshot {
    uint64_t total_pages;
    uint64_t free_pages;
    uint64_t low_watermark_pages;
    uint64_t checks;
    uint64_t pressure_events;
    uint64_t reclaimed_pages;
    uint64_t reclaimable_clean_file_pages;
};

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_MM_PRESSURE_FAIL %s errno=%d (%s)\n",
            stage, errno, strerror(errno));
    return 1;
}

static int parse_field(const char *buffer, const char *name, uint64_t *value) {
    char key[80];
    int key_length = snprintf(key, sizeof(key), "%s=", name);
    if (key_length <= 0 || (size_t)key_length >= sizeof(key)) {
        errno = EOVERFLOW;
        return -1;
    }
    const char *start = strstr(buffer, key);
    if (start == NULL || (start != buffer && start[-1] != '\n')) {
        errno = EPROTO;
        return -1;
    }
    start += (size_t)key_length;
    errno = 0;
    char *end = NULL;
    unsigned long long parsed = strtoull(start, &end, 10);
    if (errno != 0 || end == start || (*end != '\n' && *end != '\0')) {
        errno = EPROTO;
        return -1;
    }
    *value = (uint64_t)parsed;
    return 0;
}

static int read_pressure_snapshot(struct pressure_snapshot *snapshot) {
    char buffer[4096] = {0};
    int fd = open(PRESSURE_PATH, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    ssize_t count = read(fd, buffer, sizeof(buffer) - 1);
    int saved_errno = errno;
    if (close(fd) != 0 && count >= 0) {
        return -1;
    }
    errno = saved_errno;
    if (count <= 0 || (size_t)count == sizeof(buffer) - 1) {
        errno = EOVERFLOW;
        return -1;
    }
    if (strstr(buffer, "schema=thekernel-mm-pressure-v1\n") == NULL ||
        parse_field(buffer, "total_pages", &snapshot->total_pages) != 0 ||
        parse_field(buffer, "free_pages", &snapshot->free_pages) != 0 ||
        parse_field(buffer, "low_watermark_pages",
                    &snapshot->low_watermark_pages) != 0 ||
        parse_field(buffer, "checks", &snapshot->checks) != 0 ||
        parse_field(buffer, "pressure_events", &snapshot->pressure_events) != 0 ||
        parse_field(buffer, "reclaimed_pages", &snapshot->reclaimed_pages) != 0 ||
        parse_field(buffer, "reclaimable_clean_file_pages",
                    &snapshot->reclaimable_clean_file_pages) != 0) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

static int create_clean_cache(void) {
    unsigned char page[PAGE_BYTES];
    memset(page, 0x5a, sizeof(page));
    int fd = open(CACHE_PATH, O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0600);
    if (fd < 0) {
        return -1;
    }
    for (size_t page_index = 0; page_index < CACHE_PAGES; ++page_index) {
        page[0] = (unsigned char)page_index;
        if (write(fd, page, sizeof(page)) != (ssize_t)sizeof(page)) {
            int saved_errno = errno;
            close(fd);
            errno = saved_errno != 0 ? saved_errno : EIO;
            return -1;
        }
    }
    if (fsync(fd) != 0 || close(fd) != 0) {
        return -1;
    }
    return 0;
}

static int verify_reclaimed_file(void) {
    unsigned char page[PAGE_BYTES];
    int fd = open(CACHE_PATH, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    ssize_t count = read(fd, page, sizeof(page));
    int saved_errno = errno;
    if (close(fd) != 0 && count >= 0) {
        return -1;
    }
    errno = saved_errno;
    if (count != (ssize_t)sizeof(page) || page[0] != 0 || page[1] != 0x5a) {
        errno = EIO;
        return -1;
    }
    return 0;
}

static void release_allocations(void **allocations, size_t count) {
    for (size_t index = 0; index < count; ++index) {
        if (allocations[index] != NULL) {
            munmap(allocations[index], ALLOCATION_CHUNK_PAGES * PAGE_BYTES);
        }
    }
}

int main(void) {
    struct pressure_snapshot worker_before;
    struct pressure_snapshot worker_after;
    if (read_pressure_snapshot(&worker_before) != 0) {
        return fail("worker-snapshot-before");
    }
    if (usleep(1500000) != 0 || read_pressure_snapshot(&worker_after) != 0) {
        return fail("worker-snapshot-after");
    }
    if (worker_after.checks <= worker_before.checks) {
        errno = ETIMEDOUT;
        return fail("worker-not-running");
    }
    puts("THEKERNEL_MM_PRESSURE_WORKER_OK");

    if (create_clean_cache() != 0) {
        return fail("create-clean-cache");
    }

    struct pressure_snapshot before;
    if (read_pressure_snapshot(&before) != 0) {
        unlink(CACHE_PATH);
        return fail("reclaim-snapshot-before");
    }
    if (before.reclaimable_clean_file_pages == 0 ||
        before.free_pages <= before.low_watermark_pages) {
        unlink(CACHE_PATH);
        errno = ENODATA;
        return fail("reclaim-precondition");
    }

    uint64_t max_pages = before.free_pages - before.low_watermark_pages;
    max_pages += before.reclaimable_clean_file_pages + 32U;
    if (max_pages > before.total_pages) {
        max_pages = before.total_pages;
    }

    void *allocations[MAX_ALLOCATION_CHUNKS] = {0};
    size_t allocation_count = 0;
    uint64_t allocated_pages = 0;
    int reclaimed = 0;
    struct pressure_snapshot observed = before;
    while (allocation_count < MAX_ALLOCATION_CHUNKS &&
           allocated_pages < max_pages) {
        void *mapping = mmap(NULL, ALLOCATION_CHUNK_PAGES * PAGE_BYTES,
                             PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (mapping == MAP_FAILED) {
            break;
        }
        allocations[allocation_count++] = mapping;
        volatile unsigned char *bytes = mapping;
        for (size_t page = 0; page < ALLOCATION_CHUNK_PAGES; ++page) {
            bytes[page * PAGE_BYTES] = (unsigned char)page;
        }
        allocated_pages += ALLOCATION_CHUNK_PAGES;

        if (read_pressure_snapshot(&observed) != 0) {
            release_allocations(allocations, allocation_count);
            unlink(CACHE_PATH);
            return fail("reclaim-snapshot-drive");
        }
        if (observed.pressure_events > before.pressure_events &&
            observed.reclaimed_pages > before.reclaimed_pages) {
            reclaimed = 1;
            break;
        }
        if (observed.free_pages <= observed.low_watermark_pages) {
            break;
        }
        if (usleep(25000) != 0) {
            release_allocations(allocations, allocation_count);
            unlink(CACHE_PATH);
            return fail("reclaim-drive-sleep");
        }
    }

    for (size_t attempt = 0; !reclaimed && attempt < POLL_ATTEMPTS; ++attempt) {
        if (usleep(100000) != 0 || read_pressure_snapshot(&observed) != 0) {
            release_allocations(allocations, allocation_count);
            unlink(CACHE_PATH);
            return fail("reclaim-poll");
        }
        reclaimed = observed.pressure_events > before.pressure_events &&
                    observed.reclaimed_pages > before.reclaimed_pages;
    }

    int verify_result = reclaimed ? verify_reclaimed_file() : -1;
    if (!reclaimed) {
        fprintf(stderr,
                "THEKERNEL_MM_PRESSURE_DIAG before_total=%" PRIu64
                " before_free=%" PRIu64 " before_low=%" PRIu64
                " before_reclaimable=%" PRIu64 " allocated_pages=%" PRIu64
                " observed_free=%" PRIu64 " observed_low=%" PRIu64
                " events_before=%" PRIu64 " events_after=%" PRIu64
                " reclaimed_before=%" PRIu64 " reclaimed_after=%" PRIu64 "\n",
                before.total_pages, before.free_pages,
                before.low_watermark_pages,
                before.reclaimable_clean_file_pages, allocated_pages,
                observed.free_pages, observed.low_watermark_pages,
                before.pressure_events, observed.pressure_events,
                before.reclaimed_pages, observed.reclaimed_pages);
        errno = ETIMEDOUT;
    }
    release_allocations(allocations, allocation_count);
    if (unlink(CACHE_PATH) != 0 && verify_result == 0) {
        verify_result = -1;
    }
    if (verify_result != 0) {
        return fail(reclaimed ? "reclaimed-file-contents" : "reclaim-timeout");
    }

    puts("THEKERNEL_MM_PRESSURE_RECLAIM_OK");
    puts("THEKERNEL_MM_PRESSURE_OK");
    return 0;
}
