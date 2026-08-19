#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/uio.h>
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

#define IORING_SETUP_CQSIZE (1U << 3)
#define IORING_ENTER_GETEVENTS (1U << 0)
#define IORING_REGISTER_BUFFERS 0U
#define IORING_UNREGISTER_BUFFERS 1U
#define IORING_REGISTER_FILES 2U
#define IORING_UNREGISTER_FILES 3U
#define IORING_REGISTER_PROBE 8U
#define IORING_OP_NOP 0U
#define IORING_OP_READ_FIXED 4U
#define IORING_OP_WRITE_FIXED 5U
#define IORING_OP_FSYNC 3U
#define IORING_OP_POLL_ADD 6U
#define IORING_OP_ASYNC_CANCEL 14U
#define IORING_OP_READ 22U
#define IORING_OP_WRITE 23U
#define IORING_OP_LAST_LINUX_6_12 58U
#define IOSQE_FIXED_FILE (1U << 0)
#define IORING_OP_SUPPORTED (1U << 0)
#define IORING_FEATURES_EXPECTED 0x47U
#define IORING_PROBE_OPS 24U
#define POLLIN 0x0001U

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

struct io_uring_probe_op {
    uint8_t op;
    uint8_t resv;
    uint16_t flags;
    uint32_t resv2;
};

struct io_uring_probe {
    uint8_t last_op;
    uint8_t ops_len;
    uint16_t resv;
    uint32_t resv2[3];
    struct io_uring_probe_op ops[IORING_PROBE_OPS];
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

_Static_assert(sizeof(struct io_sqring_offsets) == 40, "bad SQ offsets ABI");
_Static_assert(sizeof(struct io_cqring_offsets) == 40, "bad CQ offsets ABI");
_Static_assert(sizeof(struct io_uring_params) == 120, "bad params ABI");
_Static_assert(sizeof(struct io_uring_cqe) == 16, "bad CQE ABI");
_Static_assert(sizeof(struct raw_sqe) == 64, "bad SQE ABI");
_Static_assert(sizeof(struct io_uring_probe_op) == 8, "bad probe op ABI");

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_IO_URING_FAIL %s errno=%d (%s)\n",
            stage, errno, strerror(errno));
    return 1;
}

static int fail_value(const char *stage, long actual, long expected) {
    fprintf(stderr, "THEKERNEL_IO_URING_FAIL %s actual=%ld expected=%ld\n",
            stage, actual, expected);
    return 1;
}

static size_t page_round(size_t value) {
    long page = sysconf(_SC_PAGESIZE);
    if (page <= 0 || (value > SIZE_MAX - (size_t)page + 1)) {
        return 0;
    }
    return (value + (size_t)page - 1) & ~((size_t)page - 1);
}

static uint32_t load_acquire(const unsigned char *base, uint32_t offset) {
    const _Atomic uint32_t *word =
        (const _Atomic uint32_t *)(const void *)(base + offset);
    return atomic_load_explicit(word, memory_order_acquire);
}

static void store_release(unsigned char *base, uint32_t offset, uint32_t value) {
    _Atomic uint32_t *word = (_Atomic uint32_t *)(void *)(base + offset);
    atomic_store_explicit(word, value, memory_order_release);
}

static void write_u32(unsigned char *bytes, size_t offset, uint32_t value) {
    memcpy(bytes + offset, &value, sizeof(value));
}

static void write_u16(unsigned char *bytes, size_t offset, uint16_t value) {
    memcpy(bytes + offset, &value, sizeof(value));
}

static void write_i32(unsigned char *bytes, size_t offset, int32_t value) {
    memcpy(bytes + offset, &value, sizeof(value));
}

static void write_u64(unsigned char *bytes, size_t offset, uint64_t value) {
    memcpy(bytes + offset, &value, sizeof(value));
}

static struct raw_sqe make_sqe(uint8_t opcode, uint64_t user_data) {
    struct raw_sqe sqe;
    memset(&sqe, 0, sizeof(sqe));
    sqe.bytes[0] = opcode;
    write_u64(sqe.bytes, 32, user_data);
    return sqe;
}

static int expect_setup_error(uint32_t entries, struct io_uring_params *params,
                              int expected_errno, const char *stage) {
    errno = 0;
    long result = syscall(SYS_io_uring_setup, entries, params);
    if (result != -1 || errno != expected_errno) {
        if (result >= 0) {
            close((int)result);
        }
        return fail_value(stage, result == -1 ? errno : result, expected_errno);
    }
    return 0;
}

static int map_ring(struct ring *ring) {
    const struct io_uring_params *params = &ring->params;
    size_t cq_end = (size_t)params->cq_off.cqes +
                    (size_t)params->cq_entries * sizeof(struct io_uring_cqe);
    size_t sq_end = (size_t)params->sq_off.array +
                    (size_t)params->sq_entries * sizeof(uint32_t);
    size_t ring_end = cq_end > sq_end ? cq_end : sq_end;
    ring->ring_bytes = page_round(ring_end);
    ring->sqe_bytes = page_round((size_t)params->sq_entries * sizeof(struct raw_sqe));
    if (ring->ring_bytes == 0 || ring->sqe_bytes == 0) {
        errno = EOVERFLOW;
        return fail("mapping-size");
    }

    errno = 0;
    void *invalid = mmap(NULL, ring->ring_bytes + (size_t)sysconf(_SC_PAGESIZE),
                         PROT_READ | PROT_WRITE, MAP_SHARED, ring->fd,
                         IORING_OFF_SQ_RING);
    if (invalid != MAP_FAILED || errno != EINVAL) {
        if (invalid != MAP_FAILED) {
            munmap(invalid, ring->ring_bytes + (size_t)sysconf(_SC_PAGESIZE));
        }
        return fail_value("mmap-wrong-length", errno, EINVAL);
    }
    errno = 0;
    invalid = mmap(NULL, ring->ring_bytes, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE, ring->fd, IORING_OFF_SQ_RING);
    if (invalid != MAP_FAILED || errno != EINVAL) {
        if (invalid != MAP_FAILED) {
            munmap(invalid, ring->ring_bytes);
        }
        return fail_value("mmap-private", errno, EINVAL);
    }
    errno = 0;
    invalid = mmap(NULL, ring->ring_bytes, PROT_READ | PROT_WRITE,
                   MAP_SHARED, ring->fd, 0x2000);
    if (invalid != MAP_FAILED || errno != EINVAL) {
        if (invalid != MAP_FAILED) {
            munmap(invalid, ring->ring_bytes);
        }
        return fail_value("mmap-wrong-offset", errno, EINVAL);
    }

    ring->sq_ring = mmap(NULL, ring->ring_bytes, PROT_READ | PROT_WRITE,
                         MAP_SHARED, ring->fd, IORING_OFF_SQ_RING);
    if (ring->sq_ring == MAP_FAILED) {
        return fail("mmap-sq-ring");
    }
    ring->cq_ring = mmap(NULL, ring->ring_bytes, PROT_READ | PROT_WRITE,
                         MAP_SHARED, ring->fd, IORING_OFF_CQ_RING);
    if (ring->cq_ring == MAP_FAILED) {
        return fail("mmap-cq-ring");
    }
    ring->sqes = mmap(NULL, ring->sqe_bytes, PROT_READ | PROT_WRITE,
                      MAP_SHARED, ring->fd, IORING_OFF_SQES);
    if (ring->sqes == MAP_FAILED) {
        return fail("mmap-sqes");
    }
    return 0;
}

static int queue_one(struct ring *ring, const struct raw_sqe *sqe) {
    const struct io_uring_params *params = &ring->params;
    uint32_t head = load_acquire(ring->sq_ring, params->sq_off.head);
    uint32_t tail = load_acquire(ring->sq_ring, params->sq_off.tail);
    if (tail - head >= params->sq_entries) {
        errno = EBUSY;
        return fail("sq-capacity");
    }
    uint32_t index = tail & load_acquire(ring->sq_ring, params->sq_off.ring_mask);
    ring->sqes[index] = *sqe;
    store_release(ring->sq_ring, params->sq_off.array + index * sizeof(uint32_t), index);
    store_release(ring->sq_ring, params->sq_off.tail, tail + 1);
    return 0;
}

static int submit_one(struct ring *ring, const struct raw_sqe *sqe,
                      uint64_t user_data, int32_t expected_result) {
    const struct io_uring_params *params = &ring->params;
    if (queue_one(ring, sqe)) {
        return 1;
    }

    long submitted = syscall(SYS_io_uring_enter, ring->fd, 1U, 1U,
                             IORING_ENTER_GETEVENTS, NULL, 0U);
    if (submitted != 1) {
        return fail_value("enter", submitted, 1);
    }

    uint32_t cq_head = load_acquire(ring->cq_ring, params->cq_off.head);
    uint32_t cq_tail = load_acquire(ring->cq_ring, params->cq_off.tail);
    if (cq_tail - cq_head != 1) {
        return fail_value("cq-count", cq_tail - cq_head, 1);
    }
    uint32_t cq_index = cq_head & load_acquire(ring->cq_ring, params->cq_off.ring_mask);
    const struct io_uring_cqe *cqe =
        (const struct io_uring_cqe *)(const void *)(ring->cq_ring +
            params->cq_off.cqes + cq_index * sizeof(*cqe));
    if (cqe->user_data != user_data) {
        return fail_value("cqe-user-data", (long)cqe->user_data, (long)user_data);
    }
    if (cqe->res != expected_result || cqe->flags != 0) {
        return fail_value("cqe-result", cqe->res, expected_result);
    }
    store_release(ring->cq_ring, params->cq_off.head, cq_head + 1);
    return 0;
}

static int test_immediate_poll(struct ring *ring) {
    int fds[2];
    if (pipe2(fds, O_CLOEXEC | O_NONBLOCK) != 0) {
        return fail("poll-pipe");
    }
    if (write(fds[1], "P", 1) != 1) {
        close(fds[0]);
        close(fds[1]);
        return fail("poll-write");
    }
    struct raw_sqe poll_sqe = make_sqe(IORING_OP_POLL_ADD, 0x504f4c4cULL);
    write_i32(poll_sqe.bytes, 4, fds[0]);
    write_u32(poll_sqe.bytes, 28, POLLIN);
    int failed = submit_one(ring, &poll_sqe, 0x504f4c4cULL, POLLIN);
    if (close(fds[0]) != 0 || close(fds[1]) != 0) {
        return fail("poll-close");
    }
    return failed;
}

static const struct io_uring_cqe *cqe_at(const struct ring *ring, uint32_t head) {
    const struct io_uring_params *params = &ring->params;
    uint32_t slot = head & load_acquire(ring->cq_ring, params->cq_off.ring_mask);
    return (const struct io_uring_cqe *)(const void *)(ring->cq_ring +
        params->cq_off.cqes + slot * sizeof(struct io_uring_cqe));
}

static int expect_cqe(const struct ring *ring, uint32_t head,
                      uint64_t user_data, int32_t result, const char *stage) {
    const struct io_uring_cqe *cqe = cqe_at(ring, head);
    if (cqe->user_data != user_data || cqe->res != result || cqe->flags != 0) {
        errno = EIO;
        return fail(stage);
    }
    return 0;
}

static int submit_pending_poll(struct ring *ring, int fd, uint64_t user_data) {
    struct raw_sqe poll_sqe = make_sqe(IORING_OP_POLL_ADD, user_data);
    write_i32(poll_sqe.bytes, 4, fd);
    write_u32(poll_sqe.bytes, 28, POLLIN);
    uint32_t cq_tail = load_acquire(ring->cq_ring, ring->params.cq_off.tail);
    if (queue_one(ring, &poll_sqe)) {
        return 1;
    }
    long submitted = syscall(SYS_io_uring_enter, ring->fd, 1U, 0U, 0U, NULL, 0U);
    if (submitted != 1 ||
        load_acquire(ring->cq_ring, ring->params.cq_off.tail) != cq_tail) {
        return fail_value("pending-poll-enter", submitted, 1);
    }
    return 0;
}

static int test_deferred_poll_and_cancel(struct ring *ring) {
    int ready_fds[2];
    if (pipe2(ready_fds, O_CLOEXEC | O_NONBLOCK) != 0) {
        return fail("deferred-poll-pipe");
    }
    const uint64_t ready_user_data = 0x57414b45ULL;
    uint32_t ready_head = load_acquire(ring->cq_ring, ring->params.cq_off.head);
    if (submit_pending_poll(ring, ready_fds[0], ready_user_data)) {
        return 1;
    }
    if (write(ready_fds[1], "W", 1) != 1) {
        return fail("deferred-poll-write");
    }
    long waited = syscall(SYS_io_uring_enter, ring->fd, 0U, 1U,
                          IORING_ENTER_GETEVENTS, NULL, 0U);
    if (waited != 0) {
        return fail_value("deferred-poll-wait", waited, 0);
    }
    uint32_t ready_tail = load_acquire(ring->cq_ring, ring->params.cq_off.tail);
    if (ready_tail - ready_head != 1 ||
        expect_cqe(ring, ready_head, ready_user_data, POLLIN,
                   "deferred-poll-cqe")) {
        return 1;
    }
    store_release(ring->cq_ring, ring->params.cq_off.head, ready_tail);
    if (close(ready_fds[0]) != 0 || close(ready_fds[1]) != 0) {
        return fail("deferred-poll-close");
    }

    int cancel_fds[2];
    if (pipe2(cancel_fds, O_CLOEXEC | O_NONBLOCK) != 0) {
        return fail("cancel-poll-pipe");
    }
    const uint64_t target_user_data = 0x544152474554ULL;
    const uint64_t cancel_user_data = 0x43414e43454cULL;
    uint32_t cancel_head = load_acquire(ring->cq_ring, ring->params.cq_off.head);
    if (submit_pending_poll(ring, cancel_fds[0], target_user_data)) {
        return 1;
    }
    struct raw_sqe cancel_sqe = make_sqe(IORING_OP_ASYNC_CANCEL, cancel_user_data);
    write_u64(cancel_sqe.bytes, 16, target_user_data);
    if (queue_one(ring, &cancel_sqe)) {
        return 1;
    }
    long cancelled = syscall(SYS_io_uring_enter, ring->fd, 1U, 0U, 0U, NULL, 0U);
    if (cancelled != 1) {
        return fail_value("cancel-enter", cancelled, 1);
    }
    uint32_t cancel_tail = load_acquire(ring->cq_ring, ring->params.cq_off.tail);
    if (cancel_tail - cancel_head != 2 ||
        expect_cqe(ring, cancel_head, target_user_data, -ECANCELED,
                   "cancel-target-cqe") ||
        expect_cqe(ring, cancel_head + 1, cancel_user_data, 0,
                   "cancel-request-cqe")) {
        return 1;
    }
    store_release(ring->cq_ring, ring->params.cq_off.head, cancel_tail);
    if (close(cancel_fds[0]) != 0 || close(cancel_fds[1]) != 0) {
        return fail("cancel-poll-close");
    }
    return 0;
}

static int test_cq_backpressure(struct ring *ring) {
    const struct io_uring_params *params = &ring->params;
    uint32_t cq_head = load_acquire(ring->cq_ring, params->cq_off.head);
    uint32_t base_tail = load_acquire(ring->cq_ring, params->cq_off.tail);
    if (cq_head != base_tail) {
        return fail_value("pressure-cq-not-empty", base_tail - cq_head, 0);
    }

    for (uint32_t index = 0; index < params->sq_entries; ++index) {
        struct raw_sqe nop = make_sqe(IORING_OP_NOP, 0x50520000ULL + index);
        if (queue_one(ring, &nop)) {
            return 1;
        }
        long submitted = syscall(SYS_io_uring_enter, ring->fd, 1U, 0U, 0U, NULL, 0U);
        if (submitted != 1) {
            return fail_value("pressure-fill-enter", submitted, 1);
        }
    }

    uint32_t full_tail = load_acquire(ring->cq_ring, params->cq_off.tail);
    if (full_tail - cq_head != params->sq_entries) {
        return fail_value("pressure-fill-count", full_tail - cq_head,
                          params->sq_entries);
    }
    struct raw_sqe pending = make_sqe(IORING_OP_NOP,
                                      0x50520000ULL + params->sq_entries);
    if (queue_one(ring, &pending)) {
        return 1;
    }
    uint32_t sq_head = load_acquire(ring->sq_ring, params->sq_off.head);
    long blocked = syscall(SYS_io_uring_enter, ring->fd, 1U, 0U, 0U, NULL, 0U);
    if (blocked != 0 || load_acquire(ring->sq_ring, params->sq_off.head) != sq_head) {
        return fail_value("pressure-backpressure", blocked, 0);
    }
    if (load_acquire(ring->sq_ring, params->sq_off.dropped) != 0 ||
        load_acquire(ring->cq_ring, params->cq_off.overflow) != 0) {
        errno = EOVERFLOW;
        return fail("pressure-no-drop");
    }

    for (uint32_t index = 0; index < params->sq_entries; ++index) {
        uint32_t slot = (cq_head + index) &
                        load_acquire(ring->cq_ring, params->cq_off.ring_mask);
        const struct io_uring_cqe *cqe =
            (const struct io_uring_cqe *)(const void *)(ring->cq_ring +
                params->cq_off.cqes + slot * sizeof(*cqe));
        if (cqe->user_data != 0x50520000ULL + index || cqe->res != 0) {
            return fail_value("pressure-cqe", index, cqe->res);
        }
    }
    store_release(ring->cq_ring, params->cq_off.head, full_tail);
    long resumed = syscall(SYS_io_uring_enter, ring->fd, 1U, 1U,
                           IORING_ENTER_GETEVENTS, NULL, 0U);
    if (resumed != 1) {
        return fail_value("pressure-resume", resumed, 1);
    }
    uint32_t resumed_tail = load_acquire(ring->cq_ring, params->cq_off.tail);
    if (resumed_tail - full_tail != 1) {
        return fail_value("pressure-resume-count", resumed_tail - full_tail, 1);
    }
    uint32_t resumed_slot = full_tail &
                            load_acquire(ring->cq_ring, params->cq_off.ring_mask);
    const struct io_uring_cqe *resumed_cqe =
        (const struct io_uring_cqe *)(const void *)(ring->cq_ring +
            params->cq_off.cqes + resumed_slot * sizeof(*resumed_cqe));
    if (resumed_cqe->user_data != 0x50520000ULL + params->sq_entries ||
        resumed_cqe->res != 0) {
        errno = EIO;
        return fail("pressure-resume-cqe");
    }
    store_release(ring->cq_ring, params->cq_off.head, resumed_tail);
    return 0;
}

static int test_default_batch_stops_on_submission_failure(struct ring *ring) {
    const struct io_uring_params *params = &ring->params;
    uint32_t sq_head = load_acquire(ring->sq_ring, params->sq_off.head);
    uint32_t cq_head = load_acquire(ring->cq_ring, params->cq_off.head);
    uint32_t cq_tail = load_acquire(ring->cq_ring, params->cq_off.tail);
    if (cq_head != cq_tail) {
        return fail_value("batch-cq-not-empty", cq_tail - cq_head, 0);
    }

    const uint64_t failed_user_data = 0x4241544348464149ULL;
    const uint64_t next_user_data = 0x42415443484e4558ULL;
    struct raw_sqe unsupported = make_sqe(IORING_OP_FSYNC, failed_user_data);
    struct raw_sqe next = make_sqe(IORING_OP_NOP, next_user_data);
    if (queue_one(ring, &unsupported) || queue_one(ring, &next)) {
        return 1;
    }

    long stopped = syscall(SYS_io_uring_enter, ring->fd, 2U, 0U, 0U, NULL, 0U);
    uint32_t first_tail = load_acquire(ring->cq_ring, params->cq_off.tail);
    if (stopped != 1) {
        return fail_value("batch-stop", stopped, 1);
    }
    uint32_t stopped_head = load_acquire(ring->sq_ring, params->sq_off.head);
    if (stopped_head != sq_head + 1U) {
        return fail_value("batch-stop-sq-head", stopped_head - sq_head, 1);
    }
    if (first_tail - cq_head != 1U) {
        return fail_value("batch-stop-cq-count", first_tail - cq_head, 1);
    }
    if (expect_cqe(ring, cq_head, failed_user_data, -EOPNOTSUPP,
                   "batch-failed-cqe")) {
        return 1;
    }
    store_release(ring->cq_ring, params->cq_off.head, first_tail);

    long resumed = syscall(SYS_io_uring_enter, ring->fd, 1U, 1U,
                           IORING_ENTER_GETEVENTS, NULL, 0U);
    uint32_t resumed_tail = load_acquire(ring->cq_ring, params->cq_off.tail);
    if (resumed != 1) {
        return fail_value("batch-resume", resumed, 1);
    }
    uint32_t resumed_head = load_acquire(ring->sq_ring, params->sq_off.head);
    if (resumed_head != sq_head + 2U) {
        return fail_value("batch-resume-sq-head", resumed_head - sq_head, 2);
    }
    if (resumed_tail - first_tail != 1U) {
        return fail_value("batch-resume-cq-count", resumed_tail - first_tail, 1);
    }
    if (expect_cqe(ring, first_tail, next_user_data, 0, "batch-resumed-cqe")) {
        return 1;
    }
    store_release(ring->cq_ring, params->cq_off.head, resumed_tail);
    return 0;
}

static int test_probe(struct ring *ring) {
    struct io_uring_probe probe;
    /* REGISTER_PROBE is a pure output operation; reused storage need not be zero. */
    memset(&probe, 0xa5, sizeof(probe));
    if (syscall(SYS_io_uring_register, ring->fd, IORING_REGISTER_PROBE,
                &probe, IORING_PROBE_OPS) != 0) {
        return fail("register-probe");
    }
    if (probe.last_op != IORING_OP_LAST_LINUX_6_12 - 1 ||
        probe.ops_len != IORING_PROBE_OPS) {
        return fail_value("probe-header", probe.last_op,
                          IORING_OP_LAST_LINUX_6_12 - 1);
    }
    for (uint32_t opcode = 0; opcode < IORING_PROBE_OPS; ++opcode) {
        int supported = opcode == IORING_OP_NOP || opcode == IORING_OP_READ_FIXED ||
                        opcode == IORING_OP_WRITE_FIXED || opcode == IORING_OP_POLL_ADD ||
                        opcode == IORING_OP_ASYNC_CANCEL || opcode == IORING_OP_READ ||
                        opcode == IORING_OP_WRITE;
        uint16_t expected = supported ? IORING_OP_SUPPORTED : 0;
        if (probe.ops[opcode].op != opcode || probe.ops[opcode].flags != expected) {
            return fail_value("probe-op", opcode, expected);
        }
    }
    return 0;
}

static int expect_register_error(struct ring *ring, uint32_t opcode,
                                 const void *argument, uint32_t count,
                                 int expected_errno, const char *stage) {
    errno = 0;
    long result = syscall(SYS_io_uring_register, ring->fd, opcode, argument, count);
    if (result != -1 || errno != expected_errno) {
        return fail_value(stage, result == -1 ? errno : result, expected_errno);
    }
    return 0;
}

static long long read_io_counter(const char *key) {
    char line[256];
    char buffer[512];
    size_t key_len = strlen(key);
    size_t line_len = 0;
    int line_overflow = 0;
    int fd = open("/proc/io_stats", O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    for (;;) {
        ssize_t length = read(fd, buffer, sizeof(buffer));
        if (length < 0 && errno == EINTR) {
            continue;
        }
        if (length < 0) {
            close(fd);
            return -1;
        }
        if (length == 0) {
            break;
        }
        for (ssize_t index = 0; index < length; ++index) {
            char value = buffer[index];
            if (value == '\n') {
                if (!line_overflow) {
                    line[line_len] = '\0';
                    if (strncmp(line, key, key_len) == 0 && line[key_len] == ' ') {
                        long long result = strtoll(line + key_len + 1, NULL, 10);
                        close(fd);
                        return result;
                    }
                }
                line_len = 0;
                line_overflow = 0;
            } else if (!line_overflow) {
                if (line_len + 1 < sizeof(line)) {
                    line[line_len++] = value;
                } else {
                    line_overflow = 1;
                }
            }
        }
    }
    if (!line_overflow && line_len != 0) {
        line[line_len] = '\0';
        if (strncmp(line, key, key_len) == 0 && line[key_len] == ' ') {
            long long result = strtoll(line + key_len + 1, NULL, 10);
            close(fd);
            return result;
        }
    }
    close(fd);
    return -1;
}

enum { IO_DIAGNOSTICS_OK = 0, IO_DIAGNOSTICS_UNAVAILABLE = 1 };

static int write_io_test_control_command(const char *command) {
    size_t length = strlen(command);
    int fd = open("/proc/io_test_control", O_WRONLY | O_CLOEXEC);
    if (fd < 0) {
        if (errno == ENOENT) {
            return IO_DIAGNOSTICS_UNAVAILABLE;
        }
        return -1;
    }
    ssize_t written = write(fd, command, length);
    close(fd);
    return written == (ssize_t)length ? 0 : -1;
}

static int test_registered_buffers(struct ring *ring) {
    static const char payload[] = "thekernel-io-uring\n";
    long page_value = sysconf(_SC_PAGESIZE);
    if (page_value <= 0) {
        errno = EINVAL;
        return fail("buffers-page-size");
    }
    size_t page = (size_t)page_value;
    char *input = mmap(NULL, page, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *output = mmap(NULL, page, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (input == MAP_FAILED || output == MAP_FAILED) {
        if (input != MAP_FAILED) {
            munmap(input, page);
        }
        if (output != MAP_FAILED) {
            munmap(output, page);
        }
        return fail("buffers-mmap");
    }
    memset(input, 0, page);
    memset(output, 0, page);
    memcpy(input + 7, payload, sizeof(payload));

    int result = 1;
    int file = -1;
    int files_registered = 0;
    int buffers_registered = 0;
    struct iovec iov[2] = {
        {.iov_base = input, .iov_len = page},
        {.iov_base = output, .iov_len = page},
    };
    if (expect_register_error(ring, IORING_REGISTER_BUFFERS, NULL, 1U,
                              EFAULT, "buffers-null")) {
        goto out;
    }
    if (syscall(SYS_io_uring_register, ring->fd, IORING_REGISTER_BUFFERS,
                iov, 2U) != 0) {
        fail("buffers-register");
        goto out;
    }
    buffers_registered = 1;

    file = open("/tmp/thekernel-io-uring", O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0600);
    if (file < 0) {
        fail("fixed-file-open");
        goto out;
    }
    if (syscall(SYS_io_uring_register, ring->fd, IORING_REGISTER_FILES,
                &file, 1U) != 0) {
        fail("register-files");
        goto out;
    }
    files_registered = 1;
    if (close(file) != 0) {
        fail("fixed-file-close-original");
        file = -1;
        goto out;
    }
    file = -1;

    struct raw_sqe write_sqe = make_sqe(IORING_OP_WRITE_FIXED, 0x5752495445ULL);
    write_sqe.bytes[1] = IOSQE_FIXED_FILE;
    write_i32(write_sqe.bytes, 4, 0);
    write_u64(write_sqe.bytes, 8, 0);
    write_u64(write_sqe.bytes, 16, (uintptr_t)(input + 7));
    write_u32(write_sqe.bytes, 24, (uint32_t)sizeof(payload));
    write_u16(write_sqe.bytes, 40, 0);
    if (submit_one(ring, &write_sqe, 0x5752495445ULL, (int32_t)sizeof(payload))) {
        goto out;
    }

    struct raw_sqe read_sqe = make_sqe(IORING_OP_READ_FIXED, 0x52454144ULL);
    read_sqe.bytes[1] = IOSQE_FIXED_FILE;
    write_i32(read_sqe.bytes, 4, 0);
    write_u64(read_sqe.bytes, 8, 0);
    write_u64(read_sqe.bytes, 16, (uintptr_t)(output + 11));
    write_u32(read_sqe.bytes, 24, (uint32_t)sizeof(payload));
    write_u16(read_sqe.bytes, 40, 1);
    if (submit_one(ring, &read_sqe, 0x52454144ULL, (int32_t)sizeof(payload))) {
        goto out;
    }
    if (memcmp(output + 11, payload, sizeof(payload)) != 0) {
        errno = EIO;
        fail("fixed-buffer-contents");
        goto out;
    }

    struct raw_sqe bad_index = make_sqe(IORING_OP_READ_FIXED, 0x424144494458ULL);
    bad_index.bytes[1] = IOSQE_FIXED_FILE;
    write_i32(bad_index.bytes, 4, 0);
    write_u64(bad_index.bytes, 8, 0);
    write_u64(bad_index.bytes, 16, (uintptr_t)output);
    write_u32(bad_index.bytes, 24, 1U);
    write_u16(bad_index.bytes, 40, 2U);
    if (submit_one(ring, &bad_index, 0x424144494458ULL, -EFAULT)) {
        goto out;
    }
    if (syscall(SYS_io_uring_register, ring->fd, IORING_UNREGISTER_FILES,
                NULL, 0U) != 0) {
        fail("unregister-files");
        goto out;
    }
    files_registered = 0;
    if (syscall(SYS_io_uring_register, ring->fd, IORING_UNREGISTER_BUFFERS,
                NULL, 0U) != 0) {
        fail("unregister-buffers");
        goto out;
    }
    buffers_registered = 0;
    if (unlink("/tmp/thekernel-io-uring") != 0) {
        fail("fixed-file-unlink");
        goto out;
    }
    result = 0;
out:
    if (file >= 0) {
        close(file);
    }
    if (files_registered) {
        syscall(SYS_io_uring_register, ring->fd, IORING_UNREGISTER_FILES,
                NULL, 0U);
    }
    if (buffers_registered) {
        syscall(SYS_io_uring_register, ring->fd, IORING_UNREGISTER_BUFFERS,
                NULL, 0U);
    }
    if (munmap(input, page) != 0 || munmap(output, page) != 0) {
        return fail("buffers-munmap");
    }
    return result;
}

static int test_dma_direct_fixed_buffers(struct ring *ring,
                                         int *diagnostics_available) {
    enum { DMA_BYTES = 256 * 1024 };
    static const char path[] = "/thekernel-io-uring-dma-direct";
    unsigned char *private_buffer = MAP_FAILED;
    unsigned char *shared_buffer = MAP_FAILED;
    struct iovec iov;
    int file = -1;
    int files_registered = 0;
    int buffers_registered = 0;
    int diagnostics_enabled = 0;
    int result = 1;
    *diagnostics_available = 0;

    long page_value = sysconf(_SC_PAGESIZE);
    if (page_value <= 0 || (size_t)page_value < 4096) {
        errno = EINVAL;
        return fail("dma-page-size");
    }
    private_buffer = mmap(NULL, DMA_BYTES, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (private_buffer == MAP_FAILED ||
        ((uintptr_t)private_buffer % (size_t)page_value) != 0) {
        if (private_buffer != MAP_FAILED) {
            munmap(private_buffer, DMA_BYTES);
        }
        errno = EFAULT;
        return fail("dma-private-buffer-alignment");
    }

    memset(private_buffer, 0xa5, DMA_BYTES);
    int ordinary = open(path, O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0600);
    if (ordinary < 0) {
        fail("dma-seed-open");
        goto out;
    }
    size_t written = 0;
    while (written < DMA_BYTES) {
        ssize_t count = write(ordinary, private_buffer + written, DMA_BYTES - written);
        if (count < 0 && errno == EINTR) {
            continue;
        }
        if (count <= 0) {
            fail("dma-seed-write");
            close(ordinary);
            goto out;
        }
        written += (size_t)count;
    }
    if (fsync(ordinary) != 0 || close(ordinary) != 0) {
        fail("dma-seed-fsync");
        goto out;
    }

    file = open(path, O_RDWR | O_DIRECT | O_CLOEXEC);
    if (file < 0) {
        fail("dma-direct-open");
        goto out;
    }
    int control_result = write_io_test_control_command("counters=on\n");
    if (control_result == IO_DIAGNOSTICS_OK) {
        diagnostics_enabled = 1;
        control_result = write_io_test_control_command("counters=reset\n");
    }
    if (control_result == IO_DIAGNOSTICS_UNAVAILABLE) {
        *diagnostics_available = 0;
    } else if (control_result != IO_DIAGNOSTICS_OK) {
        fail("dma-counter-control");
        goto out;
    } else {
        *diagnostics_available = 1;
    }

    iov.iov_base = private_buffer;
    iov.iov_len = DMA_BYTES;
    if (syscall(SYS_io_uring_register, ring->fd, IORING_REGISTER_BUFFERS,
                &iov, 1U) != 0) {
        fail("dma-private-register-buffer");
        goto out;
    }
    buffers_registered = 1;
    if (syscall(SYS_io_uring_register, ring->fd, IORING_REGISTER_FILES,
                &file, 1U) != 0) {
        fail("dma-register-file");
        goto out;
    }
    files_registered = 1;
    if (close(file) != 0) {
        fail("dma-close-original");
        file = -1;
        goto out;
    }
    file = -1;

    static const size_t dma_lengths[] = {4096, 16 * 1024, 64 * 1024, DMA_BYTES};
    static const size_t dma_offsets[] = {4096, 32 * 1024, 96 * 1024, 0};
    for (size_t test = 0; test < sizeof(dma_lengths) / sizeof(dma_lengths[0]); ++test) {
        const size_t length = dma_lengths[test];
        const size_t offset = dma_offsets[test];
        const unsigned char marker = (unsigned char)(0x50 + test);
        memset(private_buffer, marker, length);
        long long write_hits_before = 0;
        long long write_bytes_before = 0;
        if (*diagnostics_available) {
            write_hits_before = read_io_counter("io_uring.dma_direct_write_hits");
            write_bytes_before = read_io_counter("io_uring.dma_direct_write_bytes");
            if (write_hits_before < 0 || write_bytes_before < 0) {
                fail("dma-write-counter-read");
                goto out;
            }
        }
        uint64_t write_tag = 0x444d415700000000ULL | (uint64_t)test;
        struct raw_sqe write_sqe = make_sqe(IORING_OP_WRITE_FIXED, write_tag);
        write_sqe.bytes[1] = IOSQE_FIXED_FILE;
        write_i32(write_sqe.bytes, 4, 0);
        write_u64(write_sqe.bytes, 8, offset);
        write_u64(write_sqe.bytes, 16, (uintptr_t)private_buffer);
        write_u32(write_sqe.bytes, 24, length);
        write_u16(write_sqe.bytes, 40, 0);
        if (submit_one(ring, &write_sqe, write_tag, length) != 0) {
            goto out;
        }
        if (*diagnostics_available) {
            long long write_hits_after =
                read_io_counter("io_uring.dma_direct_write_hits");
            long long write_bytes_after =
                read_io_counter("io_uring.dma_direct_write_bytes");
            if (write_hits_after <= write_hits_before ||
                write_bytes_after < write_bytes_before + (long long)length) {
                fail_value("dma-write-counter", write_hits_after, write_hits_before + 1);
                goto out;
            }
        }

        memset(private_buffer, 0, length);
        long long read_hits_before = 0;
        long long read_bytes_before = 0;
        if (*diagnostics_available) {
            read_hits_before = read_io_counter("io_uring.dma_direct_read_hits");
            read_bytes_before = read_io_counter("io_uring.dma_direct_read_bytes");
            if (read_hits_before < 0 || read_bytes_before < 0) {
                fail("dma-read-counter-read");
                goto out;
            }
        }
        uint64_t read_tag = 0x444d415200000000ULL | (uint64_t)test;
        struct raw_sqe read_sqe = make_sqe(IORING_OP_READ_FIXED, read_tag);
        read_sqe.bytes[1] = IOSQE_FIXED_FILE;
        write_i32(read_sqe.bytes, 4, 0);
        write_u64(read_sqe.bytes, 8, offset);
        write_u64(read_sqe.bytes, 16, (uintptr_t)private_buffer);
        write_u32(read_sqe.bytes, 24, length);
        write_u16(read_sqe.bytes, 40, 0);
        if (submit_one(ring, &read_sqe, read_tag, length) != 0) {
            goto out;
        }
        for (size_t index = 0; index < length; ++index) {
            if (private_buffer[index] != marker) {
                errno = EIO;
                fail("dma-read-contents");
                goto out;
            }
        }
        if (*diagnostics_available) {
            long long read_hits_after = read_io_counter("io_uring.dma_direct_read_hits");
            long long read_bytes_after = read_io_counter("io_uring.dma_direct_read_bytes");
            if (read_hits_after <= read_hits_before ||
                read_bytes_after < read_bytes_before + (long long)length) {
                fail_value("dma-read-counter", read_hits_after, read_hits_before + 1);
                goto out;
            }
        }
    }

    if (syscall(SYS_io_uring_register, ring->fd, IORING_UNREGISTER_BUFFERS,
                NULL, 0U) != 0) {
        fail("dma-private-unregister-buffer");
        goto out;
    }
    buffers_registered = 0;
    shared_buffer = mmap(NULL, DMA_BYTES, PROT_READ | PROT_WRITE,
                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared_buffer == MAP_FAILED ||
        ((uintptr_t)shared_buffer % (size_t)page_value) != 0) {
        if (shared_buffer != MAP_FAILED) {
            munmap(shared_buffer, DMA_BYTES);
        }
        errno = EFAULT;
        fail("dma-shared-buffer-alignment");
        goto out;
    }
    memset(shared_buffer, 0x3c, DMA_BYTES);
    iov.iov_base = shared_buffer;
    if (syscall(SYS_io_uring_register, ring->fd, IORING_REGISTER_BUFFERS,
                &iov, 1U) != 0) {
        fail("dma-shared-register-buffer");
        goto out;
    }
    buffers_registered = 1;
    long long write_hits_ineligible = 0;
    long long write_fallbacks_ineligible = 0;
    long long write_provenance_fallbacks_ineligible = 0;
    if (*diagnostics_available) {
        write_hits_ineligible = read_io_counter("io_uring.dma_direct_write_hits");
        write_fallbacks_ineligible =
            read_io_counter("io_uring.dma_direct_write_fallbacks");
        write_provenance_fallbacks_ineligible =
            read_io_counter("io_uring.dma_direct_write_fallback_provenance");
        if (write_hits_ineligible < 0 || write_fallbacks_ineligible < 0 ||
            write_provenance_fallbacks_ineligible < 0) {
            fail("dma-ineligible-write-counter-read");
            goto out;
        }
    }
    struct raw_sqe shared_write = make_sqe(IORING_OP_WRITE_FIXED, 0x444d415348525754ULL);
    shared_write.bytes[1] = IOSQE_FIXED_FILE;
    write_i32(shared_write.bytes, 4, 0);
    write_u64(shared_write.bytes, 8, 0);
    write_u64(shared_write.bytes, 16, (uintptr_t)shared_buffer);
    write_u32(shared_write.bytes, 24, DMA_BYTES);
    write_u16(shared_write.bytes, 40, 0);
    if (submit_one(ring, &shared_write, 0x444d415348525754ULL, DMA_BYTES) != 0) {
        goto out;
    }
    if (*diagnostics_available) {
        long long write_hits_after =
            read_io_counter("io_uring.dma_direct_write_hits");
        long long write_fallbacks_after =
            read_io_counter("io_uring.dma_direct_write_fallbacks");
        long long write_provenance_fallbacks_after =
            read_io_counter("io_uring.dma_direct_write_fallback_provenance");
        if (write_hits_after != write_hits_ineligible ||
            write_fallbacks_after != write_fallbacks_ineligible + 1 ||
            write_provenance_fallbacks_after !=
                write_provenance_fallbacks_ineligible + 1) {
            errno = EIO;
            fail("dma-ineligible-write-counters");
            goto out;
        }
    }
    memset(shared_buffer, 0, DMA_BYTES);
    long long read_hits_ineligible = 0;
    long long read_fallbacks_ineligible = 0;
    long long read_provenance_fallbacks_ineligible = 0;
    if (*diagnostics_available) {
        read_hits_ineligible = read_io_counter("io_uring.dma_direct_read_hits");
        read_fallbacks_ineligible =
            read_io_counter("io_uring.dma_direct_read_fallbacks");
        read_provenance_fallbacks_ineligible =
            read_io_counter("io_uring.dma_direct_read_fallback_provenance");
        if (read_hits_ineligible < 0 || read_fallbacks_ineligible < 0 ||
            read_provenance_fallbacks_ineligible < 0) {
            fail("dma-ineligible-read-counter-read");
            goto out;
        }
    }
    struct raw_sqe shared_read = make_sqe(IORING_OP_READ_FIXED, 0x444d415348524454ULL);
    shared_read.bytes[1] = IOSQE_FIXED_FILE;
    write_i32(shared_read.bytes, 4, 0);
    write_u64(shared_read.bytes, 8, 0);
    write_u64(shared_read.bytes, 16, (uintptr_t)shared_buffer);
    write_u32(shared_read.bytes, 24, DMA_BYTES);
    write_u16(shared_read.bytes, 40, 0);
    if (submit_one(ring, &shared_read, 0x444d415348524454ULL, DMA_BYTES) != 0) {
        goto out;
    }
    for (size_t index = 0; index < DMA_BYTES; ++index) {
        if (shared_buffer[index] != 0x3c) {
            errno = EIO;
            fail("dma-ineligible-read-contents");
            goto out;
        }
    }
    if (*diagnostics_available) {
        long long read_hits_after =
            read_io_counter("io_uring.dma_direct_read_hits");
        long long read_fallbacks_after =
            read_io_counter("io_uring.dma_direct_read_fallbacks");
        long long read_provenance_fallbacks_after =
            read_io_counter("io_uring.dma_direct_read_fallback_provenance");
        if (read_hits_after != read_hits_ineligible ||
            read_fallbacks_after != read_fallbacks_ineligible + 1 ||
            read_provenance_fallbacks_after !=
                read_provenance_fallbacks_ineligible + 1) {
            errno = EIO;
            fail("dma-ineligible-read-counters");
            goto out;
        }
    }

    if (syscall(SYS_io_uring_register, ring->fd, IORING_UNREGISTER_FILES,
                NULL, 0U) != 0) {
        fail("dma-unregister-files");
        goto out;
    }
    files_registered = 0;
    if (syscall(SYS_io_uring_register, ring->fd, IORING_UNREGISTER_BUFFERS,
                NULL, 0U) != 0) {
        fail("dma-shared-unregister-buffer");
        goto out;
    }
    buffers_registered = 0;
    result = 0;
out:
    if (file >= 0) {
        close(file);
    }
    if (files_registered) {
        syscall(SYS_io_uring_register, ring->fd, IORING_UNREGISTER_FILES,
                NULL, 0U);
    }
    if (buffers_registered) {
        syscall(SYS_io_uring_register, ring->fd, IORING_UNREGISTER_BUFFERS,
                NULL, 0U);
    }
    if (private_buffer != MAP_FAILED) {
        munmap(private_buffer, DMA_BYTES);
    }
    if (shared_buffer != MAP_FAILED) {
        munmap(shared_buffer, DMA_BYTES);
    }
    if (diagnostics_enabled) {
        int control_result = write_io_test_control_command("counters=off\n");
        if (result == 0 && control_result != IO_DIAGNOSTICS_OK) {
            fail("dma-counter-off");
            result = 1;
        }
    }
    if (unlink(path) != 0 && errno != ENOENT) {
        if (result == 0) {
            return fail("dma-unlink");
        }
        return result;
    }
    return result;
}

static int test_ring(int *dma_direct_diagnostics_available) {
    struct io_uring_params invalid;
    memset(&invalid, 0, sizeof(invalid));
    if (expect_setup_error(0, &invalid, EINVAL, "setup-zero")) {
        return 1;
    }
    invalid.resv[0] = 1;
    if (expect_setup_error(8, &invalid, EINVAL, "setup-reserved")) {
        return 1;
    }

    struct ring ring;
    memset(&ring, 0, sizeof(ring));
    ring.params.flags = IORING_SETUP_CQSIZE;
    ring.params.cq_entries = 8U;
    ring.fd = (int)syscall(SYS_io_uring_setup, 8U, &ring.params);
    if (ring.fd < 0) {
        return fail("setup");
    }
    if (ring.params.sq_entries != 8 || ring.params.cq_entries != 8 ||
        ring.params.features != IORING_FEATURES_EXPECTED ||
        ring.params.sq_off.array == 0) {
        return fail_value("setup-layout", ring.params.features,
                          IORING_FEATURES_EXPECTED);
    }
    if (map_ring(&ring)) {
        return 1;
    }

    struct raw_sqe nop = make_sqe(IORING_OP_NOP, 0x4e4f50ULL);
    if (submit_one(&ring, &nop, 0x4e4f50ULL, 0) ||
        test_default_batch_stops_on_submission_failure(&ring) ||
        test_probe(&ring) || test_registered_buffers(&ring) ||
        test_dma_direct_fixed_buffers(&ring, dma_direct_diagnostics_available) ||
        test_immediate_poll(&ring) || test_deferred_poll_and_cancel(&ring) ||
        test_cq_backpressure(&ring)) {
        return 1;
    }

    uint32_t sq_tail = load_acquire(ring.sq_ring, ring.params.sq_off.tail);
    if (load_acquire(ring.cq_ring, ring.params.sq_off.tail) != sq_tail) {
        errno = EIO;
        return fail("single-mmap-alias");
    }
    if (close(ring.fd) != 0) {
        return fail("ring-close");
    }
    if (load_acquire(ring.cq_ring, ring.params.sq_off.tail) != sq_tail) {
        errno = EIO;
        return fail("mapping-lifetime");
    }
    if (munmap(ring.sqes, ring.sqe_bytes) != 0 ||
        munmap(ring.cq_ring, ring.ring_bytes) != 0 ||
        munmap(ring.sq_ring, ring.ring_bytes) != 0) {
        return fail("munmap");
    }
    return 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);
    int dma_direct_diagnostics_available = 0;
    if (test_ring(&dma_direct_diagnostics_available)) {
        return 1;
    }
    puts("THEKERNEL_IO_URING_OK");
    if (dma_direct_diagnostics_available) {
        puts("THEKERNEL_IO_URING_DMA_DIRECT_OK");
    }
    return 0;
}
