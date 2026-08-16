#define _GNU_SOURCE

#if !defined(__x86_64__)
#error "rseq smoke test requires the x86_64 Linux ABI"
#endif

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/auxv.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

/* Keep this test independent of libc's optional rseq wrapper. */
#ifndef __NR_rseq
#define __NR_rseq 334
#endif

#define RSEQ_AREA_SIZE 32U
#define RSEQ_AREA_ALIGN 32U
#define RSEQ_FLAG_UNREGISTER 1U
#define RSEQ_SIG UINT32_C(0x53053053)
#define RSEQ_CPU_ID_UNINITIALIZED UINT32_MAX
#define RSEQ_CPU_ID_REGISTRATION_FAILED (UINT32_MAX - 1U)
#define RSEQ_POISON UINT32_C(0xa5a5a5a5)
#define RSEQ_CHILD_COW_MARK UINT32_C(0xc0c0c0c0)

/* These are the Linux auxv numbers, kept local so this test does not depend
 * on a libc rseq header being installed in the guest toolchain. */
#ifndef AT_RSEQ_FEATURE_SIZE
#define AT_RSEQ_FEATURE_SIZE 27U
#endif
#ifndef AT_RSEQ_ALIGN
#define AT_RSEQ_ALIGN 28U
#endif

struct rseq_area {
    uint32_t cpu_id_start;
    uint32_t cpu_id;
    uint64_t rseq_cs;
    uint32_t flags;
    uint32_t node_id;
    uint32_t mm_cid;
} __attribute__((aligned(RSEQ_AREA_ALIGN)));

struct rseq_critical_section {
    uint32_t version;
    uint32_t flags;
    uint64_t start_ip;
    uint64_t post_commit_offset;
    uint64_t abort_ip;
} __attribute__((aligned(RSEQ_AREA_ALIGN)));

struct signal_control {
    volatile sig_atomic_t ready;
    volatile sig_atomic_t stop;
};

_Static_assert(sizeof(struct rseq_area) == RSEQ_AREA_SIZE,
               "rseq area must retain the Linux v6.6 size");
_Static_assert(_Alignof(struct rseq_area) == RSEQ_AREA_ALIGN,
               "rseq area must retain the Linux v6.6 alignment");
_Static_assert(sizeof(struct rseq_critical_section) == RSEQ_AREA_SIZE,
               "rseq critical section must retain the Linux v6.6 size");
_Static_assert(_Alignof(struct rseq_critical_section) == RSEQ_AREA_ALIGN,
               "rseq critical section must retain the Linux v6.6 alignment");

static volatile sig_atomic_t signal_seen;
static volatile sig_atomic_t *signal_stop;
volatile sig_atomic_t thekernel_rseq_abort_seen;

/* A small x86_64-only critical section.  The rseq_cs store is immediately
 * before start_ip, matching Linux's required entry layout: if an event lands
 * after that store, the saved RIP is already inside the descriptor interval;
 * if it lands before the store, rseq_cs is still inactive.  The ready store is
 * itself inside the interval, so the parent can send SIGUSR1 only after the
 * child is executing there without requiring another syscall in the child.
 * An abort caused by ordinary scheduling retries the window; only an abort
 * whose signal handler has run completes the test.  The four-byte word
 * immediately before the abort target is RSEQ_SIG, as required by the
 * kernel's abort validation. */
__asm__(
    ".text\n"
    ".p2align 5\n"
    ".globl thekernel_rseq_window\n"
    ".type thekernel_rseq_window,@function\n"
    "thekernel_rseq_window:\n"
    "    movq %rsi,8(%rdi)\n"
    "thekernel_rseq_start:\n"
    "    movl $1,(%rcx)\n"
    "1:\n"
    "    cmpb $0,(%rdx)\n"
    "    je 1b\n"
    "thekernel_rseq_post:\n"
    "    ret\n"
    ".size thekernel_rseq_window,thekernel_rseq_post-thekernel_rseq_window\n"
    ".p2align 5\n"
    ".globl thekernel_rseq_abort\n"
    ".type thekernel_rseq_abort,@function\n"
    "thekernel_rseq_signature:\n"
    "    .long 0x53053053\n"
    "thekernel_rseq_abort:\n"
    "    cmpl $0,signal_seen(%rip)\n"
    "    je thekernel_rseq_window\n"
    "    movl $1,thekernel_rseq_abort_seen(%rip)\n"
    "    movb $1,(%rdx)\n"
    "    ret\n"
    ".size thekernel_rseq_abort,.-thekernel_rseq_abort\n");

extern void thekernel_rseq_window(struct rseq_area *area,
                                  struct rseq_critical_section *critical_section,
                                  volatile sig_atomic_t *stop,
                                  volatile sig_atomic_t *ready);
extern char thekernel_rseq_start[];
extern char thekernel_rseq_post[];
extern char thekernel_rseq_abort[];

static int fail(const char *stage)
{
    fprintf(stderr, "THEKERNEL_RSEQ_FAIL %s errno=%d (%s)\n", stage,
            errno, strerror(errno));
    return 1;
}

static int fail_value(const char *stage, long actual, long expected)
{
    fprintf(stderr,
            "THEKERNEL_RSEQ_FAIL %s actual=%ld expected=%ld errno=%d (%s)\n",
            stage, actual, expected, errno, strerror(errno));
    return 1;
}

static void marker(const char *value)
{
    puts(value);
    fflush(stdout);
}

static long rseq_call(struct rseq_area *area, uint32_t length, uint32_t flags,
                      uint32_t signature)
{
    return syscall(__NR_rseq, area, length, flags, signature);
}

static int expect_errno(const char *stage, struct rseq_area *area,
                        uint32_t length, uint32_t flags, uint32_t signature,
                        int expected)
{
    errno = 0;
    long result = rseq_call(area, length, flags, signature);
    int saved_errno = errno;
    if (result != -1 || saved_errno != expected) {
        errno = saved_errno;
        return fail_value(stage, result, -1);
    }
    return 0;
}

static int area_is_published(const volatile struct rseq_area *area,
                             const char *stage)
{
    uint32_t cpu_id_start = area->cpu_id_start;
    uint32_t cpu_id = area->cpu_id;
    if (cpu_id_start == RSEQ_POISON || cpu_id == RSEQ_POISON ||
        cpu_id_start == RSEQ_CPU_ID_UNINITIALIZED ||
        cpu_id_start == RSEQ_CPU_ID_REGISTRATION_FAILED ||
        cpu_id != cpu_id_start) {
        errno = EPROTO;
        return fail(stage);
    }
    return 0;
}

static void initialize_inactive_area(struct rseq_area *area)
{
    memset(area, 0xa5, sizeof(*area));
    area->rseq_cs = 0;
    area->flags = 0;
}

static int monotonic_ms(uint64_t *result)
{
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        return -1;
    }
    *result = (uint64_t)now.tv_sec * UINT64_C(1000) +
              (uint64_t)now.tv_nsec / UINT64_C(1000000);
    return 0;
}

static int wait_for_child(pid_t child, int timeout_ms, int *status,
                          const char *stage)
{
    uint64_t start;
    if (monotonic_ms(&start) != 0) {
        return fail(stage);
    }
    for (;;) {
        pid_t waited = waitpid(child, status, WNOHANG);
        if (waited == child) {
            return 0;
        }
        if (waited < 0) {
            if (errno == EINTR) {
                continue;
            }
            return fail(stage);
        }

        uint64_t now;
        if (monotonic_ms(&now) != 0) {
            (void)kill(child, SIGKILL);
            (void)waitpid(child, status, 0);
            return fail(stage);
        }
        if (now - start >= (uint64_t)timeout_ms) {
            (void)kill(child, SIGKILL);
            (void)waitpid(child, status, 0);
            errno = ETIMEDOUT;
            return fail(stage);
        }
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 1000000};
        (void)nanosleep(&pause, NULL);
    }
}

static int expect_child_success(pid_t child, const char *stage)
{
    int status = 0;
    if (wait_for_child(child, 3000, &status, stage) != 0) {
        return 1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        if (WIFSIGNALED(status)) {
            fprintf(stderr, "THEKERNEL_RSEQ_FAIL %s signal=%d\n", stage,
                    WTERMSIG(status));
        } else {
            fprintf(stderr, "THEKERNEL_RSEQ_FAIL %s status=0x%x\n", stage,
                    status);
        }
        return 1;
    }
    return 0;
}

static int wait_for_ready(const struct signal_control *control,
                          const char *stage)
{
    uint64_t start;
    if (monotonic_ms(&start) != 0) {
        return fail(stage);
    }
    for (;;) {
        if (control->ready != 0) {
            return 0;
        }
        uint64_t now;
        if (monotonic_ms(&now) != 0 || now - start >= UINT64_C(2000)) {
            errno = ETIMEDOUT;
            return fail(stage);
        }
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 1000000};
        (void)nanosleep(&pause, NULL);
    }
}

static int read_auxv(unsigned long *feature_size, unsigned long *alignment)
{
    errno = 0;
    *feature_size = getauxval(AT_RSEQ_FEATURE_SIZE);
    if (*feature_size == 0 && errno != 0) {
        return fail("auxv-rseq-feature-size");
    }
    errno = 0;
    *alignment = getauxval(AT_RSEQ_ALIGN);
    if (*alignment == 0 && errno != 0) {
        return fail("auxv-rseq-align");
    }
    return 0;
}

static int test_auxv(void)
{
    unsigned long feature_size = 0;
    unsigned long alignment = 0;
    if (read_auxv(&feature_size, &alignment) != 0) {
        return 1;
    }
    if (feature_size != 24UL || alignment != 32UL) {
        errno = EPROTO;
        fprintf(stderr,
                "THEKERNEL_RSEQ_FAIL auxv-values feature_size=%lu align=%lu\n",
                feature_size, alignment);
        return 1;
    }
    marker("THEKERNEL_RSEQ_AUXV_OK feature_size=24 align=32");
    return 0;
}

static void *map_anonymous_area(size_t page_size)
{
    return mmap(NULL, page_size, PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
}

/* Seed a private file-backed page without touching the eventual mapping.  A
 * successful rseq return gate must therefore perform the first page touch and
 * replace the poison CPU fields; a later user read alone cannot make this case
 * pass if the gate is missing. */
static void *map_seeded_area(size_t page_size)
{
    char path[] = "/tmp/thekernel-rseq-XXXXXX";
    int fd = mkstemp(path);
    if (fd < 0) {
        return MAP_FAILED;
    }
    (void)unlink(path);
    if (ftruncate(fd, (off_t)page_size) != 0) {
        int saved_errno = errno;
        close(fd);
        errno = saved_errno;
        return MAP_FAILED;
    }

    unsigned char *seed = malloc(page_size);
    if (seed == NULL) {
        int saved_errno = errno;
        close(fd);
        errno = saved_errno;
        return MAP_FAILED;
    }
    memset(seed, (unsigned char)0xa5, page_size);
    struct rseq_area initial;
    initialize_inactive_area(&initial);
    memcpy(seed, &initial, sizeof(initial));
    size_t written = 0;
    while (written < page_size) {
        ssize_t count = write(fd, seed + written, page_size - written);
        if (count < 0 && errno == EINTR) {
            continue;
        }
        if (count <= 0) {
            int saved_errno = count == 0 ? EIO : errno;
            free(seed);
            close(fd);
            errno = saved_errno;
            return MAP_FAILED;
        }
        written += (size_t)count;
    }
    free(seed);

    void *mapping = mmap(NULL, page_size, PROT_READ | PROT_WRITE, MAP_PRIVATE,
                         fd, 0);
    int saved_errno = errno;
    if (close(fd) != 0 && mapping != MAP_FAILED) {
        (void)munmap(mapping, page_size);
        return MAP_FAILED;
    }
    errno = saved_errno;
    return mapping;
}

static int test_registration(size_t page_size)
{
    struct rseq_area *area = map_anonymous_area(page_size);
    if (area == MAP_FAILED) {
        return fail("registration-mmap");
    }
    initialize_inactive_area(area);

    if (expect_errno("registration-length", area, RSEQ_AREA_SIZE - 1U, 0,
                     RSEQ_SIG, EINVAL) != 0 ||
        expect_errno("registration-flags", area, RSEQ_AREA_SIZE, 2U,
                     RSEQ_SIG, EINVAL) != 0) {
        (void)munmap(area, page_size);
        return 1;
    }

    errno = 0;
    if (rseq_call(area, RSEQ_AREA_SIZE, 0, RSEQ_SIG) != 0) {
        int saved_errno = errno;
        (void)munmap(area, page_size);
        errno = saved_errno;
        return fail("registration-success");
    }
    if (area_is_published(area, "registration-cpu-publication") != 0) {
        (void)munmap(area, page_size);
        return 1;
    }
    if (expect_errno("registration-duplicate", area, RSEQ_AREA_SIZE, 0,
                     RSEQ_SIG, EBUSY) != 0 ||
        expect_errno("unregister-signature", area, RSEQ_AREA_SIZE,
                     RSEQ_FLAG_UNREGISTER, RSEQ_SIG ^ 1U, EPERM) != 0) {
        (void)munmap(area, page_size);
        return 1;
    }

    errno = 0;
    if (rseq_call(area, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER, RSEQ_SIG) !=
        0) {
        int saved_errno = errno;
        (void)munmap(area, page_size);
        errno = saved_errno;
        return fail("unregister-success");
    }
    if (munmap(area, page_size) != 0) {
        return fail("registration-munmap");
    }
    marker("THEKERNEL_RSEQ_REGISTRATION_OK");
    return 0;
}

static int test_first_touch(size_t page_size)
{
    struct rseq_area *area = map_seeded_area(page_size);
    if (area == MAP_FAILED) {
        return fail("first-touch-mmap");
    }

    errno = 0;
    if (rseq_call(area, RSEQ_AREA_SIZE, 0, RSEQ_SIG) != 0) {
        int saved_errno = errno;
        (void)munmap(area, page_size);
        errno = saved_errno;
        return fail("first-touch-registration");
    }
    if (area_is_published(area, "first-touch-cpu-publication") != 0) {
        (void)rseq_call(area, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER, RSEQ_SIG);
        (void)munmap(area, page_size);
        return 1;
    }
    if (rseq_call(area, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER, RSEQ_SIG) !=
        0) {
        int saved_errno = errno;
        (void)munmap(area, page_size);
        errno = saved_errno;
        return fail("first-touch-unregister");
    }
    if (munmap(area, page_size) != 0) {
        return fail("first-touch-munmap");
    }
    marker("THEKERNEL_RSEQ_FIRST_TOUCH_OK");
    return 0;
}

static int test_fork_cow(size_t page_size)
{
    struct rseq_area *area = map_anonymous_area(page_size);
    if (area == MAP_FAILED) {
        return fail("fork-cow-mmap");
    }
    initialize_inactive_area(area);

    if (rseq_call(area, RSEQ_AREA_SIZE, 0, RSEQ_SIG) != 0) {
        int saved_errno = errno;
        (void)munmap(area, page_size);
        errno = saved_errno;
        return fail("fork-cow-registration");
    }
    /* Force both post-fork return gates to publish fresh CPU fields instead
     * of inheriting the parent's pre-fork publication. */
    area->cpu_id_start = RSEQ_POISON;
    area->cpu_id = RSEQ_POISON;

    pid_t child = fork();
    if (child < 0) {
        int saved_errno = errno;
        (void)rseq_call(area, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER, RSEQ_SIG);
        (void)munmap(area, page_size);
        errno = saved_errno;
        return fail("fork-cow-fork");
    }
    if (child == 0) {
        if (syscall(SYS_getpid) < 0 ||
            area_is_published(area, "fork-cow-child-cpu-publication") != 0) {
            _exit(1);
        }
        /* This write must become private to the child.  The parent checks that
         * its own COW page did not observe it after waiting for us. */
        area->flags = RSEQ_CHILD_COW_MARK;
        _exit(0);
    }

    int status = 0;
    if (wait_for_child(child, 3000, &status, "fork-cow-child-wait") != 0) {
        (void)rseq_call(area, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER, RSEQ_SIG);
        (void)munmap(area, page_size);
        return 1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        (void)rseq_call(area, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER, RSEQ_SIG);
        (void)munmap(area, page_size);
        errno = EPROTO;
        return fail("fork-cow-child-status");
    }
    if (area->flags == RSEQ_CHILD_COW_MARK) {
        (void)rseq_call(area, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER, RSEQ_SIG);
        (void)munmap(area, page_size);
        errno = EFAULT;
        return fail("fork-cow-shared-page");
    }
    if (syscall(SYS_getpid) < 0 ||
        area_is_published(area, "fork-cow-parent-cpu-publication") != 0) {
        (void)rseq_call(area, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER, RSEQ_SIG);
        (void)munmap(area, page_size);
        return 1;
    }
    if (rseq_call(area, RSEQ_AREA_SIZE, RSEQ_FLAG_UNREGISTER, RSEQ_SIG) !=
        0) {
        int saved_errno = errno;
        (void)munmap(area, page_size);
        errno = saved_errno;
        return fail("fork-cow-unregister");
    }
    if (munmap(area, page_size) != 0) {
        return fail("fork-cow-munmap");
    }
    marker("THEKERNEL_RSEQ_FORK_COW_OK");
    return 0;
}

static void signal_handler(int signo, siginfo_t *info, void *context)
{
    (void)signo;
    (void)info;
    const ucontext_t *ucontext = context;
    if ((uintptr_t)ucontext->uc_mcontext.gregs[REG_RIP] ==
        (uintptr_t)thekernel_rseq_abort) {
        signal_seen = 1;
        if (signal_stop != NULL) {
            *signal_stop = 1;
        }
    }
}

static int install_signal_handler(void)
{
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    sigemptyset(&action.sa_mask);
    action.sa_sigaction = signal_handler;
    action.sa_flags = SA_SIGINFO;
    if (sigaction(SIGUSR1, &action, NULL) != 0) {
        return fail("signal-abort-handler");
    }
    return 0;
}

static int setup_active_rseq(struct rseq_area *area,
                             struct rseq_critical_section *critical_section)
{
    if (rseq_call(area, RSEQ_AREA_SIZE, 0, RSEQ_SIG) != 0) {
        return fail("signal-abort-registration");
    }
    uintptr_t start = (uintptr_t)thekernel_rseq_start;
    uintptr_t post = (uintptr_t)thekernel_rseq_post;
    uintptr_t abort_ip = (uintptr_t)thekernel_rseq_abort;
    if (post <= start || abort_ip < sizeof(uint32_t)) {
        errno = EPROTO;
        return fail("signal-abort-assembly-layout");
    }
    memset(critical_section, 0, sizeof(*critical_section));
    critical_section->start_ip = (uint64_t)start;
    critical_section->post_commit_offset = (uint64_t)(post - start);
    critical_section->abort_ip = (uint64_t)abort_ip;
    return 0;
}

static int run_signal_abort_child(struct rseq_area *area,
                                  struct rseq_critical_section *critical_section,
                                  struct signal_control *control)
{
    if (install_signal_handler() != 0 ||
        setup_active_rseq(area, critical_section) != 0) {
        _exit(1);
    }
    thekernel_rseq_abort_seen = 0;
    signal_seen = 0;
    signal_stop = &control->stop;
    thekernel_rseq_window(area, critical_section, &control->stop,
                          &control->ready);
    if (signal_seen == 0) {
        _exit(3);
    }
    if (thekernel_rseq_abort_seen == 0) {
        _exit(4);
    }
    _exit(0);
}

static int run_sigkill_child(struct rseq_area *area,
                             struct rseq_critical_section *critical_section,
                             struct signal_control *control)
{
    if (setup_active_rseq(area, critical_section) != 0) {
        _exit(1);
    }
    thekernel_rseq_window(area, critical_section, &control->stop,
                          &control->ready);
    _exit(2);
}

static int send_until_signal_abort(pid_t child,
                                   const struct signal_control *control)
{
    uint64_t start;
    if (monotonic_ms(&start) != 0) {
        return fail("signal-abort-send");
    }
    while (control->stop == 0) {
        if (kill(child, SIGUSR1) != 0) {
            return fail("signal-abort-send");
        }
        uint64_t now;
        if (monotonic_ms(&now) != 0 || now - start >= UINT64_C(2000)) {
            errno = ETIMEDOUT;
            return fail("signal-abort-send");
        }
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 1000000};
        (void)nanosleep(&pause, NULL);
    }
    return 0;
}

static int test_signal_abort(size_t page_size)
{
    struct rseq_area *area = map_anonymous_area(page_size);
    struct signal_control *control = mmap(
        NULL, page_size, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS,
        -1, 0);
    if (area == MAP_FAILED || control == MAP_FAILED) {
        if (area != MAP_FAILED) {
            (void)munmap(area, page_size);
        }
        if (control != MAP_FAILED) {
            (void)munmap(control, page_size);
        }
        return fail("signal-abort-mmap");
    }
    memset(area, 0, sizeof(*area));
    memset(control, 0, sizeof(*control));
    struct rseq_critical_section *critical_section =
        mmap(NULL, page_size, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (critical_section == MAP_FAILED) {
        (void)munmap(area, page_size);
        (void)munmap(control, page_size);
        return fail("signal-abort-descriptor-mmap");
    }

    pid_t child = fork();
    if (child < 0) {
        int saved_errno = errno;
        (void)munmap(critical_section, page_size);
        (void)munmap(area, page_size);
        (void)munmap(control, page_size);
        errno = saved_errno;
        return fail("signal-abort-fork");
    }
    if (child == 0) {
        run_signal_abort_child(area, critical_section, control);
    }
    if (wait_for_ready(control, "signal-abort-ready") != 0 ||
        send_until_signal_abort(child, control) != 0) {
        (void)kill(child, SIGKILL);
        (void)waitpid(child, NULL, 0);
        (void)munmap(critical_section, page_size);
        (void)munmap(area, page_size);
        (void)munmap(control, page_size);
        return fail("signal-abort-send");
    }
    if (expect_child_success(child, "signal-abort-child") != 0) {
        (void)munmap(critical_section, page_size);
        (void)munmap(area, page_size);
        (void)munmap(control, page_size);
        return 1;
    }
    if (munmap(critical_section, page_size) != 0 ||
        munmap(area, page_size) != 0 || munmap(control, page_size) != 0) {
        return fail("signal-abort-munmap");
    }
    marker("THEKERNEL_RSEQ_SIGNAL_ABORT_OK");
    return 0;
}

static int test_sigkill_not_blocked(size_t page_size)
{
    struct rseq_area *area = map_anonymous_area(page_size);
    struct signal_control *control = mmap(
        NULL, page_size, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS,
        -1, 0);
    if (area == MAP_FAILED || control == MAP_FAILED) {
        if (area != MAP_FAILED) {
            (void)munmap(area, page_size);
        }
        if (control != MAP_FAILED) {
            (void)munmap(control, page_size);
        }
        return fail("sigkill-mmap");
    }
    memset(area, 0, sizeof(*area));
    memset(control, 0, sizeof(*control));
    struct rseq_critical_section *critical_section =
        mmap(NULL, page_size, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (critical_section == MAP_FAILED) {
        (void)munmap(area, page_size);
        (void)munmap(control, page_size);
        return fail("sigkill-descriptor-mmap");
    }

    pid_t child = fork();
    if (child < 0) {
        int saved_errno = errno;
        (void)munmap(critical_section, page_size);
        (void)munmap(area, page_size);
        (void)munmap(control, page_size);
        errno = saved_errno;
        return fail("sigkill-fork");
    }
    if (child == 0) {
        run_sigkill_child(area, critical_section, control);
    }
    if (wait_for_ready(control, "sigkill-ready") != 0 ||
        kill(child, SIGKILL) != 0) {
        (void)kill(child, SIGKILL);
        (void)waitpid(child, NULL, 0);
        (void)munmap(critical_section, page_size);
        (void)munmap(area, page_size);
        (void)munmap(control, page_size);
        return fail("sigkill-send");
    }

    int status = 0;
    if (wait_for_child(child, 2000, &status, "sigkill-wait") != 0) {
        (void)munmap(critical_section, page_size);
        (void)munmap(area, page_size);
        (void)munmap(control, page_size);
        return 1;
    }
    if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGKILL) {
        (void)munmap(critical_section, page_size);
        (void)munmap(area, page_size);
        (void)munmap(control, page_size);
        errno = EPROTO;
        return fail("sigkill-default-action");
    }
    if (munmap(critical_section, page_size) != 0 ||
        munmap(area, page_size) != 0 || munmap(control, page_size) != 0) {
        return fail("sigkill-munmap");
    }
    marker("THEKERNEL_RSEQ_SIGKILL_OK");
    return 0;
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    long raw_page_size = sysconf(_SC_PAGESIZE);
    if (raw_page_size <= 0) {
        return fail("page-size");
    }
    size_t page_size = (size_t)raw_page_size;
    if (page_size < sizeof(struct rseq_area)) {
        errno = EINVAL;
        return fail("page-size-small");
    }

    if (test_auxv() != 0 || test_registration(page_size) != 0 ||
        test_first_touch(page_size) != 0 || test_fork_cow(page_size) != 0 ||
        test_signal_abort(page_size) != 0 ||
        test_sigkill_not_blocked(page_size) != 0) {
        return 1;
    }
    marker("THEKERNEL_RSEQ_OK");
    return 0;
}
