#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <linux/aio_abi.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

/*
 * Differential coverage for the classic AIO entry points.
 *
 * Every assertion below is a rule that the Linux v7.2.3 sources state
 * unconditionally, so it holds on both TheKernel and the reference kernel:
 *
 *   io_setup(2)   fs/aio.c
 *                 - the context out-parameter is read before anything else
 *                   (-EFAULT for an unreadable pointer)
 *                 - a non-zero `*ctxp` or a zero `nr_events` is -EINVAL
 *   io_submit(2)  fs/aio.c
 *                 - a negative `nr`, a missing context and an unreadable iocb
 *                   are -EINVAL/-EINVAL/-EFAULT
 *                 - `nr == 0` succeeds without touching the iocb array
 *                 - `__io_submit_one()` opens the descriptor before it
 *                   dispatches on `aio_lio_opcode`, so a closed descriptor is
 *                   -EBADF and opcode 6 (`IOCB_CMD_NOOP`, which has no case in
 *                   that switch) is -EINVAL
 *   io_cancel(2)  fs/aio.c
 *                 - `iocb->aio_key` is read first (-EFAULT), a value other
 *                   than KIOCB_KEY is -EINVAL, and an unknown or already
 *                   completed request is -EINVAL
 *                 - a request that really is pending answers -EINPROGRESS and
 *                   delivers one completion event for its own iocb
 *   io_destroy(2) fs/aio.c
 *                 - a zero, unknown or already destroyed context is -EINVAL
 */

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_AIO_FAIL %s errno=%d (%s)\n", stage, errno,
            strerror(errno));
    return 1;
}

static int fail_value(const char *stage, long actual, long expected) {
    fprintf(stderr, "THEKERNEL_AIO_FAIL %s actual=%ld expected=%ld\n", stage,
            actual, expected);
    return 1;
}

static int expect_errno(const char *stage, long result, int expected) {
    if (result != -1 || errno != expected) {
        fprintf(stderr,
                "THEKERNEL_AIO_FAIL %s result=%ld errno=%d expected=%d\n",
                stage, result, errno, expected);
        return 1;
    }
    return 0;
}

static long do_io_setup(unsigned nr_events, aio_context_t *ctxp) {
    return syscall(SYS_io_setup, nr_events, ctxp);
}

static long do_io_submit(aio_context_t ctx, long nr, struct iocb **iocbs) {
    return syscall(SYS_io_submit, ctx, nr, iocbs);
}

static long do_io_cancel(aio_context_t ctx, struct iocb *iocb,
                         struct io_event *event) {
    return syscall(SYS_io_cancel, ctx, iocb, event);
}

static long do_io_getevents(aio_context_t ctx, long min, long max,
                            struct io_event *events, struct timespec *timeout) {
    return syscall(SYS_io_getevents, ctx, min, max, events, timeout);
}

static long do_io_destroy(aio_context_t ctx) {
    return syscall(SYS_io_destroy, ctx);
}

/* `struct __aio_sigset` verbatim (fs/aio.c:2324-2327). */
struct aio_sigset_abi {
    const void *sigmask;
    size_t sigsetsize;
};

static long do_io_pgetevents(aio_context_t ctx, long min, long max,
                             struct io_event *events, struct timespec *timeout,
                             const struct aio_sigset_abi *usig) {
    return syscall(SYS_io_pgetevents, ctx, min, max, events, timeout, usig);
}

/* A context id that no `io_setup` in this process can have produced. */
#define MISSING_CONTEXT 0x1234UL
/* A pointer no kernel may dereference successfully. */
#define BAD_PTR ((void *)(uintptr_t)1)
/* x86_64 kernel `sigset_t` selects a signal with bit (signo-1). */
#define SIGBIT(signo) (1ULL << ((signo) - 1))

static void sigusr2_noop(int signo) { (void)signo; }

static struct iocb *make_iocb(struct iocb *iocb, uint16_t opcode, int fd) {
    memset(iocb, 0, sizeof(*iocb));
    iocb->aio_lio_opcode = opcode;
    iocb->aio_fildes = (uint32_t)fd;
    return iocb;
}

static int test_io_setup_validation(aio_context_t *out) {
    aio_context_t ctx = 0;

    /* `io_setup()` reads `*ctxp` first (`fs/aio.c:1441-1442`), then rejects a
     * non-zero value or a zero `nr_events` with -EINVAL. */
    errno = 0;
    if (expect_errno("io-setup-zero-events", do_io_setup(0, &ctx), EINVAL))
        return 1;
    ctx = MISSING_CONTEXT;
    errno = 0;
    if (expect_errno("io-setup-nonzero-context", do_io_setup(8, &ctx), EINVAL))
        return 1;
    errno = 0;
    if (expect_errno("io-setup-null-context", do_io_setup(8, NULL), EFAULT))
        return 1;

    ctx = 0;
    if (do_io_setup(8, &ctx) != 0)
        return fail("io-setup");
    if (ctx == 0)
        return fail_value("io-setup-context-id", (long)ctx, 1);
    *out = ctx;
    return 0;
}

static int test_io_submit_validation(aio_context_t ctx, int valid_fd,
                                     int closed_fd) {
    struct iocb iocb;
    struct iocb *list[1];

    errno = 0;
    if (expect_errno("io-submit-negative-nr", do_io_submit(ctx, -1, list),
                     EINVAL))
        return 1;
    /* `nr == 0` succeeds without reading the array at all. */
    if (do_io_submit(ctx, 0, NULL) != 0)
        return fail("io-submit-zero-nr");
    errno = 0;
    if (expect_errno("io-submit-missing-context",
                     do_io_submit(MISSING_CONTEXT, 1, list), EINVAL))
        return 1;
    list[0] = NULL;
    errno = 0;
    if (expect_errno("io-submit-null-iocb", do_io_submit(ctx, 1, list),
                     EFAULT))
        return 1;

    /* `__io_submit_one()` has no case for `IOCB_CMD_NOOP`, so opcode 6 falls
     * to `default: return -EINVAL;` (`fs/aio.c:2055-2072`). */
    list[0] = make_iocb(&iocb, IOCB_CMD_NOOP, valid_fd);
    errno = 0;
    if (expect_errno("io-submit-noop-opcode", do_io_submit(ctx, 1, list),
                     EINVAL))
        return 1;
    /* 4 is the retired experimental `IOCB_CMD_PREADX`: also undispatched. */
    list[0] = make_iocb(&iocb, 4, valid_fd);
    errno = 0;
    if (expect_errno("io-submit-unassigned-opcode", do_io_submit(ctx, 1, list),
                     EINVAL))
        return 1;
    /* The descriptor is opened before the opcode is examined:
     *     req->ki_filp = fget(iocb->aio_fildes);
     *     if (unlikely(!req->ki_filp)) return -EBADF;
     *     switch (iocb->aio_lio_opcode) { ... default: return -EINVAL; }
     * (`fs/aio.c:2026-2029` and `:2055-2072`), so a closed descriptor outranks the invalid
     * opcode. */
    list[0] = make_iocb(&iocb, IOCB_CMD_NOOP, closed_fd);
    errno = 0;
    if (expect_errno("io-submit-noop-closed-fd", do_io_submit(ctx, 1, list),
                     EBADF))
        return 1;
    return 0;
}

static int test_io_cancel_validation(aio_context_t ctx, int valid_fd) {
    struct iocb iocb;
    struct io_event event;

    /* `io_cancel()` reads `iocb->aio_key` before anything else
     * (`fs/aio.c:2238-2241`). */
    errno = 0;
    if (expect_errno("io-cancel-null-iocb", do_io_cancel(ctx, NULL, &event),
                     EFAULT))
        return 1;
    make_iocb(&iocb, IOCB_CMD_NOOP, valid_fd);
    iocb.aio_key = 1;
    errno = 0;
    if (expect_errno("io-cancel-nonzero-key", do_io_cancel(ctx, &iocb, &event),
                     EINVAL))
        return 1;
    /* KIOCB_KEY but no such active request: -EINVAL, never -EAGAIN. */
    make_iocb(&iocb, IOCB_CMD_NOOP, valid_fd);
    errno = 0;
    if (expect_errno("io-cancel-unknown-request",
                     do_io_cancel(ctx, &iocb, &event), EINVAL))
        return 1;
    errno = 0;
    if (expect_errno("io-cancel-missing-context",
                     do_io_cancel(MISSING_CONTEXT, &iocb, &event), EINVAL))
        return 1;
    return 0;
}

static int test_io_cancel_pending_request(aio_context_t ctx, int read_fd) {
    struct iocb iocb;
    struct iocb *list[1];
    struct io_event events[2];
    struct timespec timeout = {5, 0};
    long result;

    /* A poll request on an empty pipe stays pending, so it is on the active
     * list that `io_cancel()` walks. */
    list[0] = make_iocb(&iocb, IOCB_CMD_POLL, read_fd);
    iocb.aio_buf = POLLIN;
    result = do_io_submit(ctx, 1, list);
    if (result != 1)
        return fail_value("io-cancel-pending-submit", result, 1);

    /* `ki_cancel()` returning 0 is reported as -EINPROGRESS, and the event
     * arrives through the completion ring; the deprecated `result` argument
     * stays untouched (`fs/aio.c:2256-2263`). */
    errno = 0;
    result = do_io_cancel(ctx, &iocb, &events[0]);
    if (result != -1 || errno != EINPROGRESS) {
        fprintf(stderr,
                "THEKERNEL_AIO_FAIL io-cancel-pending result=%ld errno=%d\n",
                result, errno);
        return 1;
    }
    result = do_io_getevents(ctx, 1, 2, events, &timeout);
    if (result != 1)
        return fail_value("io-cancel-pending-events", result, 1);
    if (events[0].obj != (uint64_t)(uintptr_t)&iocb)
        return fail_value("io-cancel-pending-event-obj",
                          (long)events[0].obj, (long)(uintptr_t)&iocb);
    /* A cancelled *poll* request does not publish -ECANCELED.  `aio_poll_cancel()`
     * sets `req->cancelled` and re-schedules `aio_poll_complete_work()`
     * (`fs/aio.c:1823-1837`); that work skips `vfs_poll()` for a cancelled
     * request and publishes `iocb->ki_res.res = mangle_poll(mask)` with `mask`
     * still at its initialiser (`fs/aio.c:1777-1817`).  `__MAP(0, from, to)` is
     * zero for every bit, so `mangle_poll(0) == 0`
     * (`include/linux/poll.h:120-127`).  Only the non-poll completion paths
     * report -ECANCELED, which is why this value is asserted here. */
    if (events[0].res != 0)
        return fail_value("io-cancel-pending-event-res", (long)events[0].res, 0);
    return 0;
}

static int test_io_destroy_validation(aio_context_t live) {
    errno = 0;
    if (expect_errno("io-destroy-zero", do_io_destroy(0), EINVAL))
        return 1;
    errno = 0;
    if (expect_errno("io-destroy-missing", do_io_destroy(MISSING_CONTEXT),
                     EINVAL))
        return 1;
    if (do_io_destroy(live) != 0)
        return fail("io-destroy-live");
    errno = 0;
    if (expect_errno("io-destroy-twice", do_io_destroy(live), EINVAL))
        return 1;
    return 0;
}

/*
 * io_pgetevents(2) is the AIO wait that carries a sixth
 * `const struct __aio_sigset __user *` argument.  `SYSCALL_DEFINE6`
 * (fs/aio.c:2329-2360) orders its work as:
 *
 *   1. `get_timespec64(&ts, timeout)`                -> -EFAULT
 *   2. `copy_from_user(&ksig, usig, sizeof(ksig))`   -> -EFAULT
 *   3. `set_user_sigmask(ksig.sigmask, ksig.sigsetsize)`
 *      (kernel/signal.c:3282-3298): a NULL mask returns 0 *before* the size
 *      check, a wrong size is -EINVAL, an unreadable mask is -EFAULT; the
 *      caller's blocked set is saved and the supplied set installed.
 *   4. `do_io_getevents()`: `lookup_ioctx()` then the
 *      `min_nr <= nr && min_nr >= 0` range check (fs/aio.c:2268-2281).
 *   5. `restore_saved_sigmask_unless(interrupted)` puts the caller's set back
 *      (fs/aio.c:2353-2355).
 */
static int test_io_pgetevents_validation(int pipe_fds[2]) {
    aio_context_t ctx = 0;
    struct aio_sigset_abi usig;
    struct timespec zero = {0, 0};
    struct iocb iocb;
    struct iocb *list[1];
    struct io_event event;
    struct timespec timeout = {5, 0};
    /* The caller's blocked set, and the different set that
     * `set_user_sigmask()` installs for the duration of the call. */
    uint64_t temporary = SIGBIT(SIGUSR2);
    uint64_t installed = SIGBIT(SIGUSR1) | SIGBIT(SIGUSR2);
    uint64_t empty_mask = 0;
    uint64_t after = 0;
    long result;

    if (do_io_setup(4, &ctx) != 0)
        return fail("io-pgetevents-setup");

    /* Step 1 precedes step 3: an unreadable timeout is -EFAULT even though
     * the sigset descriptor carries a size `set_user_sigmask()` rejects. */
    usig.sigmask = &usig;
    usig.sigsetsize = 4;
    errno = 0;
    if (expect_errno("io-pgetevents-timeout-first",
                     do_io_pgetevents(MISSING_CONTEXT, 0, 0, NULL, BAD_PTR,
                                      &usig),
                     EFAULT))
        return 1;
    errno = 0;
    if (expect_errno("io-pgetevents-sigset-size",
                     do_io_pgetevents(MISSING_CONTEXT, 0, 0, NULL, &zero,
                                      &usig),
                     EINVAL))
        return 1;
    puts("THEKERNEL_ABI_ASSERT io_pgetevents.raw-differential "
         "TIMEOUT_COPY_BEFORE_SIGSET pass");

    /* Step 2 then step 3: the 16-byte descriptor copy, then the size check,
     * then the mask copy it points at. */
    errno = 0;
    if (expect_errno("io-pgetevents-descriptor-copy",
                     do_io_pgetevents(MISSING_CONTEXT, 0, 0, NULL, NULL,
                                      (const struct aio_sigset_abi *)BAD_PTR),
                     EFAULT))
        return 1;
    usig.sigmask = BAD_PTR;
    usig.sigsetsize = 4;
    errno = 0;
    if (expect_errno("io-pgetevents-size-before-mask",
                     do_io_pgetevents(MISSING_CONTEXT, 0, 0, NULL, NULL,
                                      &usig),
                     EINVAL))
        return 1;
    usig.sigsetsize = 8;
    errno = 0;
    if (expect_errno("io-pgetevents-mask-copy",
                     do_io_pgetevents(MISSING_CONTEXT, 0, 0, NULL, NULL,
                                      &usig),
                     EFAULT))
        return 1;
    puts("THEKERNEL_ABI_ASSERT io_pgetevents.raw-differential "
         "SIGSET_COPY_ORDER pass");

    /* A NULL mask short-circuits the size check (kernel/signal.c:3286-3289),
     * so any sigsetsize is accepted and no mask is installed. */
    usig.sigmask = NULL;
    usig.sigsetsize = 0;
    result = do_io_pgetevents(ctx, 0, 0, NULL, NULL, &usig);
    if (result != 0)
        return fail_value("io-pgetevents-null-mask-zero-size", result, 0);
    usig.sigsetsize = 4;
    result = do_io_pgetevents(ctx, 0, 0, NULL, NULL, &usig);
    if (result != 0)
        return fail_value("io-pgetevents-null-mask-bad-size", result, 0);
    usig.sigsetsize = 8;
    result = do_io_pgetevents(ctx, 0, 0, BAD_PTR, NULL, &usig);
    if (result != 0)
        return fail_value("io-pgetevents-null-mask-no-event", result, 0);
    puts("THEKERNEL_ABI_ASSERT io_pgetevents.raw-differential "
         "NULL_SIGMASK_ANY_SIZE pass");

    /* Step 4: the range check and the context lookup. */
    errno = 0;
    if (expect_errno("io-pgetevents-negative-min",
                     do_io_pgetevents(ctx, -1, 0, NULL, NULL, NULL), EINVAL))
        return 1;
    errno = 0;
    if (expect_errno("io-pgetevents-negative-nr",
                     do_io_pgetevents(ctx, 0, -1, NULL, NULL, NULL), EINVAL))
        return 1;
    errno = 0;
    if (expect_errno("io-pgetevents-min-above-nr",
                     do_io_pgetevents(ctx, 2, 1, NULL, NULL, NULL), EINVAL))
        return 1;
    errno = 0;
    if (expect_errno("io-pgetevents-missing-context",
                     do_io_pgetevents(MISSING_CONTEXT, 0, 1, NULL, NULL, NULL),
                     EINVAL))
        return 1;
    puts("THEKERNEL_ABI_ASSERT io_pgetevents.raw-differential "
         "NR_AND_CONTEXT_EINVAL pass");

    /* Step 5: `set_user_sigmask()` installs the supplied mask for the duration
     * of the call and `restore_saved_sigmask_unless()` puts the caller's set
     * back (`fs/aio.c:2337-2352`, `kernel/signal.c:3282-3300`).  The
     * temporary set is deliberately different from the caller's set, so a
     * missing restore is visible.  A signal that is both pending and blocked
     * does not set `TIF_SIGPENDING` in a single-threaded process
     * (`kernel/signal.c` `complete_signal()`), so it must not change the
     * restore either. */
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = sigusr2_noop;
    if (sigaction(SIGUSR2, &action, NULL) != 0)
        return fail("io-pgetevents-sigaction");
    if (syscall(SYS_rt_sigprocmask, SIG_SETMASK, &temporary, NULL, 8) != 0)
        return fail("io-pgetevents-setmask");
    if (syscall(SYS_tgkill, getpid(), (pid_t)syscall(SYS_gettid), SIGUSR2) !=
        0)
        return fail("io-pgetevents-tgkill");
    usig.sigmask = &installed;
    usig.sigsetsize = 8;
    result = do_io_pgetevents(ctx, 0, 0, NULL, NULL, &usig);
    if (result != 0)
        return fail_value("io-pgetevents-masked-wait", result, 0);
    if (syscall(SYS_rt_sigprocmask, SIG_BLOCK, NULL, &after, 8) != 0)
        return fail("io-pgetevents-read-mask");
    if (after != temporary) {
        fprintf(stderr,
                "THEKERNEL_AIO_FAIL io-pgetevents-mask-restored after=%llx "
                "before=%llx\n",
                (unsigned long long)after, (unsigned long long)temporary);
        return 1;
    }
    if (syscall(SYS_rt_sigprocmask, SIG_SETMASK, &empty_mask, NULL, 8) != 0)
        return fail("io-pgetevents-unblock");
    puts("THEKERNEL_ABI_ASSERT io_pgetevents.raw-differential MASK_RESTORED "
         "pass");

    /* A wait that really completes still delivers through this entry point
     * with a sigset installed. */
    if (write(pipe_fds[1], "x", 1) != 1)
        return fail("io-pgetevents-pipe-write");
    list[0] = make_iocb(&iocb, IOCB_CMD_POLL, pipe_fds[0]);
    iocb.aio_buf = POLLIN;
    if (do_io_submit(ctx, 1, list) != 1)
        return fail("io-pgetevents-submit");
    usig.sigmask = &installed;
    usig.sigsetsize = 8;
    result = do_io_pgetevents(ctx, 1, 1, &event, &timeout, &usig);
    if (result != 1)
        return fail_value("io-pgetevents-delivery", result, 1);
    if (event.obj != (uint64_t)(uintptr_t)&iocb)
        return fail_value("io-pgetevents-event-obj", (long)event.obj,
                          (long)(uintptr_t)&iocb);
    puts("THEKERNEL_ABI_ASSERT io_pgetevents.raw-differential "
         "EVENT_DELIVERED_WITH_SIGSET pass");

    if (do_io_destroy(ctx) != 0)
        return fail("io-pgetevents-destroy");
    return 0;
}

int main(void) {
    aio_context_t ready = 0;
    aio_context_t live = 0;
    int pipe_fds[2];
    int dead_fds[2];
    int closed_fd;

    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    puts("THEKERNEL_ABI_CASE aio.portable-differential");

    if (pipe(pipe_fds) != 0)
        return fail("aio-pipe");
    /* Both ends of a second pipe are closed and never reopened: no descriptor
     * is allocated between here and the -EBADF assertion. */
    if (pipe(dead_fds) != 0)
        return fail("aio-dead-pipe");
    closed_fd = dead_fds[1];
    close(dead_fds[0]);
    close(dead_fds[1]);

    if (test_io_setup_validation(&ready))
        return 1;
    puts("THEKERNEL_ABI_ASSERT aio.portable-differential IO_SETUP_VALIDATION "
         "pass");

    if (test_io_submit_validation(ready, pipe_fds[1], closed_fd))
        return 1;
    puts("THEKERNEL_ABI_ASSERT aio.portable-differential "
         "IO_SUBMIT_OPCODE_VALIDATION pass");

    if (test_io_cancel_validation(ready, pipe_fds[0]))
        return 1;
    puts("THEKERNEL_ABI_ASSERT aio.portable-differential "
         "IO_CANCEL_VALIDATION pass");

    if (test_io_cancel_pending_request(ready, pipe_fds[0]))
        return 1;
    puts("THEKERNEL_ABI_ASSERT aio.portable-differential "
         "IO_CANCEL_PENDING_REQUEST pass");

    if (test_io_destroy_validation(ready))
        return 1;
    puts("THEKERNEL_ABI_ASSERT aio.portable-differential IO_DESTROY_VALIDATION "
         "pass");

    /* A second context proves that destroying one does not disturb the
     * manager's id allocation. */
    if (do_io_setup(4, &live) != 0)
        return fail("aio-second-setup");
    if (do_io_destroy(live) != 0)
        return fail("aio-second-destroy");
    puts("THEKERNEL_ABI_RESULT aio.portable-differential pass");

    /* Cases are reported strictly one after another: the ABI parser rejects a
     * case banner that appears while another case is still open. */
    puts("THEKERNEL_ABI_CASE io_pgetevents.raw-differential");
    if (test_io_pgetevents_validation(pipe_fds))
        return 1;
    puts("THEKERNEL_ABI_RESULT io_pgetevents.raw-differential pass");

    puts("THEKERNEL_AIO_OK");
    return 0;
}
