#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

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

#define IORING_SETUP_CQSIZE (1U << 3)
#define IORING_ENTER_GETEVENTS (1U << 0)

#define IORING_REGISTER_BUFFERS 0U
#define IORING_UNREGISTER_BUFFERS 1U
#define IORING_REGISTER_FILES 2U
#define IORING_UNREGISTER_FILES 3U

#define IORING_OP_READ_FIXED 4U
#define IORING_OP_WRITE_FIXED 5U

#define IORING_MAX_REGISTERED_BUFFERS (1U << 14)

#define TEST_RING_ENTRIES 8U
#define THEKERNEL_RING_BUFFER_PAGES 4096U
#define THEKERNEL_GLOBAL_BUFFER_RINGS 4U
#define STRESS_ROUNDS 50U
#define WAIT_LOOPS 2000U

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
    void *buffer;
    size_t buffer_length;
    int buffers_registered;
};

struct inflight_context {
    struct ring *ring;
    int read_fd;
    void *buffer;
    _Atomic int ready;
    int submit_result;
    int submit_errno;
};

struct unregister_context {
    struct ring *ring;
    struct inflight_context *inflight;
    long result;
    int saved_errno;
};

struct stress_shared {
    _Atomic int failed;
};

struct stress_register_context {
    struct stress_shared *shared;
    unsigned int index;
};

struct stress_fixed_context {
    struct stress_shared *shared;
    unsigned int index;
};

static int linux_host;
static size_t page_bytes;

_Static_assert(sizeof(struct io_sqring_offsets) == 40, "bad SQ offsets ABI");
_Static_assert(sizeof(struct io_cqring_offsets) == 40, "bad CQ offsets ABI");
_Static_assert(sizeof(struct io_uring_params) == 120, "bad params ABI");
_Static_assert(sizeof(struct io_uring_cqe) == 16, "bad CQE ABI");
_Static_assert(sizeof(struct raw_sqe) == 64, "bad SQE ABI");

static int fail_stage(const char *stage) {
    int saved_errno = errno;
    fprintf(stderr, "THEKERNEL_IO_URING_BUFFERS_FAIL stage=%s errno=%d (%s)\n",
            stage, saved_errno, strerror(saved_errno));
    errno = saved_errno;
    return 1;
}

static int fail_value(const char *stage, long actual, long expected) {
    fprintf(stderr,
            "THEKERNEL_IO_URING_BUFFERS_FAIL stage=%s actual=%ld expected=%ld\n",
            stage, actual, expected);
    errno = EIO;
    return 1;
}

static size_t page_round(size_t value) {
    if (page_bytes == 0 || value > SIZE_MAX - page_bytes + 1) {
        return 0;
    }
    return (value + page_bytes - 1) & ~(page_bytes - 1);
}

static size_t budget_bytes(void) {
    /* Linux has no TheKernel per-ring/global 16 MiB accounting contract. The
     * host's memlock limit is also intentionally small in CI, so host probes
     * use four one-page charges while the guest exercises the exact budget. */
    return linux_host ? page_bytes : (size_t)THEKERNEL_RING_BUFFER_PAGES * page_bytes;
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
    if (ring->buffers_registered && ring->fd >= 0) {
        syscall(SYS_io_uring_register, ring->fd, IORING_UNREGISTER_BUFFERS,
                NULL, 0U);
        ring->buffers_registered = 0;
    }
    if (ring->fd >= 0) {
        close(ring->fd);
        ring->fd = -1;
    }
    if (ring->buffer != NULL && ring->buffer != MAP_FAILED) {
        munmap(ring->buffer, ring->buffer_length);
        ring->buffer = NULL;
        ring->buffer_length = 0;
    }
    ring_unmap(ring);
}

static void ring_close_without_unregister(struct ring *ring) {
    if (ring->fd >= 0) {
        close(ring->fd);
        ring->fd = -1;
    }
    ring->buffers_registered = 0;
    if (ring->buffer != NULL && ring->buffer != MAP_FAILED) {
        munmap(ring->buffer, ring->buffer_length);
        ring->buffer = NULL;
        ring->buffer_length = 0;
    }
    ring_unmap(ring);
}

static int ring_setup(struct ring *ring) {
    memset(ring, 0, sizeof(*ring));
    ring->fd = -1;
    ring->params.flags = IORING_SETUP_CQSIZE;
    ring->params.cq_entries = TEST_RING_ENTRIES;
    ring->fd = (int)syscall(SYS_io_uring_setup, TEST_RING_ENTRIES, &ring->params);
    if (ring->fd < 0) {
        return -1;
    }

    size_t sq_end = (size_t)ring->params.sq_off.array +
                    (size_t)ring->params.sq_entries * sizeof(uint32_t);
    size_t cq_end = (size_t)ring->params.cq_off.cqes +
                    (size_t)ring->params.cq_entries * sizeof(struct io_uring_cqe);
    ring->ring_bytes = page_round(sq_end > cq_end ? sq_end : cq_end);
    ring->sqe_bytes = page_round((size_t)ring->params.sq_entries * sizeof(struct raw_sqe));
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

static void *alloc_user_buffer(size_t length) {
    void *buffer = mmap(NULL, length, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (buffer == MAP_FAILED) {
        return MAP_FAILED;
    }
    memset(buffer, 0, length);
    return buffer;
}

static void *alloc_budget_buffer(size_t length) {
    return mmap(NULL, length, PROT_READ | PROT_WRITE,
                MAP_SHARED | MAP_ANONYMOUS, -1, 0);
}

static int register_iov(struct ring *ring, struct iovec *iov, unsigned int count) {
    errno = 0;
    long result = syscall(SYS_io_uring_register, ring->fd,
                          IORING_REGISTER_BUFFERS, iov, count);
    if (result != 0) {
        return -1;
    }
    ring->buffers_registered = 1;
    return 0;
}

static int unregister_buffers(struct ring *ring) {
    errno = 0;
    long result = syscall(SYS_io_uring_register, ring->fd,
                          IORING_UNREGISTER_BUFFERS, NULL, 0U);
    if (result != 0) {
        return -1;
    }
    ring->buffers_registered = 0;
    return 0;
}

static int observe_completions(struct ring *ring, const char *stage) {
    errno = 0;
    if (syscall(SYS_io_uring_enter, ring->fd, 0U, 0U, 0U, NULL, 0U) < 0) {
        return fail_stage(stage);
    }
    return 0;
}

static int expect_register_errno(struct ring *ring, uint32_t opcode,
                                 const void *argument, uint32_t count,
                                 int expected_errno, const char *stage) {
    errno = 0;
    long result = syscall(SYS_io_uring_register, ring->fd, opcode,
                          argument, count);
    if (result != -1 || errno != expected_errno) {
        if (result == 0) {
            syscall(SYS_io_uring_register, ring->fd,
                    IORING_UNREGISTER_BUFFERS, NULL, 0U);
        }
        if (result >= 0) {
            return fail_value(stage, result, -1);
        }
        return fail_value(stage, errno, expected_errno);
    }
    return 0;
}

static int queue_fixed(struct ring *ring, uint8_t opcode, int fd,
                       uint64_t offset, uintptr_t address, uint32_t length,
                       uint16_t slot, uint64_t user_data) {
    uint32_t head = load_u32(ring->sq_ring, ring->params.sq_off.head);
    uint32_t tail = load_u32(ring->sq_ring, ring->params.sq_off.tail);
    if (tail - head >= ring->params.sq_entries) {
        errno = EBUSY;
        return -1;
    }
    uint32_t index = tail & load_u32(ring->sq_ring, ring->params.sq_off.ring_mask);
    struct raw_sqe *sqe = &ring->sqes[index];
    memset(sqe, 0, sizeof(*sqe));
    sqe->bytes[0] = opcode;
    write_u32(sqe->bytes, 4, (uint32_t)fd);
    write_u64(sqe->bytes, 8, offset);
    write_u64(sqe->bytes, 16, (uint64_t)address);
    write_u32(sqe->bytes, 24, length);
    write_u16(sqe->bytes, 40, slot);
    write_u64(sqe->bytes, 32, user_data);
    store_u32(ring->sq_ring, ring->params.sq_off.array + index * sizeof(uint32_t),
              index);
    store_u32(ring->sq_ring, ring->params.sq_off.tail, tail + 1);
    return 0;
}

static const struct io_uring_cqe *next_cqe(const struct ring *ring, uint32_t head) {
    uint32_t index = head & load_u32(ring->cq_ring, ring->params.cq_off.ring_mask);
    return (const struct io_uring_cqe *)(const void *)
        (ring->cq_ring + ring->params.cq_off.cqes +
         index * sizeof(struct io_uring_cqe));
}

static int consume_cqe(struct ring *ring, uint64_t user_data,
                       int32_t expected_result, const char *stage) {
    uint32_t head = load_u32(ring->cq_ring, ring->params.cq_off.head);
    uint32_t tail = load_u32(ring->cq_ring, ring->params.cq_off.tail);
    if (tail == head) {
        errno = EAGAIN;
        return fail_stage(stage);
    }
    const struct io_uring_cqe *cqe = next_cqe(ring, head);
    if (cqe->user_data != user_data || cqe->res != expected_result ||
        cqe->flags != 0) {
        return fail_value(stage, cqe->res, expected_result);
    }
    store_u32(ring->cq_ring, ring->params.cq_off.head, head + 1);
    return 0;
}

static int wait_cqe(struct ring *ring, uint64_t user_data,
                    int32_t expected_result, const char *stage) {
    for (unsigned int attempt = 0; attempt < WAIT_LOOPS; ++attempt) {
        uint32_t head = load_u32(ring->cq_ring, ring->params.cq_off.head);
        uint32_t tail = load_u32(ring->cq_ring, ring->params.cq_off.tail);
        if (tail != head) {
            return consume_cqe(ring, user_data, expected_result, stage);
        }

        struct pollfd descriptor = {.fd = ring->fd, .events = POLLIN, .revents = 0};
        int polled = poll(&descriptor, 1, 1);
        if (polled < 0) {
            if (errno == EINTR) {
                continue;
            }
            return fail_stage(stage);
        }
        if (polled > 0) {
            errno = 0;
            long waited = syscall(SYS_io_uring_enter, ring->fd, 0U, 1U,
                                  IORING_ENTER_GETEVENTS, NULL, 0U);
            if (waited < 0 && errno != EINTR) {
                return fail_stage(stage);
            }
        }
    }
    errno = ETIMEDOUT;
    return fail_stage(stage);
}

static int wait_cqe_any_result(struct ring *ring, uint64_t user_data,
                               int32_t *actual, const char *stage) {
    for (unsigned int attempt = 0; attempt < WAIT_LOOPS; ++attempt) {
        uint32_t head = load_u32(ring->cq_ring, ring->params.cq_off.head);
        uint32_t tail = load_u32(ring->cq_ring, ring->params.cq_off.tail);
        if (tail != head) {
            const struct io_uring_cqe *cqe = next_cqe(ring, head);
            if (cqe->user_data != user_data || cqe->flags != 0) {
                errno = EIO;
                return fail_stage(stage);
            }
            *actual = cqe->res;
            store_u32(ring->cq_ring, ring->params.cq_off.head, head + 1);
            return 0;
        }
        struct pollfd descriptor = {.fd = ring->fd, .events = POLLIN, .revents = 0};
        int polled = poll(&descriptor, 1, 1);
        if (polled < 0) {
            if (errno == EINTR) {
                continue;
            }
            return fail_stage(stage);
        }
        if (polled > 0) {
            errno = 0;
            long waited = syscall(SYS_io_uring_enter, ring->fd, 0U, 1U,
                                  IORING_ENTER_GETEVENTS, NULL, 0U);
            if (waited < 0 && errno != EINTR) {
                return fail_stage(stage);
            }
        }
    }
    errno = ETIMEDOUT;
    return fail_stage(stage);
}

static int submit_fixed(struct ring *ring, uint8_t opcode, int fd,
                        uint64_t offset, uintptr_t address, uint32_t length,
                        uint16_t slot, uint64_t user_data,
                        int32_t expected_result, const char *stage) {
    if (queue_fixed(ring, opcode, fd, offset, address, length, slot, user_data) != 0) {
        return fail_stage(stage);
    }
    errno = 0;
    long submitted = syscall(SYS_io_uring_enter, ring->fd, 1U, 1U,
                             IORING_ENTER_GETEVENTS, NULL, 0U);
    if (submitted != 1) {
        return fail_value(stage, submitted, 1);
    }
    return wait_cqe(ring, user_data, expected_result, stage);
}

static int submit_fixed_any(struct ring *ring, uint8_t opcode, int fd,
                            uintptr_t address, uint32_t length, uint16_t slot,
                            uint64_t user_data, int32_t *actual,
                            const char *stage) {
    if (queue_fixed(ring, opcode, fd, 0, address, length, slot, user_data) != 0) {
        return fail_stage(stage);
    }
    errno = 0;
    long submitted = syscall(SYS_io_uring_enter, ring->fd, 1U, 1U,
                             IORING_ENTER_GETEVENTS, NULL, 0U);
    if (submitted != 1) {
        return fail_value(stage, submitted, 1);
    }
    for (unsigned int attempt = 0; attempt < WAIT_LOOPS; ++attempt) {
        uint32_t head = load_u32(ring->cq_ring, ring->params.cq_off.head);
        uint32_t tail = load_u32(ring->cq_ring, ring->params.cq_off.tail);
        if (tail != head) {
            const struct io_uring_cqe *cqe = next_cqe(ring, head);
            if (cqe->user_data != user_data || cqe->flags != 0) {
                errno = EIO;
                return fail_stage(stage);
            }
            *actual = cqe->res;
            store_u32(ring->cq_ring, ring->params.cq_off.head, head + 1);
            return 0;
        }
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 1000000L};
        nanosleep(&pause, NULL);
    }
    errno = ETIMEDOUT;
    return fail_stage(stage);
}

static int open_test_file(const char *path) {
    return open(path, O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0600);
}

static int fill_test_file(int fd, unsigned char value, size_t length) {
    unsigned char *contents = malloc(length);
    if (contents == NULL) {
        errno = ENOMEM;
        return -1;
    }
    memset(contents, value, length);
    ssize_t written = pwrite(fd, contents, length, 0);
    int saved_errno = errno;
    free(contents);
    if (written != (ssize_t)length) {
        errno = written < 0 ? saved_errno : EIO;
        return -1;
    }
    return 0;
}

struct fork_fixed_cow_report {
    unsigned char before_child_cow;
    unsigned char after_child_read;
    int32_t fixed_read_result;
    int32_t fixed_write_result;
};

static int write_full_fd(int fd, const void *data, size_t length,
                         const char *stage) {
    const unsigned char *bytes = data;
    size_t offset = 0;
    while (offset < length) {
        ssize_t written = write(fd, bytes + offset, length - offset);
        if (written < 0 && errno == EINTR) {
            continue;
        }
        if (written <= 0) {
            return fail_stage(stage);
        }
        offset += (size_t)written;
    }
    return 0;
}

static int read_full_fd(int fd, void *data, size_t length, const char *stage) {
    unsigned char *bytes = data;
    size_t offset = 0;
    while (offset < length) {
        ssize_t read_count = read(fd, bytes + offset, length - offset);
        if (read_count < 0 && errno == EINTR) {
            continue;
        }
        if (read_count <= 0) {
            if (read_count == 0) {
                errno = EPIPE;
            }
            return fail_stage(stage);
        }
        offset += (size_t)read_count;
    }
    return 0;
}

static int test_fork_fixed_cow(void) {
    static const char source_path[] = "/tmp/thekernel-io-uring-buffers-fork-source";
    static const char child_source_path[] =
        "/tmp/thekernel-io-uring-buffers-fork-child-source";
    static const char output_path[] = "/tmp/thekernel-io-uring-buffers-fork-output";
    struct ring ring;
    memset(&ring, 0, sizeof(ring));
    ring.fd = -1;
    void *buffer = MAP_FAILED;
    int source = -1;
    int child_source = -1;
    int output = -1;
    int ready[2] = {-1, -1};
    int go[2] = {-1, -1};
    int report_pipe[2] = {-1, -1};
    pid_t child = -1;
    int child_status = 0;
    int result = 1;

    if (ring_setup(&ring) != 0 ||
        (source = open_test_file(source_path)) < 0 ||
        (child_source = open_test_file(child_source_path)) < 0 ||
        (output = open_test_file(output_path)) < 0 ||
        fill_test_file(source, 'R', page_bytes) != 0 ||
        fill_test_file(child_source, 'S', page_bytes) != 0 ||
        ftruncate(output, (off_t)page_bytes) != 0) {
        fail_stage("fork-cow-setup");
        goto out;
    }
    buffer = alloc_user_buffer(page_bytes);
    if (buffer == MAP_FAILED) {
        fail_stage("fork-cow-buffer");
        goto out;
    }
    memset(buffer, 'P', page_bytes);
    struct iovec iov = {.iov_base = buffer, .iov_len = page_bytes};
    if (register_iov(&ring, &iov, 1U) != 0) {
        fail_stage("fork-cow-register");
        goto out;
    }
    if (pipe(ready) != 0 || pipe(go) != 0 || pipe(report_pipe) != 0) {
        fail_stage("fork-cow-pipes");
        goto out;
    }

    child = fork();
    if (child < 0) {
        fail_stage("fork-cow-fork");
        goto out;
    }
    if (child == 0) {
        struct fork_fixed_cow_report report;
        char command;
        memset(&report, 0, sizeof(report));
        close(ready[0]);
        close(go[1]);
        close(report_pipe[0]);
        if (write_full_fd(ready[1], "r", 1U, "fork-cow-child-ready") != 0 ||
            read_full_fd(go[0], &command, 1U,
                         "fork-cow-child-go") != 0) {
            _exit(80);
        }
        report.before_child_cow = *(unsigned char *)buffer;
        memset(buffer, 'C', page_bytes);
        report.fixed_read_result = 0;
        if (submit_fixed(&ring, IORING_OP_READ_FIXED, child_source, 0,
                         (uintptr_t)buffer, (uint32_t)page_bytes, 0,
                         0x464f524b5f524455ULL, (int32_t)page_bytes,
                         "fork-cow-child-fixed-read") != 0) {
            report.fixed_read_result = -1;
        }
        report.after_child_read = *(unsigned char *)buffer;
        report.fixed_write_result = 0;
        if (submit_fixed(&ring, IORING_OP_WRITE_FIXED, output, 0,
                         (uintptr_t)buffer, (uint32_t)page_bytes, 0,
                         0x464f524b5f575254ULL, (int32_t)page_bytes,
                         "fork-cow-child-fixed-write") != 0) {
            report.fixed_write_result = -1;
        }
        if (write_full_fd(report_pipe[1], &report, sizeof(report),
                          "fork-cow-child-report") != 0) {
            _exit(81);
        }
        _exit(0);
    }

    close(ready[1]);
    close(go[0]);
    close(report_pipe[1]);
    if (read_full_fd(ready[0], &(char){0}, 1U, "fork-cow-parent-ready") != 0) {
        goto child_out;
    }
    /* The child has not written its VA yet. Linux's fork path has already
     * copied a pinned anonymous page for the child, so this fixed READ must
     * not change the child's inherited VA. */
    if (submit_fixed(&ring, IORING_OP_READ_FIXED, source, 0,
                     (uintptr_t)buffer, (uint32_t)page_bytes, 0,
                     0x464f524b5f505244ULL, (int32_t)page_bytes,
                     "fork-cow-parent-fixed-read") != 0 ||
        *(unsigned char *)buffer != 'R') {
        if (*(unsigned char *)buffer != 'R') {
            fail_value("fork-cow-parent-page", *(unsigned char *)buffer, 'R');
        }
        goto child_out;
    }
    if (write_full_fd(go[1], "g", 1U, "fork-cow-parent-go") != 0) {
        goto child_out;
    }
    struct fork_fixed_cow_report report;
    if (read_full_fd(report_pipe[0], &report, sizeof(report),
                     "fork-cow-parent-report") != 0 ||
        waitpid(child, &child_status, 0) != child) {
        goto out;
    }
    child = -1;
    if (!WIFEXITED(child_status) || WEXITSTATUS(child_status) != 0) {
        fail_value("fork-cow-child-status", WIFEXITED(child_status)
                       ? WEXITSTATUS(child_status) : 128 + WTERMSIG(child_status), 0);
        goto out;
    }
    if (report.before_child_cow != 'P' || report.after_child_read != 'C' ||
        report.fixed_read_result != 0 || report.fixed_write_result != 0) {
        fail_stage("fork-cow-child-pages");
        goto out;
    }
    unsigned char output_byte = 0;
    if (pread(output, &output_byte, 1U, 0) != 1 || output_byte != 'S') {
        fail_value("fork-cow-fixed-write-source", output_byte, 'S');
        goto out;
    }
    if (*(unsigned char *)buffer != 'S') {
        fail_value("fork-cow-parent-final-page", *(unsigned char *)buffer, 'S');
        goto out;
    }
    fprintf(stderr,
            "io_uring_buffers: linux_host=%d fork_fixed_cow=parent-pinned-page\n",
            linux_host);
    result = 0;
    goto out;

child_out:
    if (child > 0) {
        (void)write(go[1], "g", 1U);
        (void)waitpid(child, &child_status, 0);
        child = -1;
    }
out:
    if (child > 0) {
        (void)write(go[1], "g", 1U);
        (void)waitpid(child, &child_status, 0);
    }
    if (ready[0] >= 0) close(ready[0]);
    if (ready[1] >= 0) close(ready[1]);
    if (go[0] >= 0) close(go[0]);
    if (go[1] >= 0) close(go[1]);
    if (report_pipe[0] >= 0) close(report_pipe[0]);
    if (report_pipe[1] >= 0) close(report_pipe[1]);
    if (output >= 0) close(output);
    if (child_source >= 0) close(child_source);
    if (source >= 0) close(source);
    unlink(source_path);
    unlink(child_source_path);
    unlink(output_path);
    int ring_owns_buffer = ring.buffer == buffer;
    ring_cleanup(&ring);
    if (ring_owns_buffer) {
        buffer = MAP_FAILED;
    }
    if (buffer != MAP_FAILED) {
        munmap(buffer, page_bytes);
    }
    return result;
}

static int expect_fixed_bytes(struct ring *ring, int fd, void *buffer,
                              uint16_t slot, const char *payload,
                              const char *stage) {
    size_t length = strlen(payload) + 1;
    memcpy(buffer, payload, length);
    return submit_fixed(ring, IORING_OP_WRITE_FIXED, fd, 0,
                        (uintptr_t)buffer, (uint32_t)length, slot,
                        0x4255464645525752ULL, (int32_t)length, stage);
}

static int test_registration_errors(void) {
    struct ring ring;
    if (ring_setup(&ring) != 0) {
        return fail_stage("errors-setup");
    }
    void *buffer = alloc_user_buffer(page_bytes);
    if (buffer == MAP_FAILED) {
        ring_cleanup(&ring);
        return fail_stage("errors-buffer");
    }
    struct iovec null_iov = {.iov_base = buffer, .iov_len = page_bytes};
    struct iovec bad_base = {.iov_base = (void *)(uintptr_t)1, .iov_len = 1};
    struct iovec zero_length = {.iov_base = buffer, .iov_len = 0};
    struct iovec bad_zero_length = {.iov_base = (void *)(uintptr_t)1, .iov_len = 0};
    struct iovec overflow = {.iov_base = buffer, .iov_len = SIZE_MAX};
    struct iovec address_overflow = {
        .iov_base = (void *)(uintptr_t)(UINTPTR_MAX - 1),
        .iov_len = 2,
    };
    struct iovec order_bad_then_zero[2] = {
        {.iov_base = (void *)(uintptr_t)1, .iov_len = 1},
        {.iov_base = buffer, .iov_len = 0},
    };
    struct iovec order_zero_then_bad[2] = {
        {.iov_base = buffer, .iov_len = 0},
        {.iov_base = (void *)(uintptr_t)1, .iov_len = 1},
    };
    int result = 1;
    if (expect_register_errno(&ring, IORING_REGISTER_BUFFERS, NULL, 1U,
                              EFAULT, "errors-null") ||
        expect_register_errno(&ring, IORING_REGISTER_BUFFERS, NULL, 0U,
                              EFAULT, "errors-null-zero-count") ||
        expect_register_errno(&ring, IORING_REGISTER_BUFFERS, NULL,
                              IORING_MAX_REGISTERED_BUFFERS + 1U, EFAULT,
                              "errors-null-over-count") ||
        expect_register_errno(&ring, IORING_REGISTER_BUFFERS, &bad_base, 1U,
                              EFAULT, "errors-bad-base") ||
        expect_register_errno(&ring, IORING_REGISTER_BUFFERS, &zero_length, 1U,
                              EFAULT, "errors-zero-length") ||
        expect_register_errno(&ring, IORING_REGISTER_BUFFERS, &bad_zero_length,
                              1U, EFAULT, "errors-bad-zero-length") ||
        expect_register_errno(&ring, IORING_REGISTER_BUFFERS, order_bad_then_zero,
                              2U, EFAULT, "errors-order-bad-then-zero") ||
        expect_register_errno(&ring, IORING_REGISTER_BUFFERS, order_zero_then_bad,
                              2U, EFAULT, "errors-order-zero-then-bad") ||
        expect_register_errno(&ring, IORING_REGISTER_BUFFERS, &overflow, 1U,
                              EINVAL, "errors-length-overflow") ||
        expect_register_errno(&ring, IORING_REGISTER_BUFFERS, &address_overflow,
                              1U, EOVERFLOW, "errors-address-overflow") ||
        expect_register_errno(&ring, IORING_REGISTER_BUFFERS, &null_iov,
                              IORING_MAX_REGISTERED_BUFFERS + 1U, EINVAL,
                              "errors-count-limit")) {
        goto out;
    }
    fprintf(stderr, "io_uring_buffers: linux_host=%d registration_precedence=checked\n",
            linux_host);
    result = 0;
out:
    munmap(buffer, page_bytes);
    ring_cleanup(&ring);
    return result;
}

static int test_registration_matrix(void) {
    struct ring ring;
    struct ring empty_ring;
    memset(&ring, 0, sizeof(ring));
    memset(&empty_ring, 0, sizeof(empty_ring));
    ring.fd = -1;
    empty_ring.fd = -1;
    void *buffer = MAP_FAILED;
    int file_result = 1;
    if (ring_setup(&ring) != 0 || ring_setup(&empty_ring) != 0) {
        fail_stage("registration-setup");
        goto out;
    }
    buffer = alloc_user_buffer(page_bytes);
    if (buffer == MAP_FAILED) {
        fail_stage("registration-buffer");
        goto out;
    }
    struct iovec iov = {.iov_base = buffer, .iov_len = page_bytes};
    if (register_iov(&ring, &iov, 1U) != 0) {
        fail_stage("registration-first");
        goto out;
    }
    if (expect_register_errno(&ring, IORING_REGISTER_BUFFERS, &iov, 1U,
                              EBUSY, "registration-duplicate") ||
        expect_register_errno(&ring, IORING_UNREGISTER_BUFFERS,
                              (void *)(uintptr_t)1, 0U, EINVAL,
                              "registration-unregister-argument") ||
        unregister_buffers(&ring) != 0) {
        fail_stage("registration-retire");
        goto out;
    }
    if (expect_register_errno(&empty_ring, IORING_UNREGISTER_BUFFERS, NULL, 0U,
                              ENXIO, "registration-no-table") ||
        expect_register_errno(&empty_ring, IORING_UNREGISTER_FILES,
                              (void *)(uintptr_t)1, 0U, EINVAL,
                              "registration-other-unregister")) {
        goto out;
    }
    file_result = 0;
out:
    if (buffer != MAP_FAILED) {
        munmap(buffer, page_bytes);
    }
    ring_cleanup(&ring);
    ring_cleanup(&empty_ring);
    return file_result;
}

static int test_file_backed_registration(void) {
    static const char path[] = "/tmp/thekernel-io-uring-buffer-file-backed";
    struct ring ring;
    memset(&ring, 0, sizeof(ring));
    ring.fd = -1;
    int backing = -1;
    void *buffer = MAP_FAILED;
    int result = 1;

    if (ring_setup(&ring) != 0 ||
        (backing = open_test_file(path)) < 0 ||
        ftruncate(backing, (off_t)page_bytes) != 0) {
        fail_stage("file-backed-setup");
        goto out;
    }
    buffer = mmap(NULL, page_bytes, PROT_READ | PROT_WRITE,
                  MAP_SHARED, backing, 0);
    if (buffer == MAP_FAILED) {
        fail_stage("file-backed-mmap");
        goto out;
    }
    memset(buffer, 0x5a, page_bytes);
    struct iovec iov = {.iov_base = buffer, .iov_len = page_bytes};
    if (register_iov(&ring, &iov, 1U) != 0 || unregister_buffers(&ring) != 0) {
        fail_stage("file-backed-register");
        goto out;
    }
    result = 0;
out:
    if (buffer != MAP_FAILED) {
        munmap(buffer, page_bytes);
    }
    if (backing >= 0) {
        close(backing);
    }
    unlink(path);
    ring_cleanup(&ring);
    return result;
}

static int test_ring_budget(void) {
    struct ring old_ring;
    struct ring over_ring;
    memset(&old_ring, 0, sizeof(old_ring));
    memset(&over_ring, 0, sizeof(over_ring));
    old_ring.fd = -1;
    over_ring.fd = -1;
    void *old_buffer = MAP_FAILED;
    void *over_buffer = MAP_FAILED;
    int file = -1;
    int result = 1;
    size_t target = budget_bytes();
    size_t over = target + page_bytes;
    if (ring_setup(&old_ring) != 0 || ring_setup(&over_ring) != 0) {
        fail_stage("budget-setup");
        goto out;
    }
    old_buffer = alloc_budget_buffer(target);
    over_buffer = alloc_budget_buffer(over);
    if (old_buffer == MAP_FAILED || over_buffer == MAP_FAILED) {
        fail_stage("budget-buffer");
        goto out;
    }
    struct iovec old_iov = {.iov_base = old_buffer, .iov_len = target};
    struct iovec over_iov = {.iov_base = over_buffer, .iov_len = over};
    if (register_iov(&old_ring, &old_iov, 1U) != 0) {
        fail_stage("budget-register-limit");
        goto out;
    }
    old_ring.buffer = old_buffer;
    old_ring.buffer_length = target;
    errno = 0;
    long over_result = syscall(SYS_io_uring_register, over_ring.fd,
                               IORING_REGISTER_BUFFERS, &over_iov, 1U);
    if (!linux_host && (over_result != -1 || errno != EBUSY)) {
        fail_value("budget-over-limit", over_result == -1 ? errno : over_result,
                   EBUSY);
        goto out;
    }
    if (linux_host) {
        if (over_result != 0) {
            fail_value("budget-linux-host-over-limit", errno, 0);
            goto out;
        }
        over_ring.buffers_registered = 1;
        fprintf(stderr,
                "io_uring_buffers: linux_host=1 no_16MiB_ring_budget_errno over_bytes=%zu\n",
                over);
    }
    file = open_test_file("/tmp/thekernel-io-uring-buffers-budget");
    if (file < 0) {
        fail_stage("budget-file");
        goto out;
    }
    if (expect_fixed_bytes(&old_ring, file, old_buffer, 0,
                           "budget-old-table", "budget-old-table-use")) {
        goto out;
    }
    result = 0;
out:
    if (file >= 0) {
        close(file);
        unlink("/tmp/thekernel-io-uring-buffers-budget");
    }
    if (old_ring.buffers_registered) {
        if (observe_completions(&old_ring, "budget-completion-observe") != 0 ||
            unregister_buffers(&old_ring) != 0) {
            fail_stage("budget-unregister");
            result = 1;
        }
    }
    int old_ring_owns_buffer = old_ring.buffer == old_buffer;
    ring_cleanup(&old_ring);
    if (old_ring_owns_buffer) {
        old_buffer = MAP_FAILED;
    }
    ring_cleanup(&over_ring);
    if (old_buffer != MAP_FAILED) {
        munmap(old_buffer, target);
    }
    if (over_buffer != MAP_FAILED) {
        munmap(over_buffer, over);
    }
    return result;
}

static int test_slots_and_generation(void) {
    struct ring ring;
    memset(&ring, 0, sizeof(ring));
    ring.fd = -1;
    void *first = MAP_FAILED;
    void *second = MAP_FAILED;
    void *replacement = MAP_FAILED;
    int file = -1;
    int result = 1;
    static const char first_payload[] = "registered-first\n";
    static const char second_payload[] = "registered-second\n";
    size_t first_length = sizeof(first_payload);
    size_t second_length = sizeof(second_payload);
    if (ring_setup(&ring) != 0) {
        return fail_stage("generation-setup");
    }
    first = alloc_user_buffer(page_bytes);
    second = alloc_user_buffer(page_bytes);
    replacement = alloc_user_buffer(page_bytes);
    if (first == MAP_FAILED || second == MAP_FAILED || replacement == MAP_FAILED) {
        fail_stage("generation-buffer");
        goto out;
    }
    struct iovec iov[2] = {
        {.iov_base = first, .iov_len = page_bytes},
        {.iov_base = second, .iov_len = page_bytes},
    };
    if (register_iov(&ring, iov, 2U) != 0) {
        fail_stage("generation-register-two");
        goto out;
    }
    file = open_test_file("/tmp/thekernel-io-uring-buffers-generation");
    if (file < 0) {
        fail_stage("generation-file");
        goto out;
    }
    memcpy((unsigned char *)first + 7, first_payload, first_length);
    if (submit_fixed(&ring, IORING_OP_WRITE_FIXED, file, 0,
                     (uintptr_t)((unsigned char *)first + 7),
                     (uint32_t)first_length, 0, 0x47454e5f4f4c44ULL,
                     (int32_t)first_length, "generation-old-write") ||
        submit_fixed(&ring, IORING_OP_READ_FIXED, file, 0,
                     (uintptr_t)((unsigned char *)second + 11),
                     (uint32_t)first_length, 1, 0x47454e4e5f5244ULL,
                     (int32_t)first_length, "generation-subrange-read") ||
        memcmp((unsigned char *)second + 11, first_payload, first_length) != 0) {
        errno = EIO;
        fail_stage("generation-subrange-contents");
        goto out;
    }
    if (submit_fixed(&ring, IORING_OP_READ_FIXED, file, 0,
                     (uintptr_t)((unsigned char *)first + page_bytes - 1), 2U,
                     0, 0x47454e5f424f5544ULL, -EFAULT,
                     "generation-subrange-bounds")) {
        goto out;
    }
    if (unregister_buffers(&ring) != 0) {
        fail_stage("generation-unregister");
        goto out;
    }
    struct iovec new_iov = {.iov_base = replacement, .iov_len = page_bytes};
    if (register_iov(&ring, &new_iov, 1U) != 0) {
        fail_stage("generation-reregister");
        goto out;
    }
    memcpy(replacement, second_payload, second_length);
    if (submit_fixed(&ring, IORING_OP_WRITE_FIXED, file, 0,
                     (uintptr_t)replacement, (uint32_t)second_length, 0,
                     0x47454e5f4e4557ULL, (int32_t)second_length,
                     "generation-new-write") ||
        submit_fixed(&ring, IORING_OP_READ_FIXED, file, 0,
                     (uintptr_t)((unsigned char *)replacement + 64),
                     (uint32_t)second_length, 0, 0x47454e5f4e5752ULL,
                     (int32_t)second_length, "generation-new-read") ||
        memcmp((unsigned char *)replacement + 64, second_payload, second_length) != 0) {
        errno = EIO;
        fail_stage("generation-new-contents");
        goto out;
    }
    result = 0;
out:
    if (file >= 0) {
        close(file);
        unlink("/tmp/thekernel-io-uring-buffers-generation");
    }
    if (ring.buffers_registered) {
        observe_completions(&ring, "generation-completion-observe");
        unregister_buffers(&ring);
    }
    if (first != MAP_FAILED) {
        munmap(first, page_bytes);
    }
    if (second != MAP_FAILED) {
        munmap(second, page_bytes);
    }
    if (replacement != MAP_FAILED) {
        munmap(replacement, page_bytes);
    }
    ring_cleanup(&ring);
    return result;
}

static void *inflight_submit_thread(void *opaque) {
    struct inflight_context *context = opaque;
    if (queue_fixed(context->ring, IORING_OP_READ_FIXED, context->read_fd, 0,
                    (uintptr_t)context->buffer, 1U, 0,
                    0x494e464c49474854ULL) != 0) {
        context->submit_result = -1;
        context->submit_errno = errno;
        atomic_store_explicit(&context->ready, 1, memory_order_release);
    } else {
        /* Signal after SQE publication. The nonblocking pipe keeps this
         * differential probe bounded on TheKernel, whose positioned-read
         * adapter reports EAGAIN instead of sleeping in io_uring. */
        atomic_store_explicit(&context->ready, 1, memory_order_release);
        errno = 0;
        long submitted = syscall(SYS_io_uring_enter, context->ring->fd, 1U,
                                 0U, 0U, NULL, 0U);
        if (submitted != 1) {
            context->submit_result = -1;
            context->submit_errno = errno;
        } else {
            context->submit_result = 0;
            context->submit_errno = 0;
        }
    }
    return NULL;
}

static void *inflight_unregister_thread(void *opaque) {
    struct unregister_context *context = opaque;
    while (atomic_load_explicit(&context->inflight->ready, memory_order_acquire) == 0) {
        sched_yield();
    }
    struct timespec pause = {.tv_sec = 0, .tv_nsec = 1000000L};
    nanosleep(&pause, NULL);
    errno = 0;
    context->result = syscall(SYS_io_uring_register, context->ring->fd,
                              IORING_UNREGISTER_BUFFERS, NULL, 0U);
    context->saved_errno = errno;
    return NULL;
}

static int test_inflight_unregister_body(void) {
    struct ring ring;
    memset(&ring, 0, sizeof(ring));
    ring.fd = -1;
    void *buffer = MAP_FAILED;
    int pipe_fds[2] = {-1, -1};
    pthread_t submitter;
    pthread_t unregisterer;
    int submitter_started = 0;
    int unregisterer_started = 0;
    int file = -1;
    int result = 1;
    struct inflight_context inflight;
    struct unregister_context unregistration;
    memset(&inflight, 0, sizeof(inflight));
    memset(&unregistration, 0, sizeof(unregistration));
    atomic_init(&inflight.ready, 0);
    if (ring_setup(&ring) != 0 || pipe2(pipe_fds, O_CLOEXEC | O_NONBLOCK) != 0) {
        fail_stage("inflight-setup");
        goto out;
    }
    buffer = alloc_user_buffer(page_bytes);
    if (buffer == MAP_FAILED) {
        fail_stage("inflight-buffer");
        goto out;
    }
    struct iovec iov = {.iov_base = buffer, .iov_len = page_bytes};
    if (register_iov(&ring, &iov, 1U) != 0) {
        fail_stage("inflight-register");
        goto out;
    }
    inflight.ring = &ring;
    inflight.read_fd = pipe_fds[0];
    inflight.buffer = buffer;
    unregistration.ring = &ring;
    unregistration.inflight = &inflight;
    if (pthread_create(&submitter, NULL, inflight_submit_thread, &inflight) != 0) {
        fail_stage("inflight-submit-thread");
        goto out;
    }
    submitter_started = 1;
    if (pthread_create(&unregisterer, NULL, inflight_unregister_thread,
                       &unregistration) != 0) {
        fail_stage("inflight-unregister-thread");
        goto out;
    }
    unregisterer_started = 1;
    for (unsigned int attempt = 0; attempt < WAIT_LOOPS; ++attempt) {
        if (atomic_load_explicit(&inflight.ready, memory_order_acquire) != 0) {
            break;
        }
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 1000000L};
        nanosleep(&pause, NULL);
    }
    if (atomic_load_explicit(&inflight.ready, memory_order_acquire) == 0) {
        errno = ETIMEDOUT;
        fail_stage("inflight-submit-ready");
        goto out;
    }
    if (pthread_join(unregisterer, NULL) != 0) {
        fail_stage("inflight-unregister-join");
        unregisterer_started = 0;
        goto out;
    }
    unregisterer_started = 0;
    if (unregistration.result != 0) {
        errno = unregistration.saved_errno;
        fail_value("inflight-unregister-result", unregistration.result, 0);
        goto out;
    }
    fprintf(stderr, "io_uring_buffers: linux_host=%d inflight_unregister_result=0\n",
            linux_host);
    if (pthread_join(submitter, NULL) != 0) {
        fail_stage("inflight-submit-join");
        submitter_started = 0;
        goto out;
    }
    submitter_started = 0;
    if (inflight.submit_result != 0) {
        errno = inflight.submit_errno;
        fail_stage("inflight-submit-result");
        goto out;
    }
    int post_unregister_errno = linux_host ? EFAULT : EBADF;
    fprintf(stderr,
            "io_uring_buffers: linux_host=%d fixed_after_unregister_errno=%d\n",
            linux_host, post_unregister_errno);
    file = open_test_file("/tmp/thekernel-io-uring-buffers-inflight");
    if (file < 0) {
        fail_stage("inflight-fixed-file");
        goto out;
    }
    int32_t original_result = 0;
    if (write(pipe_fds[1], "I", 1) != 1 ||
        wait_cqe_any_result(&ring, 0x494e464c49474854ULL, &original_result,
                            "inflight-original-completion") ||
        (original_result >= 0 && original_result != 1)) {
        fprintf(stderr, "io_uring_buffers: linux_host=%d inflight_original_cqe=%d\n",
                linux_host, original_result);
        fail_stage("inflight-release");
        goto out;
    }
    fprintf(stderr, "io_uring_buffers: linux_host=%d inflight_original_cqe=%d\n",
            linux_host, original_result);
    /* Issue the post-retirement lookup after the bounded probe completion;
     * the table is already closed and must report EBADF (Linux reports EFAULT
     * for this fixed-buffer lookup). */
    if (submit_fixed(&ring, IORING_OP_READ_FIXED, file, 0,
                     (uintptr_t)buffer, 1U, 0, 0x494e464c49474445ULL,
                     -post_unregister_errno, "inflight-fixed-after-unregister")) {
        goto out;
    }
    errno = 0;
    if (syscall(SYS_io_uring_enter, ring.fd, 0U, 0U, 0U, NULL, 0U) < 0) {
        fail_stage("inflight-completion-observe");
        goto out;
    }
    size_t full = budget_bytes();
    void *full_buffer = alloc_budget_buffer(full);
    if (full_buffer == MAP_FAILED) {
        fail_stage("inflight-refund-buffer");
        goto out;
    }
    struct ring refund_ring;
    memset(&refund_ring, 0, sizeof(refund_ring));
    refund_ring.fd = -1;
    ring_cleanup(&ring);
    if (ring_setup(&refund_ring) != 0) {
        munmap(full_buffer, full);
        fail_stage("inflight-refund-setup");
        goto out;
    }
    struct iovec full_iov = {.iov_base = full_buffer, .iov_len = full};
    if (register_iov(&refund_ring, &full_iov, 1U) != 0) {
        ring_cleanup(&refund_ring);
        munmap(full_buffer, full);
        fail_stage("inflight-refund-register");
        goto out;
    }
    if (unregister_buffers(&refund_ring) != 0) {
        ring_cleanup(&refund_ring);
        munmap(full_buffer, full);
        fail_stage("inflight-refund-unregister");
        goto out;
    }
    ring_cleanup(&refund_ring);
    munmap(full_buffer, full);
    result = 0;
out:
    if (unregisterer_started) {
        atomic_store_explicit(&inflight.ready, 1, memory_order_release);
        pthread_join(unregisterer, NULL);
    }
    if (submitter_started) {
        /* The submitter has no wait operation of its own; a failed setup is
         * the only path which can leave it here. */
        pthread_join(submitter, NULL);
    }
    if (file >= 0) {
        close(file);
        unlink("/tmp/thekernel-io-uring-buffers-inflight");
    }
    if (pipe_fds[0] >= 0) {
        close(pipe_fds[0]);
    }
    if (pipe_fds[1] >= 0) {
        close(pipe_fds[1]);
    }
    if (buffer != MAP_FAILED) {
        munmap(buffer, page_bytes);
    }
    ring_cleanup(&ring);
    return result;
}

static int test_inflight_unregister(void) {
    pid_t child = fork();
    if (child < 0) {
        return fail_stage("inflight-isolation-fork");
    }
    if (child == 0) {
        _exit(test_inflight_unregister_body());
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0) {
        fprintf(stderr, "THEKERNEL_IO_URING_BUFFERS_FAIL stage=inflight-isolation-child status=%d\n",
                status);
        errno = ECHILD;
        return 1;
    }
    return 0;
}

static int prepare_budget_ring(struct ring *ring, size_t length) {
    if (ring_setup(ring) != 0) {
        return -1;
    }
    ring->buffer = alloc_user_buffer(length);
    ring->buffer_length = length;
    if (ring->buffer == MAP_FAILED) {
        ring->buffer = NULL;
        ring->buffer_length = 0;
        return -1;
    }
    struct iovec iov = {.iov_base = ring->buffer, .iov_len = length};
    if (register_iov(ring, &iov, 1U) != 0) {
        return -1;
    }
    return 0;
}

static int wait_budget_registration(struct ring *ring, size_t length,
                                    const char *stage) {
    memset(ring, 0, sizeof(*ring));
    ring->fd = -1;
    if (ring_setup(ring) != 0) {
        return fail_stage(stage);
    }
    ring->buffer = alloc_user_buffer(length);
    ring->buffer_length = length;
    if (ring->buffer == MAP_FAILED) {
        ring->buffer = NULL;
        ring->buffer_length = 0;
        ring_cleanup(ring);
        return fail_stage(stage);
    }
    struct iovec iov = {.iov_base = ring->buffer, .iov_len = length};
    for (unsigned int attempt = 0; attempt < WAIT_LOOPS; ++attempt) {
        if (register_iov(ring, &iov, 1U) == 0) {
            return 0;
        }
        int saved_errno = errno;
        if (saved_errno != EBUSY && saved_errno != EAGAIN && saved_errno != ENOMEM) {
            errno = saved_errno;
            ring_cleanup(ring);
            return fail_stage(stage);
        }
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 1000000L};
        nanosleep(&pause, NULL);
    }
    ring_cleanup(ring);
    errno = ETIMEDOUT;
    return fail_stage(stage);
}

static int prepare_budget_set(struct ring *rings, unsigned int count,
                              size_t length, const char *stage) {
    for (unsigned int index = 0; index < count; ++index) {
        memset(&rings[index], 0, sizeof(rings[index]));
        rings[index].fd = -1;
        if (ring_setup(&rings[index]) != 0) {
            errno = errno == 0 ? EBUSY : errno;
            fail_stage(stage);
            for (unsigned int cleanup = 0; cleanup <= index; ++cleanup) {
                ring_cleanup(&rings[cleanup]);
            }
            return 1;
        }
        rings[index].buffer = alloc_budget_buffer(length);
        rings[index].buffer_length = length;
        if (rings[index].buffer == MAP_FAILED) {
            rings[index].buffer = NULL;
            rings[index].buffer_length = 0;
            fail_stage(stage);
            for (unsigned int cleanup = 0; cleanup <= index; ++cleanup) {
                ring_cleanup(&rings[cleanup]);
            }
            return 1;
        }
        struct iovec iov = {.iov_base = rings[index].buffer, .iov_len = length};
        if (register_iov(&rings[index], &iov, 1U) != 0) {
            fprintf(stderr,
                    "io_uring_buffers: budget-set index=%u errno=%d (%s)\n",
                    index, errno, strerror(errno));
            fail_stage(stage);
            for (unsigned int cleanup = 0; cleanup <= index; ++cleanup) {
                ring_cleanup(&rings[cleanup]);
            }
            return 1;
        }
    }
    return 0;
}

static int test_close_teardown(void) {
    struct ring kept[THEKERNEL_GLOBAL_BUFFER_RINGS - 1];
    struct ring closed;
    struct ring replacement;
    memset(kept, 0, sizeof(kept));
    memset(&closed, 0, sizeof(closed));
    memset(&replacement, 0, sizeof(replacement));
    for (unsigned int index = 0; index < sizeof(kept) / sizeof(kept[0]); ++index) {
        kept[index].fd = -1;
    }
    closed.fd = -1;
    replacement.fd = -1;
    size_t length = budget_bytes();
    if (prepare_budget_set(kept, THEKERNEL_GLOBAL_BUFFER_RINGS - 1,
                           length, "close-kept-budget")) {
        return 1;
    }
    if (prepare_budget_ring(&closed, length) != 0) {
        fail_stage("close-ring-register");
        for (unsigned int index = 0; index < THEKERNEL_GLOBAL_BUFFER_RINGS - 1;
             ++index) {
            ring_cleanup(&kept[index]);
        }
        ring_cleanup(&closed);
        return 1;
    }
    errno = 0;
    if (close(closed.fd) != 0) {
        fail_stage("close-ring-fd");
        closed.fd = -1;
        ring_cleanup(&closed);
        for (unsigned int index = 0; index < THEKERNEL_GLOBAL_BUFFER_RINGS - 1;
             ++index) {
            ring_cleanup(&kept[index]);
        }
        return 1;
    }
    closed.fd = -1;
    ring_close_without_unregister(&closed);
    if (wait_budget_registration(&replacement, length, "close-finalizer-budget")) {
        for (unsigned int index = 0; index < THEKERNEL_GLOBAL_BUFFER_RINGS - 1;
             ++index) {
            ring_cleanup(&kept[index]);
        }
        return 1;
    }
    ring_cleanup(&replacement);
    for (unsigned int index = 0; index < THEKERNEL_GLOBAL_BUFFER_RINGS - 1; ++index) {
        ring_cleanup(&kept[index]);
    }
    return 0;
}

static int test_fork_exec_teardown(void) {
    struct ring kept[THEKERNEL_GLOBAL_BUFFER_RINGS - 1];
    memset(kept, 0, sizeof(kept));
    for (unsigned int index = 0; index < sizeof(kept) / sizeof(kept[0]); ++index) {
        kept[index].fd = -1;
    }
    size_t length = budget_bytes();
    if (prepare_budget_set(kept, THEKERNEL_GLOBAL_BUFFER_RINGS - 1,
                           length, "exec-kept-budget")) {
        return 1;
    }
    pid_t child = fork();
    if (child < 0) {
        fail_stage("exec-fork");
        for (unsigned int index = 0; index < THEKERNEL_GLOBAL_BUFFER_RINGS - 1;
             ++index) {
            ring_cleanup(&kept[index]);
        }
        return 1;
    }
    if (child == 0) {
        struct ring child_ring;
        memset(&child_ring, 0, sizeof(child_ring));
        child_ring.fd = -1;
        if (prepare_budget_ring(&child_ring, length) != 0) {
            _exit(70);
        }
        execl("/bin/true", "true", (char *)NULL);
        _exit(71);
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0) {
        fprintf(stderr, "THEKERNEL_IO_URING_BUFFERS_FAIL stage=exec-child status=%d\n",
                status);
        for (unsigned int index = 0; index < THEKERNEL_GLOBAL_BUFFER_RINGS - 1;
             ++index) {
            ring_cleanup(&kept[index]);
        }
        errno = ECHILD;
        return 1;
    }
    fprintf(stderr, "io_uring_buffers: linux_host=%d fork_exec_child_teardown=observed\n",
            linux_host);
    struct ring replacement;
    memset(&replacement, 0, sizeof(replacement));
    replacement.fd = -1;
    int result = wait_budget_registration(&replacement, length,
                                          "exec-finalizer-budget");
    if (result == 0) {
        ring_cleanup(&replacement);
    }
    for (unsigned int index = 0; index < THEKERNEL_GLOBAL_BUFFER_RINGS - 1; ++index) {
        ring_cleanup(&kept[index]);
    }
    return result;
}

static int test_copy_fallback(void) {
    struct ring ring;
    memset(&ring, 0, sizeof(ring));
    ring.fd = -1;
    void *buffer = MAP_FAILED;
    int file = -1;
    int result = 1;
    static const char payload[] = "copy-fallback-registered-buffer\n";
    size_t registered_length = 128;
    if (ring_setup(&ring) != 0) {
        return fail_stage("copy-setup");
    }
    buffer = alloc_user_buffer(page_bytes);
    if (buffer == MAP_FAILED) {
        fail_stage("copy-buffer");
        goto out;
    }
    struct iovec iov = {.iov_base = buffer, .iov_len = registered_length};
    if (register_iov(&ring, &iov, 1U) != 0) {
        fail_stage("copy-register");
        goto out;
    }
    ring.buffer = buffer;
    ring.buffer_length = page_bytes;
    file = open_test_file("/tmp/thekernel-io-uring-buffers-copy");
    if (file < 0) {
        fail_stage("copy-file");
        goto out;
    }
    memcpy(buffer, payload, sizeof(payload));
    if (submit_fixed(&ring, IORING_OP_WRITE_FIXED, file, 0,
                     (uintptr_t)buffer, (uint32_t)sizeof(payload), 0,
                     0x434f50595f5752ULL, (int32_t)sizeof(payload),
                     "copy-write")) {
        goto out;
    }
    memset(buffer, 0, page_bytes);
    if (submit_fixed(&ring, IORING_OP_READ_FIXED, file, 0,
                     (uintptr_t)buffer, (uint32_t)sizeof(payload), 0,
                     0x434f50595f5244ULL, (int32_t)sizeof(payload),
                     "copy-read") || memcmp(buffer, payload, sizeof(payload)) != 0) {
        errno = EIO;
        fail_stage("copy-contents");
        goto out;
    }
    result = 0;
out:
    if (file >= 0) {
        close(file);
        unlink("/tmp/thekernel-io-uring-buffers-copy");
    }
    if (ring.buffers_registered) {
        observe_completions(&ring, "copy-completion-observe");
    }
    int ring_owns_buffer = ring.buffer == buffer;
    ring_cleanup(&ring);
    if (ring_owns_buffer) {
        buffer = MAP_FAILED;
    }
    if (buffer != MAP_FAILED) {
        munmap(buffer, page_bytes);
    }
    return result;
}

static int test_unmap_after_registration(void) {
    struct ring ring;
    memset(&ring, 0, sizeof(ring));
    ring.fd = -1;
    void *buffer = MAP_FAILED;
    int file = -1;
    int result = 1;
    if (ring_setup(&ring) != 0) {
        return fail_stage("munmap-setup");
    }
    buffer = alloc_user_buffer(page_bytes);
    if (buffer == MAP_FAILED) {
        fail_stage("munmap-buffer");
        goto out;
    }
    struct iovec iov = {.iov_base = buffer, .iov_len = page_bytes};
    if (register_iov(&ring, &iov, 1U) != 0) {
        fail_stage("munmap-register");
        goto out;
    }
    file = open_test_file("/tmp/thekernel-io-uring-buffers-munmap");
    if (file < 0) {
        fail_stage("munmap-file");
        goto out;
    }
    memcpy(buffer, "M", 2);
    void *stale_address = buffer;
    if (munmap(buffer, page_bytes) != 0) {
        fail_stage("munmap-user-buffer");
        goto out;
    }
    buffer = MAP_FAILED;
    ring.buffer = NULL;
    ring.buffer_length = 0;
    int32_t actual = 0;
    if (submit_fixed_any(&ring, IORING_OP_READ_FIXED, file,
                         (uintptr_t)stale_address, 1U, 0,
                         0x4d554e4d41505f52ULL, &actual, "munmap-fixed-io")) {
        goto out;
    }
    if (actual >= 0) {
        fprintf(stderr, "io_uring_buffers: linux_host=%d munmap_fixed_cqe=%d\n",
                linux_host, actual);
    } else {
        fprintf(stderr, "io_uring_buffers: linux_host=%d munmap_fixed_cqe_error=%d\n",
                linux_host, -actual);
    }
    result = 0;
out:
    if (file >= 0) {
        close(file);
        unlink("/tmp/thekernel-io-uring-buffers-munmap");
    }
    if (buffer != MAP_FAILED) {
        munmap(buffer, page_bytes);
    }
    if (ring.buffers_registered) {
        observe_completions(&ring, "munmap-completion-observe");
    }
    ring_cleanup(&ring);
    return result;
}

static void stress_record_failure(struct stress_shared *shared,
                                  const char *stage, int saved_errno) {
    int expected = 0;
    if (atomic_compare_exchange_strong_explicit(&shared->failed, &expected, 1,
                                                memory_order_acq_rel,
                                                memory_order_acquire)) {
        errno = saved_errno == 0 ? EIO : saved_errno;
        fail_stage(stage);
    }
}

static void *stress_register_worker(void *opaque) {
    struct stress_register_context *context = opaque;
    struct ring ring;
    memset(&ring, 0, sizeof(ring));
    ring.fd = -1;
    void *buffer = MAP_FAILED;
    if (ring_setup(&ring) != 0) {
        stress_record_failure(context->shared, "stress-register-setup", errno);
        return NULL;
    }
    buffer = alloc_user_buffer(page_bytes);
    if (buffer == MAP_FAILED) {
        stress_record_failure(context->shared, "stress-register-buffer", errno);
        ring_cleanup(&ring);
        return NULL;
    }
    struct iovec iov = {.iov_base = buffer, .iov_len = page_bytes};
    for (unsigned int round = 0; round < STRESS_ROUNDS; ++round) {
        if (register_iov(&ring, &iov, 1U) != 0) {
            stress_record_failure(context->shared, "stress-register-first", errno);
            break;
        }
        if (unregister_buffers(&ring) != 0) {
            stress_record_failure(context->shared, "stress-register-first-unregister", errno);
            break;
        }
        if (register_iov(&ring, &iov, 1U) != 0) {
            stress_record_failure(context->shared, "stress-register-second", errno);
            break;
        }
        if (unregister_buffers(&ring) != 0) {
            stress_record_failure(context->shared, "stress-register-second-unregister", errno);
            break;
        }
    }
    munmap(buffer, page_bytes);
    ring_cleanup(&ring);
    (void)context->index;
    return NULL;
}

static void *stress_fixed_worker(void *opaque) {
    struct stress_fixed_context *context = opaque;
    struct ring ring;
    memset(&ring, 0, sizeof(ring));
    ring.fd = -1;
    void *buffer = MAP_FAILED;
    int file = -1;
    char path[64];
    if (snprintf(path, sizeof(path), "/tmp/thekernel-iouring-stress-%u",
                 context->index) < 0) {
        stress_record_failure(context->shared, "stress-fixed-path", EINVAL);
        return NULL;
    }
    if (ring_setup(&ring) != 0) {
        stress_record_failure(context->shared, "stress-fixed-setup", errno);
        return NULL;
    }
    buffer = alloc_user_buffer(page_bytes);
    if (buffer == MAP_FAILED) {
        stress_record_failure(context->shared, "stress-fixed-buffer", errno);
        ring_cleanup(&ring);
        return NULL;
    }
    struct iovec iov = {.iov_base = buffer, .iov_len = page_bytes};
    if (register_iov(&ring, &iov, 1U) != 0) {
        stress_record_failure(context->shared, "stress-fixed-register", errno);
        goto out;
    }
    file = open_test_file(path);
    if (file < 0) {
        stress_record_failure(context->shared, "stress-fixed-file", errno);
        goto out;
    }
    static const char payload[] = "stress-fixed-io\n";
    for (unsigned int round = 0; round < STRESS_ROUNDS; ++round) {
        memcpy(buffer, payload, sizeof(payload));
        memset((unsigned char *)buffer + 64, 0, sizeof(payload));
        uint64_t offset = (uint64_t)round * 64U;
        if (submit_fixed(&ring, IORING_OP_WRITE_FIXED, file, offset,
                         (uintptr_t)buffer, (uint32_t)sizeof(payload), 0,
                         0x5354524553535752ULL + round,
                         (int32_t)sizeof(payload), "stress-fixed-write") ||
            submit_fixed(&ring, IORING_OP_READ_FIXED, file, offset,
                         (uintptr_t)((unsigned char *)buffer + 64),
                         (uint32_t)sizeof(payload), 0,
                         0x5354524553535244ULL + round,
                         (int32_t)sizeof(payload), "stress-fixed-read") ||
            memcmp((unsigned char *)buffer + 64, payload, sizeof(payload)) != 0) {
            stress_record_failure(context->shared, "stress-fixed-contents", EIO);
            break;
        }
    }
out:
    if (file >= 0) {
        close(file);
        unlink(path);
    }
    if (ring.buffers_registered) {
        observe_completions(&ring, "stress-fixed-completion-observe");
    }
    if (buffer != MAP_FAILED) {
        munmap(buffer, page_bytes);
    }
    ring_cleanup(&ring);
    return NULL;
}

static void *stress_close_worker(void *opaque) {
    struct stress_register_context *context = opaque;
    for (unsigned int round = 0; round < STRESS_ROUNDS; ++round) {
        struct ring ring;
        memset(&ring, 0, sizeof(ring));
        ring.fd = -1;
        if (prepare_budget_ring(&ring, page_bytes) != 0) {
            stress_record_failure(context->shared, "stress-close-register", errno);
            return NULL;
        }
        ring_close_without_unregister(&ring);
    }
    (void)context->index;
    return NULL;
}

static int test_multithreaded_stress(void) {
    struct stress_shared shared;
    atomic_init(&shared.failed, 0);
    struct stress_register_context registration[4];
    struct stress_fixed_context fixed[2];
    pthread_t registration_threads[4];
    pthread_t fixed_threads[2];
    pthread_t close_thread;
    unsigned int registration_started = 0;
    unsigned int fixed_started = 0;
    int close_started = 0;
    for (unsigned int index = 0; index < 4; ++index) {
        registration[index].shared = &shared;
        registration[index].index = index;
        int error = pthread_create(&registration_threads[index], NULL,
                                   stress_register_worker, &registration[index]);
        if (error != 0) {
            stress_record_failure(&shared, "stress-register-thread-create", error);
            break;
        }
        registration_started += 1;
    }
    for (unsigned int index = 0; index < 2 && atomic_load(&shared.failed) == 0; ++index) {
        fixed[index].shared = &shared;
        fixed[index].index = index;
        int error = pthread_create(&fixed_threads[index], NULL,
                                   stress_fixed_worker, &fixed[index]);
        if (error != 0) {
            stress_record_failure(&shared, "stress-fixed-thread-create", error);
            break;
        }
        fixed_started += 1;
    }
    if (atomic_load(&shared.failed) == 0) {
        struct stress_register_context close_context = {
            .shared = &shared,
            .index = 0,
        };
        int error = pthread_create(&close_thread, NULL, stress_close_worker,
                                   &close_context);
        if (error != 0) {
            stress_record_failure(&shared, "stress-close-thread-create", error);
        } else {
            close_started = 1;
        }
        if (close_started) {
            pthread_join(close_thread, NULL);
        }
    }
    for (unsigned int index = 0; index < fixed_started; ++index) {
        pthread_join(fixed_threads[index], NULL);
    }
    for (unsigned int index = 0; index < registration_started; ++index) {
        pthread_join(registration_threads[index], NULL);
    }
    if (atomic_load(&shared.failed) != 0) {
        return 1;
    }

    struct ring full[THEKERNEL_GLOBAL_BUFFER_RINGS];
    memset(full, 0, sizeof(full));
    for (unsigned int index = 0; index < THEKERNEL_GLOBAL_BUFFER_RINGS; ++index) {
        full[index].fd = -1;
    }
    int result = 0;
    size_t length = budget_bytes();
    for (unsigned int index = 0; index < THEKERNEL_GLOBAL_BUFFER_RINGS; ++index) {
        if (wait_budget_registration(&full[index], length,
                                     "stress-final-budget")) {
            result = 1;
            break;
        }
    }
    for (unsigned int index = 0; index < THEKERNEL_GLOBAL_BUFFER_RINGS; ++index) {
        ring_cleanup(&full[index]);
    }
    return result;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);
    if (argc == 2 && strcmp(argv[1], "--linux-host") == 0) {
        linux_host = 1;
    } else if (argc != 1) {
        errno = EINVAL;
        return fail_stage("arguments");
    }
    long system_page = sysconf(_SC_PAGESIZE);
    if (system_page != 4096) {
        errno = EINVAL;
        return fail_stage("page-size");
    }
    page_bytes = (size_t)system_page;
    if (linux_host) {
        fprintf(stderr,
                "io_uring_buffers: linux-host differential mode budget=one-page\n");
    }

    if (test_registration_errors() != 0) {
        return 1;
    }
    puts("THEKERNEL_IO_URING_BUFFERS_ERRORS_OK");
    if (test_registration_matrix() != 0 || test_file_backed_registration() != 0 ||
        test_fork_fixed_cow() != 0) {
        return 1;
    }
    puts("THEKERNEL_IO_URING_BUFFERS_REGISTRATION_OK");
    if (test_close_teardown() != 0) {
        return 1;
    }
    puts("THEKERNEL_IO_URING_BUFFERS_CLOSE_OK");
    if (test_fork_exec_teardown() != 0) {
        return 1;
    }
    puts("THEKERNEL_IO_URING_BUFFERS_EXEC_OK");
    if (test_slots_and_generation() != 0) {
        return 1;
    }
    puts("THEKERNEL_IO_URING_BUFFERS_GENERATION_OK");
    if (test_copy_fallback() != 0) {
        return 1;
    }
    puts("THEKERNEL_IO_URING_BUFFERS_COPY_OK");
    if (test_unmap_after_registration() != 0) {
        return 1;
    }
    puts("THEKERNEL_IO_URING_BUFFERS_MUNMAP_OK");
    if (test_multithreaded_stress() != 0) {
        return 1;
    }
    puts("THEKERNEL_IO_URING_BUFFERS_STRESS_OK rounds=50");
    if (test_ring_budget() != 0) {
        return 1;
    }
    puts("THEKERNEL_IO_URING_BUFFERS_BUDGET_OK");
    if (test_inflight_unregister() != 0) {
        return 1;
    }
    puts("THEKERNEL_IO_URING_BUFFERS_INFLIGHT_OK");
    puts("THEKERNEL_IO_URING_BUFFERS_OK");
    return 0;
}
