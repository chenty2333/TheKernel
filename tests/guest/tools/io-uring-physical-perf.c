#define _GNU_SOURCE

/*
 * Registered-buffer O_DIRECT/io_uring performance cells.
 *
 * The helper uses the raw io_uring ABI so the same source builds with the
 * repository's static musl toolchain and with host Linux.  A cell first
 * proves all CQEs (user_data, missing/duplicate accounting, and digest), and
 * only then records a bounded latency window.  On a test-io-control
 * TheKernel guest, the physical DMA counters are the path oracle: queue
 * depth is never inferred from a generic virtio async toggle.  A Linux guest
 * remains runnable, but deliberately reports only its Linux io_uring path;
 * it must not manufacture TheKernel counter evidence.
 */

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/fiemap.h>
#include <linux/fs.h>
#include <poll.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/sysmacros.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error "io_uring physical performance helper requires the x86_64 Linux ABI"
#endif

#ifndef O_DIRECT
#define O_DIRECT 040000
#endif
#ifndef O_CLOEXEC
#define O_CLOEXEC 02000000
#endif
#ifndef SYS_io_uring_setup
#define SYS_io_uring_setup 425
#endif
#ifndef SYS_io_uring_enter
#define SYS_io_uring_enter 426
#endif
#ifndef SYS_io_uring_register
#define SYS_io_uring_register 427
#endif

#define PERF_SCHEMA "thekernel-perf-v1"
#define PAGE_BYTES 4096U
#define RING_ENTRIES 64U
#define WARMUP_SAMPLES 4U
#define LATENCY_SAMPLES 32U
#define SIZE_COUNT 3U
#define QD_COUNT 3U
#define OP_COUNT 2U
#define CQ_WAIT_LOOPS 2000U
#define IORING_OFF_SQ_RING 0ULL
#define IORING_OFF_CQ_RING 0x08000000ULL
#define IORING_OFF_SQES 0x10000000ULL
#define IORING_ENTER_GETEVENTS (1U << 0)
#define IORING_REGISTER_BUFFERS 0U
#define IORING_UNREGISTER_BUFFERS 1U
#define IORING_OP_READ_FIXED 4U
#define IORING_OP_WRITE_FIXED 5U
#define EXT4_SUPER_MAGIC UINT32_C(0xEF53)
#define MULTI_EXTENT_COUNT 16U
#define MULTI_EXTENT_CHUNK (16U * 1024U)
#define MULTI_EXTENT_SHARD_REQUESTS 8U
#define MAX_FILE_SHARDS 4U
#define FIEMAP_EXTENT_CAPACITY 4096U

static const size_t REQUEST_SIZES[SIZE_COUNT] = {4096U, 65536U, 262144U};
static const unsigned int QUEUE_DEPTHS[QD_COUNT] = {1U, 8U, 32U};

struct io_sqring_offsets {
    uint32_t head;
    uint32_t tail;
    uint32_t ring_mask;
    uint32_t ring_entries;
    uint32_t flags;
    uint32_t dropped;
    uint32_t array;
    uint32_t resv1;
    uint64_t user_addr;
};

struct io_cqring_offsets {
    uint32_t head;
    uint32_t tail;
    uint32_t ring_mask;
    uint32_t ring_entries;
    uint32_t overflow;
    uint32_t cqes;
    uint32_t flags;
    uint32_t resv1;
    uint64_t user_addr;
};

struct io_uring_params {
    uint32_t sq_entries;
    uint32_t cq_entries;
    uint32_t flags;
    uint32_t sq_thread_cpu;
    uint32_t sq_thread_idle;
    uint32_t features;
    uint32_t wq_fd;
    uint32_t resv[3];
    struct io_sqring_offsets sq_off;
    struct io_cqring_offsets cq_off;
};

struct io_uring_cqe {
    uint64_t user_data;
    int32_t res;
    uint32_t flags;
};

struct raw_sqe {
    unsigned char bytes[64];
};

struct ring {
    int fd;
    struct io_uring_params params;
    size_t ring_bytes;
    size_t sqe_bytes;
    unsigned char *sq_ring;
    unsigned char *cq_ring;
    struct raw_sqe *sqes;
    int registered;
};

struct batch_result {
    unsigned int cqe_count;
    unsigned int missing;
    unsigned int duplicate;
    unsigned int bad_user_data;
    unsigned int bad_result;
    uint64_t digest;
};

struct sample_set {
    uint64_t wall_ns[LATENCY_SAMPLES];
    uint64_t cpu_ns[LATENCY_SAMPLES];
};

struct io_counter_state {
    int thekernel;
    int control_enabled;
};

struct data_mount {
    const char *directory;
    const char *device;
    unsigned int major_number;
    unsigned int minor_number;
};

struct physical_observation {
    long long submitted;
    long long child_submitted;
    long long completed;
    long long child_completed;
    long long highwater;
    long long extent_highwater;
    long long direct_bytes;
    long long quarantine;
    long long direct_hits;
    long long direct_fallbacks;
};

static int page_size;

_Static_assert(sizeof(struct io_sqring_offsets) == 40, "bad SQ offsets ABI");
_Static_assert(sizeof(struct io_cqring_offsets) == 40, "bad CQ offsets ABI");
_Static_assert(sizeof(struct io_uring_params) == 120, "bad params ABI");
_Static_assert(sizeof(struct io_uring_cqe) == 16, "bad CQE ABI");
_Static_assert(sizeof(struct raw_sqe) == 64, "bad SQE ABI");

static void error_message(const char *stage)
{
    int saved_errno = errno;
    fprintf(stderr,
            "TKPERF_ERROR schema=%s workload=io-uring-physical stage=%s errno=%d (%s)\n",
            PERF_SCHEMA, stage, saved_errno, strerror(saved_errno));
    errno = saved_errno;
}

static int clock_ns(clockid_t clock_id, uint64_t *result)
{
    struct timespec value;
    if (clock_gettime(clock_id, &value) != 0) {
        return -1;
    }
    if (value.tv_sec < 0 || value.tv_nsec < 0 || value.tv_nsec >= 1000000000L) {
        errno = EOVERFLOW;
        return -1;
    }
    *result = (uint64_t)value.tv_sec * UINT64_C(1000000000) +
              (uint64_t)value.tv_nsec;
    return 0;
}

static uint32_t load_u32(const unsigned char *base, uint32_t offset)
{
    const _Atomic uint32_t *word =
        (const _Atomic uint32_t *)(const void *)(base + offset);
    return atomic_load_explicit(word, memory_order_acquire);
}

static void store_u32(unsigned char *base, uint32_t offset, uint32_t value)
{
    _Atomic uint32_t *word = (_Atomic uint32_t *)(void *)(base + offset);
    atomic_store_explicit(word, value, memory_order_release);
}

static void write_u16(unsigned char *bytes, size_t offset, uint16_t value)
{
    memcpy(bytes + offset, &value, sizeof(value));
}

static void write_u32(unsigned char *bytes, size_t offset, uint32_t value)
{
    memcpy(bytes + offset, &value, sizeof(value));
}

static void write_u64(unsigned char *bytes, size_t offset, uint64_t value)
{
    memcpy(bytes + offset, &value, sizeof(value));
}

static size_t page_round(size_t value)
{
    size_t page = (size_t)page_size;
    if (page == 0 || value > SIZE_MAX - page + 1U) {
        return 0;
    }
    return (value + page - 1U) & ~(page - 1U);
}

static void ring_unmap(struct ring *ring)
{
    if (ring->sqes != NULL && ring->sqes != MAP_FAILED) {
        munmap(ring->sqes, ring->sqe_bytes);
    }
    if (ring->cq_ring != NULL && ring->cq_ring != MAP_FAILED) {
        munmap(ring->cq_ring, ring->ring_bytes);
    }
    if (ring->sq_ring != NULL && ring->sq_ring != MAP_FAILED) {
        munmap(ring->sq_ring, ring->ring_bytes);
    }
    ring->sqes = NULL;
    ring->cq_ring = NULL;
    ring->sq_ring = NULL;
}

static void ring_cleanup(struct ring *ring)
{
    if (ring->registered && ring->fd >= 0) {
        (void)syscall(SYS_io_uring_register, ring->fd,
                      IORING_UNREGISTER_BUFFERS, NULL, 0U);
        ring->registered = 0;
    }
    if (ring->fd >= 0) {
        close(ring->fd);
        ring->fd = -1;
    }
    ring_unmap(ring);
}

static int ring_setup(struct ring *ring)
{
    memset(ring, 0, sizeof(*ring));
    ring->fd = -1;
    ring->fd = (int)syscall(SYS_io_uring_setup, RING_ENTRIES, &ring->params);
    if (ring->fd < 0) {
        return -1;
    }
    size_t sq_end = (size_t)ring->params.sq_off.array +
                    (size_t)ring->params.sq_entries * sizeof(uint32_t);
    size_t cq_end = (size_t)ring->params.cq_off.cqes +
                    (size_t)ring->params.cq_entries * sizeof(struct io_uring_cqe);
    ring->ring_bytes = page_round(sq_end > cq_end ? sq_end : cq_end);
    ring->sqe_bytes = page_round((size_t)ring->params.sq_entries *
                                 sizeof(struct raw_sqe));
    if (ring->ring_bytes == 0 || ring->sqe_bytes == 0) {
        errno = EOVERFLOW;
        ring_cleanup(ring);
        return -1;
    }
    ring->sq_ring = mmap(NULL, ring->ring_bytes, PROT_READ | PROT_WRITE,
                         MAP_SHARED, ring->fd, IORING_OFF_SQ_RING);
    ring->cq_ring = mmap(NULL, ring->ring_bytes, PROT_READ | PROT_WRITE,
                         MAP_SHARED, ring->fd, IORING_OFF_CQ_RING);
    ring->sqes = mmap(NULL, ring->sqe_bytes, PROT_READ | PROT_WRITE,
                      MAP_SHARED, ring->fd, IORING_OFF_SQES);
    if (ring->sq_ring == MAP_FAILED || ring->cq_ring == MAP_FAILED ||
        ring->sqes == MAP_FAILED) {
        errno = ENOMEM;
        ring_cleanup(ring);
        return -1;
    }
    return 0;
}

static int register_buffer(struct ring *ring, void *buffer, size_t length)
{
    struct iovec iov = {.iov_base = buffer, .iov_len = length};
    if (syscall(SYS_io_uring_register, ring->fd, IORING_REGISTER_BUFFERS,
                &iov, 1U) != 0) {
        return -1;
    }
    ring->registered = 1;
    return 0;
}

static int queue_fixed(struct ring *ring, int file, uint64_t offset,
                       void *address, uint32_t length, uint64_t user_data,
                       uint8_t opcode)
{
    uint32_t head = load_u32(ring->sq_ring, ring->params.sq_off.head);
    uint32_t tail = load_u32(ring->sq_ring, ring->params.sq_off.tail);
    if (tail - head >= ring->params.sq_entries) {
        errno = EBUSY;
        return -1;
    }
    uint32_t mask = load_u32(ring->sq_ring, ring->params.sq_off.ring_mask);
    uint32_t index = tail & mask;
    struct raw_sqe *sqe = &ring->sqes[index];
    memset(sqe, 0, sizeof(*sqe));
    sqe->bytes[0] = opcode;
    write_u32(sqe->bytes, 4, (uint32_t)file);
    write_u64(sqe->bytes, 8, offset);
    write_u64(sqe->bytes, 16, (uint64_t)(uintptr_t)address);
    write_u32(sqe->bytes, 24, length);
    write_u64(sqe->bytes, 32, user_data);
    write_u16(sqe->bytes, 40, 0U);
    store_u32(ring->sq_ring, ring->params.sq_off.array +
              index * sizeof(uint32_t), index);
    store_u32(ring->sq_ring, ring->params.sq_off.tail, tail + 1U);
    return 0;
}

static int submit_all(struct ring *ring, unsigned int count)
{
    errno = 0;
    long result = syscall(SYS_io_uring_enter, ring->fd, count, count,
                          IORING_ENTER_GETEVENTS, NULL, 0U);
    if (result < 0) {
        return -1;
    }
    if (result != (long)count) {
        errno = EIO;
        return -1;
    }
    return 0;
}

static const struct io_uring_cqe *next_cqe(const struct ring *ring,
                                           uint32_t head)
{
    uint32_t mask = load_u32(ring->cq_ring, ring->params.cq_off.ring_mask);
    uint32_t index = head & mask;
    return (const struct io_uring_cqe *)(const void *)
        (ring->cq_ring + ring->params.cq_off.cqes +
         index * sizeof(struct io_uring_cqe));
}

static int collect_cqes(struct ring *ring, unsigned int queue_depth,
                        size_t request_size, struct batch_result *result)
{
    unsigned int seen = 0;
    unsigned int cqe_seen = 0;
    uint64_t digest = UINT64_C(1469598103934665603);
    const uint64_t prefix = UINT64_C(0x5045524600000000);
    for (unsigned int attempt = 0;
         attempt < CQ_WAIT_LOOPS && cqe_seen < queue_depth;
         ++attempt) {
        uint32_t head = load_u32(ring->cq_ring, ring->params.cq_off.head);
        uint32_t tail = load_u32(ring->cq_ring, ring->params.cq_off.tail);
        if (tail == head) {
            struct pollfd descriptor = {
                .fd = ring->fd,
                .events = POLLIN,
                .revents = 0,
            };
            if (poll(&descriptor, 1, 1) < 0 && errno != EINTR) {
                return -1;
            }
            continue;
        }
        const struct io_uring_cqe *cqe = next_cqe(ring, head);
        uint64_t user_data = cqe->user_data;
        if ((user_data & UINT64_C(0xffffffff00000000)) != prefix) {
            ++result->bad_user_data;
        } else {
            unsigned int index = (unsigned int)(user_data & UINT64_C(0xffffffff));
            if (index >= queue_depth) {
                ++result->bad_user_data;
            } else {
                unsigned int bit = 1U << index;
                if ((seen & bit) != 0) {
                    ++result->duplicate;
                } else {
                    seen |= bit;
                    ++result->cqe_count;
                    ++cqe_seen;
                    if (cqe->res != (int32_t)request_size) {
                        ++result->bad_result;
                    }
                    digest ^= user_data;
                    digest *= UINT64_C(1099511628211);
                    digest ^= (uint32_t)cqe->res;
                    digest *= UINT64_C(1099511628211);
                }
            }
        }
        store_u32(ring->cq_ring, ring->params.cq_off.head, head + 1U);
    }
    if (result->cqe_count < queue_depth) {
        result->missing = queue_depth - result->cqe_count;
        errno = ETIMEDOUT;
    }
    result->digest = digest;
    if (result->missing != 0 || result->duplicate != 0 ||
        result->bad_user_data != 0 || result->bad_result != 0) {
        if (errno == 0) {
            errno = EPROTO;
        }
        return -1;
    }
    return 0;
}

static uint64_t content_digest(const unsigned char *buffer, size_t length)
{
    uint64_t value = UINT64_C(1469598103934665603);
    for (size_t index = 0; index < length; ++index) {
        value ^= buffer[index];
        value *= UINT64_C(1099511628211);
    }
    return value;
}

static unsigned char pattern_byte(unsigned int index, size_t request_size)
{
    if (request_size == MULTI_EXTENT_COUNT * MULTI_EXTENT_CHUNK) {
        index %= MULTI_EXTENT_SHARD_REQUESTS;
    }
    return (unsigned char)(0x31U + (index * 17U) + (request_size / PAGE_BYTES));
}

static void fill_pattern(unsigned char *buffer, size_t length,
                         unsigned int index, size_t request_size)
{
    memset(buffer, pattern_byte(index, request_size), length);
}

static uint64_t expected_digest(size_t request_size, unsigned int index)
{
    unsigned char *buffer = malloc(request_size);
    if (buffer == NULL) {
        return 0;
    }
    fill_pattern(buffer, request_size, index, request_size);
    uint64_t result = content_digest(buffer, request_size);
    free(buffer);
    return result;
}

static int digest_buffers(const unsigned char *buffer, size_t request_size,
                          unsigned int queue_depth, uint64_t *digest)
{
    uint64_t value = UINT64_C(1469598103934665603);
    for (unsigned int index = 0; index < queue_depth; ++index) {
        uint64_t actual = content_digest(buffer + index * request_size,
                                         request_size);
        uint64_t expected = expected_digest(request_size, index);
        if (actual != expected) {
            return -1;
        }
        value ^= actual;
        value *= UINT64_C(1099511628211);
    }
    *digest = value;
    return 0;
}

static int verify_fragmented_layout(int file, size_t request_size,
                                    unsigned int queue_depth)
{
    if (request_size != MULTI_EXTENT_COUNT * MULTI_EXTENT_CHUNK ||
        queue_depth == 0U || queue_depth > 32U) {
        errno = EINVAL;
        return -1;
    }
    size_t total = request_size * queue_depth;
    size_t map_bytes = sizeof(struct fiemap) +
                       (size_t)FIEMAP_EXTENT_CAPACITY *
                           sizeof(struct fiemap_extent);
    struct fiemap *map = calloc(1, map_bytes);
    if (map == NULL) {
        errno = ENOMEM;
        return -1;
    }
    map->fm_start = 0;
    map->fm_length = total;
    map->fm_flags = FIEMAP_FLAG_SYNC;
    map->fm_extent_count = FIEMAP_EXTENT_CAPACITY;
    if (ioctl(file, FS_IOC_FIEMAP, map) != 0) {
        int saved_errno = errno;
        free(map);
        errno = saved_errno;
        return -1;
    }
    if (map->fm_mapped_extents == 0U ||
        map->fm_mapped_extents > FIEMAP_EXTENT_CAPACITY) {
        free(map);
        errno = EPROTO;
        return -1;
    }

    unsigned int counts[32] = {0};
    size_t bytes[32] = {0};
    int saw_last = 0;
    for (uint32_t index = 0; index < map->fm_mapped_extents; ++index) {
        const struct fiemap_extent *extent = &map->fm_extents[index];
        uint64_t logical_end = extent->fe_logical + extent->fe_length;
        if ((extent->fe_flags & FIEMAP_EXTENT_UNWRITTEN) != 0U ||
            extent->fe_length == 0U || logical_end < extent->fe_logical ||
            logical_end > total ||
            extent->fe_physical + extent->fe_length < extent->fe_physical) {
            free(map);
            errno = EPROTO;
            return -1;
        }
        size_t request = (size_t)(extent->fe_logical / request_size);
        size_t request_base = request * request_size;
        if (request >= queue_depth ||
            extent->fe_logical !=
                (uint64_t)(request_base + bytes[request]) ||
            logical_end > (uint64_t)(request + 1U) * request_size ||
            extent->fe_length != MULTI_EXTENT_CHUNK ||
            counts[request] == MULTI_EXTENT_COUNT ||
            bytes[request] > request_size - (size_t)extent->fe_length) {
            free(map);
            errno = EPROTO;
            return -1;
        }
        ++counts[request];
        bytes[request] += (size_t)extent->fe_length;
        if ((extent->fe_flags & FIEMAP_EXTENT_LAST) != 0U) {
            saw_last = 1;
        }
    }
    free(map);
    /* Fragmented fixtures keep five sparse extents beyond the measured
     * range so ext4 has already externalized its extent tree.  LAST on this
     * prefix query would therefore be a false end-of-file claim. */
    if (saw_last) {
        errno = EPROTO;
        return -1;
    }
    for (unsigned int request = 0; request < queue_depth; ++request) {
        if (counts[request] != MULTI_EXTENT_COUNT ||
            bytes[request] != request_size) {
            errno = EPROTO;
            return -1;
        }
    }
    return 0;
}

static int prepare_file(const char *path, size_t request_size,
                        unsigned int queue_depth, int fragmented)
{
    if (queue_depth == 0U || queue_depth > 32U || request_size == 0U ||
        request_size > SIZE_MAX / queue_depth) {
        errno = EINVAL;
        return -1;
    }
    size_t total = request_size * queue_depth;
    int seed = open(path, O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0600);
    if (seed < 0) {
        return -1;
    }
    char distractor_path[224];
    int distractor = -1;
    if (fragmented) {
        int length = snprintf(distractor_path, sizeof(distractor_path),
                              "%s-fragmenter", path);
        if (length < 0 || (size_t)length >= sizeof(distractor_path) ||
            request_size != MULTI_EXTENT_COUNT * MULTI_EXTENT_CHUNK) {
            close(seed);
            errno = EINVAL;
            return -1;
        }
        distractor = open(distractor_path,
                          O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0600);
        if (distractor < 0) {
            int saved_errno = errno;
            close(seed);
            errno = saved_errno;
            return -1;
        }
    } else if (fallocate(seed, 0, 0, (off_t)total) != 0) {
        int saved_errno = errno;
        close(seed);
        errno = saved_errno;
        return -1;
    }

    size_t block_size = fragmented ? MULTI_EXTENT_CHUNK : request_size;
    unsigned char *block = malloc(block_size);
    if (block == NULL) {
        int saved_errno = ENOMEM;
        close(seed);
        if (distractor >= 0) {
            close(distractor);
            unlink(distractor_path);
        }
        errno = saved_errno;
        return -1;
    }
    size_t distractor_offset = 0;
    if (distractor >= 0) {
        /* Build five sparse one-block extents beyond the measured range
         * first.  A fresh ext4 inode holds only a few extents inline; growing
         * that tree while allocating the measured fifth extent can split one
         * 16 KiB target chunk into 4 KiB + 12 KiB.  Paying the tree-growth
         * allocation in this unmeasured prelude makes the 16-child geometry
         * deterministic without weakening the FIEMAP oracle. */
        memset(block, 0x7f, PAGE_BYTES);
        for (unsigned int tree_extent = 0; tree_extent < 5U; ++tree_extent) {
            off_t tree_offset = (off_t)total +
                                (off_t)(2U * tree_extent + 1U) * PAGE_BYTES;
            ssize_t written = pwrite(seed, block, PAGE_BYTES, tree_offset);
            if (written != (ssize_t)PAGE_BYTES || fsync(seed) != 0) {
                int saved_errno = errno != 0 ? errno : EIO;
                free(block);
                close(seed);
                close(distractor);
                unlink(distractor_path);
                errno = saved_errno;
                return -1;
            }
        }

        /* Start the alternating target/distractor stripes with the
         * distractor so both sides of the measured allocation have a stable
         * competing owner. */
        memset(block, 0x80, MULTI_EXTENT_CHUNK);
        ssize_t written = pwrite(distractor, block, MULTI_EXTENT_CHUNK, 0);
        if (written != (ssize_t)MULTI_EXTENT_CHUNK || fsync(distractor) != 0) {
            int saved_errno = errno != 0 ? errno : EIO;
            free(block);
            close(seed);
            close(distractor);
            unlink(distractor_path);
            errno = saved_errno;
            return -1;
        }
        close(distractor);
        distractor = -1;
        distractor_offset = MULTI_EXTENT_CHUNK;
    }
    if (!fragmented) {
        for (unsigned int index = 0; index < queue_depth; ++index) {
            fill_pattern(block, request_size, index, request_size);
            ssize_t written = pwrite(seed, block, request_size,
                                     (off_t)index * (off_t)request_size);
            if (written != (ssize_t)request_size) {
                int saved_errno = errno != 0 ? errno : EIO;
                free(block);
                close(seed);
                errno = saved_errno;
                return -1;
            }
        }
        if (fsync(seed) != 0) {
            int saved_errno = errno;
            free(block);
            close(seed);
            errno = saved_errno;
            return -1;
        }
        free(block);
        close(seed);
        return 0;
    }

    /* Commit each 16 KiB target chunk separately, with one equally-sized
     * allocation in a competing inode between chunks.  Closing both inodes
     * after fsync drops ext4's preallocation state, making every target
     * chunk independently visible to FIEMAP as a written extent. */
    for (unsigned int request = 0; request < queue_depth; ++request) {
        for (unsigned int extent = 0; extent < MULTI_EXTENT_COUNT; ++extent) {
            size_t offset = (size_t)request * request_size +
                            (size_t)extent * MULTI_EXTENT_CHUNK;
            fill_pattern(block, MULTI_EXTENT_CHUNK, request, request_size);
            ssize_t written = pwrite(seed, block, MULTI_EXTENT_CHUNK,
                                     (off_t)offset);
            if (written != (ssize_t)MULTI_EXTENT_CHUNK || fsync(seed) != 0) {
                int saved_errno = errno != 0 ? errno : EIO;
                free(block);
                close(seed);
                close(distractor);
                unlink(distractor_path);
                errno = saved_errno;
                return -1;
            }
            close(seed);
            seed = -1;

            distractor = open(distractor_path, O_RDWR | O_CLOEXEC);
            if (distractor < 0) {
                int saved_errno = errno;
                free(block);
                unlink(distractor_path);
                errno = saved_errno;
                return -1;
            }
            memset(block, (int)(0x80U +
                                ((request * MULTI_EXTENT_COUNT + extent) & 0x7fU)),
                   MULTI_EXTENT_CHUNK);
            written = pwrite(distractor, block, MULTI_EXTENT_CHUNK,
                             (off_t)distractor_offset);
            if (written != (ssize_t)MULTI_EXTENT_CHUNK || fsync(distractor) != 0) {
                int saved_errno = errno != 0 ? errno : EIO;
                free(block);
                close(distractor);
                unlink(distractor_path);
                errno = saved_errno;
                return -1;
            }
            close(distractor);
            distractor = -1;
            distractor_offset += MULTI_EXTENT_CHUNK;

            if (request != queue_depth - 1U || extent != MULTI_EXTENT_COUNT - 1U) {
                seed = open(path, O_RDWR | O_CLOEXEC);
                if (seed < 0) {
                    int saved_errno = errno;
                    free(block);
                    unlink(distractor_path);
                    errno = saved_errno;
                    return -1;
                }
            }
        }
    }
    free(block);
    seed = open(path, O_RDONLY | O_CLOEXEC);
    if (seed < 0) {
        int saved_errno = errno;
        unlink(distractor_path);
        errno = saved_errno;
        return -1;
    }
    int layout_result = verify_fragmented_layout(seed, request_size, queue_depth);
    int saved_errno = errno;
    close(seed);
    if (layout_result != 0) {
        unlink(distractor_path);
        errno = saved_errno != 0 ? saved_errno : EPROTO;
        return -1;
    }
    return 0;
}

static int remove_fragmenter(const char *path)
{
    char fragmenter_path[224];
    int length = snprintf(fragmenter_path, sizeof(fragmenter_path),
                          "%s-fragmenter", path);
    if (length < 0 || (size_t)length >= sizeof(fragmenter_path)) {
        errno = ENAMETOOLONG;
        return -1;
    }
    return unlink(fragmenter_path);
}

static int verify_files(const int *files, unsigned int requests_per_file,
                        size_t request_size, unsigned int queue_depth,
                        uint64_t *digest)
{
    unsigned char *block = NULL;
    if (posix_memalign((void **)&block, PAGE_BYTES, request_size) != 0) {
        errno = ENOMEM;
        return -1;
    }
    uint64_t value = UINT64_C(1469598103934665603);
    for (unsigned int index = 0; index < queue_depth; ++index) {
        unsigned int shard = index / requests_per_file;
        unsigned int shard_index = index % requests_per_file;
        ssize_t result = pread(files[shard], block, request_size,
                               (off_t)shard_index * (off_t)request_size);
        if (result != (ssize_t)request_size) {
            int saved_errno = errno != 0 ? errno : EIO;
            free(block);
            errno = saved_errno;
            return -1;
        }
        uint64_t actual = content_digest(block, request_size);
        if (actual != expected_digest(request_size, index)) {
            free(block);
            errno = EIO;
            return -1;
        }
        value ^= actual;
        value *= UINT64_C(1099511628211);
    }
    free(block);
    *digest = value;
    return 0;
}

static int run_batch(struct ring *ring, const int *files,
                     unsigned int requests_per_file, void *buffer,
                     size_t request_size, unsigned int queue_depth,
                     uint8_t opcode, struct batch_result *result)
{
    memset(result, 0, sizeof(*result));
    const uint64_t user_prefix = UINT64_C(0x5045524600000000);
    for (unsigned int index = 0; index < queue_depth; ++index) {
        unsigned int shard = index / requests_per_file;
        unsigned int shard_index = index % requests_per_file;
        if (queue_fixed(ring, files[shard],
                        (uint64_t)shard_index * request_size,
                        (unsigned char *)buffer + index * request_size,
                        (uint32_t)request_size, user_prefix | index, opcode) != 0) {
            return -1;
        }
    }
    if (submit_all(ring, queue_depth) != 0) {
        return -1;
    }
    int result_code = collect_cqes(ring, queue_depth, request_size, result);
    if (result_code != 0 && errno == 0) {
        errno = EPROTO;
    }
    return result_code;
}

static int timed_batch(struct ring *ring, const int *files,
                       unsigned int requests_per_file, void *buffer,
                       size_t request_size, unsigned int queue_depth,
                       uint8_t opcode, uint64_t *wall_ns, uint64_t *cpu_ns)
{
    struct batch_result result;
    uint64_t wall_start;
    uint64_t cpu_start;
    uint64_t wall_end;
    uint64_t cpu_end;
    if (clock_ns(CLOCK_MONOTONIC, &wall_start) != 0 ||
        clock_ns(CLOCK_PROCESS_CPUTIME_ID, &cpu_start) != 0 ||
        run_batch(ring, files, requests_per_file, buffer, request_size,
                  queue_depth, opcode, &result) != 0 ||
        clock_ns(CLOCK_PROCESS_CPUTIME_ID, &cpu_end) != 0 ||
        clock_ns(CLOCK_MONOTONIC, &wall_end) != 0) {
        return -1;
    }
    if (wall_end < wall_start || cpu_end < cpu_start) {
        errno = EPROTO;
        return -1;
    }
    *wall_ns = wall_end - wall_start;
    *cpu_ns = cpu_end - cpu_start;
    return 0;
}

static int compare_u64(const void *left, const void *right)
{
    const uint64_t lhs = *(const uint64_t *)left;
    const uint64_t rhs = *(const uint64_t *)right;
    return (lhs > rhs) - (lhs < rhs);
}

static uint64_t quantile(uint64_t *values, size_t count, unsigned int permille)
{
    size_t rank = (count * permille + 999U) / 1000U;
    if (rank == 0) {
        rank = 1;
    }
    if (rank > count) {
        rank = count;
    }
    qsort(values, count, sizeof(*values), compare_u64);
    return values[rank - 1U];
}

static const char *operation_name(uint8_t opcode)
{
    return opcode == IORING_OP_READ_FIXED ? "read_fixed" : "write_fixed";
}

static int verify_data_mount(const char *directory, const char *device,
                             struct data_mount *proof)
{
    struct stat directory_stat;
    struct stat device_stat;
    struct statfs filesystem;
    size_t directory_length;

    if (directory == NULL || device == NULL || directory[0] != '/') {
        errno = EINVAL;
        return -1;
    }
    directory_length = strlen(directory);
    if (directory_length == 0U || directory_length >= 192U ||
        strcmp(directory, "/") == 0 ||
        strncmp(directory, "/tmp", 4U) == 0) {
        /* The benchmark must not accidentally turn the rootfs/tmpfs into a
         * block benchmark. */
        errno = EINVAL;
        return -1;
    }
    if (stat(directory, &directory_stat) != 0 ||
        !S_ISDIR(directory_stat.st_mode)) {
        errno = ENOTDIR;
        return -1;
    }
    if (stat(device, &device_stat) != 0 || !S_ISBLK(device_stat.st_mode)) {
        errno = ENODEV;
        return -1;
    }
    if (statfs(directory, &filesystem) != 0 ||
        (uint64_t)filesystem.f_type != EXT4_SUPER_MAGIC) {
        errno = EINVAL;
        return -1;
    }
    /* st_dev is the mounted filesystem's device identity; st_rdev is the
     * identity of the explicit block device supplied by the harness. */
    if (directory_stat.st_dev != device_stat.st_rdev) {
        errno = EXDEV;
        return -1;
    }
    proof->directory = directory;
    proof->device = device;
    proof->major_number = (unsigned int)major(device_stat.st_rdev);
    proof->minor_number = (unsigned int)minor(device_stat.st_rdev);
    return 0;
}

static void emit_data_mount(uint64_t run_id, const struct data_mount *proof)
{
    printf("TKPERF_DATA schema=%s workload=io-uring-physical"
           " run_id=%016" PRIx64 " device=%s mount=%s fs=ext4"
           " major=%u minor=%u identity=verified mapping=unique-rootfs-extra\n",
           PERF_SCHEMA, run_id, proof->device, proof->directory,
           proof->major_number, proof->minor_number);
}

static int read_counter(const char *name, long long *result)
{
    FILE *file = fopen("/proc/io_stats", "r");
    if (file == NULL) {
        return errno == ENOENT ? 1 : -1;
    }
    char line[256];
    int found = 0;
    while (fgets(line, sizeof(line), file) != NULL) {
        char key[160];
        long long value;
        if (sscanf(line, "%159s %lld", key, &value) == 2 &&
            strcmp(key, name) == 0) {
            *result = value;
            found = 1;
            break;
        }
    }
    int saved_errno = errno;
    fclose(file);
    errno = saved_errno;
    return found ? 0 : 1;
}

static int write_control_command(const char *command)
{
    size_t length = strlen(command);
    int fd = open("/proc/io_test_control", O_WRONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    ssize_t written = write(fd, command, length);
    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    if (written != (ssize_t)length) {
        if (written >= 0) {
            errno = EIO;
        }
        return -1;
    }
    return 0;
}

static int detect_counters(struct io_counter_state *state)
{
    memset(state, 0, sizeof(*state));
    struct stat stats;
    struct stat control;
    int stats_present = stat("/proc/io_stats", &stats) == 0;
    int control_present = stat("/proc/io_test_control", &control) == 0;
    if (!stats_present && !control_present) {
        /* Linux does not expose TheKernel's test counter namespace. */
        return 0;
    }
    if (!stats_present || !control_present) {
        /* A partial namespace is not enough to establish a physical oracle. */
        errno = EOPNOTSUPP;
        return -1;
    }
    if (write_control_command("counters=on\n") != 0 ||
        write_control_command("counters=reset\n") != 0) {
        return -1;
    }
    state->thekernel = 1;
    state->control_enabled = 1;
    return 0;
}

static int disable_counters(struct io_counter_state *state)
{
    if (!state->control_enabled) {
        return 0;
    }
    state->control_enabled = 0;
    return write_control_command("counters=off\n");
}

static int reset_counters(const struct io_counter_state *state)
{
    if (!state->thekernel) {
        return 0;
    }
    return write_control_command("counters=reset\n");
}

static uint64_t make_run_id(void)
{
    uint64_t value;
    if (clock_ns(CLOCK_MONOTONIC, &value) != 0) {
        value = UINT64_C(0x544b504859534943);
    }
    return value ^ ((uint64_t)(uint32_t)getpid() << 32);
}

static void unsupported_markers(uint64_t run_id, const char *cell,
                                size_t request_size, unsigned int queue_depth,
                                uint8_t opcode, const char *reason,
                                const char *path, const char *oracle)
{
    printf("TKPERF_CORRECTNESS schema=%s workload=io-uring-physical"
           " run_id=%016" PRIx64
           " cell=%s op=%s size=%zu qd=%u status=unsupported reason=%s"
           " cqe=0 missing=unsupported duplicate=unsupported digest=unsupported"
           " path=%s oracle=%s proof=unsupported-ablation\n",
           PERF_SCHEMA, run_id, cell, operation_name(opcode), request_size,
           queue_depth, reason, path, oracle);
    printf("TKPERF_WINDOW schema=%s workload=io-uring-physical"
           " run_id=%016" PRIx64
           " cell=%s op=%s size=%zu qd=%u status=unsupported warmup=0"
           " samples=0 clocks=monotonic,process-cpu reason=%s path=%s oracle=%s"
           " proof=unsupported-ablation\n",
           PERF_SCHEMA, run_id, cell, operation_name(opcode), request_size,
           queue_depth, reason, path, oracle);
    printf("TKPERF_LATENCY schema=%s workload=io-uring-physical"
           " run_id=%016" PRIx64
           " cell=%s op=%s size=%zu qd=%u status=unsupported samples=0"
           " wall_p50_ns=unsupported wall_p99_ns=unsupported"
           " cpu_p50_ns=unsupported cpu_p99_ns=unsupported reason=%s path=%s"
           " oracle=%s proof=unsupported-ablation\n",
           PERF_SCHEMA, run_id, cell, operation_name(opcode), request_size,
           queue_depth, reason, path, oracle);
}

static int environmental_unsupported_errno(int error_number)
{
    return error_number == ENOMEM || error_number == EPERM ||
           error_number == EOPNOTSUPP || error_number == ENOSYS ||
           error_number == EINVAL;
}

static const char *environmental_reason(int error_number)
{
    switch (error_number) {
    case ENOMEM:
        return "registered-buffer-resource-limit";
    case EPERM:
        return "io-uring-permission";
    case EOPNOTSUPP:
        return "physical-io-unsupported";
    case ENOSYS:
        return "io-uring-unavailable";
    case EINVAL:
        return "direct-io-geometry-unsupported";
    default:
        return "environmental-unsupported";
    }
}

static int read_physical_observation(uint8_t opcode,
                                     struct physical_observation *observation)
{
    const char *hits = opcode == IORING_OP_READ_FIXED
                           ? "io_uring.dma_direct_read_hits"
                           : "io_uring.dma_direct_write_hits";
    const char *fallbacks = opcode == IORING_OP_READ_FIXED
                                ? "io_uring.dma_direct_read_fallbacks"
                                : "io_uring.dma_direct_write_fallbacks";
    if (read_counter("io_uring.physical_submitted", &observation->submitted) != 0 ||
        read_counter("io_uring.physical_child_submitted",
                     &observation->child_submitted) != 0 ||
        read_counter("io_uring.physical_completed", &observation->completed) != 0 ||
        read_counter("io_uring.physical_child_completed",
                     &observation->child_completed) != 0 ||
        read_counter("io_uring.physical_qd_highwater", &observation->highwater) != 0 ||
        read_counter("io_uring.physical_extent_highwater",
                     &observation->extent_highwater) != 0 ||
        read_counter("io_uring.physical_direct_bytes", &observation->direct_bytes) != 0 ||
        read_counter("io_uring.physical_quarantine", &observation->quarantine) != 0 ||
        read_counter(hits, &observation->direct_hits) != 0 ||
        read_counter(fallbacks, &observation->direct_fallbacks) != 0) {
        errno = EOPNOTSUPP;
        return -1;
    }
    if (observation->submitted < 0 || observation->child_submitted < 0 ||
        observation->completed < 0 || observation->child_completed < 0 ||
        observation->highwater < 0 || observation->extent_highwater < 0 ||
        observation->direct_bytes < 0 ||
        observation->quarantine < 0 || observation->direct_hits < 0 ||
        observation->direct_fallbacks < 0) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

static int physical_observation_delta(
    const struct physical_observation *before,
    const struct physical_observation *after, size_t request_size,
    unsigned int queue_depth, unsigned int batch_count,
    unsigned int expected_extent_highwater,
    unsigned int expected_extents_per_request,
    long long *submitted, long long *child_submitted,
    long long *completed, long long *child_completed,
    long long *direct_bytes, long long *quarantine, long long *direct_hits,
    long long *direct_fallbacks)
{
    /* qd is the requested SQ batch size.  physical_qd_highwater is the
     * achieved live-owner depth, which can be lower when admission and
     * completion run concurrently; completion must remain immediate. */
    int highwater_valid;
    long long expected_requests = (long long)queue_depth * batch_count;
    long long expected_bytes = (long long)request_size * queue_depth *
                               batch_count;
    long long expected_children = expected_requests * expected_extents_per_request;
    if (after->submitted < before->submitted ||
        after->child_submitted < before->child_submitted ||
        after->completed < before->completed ||
        after->child_completed < before->child_completed ||
        after->direct_bytes < before->direct_bytes ||
        after->quarantine < before->quarantine ||
        after->direct_hits < before->direct_hits ||
        after->direct_fallbacks < before->direct_fallbacks ||
        after->highwater < 0 || after->extent_highwater < 0) {
        errno = EPROTO;
        return -1;
    }
    *submitted = after->submitted - before->submitted;
    *child_submitted = after->child_submitted - before->child_submitted;
    *completed = after->completed - before->completed;
    *child_completed = after->child_completed - before->child_completed;
    *direct_bytes = after->direct_bytes - before->direct_bytes;
    *quarantine = after->quarantine - before->quarantine;
    *direct_hits = after->direct_hits - before->direct_hits;
    *direct_fallbacks = after->direct_fallbacks - before->direct_fallbacks;
    highwater_valid = after->highwater >= (queue_depth == 1U ? 1 : 2) &&
                      after->highwater <= *submitted;
    if (batch_count == 0U || *submitted != expected_requests ||
        *child_submitted != expected_children ||
        *completed != expected_requests || *child_completed != expected_children ||
        *completed > *submitted || *child_completed > *child_submitted ||
        !highwater_valid ||
        after->extent_highwater != (long long)expected_extent_highwater ||
        *direct_bytes != expected_bytes || *direct_hits != expected_requests ||
        *direct_fallbacks != 0 || *quarantine != 0) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

static int run_cell(uint64_t run_id, const char *data_directory,
                    const struct io_counter_state *counters,
                    size_t request_size,
                    unsigned int queue_depth, uint8_t opcode,
                    const char *cell)
{
    size_t total = request_size * queue_depth;
    int fragmented = request_size == MULTI_EXTENT_COUNT * MULTI_EXTENT_CHUNK;
    unsigned int requests_per_file =
        fragmented && queue_depth > MULTI_EXTENT_SHARD_REQUESTS
            ? MULTI_EXTENT_SHARD_REQUESTS
            : queue_depth;
    unsigned int file_count =
        (queue_depth + requests_per_file - 1U) / requests_per_file;
    unsigned int expected_extent_highwater = fragmented ? MULTI_EXTENT_COUNT : 1U;
    unsigned int expected_extents_per_request = expected_extent_highwater;
    unsigned char *buffer = mmap(NULL, total, PROT_READ | PROT_WRITE,
                                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (buffer == MAP_FAILED || (uintptr_t)buffer % PAGE_BYTES != 0) {
        if (buffer != MAP_FAILED) {
            munmap(buffer, total);
        }
        errno = ENOMEM;
        return -1;
    }
    memset(buffer, 0xa5, total);
    if (file_count == 0U || file_count > MAX_FILE_SHARDS) {
        munmap(buffer, total);
        errno = EINVAL;
        return -1;
    }
    char paths[MAX_FILE_SHARDS][192] = {{0}};
    int files[MAX_FILE_SHARDS];
    for (unsigned int shard = 0; shard < MAX_FILE_SHARDS; ++shard) {
        files[shard] = -1;
    }

    int result = 1;
    int correctness_emitted = 0;
    struct ring ring;
    struct physical_observation before_physical;
    struct physical_observation after_physical;
    char correctness_physical_fields[384] = "";
    char measurement_physical_fields[384] = "";
    memset(&ring, 0, sizeof(ring));
    ring.fd = -1;
    for (unsigned int shard = 0; shard < file_count; ++shard) {
        unsigned int first = shard * requests_per_file;
        unsigned int shard_requests = queue_depth - first;
        if (shard_requests > requests_per_file) {
            shard_requests = requests_per_file;
        }
        int path_length = snprintf(
            paths[shard], sizeof(paths[shard]),
            "%s/thekernel-io-physical-%ld-%zu-%u-%u-%u",
            data_directory, (long)getpid(), request_size, queue_depth,
            (unsigned int)opcode, shard);
        if (path_length < 0 ||
            (size_t)path_length >= sizeof(paths[shard])) {
            errno = ENAMETOOLONG;
            goto out;
        }
        if (prepare_file(paths[shard], request_size, shard_requests,
                         fragmented) != 0) {
            goto out;
        }
        files[shard] = open(paths[shard], O_RDWR | O_DIRECT | O_CLOEXEC);
        if (files[shard] < 0) {
            goto out;
        }
    }
    if (fragmented) {
        /* Keep every shard's competing allocation alive until all target
         * layouts are complete.  Releasing an earlier fragmenter lets a
         * later shard's 4 KiB extent-tree allocations split those recycled
         * 16 KiB gaps and destroys the geometry this test is proving. */
        for (unsigned int shard = 0; shard < file_count; ++shard) {
            if (remove_fragmenter(paths[shard]) != 0) {
                goto out;
            }
        }
    }
    if (opcode == IORING_OP_WRITE_FIXED) {
        for (unsigned int index = 0; index < queue_depth; ++index) {
            fill_pattern(buffer + index * request_size, request_size, index,
                         request_size);
        }
    }
    if (ring_setup(&ring) != 0 || register_buffer(&ring, buffer, total) != 0) {
        goto out;
    }

    struct batch_result correctness_result;
    if (opcode == IORING_OP_READ_FIXED) {
        memset(buffer, 0xa5, total);
    }
    if (reset_counters(counters) != 0) {
        goto out;
    }
    if (counters->thekernel &&
        read_physical_observation(opcode, &before_physical) != 0) {
        goto out;
    }
    if (run_batch(&ring, files, requests_per_file, buffer, request_size,
                  queue_depth, opcode, &correctness_result) != 0) {
        goto out;
    }
    if (counters->thekernel) {
        long long submitted;
        long long child_submitted;
        long long completed;
        long long child_completed;
        long long direct_bytes;
        long long quarantine;
        long long direct_hits;
        long long direct_fallbacks;
        if (read_physical_observation(opcode, &after_physical) != 0 ||
            physical_observation_delta(
                &before_physical, &after_physical, request_size, queue_depth, 1U,
                expected_extent_highwater, expected_extents_per_request,
                &submitted, &child_submitted, &completed, &child_completed,
                &direct_bytes, &quarantine,
                &direct_hits, &direct_fallbacks) != 0) {
            errno = EPROTO;
            goto out;
        }
        (void)snprintf(
            correctness_physical_fields, sizeof(correctness_physical_fields),
            " physical_submitted=%lld physical_completed=%lld"
            " physical_child_submitted=%lld physical_child_completed=%lld"
            " physical_qd_highwater=%lld physical_direct_bytes=%lld"
            " physical_extent_highwater=%lld"
            " physical_quarantine=%lld direct_hit_delta=%lld"
            " direct_fallback_delta=%lld",
            submitted, completed, child_submitted, child_completed,
            after_physical.highwater, direct_bytes,
            after_physical.extent_highwater,
            quarantine, direct_hits, direct_fallbacks);
    }
    uint64_t digest = 0;
    if (opcode == IORING_OP_READ_FIXED) {
        if (digest_buffers(buffer, request_size, queue_depth, &digest) != 0) {
            errno = EIO;
            goto out;
        }
    } else {
        for (unsigned int shard = 0; shard < file_count; ++shard) {
            if (fsync(files[shard]) != 0) {
                goto out;
            }
        }
        if (verify_files(files, requests_per_file, request_size, queue_depth,
                         &digest) != 0) {
            goto out;
        }
    }
    if (correctness_result.missing != 0 || correctness_result.duplicate != 0 ||
        correctness_result.bad_user_data != 0 || correctness_result.bad_result != 0) {
        errno = EPROTO;
        goto out;
    }
    printf("TKPERF_CORRECTNESS schema=%s workload=io-uring-physical"
           " run_id=%016" PRIx64
           " cell=%s op=%s size=%zu qd=%u status=ok cqe=%u missing=%u"
           " duplicate=%u digest=%016" PRIx64 " user_data=verified"
           " path=%s oracle=%s proof=%s%s\n",
           PERF_SCHEMA, run_id, cell, operation_name(opcode), request_size,
           queue_depth, correctness_result.cqe_count, correctness_result.missing,
           correctness_result.duplicate,
           digest,
           counters->thekernel ? "thekernel-physical-dma" : "linux-io-uring",
           counters->thekernel ? "thekernel-physical-counters"
                               : "linux-kernel-no-thekernel-counters",
           counters->thekernel ? "physical-dma"
                               : "linux-active/unsupported-ablation",
           correctness_physical_fields);
    correctness_emitted = 1;

    struct sample_set samples;
    if (reset_counters(counters) != 0) {
        goto out;
    }
    if (counters->thekernel &&
        read_physical_observation(opcode, &before_physical) != 0) {
        goto out;
    }
    for (unsigned int index = 0; index < WARMUP_SAMPLES; ++index) {
        uint64_t wall_ns;
        uint64_t cpu_ns;
        if (timed_batch(&ring, files, requests_per_file, buffer, request_size,
                        queue_depth, opcode, &wall_ns, &cpu_ns) != 0) {
            goto out;
        }
    }
    for (unsigned int index = 0; index < LATENCY_SAMPLES; ++index) {
        if (timed_batch(&ring, files, requests_per_file, buffer, request_size,
                        queue_depth, opcode, &samples.wall_ns[index],
                        &samples.cpu_ns[index]) != 0) {
            goto out;
        }
    }
    if (counters->thekernel) {
        long long submitted;
        long long child_submitted;
        long long completed;
        long long child_completed;
        long long direct_bytes;
        long long quarantine;
        long long direct_hits;
        long long direct_fallbacks;
        if (read_physical_observation(opcode, &after_physical) != 0 ||
            physical_observation_delta(
                &before_physical, &after_physical, request_size, queue_depth,
                WARMUP_SAMPLES + LATENCY_SAMPLES, expected_extent_highwater,
                expected_extents_per_request,
                &submitted, &child_submitted, &completed, &child_completed,
                &direct_bytes, &quarantine, &direct_hits, &direct_fallbacks) != 0) {
            errno = EPROTO;
            goto out;
        }
        (void)snprintf(
            measurement_physical_fields, sizeof(measurement_physical_fields),
            " physical_submitted=%lld physical_completed=%lld"
            " physical_child_submitted=%lld physical_child_completed=%lld"
            " physical_qd_highwater=%lld physical_direct_bytes=%lld"
            " physical_extent_highwater=%lld"
            " physical_quarantine=%lld direct_hit_delta=%lld"
            " direct_fallback_delta=%lld",
            submitted, completed, child_submitted, child_completed,
            after_physical.highwater, direct_bytes,
            after_physical.extent_highwater,
            quarantine, direct_hits, direct_fallbacks);
    }
    printf("TKPERF_WINDOW schema=%s workload=io-uring-physical"
           " run_id=%016" PRIx64
           " cell=%s op=%s size=%zu qd=%u status=ok warmup=%u samples=%u"
           " clocks=monotonic,process-cpu path=%s oracle=%s proof=%s%s\n",
           PERF_SCHEMA, run_id, cell, operation_name(opcode), request_size,
           queue_depth, WARMUP_SAMPLES, LATENCY_SAMPLES,
           counters->thekernel ? "thekernel-physical-dma" : "linux-io-uring",
           counters->thekernel ? "thekernel-physical-counters"
                               : "linux-kernel-no-thekernel-counters",
           counters->thekernel ? "physical-dma"
                               : "linux-active/unsupported-ablation",
           measurement_physical_fields);
    printf("TKPERF_LATENCY schema=%s workload=io-uring-physical"
           " run_id=%016" PRIx64
           " cell=%s op=%s size=%zu qd=%u status=ok samples=%u"
           " wall_p50_ns=%" PRIu64 " wall_p99_ns=%" PRIu64
           " cpu_p50_ns=%" PRIu64 " cpu_p99_ns=%" PRIu64
           " path=%s oracle=%s proof=%s%s\n",
           PERF_SCHEMA, run_id, cell, operation_name(opcode), request_size,
           queue_depth, LATENCY_SAMPLES,
           quantile(samples.wall_ns, LATENCY_SAMPLES, 500U),
           quantile(samples.wall_ns, LATENCY_SAMPLES, 990U),
           quantile(samples.cpu_ns, LATENCY_SAMPLES, 500U),
           quantile(samples.cpu_ns, LATENCY_SAMPLES, 990U),
           counters->thekernel ? "thekernel-physical-dma" : "linux-io-uring",
           counters->thekernel ? "thekernel-physical-counters"
                               : "linux-kernel-no-thekernel-counters",
           counters->thekernel ? "physical-dma"
                               : "linux-active/unsupported-ablation",
           measurement_physical_fields);
    result = 0;
out:
    if (result != 0) {
        int saved_errno = errno;
        if (!correctness_emitted && environmental_unsupported_errno(saved_errno)) {
            unsupported_markers(run_id, cell, request_size, queue_depth, opcode,
                                environmental_reason(saved_errno),
                                counters->thekernel ? "thekernel-physical-dma"
                                                     : "linux-io-uring",
                                counters->thekernel ? "thekernel-physical-counters"
                                                     : "linux-kernel-no-thekernel-counters");
            result = 2;
        } else {
            error_message("cell");
        }
    }
    ring_cleanup(&ring);
    for (unsigned int shard = 0; shard < file_count; ++shard) {
        if (files[shard] >= 0) {
            close(files[shard]);
        }
        if (paths[shard][0] != '\0') {
            if (fragmented) {
                (void)remove_fragmenter(paths[shard]);
            }
            unlink(paths[shard]);
        }
    }
    munmap(buffer, total);
    return result;
}

int main(int argc, char **argv)
{
    const char *data_directory = NULL;
    const char *data_device = NULL;
    struct data_mount data_proof;
    if (argc != 5 || strcmp(argv[1], "--data-dir") != 0 ||
        strcmp(argv[3], "--data-device") != 0 || argv[2][0] == '\0' ||
        argv[4][0] == '\0') {
        errno = EINVAL;
        error_message("arguments");
        return EXIT_FAILURE;
    }
    data_directory = argv[2];
    data_device = argv[4];
    if (verify_data_mount(data_directory, data_device, &data_proof) != 0) {
        error_message("data-mount");
        return EXIT_FAILURE;
    }
    long system_page = sysconf(_SC_PAGESIZE);
    if (system_page != PAGE_BYTES) {
        errno = EINVAL;
        error_message("page-size");
        return EXIT_FAILURE;
    }
    page_size = (int)system_page;
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    struct io_counter_state counters;
    if (detect_counters(&counters) != 0) {
        error_message("counter-control");
        return EXIT_FAILURE;
    }
    uint64_t run_id = make_run_id();
    unsigned int cells = SIZE_COUNT * QD_COUNT * OP_COUNT;
    printf("TKPERF_RUN schema=%s workload=io-uring-physical"
           " run_id=%016" PRIx64 " cells=%u sizes=4096,65536,262144"
           " qd=1,8,32 ops=read_fixed,write_fixed"
           " clocks=monotonic,process-cpu path=%s oracle=%s\n",
           PERF_SCHEMA, run_id, cells,
           counters.thekernel ? "thekernel-physical-dma" : "linux-io-uring",
           counters.thekernel ? "thekernel-physical-counters"
                              : "linux-kernel-no-thekernel-counters");
    emit_data_mount(run_id, &data_proof);

    int ring_available = 1;
    struct ring probe;
    if (ring_setup(&probe) != 0) {
        ring_available = 0;
    } else {
        ring_cleanup(&probe);
    }

    unsigned int unsupported_count = 0;
    for (unsigned int size_index = 0; size_index < SIZE_COUNT; ++size_index) {
        for (unsigned int op_index = 0; op_index < OP_COUNT; ++op_index) {
            uint8_t opcode = op_index == 0U ? IORING_OP_READ_FIXED
                                            : IORING_OP_WRITE_FIXED;
            for (unsigned int qd_index = 0; qd_index < QD_COUNT; ++qd_index) {
                size_t request_size = REQUEST_SIZES[size_index];
                unsigned int queue_depth = QUEUE_DEPTHS[qd_index];
                char cell[96];
                int written = snprintf(cell, sizeof(cell), "%s_size%u_qd%u",
                                       operation_name(opcode),
                                       (unsigned int)request_size, queue_depth);
                if (written < 0 || (size_t)written >= sizeof(cell)) {
                    errno = ENAMETOOLONG;
                    error_message("cell-name");
                    return EXIT_FAILURE;
                }
                const char *reason = NULL;
                if (!ring_available) {
                    reason = "io-uring-unavailable";
                }
                if (reason != NULL) {
                    unsupported_markers(run_id, cell, request_size, queue_depth,
                                        opcode, reason,
                                        counters.thekernel ? "thekernel-physical-dma"
                                                           : "linux-io-uring",
                                        counters.thekernel ? "thekernel-physical-counters"
                                                           : "linux-kernel-no-thekernel-counters");
                    ++unsupported_count;
                    continue;
                }
                int cell_result = run_cell(run_id, data_directory, &counters,
                                           request_size,
                                           queue_depth,
                                           opcode, cell);
                if (cell_result == 2) {
                    ++unsupported_count;
                    continue;
                }
                if (cell_result != 0) {
                    (void)disable_counters(&counters);
                    return EXIT_FAILURE;
                }
            }
        }
    }
    if (disable_counters(&counters) != 0) {
        error_message("counter-disable");
        return EXIT_FAILURE;
    }
    printf("TKPERF_DONE schema=%s workload=io-uring-physical"
           " run_id=%016" PRIx64 " status=%s cells=%u unsupported=%u\n",
           PERF_SCHEMA, run_id,
           unsupported_count == cells ? "unsupported" : "ok", cells,
           unsupported_count);
    return EXIT_SUCCESS;
}
