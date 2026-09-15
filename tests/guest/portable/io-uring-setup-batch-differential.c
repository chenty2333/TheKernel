#define _GNU_SOURCE

#include <errno.h>
#include <linux/io_uring.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

/*
 * Differential coverage for the `io_uring_setup()` flag contract and for the
 * `io_submit_sqes()` batch contract of Linux v7.2.3.
 *
 * `SYSCALL_DEFINE2(io_uring_setup)` (`io_uring/io_uring.c:3106-3121`) copies
 * `struct io_uring_params`, rejects a non-zero `resv[3]` with -EINVAL and
 * stores the `entries` argument in `sq_entries`.  `io_prepare_config()`
 * (`io_uring/io_uring.c:2863-2900`) then runs:
 *
 *   - `io_uring_sanitise_params()` (`io_uring/io_uring.c:2797-2856`): an
 *     unknown flag bit, `SQPOLL` together with `COOP_TASKRUN`,
 *     `TASKRUN_FLAG` or `DEFER_TASKRUN`, `DEFER_TASKRUN` without
 *     `SINGLE_ISSUER`, `TASKRUN_FLAG` without `COOP_TASKRUN`/`DEFER_TASKRUN`,
 *     `HYBRID_IOPOLL` without `IOPOLL`, `REGISTERED_FD_ONLY` without
 *     `NO_MMAP`, `CQE32` with `CQE_MIXED`, `SQE128` with `SQE_MIXED` and
 *     `SQ_REWIND` with `SQPOLL` or without `NO_SQARRAY` are all -EINVAL.
 *   - `io_uring_fill_params()` (`io_uring/io_uring.c:2858-2898`): zero
 *     entries are -EINVAL, oversized entries need `CLAMP` (they are clamped
 *     to `IORING_MAX_ENTRIES`, else -EINVAL), `CQSIZE` demands a non-zero
 *     `cq_entries` that rounds up to at least the SQ ring size, and without
 *     `CQSIZE` the CQ ring is twice the SQ ring.
 *   - `io_sq_offload_create()` (`io_uring/sqpoll.c:527-530`): `SQ_AFF`
 *     without `SQPOLL` is -EINVAL.
 *
 * `IORING_SETUP_SUBMIT_ALL` (`(1U << 7)`) is part of that vocabulary and is
 * the only flag that changes how a batch is consumed: `io_submit_sqes()`
 * (`io_uring/io_uring.c:2020-2076`) stops after the first SQE whose
 * `io_submit_sqe()` failed unless the ring carries it, and the failed SQE
 * itself stays consumed with its own completion:
 *
 *       if (unlikely(io_submit_sqe(ctx, req, sqe, &left)) &&
 *           !(ctx->flags & IORING_SETUP_SUBMIT_ALL)) {
 *               left--;
 *               break;
 *       }
 *
 * so a two-SQE batch whose first SQE cannot be initialized returns 1 and
 * leaves the second SQE pending, while the same batch on a `SUBMIT_ALL` ring
 * returns 2 after completing both.
 */

#define IORING_MAX_ENTRIES_ORACLE 32768U
#define IORING_SETUP_UNKNOWN_BIT (1U << 21)

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_IO_URING_SETUP_BATCH_FAIL %s errno=%d (%s)\n",
            stage, errno, strerror(errno));
    return 1;
}

static int expect_errno(const char *stage, long result, int expected) {
    if (result != -1 || errno != expected) {
        fprintf(stderr,
                "THEKERNEL_IO_URING_SETUP_BATCH_FAIL %s result=%ld errno=%d "
                "expected=%d\n",
                stage, result, errno, expected);
        return 1;
    }
    return 0;
}

static long do_setup(unsigned entries, struct io_uring_params *params) {
    return syscall(SYS_io_uring_setup, entries, params);
}

static long do_enter(int ring, unsigned to_submit) {
    return syscall(SYS_io_uring_enter, ring, to_submit, 0, 0, NULL, 0);
}

struct ring_view {
    struct io_uring_params params;
    void *ring_ptr;
    struct io_uring_sqe *sqes;
    unsigned *sq_head;
    unsigned *sq_tail;
    unsigned *sq_mask;
    unsigned *sq_array;
    unsigned *cq_head;
    unsigned *cq_tail;
    unsigned *cq_mask;
    struct io_uring_cqe *cqes;
};

static int ring_map(int ring, struct ring_view *view) {
    unsigned char *ring_bytes;
    struct io_uring_params *params = &view->params;
    size_t sq_ring_bytes =
        params->sq_off.array + params->sq_entries * sizeof(unsigned);
    size_t cq_ring_bytes =
        params->cq_off.cqes + params->cq_entries * sizeof(struct io_uring_cqe);
    size_t ring_bytes_len = sq_ring_bytes > cq_ring_bytes ? sq_ring_bytes
                                                          : cq_ring_bytes;
    size_t sqe_bytes = params->sq_entries * sizeof(struct io_uring_sqe);

    /*
     * One region holds both rings on every kernel that advertises
     * IORING_FEAT_SINGLE_MMAP, and mapping it once with the larger of the two
     * lengths exposes both views.
     */
    view->ring_ptr = mmap(NULL, ring_bytes_len, PROT_READ | PROT_WRITE,
                          MAP_SHARED | MAP_POPULATE, ring, IORING_OFF_SQ_RING);
    if (view->ring_ptr == MAP_FAILED)
        return fail("ring-mmap");
    ring_bytes = view->ring_ptr;
    view->sqes = mmap(NULL, sqe_bytes, PROT_READ | PROT_WRITE,
                      MAP_SHARED | MAP_POPULATE, ring, IORING_OFF_SQES);
    if (view->sqes == MAP_FAILED)
        return fail("sqe-mmap");

    view->sq_head = (unsigned *)(ring_bytes + params->sq_off.head);
    view->sq_tail = (unsigned *)(ring_bytes + params->sq_off.tail);
    view->sq_mask = (unsigned *)(ring_bytes + params->sq_off.ring_mask);
    view->sq_array = (unsigned *)(ring_bytes + params->sq_off.array);
    view->cq_head = (unsigned *)(ring_bytes + params->cq_off.head);
    view->cq_tail = (unsigned *)(ring_bytes + params->cq_off.tail);
    view->cq_mask = (unsigned *)(ring_bytes + params->cq_off.ring_mask);
    view->cqes = (struct io_uring_cqe *)(ring_bytes + params->cq_off.cqes);
    return 0;
}

static struct io_uring_sqe *ring_sqe(struct ring_view *view, unsigned index) {
    unsigned slot = (unsigned)(*view->sq_tail & *view->sq_mask);
    struct io_uring_sqe *sqe = &view->sqes[slot];

    memset(sqe, 0, sizeof(*sqe));
    view->sq_array[slot] = (unsigned)(index & *view->sq_mask);
    __atomic_store_n(view->sq_tail, *view->sq_tail + 1, __ATOMIC_RELEASE);
    return sqe;
}

static unsigned cqe_count(struct ring_view *view) {
    return __atomic_load_n(view->cq_tail, __ATOMIC_ACQUIRE) -
           __atomic_load_n(view->cq_head, __ATOMIC_ACQUIRE);
}

static void cqe_consume(struct ring_view *view) {
    unsigned head = __atomic_load_n(view->cq_head, __ATOMIC_ACQUIRE);

    __atomic_store_n(view->cq_head, head + 1, __ATOMIC_RELEASE);
}

static int test_setup_flags(void) {
    struct io_uring_params params;
    struct io_uring_params clamped;
    struct io_uring_params reserved;
    struct io_uring_params cqsized;
    long ring;

    /* `IORING_SETUP_SUBMIT_ALL` is inside `IORING_SETUP_FLAGS` and is echoed
     * back in `params.flags` unchanged. */
    memset(&params, 0, sizeof(params));
    params.flags = IORING_SETUP_SUBMIT_ALL;
    ring = do_setup(8, &params);
    if (ring < 0)
        return fail("submit-all-setup");
    if (params.flags != IORING_SETUP_SUBMIT_ALL) {
        fprintf(stderr,
                "THEKERNEL_IO_URING_SETUP_BATCH_FAIL submit-all-echoed flags=%u\n",
                params.flags);
        close((int)ring);
        return 1;
    }
    close((int)ring);

    /* IPI-free task-work flags are rejected together with SQPOLL, and the
     * same combination without SQPOLL is a legal ring. */
    memset(&params, 0, sizeof(params));
    params.flags = IORING_SETUP_SQPOLL | IORING_SETUP_COOP_TASKRUN;
    errno = 0;
    if (expect_errno("sqpoll-coop", do_setup(8, &params), EINVAL))
        return 1;
    memset(&params, 0, sizeof(params));
    params.flags = IORING_SETUP_SQPOLL | IORING_SETUP_DEFER_TASKRUN |
                   IORING_SETUP_SINGLE_ISSUER;
    errno = 0;
    if (expect_errno("sqpoll-defer", do_setup(8, &params), EINVAL))
        return 1;
    memset(&params, 0, sizeof(params));
    params.flags = IORING_SETUP_DEFER_TASKRUN | IORING_SETUP_SINGLE_ISSUER;
    ring = do_setup(8, &params);
    if (ring < 0)
        return fail("defer-single-issuer-setup");
    close((int)ring);

    /* `SQ_AFF` without `SQPOLL` has no target thread to bind. */
    memset(&params, 0, sizeof(params));
    params.flags = IORING_SETUP_SQ_AFF;
    errno = 0;
    if (expect_errno("sq-aff-without-sqpoll", do_setup(8, &params), EINVAL))
        return 1;

    /* An unknown bit is -EINVAL, and so is a non-zero reserved word. */
    memset(&params, 0, sizeof(params));
    params.flags = IORING_SETUP_UNKNOWN_BIT;
    errno = 0;
    if (expect_errno("unknown-flag-bit", do_setup(8, &params), EINVAL))
        return 1;
    memset(&reserved, 0, sizeof(reserved));
    reserved.resv[1] = 1;
    errno = 0;
    if (expect_errno("reserved-word", do_setup(8, &reserved), EINVAL))
        return 1;

    /* Geometry: zero entries, an oversized ring without `CLAMP`, the same
     * ring with `CLAMP`, and `CQSIZE` with a count below the SQ ring. */
    memset(&params, 0, sizeof(params));
    errno = 0;
    if (expect_errno("zero-entries", do_setup(0, &params), EINVAL))
        return 1;
    memset(&params, 0, sizeof(params));
    errno = 0;
    if (expect_errno("entries-too-large",
                     do_setup(IORING_MAX_ENTRIES_ORACLE + 1, &params), EINVAL))
        return 1;
    memset(&clamped, 0, sizeof(clamped));
    clamped.flags = IORING_SETUP_CLAMP;
    ring = do_setup(IORING_MAX_ENTRIES_ORACLE + 1, &clamped);
    if (ring < 0)
        return fail("entries-clamped");
    if (clamped.sq_entries != IORING_MAX_ENTRIES_ORACLE ||
        clamped.cq_entries != 2 * IORING_MAX_ENTRIES_ORACLE) {
        fprintf(stderr,
                "THEKERNEL_IO_URING_SETUP_BATCH_FAIL clamped-geometry "
                "sq=%u cq=%u\n",
                clamped.sq_entries, clamped.cq_entries);
        close((int)ring);
        return 1;
    }
    close((int)ring);
    memset(&cqsized, 0, sizeof(cqsized));
    cqsized.flags = IORING_SETUP_CQSIZE;
    cqsized.cq_entries = 4;
    errno = 0;
    if (expect_errno("cq-smaller-than-sq", do_setup(8, &cqsized), EINVAL))
        return 1;
    memset(&cqsized, 0, sizeof(cqsized));
    cqsized.flags = IORING_SETUP_CQSIZE;
    cqsized.cq_entries = 64;
    ring = do_setup(8, &cqsized);
    if (ring < 0)
        return fail("cq-sized-setup");
    if (cqsized.sq_entries != 8 || cqsized.cq_entries != 64) {
        fprintf(stderr,
                "THEKERNEL_IO_URING_SETUP_BATCH_FAIL cq-sized-geometry "
                "sq=%u cq=%u\n",
                cqsized.sq_entries, cqsized.cq_entries);
        close((int)ring);
        return 1;
    }
    close((int)ring);

    puts("THEKERNEL_ABI_ASSERT io-uring-setup-batch.portable-differential "
         "SETUP_FLAGS pass");
    return 0;
}

/*
 * Queues one SQE that cannot be initialized (an opcode outside
 * `IORING_OP_LAST`, which `io_init_req()` answers with -EINVAL) followed by a
 * NOP, submits both, and reports what the ring did with the batch.
 */
static int submit_failing_batch(unsigned flags, const char *stage,
                                unsigned *consumed, unsigned *cqes_seen,
                                long *first_result, long *second_result) {
    struct ring_view view;
    struct io_uring_sqe *sqe;
    long ring;
    long result;
    unsigned seen;

    memset(&view, 0, sizeof(view));
    view.params.flags = flags;
    ring = do_setup(4, &view.params);
    if (ring < 0)
        return fail(stage);
    if ((view.params.features & IORING_FEAT_SINGLE_MMAP) == 0) {
        fprintf(stderr,
                "THEKERNEL_IO_URING_SETUP_BATCH_FAIL %s single-mmap-absent "
                "features=%u\n",
                stage, view.params.features);
        close((int)ring);
        return 1;
    }
    if (ring_map((int)ring, &view)) {
        close((int)ring);
        return 1;
    }

    sqe = ring_sqe(&view, 0);
    sqe->opcode = 200;
    sqe->user_data = 0xa1;
    sqe = ring_sqe(&view, 1);
    sqe->opcode = IORING_OP_NOP;
    sqe->user_data = 0xa2;

    result = do_enter((int)ring, 2);
    if (result < 0) {
        close((int)ring);
        return fail(stage);
    }
    *consumed = (unsigned)result;
    seen = cqe_count(&view);
    *cqes_seen = seen;
    *first_result = 0;
    *second_result = 0;
    if (seen > 0) {
        *first_result = view.cqes[*view.cq_head & *view.cq_mask].res;
        cqe_consume(&view);
    }
    if (seen > 1) {
        *second_result = view.cqes[*view.cq_head & *view.cq_mask].res;
        cqe_consume(&view);
    }
    close((int)ring);
    return 0;
}

static int test_batch_stop(void) {
    unsigned consumed = 0;
    unsigned cqes_seen = 0;
    long first = 0;
    long second = 0;

    /*
     * Without `IORING_SETUP_SUBMIT_ALL` the ring consumes the failing SQE,
     * completes it with -EINVAL and leaves the NOP for the next enter
     * (`io_uring/io_uring.c:2053-2062`).
     */
    if (submit_failing_batch(0, "batch-stop", &consumed, &cqes_seen, &first,
                             &second))
        return 1;
    if (consumed != 1 || cqes_seen != 1 || first != -EINVAL) {
        fprintf(stderr,
                "THEKERNEL_IO_URING_SETUP_BATCH_FAIL batch-stop consumed=%u "
                "cqes=%u first=%ld second=%ld\n",
                consumed, cqes_seen, first, second);
        return 1;
    }
    puts("THEKERNEL_ABI_ASSERT io-uring-setup-batch.portable-differential "
         "BATCH_STOP pass");
    return 0;
}

static int test_submit_all(void) {
    unsigned consumed = 0;
    unsigned cqes_seen = 0;
    long first = 0;
    long second = 0;

    /*
     * With `IORING_SETUP_SUBMIT_ALL` the whole batch is consumed: the failing
     * SQE completes with -EINVAL and the NOP still runs.
     */
    if (submit_failing_batch(IORING_SETUP_SUBMIT_ALL, "submit-all", &consumed,
                             &cqes_seen, &first, &second))
        return 1;
    if (consumed != 2 || cqes_seen != 2 || first != -EINVAL || second != 0) {
        fprintf(stderr,
                "THEKERNEL_IO_URING_SETUP_BATCH_FAIL submit-all consumed=%u "
                "cqes=%u first=%ld second=%ld\n",
                consumed, cqes_seen, first, second);
        return 1;
    }
    puts("THEKERNEL_ABI_ASSERT io-uring-setup-batch.portable-differential "
         "SUBMIT_ALL pass");
    return 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    puts("THEKERNEL_ABI_CASE io-uring-setup-batch.portable-differential");

    if (test_setup_flags() || test_batch_stop() || test_submit_all())
        return 1;

    puts("THEKERNEL_IO_URING_SETUP_BATCH_OK");
    puts("THEKERNEL_ABI_RESULT io-uring-setup-batch.portable-differential pass");
    return 0;
}
