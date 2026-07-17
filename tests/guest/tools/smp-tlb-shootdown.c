#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <setjmp.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef MREMAP_MAYMOVE
#define MREMAP_MAYMOVE 1
#endif

#ifndef MREMAP_FIXED
#define MREMAP_FIXED 2
#endif

enum {
    WAIT_TIMEOUT_SECONDS = 10,
    WAIT_PAUSE_NANOSECONDS = 1000000,
    CHILD_REPORT_MAGIC = 0x544c4247,
};

enum worker_phase {
    WORKER_INIT = 0,
    WORKER_WARMED = 1,
    WORKER_GO = 2,
    WORKER_ABORT = 3,
    WORKER_DONE = 4,
};

enum worker_operation {
    WORKER_REVOKED_WRITE,
    WORKER_OLD_ALIAS_READ,
    WORKER_COW_WRITE,
};

struct worker_context {
    atomic_int phase;
    atomic_int error_number;
    atomic_size_t stale_count;
    atomic_size_t spin_heartbeat;
    enum worker_operation operation;
    volatile unsigned char *mapping;
    size_t pages;
    size_t page_size;
    int worker_cpu;
    int actual_cpu_before;
    int actual_cpu_after;
    volatile unsigned char sink;
};

struct child_report {
    uint32_t magic;
    uint32_t reserved;
    uint64_t stale_count;
};

static _Thread_local sigjmp_buf expected_fault_environment;
static _Thread_local volatile sig_atomic_t expected_fault_active;
static _Thread_local void *volatile expected_fault_address;

static _Noreturn void operational_fail(const char *case_name, size_t pages,
                                       const char *step, int error_number)
{
    printf("SMP_TLB_GATE status=fail kind=operational case=%s pages=%zu"
           " step=%s errno=%d\n",
           case_name, pages, step, error_number);
    fflush(stdout);
    _Exit(2);
}

static int monotonic_now(uint64_t *result)
{
    struct timespec now;

    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        return -1;
    }
    *result = (uint64_t)now.tv_sec * UINT64_C(1000000000) +
              (uint64_t)now.tv_nsec;
    return 0;
}

static int monotonic_deadline(uint64_t *result)
{
    uint64_t now;
    const uint64_t timeout =
        (uint64_t)WAIT_TIMEOUT_SECONDS * UINT64_C(1000000000);

    if (monotonic_now(&now) != 0) {
        return -1;
    }
    *result = UINT64_MAX - now < timeout ? UINT64_MAX : now + timeout;
    return 0;
}

static void pause_before_retry(void)
{
    const struct timespec pause = {
        .tv_sec = 0,
        .tv_nsec = WAIT_PAUSE_NANOSECONDS,
    };

    (void)nanosleep(&pause, NULL);
}

static int deadline_remaining_ms(uint64_t deadline, int *milliseconds)
{
    uint64_t now;
    uint64_t remaining;

    if (monotonic_now(&now) != 0) {
        return -1;
    }
    if (now >= deadline) {
        *milliseconds = 0;
        return 0;
    }
    remaining = deadline - now;
    remaining = (remaining + UINT64_C(999999)) / UINT64_C(1000000);
    *milliseconds = remaining > (uint64_t)INT_MAX ? INT_MAX : (int)remaining;
    return 1;
}

static void expected_sigsegv_handler(int signal_number, siginfo_t *info,
                                     void *context)
{
    static const char unexpected[] =
        "SMP_TLB_GATE status=fail kind=signal reason=unexpected_sigsegv\n";

    (void)context;
    if (signal_number == SIGSEGV && expected_fault_active != 0 &&
        info != NULL && info->si_addr == expected_fault_address) {
        expected_fault_active = 0;
        expected_fault_address = NULL;
        siglongjmp(expected_fault_environment, 1);
    }
    (void)write(STDERR_FILENO, unexpected, sizeof(unexpected) - 1U);
    _exit(128 + SIGSEGV);
}

static void install_signal_handlers(void)
{
    struct sigaction action;
    struct sigaction ignore;

    memset(&action, 0, sizeof(action));
    sigemptyset(&action.sa_mask);
    action.sa_sigaction = expected_sigsegv_handler;
    action.sa_flags = SA_SIGINFO;
    if (sigaction(SIGSEGV, &action, NULL) != 0) {
        operational_fail("setup", 0, "sigaction", errno);
    }

    memset(&ignore, 0, sizeof(ignore));
    sigemptyset(&ignore.sa_mask);
    ignore.sa_handler = SIG_IGN;
    if (sigaction(SIGPIPE, &ignore, NULL) != 0) {
        operational_fail("setup", 0, "sigpipe", errno);
    }
}

static bool guarded_read(volatile unsigned char *address,
                         unsigned char *value)
{
    if (sigsetjmp(expected_fault_environment, 1) != 0) {
        expected_fault_active = 0;
        expected_fault_address = NULL;
        return false;
    }
    expected_fault_address = (void *)address;
    expected_fault_active = 1;
    atomic_signal_fence(memory_order_seq_cst);
    *value = *address;
    atomic_signal_fence(memory_order_seq_cst);
    expected_fault_active = 0;
    expected_fault_address = NULL;
    return true;
}

static bool guarded_write(volatile unsigned char *address,
                          unsigned char value)
{
    if (sigsetjmp(expected_fault_environment, 1) != 0) {
        expected_fault_active = 0;
        expected_fault_address = NULL;
        return false;
    }
    expected_fault_address = (void *)address;
    expected_fault_active = 1;
    atomic_signal_fence(memory_order_seq_cst);
    *address = value;
    atomic_signal_fence(memory_order_seq_cst);
    expected_fault_active = 0;
    expected_fault_address = NULL;
    return true;
}

static unsigned char old_pattern(size_t page)
{
    return (unsigned char)(0x20U ^ (page & 0xffU));
}

static unsigned char new_pattern(size_t page)
{
    return (unsigned char)(0xa0U ^ (page & 0xffU));
}

static unsigned char cow_parent_pattern(size_t page)
{
    return (unsigned char)(0xdfU ^ (page & 0xffU));
}

static void fill_page_sentinels(volatile unsigned char *mapping, size_t pages,
                                size_t page_size,
                                unsigned char (*pattern)(size_t))
{
    size_t page;

    for (page = 0; page < pages; ++page) {
        mapping[page * page_size] = pattern(page);
    }
}

static bool page_sentinels_match(volatile unsigned char *mapping, size_t pages,
                                 size_t page_size,
                                 unsigned char (*pattern)(size_t))
{
    size_t page;

    for (page = 0; page < pages; ++page) {
        unsigned char value = 0;

        if (!guarded_read(mapping + page * page_size, &value) ||
            value != pattern(page)) {
            return false;
        }
    }
    return true;
}

static int pin_current_thread(int cpu)
{
    cpu_set_t affinity;

    CPU_ZERO(&affinity);
    CPU_SET(cpu, &affinity);
    return pthread_setaffinity_np(pthread_self(), sizeof(affinity), &affinity);
}

static void set_worker_error(struct worker_context *context, int error_number)
{
    atomic_store_explicit(&context->error_number, error_number,
                          memory_order_relaxed);
    atomic_store_explicit(&context->phase, WORKER_DONE, memory_order_release);
}

static int worker_wait_for_release(struct worker_context *context)
{
    size_t heartbeat = 0;

    for (;;) {
        int phase = atomic_load_explicit(&context->phase,
                                         memory_order_acquire);

        if (phase == WORKER_GO) {
            return 0;
        }
        if (phase == WORKER_ABORT) {
            return ECANCELED;
        }
        heartbeat += 1U;
        if ((heartbeat & 0xffU) == 0) {
            atomic_store_explicit(&context->spin_heartbeat, heartbeat,
                                  memory_order_relaxed);
        }
        atomic_signal_fence(memory_order_seq_cst);
    }
}

static void worker_warm_mapping(struct worker_context *context)
{
    size_t page;
    unsigned char sink = 0;

    for (page = 0; page < context->pages; ++page) {
        volatile unsigned char *address =
            context->mapping + page * context->page_size;

        switch (context->operation) {
        case WORKER_REVOKED_WRITE:
        case WORKER_COW_WRITE:
            *address = old_pattern(page);
            break;
        case WORKER_OLD_ALIAS_READ:
            sink ^= *address;
            break;
        }
    }
    context->sink = sink;
}

static size_t worker_run_transition(struct worker_context *context)
{
    size_t stale_count = 0;
    size_t remaining = context->pages;

    /* Probe the most recently warmed translations before fault delivery. */
    while (remaining != 0) {
        const size_t page = --remaining;
        volatile unsigned char *address =
            context->mapping + page * context->page_size;
        unsigned char value = 0;

        switch (context->operation) {
        case WORKER_REVOKED_WRITE:
            if (guarded_write(address, new_pattern(page))) {
                stale_count += 1U;
            }
            break;
        case WORKER_OLD_ALIAS_READ:
            if (guarded_read(address, &value)) {
                stale_count += 1U;
            }
            break;
        case WORKER_COW_WRITE:
            if (!guarded_write(address, cow_parent_pattern(page))) {
                stale_count += 1U;
            }
            break;
        }
    }
    return stale_count;
}

static void *mapping_worker(void *argument)
{
    struct worker_context *context = argument;
    int error_number;
    size_t stale_count;

    error_number = pin_current_thread(context->worker_cpu);
    if (error_number != 0) {
        set_worker_error(context, error_number);
        return NULL;
    }
    errno = 0;
    context->actual_cpu_before = sched_getcpu();
    if (context->actual_cpu_before < 0 ||
        context->actual_cpu_before != context->worker_cpu) {
        set_worker_error(context,
                         context->actual_cpu_before < 0
                             ? (errno != 0 ? errno : EIO)
                             : EXDEV);
        return NULL;
    }
    worker_warm_mapping(context);
    atomic_store_explicit(&context->phase, WORKER_WARMED,
                          memory_order_release);
    error_number = worker_wait_for_release(context);
    if (error_number == ECANCELED) {
        atomic_store_explicit(&context->phase, WORKER_DONE,
                              memory_order_release);
        return NULL;
    }
    if (error_number != 0) {
        set_worker_error(context, error_number);
        return NULL;
    }
    stale_count = worker_run_transition(context);
    errno = 0;
    context->actual_cpu_after = sched_getcpu();
    if (context->actual_cpu_after < 0 ||
        context->actual_cpu_after != context->worker_cpu) {
        set_worker_error(context,
                         context->actual_cpu_after < 0
                             ? (errno != 0 ? errno : EIO)
                             : EXDEV);
        return NULL;
    }
    atomic_store_explicit(&context->stale_count, stale_count,
                          memory_order_relaxed);
    atomic_store_explicit(&context->phase, WORKER_DONE,
                          memory_order_release);
    return NULL;
}

static int wait_for_worker_phase(struct worker_context *context,
                                 enum worker_phase wanted)
{
    uint64_t deadline;

    if (monotonic_deadline(&deadline) != 0) {
        return errno != 0 ? errno : EIO;
    }
    for (;;) {
        int phase = atomic_load_explicit(&context->phase,
                                         memory_order_acquire);
        uint64_t now;

        if (phase == (int)wanted) {
            return 0;
        }
        if (phase == WORKER_DONE) {
            int worker_error = atomic_load_explicit(&context->error_number,
                                                    memory_order_relaxed);

            return worker_error != 0 ? worker_error : EPROTO;
        }
        if (monotonic_now(&now) != 0) {
            return errno != 0 ? errno : EIO;
        }
        if (now >= deadline) {
            return ETIMEDOUT;
        }
        pause_before_retry();
    }
}

static int join_worker_bounded(pthread_t worker)
{
    uint64_t deadline;

    if (monotonic_deadline(&deadline) != 0) {
        return errno != 0 ? errno : EIO;
    }
    for (;;) {
        int result = pthread_tryjoin_np(worker, NULL);
        uint64_t now;

        if (result == 0) {
            return 0;
        }
        if (result != EBUSY) {
            return result;
        }
        if (monotonic_now(&now) != 0) {
            return errno != 0 ? errno : EIO;
        }
        if (now >= deadline) {
            return ETIMEDOUT;
        }
        pause_before_retry();
    }
}

static void initialize_worker(struct worker_context *context,
                              enum worker_operation operation,
                              volatile unsigned char *mapping, size_t pages,
                              size_t page_size, int worker_cpu)
{
    atomic_init(&context->phase, WORKER_INIT);
    atomic_init(&context->error_number, 0);
    atomic_init(&context->stale_count, 0);
    atomic_init(&context->spin_heartbeat, 0);
    context->operation = operation;
    context->mapping = mapping;
    context->pages = pages;
    context->page_size = page_size;
    context->worker_cpu = worker_cpu;
    context->actual_cpu_before = -1;
    context->actual_cpu_after = -1;
    context->sink = 0;
}

static pthread_t start_worker(struct worker_context *context,
                              const char *case_name, size_t pages)
{
    pthread_t worker;
    int result;

    result = pthread_create(&worker, NULL, mapping_worker, context);
    if (result != 0) {
        operational_fail(case_name, pages, "pthread_create", result);
    }
    result = wait_for_worker_phase(context, WORKER_WARMED);
    if (result != 0) {
        atomic_store_explicit(&context->phase, WORKER_ABORT,
                              memory_order_release);
        (void)join_worker_bounded(worker);
        operational_fail(case_name, pages, "worker_warm", result);
    }
    return worker;
}

static size_t release_and_join_worker(struct worker_context *context,
                                      pthread_t worker,
                                      const char *case_name, size_t pages,
                                      size_t heartbeat_before)
{
    uint64_t deadline;
    int result;

    if (monotonic_deadline(&deadline) != 0) {
        operational_fail(case_name, pages, "worker_progress_clock",
                         errno != 0 ? errno : EIO);
    }
    for (;;) {
        size_t heartbeat = atomic_load_explicit(&context->spin_heartbeat,
                                                memory_order_relaxed);
        uint64_t now;

        if (heartbeat != heartbeat_before) {
            break;
        }
        if (atomic_load_explicit(&context->phase, memory_order_acquire) ==
            WORKER_DONE) {
            int worker_error = atomic_load_explicit(&context->error_number,
                                                    memory_order_relaxed);

            operational_fail(case_name, pages, "worker_progress",
                             worker_error != 0 ? worker_error : EPROTO);
        }
        if (monotonic_now(&now) != 0) {
            operational_fail(case_name, pages, "worker_progress_clock",
                             errno != 0 ? errno : EIO);
        }
        if (now >= deadline) {
            operational_fail(case_name, pages, "worker_progress", ETIMEDOUT);
        }
        pause_before_retry();
    }
    atomic_store_explicit(&context->phase, WORKER_GO, memory_order_release);
    result = wait_for_worker_phase(context, WORKER_DONE);
    if (result != 0) {
        operational_fail(case_name, pages, "worker_transition", result);
    }
    result = join_worker_bounded(worker);
    if (result != 0) {
        operational_fail(case_name, pages, "pthread_join", result);
    }
    return atomic_load_explicit(&context->stale_count, memory_order_relaxed);
}

static void emit_case_result(const char *case_name, size_t pages,
                             int worker_cpu, size_t stale_count)
{
    printf("SMP_TLB_CASE case=%s pages=%zu worker_cpu=%d status=%s"
           " stale_count=%zu\n",
           case_name, pages, worker_cpu,
           stale_count == 0 ? "ok" : "stale", stale_count);
}

static volatile unsigned char *map_rw(size_t length, const char *case_name,
                                      size_t pages, const char *step)
{
    void *mapping = mmap(NULL, length, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

    if (mapping == MAP_FAILED) {
        operational_fail(case_name, pages, step, errno);
    }
    return mapping;
}

static void checked_unmap(volatile unsigned char *mapping, size_t length,
                          const char *case_name, size_t pages,
                          const char *step)
{
    if (munmap((void *)mapping, length) != 0) {
        operational_fail(case_name, pages, step, errno);
    }
}

static size_t run_mprotect_case(size_t pages, size_t page_size,
                                int worker_cpu)
{
    static const char case_name[] = "mprotect_revoke_write";
    const size_t length = pages * page_size;
    volatile unsigned char *mapping =
        map_rw(length, case_name, pages, "mmap");
    struct worker_context context;
    pthread_t worker;
    size_t heartbeat_before;
    size_t stale_count;

    fill_page_sentinels(mapping, pages, page_size, old_pattern);
    initialize_worker(&context, WORKER_REVOKED_WRITE, mapping, pages,
                      page_size, worker_cpu);
    worker = start_worker(&context, case_name, pages);
    heartbeat_before = atomic_load_explicit(&context.spin_heartbeat,
                                            memory_order_relaxed);
    if (mprotect((void *)mapping, length, PROT_READ) != 0) {
        int saved_errno = errno;

        atomic_store_explicit(&context.phase, WORKER_ABORT,
                              memory_order_release);
        (void)join_worker_bounded(worker);
        operational_fail(case_name, pages, "mprotect_read", saved_errno);
    }
    if (!page_sentinels_match(mapping, pages, page_size, old_pattern)) {
        atomic_store_explicit(&context.phase, WORKER_ABORT,
                              memory_order_release);
        (void)join_worker_bounded(worker);
        operational_fail(case_name, pages, "read_only_content", EIO);
    }
    stale_count = release_and_join_worker(&context, worker, case_name, pages,
                                          heartbeat_before);
    if (mprotect((void *)mapping, length, PROT_READ | PROT_WRITE) != 0) {
        operational_fail(case_name, pages, "mprotect_restore", errno);
    }
    checked_unmap(mapping, length, case_name, pages, "munmap");
    emit_case_result(case_name, pages, worker_cpu, stale_count);
    return stale_count;
}

static size_t run_munmap_replace_case(size_t pages, size_t page_size,
                                      int worker_cpu)
{
    static const char case_name[] = "munmap_fixed_replace";
    const size_t length = pages * page_size;
    volatile unsigned char *mapping =
        map_rw(length, case_name, pages, "mmap_old");
    struct worker_context context;
    pthread_t worker;
    void *replacement;
    size_t heartbeat_before;
    size_t stale_count;

    fill_page_sentinels(mapping, pages, page_size, old_pattern);
    initialize_worker(&context, WORKER_OLD_ALIAS_READ, mapping, pages,
                      page_size, worker_cpu);
    worker = start_worker(&context, case_name, pages);
    heartbeat_before = atomic_load_explicit(&context.spin_heartbeat,
                                            memory_order_relaxed);
    if (munmap((void *)mapping, length) != 0) {
        int saved_errno = errno;

        atomic_store_explicit(&context.phase, WORKER_ABORT,
                              memory_order_release);
        (void)join_worker_bounded(worker);
        operational_fail(case_name, pages, "munmap_old", saved_errno);
    }
    replacement = mmap((void *)mapping, length, PROT_NONE,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    if (replacement == MAP_FAILED || replacement != (void *)mapping) {
        int saved_errno = replacement == MAP_FAILED ? errno : EFAULT;

        atomic_store_explicit(&context.phase, WORKER_ABORT,
                              memory_order_release);
        (void)join_worker_bounded(worker);
        operational_fail(case_name, pages, "mmap_fixed", saved_errno);
    }
    stale_count = release_and_join_worker(&context, worker, case_name, pages,
                                          heartbeat_before);
    if (mprotect((void *)mapping, length, PROT_READ | PROT_WRITE) != 0) {
        operational_fail(case_name, pages, "mprotect_replacement", errno);
    }
    fill_page_sentinels(mapping, pages, page_size, new_pattern);
    if (!page_sentinels_match(mapping, pages, page_size, new_pattern)) {
        operational_fail(case_name, pages, "replacement_content", EIO);
    }
    checked_unmap(mapping, length, case_name, pages, "munmap_replacement");
    emit_case_result(case_name, pages, worker_cpu, stale_count);
    return stale_count;
}

static size_t run_mremap_case(size_t pages, size_t page_size, int worker_cpu)
{
    static const char case_name[] = "mremap_fixed_old_alias";
    const size_t length = pages * page_size;
    volatile unsigned char *source =
        map_rw(length, case_name, pages, "mmap_source");
    void *destination = mmap(NULL, length, PROT_NONE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    struct worker_context context;
    pthread_t worker;
    void *moved;
    size_t heartbeat_before;
    size_t stale_count;

    if (destination == MAP_FAILED) {
        operational_fail(case_name, pages, "mmap_destination", errno);
    }
    fill_page_sentinels(source, pages, page_size, old_pattern);
    initialize_worker(&context, WORKER_OLD_ALIAS_READ, source, pages,
                      page_size, worker_cpu);
    worker = start_worker(&context, case_name, pages);
    heartbeat_before = atomic_load_explicit(&context.spin_heartbeat,
                                            memory_order_relaxed);
    moved = mremap((void *)source, length, length,
                   MREMAP_MAYMOVE | MREMAP_FIXED, destination);
    if (moved == MAP_FAILED || moved != destination) {
        int saved_errno = moved == MAP_FAILED ? errno : EFAULT;

        atomic_store_explicit(&context.phase, WORKER_ABORT,
                              memory_order_release);
        (void)join_worker_bounded(worker);
        operational_fail(case_name, pages, "mremap_fixed", saved_errno);
    }
    if (!page_sentinels_match((volatile unsigned char *)moved, pages,
                              page_size, old_pattern)) {
        atomic_store_explicit(&context.phase, WORKER_ABORT,
                              memory_order_release);
        (void)join_worker_bounded(worker);
        operational_fail(case_name, pages, "destination_content", EIO);
    }
    stale_count = release_and_join_worker(&context, worker, case_name, pages,
                                          heartbeat_before);
    checked_unmap((volatile unsigned char *)moved, length, case_name, pages,
                  "munmap_destination");
    emit_case_result(case_name, pages, worker_cpu, stale_count);
    return stale_count;
}

static int wait_for_io(int fd, short events, uint64_t deadline)
{
    struct pollfd descriptor = {
        .fd = fd,
        .events = events,
    };

    for (;;) {
        int milliseconds;
        int remaining = deadline_remaining_ms(deadline, &milliseconds);
        int result;

        if (remaining < 0) {
            return -1;
        }
        if (remaining == 0) {
            errno = ETIMEDOUT;
            return -1;
        }
        result = poll(&descriptor, 1, milliseconds);
        if (result > 0) {
            if ((descriptor.revents & POLLNVAL) != 0) {
                errno = EBADF;
                return -1;
            }
            return 0;
        }
        if (result == 0) {
            errno = ETIMEDOUT;
            return -1;
        }
        if (errno != EINTR) {
            return -1;
        }
    }
}

static int read_full_deadline(int fd, void *buffer, size_t length,
                              uint64_t deadline)
{
    unsigned char *cursor = buffer;
    size_t complete = 0;

    while (complete < length) {
        uint64_t now;
        ssize_t result;

        if (monotonic_now(&now) != 0) {
            return -1;
        }
        if (now >= deadline) {
            errno = ETIMEDOUT;
            return -1;
        }
        result = read(fd, cursor + complete, length - complete);

        if (result > 0) {
            complete += (size_t)result;
            continue;
        }
        if (result == 0) {
            errno = EPIPE;
            return -1;
        }
        if (errno == EINTR) {
            continue;
        }
        if (errno != EAGAIN && errno != EWOULDBLOCK) {
            return -1;
        }
        if (wait_for_io(fd, POLLIN, deadline) != 0) {
            return -1;
        }
    }
    return 0;
}

static int write_full_deadline(int fd, const void *buffer, size_t length,
                               uint64_t deadline)
{
    const unsigned char *cursor = buffer;
    size_t complete = 0;

    while (complete < length) {
        uint64_t now;
        ssize_t result;

        if (monotonic_now(&now) != 0) {
            return -1;
        }
        if (now >= deadline) {
            errno = ETIMEDOUT;
            return -1;
        }
        result = write(fd, cursor + complete, length - complete);

        if (result > 0) {
            complete += (size_t)result;
            continue;
        }
        if (result == 0) {
            errno = EPIPE;
            return -1;
        }
        if (errno == EINTR) {
            continue;
        }
        if (errno != EAGAIN && errno != EWOULDBLOCK) {
            return -1;
        }
        if (wait_for_io(fd, POLLOUT, deadline) != 0) {
            return -1;
        }
    }
    return 0;
}

static int wait_for_child_bounded(pid_t child, int *status)
{
    uint64_t deadline;

    if (monotonic_deadline(&deadline) != 0) {
        return -1;
    }
    for (;;) {
        pid_t result = waitpid(child, status, WNOHANG);
        uint64_t now;

        if (result == child) {
            return 0;
        }
        if (result < 0) {
            return -1;
        }
        if (monotonic_now(&now) != 0) {
            return -1;
        }
        if (now >= deadline) {
            errno = ETIMEDOUT;
            return -1;
        }
        pause_before_retry();
    }
}

static _Noreturn void run_cow_child(volatile unsigned char *mapping,
                                    size_t pages, size_t page_size,
                                    int start_fd, int report_fd)
{
    struct child_report report = {
        .magic = CHILD_REPORT_MAGIC,
        .reserved = 0,
        .stale_count = 0,
    };
    unsigned char command;
    uint64_t deadline;
    size_t page;

    if (monotonic_deadline(&deadline) != 0 ||
        read_full_deadline(start_fd, &command, sizeof(command), deadline) != 0 ||
        command != 1U) {
        _exit(20);
    }
    for (page = 0; page < pages; ++page) {
        unsigned char value = 0;

        if (!guarded_read(mapping + page * page_size, &value) ||
            value != old_pattern(page)) {
            report.stale_count += 1U;
        }
    }
    if (monotonic_deadline(&deadline) != 0 ||
        write_full_deadline(report_fd, &report, sizeof(report), deadline) != 0) {
        _exit(21);
    }
    _exit(0);
}

static size_t run_fork_cow_case(size_t pages, size_t page_size, int worker_cpu)
{
    static const char case_name[] = "fork_cow_snapshot";
    const size_t length = pages * page_size;
    volatile unsigned char *mapping =
        map_rw(length, case_name, pages, "mmap");
    struct worker_context context;
    struct child_report report;
    pthread_t worker;
    int start_pipe[2];
    int report_pipe[2];
    pid_t child;
    uint64_t deadline;
    unsigned char command = 1;
    size_t heartbeat_before;
    size_t stale_count;
    size_t page;
    int child_status;

    fill_page_sentinels(mapping, pages, page_size, old_pattern);
    if (pipe2(start_pipe, O_CLOEXEC | O_NONBLOCK) != 0) {
        operational_fail(case_name, pages, "pipe_start", errno);
    }
    if (pipe2(report_pipe, O_CLOEXEC | O_NONBLOCK) != 0) {
        operational_fail(case_name, pages, "pipe_report", errno);
    }
    initialize_worker(&context, WORKER_COW_WRITE, mapping, pages, page_size,
                      worker_cpu);
    worker = start_worker(&context, case_name, pages);
    heartbeat_before = atomic_load_explicit(&context.spin_heartbeat,
                                            memory_order_relaxed);
    child = fork();
    if (child < 0) {
        int saved_errno = errno;

        atomic_store_explicit(&context.phase, WORKER_ABORT,
                              memory_order_release);
        (void)join_worker_bounded(worker);
        operational_fail(case_name, pages, "fork", saved_errno);
    }
    if (child == 0) {
        (void)close(start_pipe[1]);
        (void)close(report_pipe[0]);
        run_cow_child(mapping, pages, page_size, start_pipe[0], report_pipe[1]);
    }

    (void)close(start_pipe[0]);
    (void)close(report_pipe[1]);
    stale_count = release_and_join_worker(&context, worker, case_name, pages,
                                          heartbeat_before);
    for (page = 0; page < pages; ++page) {
        if (mapping[page * page_size] != cow_parent_pattern(page)) {
            stale_count += 1U;
        }
    }
    if (monotonic_deadline(&deadline) != 0 ||
        write_full_deadline(start_pipe[1], &command, sizeof(command),
                            deadline) != 0) {
        (void)kill(child, SIGKILL);
        operational_fail(case_name, pages, "signal_child", errno);
    }
    if (monotonic_deadline(&deadline) != 0 ||
        read_full_deadline(report_pipe[0], &report, sizeof(report),
                           deadline) != 0) {
        (void)kill(child, SIGKILL);
        operational_fail(case_name, pages, "read_child_report", errno);
    }
    if (wait_for_child_bounded(child, &child_status) != 0) {
        (void)kill(child, SIGKILL);
        operational_fail(case_name, pages, "waitpid", errno);
    }
    if (!WIFEXITED(child_status) || WEXITSTATUS(child_status) != 0) {
        operational_fail(case_name, pages, "child_status",
                         WIFEXITED(child_status) ? WEXITSTATUS(child_status)
                                                 : EINTR);
    }
    if (report.magic != CHILD_REPORT_MAGIC || report.reserved != 0) {
        operational_fail(case_name, pages, "child_report", EPROTO);
    }
    if (report.stale_count > (uint64_t)SIZE_MAX - stale_count) {
        operational_fail(case_name, pages, "stale_overflow", EOVERFLOW);
    }
    stale_count += (size_t)report.stale_count;
    (void)close(start_pipe[1]);
    (void)close(report_pipe[0]);
    checked_unmap(mapping, length, case_name, pages, "munmap");
    emit_case_result(case_name, pages, worker_cpu, stale_count);
    return stale_count;
}

static int parse_expected_cpus(int argc, char **argv)
{
    char *end = NULL;
    const char *cursor;
    long value;

    if (argc != 3 || strcmp(argv[1], "--expect-cpus") != 0 ||
        argv[2][0] == '\0') {
        printf("SMP_TLB_GATE status=fail kind=usage"
               " reason=expected_--expect-cpus_N\n");
        return -1;
    }
    for (cursor = argv[2]; *cursor != '\0'; ++cursor) {
        if (*cursor < '0' || *cursor > '9') {
            printf("SMP_TLB_GATE status=fail kind=usage"
                   " reason=invalid_expected_cpu_count\n");
            return -1;
        }
    }
    errno = 0;
    value = strtol(argv[2], &end, 10);
    if (errno != 0 || end == argv[2] || *end != '\0' || value < 2 ||
        value > CPU_SETSIZE) {
        printf("SMP_TLB_GATE status=fail kind=usage"
               " reason=invalid_expected_cpu_count\n");
        return -1;
    }
    return (int)value;
}

static void inspect_topology(int expected_cpus, int *control_cpu,
                             int worker_cpus[CPU_SETSIZE],
                             size_t *worker_count, long *online_cpus)
{
    cpu_set_t affinity;
    int affinity_count;
    int cpu;

    errno = 0;
    *online_cpus = sysconf(_SC_NPROCESSORS_ONLN);
    if (*online_cpus < 1) {
        operational_fail("topology", 0, "sysconf_online",
                         errno != 0 ? errno : EINVAL);
    }
    if (sched_getaffinity(0, sizeof(affinity), &affinity) != 0) {
        operational_fail("topology", 0, "sched_getaffinity", errno);
    }
    affinity_count = CPU_COUNT(&affinity);
    if (*online_cpus != expected_cpus || affinity_count != expected_cpus) {
        printf("SMP_TLB_GATE status=fail kind=topology expected_cpus=%d"
               " online_cpus=%ld affinity_cpus=%d\n",
               expected_cpus, *online_cpus, affinity_count);
        fflush(stdout);
        _Exit(2);
    }
    *control_cpu = -1;
    *worker_count = 0;
    for (cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
        if (!CPU_ISSET(cpu, &affinity)) {
            continue;
        }
        if (*control_cpu < 0) {
            *control_cpu = cpu;
            continue;
        }
        worker_cpus[*worker_count] = cpu;
        *worker_count += 1U;
    }
    if (*control_cpu < 0 || *worker_count != (size_t)expected_cpus - 1U) {
        operational_fail("topology", 0, "select_distinct_cpus", EINVAL);
    }
}

int main(int argc, char **argv)
{
    static const size_t page_cases[] = {1, 64};
    int worker_cpus[CPU_SETSIZE];
    long page_size_value;
    long online_cpus;
    int expected_cpus;
    int control_cpu;
    int actual_control_cpu;
    size_t worker_count;
    size_t total_stale = 0;
    size_t case_index;
    size_t worker_index;
    int result;

    (void)setvbuf(stdout, NULL, _IONBF, 0);
    expected_cpus = parse_expected_cpus(argc, argv);
    if (expected_cpus < 0) {
        return 2;
    }
    inspect_topology(expected_cpus, &control_cpu, worker_cpus, &worker_count,
                     &online_cpus);
    result = pin_current_thread(control_cpu);
    if (result != 0) {
        operational_fail("topology", 0, "pin_control", result);
    }
    errno = 0;
    actual_control_cpu = sched_getcpu();
    if (actual_control_cpu < 0 || actual_control_cpu != control_cpu) {
        operational_fail("topology", 0, "verify_control_cpu",
                         actual_control_cpu < 0
                             ? (errno != 0 ? errno : EIO)
                             : EXDEV);
    }
    install_signal_handlers();
    errno = 0;
    page_size_value = sysconf(_SC_PAGESIZE);
    if (page_size_value <= 0 || (uint64_t)page_size_value > SIZE_MAX / 64U) {
        operational_fail("setup", 0, "sysconf_page_size",
                         errno != 0 ? errno : EOVERFLOW);
    }

    printf("SMP_TLB_TOPOLOGY online_cpus=%ld control_cpu=%d worker_count=%zu"
           " worker_cpus=",
           online_cpus, control_cpu, worker_count);
    for (worker_index = 0; worker_index < worker_count; ++worker_index) {
        printf("%s%d", worker_index == 0 ? "" : ",",
               worker_cpus[worker_index]);
    }
    putchar('\n');
    for (case_index = 0;
         case_index < sizeof(page_cases) / sizeof(page_cases[0]);
         ++case_index) {
        const size_t pages = page_cases[case_index];
        const size_t page_size = (size_t)page_size_value;

        for (worker_index = 0; worker_index < worker_count; ++worker_index) {
            const int worker_cpu = worker_cpus[worker_index];

            total_stale += run_mprotect_case(pages, page_size, worker_cpu);
            total_stale +=
                run_munmap_replace_case(pages, page_size, worker_cpu);
            total_stale += run_mremap_case(pages, page_size, worker_cpu);
            total_stale += run_fork_cow_case(pages, page_size, worker_cpu);
        }
    }

    if (total_stale == 0) {
        printf("SMP_TLB_GATE status=ok stale_count=0\n");
        return 0;
    }
    printf("SMP_TLB_GATE status=fail kind=stale stale_count=%zu\n",
           total_stale);
    return 1;
}
