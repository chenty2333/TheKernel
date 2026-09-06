#define _GNU_SOURCE

/*
 * Portable io_uring/O_DIRECT differential helper.
 *
 * This is deliberately a single, dependency-free C translation unit.  The
 * host runner sets THEKERNEL_PORTABLE_HOST=1; the same source is installed in
 * a TheKernel rootfs and can be run without that environment.  The helper reports
 * the Linux-observable CQE result and buffer contents, rather than treating a
 * submitted request as proof that the physical-DMA path was used.
 */

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/vfs.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

#ifndef O_DIRECT
#define O_DIRECT 040000
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

#define IORING_OFF_SQ_RING 0ULL
#define IORING_OFF_CQ_RING 0x08000000ULL
#define IORING_OFF_SQES 0x10000000ULL
#define IORING_ENTER_GETEVENTS (1U << 0)
#define IORING_REGISTER_BUFFERS 0U
#define IORING_UNREGISTER_BUFFERS 1U
#define IORING_OP_READ_FIXED 4U
#define IORING_OP_WRITE_FIXED 5U
#define RING_ENTRIES 8U
#define DIRECT_BLOCK 512U
#define BUFFER_PAGES 2U
#define WAIT_LOOPS 2000U
#define FRAGMENT_GAP_BLOCKS 4096U

/* Keep this helper independent of the build host's linux/fiemap.h package.
 * These are the stable UAPI layouts used by FS_IOC_FIEMAP. */
#define TK_FIEMAP_FLAG_SYNC 0x00000001U
#define TK_FIEMAP_EXTENT_LAST 0x00000001U
struct tk_fiemap_extent {
    uint64_t logical;
    uint64_t physical;
    uint64_t length;
    uint64_t reserved64[2];
    uint32_t flags;
    uint32_t reserved[3];
};

struct tk_fiemap {
    uint64_t start;
    uint64_t length;
    uint32_t flags;
    uint32_t mapped_extents;
    uint32_t extent_count;
    uint32_t reserved;
    struct tk_fiemap_extent extents[];
};

#define TK_FS_IOC_FIEMAP \
    _IOWR('f', 11, struct tk_fiemap)

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
};

static int linux_host;
static size_t page_bytes;

_Static_assert(sizeof(struct io_sqring_offsets) == 40, "bad SQ offsets ABI");
_Static_assert(sizeof(struct io_cqring_offsets) == 40, "bad CQ offsets ABI");
_Static_assert(sizeof(struct io_uring_params) == 120, "bad params ABI");
_Static_assert(sizeof(struct io_uring_cqe) == 16, "bad CQE ABI");
_Static_assert(sizeof(struct raw_sqe) == 64, "bad SQE ABI");

static int fail_errno(const char *stage) {
    int saved_errno = errno;
    fprintf(stderr,
            "THEKERNEL_IO_URING_DIRECTIO_FAIL stage=%s errno=%d (%s)\n",
            stage, saved_errno, strerror(saved_errno));
    errno = saved_errno;
    return 1;
}

static int fail_result(const char *stage, int32_t actual, int32_t expected) {
    fprintf(stderr,
            "THEKERNEL_IO_URING_DIRECTIO_FAIL stage=%s actual=%d expected=%d errno=%d (%s)\n",
            stage, actual, expected, errno, strerror(errno));
    return 1;
}

static int fail_observation(const char *stage, int32_t actual,
                            uint32_t request_length) {
    fprintf(stderr,
            "THEKERNEL_IO_URING_DIRECTIO_FAIL stage=%s actual=%d expected=-EINVAL-or-%u errno=%d (%s)\n",
            stage, actual, request_length, errno, strerror(errno));
    return 1;
}

static size_t page_round(size_t value) {
    if (page_bytes == 0 || value > SIZE_MAX - page_bytes + 1) {
        return 0;
    }
    return (value + page_bytes - 1) & ~(page_bytes - 1);
}

static uint32_t load_u32(const unsigned char *base, uint32_t offset) {
    const _Atomic uint32_t *word =
        (const _Atomic uint32_t *)(const void *)(base + offset);
    return atomic_load_explicit(word, memory_order_acquire);
}

static void store_u32(unsigned char *base, uint32_t offset, uint32_t value) {
    _Atomic uint32_t *word = (_Atomic uint32_t *)(void *)(base + offset);
    atomic_store_explicit(word, value, memory_order_release);
}

static void write_u16(unsigned char *bytes, size_t offset, uint16_t value) {
    memcpy(bytes + offset, &value, sizeof(value));
}

static void write_u32(unsigned char *bytes, size_t offset, uint32_t value) {
    memcpy(bytes + offset, &value, sizeof(value));
}

static void write_u64(unsigned char *bytes, size_t offset, uint64_t value) {
    memcpy(bytes + offset, &value, sizeof(value));
}

static void ring_unmap(struct ring *ring) {
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

static void ring_cleanup(struct ring *ring) {
    if (ring->fd >= 0) {
        close(ring->fd);
        ring->fd = -1;
    }
    ring_unmap(ring);
}

static int ring_setup(struct ring *ring) {
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

static int register_buffers(struct ring *ring, struct iovec *iovecs,
                            unsigned int count) {
    errno = 0;
    long result = syscall(SYS_io_uring_register, ring->fd,
                          IORING_REGISTER_BUFFERS, iovecs, count);
    return result == 0 ? 0 : -1;
}

static int unregister_buffer(struct ring *ring) {
    errno = 0;
    long result = syscall(SYS_io_uring_register, ring->fd,
                          IORING_UNREGISTER_BUFFERS, NULL, 0U);
    return result == 0 ? 0 : -1;
}

static int queue_fixed_flags(struct ring *ring, int fd, uint64_t offset,
                             uintptr_t address, uint32_t length, uint16_t slot,
                             uint64_t user_data, uint8_t opcode, uint32_t rw_flags) {
    uint32_t head = load_u32(ring->sq_ring, ring->params.sq_off.head);
    uint32_t tail = load_u32(ring->sq_ring, ring->params.sq_off.tail);
    if (tail - head >= ring->params.sq_entries) {
        errno = EBUSY;
        return -1;
    }
    uint32_t index = tail & load_u32(ring->sq_ring,
                                     ring->params.sq_off.ring_mask);
    struct raw_sqe *sqe = &ring->sqes[index];
    memset(sqe, 0, sizeof(*sqe));
    sqe->bytes[0] = opcode;
    write_u32(sqe->bytes, 4, (uint32_t)fd);
    write_u64(sqe->bytes, 8, offset);
    write_u64(sqe->bytes, 16, (uint64_t)address);
    write_u32(sqe->bytes, 24, length);
    write_u32(sqe->bytes, 28, rw_flags);
    write_u64(sqe->bytes, 32, user_data);
    write_u16(sqe->bytes, 40, slot);
    store_u32(ring->sq_ring, ring->params.sq_off.array +
              index * sizeof(uint32_t), index);
    store_u32(ring->sq_ring, ring->params.sq_off.tail, tail + 1);
    return 0;
}

static int queue_fixed(struct ring *ring, int fd, uint64_t offset,
                       uintptr_t address, uint32_t length, uint16_t slot,
                       uint64_t user_data, uint8_t opcode) {
    return queue_fixed_flags(ring, fd, offset, address, length, slot,
                             user_data, opcode, 0);
}

static int enter_and_wait(struct ring *ring, uint32_t to_submit) {
    errno = 0;
    long submitted = syscall(SYS_io_uring_enter, ring->fd, to_submit, 1U,
                             IORING_ENTER_GETEVENTS, NULL, 0U);
    if (submitted < 0) {
        return -1;
    }
    if (to_submit != 0 && submitted != (long)to_submit) {
        errno = EIO;
        return -1;
    }
    return 0;
}

static int enter_admitted(struct ring *ring, uint32_t to_submit,
                          const char *stage) {
    errno = 0;
    long submitted = syscall(SYS_io_uring_enter, ring->fd, to_submit, 0U,
                             0U, NULL, 0U);
    if (submitted < 0) {
        return fail_errno(stage);
    }
    if (submitted != (long)to_submit) {
        errno = EIO;
        return fail_result(stage, (int32_t)submitted, (int32_t)to_submit);
    }
    return 0;
}

static int completion_ring_empty(const struct ring *ring) {
    uint32_t head = load_u32(ring->cq_ring, ring->params.cq_off.head);
    uint32_t tail = load_u32(ring->cq_ring, ring->params.cq_off.tail);
    return head == tail;
}

static const struct io_uring_cqe *next_cqe(const struct ring *ring,
                                           uint32_t head) {
    uint32_t index = head & load_u32(ring->cq_ring,
                                     ring->params.cq_off.ring_mask);
    return (const struct io_uring_cqe *)(const void *)
        (ring->cq_ring + ring->params.cq_off.cqes +
         index * sizeof(struct io_uring_cqe));
}

static int wait_result(struct ring *ring, uint64_t user_data,
                       int32_t *result, const char *stage) {
    for (unsigned int attempt = 0; attempt < WAIT_LOOPS; ++attempt) {
        uint32_t head = load_u32(ring->cq_ring, ring->params.cq_off.head);
        uint32_t tail = load_u32(ring->cq_ring, ring->params.cq_off.tail);
        if (tail != head) {
            const struct io_uring_cqe *cqe = next_cqe(ring, head);
            if (cqe->user_data != user_data || cqe->flags != 0) {
                errno = EIO;
                return fail_errno(stage);
            }
            *result = cqe->res;
            store_u32(ring->cq_ring, ring->params.cq_off.head, head + 1);
            return 0;
        }
        if (ring->fd < 0) {
            struct timespec delay = {
                .tv_sec = 0,
                .tv_nsec = 1000000L,
            };
            (void)nanosleep(&delay, NULL);
            continue;
        }
        struct pollfd descriptor = {
            .fd = ring->fd,
            .events = POLLIN,
            .revents = 0,
        };
        int polled = poll(&descriptor, 1, 1);
        if (polled < 0 && errno != EINTR) {
            return fail_errno(stage);
        }
        if (polled > 0 && enter_and_wait(ring, 0) != 0 && errno != EINTR) {
            return fail_errno(stage);
        }
    }
    errno = ETIMEDOUT;
    return fail_errno(stage);
}

static int run_fixed(struct ring *ring, int fd, uint64_t offset,
                     void *address, uint32_t length, uint16_t slot,
                     uint64_t user_data, uint8_t opcode, int32_t *actual,
                     const char *stage) {
    if (queue_fixed(ring, fd, offset, (uintptr_t)address, length, slot,
                    user_data, opcode) != 0) {
        return fail_errno(stage);
    }
    if (enter_and_wait(ring, 1U) != 0) {
        return fail_errno(stage);
    }
    if (wait_result(ring, user_data, actual, stage) != 0) {
        return 1;
    }
    return 0;
}

static int submit_fixed(struct ring *ring, int fd, uint64_t offset,
                        void *address, uint32_t length, uint16_t slot,
                        uint64_t user_data, uint8_t opcode, int32_t expected,
                        const char *stage) {
    int32_t actual = 0;
    if (run_fixed(ring, fd, offset, address, length, slot, user_data, opcode,
                  &actual, stage) != 0) {
        return 1;
    }
    if (actual != expected) {
        return fail_result(stage, actual, expected);
    }
    return 0;
}

static int write_full_at(int fd, const void *data, size_t length, off_t offset) {
    const unsigned char *bytes = data;
    size_t written = 0;
    while (written < length) {
        ssize_t result = pwrite(fd, bytes + written, length - written,
                                offset + (off_t)written);
        if (result < 0 && errno == EINTR) {
            continue;
        }
        if (result <= 0) {
            return -1;
        }
        written += (size_t)result;
    }
    return 0;
}

static int write_full(int fd, const void *data, size_t length) {
    const unsigned char *bytes = data;
    size_t written = 0;
    while (written < length) {
        ssize_t result = write(fd, bytes + written, length - written);
        if (result < 0 && errno == EINTR) {
            continue;
        }
        if (result <= 0) {
            return -1;
        }
        written += (size_t)result;
    }
    return 0;
}

static int check_fill(const unsigned char *buffer, size_t length,
                      unsigned char value, const char *stage) {
    for (size_t index = 0; index < length; ++index) {
        if (buffer[index] != value) {
            errno = EIO;
            return fail_result(stage, buffer[index], value);
        }
    }
    return 0;
}

static int check_zero(const unsigned char *buffer, size_t length,
                      const char *stage) {
    return check_fill(buffer, length, 0, stage);
}

/* iomap direct-I/O short reads may preserve destination bytes after the CQE
 * length or clear the aligned tail.  Accept only those two uniform Linux-
 * visible states; mixed data is not a valid short-read oracle. */
static int check_short_read_tail(const unsigned char *buffer, size_t length,
                                 const char **behavior, const char *stage) {
    int preserved = 1;
    int zeroed = 1;
    size_t first_mismatch = 0;
    unsigned char mismatch_value = 0;
    for (size_t index = 0; index < length; ++index) {
        unsigned char value = buffer[index];
        if (value != 0xcc && preserved) {
            first_mismatch = index;
            mismatch_value = value;
            preserved = 0;
        }
        if (value != 0) {
            zeroed = 0;
        }
    }
    if (preserved) {
        *behavior = "preserved";
        return 0;
    }
    if (zeroed) {
        *behavior = "zeroed";
        return 0;
    }
    errno = EIO;
    fprintf(stderr,
            "THEKERNEL_IO_URING_DIRECTIO_FAIL stage=%s first_mismatch=%zu "
            "actual=0x%02x expected=uniform-0xcc-or-uniform-0 errno=%d (%s)\n",
            stage, first_mismatch, mismatch_value, errno, strerror(errno));
    return 1;
}

static void print_boundary(const char *name, int32_t result, int expected) {
    fprintf(stderr,
            "io_uring_directio: linux_host=%d %s result=%d expected=%d\n",
            linux_host, name, result, expected);
}

static void print_observation(const char *name, int32_t result) {
    fprintf(stderr,
            "io_uring_directio: linux_host=%d %s result=%d observation=filesystem-dependent\n",
            linux_host, name, result);
}

static int alignment_observation(int32_t result, uint32_t request_length,
                                 const char *stage) {
    /* Linux permits a filesystem to reject this with EINVAL or service it via
     * an O_DIRECT fallback.  Record both observations; neither outcome is a
     * portable cross-filesystem oracle for the guest. */
    if (result != -EINVAL && result != (int32_t)request_length) {
        return fail_observation(stage, result, request_length);
    }
    return 0;
}

static int test_alignment(struct ring *ring, int file, unsigned char *buffer) {
    int32_t address_result = 0;
    int32_t length_result = 0;
    int32_t offset_result = 0;
    memset(buffer, 0xa5, page_bytes);
    if (run_fixed(ring, file, 0, buffer + 1, DIRECT_BLOCK, 0,
                  0x414444525f424144ULL, IORING_OP_READ_FIXED,
                  &address_result, "alignment-address") != 0 ||
        run_fixed(ring, file, 0, buffer, DIRECT_BLOCK + 1U, 0,
                  0x4c454e4754485f42ULL, IORING_OP_READ_FIXED,
                  &length_result, "alignment-length") != 0 ||
        run_fixed(ring, file, 1, buffer, DIRECT_BLOCK, 0,
                  0x4f46465345545f42ULL, IORING_OP_READ_FIXED,
                  &offset_result, "alignment-offset") != 0 ||
        alignment_observation(address_result, DIRECT_BLOCK,
                              "alignment-address-observation") != 0 ||
        alignment_observation(length_result, DIRECT_BLOCK + 1U,
                              "alignment-length-observation") != 0 ||
        alignment_observation(offset_result, DIRECT_BLOCK,
                              "alignment-offset-observation") != 0) {
        return 1;
    }
    print_observation("alignment_address", address_result);
    print_observation("alignment_length", length_result);
    print_observation("alignment_offset", offset_result);
    puts("THEKERNEL_IO_URING_DIRECTIO_ALIGNMENT_OK");
    return 0;
}

static int test_eof_and_short(struct ring *ring, int file,
                              unsigned char *buffer, uint64_t file_size) {
    memset(buffer, 0xa5, page_bytes);
    if (submit_fixed(ring, file, file_size, buffer, DIRECT_BLOCK, 0,
                     0x454f465f4551ULL, IORING_OP_READ_FIXED, 0,
                     "eof-exact") != 0 ||
        check_fill(buffer, DIRECT_BLOCK, 0xa5, "eof-exact-buffer") != 0) {
        return 1;
    }
    memset(buffer, 0xa5, page_bytes);
    if (submit_fixed(ring, file, file_size + DIRECT_BLOCK, buffer,
                     DIRECT_BLOCK, 0, 0x454f465f50415354ULL,
                     IORING_OP_READ_FIXED, 0, "eof-past") != 0 ||
        check_fill(buffer, DIRECT_BLOCK, 0xa5, "eof-past-buffer") != 0) {
        return 1;
    }
    print_boundary("eof_exact", 0, 0);
    print_boundary("eof_past", 0, 0);
    puts("THEKERNEL_IO_URING_DIRECTIO_EOF_OK");

    memset(buffer, 0xcc, page_bytes);
    int32_t short_result = 0;
    if (queue_fixed(ring, file, file_size - DIRECT_BLOCK, (uintptr_t)buffer,
                    (uint32_t)page_bytes, 0, 0x53484f52545f5244ULL,
                    IORING_OP_READ_FIXED) != 0 ||
        enter_and_wait(ring, 1U) != 0 ||
        wait_result(ring, 0x53484f52545f5244ULL, &short_result,
                    "short-read") != 0) {
        return 1;
    }
    if (short_result != (int32_t)DIRECT_BLOCK) {
        return fail_result("short-read", short_result, DIRECT_BLOCK);
    }
    if (check_fill(buffer, DIRECT_BLOCK, 'C', "short-read-content") != 0) {
        return 1;
    }
    const char *tail_behavior = NULL;
    if (check_short_read_tail(buffer + DIRECT_BLOCK,
                              page_bytes - DIRECT_BLOCK, &tail_behavior,
                              "short-read-tail") != 0) {
        return 1;
    }
    print_boundary("short_read", short_result, DIRECT_BLOCK);
    fprintf(stderr,
            "io_uring_directio: linux_host=%d short_read_tail=%s bytes=%zu\n",
            linux_host, tail_behavior, page_bytes - DIRECT_BLOCK);
    puts("THEKERNEL_IO_URING_DIRECTIO_SHORT_READ_TAIL_CHECKED");
    puts("THEKERNEL_IO_URING_DIRECTIO_SHORT_READ_OK");
    return 0;
}

static int observe_fragmented_extent(int file) {
    const uint32_t extent_count = 32U;
    const size_t bytes = sizeof(struct tk_fiemap) +
                         extent_count * sizeof(struct tk_fiemap_extent);
    struct tk_fiemap *map = calloc(1, bytes);
    if (map == NULL) {
        errno = ENOMEM;
        return fail_errno("fragmented-fiemap-alloc");
    }
    map->start = 0;
    map->length = UINT64_MAX;
    map->flags = TK_FIEMAP_FLAG_SYNC;
    map->extent_count = extent_count;
    errno = 0;
    int result = ioctl(file, TK_FS_IOC_FIEMAP, map);
    int saved_errno = errno;
    if (result != 0) {
        if (saved_errno == ENOTTY || saved_errno == EOPNOTSUPP ||
            saved_errno == ENOSYS || saved_errno == EINVAL) {
            puts("THEKERNEL_IO_URING_DIRECTIO_FRAGMENTED_EXTENT_CHECKED");
            puts("THEKERNEL_IO_URING_DIRECTIO_FRAGMENTED_EXTENT_UNSUPPORTED");
            free(map);
            return 0;
        }
        errno = saved_errno;
        free(map);
        return fail_errno("fragmented-fiemap");
    }
    if (map->mapped_extents > map->extent_count ||
        map->mapped_extents > extent_count) {
        errno = EPROTO;
        int failure = fail_result("fragmented-fiemap-count",
                                  (int32_t)map->mapped_extents,
                                  (int32_t)map->extent_count);
        free(map);
        return failure;
    }
    int has_logical_gap = 0;
    int physical_segments_distinct = 0;
    int physical_extents_valid = 1;
    for (uint32_t index = 1; index < map->mapped_extents; ++index) {
        const struct tk_fiemap_extent *previous = &map->extents[index - 1U];
        const struct tk_fiemap_extent *current = &map->extents[index];
        if (previous->logical <= UINT64_MAX - previous->length &&
            previous->logical + previous->length < current->logical) {
            has_logical_gap = 1;
        }
        if (previous->physical != current->physical)
            physical_segments_distinct = 1;
    }
    for (uint32_t index = 0; index < map->mapped_extents; ++index) {
        const struct tk_fiemap_extent *extent = &map->extents[index];
        if (extent->length == 0 ||
            extent->physical > UINT64_MAX - extent->length) {
            physical_extents_valid = 0;
            break;
        }
    }
    if (map->mapped_extents < 2U || !has_logical_gap ||
        !physical_segments_distinct || !physical_extents_valid) {
        errno = EPROTO;
        int failure = fail_result("fragmented-fiemap-count",
                                  (int32_t)map->mapped_extents, 2);
        free(map);
        return failure;
    }
    fprintf(stderr,
            "io_uring_directio: linux_host=%d fragmented_fiemap_extents=%u physical_sg_segments=%u\n",
            linux_host, map->mapped_extents, map->mapped_extents);
    puts("THEKERNEL_IO_URING_DIRECTIO_FRAGMENTED_EXTENT_CHECKED");
    puts("THEKERNEL_IO_URING_DIRECTIO_FRAGMENTED_EXTENT_PHYSICAL_SG_OK");
    free(map);
    return 0;
}

static int test_sparse_and_fragmented(struct ring *ring, int file,
                                      unsigned char *buffer) {
    memset(buffer, 0xcc, page_bytes);
    if (submit_fixed(ring, file, DIRECT_BLOCK, buffer, DIRECT_BLOCK, 0,
                     0x5350415253455f48ULL, IORING_OP_READ_FIXED, DIRECT_BLOCK,
                     "sparse-hole") != 0 ||
        check_zero(buffer, DIRECT_BLOCK, "sparse-hole-content") != 0) {
        return 1;
    }
    puts("THEKERNEL_IO_URING_DIRECTIO_SPARSE_HOLE_OK");

    /* Two written blocks with a large logical hole force separate logical
     * regions.  The FIEMAP oracle below is the only path that can authorize a
     * physical-SG claim; content equality alone is deliberately insufficient. */
    memset(buffer, 0xcc, page_bytes);
    if (submit_fixed(ring, file, 0, buffer, DIRECT_BLOCK, 0,
                     0x465241475f464952ULL, IORING_OP_READ_FIXED, DIRECT_BLOCK,
                     "fragmented-first") != 0 ||
        check_fill(buffer, DIRECT_BLOCK, 'A', "fragmented-first-content") != 0) {
        return 1;
    }
    memset(buffer, 0xcc, page_bytes);
    if (submit_fixed(ring, file, FRAGMENT_GAP_BLOCKS * DIRECT_BLOCK, buffer,
                     DIRECT_BLOCK, 0,
                     0x465241475f4c4153ULL, IORING_OP_READ_FIXED, DIRECT_BLOCK,
                     "fragmented-last") != 0 ||
        check_fill(buffer, DIRECT_BLOCK, 'B', "fragmented-last-content") != 0) {
        return 1;
    }
    return observe_fragmented_extent(file);
}

static int test_write_fixed(struct ring *ring, int file,
                            unsigned char *buffer) {
    const uint64_t offset = 2U * DIRECT_BLOCK;
    memset(buffer, 'D', DIRECT_BLOCK);
    int32_t actual = 0;
    if (run_fixed(ring, file, offset, buffer, DIRECT_BLOCK, 0,
                  0x57524954455f4649ULL, IORING_OP_WRITE_FIXED, &actual,
                  "write-fixed") != 0 || actual != (int32_t)DIRECT_BLOCK) {
        return fail_result("write-fixed", actual, DIRECT_BLOCK);
    }
    if (fsync(file) != 0) {
        return fail_errno("write-fixed-fsync");
    }
    memset(buffer, 0xcc, page_bytes);
    if (submit_fixed(ring, file, offset, buffer, DIRECT_BLOCK, 0,
                     0x57524954455f5244ULL, IORING_OP_READ_FIXED,
                     DIRECT_BLOCK, "write-fixed-read") != 0 ||
        check_fill(buffer, DIRECT_BLOCK, 'D', "write-fixed-content") != 0) {
        return 1;
    }
    print_boundary("write_fixed", actual, DIRECT_BLOCK);
    puts("THEKERNEL_IO_URING_DIRECTIO_WRITE_FIXED_OK");
    return 0;
}

static int test_registered_range(struct ring *ring, int file,
                                 unsigned char *buffer) {
    unsigned char *slot0 = buffer;
    unsigned char *slot1 = buffer + page_bytes;
    const size_t subrange_offset = DIRECT_BLOCK;
    const uint32_t subrange_length = DIRECT_BLOCK;
    const size_t crossing_offset = page_bytes - DIRECT_BLOCK;
    const uint32_t crossing_length = 2U * DIRECT_BLOCK;

    /* The first request is aligned for the fixture's O_DIRECT geometry but
     * starts inside slot 0.  Only that fixed-buffer subrange may change. */
    memset(slot0, 0x5a, page_bytes);
    memset(slot1, 0x6b, page_bytes);
    if (submit_fixed(ring, file, 0, slot0 + subrange_offset,
                     subrange_length, 0, 0x53554252414e4745ULL,
                     IORING_OP_READ_FIXED, (int32_t)subrange_length,
                     "fixed-subrange") != 0 ||
        check_fill(slot0, subrange_offset, 0x5a,
                   "fixed-subrange-prefix") != 0 ||
        check_fill(slot0 + subrange_offset + subrange_length,
                   page_bytes - subrange_offset - subrange_length, 0x5a,
                   "fixed-subrange-suffix") != 0 ||
        check_fill(slot1, page_bytes, 0x6b, "fixed-subrange-slot1") != 0 ||
        check_fill(slot0 + subrange_offset, subrange_length, 'A',
                   "fixed-subrange-content") != 0) {
        return 1;
    }
    print_boundary("fixed_subrange", (int32_t)subrange_length,
                   (int32_t)subrange_length);
    puts("THEKERNEL_IO_URING_DIRECTIO_FIXED_SUBRANGE_OK");

    /* Keep slot 1 mapped and registered, but ask slot 0 to cover bytes past
     * its own iovec.  io_uring must reject the fixed-buffer range in the CQE;
     * the submission itself is valid and must not fail at io_uring_enter. */
    memset(slot0, 0x3c, page_bytes);
    memset(slot1, 0x7d, page_bytes);
    if (queue_fixed(ring, file, 0, (uintptr_t)(slot0 + crossing_offset),
                    crossing_length, 0, 0x52414e47455f4546ULL,
                    IORING_OP_READ_FIXED) != 0 ||
        enter_admitted(ring, 1U, "fixed-range-efault-submit") != 0) {
        return 1;
    }
    int32_t actual = 0;
    if (wait_result(ring, 0x52414e47455f4546ULL, &actual,
                    "fixed-range-efault-cqe") != 0 || actual != -EFAULT) {
        return fail_result("fixed-range-efault", actual, -EFAULT);
    }
    if (check_fill(slot0, page_bytes, 0x3c, "fixed-range-efault-slot0") != 0 ||
        check_fill(slot1, page_bytes, 0x7d, "fixed-range-efault-slot1") != 0) {
        return 1;
    }
    fprintf(stderr,
            "io_uring_directio: linux_host=%d fixed_range_efault result=%d expected=%d submission=1\n",
            linux_host, actual, -EFAULT);
    puts("THEKERNEL_IO_URING_DIRECTIO_FIXED_RANGE_EFAULT_OK");
    return 0;
}

static int test_invalid_slot(struct ring *ring, int file, unsigned char *buffer) {
    memset(buffer, 0xa5, page_bytes);
    if (submit_fixed(ring, file, 0, buffer, DIRECT_BLOCK, 2,
                     0x494e565f534c4f54ULL, IORING_OP_READ_FIXED, -EFAULT,
                     "invalid-fixed-slot") != 0) {
        return 1;
    }
    print_boundary("invalid_fixed_slot", -EFAULT, -EFAULT);
    puts("THEKERNEL_IO_URING_DIRECTIO_INVALID_FIXED_SLOT_OK");
    return 0;
}

static int test_nowait_direct(struct ring *ring, int file, unsigned char *buffer) {
    const uint64_t user_data = 0x4e4f57414954ULL;
    const uint32_t rwf_nowait = 8U;
    memset(buffer, 0xa5, DIRECT_BLOCK);
    if (queue_fixed_flags(ring, file, 0, (uintptr_t)buffer, DIRECT_BLOCK, 0,
                          user_data, IORING_OP_READ_FIXED, rwf_nowait) != 0 ||
        enter_admitted(ring, 1U, "nowait-direct-enter") != 0) {
        return fail_errno("nowait-direct-queue");
    }
    int32_t actual = 0;
    if (wait_result(ring, user_data, &actual, "nowait-direct-cqe") != 0) {
        return 1;
    }
    // Linux may complete direct NOWAIT when its provider can prove immediate
    // readiness. TheKernel's physical provider has no such proof and must
    // return EAGAIN without entering its blocking writeback settlement path.
    if (actual != -EAGAIN && !(linux_host && actual == DIRECT_BLOCK)) {
        return fail_result("nowait-direct-result", actual, -EAGAIN);
    }
    if (actual == -EAGAIN &&
        check_fill(buffer, DIRECT_BLOCK, 0xa5, "nowait-direct-no-copy") != 0) {
        return 1;
    }
    puts("THEKERNEL_IO_URING_DIRECTIO_NOWAIT_OK");
    return 0;
}

static int test_unregister_admitted(struct ring *ring, int file,
                                    unsigned char *buffer) {
    /* Keep a fixed-buffer read blocked on an empty pipe.  A regular-file read
     * can retire before unregister_buffer is called, which would only test a
     * post-completion table teardown. */
    (void)file;
    int pipe_fds[2];
    if (pipe(pipe_fds) != 0) {
        return fail_errno("unregister-admitted-pipe");
    }
    if (queue_fixed(ring, pipe_fds[0], 0, (uintptr_t)buffer, DIRECT_BLOCK, 0,
                    0x554e5245475f4144ULL, IORING_OP_READ_FIXED) != 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        return fail_errno("unregister-admitted-queue");
    }
    if (enter_admitted(ring, 1U, "unregister-admitted-enter") != 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        return 1;
    }
    if (!completion_ring_empty(ring)) {
        puts("THEKERNEL_IO_URING_DIRECTIO_UNREGISTER_ADMITTED_CHECKED");
        puts("THEKERNEL_IO_URING_DIRECTIO_UNREGISTER_ADMITTED_UNSUPPORTED");
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        errno = EPROTO;
        return fail_errno("unregister-admitted-already-retired");
    }
    struct timespec delay = {
        .tv_sec = 0,
        .tv_nsec = 5 * 1000 * 1000L,
    };
    (void)nanosleep(&delay, NULL);
    if (!completion_ring_empty(ring)) {
        puts("THEKERNEL_IO_URING_DIRECTIO_UNREGISTER_ADMITTED_CHECKED");
        puts("THEKERNEL_IO_URING_DIRECTIO_UNREGISTER_ADMITTED_UNSUPPORTED");
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        errno = EPROTO;
        return fail_errno("unregister-admitted-not-outstanding");
    }
    fprintf(stderr,
            "io_uring_directio: linux_host=%d unregister_admitted_outstanding=1 cq_empty=1\n",
            linux_host);
    puts("THEKERNEL_IO_URING_DIRECTIO_UNREGISTER_ADMITTED_OUTSTANDING_OK");
    if (unregister_buffer(ring) != 0) {
        int saved_errno = errno;
        unsigned char payload[DIRECT_BLOCK];
        memset(payload, 'U', sizeof(payload));
        (void)write_full(pipe_fds[1], payload, sizeof(payload));
        int32_t retired = 0;
        (void)wait_result(ring, 0x554e5245475f4144ULL, &retired,
                          "unregister-admitted-unsupported-retirement");
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        errno = saved_errno;
        fprintf(stderr,
                "io_uring_directio: linux_host=%d unregister_admitted=unsupported errno=%d\n",
                linux_host, errno);
        puts("THEKERNEL_IO_URING_DIRECTIO_UNREGISTER_ADMITTED_CHECKED");
        puts("THEKERNEL_IO_URING_DIRECTIO_UNREGISTER_ADMITTED_UNSUPPORTED");
        return 1;
    }
    unsigned char payload[DIRECT_BLOCK];
    memset(payload, 'U', sizeof(payload));
    if (write_full(pipe_fds[1], payload, sizeof(payload)) != 0) {
        int saved_errno = errno;
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        errno = saved_errno;
        return fail_errno("unregister-admitted-release");
    }
    int32_t actual = 0;
    if (wait_result(ring, 0x554e5245475f4144ULL, &actual,
                    "unregister-admitted-cqe") != 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        return 1;
    }
    close(pipe_fds[0]);
    close(pipe_fds[1]);
    if (actual != (int32_t)DIRECT_BLOCK ||
        check_fill(buffer, DIRECT_BLOCK, 'U', "unregister-admitted-content") != 0) {
        return fail_result("unregister-admitted-cqe", actual, DIRECT_BLOCK);
    }
    puts("THEKERNEL_IO_URING_DIRECTIO_UNREGISTER_ADMITTED_CHECKED");
    fprintf(stderr,
            "io_uring_directio: linux_host=%d unregister_admitted_retired=1 unregister_errno=0\n",
            linux_host);
    print_boundary("unregister_admitted", actual, DIRECT_BLOCK);
    puts("THEKERNEL_IO_URING_DIRECTIO_UNREGISTER_ADMITTED_OK");
    return 0;
}

static int test_close_pending(struct ring *ring, int file,
                              unsigned char *buffer) {
    (void)file;
    int pipe_fds[2];
    if (pipe(pipe_fds) != 0) {
        return fail_errno("close-pending-pipe");
    }
    if (queue_fixed(ring, pipe_fds[0], 0, (uintptr_t)buffer, DIRECT_BLOCK, 0,
                    0x434c4f53455f5243ULL, IORING_OP_READ_FIXED) != 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        return fail_errno("close-pending-queue");
    }
    if (enter_admitted(ring, 1U, "close-pending-enter") != 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        return 1;
    }
    if (!completion_ring_empty(ring)) {
        errno = EPROTO;
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        return fail_errno("close-pending-not-inflight");
    }
    if (close(ring->fd) != 0) {
        int saved_errno = errno;
        ring->fd = -1;
        ring_cleanup(ring);
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        errno = saved_errno;
        return fail_errno("close-pending-close");
    }
    ring->fd = -1;
    unsigned char payload[DIRECT_BLOCK];
    memset(payload, 'P', sizeof(payload));
    if (write_full(pipe_fds[1], payload, sizeof(payload)) != 0) {
        int saved_errno = errno;
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        errno = saved_errno;
        return fail_errno("close-pending-release");
    }
    int32_t actual = 0;
    if (wait_result(ring, 0x434c4f53455f5243ULL, &actual,
                    "close-pending-cqe") != 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        return 1;
    }
    close(pipe_fds[0]);
    close(pipe_fds[1]);
    if (actual != (int32_t)DIRECT_BLOCK ||
        check_fill(buffer, DIRECT_BLOCK, 'P', "close-pending-content") != 0) {
        return fail_result("close-pending-cqe", actual, DIRECT_BLOCK);
    }
    print_boundary("close_pending", actual, DIRECT_BLOCK);
    puts("THEKERNEL_IO_URING_DIRECTIO_CLOSE_PENDING_OK");
    return 0;
}

static int open_fixture(const char *path, uint64_t *file_size) {
    int seed = open(path, O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0600);
    if (seed < 0) {
        fail_errno("fixture-create");
        return -1;
    }
    *file_size = (FRAGMENT_GAP_BLOCKS + 2U) * DIRECT_BLOCK;
    if (ftruncate(seed, (off_t)*file_size) != 0) {
        fail_errno("fixture-truncate");
        int saved_errno = errno;
        close(seed);
        errno = saved_errno;
        return -1;
    }
    unsigned char block[DIRECT_BLOCK];
    memset(block, 'A', sizeof(block));
    if (write_full_at(seed, block, sizeof(block), 0) != 0) {
        fail_errno("fixture-write-first");
        int saved_errno = errno;
        close(seed);
        errno = saved_errno;
        return -1;
    }
    memset(block, 'B', sizeof(block));
    if (write_full_at(seed, block, sizeof(block),
                      (off_t)(FRAGMENT_GAP_BLOCKS * DIRECT_BLOCK)) != 0) {
        fail_errno("fixture-write-fragment");
        int saved_errno = errno;
        close(seed);
        errno = saved_errno;
        return -1;
    }
    memset(block, 'C', sizeof(block));
    if (write_full_at(seed, block, sizeof(block),
                      (off_t)((FRAGMENT_GAP_BLOCKS + 1U) * DIRECT_BLOCK)) != 0) {
        fail_errno("fixture-write-last");
        int saved_errno = errno;
        close(seed);
        errno = saved_errno;
        return -1;
    }
    if (fsync(seed) != 0) {
        fail_errno("fixture-sync");
        int saved_errno = errno;
        close(seed);
        errno = saved_errno;
        return -1;
    }
    if (close(seed) != 0) {
        return -1;
    }
    int direct = open(path, O_RDWR | O_DIRECT | O_CLOEXEC);
    if (direct < 0) {
        fail_errno("fixture-direct-open");
    }
    if (direct >= 0) {
        struct statfs filesystem;
        int status = fcntl(direct, F_GETFL);
        if (status < 0 || fstatfs(direct, &filesystem) != 0) {
            int saved_errno = errno;
            close(direct);
            errno = saved_errno;
            return -1;
        }
        fprintf(stderr,
                "io_uring_directio: provider_magic=0x%lx direct_flag=%d "
                "path=%s physical_dma=not-observable-from-uapi\n",
                (unsigned long)filesystem.f_type, !!(status & O_DIRECT), path);
        if (!(status & O_DIRECT) ||
            (filesystem.f_type != 0xef53)) {
            close(direct);
            errno = EPROTO;
            fail_errno("fixture-provider");
            return -1;
        }
    }
    return direct;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);
    const char *host_mode = getenv("THEKERNEL_PORTABLE_HOST");
    linux_host = host_mode != NULL && host_mode[0] != '\0' &&
                 strcmp(host_mode, "0") != 0;
    if (argc == 2 && strcmp(argv[1], "--linux-host") == 0) {
        linux_host = 1;
    } else if (argc != 1) {
        errno = EINVAL;
        return fail_errno("arguments");
    }
    long system_page = sysconf(_SC_PAGESIZE);
    if (system_page != 4096) {
        errno = EINVAL;
        return fail_errno("page-size");
    }
    page_bytes = (size_t)system_page;
    const char *path = "/thekernel-io-uring-directio-differential";
    const char *path_override = getenv("THEKERNEL_DIRECTIO_PATH");
    if (linux_host && path_override != NULL && path_override[0] != '\0') {
        path = path_override;
    }
    size_t buffer_bytes = BUFFER_PAGES * page_bytes;
    unsigned char *buffer = mmap(NULL, buffer_bytes, PROT_READ | PROT_WRITE,
                                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (buffer == MAP_FAILED || ((uintptr_t)buffer % DIRECT_BLOCK) != 0) {
        if (buffer != MAP_FAILED) {
            munmap(buffer, buffer_bytes);
        }
        errno = EFAULT;
        return fail_errno("buffer-alignment");
    }
    struct ring ring;
    memset(&ring, 0, sizeof(ring));
    ring.fd = -1;
    int file = -1;
    uint64_t file_size = 0;
    struct iovec registered_iovecs[2];
    int result = 1;
    if (ring_setup(&ring) != 0) {
        result = fail_errno("ring-setup");
        goto out;
    }
    file = open_fixture(path, &file_size);
    if (file < 0) {
        result = fail_errno("fixture-open");
        goto out;
    }
    registered_iovecs[0].iov_base = buffer;
    registered_iovecs[0].iov_len = page_bytes;
    registered_iovecs[1].iov_base = buffer + page_bytes;
    registered_iovecs[1].iov_len = page_bytes;
    if (register_buffers(&ring, registered_iovecs, 2U) != 0) {
        result = fail_errno("buffer-register");
        goto out;
    }
    if (test_nowait_direct(&ring, file, buffer) != 0 ||
        test_alignment(&ring, file, buffer) != 0 ||
        test_eof_and_short(&ring, file, buffer, file_size) != 0 ||
        test_sparse_and_fragmented(&ring, file, buffer) != 0 ||
        test_write_fixed(&ring, file, buffer) != 0 ||
        test_registered_range(&ring, file, buffer) != 0 ||
        test_invalid_slot(&ring, file, buffer) != 0 ||
        test_unregister_admitted(&ring, file, buffer) != 0) {
        goto out;
    }
    if (register_buffers(&ring, registered_iovecs, 2U) != 0) {
        result = fail_errno("close-pending-reregister");
        goto out;
    }
    if (test_close_pending(&ring, file, buffer) != 0) {
        goto out;
    }
    result = 0;
out:
    if (ring.fd >= 0) {
        /* A failed earlier case may still own the registration; teardown is
         * best-effort because the test's result is reported above. */
        (void)unregister_buffer(&ring);
    }
    if (file >= 0) {
        close(file);
    }
    unlink(path);
    ring_cleanup(&ring);
    munmap(buffer, buffer_bytes);
    if (result == 0) {
        puts("THEKERNEL_IO_URING_DIRECTIO_OK");
    }
    return result;
}
