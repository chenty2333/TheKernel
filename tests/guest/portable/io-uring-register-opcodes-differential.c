#define _GNU_SOURCE

#include <errno.h>
#include <linux/io_uring.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/syscall.h>
#include <unistd.h>

/*
 * Differential coverage for the `io_uring_register()` opcode envelope of
 * Linux v7.2.3.
 *
 * `SYSCALL_DEFINE4(io_uring_register)` (`io_uring/register.c:1017-1041`)
 * validates the copy of the header it owns before any body runs:
 *
 *       use_registered_ring = !!(opcode & IORING_REGISTER_USE_REGISTERED_RING);
 *       opcode &= ~IORING_REGISTER_USE_REGISTERED_RING;
 *
 *       if (opcode >= IORING_REGISTER_LAST)          // 38 in v7.2.3
 *               return -EINVAL;
 *
 *       if (fd == -1)
 *               return io_uring_register_blind(opcode, arg, nr_args);
 *
 * so an opcode outside the enum is -EINVAL, four opcodes are dispatched
 * without a ring, and every other opcode supplied with `fd == -1` is -EINVAL.
 * `io_uring_register_blind()` (`io_uring/register.c:998-1014`) handles
 * `SEND_MSG_RING`, `QUERY`, `RESTRICTIONS` and `BPF_FILTER`.
 *
 * Each body then applies its own `arg`/`nr_args` rules, and several of them
 * read a fixed-size request record before judging its fields, which is why a
 * nil pointer is `-EFAULT` and only a readable record can be `-EINVAL`.
 */

#define IORING_REGISTER_FILES 2U
#define IORING_REGISTER_EVENTFD 4U
#define IORING_UNREGISTER_EVENTFD 5U
#define IORING_REGISTER_FILES_UPDATE 6U
#define IORING_REGISTER_RESTRICTIONS 11U
#define IORING_REGISTER_FILES2 13U
#define IORING_REGISTER_RING_FDS 20U
#define IORING_REGISTER_ZCRX_IFQ 32U
#define IORING_REGISTER_QUERY 35U
#define IORING_REGISTER_ZCRX_CTRL 36U
#define IORING_REGISTER_BPF_FILTER 37U
#define IORING_REGISTER_LAST 38U

#define IO_RINGFD_REG_MAX 16U
#define IORING_MAX_RESTRICTIONS 128U

/* `struct io_uring_rsrc_update`: `offset`, `resv`, `data`. */
struct rsrc_update {
    uint32_t offset;
    uint32_t resv;
    uint64_t data;
};

/* `struct io_uring_rsrc_register`: `nr`, `flags`, `resv2`, `data`, `tags`. */
struct rsrc_register {
    uint32_t nr;
    uint32_t flags;
    uint64_t resv2;
    uint64_t data;
    uint64_t tags;
};

/* `struct io_uring_task_restriction` without its inline record array
 * (`include/uapi/linux/io_uring.h:831-836`). */
struct task_restriction {
    uint16_t flags;
    uint16_t nr_res;
    uint32_t resv[3];
};

/* A local 72-byte stand-in for `struct zcrx_ctrl`
 * (`include/uapi/linux/io_uring/zcrx.h:137-146`) and for
 * `struct io_uring_bpf` (`include/uapi/linux/io_uring/bpf_filter.h:75-82`):
 * both are an 8-byte prefix plus a 64-byte body, and the kernel only ever
 * reads a zeroed record here. */
struct aux_record {
    uint8_t bytes[72];
};

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_IO_URING_REGISTER_OPCODES_FAIL %s errno=%d (%s)\n",
            stage, errno, strerror(errno));
    return 1;
}

static int expect_errno(const char *stage, long result, int expected) {
    if (result != -1 || errno != expected) {
        fprintf(stderr,
                "THEKERNEL_IO_URING_REGISTER_OPCODES_FAIL %s result=%ld errno=%d "
                "expected=%d\n",
                stage, result, errno, expected);
        return 1;
    }
    return 0;
}

static int expect_ok(const char *stage, long result) {
    if (result != 0) {
        fprintf(stderr,
                "THEKERNEL_IO_URING_REGISTER_OPCODES_FAIL %s result=%ld errno=%d\n",
                stage, result, errno);
        return 1;
    }
    return 0;
}

static long do_register(int ring, unsigned opcode, void *argument,
                        unsigned count) {
    return syscall(SYS_io_uring_register, ring, opcode, argument, count);
}

static int test_opcode_range(int ring) {
    /* The opcode enum is checked before the blind dispatch, so neither a ring
     * nor a record is consulted. */
    errno = 0;
    if (expect_errno("range-last", do_register(ring, IORING_REGISTER_LAST, NULL, 0), EINVAL) ||
        expect_errno("range-far", do_register(ring, 100, NULL, 0), EINVAL) ||
        expect_errno("range-last-blind", do_register(-1, IORING_REGISTER_LAST, NULL, 0), EINVAL))
        return 1;
    puts("THEKERNEL_ABI_ASSERT io-uring-register-opcodes.portable-differential "
         "OPCODE_RANGE pass");
    return 0;
}

static int test_blind_dispatch(void) {
    struct task_restriction restriction;
    struct task_restriction flagged;

    memset(&restriction, 0, sizeof(restriction));
    flagged = restriction;
    flagged.flags = 1;

    /* A blind opcode is the only one accepted without a ring; every other
     * opcode with `fd == -1` is the dispatcher's -EINVAL. */
    errno = 0;
    if (expect_errno("blind-other-opcode",
                     do_register(-1, IORING_REGISTER_FILES, NULL, 0), EINVAL) ||
        expect_errno("blind-other-opcode-record",
                     do_register(-1, IORING_REGISTER_FILES2, NULL, 32), EINVAL))
        return 1;
    /* `io_query()` (`io_uring/query.c:125-131`): a zero count with a NULL
     * chain head is a successful no-op, and a non-zero count is -EINVAL. */
    if (expect_ok("blind-query-empty", do_register(-1, IORING_REGISTER_QUERY, NULL, 0)))
        return 1;
    errno = 0;
    if (expect_errno("blind-query-count",
                     do_register(-1, IORING_REGISTER_QUERY, NULL, 1), EINVAL))
        return 1;
    /* `io_register_restrictions_task()` (`io_uring/register.c:218-235`) reads
     * a `struct io_uring_task_restriction` whose count is exactly one: a nil
     * record is then -EFAULT and a flagged record is -EINVAL. */
    errno = 0;
    if (expect_errno("blind-restriction-count",
                     do_register(-1, IORING_REGISTER_RESTRICTIONS, NULL, 0), EINVAL) ||
        expect_errno("blind-restriction-null-record",
                     do_register(-1, IORING_REGISTER_RESTRICTIONS, NULL, 1), EFAULT) ||
        expect_errno("blind-restriction-flags",
                     do_register(-1, IORING_REGISTER_RESTRICTIONS, &flagged, 1), EINVAL))
        return 1;
    puts("THEKERNEL_ABI_ASSERT io-uring-register-opcodes.portable-differential "
         "BLIND_DISPATCH pass");
    return 0;
}

static int test_auxiliary_records(int ring) {
    struct aux_record control;
    struct aux_record reserved;
    struct aux_record filter;
    uint64_t ifq[8];

    memset(&control, 0, sizeof(control));
    reserved = control;
    reserved.bytes[8] = 1;
    memset(&filter, 0, sizeof(filter));
    memset(ifq, 0, sizeof(ifq));

    /* `io_zcrx_ctrl()` (`io_uring/zcrx.c:1426-1462`): a non-zero `nr_args` is
     * -EINVAL, the 72-byte record is read (so a nil pointer is -EFAULT), a
     * non-zero reserved word is -EFAULT, and the `zcrx_id` lookup of a ring
     * that cannot have a zcrx context is -ENXIO. */
    errno = 0;
    if (expect_errno("zcrx-ctrl-count",
                     do_register(ring, IORING_REGISTER_ZCRX_CTRL, &control, 1), EINVAL) ||
        expect_errno("zcrx-ctrl-null-record",
                     do_register(ring, IORING_REGISTER_ZCRX_CTRL, NULL, 0), EFAULT) ||
        expect_errno("zcrx-ctrl-reserved",
                     do_register(ring, IORING_REGISTER_ZCRX_CTRL, &reserved, 0), EFAULT) ||
        expect_errno("zcrx-ctrl-absent-id",
                     do_register(ring, IORING_REGISTER_ZCRX_CTRL, &control, 0), ENXIO))
        return 1;
    /* `IORING_REGISTER_ZCRX_IFQ` keeps the dispatcher's own header shape
     * (`io_uring/register.c:938-942`). */
    errno = 0;
    if (expect_errno("zcrx-ifq-null",
                     do_register(ring, IORING_REGISTER_ZCRX_IFQ, NULL, 1), EINVAL) ||
        expect_errno("zcrx-ifq-count",
                     do_register(ring, IORING_REGISTER_ZCRX_IFQ, ifq, 0), EINVAL))
        return 1;
    /* `io_bpf_filter_import()` (`io_uring/bpf_filter.c:319-335`) reads its
     * 72-byte record after the dispatcher's count check and rejects every
     * field of a zeroed record with -EINVAL. */
    errno = 0;
    if (expect_errno("bpf-filter-count",
                     do_register(ring, IORING_REGISTER_BPF_FILTER, &filter, 0), EINVAL) ||
        expect_errno("bpf-filter-null-record",
                     do_register(ring, IORING_REGISTER_BPF_FILTER, NULL, 1), EFAULT) ||
        expect_errno("bpf-filter-cmd-type",
                     do_register(ring, IORING_REGISTER_BPF_FILTER, &filter, 1), EINVAL))
        return 1;
    puts("THEKERNEL_ABI_ASSERT io-uring-register-opcodes.portable-differential "
         "AUXILIARY_RECORDS pass");
    return 0;
}

static int test_eventfd_descriptor(int ring) {
    int event = eventfd(0, EFD_CLOEXEC);
    int absent = 987654;

    if (event < 0)
        return fail("eventfd-create");
    /* `io_eventfd_register()` (`io_uring/eventfd.c:121-134`) answers -EBUSY
     * for an already-published eventfd *before* it reads the caller's
     * descriptor, which it takes through `copy_from_user()` as an `__s32`
     * pointer, and `eventfd_ctx_fdget()` then owns -EBADF. */
    errno = 0;
    if (expect_errno("eventfd-count",
                     do_register(ring, IORING_REGISTER_EVENTFD, &event, 0), EINVAL) ||
        expect_errno("eventfd-null-record",
                     do_register(ring, IORING_REGISTER_EVENTFD, NULL, 1), EFAULT) ||
        expect_errno("eventfd-bad-descriptor",
                     do_register(ring, IORING_REGISTER_EVENTFD, &absent, 1), EBADF)) {
        close(event);
        return 1;
    }
    if (expect_ok("eventfd-register", do_register(ring, IORING_REGISTER_EVENTFD, &event, 1))) {
        close(event);
        return 1;
    }
    /* Published state outranks both the fault and the descriptor lookup. */
    errno = 0;
    if (expect_errno("eventfd-busy-before-fault",
                     do_register(ring, IORING_REGISTER_EVENTFD, NULL, 1), EBUSY) ||
        expect_errno("eventfd-busy-before-descriptor",
                     do_register(ring, IORING_REGISTER_EVENTFD, &absent, 1), EBUSY)) {
        close(event);
        return 1;
    }
    /* `io_eventfd_unregister()` (`io_uring/eventfd.c:159-172`) reports -ENXIO
     * when nothing is published. */
    if (expect_ok("eventfd-unregister",
                  do_register(ring, IORING_UNREGISTER_EVENTFD, NULL, 0))) {
        close(event);
        return 1;
    }
    errno = 0;
    if (expect_errno("eventfd-unregister-absent",
                     do_register(ring, IORING_UNREGISTER_EVENTFD, NULL, 0), ENXIO)) {
        close(event);
        return 1;
    }
    close(event);
    puts("THEKERNEL_ABI_ASSERT io-uring-register-opcodes.portable-differential "
         "EVENTFD_DESCRIPTOR pass");
    return 0;
}

static int test_resource_records(int ring) {
    struct rsrc_update update;
    struct rsrc_register register_record;

    memset(&update, 0, sizeof(update));
    memset(&register_record, 0, sizeof(register_record));

    /* `io_register_files_update()` (`io_uring/rsrc.c:439-453`): a zero count
     * is -EINVAL before the 16-byte record is read, and an unreadable record
     * is -EFAULT. */
    errno = 0;
    if (expect_errno("files-update-zero-count",
                     do_register(ring, IORING_REGISTER_FILES_UPDATE, NULL, 0), EINVAL) ||
        expect_errno("files-update-zero-count-record",
                     do_register(ring, IORING_REGISTER_FILES_UPDATE, &update, 0), EINVAL) ||
        expect_errno("files-update-null-record",
                     do_register(ring, IORING_REGISTER_FILES_UPDATE, NULL, 1), EFAULT))
        return 1;
    /* `io_register_rsrc()`/`io_register_rsrc_update()` pass `nr_args` as the
     * record size (`io_uring/rsrc.c:395-418`, `:455-464`), so any other size
     * is -EINVAL and the exact size reaches the copy and its field checks. */
    errno = 0;
    if (expect_errno("files2-size",
                     do_register(ring, IORING_REGISTER_FILES2, &register_record, 16), EINVAL) ||
        expect_errno("files2-null-record",
                     do_register(ring, IORING_REGISTER_FILES2, NULL, 32), EFAULT) ||
        expect_errno("files2-empty-record",
                     do_register(ring, IORING_REGISTER_FILES2, &register_record, 32), EINVAL))
        return 1;
    /* `io_ringfd_register()` (`io_uring/tctx.c:325-340`): the count is bounded
     * by `IO_RINGFD_REG_MAX` and the update array is read one 16-byte record
     * at a time, so a nil array is -EFAULT rather than -EINVAL. */
    errno = 0;
    if (expect_errno("ring-fds-zero-count",
                     do_register(ring, IORING_REGISTER_RING_FDS, &update, 0), EINVAL) ||
        expect_errno("ring-fds-above-max",
                     do_register(ring, IORING_REGISTER_RING_FDS, &update,
                                 IO_RINGFD_REG_MAX + 1), EINVAL) ||
        expect_errno("ring-fds-null-array",
                     do_register(ring, IORING_REGISTER_RING_FDS, NULL, 1), EFAULT))
        return 1;
    puts("THEKERNEL_ABI_ASSERT io-uring-register-opcodes.portable-differential "
         "RESOURCE_RECORDS pass");
    return 0;
}

int main(void) {
    struct io_uring_params params;
    long ring;

    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    puts("THEKERNEL_ABI_CASE io-uring-register-opcodes.portable-differential");

    memset(&params, 0, sizeof(params));
    ring = syscall(SYS_io_uring_setup, 8, &params);
    if (ring < 0)
        return fail("ring-setup");

    if (test_opcode_range((int)ring) || test_blind_dispatch() ||
        test_auxiliary_records((int)ring) || test_eventfd_descriptor((int)ring) ||
        test_resource_records((int)ring)) {
        close((int)ring);
        return 1;
    }

    close((int)ring);
    puts("THEKERNEL_IO_URING_REGISTER_OPCODES_OK");
    puts("THEKERNEL_ABI_RESULT io-uring-register-opcodes.portable-differential pass");
    return 0;
}
