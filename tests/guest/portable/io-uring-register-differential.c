#define _GNU_SOURCE

#include <errno.h>
#include <linux/io_uring.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <unistd.h>

/*
 * Differential coverage for the fixed-file table limits of
 * `io_uring_register(IORING_REGISTER_FILES)`.
 *
 * Two layers judge the request, in this order:
 *
 *   `io_uring_register()`'s dispatcher (`io_uring/register.c:786-790`)
 *
 *       case IORING_REGISTER_FILES:
 *               ret = -EFAULT;
 *               if (!arg)
 *                       break;
 *               ret = io_sqe_files_register(ctx, arg, nr_args, NULL);
 *
 *   `io_sqe_files_register()` (`io_uring/rsrc.c:624-631`)
 *
 *       if (ctx->file_table.data.nr)
 *               return -EBUSY;
 *       if (!nr_args)
 *               return -EINVAL;
 *       if (nr_args > IORING_MAX_FIXED_FILES)      // (1U << 20)
 *               return -EMFILE;
 *       if (nr_args > rlimit(RLIMIT_NOFILE))
 *               return -EMFILE;
 *
 * so a NULL array is -EFAULT, an already-registered table is -EBUSY before
 * either ceiling is consulted, both ceilings are -EMFILE, and every one of
 * those answers is produced before the descriptor array is read.
 */

#define IORING_REGISTER_FILES 2U
#define IORING_UNREGISTER_FILES 3U

#define IORING_MAX_FIXED_FILES (1U << 20)
#define LOWERED_LIMIT 100U
#define SLOTS 200U

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_IO_URING_REGISTER_FAIL %s errno=%d (%s)\n", stage,
            errno, strerror(errno));
    return 1;
}

static int expect_errno(const char *stage, long result, int expected) {
    if (result != -1 || errno != expected) {
        fprintf(stderr,
                "THEKERNEL_IO_URING_REGISTER_FAIL %s result=%ld errno=%d "
                "expected=%d\n",
                stage, result, errno, expected);
        return 1;
    }
    return 0;
}

static long do_io_uring_setup(unsigned entries, struct io_uring_params *params) {
    return syscall(SYS_io_uring_setup, entries, params);
}

static long do_io_uring_register(int ring, unsigned opcode, void *argument,
                                 unsigned count) {
    return syscall(SYS_io_uring_register, ring, opcode, argument, count);
}

static int restore_limit(const struct rlimit *original) {
    if (setrlimit(RLIMIT_NOFILE, original) != 0)
        return fail("files-restore-rlimit");
    return 0;
}

static int test_files_count_limits(int ring) {
    int fd = STDIN_FILENO;
    int sparse[SLOTS];
    struct rlimit original;
    struct rlimit lowered;

    for (size_t index = 0; index < SLOTS; ++index)
        sparse[index] = -1;

    /* A NULL array is the dispatcher's -EFAULT, and the zero count of the
     * next case is the table routine's -EINVAL: two different layers, two
     * different errnos, for two fields of the same header. */
    errno = 0;
    if (expect_errno("files-null-array",
                     do_io_uring_register(ring, IORING_REGISTER_FILES, NULL, 4),
                     EFAULT))
        return 1;
    errno = 0;
    if (expect_errno("files-zero-count",
                     do_io_uring_register(ring, IORING_REGISTER_FILES, &fd, 0),
                     EINVAL))
        return 1;
    /* Both ceilings are -EMFILE, and neither reads the array: the argument
     * only has to be a non-NULL pointer. */
    errno = 0;
    if (expect_errno("files-above-fixed-max",
                     do_io_uring_register(ring, IORING_REGISTER_FILES, &fd,
                                          IORING_MAX_FIXED_FILES + 1),
                     EMFILE))
        return 1;
    errno = 0;
    if (expect_errno("files-count-max-u32",
                     do_io_uring_register(ring, IORING_REGISTER_FILES, &fd,
                                          UINT32_MAX),
                     EMFILE))
        return 1;

    if (getrlimit(RLIMIT_NOFILE, &original) != 0)
        return fail("files-getrlimit");
    lowered = original;
    lowered.rlim_cur = LOWERED_LIMIT;
    if (setrlimit(RLIMIT_NOFILE, &lowered) != 0)
        return fail("files-setrlimit");

    /* `nr_args > rlimit(RLIMIT_NOFILE)`: the soft limit is the ceiling, and a
     * count exactly equal to it is still admitted. */
    errno = 0;
    if (expect_errno("files-above-soft-limit",
                     do_io_uring_register(ring, IORING_REGISTER_FILES, sparse,
                                          LOWERED_LIMIT + 1),
                     EMFILE)) {
        (void)restore_limit(&original);
        return 1;
    }
    if (do_io_uring_register(ring, IORING_REGISTER_FILES, sparse,
                             LOWERED_LIMIT) != 0) {
        (void)restore_limit(&original);
        return fail("files-at-soft-limit");
    }
    /* The table state outranks both ceilings, so an already-registered ring
     * reports -EBUSY even for a count that exceeds the soft limit. */
    errno = 0;
    if (expect_errno("files-busy-outranks-soft-limit",
                     do_io_uring_register(ring, IORING_REGISTER_FILES, sparse,
                                          LOWERED_LIMIT + 1),
                     EBUSY)) {
        (void)restore_limit(&original);
        return 1;
    }
    if (do_io_uring_register(ring, IORING_UNREGISTER_FILES, NULL, 0) != 0) {
        (void)restore_limit(&original);
        return fail("files-unregister");
    }
    /* Released again: the same request now reaches the ceiling check. */
    errno = 0;
    if (expect_errno("files-above-soft-limit-after-release",
                     do_io_uring_register(ring, IORING_REGISTER_FILES, sparse,
                                          LOWERED_LIMIT + 1),
                     EMFILE)) {
        (void)restore_limit(&original);
        return 1;
    }
    if (restore_limit(&original) != 0)
        return 1;
    return 0;
}

int main(void) {
    struct io_uring_params params;
    long ring;

    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    puts("THEKERNEL_ABI_CASE io-uring-register.portable-differential");

    memset(&params, 0, sizeof(params));
    ring = do_io_uring_setup(8, &params);
    if (ring < 0)
        return fail("ring-setup");

    if (test_files_count_limits((int)ring)) {
        close((int)ring);
        return 1;
    }
    puts("THEKERNEL_ABI_ASSERT io-uring-register.portable-differential "
         "FILES_COUNT_LIMITS pass");

    close((int)ring);
    puts("THEKERNEL_IO_URING_REGISTER_OK");
    puts("THEKERNEL_ABI_RESULT io-uring-register.portable-differential pass");
    return 0;
}
